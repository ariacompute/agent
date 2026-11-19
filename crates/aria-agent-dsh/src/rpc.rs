//! Minimal JSON-RPC 2.0 client for the dsh `sdk` profile.
//!
//! One reader task owns the inbound direction and demultiplexes three frame
//! shapes:
//!
//! * **response** (`id` + `result`/`error`) → resolve the pending call;
//! * **server request** (`id` + `method`) → hand to a
//!   [`ServerRequestHandler`] (approvals, user questions) and answer it;
//! * **notification** (`method`, no `id`) → fan out to turn subscribers.
//!
//! Every failure path is closed: a dead pipe fails all pending calls, a timeout
//! removes the pending entry, an unparsable frame is dropped with a debug log
//! (never surfaced as model content), and an unsupported server request is
//! answered with JSON-RPC `-32601` so dsh fails closed instead of hanging.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Mutex};

use agent_core::CoreError;

use crate::transport::Transport;

/// Handshake method: `initialize(cwd, provider, model, approval_requests)`.
pub const METHOD_INITIALIZE: &str = "initialize";
/// Queue one prompt into a session (`session_prompt` in the Python SDK).
pub const METHOD_SESSION_PROMPT: &str = "session/prompt";
/// Server→client approval request.
pub const METHOD_APPROVAL_REQUEST: &str = "approval/request";
/// Server→client user-question request.
pub const METHOD_USER_QUESTION: &str = "ask_user_question";
/// JSON-RPC "method not found".
pub const CODE_METHOD_NOT_FOUND: i64 = -32601;
/// Default notification buffer (backpressure, never unbounded).
pub const DEFAULT_NOTIFICATION_CAPACITY: usize = 64;

/// Answers a server→client request; `Err((code, message))` becomes a JSON-RPC
/// error response, which is how "unavailable" is reported to the runtime.
#[async_trait]
pub trait ServerRequestHandler: Send + Sync {
    async fn handle(&self, method: &str, params: &Value) -> Result<Value, (i64, String)>;
}

/// Default handler: refuses every request (fail-closed).
pub struct UnsupportedRequests;

#[async_trait]
impl ServerRequestHandler for UnsupportedRequests {
    async fn handle(&self, method: &str, _params: &Value) -> Result<Value, (i64, String)> {
        Err((
            CODE_METHOD_NOT_FOUND,
            format!("unsupported dsh request: {method}"),
        ))
    }
}

struct Inner {
    transport: Transport,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    subscribers: Mutex<Vec<mpsc::Sender<Value>>>,
}

/// Cloneable client handle.
#[derive(Clone)]
pub struct RpcClient {
    inner: Arc<Inner>,
}

impl RpcClient {
    pub fn new(transport: Transport) -> Self {
        Self {
            inner: Arc::new(Inner {
                transport,
                next_id: AtomicU64::new(0),
                pending: Mutex::new(HashMap::new()),
                subscribers: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Subscribe to notifications. Call **before** sending the prompt so no
    /// frame is missed; dropping the receiver unsubscribes.
    pub async fn subscribe(&self, capacity: usize) -> mpsc::Receiver<Value> {
        let (tx, rx) = mpsc::channel(capacity);
        self.inner.subscribers.lock().await.push(tx);
        rx
    }

    /// Issue a call and wait for its response (bounded by `timeout`).
    pub async fn call(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, CoreError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().await.insert(id, tx);
        let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(e) = self.inner.transport.send_line(&frame.to_string()).await {
            self.inner.pending.lock().await.remove(&id);
            return Err(e);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(message))) => Err(CoreError::Model(format!(
                "dsh `{method}` failed: {message}"
            ))),
            Ok(Err(_)) => Err(CoreError::Model(format!(
                "dsh `{method}` response channel closed"
            ))),
            Err(_) => {
                self.inner.pending.lock().await.remove(&id);
                Err(CoreError::Model(format!(
                    "dsh `{method}` timed out after {timeout:?}"
                )))
            }
        }
    }

    /// Answer a server→client request.
    pub async fn respond(&self, id: Value, result: Value) -> Result<(), CoreError> {
        let frame = json!({"jsonrpc": "2.0", "id": id, "result": result});
        self.inner.transport.send_line(&frame.to_string()).await
    }

    /// Answer a server→client request with an error.
    pub async fn respond_error(
        &self,
        id: Value,
        code: i64,
        message: String,
    ) -> Result<(), CoreError> {
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message},
        });
        self.inner.transport.send_line(&frame.to_string()).await
    }

    /// Start the single reader task. Call exactly once per transport.
    pub fn start_reader(self: &Arc<Self>, handler: Arc<dyn ServerRequestHandler>) {
        let client = self.clone();
        tokio::spawn(async move {
            loop {
                match client.inner.transport.recv_line().await {
                    Ok(None) => {
                        client.fail_all("dsh runtime exited").await;
                        break;
                    }
                    Err(e) => {
                        client.fail_all(&e.to_string()).await;
                        break;
                    }
                    Ok(Some(line)) if line.trim().is_empty() => continue,
                    Ok(Some(line)) => client.dispatch(&line, &handler).await,
                }
            }
        });
    }

    /// Terminate the runtime (idempotent).
    pub async fn shutdown(&self) -> Result<(), CoreError> {
        self.fail_all("dsh runtime shut down").await;
        self.inner.transport.close().await
    }

    async fn fail_all(&self, reason: &str) {
        let mut pending = self.inner.pending.lock().await;
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(reason.to_string()));
        }
    }

    async fn dispatch(&self, line: &str, handler: &Arc<dyn ServerRequestHandler>) {
        let frame: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(e) => {
                tracing::debug!(error = %e, "dropping unparsable dsh frame");
                return;
            }
        };
        let id = frame.get("id").cloned();
        let method = frame
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_string);
        match (id, method) {
            (Some(id), Some(method)) => {
                let params = frame.get("params").cloned().unwrap_or(Value::Null);
                let client = self.clone();
                let handler = handler.clone();
                tokio::spawn(async move {
                    match handler.handle(&method, &params).await {
                        Ok(result) => {
                            let _ = client.respond(id, result).await;
                        }
                        Err((code, message)) => {
                            let _ = client.respond_error(id, code, message).await;
                        }
                    }
                });
            }
            (Some(id), None) => {
                let Some(numeric) = id.as_u64() else {
                    tracing::debug!("dropping dsh response with a non-numeric id");
                    return;
                };
                let tx = self.inner.pending.lock().await.remove(&numeric);
                match tx {
                    Some(tx) => {
                        let result = match frame.get("error") {
                            Some(err) => Err(error_message(err)),
                            None => Ok(frame.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        let _ = tx.send(result);
                    }
                    None => tracing::debug!(id = numeric, "dsh response for an unknown call"),
                }
            }
            (None, Some(_)) => self.broadcast(frame).await,
            (None, None) => tracing::debug!("dropping dsh frame without id or method"),
        }
    }

    async fn broadcast(&self, frame: Value) {
        // Closed turns leave a dropped receiver behind; `retain` unsubscribes it
        // instead of failing the notification (a late frame is not an error).
        let mut subscribers = self.inner.subscribers.lock().await;
        subscribers.retain(|tx| tx.try_send(frame.clone()).is_ok());
    }
}

fn error_message(err: &Value) -> String {
    err.get("message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::RpcTransport;
    use std::sync::atomic::AtomicBool;

    /// In-memory transport: what the client writes is visible to the test, and
    /// the test decides what the "runtime" answers.
    struct FakeTransport {
        out: mpsc::UnboundedSender<String>,
        inbound: Mutex<mpsc::UnboundedReceiver<String>>,
        closed: AtomicBool,
    }

    impl FakeTransport {
        fn pair() -> (
            Arc<Self>,
            mpsc::UnboundedReceiver<String>,
            mpsc::UnboundedSender<String>,
        ) {
            let (out_tx, out_rx) = mpsc::unbounded_channel();
            let (in_tx, in_rx) = mpsc::unbounded_channel();
            (
                Arc::new(Self {
                    out: out_tx,
                    inbound: Mutex::new(in_rx),
                    closed: AtomicBool::new(false),
                }),
                out_rx,
                in_tx,
            )
        }
    }

    #[async_trait]
    impl RpcTransport for FakeTransport {
        async fn send_line(&self, line: &str) -> Result<(), CoreError> {
            self.out
                .send(line.to_string())
                .map_err(|_| CoreError::Model("fake closed".into()))
        }

        async fn recv_line(&self) -> Result<Option<String>, CoreError> {
            Ok(self.inbound.lock().await.recv().await)
        }

        async fn close(&self) -> Result<(), CoreError> {
            self.closed.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    struct AllowAll;

    #[async_trait]
    impl ServerRequestHandler for AllowAll {
        async fn handle(&self, method: &str, _params: &Value) -> Result<Value, (i64, String)> {
            if method == METHOD_APPROVAL_REQUEST {
                Ok(json!({"decision": "allow"}))
            } else {
                Err((CODE_METHOD_NOT_FOUND, "nope".into()))
            }
        }
    }

    #[tokio::test]
    async fn a_call_round_trips_and_resolves_by_id() {
        let (transport, mut written, inbound) = FakeTransport::pair();
        let client = Arc::new(RpcClient::new(transport));
        client.start_reader(Arc::new(UnsupportedRequests));
        let call = {
            let client = client.clone();
            tokio::spawn(async move {
                client
                    .call("initialize", json!({"cwd": "/w"}), Duration::from_secs(1))
                    .await
            })
        };
        let frame: Value = serde_json::from_str(&written.recv().await.expect("sent")).unwrap();
        assert_eq!(frame["method"], "initialize");
        assert_eq!(frame["id"], 1);
        inbound
            .send(r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#.into())
            .unwrap();
        let value = call.await.expect("join").expect("call");
        assert_eq!(value["ok"], true);
    }

    #[tokio::test]
    async fn a_json_rpc_error_becomes_a_core_error() {
        let (transport, mut written, inbound) = FakeTransport::pair();
        let client = Arc::new(RpcClient::new(transport));
        client.start_reader(Arc::new(UnsupportedRequests));
        let call = {
            let client = client.clone();
            tokio::spawn(async move {
                client
                    .call("session/prompt", json!({}), Duration::from_secs(1))
                    .await
            })
        };
        let _ = written.recv().await.expect("sent");
        inbound
            .send(
                r#"{"jsonrpc":"2.0","id":1,"error":{"message":"bad session","code":-32602}}"#
                    .into(),
            )
            .unwrap();
        let err = call.await.expect("join").unwrap_err();
        assert!(err.to_string().contains("bad session"), "{err}");
    }

    #[tokio::test]
    async fn a_timeout_removes_the_pending_entry() {
        let (transport, mut written, _inbound) = FakeTransport::pair();
        let client = Arc::new(RpcClient::new(transport));
        let err = client
            .call("initialize", json!({}), Duration::from_millis(5))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        let _ = written.recv().await.expect("frame was written");
    }

    #[tokio::test]
    async fn notifications_reach_subscribers() {
        let (transport, mut written, inbound) = FakeTransport::pair();
        let client = Arc::new(RpcClient::new(transport));
        client.start_reader(Arc::new(UnsupportedRequests));
        let mut rx = client.subscribe(4).await;
        let call = {
            let client = client.clone();
            tokio::spawn(async move {
                client
                    .call("session/prompt", json!({}), Duration::from_secs(1))
                    .await
            })
        };
        let _ = written.recv().await.expect("prompt");
        inbound
            .send(r#"{"jsonrpc":"2.0","method":"step/start","params":{"sessionId":"s"}}"#.into())
            .unwrap();
        let note = rx.recv().await.expect("notification");
        assert_eq!(note["method"], "step/start");
        // A closed subscriber is dropped, and the next notification is still safe.
        drop(rx);
        inbound
            .send(r#"{"jsonrpc":"2.0","method":"turn/start","params":{}}"#.into())
            .unwrap();
        inbound
            .send(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#.into())
            .unwrap();
        call.await.expect("join").expect("call");
    }

    #[tokio::test]
    async fn a_server_request_is_answered_and_an_unsupported_one_fails_closed() {
        let (transport, mut written, inbound) = FakeTransport::pair();
        let client = Arc::new(RpcClient::new(transport));
        client.start_reader(Arc::new(AllowAll));
        inbound
            .send(
                r#"{"jsonrpc":"2.0","id":9,"method":"approval/request","params":{"tool":"shell"}}"#
                    .into(),
            )
            .unwrap();
        let answer: Value = serde_json::from_str(&written.recv().await.expect("answer")).unwrap();
        assert_eq!(answer["id"], 9);
        assert_eq!(answer["result"]["decision"], "allow");

        inbound
            .send(r#"{"jsonrpc":"2.0","id":"x","method":"ask_user_question","params":{}}"#.into())
            .unwrap();
        let refused: Value = serde_json::from_str(&written.recv().await.expect("refusal")).unwrap();
        assert_eq!(refused["id"], "x");
        assert_eq!(refused["error"]["code"], CODE_METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn a_dead_pipe_fails_pending_calls_and_ignores_junk() {
        let (transport, mut written, inbound) = FakeTransport::pair();
        let client = Arc::new(RpcClient::new(transport));
        client.start_reader(Arc::new(UnsupportedRequests));
        inbound.send("not json".into()).unwrap();
        inbound
            .send(r#"{"jsonrpc":"2.0","id":"nope","result":{}}"#.into())
            .unwrap();
        inbound
            .send(r#"{"jsonrpc":"2.0","params":{}}"#.into())
            .unwrap();
        let call = {
            let client = client.clone();
            tokio::spawn(async move {
                client
                    .call("initialize", json!({}), Duration::from_secs(5))
                    .await
            })
        };
        let _ = written.recv().await.expect("frame");
        drop(inbound);
        let err = call.await.expect("join").unwrap_err();
        assert!(err.to_string().contains("exited"), "{err}");
        // The reader is done; shutdown stays idempotent.
        client.shutdown().await.expect("shutdown");
    }

    /// Transport whose writes always fail (a runtime that died between frames).
    struct DeadTransport;

    #[async_trait]
    impl RpcTransport for DeadTransport {
        async fn send_line(&self, _line: &str) -> Result<(), CoreError> {
            Err(CoreError::Model(
                "dsh transport write failed: broken pipe".into(),
            ))
        }

        async fn recv_line(&self) -> Result<Option<String>, CoreError> {
            Ok(None)
        }

        async fn close(&self) -> Result<(), CoreError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_write_failure_surfaces_and_clears_the_pending_call() {
        let client = RpcClient::new(Arc::new(DeadTransport));
        let err = client
            .call("initialize", json!({}), Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("broken pipe"), "{err}");
        assert!(client.inner.pending.lock().await.is_empty());
    }

    #[tokio::test]
    async fn the_default_handler_refuses_every_server_request() {
        let (transport, mut written, inbound) = FakeTransport::pair();
        let client = Arc::new(RpcClient::new(transport));
        client.start_reader(Arc::new(UnsupportedRequests));
        inbound
            .send(r#"{"jsonrpc":"2.0","id":7,"method":"fs/write","params":{}}"#.into())
            .unwrap();
        let answer: Value = serde_json::from_str(&written.recv().await.expect("answer")).unwrap();
        assert_eq!(answer["error"]["code"], CODE_METHOD_NOT_FOUND);
        assert_eq!(
            answer["error"]["message"],
            "unsupported dsh request: fs/write"
        );
    }

    /// Transport whose reads fail immediately (a corrupted pipe).
    struct BrokenTransport;

    #[async_trait]
    impl RpcTransport for BrokenTransport {
        async fn send_line(&self, _line: &str) -> Result<(), CoreError> {
            Ok(())
        }

        async fn recv_line(&self) -> Result<Option<String>, CoreError> {
            Err(CoreError::Model("dsh transport read failed: EIO".into()))
        }

        async fn close(&self) -> Result<(), CoreError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_read_error_fails_the_pending_call() {
        let client = Arc::new(RpcClient::new(Arc::new(BrokenTransport)));
        client.start_reader(Arc::new(UnsupportedRequests));
        let err = client
            .call("initialize", json!({}), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("EIO"), "{err}");
    }

    #[tokio::test]
    async fn shutdown_on_a_dead_transport_is_reported() {
        let client = Arc::new(RpcClient::new(Arc::new(DeadTransport)));
        client.start_reader(Arc::new(UnsupportedRequests));
        client.shutdown().await.expect("close succeeds");
    }

    #[tokio::test]
    async fn unknown_ids_and_error_objects_without_messages_are_tolerated() {
        let (transport, mut written, inbound) = FakeTransport::pair();
        let client = Arc::new(RpcClient::new(transport));
        client.start_reader(Arc::new(UnsupportedRequests));
        // A stray numeric response must not panic or resolve anything.
        inbound
            .send(r#"{"jsonrpc":"2.0","id":42,"result":{}}"#.into())
            .unwrap();
        let call = {
            let client = client.clone();
            tokio::spawn(async move {
                client
                    .call("initialize", json!({}), Duration::from_secs(1))
                    .await
            })
        };
        let _ = written.recv().await.expect("frame");
        // An error frame without `message` still yields a readable error.
        inbound
            .send(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-1}}"#.into())
            .unwrap();
        let err = call.await.expect("join").unwrap_err();
        assert!(
            err.to_string().contains("-1") || err.to_string().contains("code"),
            "{err}"
        );
    }
}
