//! End-to-end tests for the `redacted_thinking` filter: a real HTTP client
//! plays the dsh role, a fake Anthropic-compatible gateway plays the upstream,
//! and `ThinkingFilterProxy` sits in between on a loopback port.
//!
//! The regression this guards: dsh's bundled `deepseek-official` adapter accepts
//! a signed `redacted_thinking` block in turn one and then fails turn two with
//! `UNSUPPORTED_CONTENT` while replaying that history. The proxy must make the
//! block invisible to dsh (so it is never persisted) *and* scrub it from
//! histories written before the proxy existed, while leaving `thinking` intact.
//!
//! Offline and deterministic: no credentials, no `dsh` install. The one test
//! that spawns a child process is Unix-only, like `tests/stdio.rs`.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aria_agent_dsh::{parse_filter_mode, ThinkingFilterProxy};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// What the fake gateway saw, one entry per request.
#[derive(Debug, Clone)]
struct Recorded {
    target: String,
    body: Value,
    api_key: String,
    accept_encoding: String,
}

impl Recorded {
    fn body_text(&self) -> String {
        self.body.to_string()
    }
}

/// A loopback Anthropic-compatible gateway that records every request and
/// answers with a redacted block, so the proxy has something to strip.
struct Gateway {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Recorded>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Gateway {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind gateway");
        let addr = listener.local_addr().expect("gateway addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let recorded = Arc::clone(&recorded);
                tokio::spawn(async move {
                    if let Err(e) = serve_gateway(stream, recorded).await {
                        eprintln!("fake gateway connection failed: {e}");
                    }
                });
            }
        });
        Self {
            addr,
            requests,
            task,
        }
    }

    fn root(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().expect("requests").clone()
    }

    fn last(&self) -> Recorded {
        self.requests()
            .pop()
            .expect("the gateway must have received a request")
    }
}

async fn serve_gateway(
    mut stream: TcpStream,
    recorded: Arc<Mutex<Vec<Recorded>>>,
) -> std::io::Result<()> {
    let (head, body) = read_request(&mut stream).await?;
    let (target, headers) = parse_head(&head);
    let header = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    };
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let stream_wanted = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    recorded.lock().expect("requests").push(Recorded {
        target,
        body,
        api_key: header("x-api-key"),
        accept_encoding: header("accept-encoding"),
    });

    if stream_wanted {
        write_chunked_sse(&mut stream).await
    } else {
        write_json(&mut stream).await
    }
}

/// One redacted block between a thinking block and a tool call, exactly the
/// shape that broke the adapter's history replay.
async fn write_chunked_sse(stream: &mut TcpStream) -> std::io::Result<()> {
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
        )
        .await?;
    let frames = [
        r#"event: message_start
data: {"type":"message_start","message":{"content":[{"type":"thinking","thinking":"plan","signature":"sig"},{"type":"redacted_thinking","data":"sealed"}]}}"#,
        r#"event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        r#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"plan"}}"#,
        r#"event: content_block_stop
data: {"type":"content_block_stop","index":0}"#,
        r#"event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"sealed"}}"#,
        r#"event: content_block_stop
data: {"type":"content_block_stop","index":1}"#,
        r#"event: content_block_start
data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"t1","name":"shell","input":{}}}"#,
        r#"event: content_block_delta
data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":\"ls\"}"}}"#,
        r#"event: content_block_stop
data: {"type":"content_block_stop","index":2}"#,
        r#"event: message_stop
data: {"type":"message_stop"}"#,
    ];
    for (index, frame) in frames.iter().enumerate() {
        // Split one frame across two chunks: the proxy has to buffer partial
        // frames instead of assuming one chunk equals one frame.
        let payload = format!("{frame}\n\n");
        if index == 6 {
            let (left, right) = payload.split_at(payload.len() / 2);
            write_chunk(stream, left.as_bytes()).await?;
            write_chunk(stream, right.as_bytes()).await?;
        } else {
            write_chunk(stream, payload.as_bytes()).await?;
        }
    }
    stream.write_all(b"0\r\n\r\n").await
}

async fn write_chunk(stream: &mut TcpStream, data: &[u8]) -> std::io::Result<()> {
    stream
        .write_all(format!("{:x}\r\n", data.len()).as_bytes())
        .await?;
    stream.write_all(data).await?;
    stream.write_all(b"\r\n").await
}

async fn write_json(stream: &mut TcpStream) -> std::io::Result<()> {
    let body = json!({
        "id": "msg_1",
        "content": [
            {"type": "thinking", "thinking": "plan", "signature": "sig"},
            {"type": "redacted_thinking", "data": "sealed"},
            {"type": "text", "text": "done"}
        ]
    })
    .to_string();
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await
}

async fn read_request(stream: &mut TcpStream) -> std::io::Result<(String, Vec<u8>)> {
    let mut buf = Vec::new();
    let head_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(std::io::Error::other("connection closed before head"));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut body = buf[head_end + 4..].to_vec();
    let (_, headers) = parse_head(&head);
    let length = headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut rest = vec![0u8; length.saturating_sub(body.len())];
    stream.read_exact(&mut rest).await?;
    body.extend_from_slice(&rest);
    Ok((head, body))
}

fn parse_head(head: &str) -> (String, Vec<(String, String)>) {
    let mut lines = head.split("\r\n");
    let target = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("")
        .to_string();
    let headers = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_string(), value.trim().to_string()))
        })
        .collect();
    (target, headers)
}

async fn start_proxy(gateway: &Gateway) -> ThinkingFilterProxy {
    ThinkingFilterProxy::start(&gateway.root())
        .await
        .expect("proxy start")
}

async fn post_messages(proxy: &ThinkingFilterProxy, target: &str, body: Value) -> (u16, String) {
    let response = reqwest::Client::new()
        .post(format!("{}{target}", proxy.local_base_url()))
        .header("x-api-key", "test-key")
        .header("anthropic-version", "2023-06-01")
        .header("accept-encoding", "gzip, deflate")
        .header("content-type", "application/json")
        .body(body.to_string())
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .expect("proxy request");
    let status = response.status().as_u16();
    let text = response.text().await.expect("proxy body");
    (status, text)
}

fn user_turn(text: &str) -> Value {
    json!([{"role": "user", "content": [{"type": "text", "text": text}]}])
}

#[test]
fn the_switch_reads_as_enable_or_disable() {
    assert!(parse_filter_mode(None).expect("unset defaults to enable"));
    assert!(parse_filter_mode(Some("enable")).expect("enable"));
    assert!(!parse_filter_mode(Some("disable")).expect("disable"));
    assert!(parse_filter_mode(Some("always")).is_err());
}

#[tokio::test]
async fn a_streamed_turn_hides_the_redacted_block_from_dsh() {
    let gateway = Gateway::start().await;
    let proxy = start_proxy(&gateway).await;

    let (status, body) = post_messages(
        &proxy,
        "/anthropic/v1/messages?beta=true",
        json!({"model": "m", "stream": true, "messages": user_turn("hi")}),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    // dsh must never be able to persist the block in the first place.
    assert!(!body.contains("redacted_thinking"), "{body}");
    // `thinking` is preserved (DeepSeek requires it on replay).
    assert!(body.contains("thinking_delta"), "{body}");
    // The tool block was index 2 upstream and must arrive as index 1.
    assert!(body.contains("tool_use"), "{body}");
    assert!(body.contains("\"index\":1"), "{body}");
    assert!(body.contains("message_stop"), "{body}");

    let recorded = gateway.last();
    assert_eq!(recorded.target, "/anthropic/v1/messages?beta=true");
    assert_eq!(recorded.api_key, "test-key");
    // Compression is refused so every body can be inspected.
    assert_eq!(recorded.accept_encoding, "identity");
    assert!(!recorded.body_text().contains("redacted_thinking"));
}

#[tokio::test]
async fn replaying_history_with_a_redacted_block_still_completes() {
    let gateway = Gateway::start().await;
    let proxy = start_proxy(&gateway).await;

    // Turn one: the model answers with a tool call.
    let (status, first) = post_messages(
        &proxy,
        "/anthropic/v1/messages",
        json!({"model": "m", "stream": true, "messages": user_turn("list files")}),
    )
    .await;
    assert_eq!(status, 200, "{first}");

    // Turn two replays the assistant turn *including* the redacted block, as a
    // session written before the proxy existed would.
    let history = json!([
        {"role": "user", "content": [{"type": "text", "text": "list files"}]},
        {"role": "assistant", "content": [
            {"type": "thinking", "thinking": "plan", "signature": "sig"},
            {"type": "redacted_thinking", "data": "sealed"},
            {"type": "tool_use", "id": "t1", "name": "shell", "input": {"cmd": "ls"}}
        ]},
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": "a.txt"}
        ]}
    ]);
    let (status, second) = post_messages(
        &proxy,
        "/anthropic/v1/messages",
        json!({"model": "m", "stream": true, "messages": history}),
    )
    .await;

    assert_eq!(status, 200, "{second}");
    assert!(!second.contains("redacted_thinking"), "{second}");

    let recorded = gateway.last();
    let replayed = recorded.body_text();
    assert!(!replayed.contains("redacted_thinking"), "{replayed}");
    assert!(replayed.contains("\"thinking\":\"plan\""), "{replayed}");
    assert!(replayed.contains("tool_result"), "{replayed}");
}

#[tokio::test]
async fn a_non_streamed_turn_has_the_block_removed_too() {
    let gateway = Gateway::start().await;
    let proxy = start_proxy(&gateway).await;

    let (status, body) = post_messages(
        &proxy,
        "/anthropic/v1/messages",
        json!({"model": "m", "messages": user_turn("hi")}),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert!(!body.contains("redacted_thinking"), "{body}");
    assert!(body.contains("\"text\":\"done\""), "{body}");
    assert!(body.contains("\"type\":\"thinking\""), "{body}");
}

#[tokio::test]
async fn concurrent_turns_share_one_proxy() {
    let gateway = Gateway::start().await;
    let proxy = start_proxy(&gateway).await;

    let body = json!({"model": "m", "messages": user_turn("hi")});
    let (a, b) = tokio::join!(
        post_messages(&proxy, "/anthropic/v1/messages", body.clone()),
        post_messages(&proxy, "/anthropic/v1/messages", body),
    );
    assert_eq!(a.0, 200, "{}", a.1);
    assert_eq!(b.0, 200, "{}", b.1);
    assert_eq!(gateway.requests().len(), 2);
}

/// The proxy is only useful if dsh is actually pointed at it: boot the
/// deployment's config against a real child process and check what the child
/// reads from its environment. Unix-only, like `tests/stdio.rs`.
#[cfg(unix)]
#[tokio::test]
async fn the_child_process_is_pointed_at_the_proxy() {
    use aria_agent_dsh::{no_bridge, DshConfig, DshRuntime, HarnessRuntime};

    let dir = tempfile::tempdir().expect("tempdir");
    let dump = dir.path().join("env.txt");
    let bin = dir.path().join("env-dump-dsh");
    std::fs::write(
        &bin,
        r##"#!/bin/sh
printf '%s\n%s\n' "$DEEPSEEK_BASE_URL" "$DEEPSEEK_API_KEY" > "$DSH_ENV_DUMP"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
done
"##,
    )
    .expect("write script");
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&bin).expect("stat").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).expect("chmod");
    }

    // The upstream is never called in this test; the point is the child env.
    let gateway = Gateway::start().await;
    let proxy = start_proxy(&gateway).await;
    let cfg = DshConfig::for_session(dir.path(), "s1", "fake-model")
        .with_bin(bin.to_string_lossy().to_string())
        .with_env("DSH_ENV_DUMP", dump.to_string_lossy().to_string())
        .with_gateway_env(&proxy.local_base_url(), "test-key");
    let runtime = DshRuntime::connect(cfg, no_bridge())
        .await
        .expect("handshake succeeds");

    let dumped = std::fs::read_to_string(&dump).expect("env dump");
    let mut lines = dumped.lines();
    let base = lines.next().unwrap_or_default();
    let key = lines.next().unwrap_or_default();
    assert_eq!(base, format!("{}/anthropic/v1", proxy.local_base_url()));
    assert_eq!(key, "test-key");
    assert!(base.starts_with("http://127.0.0.1:"), "{base}");
    assert_ne!(base, format!("{}/anthropic/v1", gateway.root()));

    runtime.shutdown().await.expect("shutdown");
}
