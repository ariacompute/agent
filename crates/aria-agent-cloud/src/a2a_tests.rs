//! End-to-end and unit tests for the A2A surface (ADR-0028).
//!
//! Everything runs through the same pieces production runs: the real router
//! (`build_router`), the real in-memory stores and a stub model. No container,
//! no database, no listening socket — the dsh side is pinned by the
//! `SessionTag` / `RoutingBridge` / `evict` unit tests and `just dsh-smoke`.
//!
//! The split is deliberate: HTTP tests prove the wire contract (status codes,
//! JSON-RPC envelopes, SSE frames, scopes, tenant isolation); direct calls pin
//! the branches a client cannot reach (a store that fails, an answer that does
//! not match the pending request, an event payload that is not a frame).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent_core::seams::{ApprovalBroker, UserQuestions};
use agent_core::{CoreError, ModelClient, ModelRequest, ModelResponse, ModelTurn, Tool, ToolCall};
use aria_agent_a2a::jsonrpc::code;
use aria_agent_a2a::types::{
    Artifact, PushNotificationConfig, StreamFrame, TaskStatus, TaskStatusUpdateEvent,
};
use aria_agent_a2a::TaskState;
use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tower::ServiceExt;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::a2a::card::CardOverrides;
use crate::a2a::hitl::{
    parse_decision, HitlContext, PausingHitl, ResumeError, ResumeInput, RoutingBridge,
};
use crate::a2a::push::{
    validate_push_url, HttpPushNotifier, NoopPush, PushNotifier, MAX_ATTEMPTS, PUSH_TIMEOUT,
};
use crate::a2a::runtime::{A2aRuntime, FRAME_CAPACITY};
use crate::a2a::store::{A2aEventRecord, A2aTaskRow, A2aTaskStore, MemoryA2aTasks};
use crate::a2a::{A2aConfig, A2aState};
use crate::api_tests::{
    create_agent, env_admin, harness, harness_with, parse_sse, send, send_bytes, FailingModel,
    Harness, ToolCallModel, ADMIN_KEY,
};
use crate::app::build_router;
use crate::auth::{ApiKeyScopes, Principal, PrincipalKind};
use crate::store::{ItemRow, SessionRow, SessionStore, TurnRow};
use crate::AppError;

// ------------------------------------------------------------------ helpers

/// A harness with the A2A surface on, optionally with a default agent.
fn harness_a2a(agent: Option<&str>) -> Harness {
    let mut harness = harness();
    harness.state.a2a = a2a_state(agent);
    harness.app = build_router(harness.state.clone());
    harness
}

fn a2a_state(default_agent: Option<&str>) -> A2aState {
    A2aState {
        enabled: true,
        config: Arc::new(A2aConfig {
            default_agent: default_agent.map(str::to_string),
            public_url: "https://cloud.example.test/".into(),
            cards: CardOverrides::default(),
        }),
        ..A2aState::disabled()
    }
}

/// Point the harness at `agent` after it was created (ids are server-side).
///
/// Only the default agent changes: the runtime, the task store and the push
/// notifier a test installed are preserved.
fn set_default(harness: &mut Harness, agent: &str) {
    harness.state.a2a = A2aState {
        config: Arc::new(A2aConfig {
            default_agent: Some(agent.to_string()),
            ..(*harness.state.a2a.config).clone()
        }),
        ..harness.state.a2a.clone()
    };
    harness.app = build_router(harness.state.clone());
}

fn msg(text: &str) -> Value {
    json!({
        "kind": "message",
        "role": "user",
        "messageId": "m-1",
        "parts": [{"kind": "text", "type": "text", "text": text}],
    })
}

fn call(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

/// `POST /a2a` as the admin.
async fn rpc(app: &Router, method: &str, params: Value) -> (StatusCode, Value) {
    send(
        app,
        "POST",
        "/a2a",
        Some(ADMIN_KEY),
        Some(call(1, method, params)),
    )
    .await
}

/// A request with a hand-written body, so a malformed envelope is testable.
async fn send_raw(app: &Router, uri: &str, token: Option<&str>, body: &str) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .expect("router should respond");
    let status = response.status();
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .expect("read body")
        .to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn task_state(body: &Value) -> &str {
    body["result"]["status"]["state"].as_str().unwrap_or("")
}

/// Poll `tasks/get` until the task reports `want` (the run is a spawned task).
async fn await_state(app: &Router, task_id: &str, want: &str) -> Value {
    for _ in 0..500 {
        let (status, body) = rpc(app, "tasks/get", json!({"id": task_id})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        if task_state(&body) == want {
            return body["result"].clone();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("task {task_id} never reached {want}");
}

fn user(id: &str) -> Principal {
    Principal {
        id: id.into(),
        kind: PrincipalKind::User,
        scopes: ApiKeyScopes::read_write(),
    }
}

fn read_only(id: &str) -> Principal {
    Principal {
        id: id.into(),
        kind: PrincipalKind::User,
        scopes: ApiKeyScopes {
            admin: false,
            read: true,
            write: false,
        },
    }
}

fn submitted_row(task: &str, owner: &str, context: &str) -> A2aTaskRow {
    A2aTaskRow::submitted(
        task,
        owner,
        "agent-1",
        context,
        "turn-1",
        json!({"state": "submitted"}),
        1_700_000_000_000,
    )
}

fn progress_frame(task: &str) -> StreamFrame {
    StreamFrame::StatusUpdate(TaskStatusUpdateEvent::progress(
        task,
        "sess_1",
        TaskStatus::now(TaskState::Working),
    ))
}

fn final_frame(task: &str) -> StreamFrame {
    StreamFrame::StatusUpdate(TaskStatusUpdateEvent::terminal(
        task,
        "sess_1",
        TaskStatus::with_message(TaskState::Completed, "done"),
    ))
}

fn push(url: &str) -> PushNotificationConfig {
    PushNotificationConfig {
        id: None,
        url: url.into(),
        token: None,
        authentication: None,
    }
}

// -------------------------------------------------------------- discovery

#[tokio::test]
async fn the_well_known_card_is_public_and_carries_no_secrets() {
    let _env = env_admin().await;
    let mut h = harness_a2a(None);
    let agent = create_agent(&h, "Public Agent").await;
    set_default(&mut h, &agent);

    // No Authorization header: an orchestrator discovers before it has a key.
    let (status, bytes) =
        send_bytes(&h.app, "GET", "/.well-known/agent-card.json", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let card: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(card["protocolVersion"], "1.0.0");
    assert_eq!(card["name"], "Public Agent");
    // The base URL's trailing slash is normalised, not doubled.
    assert_eq!(card["url"], "https://cloud.example.test/a2a");
    assert_eq!(card["capabilities"]["streaming"], true);
    assert_eq!(card["capabilities"]["pushNotifications"], true);
    assert_eq!(card["capabilities"]["stateTransitionHistory"], true);
    assert_eq!(card["skills"][0]["id"], "chat");
    assert_eq!(card["authentication"]["schemes"][0], "bearer");
    assert_eq!(card["version"], env!("CARGO_PKG_VERSION"));
    // Never: instructions, tool families, principals or container names.
    let text = String::from_utf8(bytes).unwrap();
    for forbidden in ["instructions", "shell", "browser_", "admin", "sess_"] {
        assert!(!text.contains(forbidden), "{forbidden} leaked into {text}");
    }
    // The compatibility aliases answer the same document.
    for path in ["/.well-known/agent.json", "/a2a/agent-card"] {
        let (status, _) = send_bytes(&h.app, "GET", path, None, None).await;
        assert_eq!(status, StatusCode::OK, "{path}");
    }
    // And the per-agent card points at that agent's endpoint.
    let (status, bytes) = send_bytes(
        &h.app,
        "GET",
        &format!("/v1/agents/{agent}/.well-known/agent-card.json"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let card: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        card["url"],
        format!("https://cloud.example.test/v1/agents/{agent}/a2a")
    );
    // The second per-agent alias too.
    let (status, _) = send_bytes(
        &h.app,
        "GET",
        &format!("/v1/agents/{agent}/agent-card.json"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_card_for_something_that_does_not_exist_is_a_machine_readable_404() {
    let _env = env_admin().await;
    // No default agent configured at all.
    let h = harness_a2a(None);
    let (status, body) = send(&h.app, "GET", "/.well-known/agent-card.json", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "agent_not_found");

    // Configured, but the agent row is gone.
    let h = harness_a2a(Some("agent-missing"));
    for path in [
        "/.well-known/agent.json",
        "/v1/agents/agent-missing/agent-card.json",
    ] {
        let (status, body) = send(&h.app, "GET", path, None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(body["error"]["code"], "agent_not_found");
    }

    // A deployment that never enabled the surface answers the same way.
    let h = harness();
    let (status, _) = send(&h.app, "GET", "/a2a/agent-card", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ------------------------------------------------- envelopes, auth, scopes

#[tokio::test]
async fn the_rpc_endpoint_needs_a_key_and_a_usable_scope() {
    let _env = env_admin().await;
    let h = harness_a2a(None);
    let (status, _) = send(
        &h.app,
        "POST",
        "/a2a",
        None,
        Some(call(1, "tasks/get", json!({}))),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A key that may write but not read can send, not poll.
    let (_, created) = send(
        &h.app,
        "POST",
        "/v1/api-keys",
        Some(ADMIN_KEY),
        Some(json!({"principal_id": "writer", "scopes": ["write"]})),
    )
    .await;
    let write_only = created["key"].as_str().unwrap().to_string();
    let (status, body) = send(
        &h.app,
        "POST",
        "/a2a",
        Some(&write_only),
        Some(call(1, "tasks/get", json!({"id": "task_x"}))),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "insufficient_scope");

    // The mirror image: read-only may poll, not send.
    let (_, created) = send(
        &h.app,
        "POST",
        "/v1/api-keys",
        Some(ADMIN_KEY),
        Some(json!({"principal_id": "reader", "scopes": ["read"]})),
    )
    .await;
    let read_only_key = created["key"].as_str().unwrap().to_string();
    let (status, body) = send(
        &h.app,
        "POST",
        "/a2a",
        Some(&read_only_key),
        Some(call(1, "message/send", json!({"message": msg("hi")}))),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // The replay endpoint is a read.
    let (status, _) = send(
        &h.app,
        "GET",
        "/a2a/stream?task=task_x",
        Some(&write_only),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_broken_envelope_is_a_400_while_a_bad_method_is_a_200_error() {
    let _env = env_admin().await;
    let h = harness_a2a(None);

    let (status, body) = send_raw(&h.app, "/a2a", Some(ADMIN_KEY), "{not json").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], code::PARSE_ERROR);
    assert_eq!(body["error"]["data"]["code"], "parse_error");

    // Valid JSON, but not a JSON-RPC request (`method` is required).
    let (status, body) = send_raw(&h.app, "/a2a", Some(ADMIN_KEY), r#"{"jsonrpc":"2.0"}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], code::INVALID_REQUEST);

    // A method-level failure stays a 200 so the caller reads the code.
    let (status, body) = rpc(&h.app, "tasks/nonsense", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["error"]["code"], code::METHOD_NOT_FOUND);
    assert_eq!(body["error"]["data"]["code"], "method_not_found");
    assert_eq!(body["id"], 1);
}

// ------------------------------------------------------------ message/send

#[tokio::test]
async fn message_send_completes_and_mirrors_the_conversation_into_the_console() {
    let _env = env_admin().await;
    let mut h = harness_a2a(None);
    let agent = create_agent(&h, "sender").await;
    set_default(&mut h, &agent);

    let (status, body) = rpc(&h.app, "message/send", json!({"message": msg("hello")})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let task = &body["result"];
    let task_id = task["id"].as_str().unwrap();
    assert!(task_id.starts_with("task_"), "{task_id}");
    let context = task["contextId"].as_str().unwrap().to_string();
    assert!(context.starts_with("sess_"));
    assert_eq!(task["status"]["state"], "completed");
    assert_eq!(task["history"][0]["role"], "user");
    let artifact = &task["artifacts"][0];
    assert_eq!(artifact["artifactId"], "final");
    assert!(!artifact["parts"][0]["text"].as_str().unwrap().is_empty());
    // A part writes both discriminators: agenthub dispatches on `type`.
    assert_eq!(artifact["parts"][0]["kind"], "text");
    assert_eq!(artifact["parts"][0]["type"], "text");

    // The same conversation is visible through the existing console endpoints.
    let (_, items) = send(
        &h.app,
        "GET",
        &format!("/v1/agents/sessions/{context}/items"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    let items = items["data"].as_array().unwrap();
    assert!(items.iter().any(|item| item["role"] == "user"));
    assert!(items.iter().any(|item| item["role"] == "assistant"));
    let (_, turns) = send(
        &h.app,
        "GET",
        &format!("/v1/agents/sessions/{context}/turns"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(turns["data"][0]["status"], "completed");

    // `tasks/get` sees the same task, and `historyLength` trims the history.
    let (_, full) = rpc(&h.app, "tasks/get", json!({"id": task_id})).await;
    assert_eq!(full["result"]["id"], task_id);
    assert_eq!(full["result"]["status"]["state"], "completed");
    let (_, trimmed) = rpc(
        &h.app,
        "tasks/get",
        json!({"id": task_id, "historyLength": 0}),
    )
    .await;
    // Zero keeps no messages; the field stays present so a reader never has to
    // tell "absent" from "empty".
    assert_eq!(trimmed["result"]["history"], json!([]), "{trimmed}");
    // `taskId` is accepted as the id.
    let (_, aliased) = rpc(&h.app, "tasks/get", json!({"taskId": task_id})).await;
    assert_eq!(aliased["result"]["id"], task_id);

    // The v0.1 method aliases reach the same code.
    let (status, legacy) = rpc(&h.app, "tasks/send", json!({"message": msg("again")})).await;
    assert_eq!(status, StatusCode::OK, "{legacy}");
    assert_eq!(legacy["result"]["status"]["state"], "completed");
    let (_, legacy_stream) = send_bytes(
        &h.app,
        "POST",
        "/a2a",
        Some(ADMIN_KEY),
        Some(call(
            1,
            "tasks/sendSubscribe",
            json!({"message": msg("and again")}),
        )),
    )
    .await;
    assert!(
        String::from_utf8(legacy_stream)
            .unwrap()
            .contains("event: task"),
        "the v0.1 subscribe alias streams"
    );

    // A second turn in the same context reuses the session rather than forking.
    let (status, follow_up) = rpc(
        &h.app,
        "message/send",
        json!({"message": {"kind": "message", "role": "user", "parts": [{"kind": "text", "text": "more"}], "contextId": context}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{follow_up}");
    assert_eq!(follow_up["result"]["contextId"], context);
}

#[tokio::test]
async fn message_send_refuses_an_empty_message_an_unknown_agent_and_a_failure() {
    let _env = env_admin().await;
    let h = harness_a2a(None);
    // No default agent.
    let (status, body) = rpc(&h.app, "message/send", json!({"message": msg("hi")})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["error"]["code"], code::AGENT_NOT_FOUND);

    let agent = create_agent(&h, "empty-parts").await;
    let path = format!("/v1/agents/{agent}/a2a");

    // A deployment that disabled the surface answers the same way.
    let disabled = harness();
    let (_, body) = send(
        &disabled.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(1, "message/send", json!({"message": msg("hi")}))),
    )
    .await;
    assert_eq!(body["error"]["code"], code::AGENT_NOT_FOUND);

    // Unknown agent on the per-agent route.
    let (_, body) = send(
        &h.app,
        "POST",
        "/v1/agents/agent-missing/a2a",
        Some(ADMIN_KEY),
        Some(call(1, "message/send", json!({"message": msg("hi")}))),
    )
    .await;
    assert_eq!(body["error"]["code"], code::AGENT_NOT_FOUND);

    // Missing `params.message`.
    let (_, body) = send(
        &h.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(1, "message/send", json!({}))),
    )
    .await;
    assert_eq!(body["error"]["code"], code::INVALID_PARAMS);

    // A message with no text part.
    let (_, body) = send(
        &h.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(
            1,
            "message/send",
            json!({"message": {"role": "user", "parts": []}}),
        )),
    )
    .await;
    assert_eq!(body["error"]["code"], code::INVALID_PARAMS);

    // A push URL that is not http(s) is refused before anything is started.
    let (_, body) = send(
        &h.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(
            1,
            "message/send",
            json!({"message": msg("hi"), "configuration": {"pushNotification": {"url": "ftp://x/y"}}}),
        )),
    )
    .await;
    assert_eq!(body["error"]["code"], code::INVALID_PARAMS);

    // A model failure is a settled task, not a protocol error.
    let failing = harness_with(|| Box::new(FailingModel));
    let mut failing = failing;
    failing.state.a2a = h.state.a2a.clone();
    failing.app = build_router(failing.state.clone());
    let agent = create_agent(&failing, "failing").await;
    let (status, body) = send(
        &failing.app,
        "POST",
        &format!("/v1/agents/{agent}/a2a"),
        Some(ADMIN_KEY),
        Some(call(1, "message/send", json!({"message": msg("boom")}))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["status"]["state"], "failed");
    assert!(body["result"]["status"]["message"]["parts"][0]["text"]
        .as_str()
        .unwrap()
        .contains("model exploded"));
}

// ---------------------------------------------------------- message/stream

#[tokio::test]
async fn message_stream_frames_the_run_and_ends_without_a_sentinel() {
    let _env = env_admin().await;
    let h = harness_a2a(None);
    let agent = create_agent(&h, "streamer").await;
    let (status, bytes) = send_bytes(
        &h.app,
        "POST",
        &format!("/v1/agents/{agent}/a2a"),
        Some(ADMIN_KEY),
        Some(call(1, "message/stream", json!({"message": msg("hello")}))),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(bytes).unwrap();
    assert!(!text.contains("[DONE]"), "no sentinel allowed: {text}");
    assert!(text.contains("event: task\n"), "{text}");
    let frames = parse_sse(&text);
    assert!(frames.len() >= 2, "{text}");
    // Every frame is a JSON-RPC result carrying the request id.
    for frame in &frames {
        assert_eq!(frame["jsonrpc"], "2.0");
        assert_eq!(frame["id"], 1);
    }
    assert_eq!(frames[0]["result"]["kind"], "task");
    let last = frames.last().unwrap();
    assert_eq!(last["result"]["kind"], "status-update");
    assert_eq!(last["result"]["final"], true);
    assert_eq!(last["result"]["status"]["state"], "completed");
    // The legacy dispatch names travel with the frames.
    assert!(text.contains("event: task_artifact_update"), "{text}");
    assert!(text.contains("event: task_status_update"), "{text}");

    // A bad message never opens a stream: the error is plain JSON. Resolve the
    // agent first, or the missing default would answer `-32004` instead.
    let (status, body) = send(
        &h.app,
        "POST",
        &format!("/v1/agents/{agent}/a2a"),
        Some(ADMIN_KEY),
        Some(call(
            1,
            "message/stream",
            json!({"message": {"role": "user", "parts": []}}),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["error"]["code"], code::INVALID_PARAMS);
    // Neither is a request for an agent that does not exist.
    let (_, body) = send(
        &h.app,
        "POST",
        "/v1/agents/agent-missing/a2a",
        Some(ADMIN_KEY),
        Some(call(1, "message/stream", json!({"message": msg("hi")}))),
    )
    .await;
    assert_eq!(body["error"]["code"], code::AGENT_NOT_FOUND);
    // Nor one with an unusable push target.
    let (_, body) = send(
        &h.app,
        "POST",
        &format!("/v1/agents/{agent}/a2a"),
        Some(ADMIN_KEY),
        Some(call(
            1,
            "message/stream",
            json!({"message": msg("hi"), "configuration": {"pushNotification": {"url": "nope"}}}),
        )),
    )
    .await;
    assert_eq!(body["error"]["code"], code::INVALID_PARAMS);
}

#[tokio::test]
async fn an_answer_that_never_streamed_is_emitted_as_one_chunk() {
    use agent_core::seams::{AgentLoop, LoopContext};

    /// A harness that reports only the final text, like a non-token runtime.
    struct DoneOnly;

    #[async_trait]
    impl AgentLoop for DoneOnly {
        async fn run(
            &self,
            _ctx: LoopContext,
            _input: &str,
        ) -> Result<
            futures::stream::BoxStream<'static, Result<agent_core::AgentEvent, CoreError>>,
            CoreError,
        > {
            let stream = async_stream::stream! {
                yield Ok(agent_core::AgentEvent::Done { text: "one shot".into() });
            };
            Ok(Box::pin(stream))
        }
    }

    let _env = env_admin().await;
    struct HarnessFlag(&'static str);
    impl Drop for HarnessFlag {
        fn drop(&mut self) {
            std::env::remove_var(self.0);
        }
    }
    std::env::set_var("AGENT_HARNESS", "dsh");
    let _flag = HarnessFlag("AGENT_HARNESS");

    let mut h = harness_a2a(None);
    h.state.harness = Some(Arc::new(
        |_p: &Principal, _cfg: &agent_core::AgentConfig| Arc::new(DoneOnly) as Arc<dyn AgentLoop>,
    ));
    h.app = build_router(h.state.clone());
    let agent = create_agent(&h, "done-only").await;

    let (status, body) = send(
        &h.app,
        "POST",
        &format!("/v1/agents/{agent}/a2a"),
        Some(ADMIN_KEY),
        Some(call(1, "message/send", json!({"message": msg("hi")}))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["status"]["state"], "completed");
    assert_eq!(
        body["result"]["artifacts"][0]["parts"][0]["text"],
        "one shot"
    );

    // The stream carries exactly one artifact chunk for that answer.
    let (_, bytes) = send_bytes(
        &h.app,
        "POST",
        &format!("/v1/agents/{agent}/a2a"),
        Some(ADMIN_KEY),
        Some(call(1, "message/stream", json!({"message": msg("again")}))),
    )
    .await;
    let text = String::from_utf8(bytes).unwrap();
    let chunks = text.matches("event: task_artifact_update").count();
    assert_eq!(chunks, 1, "{text}");
}

// ------------------------------------------------- tasks/cancel + resume

#[tokio::test]
async fn a_parked_approval_can_be_allowed_and_the_task_finishes() {
    let _env = env_admin().await;
    let mut h = harness_with(|| Box::new(ToolCallModel));
    h.state.a2a = a2a_state(None);
    let cancelled = Arc::new(AtomicUsize::new(0));
    let record = cancelled.clone();
    h.state.cancel_harness = Some(Arc::new(move |_principal: Principal, session: String| {
        let record = record.clone();
        Box::pin(async move {
            assert!(!session.is_empty());
            record.fetch_add(1, Ordering::SeqCst);
        })
    }));
    h.app = build_router(h.state.clone());
    let agent = create_agent(&h, "hitl").await;
    let path = format!("/v1/agents/{agent}/a2a");

    // The shell tool needs permission, so the run parks on `input-required`.
    // The push target is registered up front so parking exercises the
    // notification path too (the no-op notifier accepts it instantly).
    let (status, body) = send(
        &h.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(
            1,
            "message/send",
            json!({
                "message": msg("run it"),
                "configuration": {"pushNotification": {"url": "https://hook.example.test/a2a"}},
            }),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let task = &body["result"];
    let task_id = task["id"].as_str().unwrap().to_string();
    let context = task["contextId"].as_str().unwrap().to_string();
    assert_eq!(task["status"]["state"], "input-required", "{task}");
    assert!(task["status"]["message"]["parts"][0]["text"]
        .as_str()
        .unwrap()
        .contains("approval required for `shell`"));

    // A second live task in the same context is refused, not interleaved.
    let (_, busy) = send(
        &h.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(
            1,
            "message/send",
            json!({"message": {"role": "user", "parts": [{"kind": "text", "text": "again"}], "contextId": context}}),
        )),
    )
    .await;
    assert_eq!(busy["error"]["code"], code::INVALID_STATE_TRANSITION);

    // An answer that is not a decision cannot resume an approval.
    let (_, bad) = send(
        &h.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(
            1,
            "tasks/resume",
            json!({"id": task_id, "metadata": {"answer": "purple"}}),
        )),
    )
    .await;
    assert_eq!(bad["error"]["code"], code::INVALID_PARAMS);

    // `allow` resumes it; the run then reaches a terminal state on its own.
    let (status, resumed) = send(
        &h.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(
            1,
            "tasks/resume",
            json!({"id": task_id, "metadata": {"decision": "allow"}}),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resumed}");
    assert_eq!(resumed["result"]["status"]["state"], "working");
    let done = await_state(&h.app, &task_id, "completed").await;
    assert!(done["artifacts"][0]["parts"][0]["text"]
        .as_str()
        .unwrap()
        .contains("done after tool"));

    // A settled task cannot be resumed again.
    let (_, again) = send(
        &h.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(
            1,
            "tasks/resume",
            json!({"id": task_id, "message": msg("allow")}),
        )),
    )
    .await;
    assert_eq!(again["error"]["code"], code::TASK_NOT_RESUMABLE);

    // Nor cancelled.
    let (_, settled) = send(
        &h.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(1, "tasks/cancel", json!({"id": task_id}))),
    )
    .await;
    assert_eq!(settled["error"]["code"], code::TASK_NOT_CANCELABLE);
    assert_eq!(cancelled.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancelling_a_live_task_stops_it_and_evicts_its_harness_session() {
    let _env = env_admin().await;
    let mut h = harness_with(|| Box::new(ToolCallModel));
    h.state.a2a = a2a_state(None);
    let evicted: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let record = evicted.clone();
    h.state.cancel_harness = Some(Arc::new(move |principal: Principal, session: String| {
        let record = record.clone();
        Box::pin(async move {
            record
                .lock()
                .unwrap()
                .push(format!("{}/{}", principal.id, session));
        })
    }));
    h.app = build_router(h.state.clone());
    let agent = create_agent(&h, "cancellable").await;
    let path = format!("/v1/agents/{agent}/a2a");

    let (_, body) = send(
        &h.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(1, "message/send", json!({"message": msg("run it")}))),
    )
    .await;
    let task_id = body["result"]["id"].as_str().unwrap().to_string();
    let context = body["result"]["contextId"].as_str().unwrap().to_string();
    assert_eq!(body["result"]["status"]["state"], "input-required");

    // Arm a push target so the cancel path also notifies its subscriber.
    let (_, set) = send(
        &h.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(
            1,
            "tasks/pushNotification/set",
            json!({"taskId": task_id, "pushNotificationConfig": {"url": "https://hook.example.test/x"}}),
        )),
    )
    .await;
    assert_eq!(set["result"]["url"], "https://hook.example.test/x");

    let (status, cancelled) = send(
        &h.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(1, "tasks/cancel", json!({"taskId": task_id}))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{cancelled}");
    assert_eq!(cancelled["result"]["status"]["state"], "canceled");
    let seen = evicted.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![format!("admin/{context}")],
        "the session runtime is evicted"
    );

    // Cancelling again is not an error the caller has to interpret.
    let (_, twice) = send(
        &h.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(1, "tasks/cancel", json!({"id": task_id}))),
    )
    .await;
    assert_eq!(twice["error"]["code"], code::TASK_NOT_CANCELABLE);

    // An unknown task is not found, and `id` is required.
    let (_, unknown) = send(
        &h.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(1, "tasks/cancel", json!({"id": "task_nope"}))),
    )
    .await;
    assert_eq!(unknown["error"]["code"], code::TASK_NOT_FOUND);
    let (_, missing) = send(
        &h.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(1, "tasks/cancel", json!({}))),
    )
    .await;
    assert_eq!(missing["error"]["code"], code::INVALID_PARAMS);
}

#[tokio::test]
async fn a_parked_question_can_be_answered_and_a_decision_does_not_fit_it() {
    let _env = env_admin().await;
    let mut h = harness_with(|| Box::new(AskModel));
    h.state.a2a = a2a_state(None);
    h.app = build_router(h.state.clone());
    let agent = create_agent(&h, "questioner").await;
    let path = format!("/v1/agents/{agent}/a2a");

    let (_, body) = send(
        &h.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(1, "message/send", json!({"message": msg("ask")}))),
    )
    .await;
    let task_id = body["result"]["id"].as_str().unwrap().to_string();
    assert_eq!(body["result"]["status"]["state"], "input-required");
    assert!(body["result"]["status"]["message"]["parts"][0]["text"]
        .as_str()
        .unwrap()
        .contains("which colour?"));

    // An approval sent to a question is a mismatch and leaves it parked.
    let (_, mismatch) = send(
        &h.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(
            1,
            "tasks/resume",
            json!({"id": task_id, "metadata": {"decision": "allow"}}),
        )),
    )
    .await;
    assert_eq!(mismatch["error"]["code"], code::INVALID_PARAMS);
    let (_, still) = rpc(&h.app, "tasks/get", json!({"id": task_id})).await;
    assert_eq!(still["result"]["status"]["state"], "input-required");

    // A free-text answer continues the run. "yes" is a plain answer here, not an
    // approval decision: only `metadata.decision` means a decision.
    let (status, resumed) = send(
        &h.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(
            1,
            "tasks/resume",
            json!({"id": task_id, "message": msg("yes")}),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resumed}");
    assert_eq!(resumed["result"]["status"]["state"], "working");
    await_state(&h.app, &task_id, "completed").await;

    // No message and no metadata is not an answer at all.
    let (_, body) = send(
        &h.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(1, "tasks/resume", json!({"id": task_id}))),
    )
    .await;
    assert_eq!(body["error"]["code"], code::INVALID_PARAMS);
    let (_, body) = send(
        &h.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(
            1,
            "tasks/resume",
            json!({"id": task_id, "metadata": {"decision": "maybe"}}),
        )),
    )
    .await;
    assert_eq!(body["error"]["code"], code::INVALID_PARAMS);

    // Unknown task, and a request that names none.
    let (_, body) = send(
        &h.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(
            1,
            "tasks/resume",
            json!({"id": "task_nope", "message": msg("x")}),
        )),
    )
    .await;
    assert_eq!(body["error"]["code"], code::TASK_NOT_FOUND);
    let (_, body) = send(
        &h.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(1, "tasks/resume", json!({}))),
    )
    .await;
    assert_eq!(body["error"]["code"], code::INVALID_PARAMS);
}

// --------------------------------------------------- push notifications

#[tokio::test]
async fn push_config_can_be_set_read_and_is_used_on_completion() {
    let _env = env_admin().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .and(header("authorization", "Bearer poke"))
        .and(body_partial_json(json!({"state": "completed"})))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let mut h = harness_a2a(None);
    h.state.a2a = A2aState {
        push: Arc::new(HttpPushNotifier::new().unwrap()),
        ..h.state.a2a.clone()
    };
    let agent = create_agent(&h, "pusher").await;
    set_default(&mut h, &agent);
    let path = format!("/v1/agents/{agent}/a2a");

    // A task with no config answers `-32006`.
    let (_, body) = send(
        &h.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(1, "message/send", json!({"message": msg("first")}))),
    )
    .await;
    let task_id = body["result"]["id"].as_str().unwrap().to_string();
    let (_, none) = send(
        &h.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(
            1,
            "tasks/pushNotification/get",
            json!({"taskId": task_id}),
        )),
    )
    .await;
    assert_eq!(none["error"]["code"], code::RESOURCE_NOT_FOUND);

    let url = format!("{}/hook", server.uri());
    let (status, set) = send(
        &h.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(
            1,
            "tasks/pushNotification/set",
            json!({"taskId": task_id, "pushNotificationConfig": {"url": url, "token": "poke"}}),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{set}");
    assert_eq!(set["result"]["url"], url);
    let (_, got) = send(
        &h.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(
            1,
            "tasks/pushNotification/get",
            json!({"id": task_id}),
        )),
    )
    .await;
    assert_eq!(got["result"]["token"], "poke");

    // Bad configs are refused, and the v0.1 spelling of the object is accepted.
    let (_, bad_url) = send(
        &h.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(
            1,
            "tasks/pushNotification/set",
            json!({"taskId": task_id, "pushNotificationConfig": {"url": "file:///etc/passwd"}}),
        )),
    )
    .await;
    assert_eq!(bad_url["error"]["code"], code::INVALID_PARAMS);
    let (_, missing) = send(
        &h.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(
            1,
            "tasks/pushNotification/set",
            json!({"taskId": task_id}),
        )),
    )
    .await;
    assert_eq!(missing["error"]["code"], code::INVALID_PARAMS);
    let (_, no_task) = send(
        &h.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(1, "tasks/pushNotification/set", json!({"id": ""}))),
    )
    .await;
    assert_eq!(no_task["error"]["code"], code::INVALID_PARAMS);
    let (_, unknown) = send(
        &h.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(
            1,
            "tasks/pushNotification/set",
            json!({"taskId": "task_nope", "pushNotification": {"url": "https://example.test/x"}}),
        )),
    )
    .await;
    assert_eq!(unknown["error"]["code"], code::TASK_NOT_FOUND);
    let (_, unknown_get) = send(
        &h.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(1, "tasks/pushNotification/get", json!({}))),
    )
    .await;
    assert_eq!(unknown_get["error"]["code"], code::INVALID_PARAMS);
    let (_, unknown_get) = send(
        &h.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(
            1,
            "tasks/pushNotification/get",
            json!({"taskId": "task_nope"}),
        )),
    )
    .await;
    assert_eq!(unknown_get["error"]["code"], code::TASK_NOT_FOUND);

    // A completion with the config armed really posts to the URL.
    let (status, body) = send(
        &h.app,
        "POST",
        &path,
        Some(ADMIN_KEY),
        Some(call(
            1,
            "message/send",
            json!({
                "message": msg("notify me"),
                "configuration": {"pushNotification": {"url": url, "token": "poke"}},
            }),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["status"]["state"], "completed");
    // The run publishes the terminal frame before awaiting the POST, so the
    // caller can return first: wait for the delivery, don't assume it landed.
    let mut delivered = 0;
    for _ in 0..500 {
        delivered = server.received_requests().await.unwrap_or_default().len();
        if delivered > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(delivered, 1, "the completion was never pushed");
    drop(server);
}

// --------------------------------------------------------- GET /a2a/stream

#[tokio::test]
async fn the_replay_stream_serves_a_finished_task_without_an_envelope() {
    let _env = env_admin().await;
    let mut h = harness_a2a(None);
    let agent = create_agent(&h, "replay").await;
    set_default(&mut h, &agent);
    let (_, body) = rpc(&h.app, "message/send", json!({"message": msg("replay me")})).await;
    let task_id = body["result"]["id"].as_str().unwrap().to_string();

    let (status, bytes) = send_bytes(
        &h.app,
        "GET",
        &format!("/a2a/stream?task={task_id}"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(bytes).unwrap();
    assert!(!text.contains("[DONE]"), "{text}");
    assert!(
        !text.contains("jsonrpc"),
        "no envelope on the replay path: {text}"
    );
    assert!(text.contains("event: task_status_update"), "{text}");
    let frames = parse_sse(&text);
    assert_eq!(frames[0]["kind"], "task");
    assert_eq!(frames.last().unwrap()["status"]["state"], "completed");
    assert_eq!(frames.last().unwrap()["final"], true);

    // `since` past the end is an empty stream, and a missing task is a 400.
    let (status, bytes) = send_bytes(
        &h.app,
        "GET",
        &format!("/a2a/stream?task={task_id}&since=9999"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(bytes.is_empty(), "{bytes:?}");
    let (status, body) = send(&h.app, "GET", "/a2a/stream", Some(ADMIN_KEY), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_params");
    let (status, body) = send(
        &h.app,
        "GET",
        "/a2a/stream?task=task_nope",
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "task_not_found");
}

#[tokio::test]
async fn the_replay_stream_follows_a_live_task_and_drops_a_repeated_frame() {
    let _env = env_admin().await;
    let h = harness_a2a(None);
    let admin = Principal::admin();
    let runtime = h.state.a2a.runtime.clone();
    let tasks = h.state.a2a.tasks.clone();

    tasks
        .create(&admin, &submitted_row("task_live", &admin.id, "sess_live"))
        .await
        .unwrap();
    let first = StreamFrame::StatusUpdate(TaskStatusUpdateEvent::progress(
        "task_live",
        "sess_live",
        TaskStatus::now(TaskState::Working),
    ));
    tasks
        .append_event(&admin, "task_live", "status", &json!(first))
        .await
        .unwrap();
    // An event that is not a frame is skipped, not fatal.
    tasks
        .append_event(&admin, "task_live", "junk", &json!({"kind": "bogus"}))
        .await
        .unwrap();

    let channel = runtime.open("task_live", "sess_live").await.unwrap();
    drop(channel.receiver);
    drop(channel.frames);

    let app = h.app.clone();
    let handle = tokio::spawn(async move {
        send_bytes(
            &app,
            "GET",
            "/a2a/stream?task=task_live",
            Some(ADMIN_KEY),
            None,
        )
        .await
    });

    // `broadcast` only succeeds once a subscriber is attached, so this doubles
    // as the handshake. The frame equals the replayed one: it must be dropped.
    let mut attached = false;
    for _ in 0..500 {
        if runtime.broadcast("task_live", first.clone()).await {
            attached = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(attached, "the replay subscriber never attached");

    assert!(
        runtime
            .broadcast("task_live", final_frame("task_live"))
            .await
    );
    let (status, bytes) = handle.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    let frames = parse_sse(&String::from_utf8(bytes).unwrap());
    assert_eq!(frames.len(), 2, "{frames:?}");
    assert_eq!(frames[0]["status"]["state"], "working");
    assert_eq!(frames[1]["status"]["state"], "completed");
}

// -------------------------------------------------------- faulty providers

/// How a chosen transition misbehaves.
#[derive(Clone, Copy)]
enum TransitionFault {
    /// The database errors.
    Error,
    /// Another writer moved the row (an illegal-move conflict).
    Conflict,
    /// The compare-and-set is lost: the row is already terminal.
    Lost,
}

/// A task store whose chosen calls fail, to pin the error branches that only a
/// broken database can reach.
struct FaultyTasks {
    inner: MemoryA2aTasks,
    fail_get: AtomicBool,
    fail_events: bool,
    fail_create: bool,
    fail_push: bool,
    /// Applied only when the transition targets this state, so a run can start
    /// normally and then lose exactly one write.
    transition_fault: Option<(TaskState, TransitionFault)>,
}

impl FaultyTasks {
    fn new() -> Self {
        Self {
            inner: MemoryA2aTasks::new(),
            fail_get: AtomicBool::new(false),
            fail_events: false,
            fail_create: false,
            fail_push: false,
            transition_fault: None,
        }
    }

    fn failing_get() -> Self {
        Self {
            fail_get: AtomicBool::new(true),
            ..Self::new()
        }
    }

    fn transition_to(state: TaskState, fault: TransitionFault) -> Self {
        Self {
            transition_fault: Some((state, fault)),
            ..Self::new()
        }
    }

    fn broken() -> AppError {
        AppError::Db(sqlx::Error::RowNotFound)
    }
}

#[async_trait]
impl A2aTaskStore for FaultyTasks {
    async fn create(&self, p: &Principal, row: &A2aTaskRow) -> Result<(), AppError> {
        if self.fail_create {
            return Err(Self::broken());
        }
        self.inner.create(p, row).await
    }
    async fn get(&self, p: &Principal, task: &str) -> Result<A2aTaskRow, AppError> {
        if self.fail_get.load(Ordering::SeqCst) {
            return Err(Self::broken());
        }
        self.inner.get(p, task).await
    }
    async fn transition(
        &self,
        p: &Principal,
        task: &str,
        next: TaskState,
        status: Option<Value>,
        now: i64,
    ) -> Result<Option<A2aTaskRow>, AppError> {
        if let Some((target, fault)) = self.transition_fault {
            if target == next {
                return match fault {
                    TransitionFault::Error => Err(Self::broken()),
                    TransitionFault::Conflict => Err(AppError::Conflict(format!(
                        "task {task} changed while transitioning"
                    ))),
                    TransitionFault::Lost => Ok(None),
                };
            }
        }
        self.inner.transition(p, task, next, status, now).await
    }
    async fn add_artifact(
        &self,
        p: &Principal,
        task: &str,
        artifact: &Artifact,
        now: i64,
    ) -> Result<Option<A2aTaskRow>, AppError> {
        self.inner.add_artifact(p, task, artifact, now).await
    }
    async fn append_event(
        &self,
        p: &Principal,
        task: &str,
        kind: &str,
        payload: &Value,
    ) -> Result<u64, AppError> {
        self.inner.append_event(p, task, kind, payload).await
    }
    async fn events_since(
        &self,
        p: &Principal,
        task: &str,
        since: u64,
        limit: usize,
    ) -> Result<Vec<A2aEventRecord>, AppError> {
        if self.fail_events {
            return Err(Self::broken());
        }
        self.inner.events_since(p, task, since, limit).await
    }
    async fn set_push(
        &self,
        p: &Principal,
        task: &str,
        config: &PushNotificationConfig,
    ) -> Result<(), AppError> {
        if self.fail_push {
            return Err(Self::broken());
        }
        self.inner.set_push(p, task, config).await
    }
    async fn set_push_last(
        &self,
        p: &Principal,
        task: &str,
        result: &Value,
        now: i64,
    ) -> Result<(), AppError> {
        self.inner.set_push_last(p, task, result, now).await
    }
    async fn fail_orphans(&self, reason: &str, now: i64) -> Result<usize, AppError> {
        self.inner.fail_orphans(reason, now).await
    }
}

#[tokio::test]
async fn a_broken_task_store_is_an_internal_error_not_a_leaked_message() {
    let _env = env_admin().await;
    let mut h = harness_a2a(None);
    h.state.a2a = A2aState {
        tasks: Arc::new(FaultyTasks::failing_get()),
        ..h.state.a2a.clone()
    };
    h.app = build_router(h.state.clone());

    let (status, body) = rpc(&h.app, "tasks/get", json!({"id": "task_any"})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["error"]["code"], code::INTERNAL_ERROR);
    assert!(!body["error"]["message"].as_str().unwrap().contains("db:"));

    // The replay endpoint answers a real 500 for the same failure.
    let (status, body) = send(
        &h.app,
        "GET",
        "/a2a/stream?task=task_any",
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error"]["code"], "internal_error");

    // A store that fails on every write surfaces the same way.
    let mut h = harness_a2a(None);
    h.state.a2a = A2aState {
        tasks: Arc::new(FaultyTasks {
            fail_create: true,
            ..FaultyTasks::new()
        }),
        ..h.state.a2a.clone()
    };
    h.app = build_router(h.state.clone());
    let agent = create_agent(&h, "unwritable").await;
    let (_, body) = send(
        &h.app,
        "POST",
        &format!("/v1/agents/{agent}/a2a"),
        Some(ADMIN_KEY),
        Some(call(1, "message/send", json!({"message": msg("hi")}))),
    )
    .await;
    assert_eq!(body["error"]["code"], code::INTERNAL_ERROR);
    assert_eq!(
        h.state.a2a.runtime.live_tasks().await,
        0,
        "the context was released"
    );
}

#[tokio::test]
async fn a_store_that_cannot_transition_or_arm_push_fails_the_start() {
    let _env = env_admin().await;
    // `set_push` fails: the task is accepted, then the run cannot be armed.
    let mut h = harness_a2a(None);
    h.state.a2a = A2aState {
        tasks: Arc::new(FaultyTasks {
            fail_push: true,
            ..FaultyTasks::new()
        }),
        ..h.state.a2a.clone()
    };
    h.app = build_router(h.state.clone());
    let agent = create_agent(&h, "pushless").await;
    let (_, body) = send(
        &h.app,
        "POST",
        &format!("/v1/agents/{agent}/a2a"),
        Some(ADMIN_KEY),
        Some(call(
            1,
            "message/send",
            json!({
                "message": msg("hi"),
                "configuration": {"pushNotification": {"url": "https://hook.example.test/x"}},
            }),
        )),
    )
    .await;
    assert_eq!(body["error"]["code"], code::INTERNAL_ERROR);

    // `transition` fails: the run cannot reach `working`, so it gives up and
    // releases the context without claiming a terminal state it never wrote.
    let mut h = harness_a2a(None);
    let tasks = Arc::new(FaultyTasks::transition_to(
        TaskState::Working,
        TransitionFault::Error,
    ));
    h.state.a2a = A2aState {
        tasks: tasks.clone(),
        ..h.state.a2a.clone()
    };
    h.app = build_router(h.state.clone());
    let agent = create_agent(&h, "unstuck").await;
    let (status, body) = send(
        &h.app,
        "POST",
        &format!("/v1/agents/{agent}/a2a"),
        Some(ADMIN_KEY),
        Some(call(1, "message/send", json!({"message": msg("hi")}))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The submitted row is all the run ever wrote; the context is free again.
    assert_eq!(body["result"]["status"]["state"], "submitted");
    assert_eq!(h.state.a2a.runtime.live_tasks().await, 0);
}

#[tokio::test]
async fn a_failing_event_log_degrades_to_an_empty_replay() {
    let _env = env_admin().await;
    let admin = Principal::admin();
    let tasks = FaultyTasks {
        fail_events: true,
        ..FaultyTasks::new()
    };
    tasks
        .create(&admin, &submitted_row("task_ev", &admin.id, "sess_ev"))
        .await
        .unwrap();
    let mut h = harness_a2a(None);
    h.state.a2a = A2aState {
        tasks: Arc::new(tasks),
        ..h.state.a2a.clone()
    };
    h.app = build_router(h.state.clone());
    let (status, bytes) = send_bytes(
        &h.app,
        "GET",
        "/a2a/stream?task=task_ev",
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(bytes.is_empty(), "{bytes:?}");
}

#[tokio::test]
async fn a_start_that_cannot_mirror_the_session_reports_a_store_failure() {
    let _env = env_admin().await;
    for (fail_get, fail_session, fail_turn, fail_item) in [
        (false, true, false, false),
        (false, false, true, false),
        (false, false, false, true),
    ] {
        let mut h = harness_a2a(None);
        h.state.sessions = Arc::new(FaultySessions {
            fail_get,
            fail_session,
            fail_turn,
            fail_item,
        });
        h.app = build_router(h.state.clone());
        let agent = create_agent(&h, "no-session").await;
        let (status, body) = send(
            &h.app,
            "POST",
            &format!("/v1/agents/{agent}/a2a"),
            Some(ADMIN_KEY),
            Some(call(1, "message/send", json!({"message": msg("hi")}))),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["error"]["code"], code::INTERNAL_ERROR);
        assert_eq!(
            h.state.a2a.runtime.live_tasks().await,
            0,
            "the reserved context must be released"
        );
    }

    // An existing (but unreadable) context fails before anything is mirrored.
    let mut h = harness_a2a(None);
    h.state.sessions = Arc::new(FaultySessions {
        fail_get: true,
        fail_session: false,
        fail_turn: false,
        fail_item: false,
    });
    h.app = build_router(h.state.clone());
    let agent = create_agent(&h, "no-read").await;
    let (_, body) = send(
        &h.app,
        "POST",
        &format!("/v1/agents/{agent}/a2a"),
        Some(ADMIN_KEY),
        Some(call(
            1,
            "message/send",
            json!({"message": {"role": "user", "parts": [{"kind": "text", "text": "hi"}], "contextId": "sess_gone"}}),
        )),
    )
    .await;
    assert_eq!(body["error"]["code"], code::INTERNAL_ERROR);
}

/// A session store that refuses the writes a start needs.
struct FaultySessions {
    fail_get: bool,
    fail_session: bool,
    fail_turn: bool,
    fail_item: bool,
}

fn broken() -> AppError {
    AppError::Db(sqlx::Error::RowNotFound)
}

#[async_trait]
impl SessionStore for FaultySessions {
    async fn insert_session(&self, _p: &Principal, _s: &SessionRow) -> Result<(), AppError> {
        if self.fail_session {
            return Err(broken());
        }
        Ok(())
    }
    async fn list_sessions(
        &self,
        _p: &Principal,
        _limit: i64,
    ) -> Result<Vec<SessionRow>, AppError> {
        Ok(Vec::new())
    }
    async fn get_session(&self, _p: &Principal, _id: &str) -> Result<SessionRow, AppError> {
        if self.fail_get {
            return Err(broken());
        }
        Err(AppError::NotFound)
    }
    async fn set_instructions(
        &self,
        _id: &str,
        _instructions: Option<&str>,
    ) -> Result<(), AppError> {
        Ok(())
    }
    async fn delete_session(&self, _id: &str) -> Result<(), AppError> {
        Ok(())
    }
    async fn insert_turn(&self, _t: &TurnRow) -> Result<(), AppError> {
        if self.fail_turn {
            return Err(broken());
        }
        Ok(())
    }
    async fn finish_turn(
        &self,
        _id: &str,
        _status: &str,
        _output: Option<&str>,
    ) -> Result<(), AppError> {
        Ok(())
    }
    async fn list_turns(&self, _p: &Principal, _session: &str) -> Result<Vec<TurnRow>, AppError> {
        Ok(Vec::new())
    }
    async fn get_turn(
        &self,
        _p: &Principal,
        _session: &str,
        _turn: &str,
    ) -> Result<Option<TurnRow>, AppError> {
        Ok(None)
    }
    async fn insert_item(&self, _i: &ItemRow) -> Result<(), AppError> {
        if self.fail_item {
            return Err(broken());
        }
        Ok(())
    }
    async fn list_items(&self, _p: &Principal, _session: &str) -> Result<Vec<ItemRow>, AppError> {
        Ok(Vec::new())
    }
}

// --------------------------------------------------------- store semantics

#[tokio::test]
async fn the_memory_store_scopes_every_read_and_write_to_the_tenant() {
    let store = MemoryA2aTasks::new();
    let owner = user("owner");
    let stranger = read_only("stranger");
    let admin = Principal::admin();
    let row = submitted_row("task_1", "owner", "sess_1");
    store.create(&owner, &row).await.unwrap();
    // A row cannot be created on behalf of somebody else.
    assert!(matches!(
        store
            .create(&stranger, &submitted_row("task_2", "owner", "sess_2"))
            .await,
        Err(AppError::NotFound)
    ));
    assert!(matches!(
        store
            .create(&owner, &submitted_row("task_1", "owner", "sess_1"))
            .await,
        Err(AppError::Conflict(_))
    ));

    // The stranger sees nothing; the admin sees everything.
    assert!(matches!(
        store.get(&stranger, "task_1").await,
        Err(AppError::NotFound)
    ));
    assert_eq!(store.get(&admin, "task_1").await.unwrap(), row);
    let invisible = [
        store
            .transition(&stranger, "task_1", TaskState::Working, None, 1)
            .await
            .is_err(),
        store
            .add_artifact(&stranger, "task_1", &Artifact::text("a", 0, "t"), 1)
            .await
            .is_err(),
        store
            .append_event(&stranger, "task_1", "task", &json!({}))
            .await
            .is_err(),
        store
            .events_since(&stranger, "task_1", 0, 10)
            .await
            .is_err(),
        store
            .set_push(&stranger, "task_1", &push("https://x.test"))
            .await
            .is_err(),
        store
            .set_push_last(&stranger, "task_1", &json!({}), 1)
            .await
            .is_err(),
    ];
    assert!(invisible.iter().all(|hidden| *hidden));

    // Every method also reports a task that does not exist at all.
    let missing = [
        store.get(&admin, "task_none").await.is_err(),
        store
            .transition(&admin, "task_none", TaskState::Working, None, 1)
            .await
            .is_err(),
        store
            .add_artifact(&admin, "task_none", &Artifact::text("a", 0, "t"), 1)
            .await
            .is_err(),
        store
            .append_event(&admin, "task_none", "task", &json!({}))
            .await
            .is_err(),
        store
            .events_since(&admin, "task_none", 0, 10)
            .await
            .is_err(),
        store
            .set_push(&admin, "task_none", &push("https://x.test"))
            .await
            .is_err(),
        store
            .set_push_last(&admin, "task_none", &json!({}), 1)
            .await
            .is_err(),
    ];
    assert!(missing.iter().all(|err| *err));
}

#[tokio::test]
async fn the_memory_store_enforces_the_lifecycle_and_keeps_terminal_states() {
    let store = MemoryA2aTasks::new();
    let admin = Principal::admin();
    store
        .create(&admin, &submitted_row("task_1", &admin.id, "sess_1"))
        .await
        .unwrap();

    // Submitted may not jump straight to completed.
    assert!(matches!(
        store
            .transition(&admin, "task_1", TaskState::Completed, None, 2)
            .await,
        Err(AppError::Conflict(_))
    ));
    let working = store
        .transition(
            &admin,
            "task_1",
            TaskState::Working,
            Some(json!({"state": "working"})),
            now_millis(),
        )
        .await
        .unwrap()
        .expect("live task moves");
    assert_eq!(working.state, TaskState::Working);
    assert_eq!(working.status, Some(json!({"state": "working"})));
    // The same state again is a refresh, and `None` keeps the last status.
    let refreshed = store
        .transition(&admin, "task_1", TaskState::Working, None, now_millis())
        .await
        .unwrap()
        .expect("refresh");
    assert_eq!(refreshed.status, Some(json!({"state": "working"})));

    let artifact = Artifact::text("answer", 0, "hello");
    let with_artifact = store
        .add_artifact(&admin, "task_1", &artifact, now_millis())
        .await
        .unwrap()
        .expect("live task accepts artifacts");
    assert_eq!(with_artifact.artifacts, vec![artifact.clone()]);

    // The event log is append-only and ordered.
    let first = store
        .append_event(&admin, "task_1", "task", &json!({"n": 1}))
        .await
        .unwrap();
    let second = store
        .append_event(&admin, "task_1", "status", &json!({"n": 2}))
        .await
        .unwrap();
    assert_eq!((first, second), (1, 2));
    let events = store.events_since(&admin, "task_1", 1, 10).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, "status");
    assert_eq!(
        store
            .events_since(&admin, "task_1", 0, 1)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(store
        .events_since(&admin, "task_1", 99, 10)
        .await
        .unwrap()
        .is_empty());
    // A task with no events at all replays an empty log.
    store
        .create(&admin, &submitted_row("task_quiet", &admin.id, "sess_2"))
        .await
        .unwrap();
    assert!(store
        .events_since(&admin, "task_quiet", 0, 10)
        .await
        .unwrap()
        .is_empty());

    // Push config round-trips, including the delivery receipt.
    let config = PushNotificationConfig {
        id: Some("p1".into()),
        url: "https://hook.test/a2a".into(),
        token: Some("t".into()),
        authentication: None,
    };
    store.set_push(&admin, "task_1", &config).await.unwrap();
    let row = store.get(&admin, "task_1").await.unwrap();
    assert_eq!(row.push.as_ref(), Some(&config));
    store
        .set_push_last(&admin, "task_1", &json!({"delivered": true}), now_millis())
        .await
        .unwrap();
    assert_eq!(
        store.get(&admin, "task_1").await.unwrap().push_last,
        Some(json!({"delivered": true}))
    );

    // Cancel wins the race, and every later write is refused as terminal.
    let cancelled = store
        .transition(&admin, "task_1", TaskState::Canceled, None, now_millis())
        .await
        .unwrap()
        .expect("live task cancels");
    assert_eq!(cancelled.state, TaskState::Canceled);
    assert!(store
        .transition(&admin, "task_1", TaskState::Completed, None, now_millis())
        .await
        .unwrap()
        .is_none());
    assert!(store
        .add_artifact(&admin, "task_1", &artifact, now_millis())
        .await
        .unwrap()
        .is_none());

    // A restart closes everything still live and reports how many. `task_quiet`
    // was still live, so close it first to keep the count about `task_live`.
    store
        .transition(
            &admin,
            "task_quiet",
            TaskState::Canceled,
            None,
            now_millis(),
        )
        .await
        .unwrap()
        .expect("a quiet task still cancels");
    store
        .create(&admin, &submitted_row("task_live", &admin.id, "sess_3"))
        .await
        .unwrap();
    assert_eq!(
        store
            .fail_orphans("process restarted", now_millis())
            .await
            .unwrap(),
        1
    );
    let live = store.get(&admin, "task_live").await.unwrap();
    assert_eq!(live.state, TaskState::Failed);
    assert_eq!(
        live.status.unwrap()["message"]["parts"][0]["text"],
        "process restarted"
    );
    assert_eq!(
        store.fail_orphans("again", now_millis()).await.unwrap(),
        0,
        "terminal tasks are never rewritten"
    );
}

fn now_millis() -> i64 {
    crate::util::now_ms()
}

// ------------------------------------------------------- runtime registry

#[tokio::test]
async fn the_runtime_allows_one_live_task_per_context_and_releases_it() {
    let runtime = A2aRuntime::new();
    assert_eq!(runtime.live_tasks().await, 0);
    let first = runtime.open("task_1", "sess_1").await.unwrap();
    assert_eq!(FRAME_CAPACITY, 256);
    assert_eq!(runtime.live_tasks().await, 1);
    assert!(runtime.is_active("task_1").await);
    assert_eq!(
        runtime.active_task_for("sess_1").await.as_deref(),
        Some("task_1")
    );
    assert!(runtime.active_task_for("sess_other").await.is_none());
    // Re-opening the same task is a no-op; a second task in the context is not.
    assert!(runtime.open("task_1", "sess_1").await.is_ok());
    assert!(runtime.open("task_2", "sess_1").await.is_err());
    // Another context is independent.
    assert!(runtime.open("task_3", "sess_2").await.is_ok());

    // No subscriber yet: a broadcast goes nowhere and says so.
    drop(first.receiver);
    assert!(!runtime.broadcast("task_1", progress_frame("task_1")).await);
    // With a subscriber it arrives.
    let mut receiver = runtime.subscribe("task_1").await.unwrap();
    assert!(runtime.broadcast("task_1", progress_frame("task_1")).await);
    assert!(receiver.recv().await.is_ok());
    // An unknown task has no channel and cannot be aborted.
    assert!(
        !runtime
            .broadcast("task_none", progress_frame("task_none"))
            .await
    );
    assert!(runtime.subscribe("task_none").await.is_none());
    assert!(!runtime.abort("task_none").await);

    // A running task can be aborted exactly once it has a handle.
    assert!(!runtime.abort("task_1").await);
    let join = tokio::spawn(async { std::future::pending::<()>().await });
    runtime.set_join("task_1", join).await;
    assert!(runtime.abort("task_1").await);
    // A handle for a task that is already gone is ignored, not a panic.
    runtime.set_join("task_none", tokio::spawn(async {})).await;

    // HITL routing table.
    let parked = parked_hitl("task_1", "sess_1").await;
    runtime.attach_hitl("sess_1", parked.hitl.clone()).await;
    assert!(runtime.hitl_for("sess_1").await.is_some());
    assert!(runtime.hitl_for("sess_2").await.is_none());
    runtime.detach_hitl("sess_1").await;
    assert!(runtime.hitl_for("sess_1").await.is_none());

    runtime.finish("task_1").await;
    assert!(!runtime.is_active("task_1").await);
    assert!(runtime.active_task_for("sess_1").await.is_none());
    runtime.finish("task_3").await;
    assert_eq!(runtime.live_tasks().await, 0);
}

/// Everything a test needs to drive one parked decision.
struct Parked {
    hitl: Arc<PausingHitl>,
    frames: broadcast::Receiver<StreamFrame>,
    store: Arc<dyn A2aTaskStore>,
    principal: Principal,
}

impl Parked {
    /// Wait until the task is parked (`input-required` has been published).
    ///
    /// Earlier progress frames (a previous resume's `working`) may still be
    /// queued, so this looks for the parked state instead of taking the next
    /// frame.
    async fn wait(&mut self) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let frame = tokio::time::timeout_at(deadline, self.frames.recv())
                .await
                .expect("the hitl never parked")
                .expect("the frame channel closed");
            if let StreamFrame::StatusUpdate(update) = &frame {
                if update.status.state == TaskState::InputRequired {
                    return;
                }
            }
        }
    }
}

/// A `PausingHitl` wired to a store that already has its task, `working`.
async fn parked_hitl(task_id: &str, context_id: &str) -> Parked {
    let store = Arc::new(MemoryA2aTasks::new());
    let principal = Principal::admin();
    store
        .create(
            &principal,
            &submitted_row(task_id, &principal.id, context_id),
        )
        .await
        .unwrap();
    store
        .transition(&principal, task_id, TaskState::Working, None, 1)
        .await
        .unwrap();
    build_hitl(
        principal,
        store as Arc<dyn A2aTaskStore>,
        task_id,
        context_id,
    )
}

fn build_hitl(
    principal: Principal,
    store: Arc<dyn A2aTaskStore>,
    task_id: &str,
    context_id: &str,
) -> Parked {
    build_hitl_with_push(principal, store, task_id, context_id, Arc::new(NoopPush))
}

fn build_hitl_with_push(
    principal: Principal,
    store: Arc<dyn A2aTaskStore>,
    task_id: &str,
    context_id: &str,
    push: Arc<dyn PushNotifier>,
) -> Parked {
    let (frames, receiver) = broadcast::channel(FRAME_CAPACITY);
    let hitl = Arc::new(PausingHitl::new(HitlContext {
        principal: principal.clone(),
        task_id: task_id.into(),
        context_id: context_id.into(),
        preset: agent_core::seams::PermissionPreset::WorkspaceWrite,
        store: store.clone(),
        push,
        frames,
    }));
    Parked {
        hitl,
        frames: receiver,
        store,
        principal,
    }
}

// ----------------------------------------------------------- HITL details

#[tokio::test]
async fn a_parked_approval_accepts_a_decision_and_a_spelled_out_answer() {
    // Decision words are shared by the RPC layer and the seam adapter.
    assert_eq!(parse_decision(" ALLOW "), Some(true));
    assert_eq!(parse_decision("rejected"), Some(false));
    assert_eq!(parse_decision("maybe"), None);

    let mut parked = parked_hitl("task_a", "sess_a").await;
    let asking = parked.hitl.clone();
    let deciding = tokio::spawn(async move {
        asking
            .decide(&agent_core::seams::ApprovalRequest::new(
                "shell",
                "sess_a",
                json!({"command": ["ls"]}),
            ))
            .await
    });
    parked.wait().await;
    assert!(parked
        .hitl
        .resume(ResumeInput::Decision(true))
        .await
        .is_ok());
    assert!(deciding.await.unwrap().is_allowed());

    // The same path with a spelled-out decision instead of a boolean.
    let mut parked = parked_hitl("task_b", "sess_b").await;
    let asking = parked.hitl.clone();
    let deciding = tokio::spawn(async move {
        asking
            .decide(&agent_core::seams::ApprovalRequest::new(
                "shell",
                "sess_b",
                json!({}),
            ))
            .await
    });
    parked.wait().await;
    assert!(parked
        .hitl
        .resume(ResumeInput::Text("deny".into()))
        .await
        .is_ok());
    let decision = deciding.await.unwrap();
    assert!(!decision.is_allowed());
    assert!(decision.reason().contains("denied"));

    // A text answer that is not a decision is refused, and the task stays
    // parked: an unparsed answer must never become an implicit denial.
    let mut parked = parked_hitl("task_c", "sess_c").await;
    let asking = parked.hitl.clone();
    let deciding = tokio::spawn(async move {
        asking
            .decide(&agent_core::seams::ApprovalRequest::new(
                "shell",
                "sess_c",
                json!({}),
            ))
            .await
    });
    parked.wait().await;
    assert_eq!(
        parked.hitl.resume(ResumeInput::Text("purple".into())).await,
        Err(ResumeError::Mismatch)
    );
    let row = parked.store.get(&parked.principal, "task_c").await.unwrap();
    assert_eq!(row.state, TaskState::InputRequired);
    // The same task still accepts a real decision afterwards.
    assert!(parked
        .hitl
        .resume(ResumeInput::Text("allow".into()))
        .await
        .is_ok());
    assert!(deciding.await.unwrap().is_allowed());
}

#[tokio::test]
async fn a_hitl_that_cannot_park_fails_closed() {
    // The task is not in the store at all: `park` cannot move it.
    let store = Arc::new(MemoryA2aTasks::new());
    let mut parked = build_hitl(Principal::admin(), store, "task_ghost", "sess_ghost");
    let decision = parked
        .hitl
        .decide(&agent_core::seams::ApprovalRequest::new(
            "shell",
            "s",
            json!({}),
        ))
        .await;
    assert!(!decision.is_allowed());
    assert!(decision.reason().contains("no longer live"));
    let err = parked.hitl.ask("which?", &[]).await.unwrap_err();
    assert!(err.to_string().contains("no longer live"));
    assert!(parked.frames.try_recv().is_err(), "nothing was published");
    // Nothing is parked, so resume has nothing to do.
    assert_eq!(
        parked.hitl.resume(ResumeInput::Decision(true)).await,
        Err(ResumeError::NotParked)
    );

    // A cancelled task refuses to park too.
    let store = Arc::new(MemoryA2aTasks::new());
    let principal = Principal::admin();
    store
        .create(
            &principal,
            &submitted_row("task_done", &principal.id, "sess_done"),
        )
        .await
        .unwrap();
    store
        .transition(&principal, "task_done", TaskState::Canceled, None, 1)
        .await
        .unwrap();
    let parked = build_hitl(principal, store, "task_done", "sess_done");
    assert!(!parked
        .hitl
        .decide(&agent_core::seams::ApprovalRequest::new(
            "shell",
            "s",
            json!({}),
        ))
        .await
        .is_allowed());

    // A second question while one is parked is refused, not queued.
    let mut parked = parked_hitl("task_two", "sess_two").await;
    let asking = parked.hitl.clone();
    let first = tokio::spawn(async move { asking.ask("first?", &[]).await });
    parked.wait().await;
    let second = parked.hitl.ask("second?", &[]).await.unwrap_err();
    assert!(second.to_string().contains("no longer live"));
    assert!(parked
        .hitl
        .resume(ResumeInput::Text("one".into()))
        .await
        .is_ok());
    assert_eq!(first.await.unwrap().unwrap(), "one");
}

#[tokio::test]
async fn a_dropped_decision_releases_the_parked_caller() {
    // An approval whose deciding side went away denies.
    let mut parked = parked_hitl("task_app", "sess_app").await;
    let asking = parked.hitl.clone();
    let deciding = tokio::spawn(async move {
        asking
            .decide(&agent_core::seams::ApprovalRequest::new(
                "shell",
                "sess_app",
                json!({}),
            ))
            .await
    });
    parked.wait().await;
    parked.hitl.abandon().await;
    let decision = deciding.await.unwrap();
    assert!(!decision.is_allowed());
    assert!(decision.reason().contains("channel closed"));
    // The task is still parked in the store; a later cancel owns the outcome.
    assert!(parked
        .hitl
        .resume(ResumeInput::Decision(true))
        .await
        .is_err());

    // A question whose deciding side went away is unavailable, not answered.
    let mut parked = parked_hitl("task_q", "sess_q").await;
    let asking = parked.hitl.clone();
    let questioning = tokio::spawn(async move { asking.ask("still there?", &[]).await });
    parked.wait().await;
    parked.hitl.abandon().await;
    let err = questioning.await.unwrap().unwrap_err();
    assert!(err.to_string().contains("channel closed"));
}

#[tokio::test]
async fn a_question_parks_the_task_and_exposes_the_dsh_request_shape() {
    use aria_agent_dsh::rpc::{
        ServerRequestHandler, METHOD_APPROVAL_REQUEST, METHOD_USER_QUESTION,
    };

    let mut parked = parked_hitl("task_dsh", "sess_dsh").await;
    assert_eq!(parked.hitl.task_id(), "task_dsh");

    // The dsh route: an approval request over JSON-RPC.
    let handler = parked.hitl.clone();
    let asking = tokio::spawn(async move {
        handler
            .handle(METHOD_APPROVAL_REQUEST, &json!({"tool": "shell"}))
            .await
    });
    parked.wait().await;
    assert!(parked
        .hitl
        .resume(ResumeInput::Decision(true))
        .await
        .is_ok());
    assert_eq!(asking.await.unwrap().unwrap()["decision"], "allow");

    // A denied approval carries a reason.
    let handler = parked.hitl.clone();
    let asking = tokio::spawn(async move {
        handler
            .handle(
                METHOD_APPROVAL_REQUEST,
                &json!({"tool": "shell", "sessionId": "sess_dsh"}),
            )
            .await
    });
    parked.wait().await;
    assert!(parked
        .hitl
        .resume(ResumeInput::Decision(false))
        .await
        .is_ok());
    let denied = asking.await.unwrap().unwrap();
    assert_eq!(denied["decision"], "deny");
    assert!(denied["reason"].as_str().unwrap().contains("denied"));

    // A question request: options are forwarded and the answer goes back.
    let handler = parked.hitl.clone();
    let asking = tokio::spawn(async move {
        handler
            .handle(
                METHOD_USER_QUESTION,
                &json!({"question": "which?", "options": ["a", "b", 7]}),
            )
            .await
    });
    parked.wait().await;
    assert!(parked
        .hitl
        .resume(ResumeInput::Text("b".into()))
        .await
        .is_ok());
    assert_eq!(asking.await.unwrap().unwrap()["answer"], "b");

    // Malformed requests are rejected without parking anything.
    let err = parked
        .hitl
        .handle(METHOD_APPROVAL_REQUEST, &json!({}))
        .await
        .unwrap_err();
    assert_eq!(err.0, -32602);
    let err = parked
        .hitl
        .handle(METHOD_USER_QUESTION, &json!({}))
        .await
        .unwrap_err();
    assert_eq!(err.0, -32602);
    let err = parked
        .hitl
        .handle("other/request", &json!({}))
        .await
        .unwrap_err();
    assert_eq!(err.0, aria_agent_dsh::rpc::CODE_METHOD_NOT_FOUND);
}

#[tokio::test]
async fn a_mismatched_answer_leaves_the_task_parked() {
    // A decision cannot answer a question.
    let mut parked = parked_hitl("task_m", "sess_m").await;
    let asking = parked.hitl.clone();
    let questioning = tokio::spawn(async move { asking.ask("which?", &["a".into()]).await });
    parked.wait().await;
    assert_eq!(
        parked.hitl.resume(ResumeInput::Decision(true)).await,
        Err(ResumeError::Mismatch)
    );
    assert!(parked
        .hitl
        .resume(ResumeInput::Text("a".into()))
        .await
        .is_ok());
    assert_eq!(questioning.await.unwrap().unwrap(), "a");

    // A task that is no longer `input-required` cannot be resumed, and the
    // answer is not consumed: the slot is restored.
    let mut parked = parked_hitl("task_n", "sess_n").await;
    let asking = parked.hitl.clone();
    let questioning = tokio::spawn(async move { asking.ask("still there?", &[]).await });
    parked.wait().await;
    // A concurrent cancel looks exactly like this from the hitl's side.
    parked
        .store
        .transition(&parked.principal, "task_n", TaskState::Canceled, None, 2)
        .await
        .unwrap();
    assert_eq!(
        parked.hitl.resume(ResumeInput::Text("yes".into())).await,
        Err(ResumeError::NotParked)
    );
    // The slot was restored, so the parked question is still waiting.
    assert!(!questioning.is_finished());
    parked.hitl.abandon().await;
    assert!(questioning.await.unwrap().is_err());
}

#[tokio::test]
async fn a_resume_whose_store_cannot_read_the_task_releases_the_answer() {
    let principal = Principal::admin();
    let store = Arc::new(FaultyTasks::new());
    store
        .create(
            &principal,
            &submitted_row("task_x", &principal.id, "sess_x"),
        )
        .await
        .unwrap();
    store
        .transition(&principal, "task_x", TaskState::Working, None, 1)
        .await
        .unwrap();
    let mut parked = build_hitl(
        principal,
        store.clone() as Arc<dyn A2aTaskStore>,
        "task_x",
        "sess_x",
    );
    let asking = parked.hitl.clone();
    let questioning = tokio::spawn(async move { asking.ask("anyone?", &[]).await });
    parked.wait().await;
    // The store goes away behind the hitl: the answer must not be consumed.
    store.fail_get.store(true, Ordering::SeqCst);
    assert_eq!(
        parked.hitl.resume(ResumeInput::Text("yes".into())).await,
        Err(ResumeError::NotParked)
    );
    parked.hitl.abandon().await;
    assert!(questioning.await.unwrap().is_err());
}

#[tokio::test]
async fn a_resume_whose_transition_fails_puts_the_answer_back() {
    let principal = Principal::admin();
    // The `working` transition is the one that fails; the setup transition is
    // taken straight from the memory store so the task can park first.
    let store = Arc::new(FaultyTasks::transition_to(
        TaskState::Working,
        TransitionFault::Error,
    ));
    store
        .create(
            &principal,
            &submitted_row("task_y", &principal.id, "sess_y"),
        )
        .await
        .unwrap();
    store
        .inner
        .transition(&principal, "task_y", TaskState::Working, None, 1)
        .await
        .unwrap();
    let mut parked = build_hitl(
        principal,
        store.clone() as Arc<dyn A2aTaskStore>,
        "task_y",
        "sess_y",
    );
    let asking = parked.hitl.clone();
    let questioning = tokio::spawn(async move { asking.ask("anyone?", &[]).await });
    parked.wait().await;

    assert_eq!(
        parked.hitl.resume(ResumeInput::Text("yes".into())).await,
        Err(ResumeError::NotParked)
    );
    // The caller is still waiting: the answer was not delivered either.
    parked.hitl.abandon().await;
    assert!(questioning.await.unwrap().is_err());
}

#[tokio::test]
async fn a_parked_task_survives_a_push_that_cannot_be_delivered() {
    let principal = Principal::admin();
    let store = Arc::new(MemoryA2aTasks::new());
    store
        .create(
            &principal,
            &submitted_row("task_p", &principal.id, "sess_p"),
        )
        .await
        .unwrap();
    store
        .transition(&principal, "task_p", TaskState::Working, None, 1)
        .await
        .unwrap();
    store
        .set_push(&principal, "task_p", &push("https://hook.test/x"))
        .await
        .unwrap();
    let mut parked = build_hitl_with_push(
        principal,
        store as Arc<dyn A2aTaskStore>,
        "task_p",
        "sess_p",
        Arc::new(FaultyPush),
    );
    let asking = parked.hitl.clone();
    let deciding = tokio::spawn(async move {
        asking
            .decide(&agent_core::seams::ApprovalRequest::new(
                "shell",
                "sess_p",
                json!({}),
            ))
            .await
    });
    parked.wait().await;
    // The failed callback is logged, not turned into a refusal.
    assert!(parked
        .hitl
        .resume(ResumeInput::Decision(true))
        .await
        .is_ok());
    assert!(deciding.await.unwrap().is_allowed());
}

#[tokio::test]
async fn the_routing_bridge_prefers_a_parked_task_and_falls_back() {
    use aria_agent_dsh::rpc::{ServerRequestHandler, METHOD_USER_QUESTION};

    struct Echo;
    #[async_trait]
    impl ServerRequestHandler for Echo {
        async fn handle(&self, _method: &str, params: &Value) -> Result<Value, (i64, String)> {
            Ok(json!({"fallback": true, "params": params}))
        }
    }

    let runtime = Arc::new(A2aRuntime::new());
    let bridge = RoutingBridge::new(runtime.clone(), Arc::new(Echo));

    // No session in the request: the fallback answers.
    let answer = bridge
        .handle(METHOD_USER_QUESTION, &json!({"question": "q"}))
        .await
        .unwrap();
    assert_eq!(answer["fallback"], true);
    // A session nobody parked on also falls back.
    let answer = bridge
        .handle("x", &json!({"sessionId": "sess_unknown"}))
        .await
        .unwrap();
    assert_eq!(answer["fallback"], true);

    // A parked session is served by its own task.
    let mut parked = parked_hitl("task_r", "sess_r").await;
    runtime.attach_hitl("sess_r", parked.hitl.clone()).await;
    let asking = tokio::spawn(async move {
        bridge
            .handle(
                METHOD_USER_QUESTION,
                &json!({"sessionId": "sess_r", "question": "routed?"}),
            )
            .await
    });
    parked.wait().await;
    assert!(parked
        .hitl
        .resume(ResumeInput::Text("yes".into()))
        .await
        .is_ok());
    assert_eq!(asking.await.unwrap().unwrap()["answer"], "yes");
}

// ------------------------------------------------------------ executor bits

#[tokio::test]
async fn a_resume_without_a_parked_channel_is_not_resumable() {
    let _env = env_admin().await;
    let h = harness_a2a(None);
    let admin = Principal::admin();
    h.state
        .a2a
        .tasks
        .create(&admin, &submitted_row("task_park", &admin.id, "sess_park"))
        .await
        .unwrap();
    // `submitted → input-required` is not a legal move; a real run goes through
    // `working` first.
    h.state
        .a2a
        .tasks
        .transition(&admin, "task_park", TaskState::Working, None, 1)
        .await
        .unwrap();
    h.state
        .a2a
        .tasks
        .transition(&admin, "task_park", TaskState::InputRequired, None, 1)
        .await
        .unwrap();
    // Nothing is registered in the runtime: there is nobody to take the answer.
    let failure = crate::a2a::executor::resume(
        &h.state,
        &admin,
        "task_park",
        ResumeInput::Text("yes".into()),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        failure,
        crate::a2a::executor::TaskFailure::NotResumable
    ));

    // A hitl that is attached but has nothing parked is just as unresumable:
    // the store says `input-required`, the seam says "nobody asked".
    let parked = build_hitl(
        admin.clone(),
        h.state.a2a.tasks.clone(),
        "task_park",
        "sess_park",
    );
    h.state
        .a2a
        .runtime
        .attach_hitl("sess_park", parked.hitl.clone())
        .await;
    let failure = crate::a2a::executor::resume(
        &h.state,
        &admin,
        "task_park",
        ResumeInput::Text("yes".into()),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        failure,
        crate::a2a::executor::TaskFailure::NotResumable
    ));
}

// ------------------------------------------------------- push delivery

#[tokio::test]
async fn the_push_url_validator_only_accepts_plain_http_targets() {
    assert!(validate_push_url("http://127.0.0.1:9/hook").is_ok());
    assert!(validate_push_url(" https://hook.example.test/a2a ").is_ok());
    assert!(validate_push_url("ftp://hook.test").is_err());
    assert!(validate_push_url("https://").is_err());
    assert!(validate_push_url("https://user:pass@hook.test/x").is_err());
    assert_eq!(MAX_ATTEMPTS, 2);
    assert_eq!(PUSH_TIMEOUT, Duration::from_secs(5));
}

#[tokio::test]
async fn the_http_notifier_posts_the_body_retries_and_reports_failure() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .and(header("authorization", "Bearer tok"))
        .and(body_partial_json(json!({"taskId": "task_1"})))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let notifier = HttpPushNotifier::new().unwrap();
    let config = PushNotificationConfig {
        id: None,
        url: format!("{}/hook", server.uri()),
        token: Some("tok".into()),
        authentication: None,
    };
    notifier
        .notify(&config, &json!({"taskId": "task_1"}))
        .await
        .unwrap();

    // A 5xx is retried once, then reported with the status.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .expect(MAX_ATTEMPTS as u64)
        .mount(&server)
        .await;
    let unreachable = push(&format!("{}/hook", server.uri()));
    let err = notifier.notify(&unreachable, &json!({})).await.unwrap_err();
    assert!(err.contains("503"), "{err}");

    // An unreachable target is an error too (no panic, no rollback).
    let refused = push("http://127.0.0.1:1/hook");
    assert!(notifier.notify(&refused, &json!({})).await.is_err());

    // A non-http target never reaches the network.
    let invalid = push("ftp://hook.test");
    assert!(notifier.notify(&invalid, &json!({})).await.is_err());

    // The no-op notifier accepts everything and sends nothing.
    NoopPush.notify(&invalid, &json!({})).await.unwrap();
}

// ---------------------------------------------------- card projection

#[tokio::test]
async fn the_card_projection_takes_description_and_skills_from_the_deployment() {
    let overrides = CardOverrides::from_json(
        r#"{"description":"A bespoke agent","skills":[{"id":"billing","name":"Billing","description":"Invoices"}]}"#,
    )
    .unwrap();
    let card = crate::a2a::card::build_card(
        "https://cloud.example.test/",
        "Payments",
        "/v1/agents/agent-1/a2a",
        &overrides,
    );
    assert_eq!(card.description, "A bespoke agent");
    assert_eq!(card.skills.len(), 1);
    assert_eq!(card.skills[0].id, "billing");
    assert_eq!(card.url, "https://cloud.example.test/v1/agents/agent-1/a2a");
    assert_eq!(card.version, env!("CARGO_PKG_VERSION"));

    // Without overrides the neutral projection is used.
    let card = crate::a2a::card::build_card(
        "https://cloud.example.test",
        "Plain",
        "/a2a",
        &CardOverrides::default(),
    );
    assert_eq!(card.description, "Plain (A2A agent)");
    assert_eq!(card.skills[0].id, "chat");
    assert_eq!(card.protocol_version, "1.0.0");
    // A malformed file is an error, not a silent default.
    assert!(CardOverrides::from_json("{").is_err());
}

// ------------------------------------------- unreachable-in-a-happy-path arms

/// An env var that is removed when the test ends. `env_admin` holds the
/// process-wide lock, so nobody reads the value while this lives.
struct EnvFlag(&'static str);

impl Drop for EnvFlag {
    fn drop(&mut self) {
        std::env::remove_var(self.0);
    }
}

/// A push notifier that always fails: the run must be unaffected.
struct FaultyPush;

#[async_trait]
impl PushNotifier for FaultyPush {
    async fn notify(&self, _config: &PushNotificationConfig, _body: &Value) -> Result<(), String> {
        Err("push endpoint is down".into())
    }
}

#[tokio::test]
async fn a_message_for_an_unknown_context_starts_a_new_session() {
    let _env = env_admin().await;
    let mut h = harness_a2a(None);
    let agent = create_agent(&h, "contexts").await;
    set_default(&mut h, &agent);

    let (status, body) = send(
        &h.app,
        "POST",
        "/a2a",
        Some(ADMIN_KEY),
        Some(call(
            1,
            "message/send",
            json!({"message": {"kind": "message", "role": "user", "contextId": "sess_missing", "parts": [{"kind": "text", "text": "hi"}]}}),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["status"]["state"], "completed");
    let context = body["result"]["contextId"].as_str().unwrap();
    assert_ne!(context, "sess_missing", "an unknown context is not adopted");
    assert!(context.starts_with("sess_"), "{context}");
}

#[tokio::test]
async fn an_agent_without_a_name_or_with_a_model_override_still_runs() {
    let _env = env_admin().await;
    let mut h = harness_a2a(None);
    let agent_id = create_agent(&h, "renamed").await;
    let admin = Principal::admin();
    let mut agent = h.state.agents.get_agent(&admin, &agent_id).await.unwrap();
    agent.name = None;
    agent.model = Some("ariacompute/ariamodel".into());
    h.state.agents.save_agent(&agent).await.unwrap();
    set_default(&mut h, &agent_id);

    let (status, body) = rpc(&h.app, "message/send", json!({"message": msg("hi")})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["status"]["state"], "completed");

    // The card falls back to the id when the row has no name.
    let (status, card) = send(&h.app, "GET", "/.well-known/agent-card.json", None, None).await;
    assert_eq!(status, StatusCode::OK, "{card}");
    assert_eq!(card["name"], agent_id);
}

#[tokio::test]
async fn a_config_that_cannot_build_the_agent_fails_the_task() {
    let _env = env_admin().await;
    // `dsh` without an installed factory is a hard error (ADR-0019): the task
    // must fail with a reason instead of hanging in `submitted`.
    std::env::set_var("AGENT_HARNESS", "dsh");
    let _flag = EnvFlag("AGENT_HARNESS");
    let h = harness_a2a(None);
    let agent = create_agent(&h, "unbuildable").await;

    let (status, body) = send(
        &h.app,
        "POST",
        &format!("/v1/agents/{agent}/a2a"),
        Some(ADMIN_KEY),
        Some(call(1, "message/send", json!({"message": msg("hi")}))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["status"]["state"], "failed");
    assert!(
        body["result"]["status"]["message"]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .contains("cannot build the agent"),
        "{body}"
    );
}

/// A harness whose `run` refuses to start (no stream at all).
struct StartFails;

#[async_trait]
impl agent_core::seams::AgentLoop for StartFails {
    async fn run(
        &self,
        _ctx: agent_core::seams::LoopContext,
        _input: &str,
    ) -> Result<
        futures::stream::BoxStream<'static, Result<agent_core::AgentEvent, CoreError>>,
        CoreError,
    > {
        Err(CoreError::Config("the runtime refuses to start".into()))
    }
}

#[tokio::test]
async fn a_run_that_cannot_start_fails_the_task() {
    let _env = env_admin().await;
    std::env::set_var("AGENT_HARNESS", "dsh");
    let _flag = EnvFlag("AGENT_HARNESS");
    let mut h = harness_a2a(None);
    h.state.harness = Some(Arc::new(
        |_p: &Principal, _cfg: &agent_core::AgentConfig| {
            Arc::new(StartFails) as Arc<dyn agent_core::seams::AgentLoop>
        },
    ));
    h.app = build_router(h.state.clone());
    let agent = create_agent(&h, "stuck").await;

    let (status, body) = send(
        &h.app,
        "POST",
        &format!("/v1/agents/{agent}/a2a"),
        Some(ADMIN_KEY),
        Some(call(1, "message/send", json!({"message": msg("hi")}))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["status"]["state"], "failed");
    assert!(
        body["result"]["status"]["message"]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .contains("refuses to start"),
        "{body}"
    );
}

#[tokio::test]
async fn a_lost_completion_is_reported_as_canceled_and_closes_the_turn() {
    let _env = env_admin().await;
    let mut h = harness_a2a(None);
    h.state.a2a = A2aState {
        tasks: Arc::new(FaultyTasks::transition_to(
            TaskState::Completed,
            TransitionFault::Lost,
        )),
        ..h.state.a2a.clone()
    };
    h.app = build_router(h.state.clone());
    let agent = create_agent(&h, "racing").await;

    let (status, body) = send(
        &h.app,
        "POST",
        &format!("/v1/agents/{agent}/a2a"),
        Some(ADMIN_KEY),
        Some(call(1, "message/send", json!({"message": msg("hi")}))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The CAS was lost, so the task is not claimed as completed.
    assert_ne!(body["result"]["status"]["state"], "completed");
    let context = body["result"]["contextId"].as_str().unwrap();
    let (_, turns) = send(
        &h.app,
        "GET",
        &format!("/v1/agents/sessions/{context}/turns"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(turns["data"][0]["status"], "canceled", "{turns}");
    let (_, items) = send(
        &h.app,
        "GET",
        &format!("/v1/agents/sessions/{context}/items"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(items["data"][1]["status"], "incomplete", "{items}");
}

#[tokio::test]
async fn a_push_that_cannot_be_delivered_never_changes_the_outcome() {
    let _env = env_admin().await;
    let mut h = harness_a2a(None);
    h.state.a2a = A2aState {
        push: Arc::new(FaultyPush),
        ..h.state.a2a.clone()
    };
    h.app = build_router(h.state.clone());
    let agent = create_agent(&h, "unreachable-hook").await;

    let (status, body) = send(
        &h.app,
        "POST",
        &format!("/v1/agents/{agent}/a2a"),
        Some(ADMIN_KEY),
        Some(call(
            1,
            "message/send",
            json!({
                "message": msg("hi"),
                "configuration": {"pushNotification": {"url": "https://hook.example.test/x"}},
            }),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["status"]["state"], "completed");
}

#[tokio::test]
async fn an_unparsable_status_falls_back_to_the_row_state() {
    let _env = env_admin().await;
    let h = harness_a2a(None);
    let admin = Principal::admin();
    let mut row = submitted_row("task_bogus", &admin.id, "sess_bogus");
    row.status = Some(json!({"state": "not-a-state"}));
    h.state.a2a.tasks.create(&admin, &row).await.unwrap();

    let (status, body) = rpc(&h.app, "tasks/get", json!({"id": "task_bogus"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["status"]["state"], "submitted");
    assert!(body["result"]["status"]["timestamp"].is_string(), "{body}");
}

#[tokio::test]
async fn cancel_reports_a_lost_race_a_conflict_and_a_broken_store() {
    let _env = env_admin().await;
    let admin = Principal::admin();

    let mut lost = harness_a2a(None);
    lost.state.a2a = A2aState {
        tasks: Arc::new(FaultyTasks::transition_to(
            TaskState::Canceled,
            TransitionFault::Lost,
        )),
        ..lost.state.a2a.clone()
    };
    lost.state
        .a2a
        .tasks
        .create(&admin, &submitted_row("task_c1", &admin.id, "sess_c1"))
        .await
        .unwrap();
    lost.app = build_router(lost.state.clone());
    let (_, body) = rpc(&lost.app, "tasks/cancel", json!({"id": "task_c1"})).await;
    assert_eq!(body["error"]["code"], code::TASK_NOT_CANCELABLE, "{body}");

    let mut conflicting = harness_a2a(None);
    conflicting.state.a2a = A2aState {
        tasks: Arc::new(FaultyTasks::transition_to(
            TaskState::Canceled,
            TransitionFault::Conflict,
        )),
        ..conflicting.state.a2a.clone()
    };
    conflicting
        .state
        .a2a
        .tasks
        .create(&admin, &submitted_row("task_c2", &admin.id, "sess_c2"))
        .await
        .unwrap();
    conflicting.app = build_router(conflicting.state.clone());
    let (_, body) = rpc(&conflicting.app, "tasks/cancel", json!({"id": "task_c2"})).await;
    assert_eq!(
        body["error"]["code"],
        code::INVALID_STATE_TRANSITION,
        "{body}"
    );

    let mut broken = harness_a2a(None);
    broken.state.a2a = A2aState {
        tasks: Arc::new(FaultyTasks::transition_to(
            TaskState::Canceled,
            TransitionFault::Error,
        )),
        ..broken.state.a2a.clone()
    };
    broken
        .state
        .a2a
        .tasks
        .create(&admin, &submitted_row("task_c3", &admin.id, "sess_c3"))
        .await
        .unwrap();
    broken.app = build_router(broken.state.clone());
    let (_, body) = rpc(&broken.app, "tasks/cancel", json!({"id": "task_c3"})).await;
    assert_eq!(body["error"]["code"], code::INTERNAL_ERROR, "{body}");

    // A store that cannot even read the row maps to the same internal error.
    let mut unreadable = harness_a2a(None);
    unreadable.state.a2a = A2aState {
        tasks: Arc::new(FaultyTasks::failing_get()),
        ..unreadable.state.a2a.clone()
    };
    unreadable.app = build_router(unreadable.state.clone());
    let (_, body) = rpc(&unreadable.app, "tasks/cancel", json!({"id": "task_any"})).await;
    assert_eq!(body["error"]["code"], code::INTERNAL_ERROR, "{body}");
}

#[tokio::test]
async fn a_send_whose_task_row_vanishes_reports_what_was_accepted() {
    let _env = env_admin().await;
    let mut h = harness_a2a(None);
    h.state.a2a = A2aState {
        tasks: Arc::new(FaultyTasks::failing_get()),
        ..h.state.a2a.clone()
    };
    h.app = build_router(h.state.clone());
    let agent = create_agent(&h, "vanishing").await;

    let (status, body) = send(
        &h.app,
        "POST",
        &format!("/v1/agents/{agent}/a2a"),
        Some(ADMIN_KEY),
        Some(call(1, "message/send", json!({"message": msg("hi")}))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["status"]["state"], "submitted");
    assert!(body["result"]["id"].as_str().unwrap().starts_with("task_"));
}

#[tokio::test]
async fn malformed_params_are_invalid_params_on_every_method() {
    let _env = env_admin().await;
    let h = harness_a2a(None);
    let agent = create_agent(&h, "strict").await;
    let path = format!("/v1/agents/{agent}/a2a");

    let cases: Vec<(&str, Value)> = vec![
        // `message` is not a message object.
        (
            "message/send",
            json!({"message": {"kind": "message", "role": 5}}),
        ),
        (
            "message/stream",
            json!({"message": {"kind": "message", "role": 5}}),
        ),
        // `params` of the wrong shape: a struct cannot come from an integer.
        ("tasks/get", json!(7)),
        ("tasks/get", json!({"id": ""})),
        ("tasks/cancel", json!(7)),
        ("tasks/resume", json!(7)),
        ("tasks/pushNotification/set", json!(7)),
        ("tasks/pushNotification/get", json!(7)),
    ];
    for (method, params) in cases {
        let (status, body) = send(
            &h.app,
            "POST",
            &path,
            Some(ADMIN_KEY),
            Some(call(1, method, params.clone())),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{method} {params}: {body}");
        assert_eq!(
            body["error"]["code"],
            code::INVALID_PARAMS,
            "{method} {params}: {body}"
        );
    }
}

#[tokio::test]
async fn a_lagged_reader_skips_ahead_and_a_closed_channel_ends_the_stream() {
    let (tx, mut rx) = broadcast::channel(1);
    tx.send(progress_frame("task_lag")).unwrap();
    tx.send(final_frame("task_lag")).unwrap();
    let frame = crate::a2a::rpc::recv_frame(&mut rx)
        .await
        .expect("a lagged reader still gets the newest frame");
    assert!(frame.is_final(), "the older frame was dropped");
    drop(tx);
    assert!(
        crate::a2a::rpc::recv_frame(&mut rx).await.is_none(),
        "a closed channel ends the stream"
    );
}

#[tokio::test]
async fn the_card_of_a_disabled_deployment_is_a_404() {
    let _env = env_admin().await;
    let mut h = harness();
    // A default agent is configured, but the surface is off: discovery must
    // answer 404 rather than describe an agent nobody can call.
    h.state.a2a = A2aState {
        config: Arc::new(A2aConfig {
            default_agent: Some("agent-off".into()),
            public_url: "https://cloud.example.test/".into(),
            cards: CardOverrides::default(),
        }),
        ..A2aState::disabled()
    };
    h.app = build_router(h.state.clone());
    let (status, body) = send(&h.app, "GET", "/.well-known/agent-card.json", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "agent_not_found");
    let (status, _) = send(
        &h.app,
        "GET",
        "/v1/agents/agent-off/agent-card.json",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------- models

/// A model that asks one question through the `ask_user` tool, then answers.
struct AskModel;

#[async_trait]
impl ModelClient for AskModel {
    async fn complete(&self, req: &ModelRequest) -> Result<ModelResponse, CoreError> {
        Ok(ModelResponse {
            text: format!("final: {}", req.input),
        })
    }
    async fn complete_with_tools(
        &self,
        req: &ModelRequest,
        _tools: &[Tool],
    ) -> Result<ModelTurn, CoreError> {
        if req.context.contains("TOOL_RESULT") {
            Ok(ModelTurn::reply("answered after question"))
        } else {
            Ok(ModelTurn {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call_q".into(),
                    name: "ask_user".into(),
                    arguments: json!({"question": "which colour?", "options": ["red", "blue"]}),
                }],
                usage: None,
            })
        }
    }
}
