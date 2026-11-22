//! Live end-to-end: a **real** `dsh` runtime answering a real model call.
//!
//! Gated, never part of the default test run:
//!
//! ```text
//! ARIA_DSH_E2E=1 \
//! DSH_BIN=/path/to/dsh \
//! GATEWAY_BASE_URL=https://dashscope.aliyuncs.com/compatible-mode/v1 \
//! GATEWAY_API_KEY=sk-… \
//! cargo test -p aria-agent-dsh --test e2e_live -- --nocapture
//! ```
//!
//! Without `ARIA_DSH_E2E=1` (and a runtime plus a provider endpoint) the test
//! prints why it cannot run and returns. It **never** fabricates a passing run:
//! a missing dependency is a skip, a broken turn is a failure.
//!
//! Two scenarios:
//! * `a_real_dsh_turn_completes` — one turn straight at the gateway.
//! * `two_turns_through_the_filter_proxy_complete` — the regression for
//!   `redacted_thinking`: the same session is prompted twice, so dsh replays the
//!   first assistant turn from its own history while talking to
//!   [`ThinkingFilterProxy`]. The bundled adapter failed that replay with
//!   `UNSUPPORTED_CONTENT` before the proxy existed.

use std::path::Path;
use std::time::Duration;

use agent_core::gateway_model;
use aria_agent_dsh::event_map::{self, failure_detail, Mapped};
use aria_agent_dsh::{no_bridge, DshConfig, DshRuntime, HarnessRuntime, ThinkingFilterProxy};
use serde_json::json;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// Why the live suite cannot run, or `None` when it can.
fn skip_reason() -> Option<&'static str> {
    if env("ARIA_DSH_E2E").as_deref() != Some("1") {
        return Some("set ARIA_DSH_E2E=1 to run the live DeepSeek Harness test");
    }
    if env("GATEWAY_API_KEY").is_none() {
        return Some("GATEWAY_API_KEY is not set (OpenAI-compatible gateway key)");
    }
    None
}

fn live_config(root: &Path, session: &str) -> (String, DshConfig) {
    let bin = env("DSH_BIN").unwrap_or_else(|| "dsh".to_string());
    let model = gateway_model()
        .or_else(|| env("DSH_E2E_MODEL"))
        .unwrap_or_else(|| "deepseek-v4".to_string());
    let mut cfg = DshConfig::for_session(root, session, &model)
        .with_bin(bin.clone())
        .with_permission_mode("read-only");
    cfg.init_timeout = Duration::from_secs(60);
    cfg.request_timeout = Duration::from_secs(120);
    if let Some(provider) = env("DSH_PROVIDER") {
        cfg.provider = provider;
    }
    (bin, cfg)
}

/// Drive one prompt to completion and return the assistant text. Fails loudly on
/// a provider failure, a protocol violation, or a stream that ends without
/// `turn/end`.
async fn run_turn(runtime: &dyn HarnessRuntime, session: &str, prompt: &str) -> String {
    let mut rx = runtime
        .prompt(session, prompt)
        .await
        .expect("prompt is queued");
    let mut chunks: Vec<String> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    while let Some(frame) = rx.recv().await {
        if let Some(detail) = failure_detail(&frame) {
            eprintln!("dsh failure: {detail}");
            failures.push(detail);
        }
        for mapped in event_map::map_notification(&frame) {
            match mapped {
                // One `Token` per assistant message. dsh can report the same
                // message twice (`assistant/message` then `.../committed`), so
                // a repeat of the previous chunk is not new text.
                Mapped::Token { text: chunk } => {
                    if !chunk.is_empty() && chunks.last() != Some(&chunk) {
                        chunks.push(chunk);
                    }
                }
                Mapped::TurnEnd { reason } => {
                    assert_eq!(
                        reason,
                        "completed",
                        "turn did not complete: {}",
                        failures.join("\n")
                    );
                    return chunks.join("\n");
                }
                Mapped::TurnFailed { message } => {
                    panic!("dsh turn failed: {message}\n{}", failures.join("\n"))
                }
                Mapped::Protocol { message } => panic!("dsh protocol error: {message}"),
                _ => {}
            }
        }
    }
    panic!(
        "stream ended without turn/end; provider failures:\n{}",
        failures.join("\n")
    );
}

#[test]
fn failure_detail_extracts_dsh_error() {
    let frame = json!({
        "method": "session.event",
        "params": {
            "sessionId": "s1",
            "event": {
                "type": "assistant/attempt",
                "data": {
                    "stream": [{
                        "chunk": {
                            "reason": {
                                "kind": "error",
                                "failure": {
                                    "code": "TRANSPORT",
                                    "message": "DeepSeek Messages transport failed"
                                }
                            }
                        }
                    }]
                }
            }
        }
    });
    assert_eq!(
        failure_detail(&frame).as_deref(),
        Some("assistant/attempt: TRANSPORT DeepSeek Messages transport failed")
    );
}

#[tokio::test]
async fn a_real_dsh_turn_completes() {
    if let Some(reason) = skip_reason() {
        eprintln!("skip: {reason}");
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let (bin, cfg) = live_config(root.path(), "e2e-session");
    // The child env is an allow-list: bridge the same GATEWAY_* values the
    // native harness uses into the names dsh reads.
    let cfg = cfg.with_gateway_env(
        env("GATEWAY_BASE_URL").unwrap_or_default().as_str(),
        env("GATEWAY_API_KEY").unwrap_or_default().as_str(),
    );

    let runtime = DshRuntime::connect(cfg, no_bridge())
        .await
        .unwrap_or_else(|e| panic!("dsh `{bin}` did not complete the handshake: {e}"));
    let text = run_turn(&runtime, "e2e-session", "Reply with the single word: pong").await;
    runtime.shutdown().await.expect("shutdown");

    assert!(
        !text.trim().is_empty(),
        "dsh completed the turn but produced no assistant text"
    );
    eprintln!("dsh replied: {}", text.trim());
}

/// The `redacted_thinking` regression, against a real runtime: turn two replays
/// the assistant history dsh persisted in turn one.
#[tokio::test]
async fn two_turns_through_the_filter_proxy_complete() {
    if let Some(reason) = skip_reason() {
        eprintln!("skip: {reason}");
        return;
    }
    let Some(gateway_base) = env("GATEWAY_BASE_URL") else {
        eprintln!("skip: GATEWAY_BASE_URL is required to point the filter proxy at a gateway");
        return;
    };

    let proxy = ThinkingFilterProxy::start(&gateway_base)
        .await
        .expect("thinking filter proxy starts");
    eprintln!("filter proxy listening on {}", proxy.local_base_url());

    let root = tempfile::tempdir().expect("tempdir");
    let (bin, cfg) = live_config(root.path(), "e2e-replay");
    let cfg = cfg.with_gateway_env(
        &proxy.local_base_url(),
        env("GATEWAY_API_KEY").unwrap_or_default().as_str(),
    );

    let runtime = DshRuntime::connect(cfg, no_bridge())
        .await
        .unwrap_or_else(|e| panic!("dsh `{bin}` did not complete the handshake: {e}"));

    let first = run_turn(
        &runtime,
        "e2e-replay",
        "State the colour of the sky in one word.",
    )
    .await;
    // Same session: dsh now replays the assistant turn (thinking included) as
    // history. This is the request that used to fail with UNSUPPORTED_CONTENT.
    let second = run_turn(
        &runtime,
        "e2e-replay",
        "Repeat the single word you just answered with.",
    )
    .await;
    runtime.shutdown().await.expect("shutdown");

    assert!(!first.trim().is_empty(), "first turn produced no text");
    assert!(!second.trim().is_empty(), "second turn produced no text");
    eprintln!("turn 1: {}\nturn 2: {}", first.trim(), second.trim());
}
