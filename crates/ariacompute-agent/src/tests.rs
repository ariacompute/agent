//! Offline tests for the cloud API client.
//!
//! A tiny `TcpListener` stub stands in for `aria-agent-cloud`: it records every
//! request (method, path, headers, body) and replays canned responses in order.
//! No real network, no async runtime, no daemon.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;

use super::*;

/// One recorded request.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    body: String,
    beta: Option<String>,
    authorization: Option<String>,
}

/// A canned HTTP response.
#[derive(Clone)]
struct Canned {
    status: u16,
    content_type: &'static str,
    body: String,
}

impl Canned {
    fn json(body: serde_json::Value) -> Self {
        Self {
            status: 200,
            content_type: "application/json",
            body: body.to_string(),
        }
    }

    fn status(status: u16, body: &str) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: body.to_string(),
        }
    }

    fn sse(body: &str) -> Self {
        Self {
            status: 200,
            content_type: "text/event-stream",
            body: body.to_string(),
        }
    }
}

struct Stub {
    base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Stub {
    fn start(responses: Vec<Canned>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let queue = Arc::new(Mutex::new(responses.into_iter().collect::<VecDeque<_>>()));
        let seen_clone = seen.clone();
        let stop_clone = stop.clone();
        let handle = std::thread::spawn(move || {
            while !stop_clone.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // Windows sockets inherit the listener's nonblocking mode on
                        // accept. Reads would then fail with `WouldBlock` before the
                        // request arrived, so the stub would reply without consuming
                        // it and reqwest would report a spurious network error.
                        stream.set_nonblocking(false).expect("blocking stream");
                        stream
                            .set_read_timeout(Some(Duration::from_secs(5)))
                            .expect("timeout");
                        let request = read_request(&mut stream);
                        if let Some(request) = request {
                            seen_clone.lock().unwrap().push(request);
                        }
                        let response = queue
                            .lock()
                            .unwrap()
                            .pop_front()
                            .unwrap_or_else(|| Canned::status(598, "no canned response"));
                        let _ = stream.write_all(&render(&response));
                        let _ = stream.flush();
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(2)),
                }
            }
        });
        Self {
            base,
            seen,
            stop,
            handle: Some(handle),
        }
    }

    fn url(&self) -> String {
        self.base.clone()
    }

    fn requests(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn read_request(stream: &mut std::net::TcpStream) -> Option<Seen> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    // Read headers first, then exactly `content-length` body bytes.
    let header_end = loop {
        if let Some(pos) = find_headers_end(&buffer) {
            break pos;
        }
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut beta = None;
    let mut authorization = None;
    let mut length = 0usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            "openai-beta" => beta = Some(value.to_string()),
            "authorization" => authorization = Some(value.to_string()),
            "content-length" => length = value.parse().unwrap_or(0),
            _ => {}
        }
    }
    let mut body = buffer[header_end..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    Some(Seen {
        method,
        path,
        body: String::from_utf8_lossy(&body).to_string(),
        beta,
        authorization,
    })
}

fn find_headers_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|pos| pos + 4)
}

fn render(response: &Canned) -> Vec<u8> {
    let head = format!(
        "HTTP/1.1 {} X\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        response.content_type,
        response.body.len()
    );
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(response.body.as_bytes());
    bytes
}

fn make_agent(base: &str, extra: impl FnOnce(&mut AriaAgentConfig)) -> Arc<AriaAgent> {
    let mut config = AriaAgentConfig {
        name: "demo".into(),
        client: Some(ClientOptions {
            base_url: Some(base.to_string()),
            api_key: Some("sk-test".into()),
            beta_header: None,
        }),
        ..AriaAgentConfig::default()
    };
    extra(&mut config);
    AriaAgent::new(config).expect("agent")
}

#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<StreamEvent>>,
}

struct Shared(Arc<Recorder>);

impl StreamListener for Shared {
    fn on_event(&self, event: StreamEvent) {
        self.0.events.lock().unwrap().push(event);
    }
}

fn boxed_listener(recorder: &Arc<Recorder>) -> Box<dyn StreamListener> {
    Box::new(Shared(recorder.clone()))
}

#[test]
fn an_agent_requires_a_name() {
    let err = AriaAgent::new(AriaAgentConfig {
        name: "   ".into(),
        ..AriaAgentConfig::default()
    })
    .err()
    .expect("must fail");
    assert!(matches!(err, AriaError::Config { .. }), "{err}");
    assert_eq!(err.kind(), "config");
    assert_eq!(err.status(), None);
}

#[test]
fn an_empty_session_id_is_rejected() {
    let stub = Stub::start(vec![]);
    let err = make_agent(&stub.url(), |_| {})
        .session(" ".into())
        .err()
        .expect("must fail");
    assert!(matches!(err, AriaError::Config { .. }), "{err}");
}

#[test]
fn the_agent_exposes_its_definition() {
    let stub = Stub::start(vec![]);
    let agent = make_agent(&stub.url(), |c| {
        c.id = Some("agent-1".into());
        c.model = Some("m1".into());
    });
    assert_eq!(agent.name(), "demo");
    assert_eq!(agent.model().as_deref(), Some("m1"));
    assert_eq!(agent.id().as_deref(), Some("agent-1"));
}

#[test]
fn create_session_posts_the_agent_name_and_inherits_memory_config() {
    let stub = Stub::start(vec![Canned::json(json!({ "id": "sess-1" }))]);
    let agent = make_agent(&stub.url(), |c| {
        c.instructions = Some("be brief".into());
        c.memory = Some(MemoryConfig {
            backend: Some("local".into()),
            memo_bin: Some("aria-memo".into()),
            memo_db: None,
        });
    });
    let session = agent.create_session().expect("session");
    assert_eq!(session.id(), "sess-1");
    assert_eq!(session.memory_backend(), "local");

    let requests = stub.requests();
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, "/v1/agents/sessions");
    assert_eq!(requests[0].beta.as_deref(), Some("agents=v1"));
    assert_eq!(requests[0].authorization.as_deref(), Some("Bearer sk-test"));
    let body: serde_json::Value = serde_json::from_str(&requests[0].body).expect("json");
    assert_eq!(body["agent"], "demo");
    assert_eq!(body["instructions"], "be brief");
}

#[test]
fn create_session_uses_the_agent_id_when_known() {
    let stub = Stub::start(vec![Canned::json(json!({ "id": "sess-2" }))]);
    let agent = make_agent(&stub.url(), |c| c.id = Some("agent-9".into()));
    agent.create_session().expect("session");
    let body: serde_json::Value = serde_json::from_str(&stub.requests()[0].body).expect("json");
    assert_eq!(body["agent_id"], "agent-9");
    assert!(body.get("agent").is_none());
}

#[test]
fn run_creates_a_session_on_demand_and_returns_the_output() {
    let stub = Stub::start(vec![
        Canned::json(json!({ "id": "sess-3" })),
        Canned::json(json!({ "id": "turn-1", "output": "hello" })),
    ]);
    let agent = make_agent(&stub.url(), |_| {});
    let result = agent.run("hi".into(), None).expect("run");
    assert_eq!(result.final_output, "hello");
    assert_eq!(result.session_id, "sess-3");
    assert_eq!(result.turn_id.as_deref(), Some("turn-1"));
    assert!(result.usage.is_none());
    let history: serde_json::Value = serde_json::from_str(&result.history_json).expect("json");
    assert_eq!(history[0]["role"], "user");
    assert_eq!(history[1]["content"], "hello");

    let requests = stub.requests();
    assert_eq!(requests[1].path, "/v1/agents/sessions/sess-3/events");
    let body: serde_json::Value = serde_json::from_str(&requests[1].body).expect("json");
    assert_eq!(body["input"], "hi");
}

#[test]
fn run_without_output_reports_an_empty_reply() {
    let stub = Stub::start(vec![Canned::json(json!({ "id": "turn-2" }))]);
    let agent = make_agent(&stub.url(), |_| {});
    let session = agent.session("sess-4".into()).expect("session");
    let result = agent.run("hi".into(), Some(session)).expect("run");
    assert_eq!(result.final_output, "");
}

#[test]
fn run_streamed_collects_text_usage_and_notifies_the_listener() {
    let sse = concat!(
        "data: {\"type\":\"agent.turn.created\",\"turn_id\":\"t1\"}\n\n",
        ": keepalive comment\n\n",
        "data: {\"type\":\"agent.turn.in_progress\",\"phase\":\"model\",\"label\":\"1 tool\"}\n\n",
        "data: {\"type\":\"agent.turn.item.added\",\"item\":{\"id\":\"call-1\"}}\n\n",
        "data: {\"type\":\"agent.turn.output_text.delta\",\"delta\":\"Hel\"}\n\n",
        "data: {\"type\":\"agent.turn.output_text.delta\",\"delta\":\"lo\"}\n\n",
        "data: {\"type\":\"agent.turn.usage\",\"model\":\"m\",\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}\n\n",
        "data: {\"type\":\"agent.turn.output_text.done\",\"text\":\"Hello\"}\n\n",
        "data: {\"type\":\"agent.turn.completed\",\"turn\":{\"output\":\"Hello\"}}\n\n",
        "data: [DONE]\n\n",
    );
    let stub = Stub::start(vec![Canned::sse(sse)]);
    let agent = make_agent(&stub.url(), |_| {});
    let listener = Arc::new(Recorder::default());
    let session = agent.session("sess-5".into()).expect("session");
    let result = agent
        .run_streamed("hi".into(), Some(session), boxed_listener(&listener), None)
        .expect("run");

    assert_eq!(result.final_output, "Hello");
    assert_eq!(result.turn_id.as_deref(), Some("t1"));
    let usage = result.usage.expect("usage");
    assert_eq!(usage.input_tokens, 3);
    assert_eq!(usage.output_tokens, 2);
    assert_eq!(usage.total_tokens, 5);
    assert_eq!(usage.model.as_deref(), Some("m"));

    let events = listener.events.lock().unwrap();
    assert_eq!(events.len(), 8, "every frame reaches the listener");
    assert_eq!(events[0].kind, "agent.turn.created");
    assert_eq!(events[1].phase.as_deref(), Some("model"));
    assert_eq!(events[1].label.as_deref(), Some("1 tool"));
    assert!(events[2].item_json.is_some());
    assert_eq!(events[3].delta.as_deref(), Some("Hel"));
    assert_eq!(events[6].text.as_deref(), Some("Hello"));
    assert_eq!(events[7].output.as_deref(), Some("Hello"));
    let history: serde_json::Value = serde_json::from_str(&result.history_json).expect("json");
    assert_eq!(history[1]["content"], "Hello");
}

#[test]
fn a_completed_frame_alone_carries_the_output() {
    let sse = concat!(
        "data: {\"type\":\"agent.turn.created\"}\n\n",
        "data: {\"type\":\"agent.turn.completed\",\"turn\":{\"output\":\"done\"}}\n\n",
    );
    let stub = Stub::start(vec![Canned::sse(sse)]);
    let agent = make_agent(&stub.url(), |_| {});
    let result = agent
        .run_streamed(
            "hi".into(),
            Some(agent.session("s".into()).expect("session")),
            Box::new(Shared(Arc::new(Recorder::default()))),
            None,
        )
        .expect("run");
    assert_eq!(result.final_output, "done");
    assert!(result.usage.is_none());
}

#[test]
fn a_completion_frame_without_text_leaves_the_transcript_alone() {
    // An empty `done` text and a `completed` frame with no output must both be
    // ignored instead of blanking the accumulated answer.
    let sse = concat!(
        "data: {\"type\":\"agent.turn.created\"}\n\n",
        "data: {\"type\":\"agent.turn.output_text.delta\",\"delta\":\"partial\"}\n\n",
        "data: {\"type\":\"agent.turn.output_text.done\",\"text\":\"\"}\n\n",
        "data: {\"type\":\"agent.turn.completed\"}\n\n",
    );
    let stub = Stub::start(vec![Canned::sse(sse)]);
    let agent = make_agent(&stub.url(), |_| {});
    let result = agent
        .run_streamed(
            "hi".into(),
            Some(agent.session("s".into()).expect("session")),
            Box::new(Shared(Arc::new(Recorder::default()))),
            None,
        )
        .expect("run");
    assert_eq!(result.final_output, "partial");
}

#[test]
fn a_failure_frame_without_a_message_uses_the_default_wording() {
    let sse = concat!(
        "data: {\"type\":\"agent.turn.created\"}\n\n",
        "data: {\"type\":\"agent.turn.failed\"}\n\n",
    );
    let stub = Stub::start(vec![Canned::sse(sse)]);
    let agent = make_agent(&stub.url(), |_| {});
    let err = agent
        .run_streamed(
            "hi".into(),
            Some(agent.session("s".into()).expect("session")),
            Box::new(Shared(Arc::new(Recorder::default()))),
            None,
        )
        .unwrap_err();
    assert_eq!(err.kind(), "api");
    assert_eq!(err.status(), Some(200));
    assert!(err.to_string().contains("run failed"), "{err}");
}

#[test]
fn the_default_memory_backend_is_cloud() {
    let stub = Stub::start(vec![]);
    let agent = make_agent(&stub.url(), |_| {});
    let session = agent.session("s".into()).expect("session");
    assert_eq!(session.memory_backend(), "cloud");
}

#[test]
fn a_streamed_failure_frame_is_a_run_error() {
    let sse = concat!(
        "data: {\"type\":\"agent.turn.created\",\"turn_id\":\"t2\"}\n\n",
        "data: {\"type\":\"agent.turn.failed\",\"error\":{\"message\":\"model exploded\"}}\n\n",
    );
    let stub = Stub::start(vec![Canned::sse(sse)]);
    let agent = make_agent(&stub.url(), |_| {});
    let err = agent
        .run_streamed(
            "hi".into(),
            Some(agent.session("s".into()).expect("session")),
            Box::new(Shared(Arc::new(Recorder::default()))),
            None,
        )
        .unwrap_err();
    assert!(err.to_string().contains("model exploded"), "{err}");
    assert_eq!(err.kind(), "api");
}

#[test]
fn max_turns_is_enforced_and_validated() {
    let stub = Stub::start(vec![
        Canned::json(json!({ "id": "s" })),
        Canned::sse("data: {\"type\":\"agent.turn.created\"}\n\ndata: {\"type\":\"agent.turn.created\"}\n\n"),
    ]);
    let agent = make_agent(&stub.url(), |_| {});
    let err = agent
        .run_streamed(
            "hi".into(),
            None,
            Box::new(Shared(Arc::new(Recorder::default()))),
            Some(1),
        )
        .unwrap_err();
    assert!(err.to_string().contains("maxTurns"), "{err}");

    let stub2 = Stub::start(vec![]);
    let agent2 = make_agent(&stub2.url(), |_| {});
    let err = agent2
        .run_streamed(
            "hi".into(),
            None,
            Box::new(Shared(Arc::new(Recorder::default()))),
            Some(0),
        )
        .unwrap_err();
    assert!(matches!(err, AriaError::Config { .. }), "{err}");
}

#[test]
fn an_empty_stream_is_an_error_instead_of_an_empty_answer() {
    let stub = Stub::start(vec![Canned::sse("")]);
    let agent = make_agent(&stub.url(), |_| {});
    let err = agent
        .run_streamed(
            "hi".into(),
            Some(agent.session("s".into()).expect("session")),
            Box::new(Shared(Arc::new(Recorder::default()))),
            None,
        )
        .unwrap_err();
    assert!(err.to_string().contains("without any event"), "{err}");
}

#[test]
fn a_streamed_read_error_is_reported() {
    // A canned body that is not JSON: the frame cannot be decoded.
    let stub = Stub::start(vec![Canned::sse("data: not-json\n\n")]);
    let agent = make_agent(&stub.url(), |_| {});
    let err = agent
        .run_streamed(
            "hi".into(),
            Some(agent.session("s".into()).expect("session")),
            Box::new(Shared(Arc::new(Recorder::default()))),
            None,
        )
        .unwrap_err();
    assert!(err.to_string().contains("stream frame"), "{err}");
}

#[test]
fn cloud_memory_round_trips_through_the_session_endpoints() {
    let stub = Stub::start(vec![
        Canned::json(json!({ "ok": true })),
        Canned::json(json!({ "value": "Ada" })),
        Canned::json(json!({ "value": null })),
    ]);
    let agent = make_agent(&stub.url(), |_| {});
    let session = agent.session("sess 6/x".into()).expect("session");
    session
        .memorize("user name".into(), "Ada".into(), None)
        .expect("memorize");
    assert_eq!(
        session.recall("user name".into(), None).expect("recall"),
        Some("Ada".into())
    );
    assert_eq!(
        session.recall("missing".into(), None).expect("recall"),
        None
    );

    let requests = stub.requests();
    assert_eq!(requests[0].path, "/v1/agents/sessions/sess%206%2Fx/memory");
    let body: serde_json::Value = serde_json::from_str(&requests[0].body).expect("json");
    assert_eq!(body["key"], "user name");
    assert_eq!(body["value"], "Ada");
    assert_eq!(body["kind"], "long_term");
    assert_eq!(
        requests[1].path,
        "/v1/agents/sessions/sess%206%2Fx/memory/user%20name"
    );
}

#[test]
fn a_per_call_backend_override_wins() {
    let stub = Stub::start(vec![Canned::json(json!({ "value": "Ada" }))]);
    let agent = make_agent(&stub.url(), |c| {
        c.memory = Some(MemoryConfig {
            backend: Some("local".into()),
            memo_bin: Some("aria-memo-does-not-exist".into()),
            memo_db: None,
        });
    });
    let session = agent.session("s".into()).expect("session");
    assert_eq!(
        session
            .recall("user".into(), Some("cloud".into()))
            .expect("recall"),
        Some("Ada".into())
    );
    // An unknown override fails before touching any backend.
    let err = session
        .recall("user".into(), Some("nowhere".into()))
        .unwrap_err();
    assert!(matches!(err, AriaError::Config { .. }), "{err}");
}

#[test]
fn read_only_session_endpoints_return_json() {
    let stub = Stub::start(vec![
        Canned::json(json!([{ "id": "i1" }])),
        Canned::json(json!([{ "id": "t1" }])),
        Canned::json(json!({ "title": "hello" })),
    ]);
    let agent = make_agent(&stub.url(), |_| {});
    let session = agent.session("sess-7".into()).expect("session");
    assert!(session.items_json().expect("items").contains("i1"));
    assert!(session.turns_json().expect("turns").contains("t1"));
    assert!(session.title_json().expect("title").contains("hello"));
    let paths: Vec<String> = stub.requests().into_iter().map(|r| r.path).collect();
    assert_eq!(
        paths,
        vec![
            "/v1/agents/sessions/sess-7/items",
            "/v1/agents/sessions/sess-7/turns",
            "/v1/agents/sessions/sess-7/title",
        ]
    );
}

#[test]
fn a_stream_endpoint_error_is_reported() {
    let stub = Stub::start(vec![Canned::status(500, r#"{"message":"boom"}"#)]);
    let agent = make_agent(&stub.url(), |_| {});
    let err = agent
        .run_streamed(
            "hi".into(),
            Some(agent.session("s".into()).expect("session")),
            Box::new(Shared(Arc::new(Recorder::default()))),
            None,
        )
        .unwrap_err();
    match &err {
        AriaError::Api {
            status,
            detail: message,
        } => {
            assert_eq!(*status, 500);
            assert_eq!(message, "boom");
        }
        other => assert_eq!(other.kind(), "api"),
    }
}

#[test]
fn both_backends_prefer_the_cloud_copy() {
    let stub = Stub::start(vec![
        Canned::json(json!({ "ok": true })),
        Canned::json(json!({ "value": "Cloud" })),
    ]);
    let agent = make_agent(&stub.url(), |c| {
        c.memory = Some(MemoryConfig {
            backend: Some("both".into()),
            // The local side is unavailable, so the cloud copy has to answer.
            memo_bin: Some("aria-memo-does-not-exist".into()),
            memo_db: None,
        });
    });
    let session = agent.session("s".into()).expect("session");
    session
        .memorize("user".into(), "Ada".into(), None)
        .expect("cloud write");
    assert_eq!(
        session.recall("user".into(), None).expect("recall"),
        Some("Cloud".into())
    );
    assert_eq!(session.memory_backend(), "both");
}

#[test]
fn a_rejected_key_is_an_auth_error() {
    let stub = Stub::start(vec![Canned::status(
        401,
        r#"{"error":{"message":"invalid key"}}"#,
    )]);
    let agent = make_agent(&stub.url(), |_| {});
    let err = agent.create_session().err().expect("must fail");
    assert_eq!(err.kind(), "auth");
    assert!(err.to_string().contains("invalid key"), "{err}");
}

#[test]
fn an_unreachable_server_is_a_network_error() {
    // Bind and immediately drop so the port is (almost certainly) closed.
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.local_addr().expect("addr").port()
    };
    let agent = make_agent(&format!("http://127.0.0.1:{port}"), |_| {});
    let err = agent.create_session().err().expect("must fail");
    assert_eq!(err.kind(), "network");
    assert_eq!(err.status(), None);
}

#[test]
fn a_malformed_body_is_an_unknown_error() {
    let stub = Stub::start(vec![Canned::status(200, "not json")]);
    let agent = make_agent(&stub.url(), |_| {});
    let err = agent.create_session().err().expect("must fail");
    assert_eq!(err.kind(), "unknown");
    assert_eq!(err.status(), None);
}

#[test]
fn events_are_decoded_for_every_modelled_kind() {
    let created = decode_event(&json!({"type":"agent.turn.created","turn_id":"t"}));
    assert_eq!(created.kind, "agent.turn.created");
    assert_eq!(created.turn_id.as_deref(), Some("t"));

    let approval = decode_event(&json!({
        "type":"agent.turn.approval.resolved",
        "tool":"shell","decision":"allow","reason":"policy"
    }));
    assert_eq!(approval.tool.as_deref(), Some("shell"));
    assert_eq!(approval.decision.as_deref(), Some("allow"));
    assert_eq!(approval.reason.as_deref(), Some("policy"));

    let failed = decode_event(&json!({"type":"agent.turn.failed","error":{"message":"x"}}));
    assert_eq!(failed.error.as_deref(), Some("x"));

    let unknown = decode_event(&json!({"type":"agent.whatever"}));
    assert_eq!(unknown.kind, "agent.whatever");
    assert!(unknown.raw_json.contains("agent.whatever"));

    let bare = decode_event(&json!({}));
    assert_eq!(bare.kind, "");
}

#[test]
fn a_poisoned_history_lock_skips_the_mirror_instead_of_panicking() {
    let stub = Stub::start(vec![]);
    let agent = make_agent(&stub.url(), |_| {});
    let session = agent.session("s".into()).expect("session");
    // The only way to poison a `Mutex` is to panic while holding it.
    let holder = Arc::clone(&session);
    let poisoned = std::thread::spawn(move || {
        let _guard = holder.history.lock().expect("lock");
        panic!("poison the history lock");
    })
    .join();
    assert!(poisoned.is_err());

    session.record("hi", "there");
    assert_eq!(session.history_json(), "[]");
}

#[test]
fn a_max_turns_failure_maps_to_config_and_other_failures_to_api() {
    let config = failure_error("exceeded maxTurns (2)".into());
    assert!(matches!(config, AriaError::Config { .. }), "{config}");
    let api = failure_error("model exploded".into());
    match api {
        AriaError::Api {
            status,
            detail: message,
        } => {
            assert_eq!(status, 200);
            assert_eq!(message, "model exploded");
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn usage_falls_back_to_the_sum_when_total_is_absent() {
    let usage = frame_usage(&json!({"usage":{"input_tokens":4,"output_tokens":6}})).expect("usage");
    assert_eq!(usage.total_tokens, 10);
    assert!(usage.model.is_none());
    assert!(frame_usage(&json!({"type":"x"})).is_none());
}
