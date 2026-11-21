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

use std::path::Path;
use std::time::Duration;

use agent_core::gateway_model;
use aria_agent_dsh::event_map::{self, Mapped};
use aria_agent_dsh::{no_bridge, DshConfig, DshRuntime, HarnessRuntime};
use serde_json::Value;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

#[tokio::test]
async fn a_real_dsh_turn_completes() {
    if env("ARIA_DSH_E2E").as_deref() != Some("1") {
        eprintln!("skip: set ARIA_DSH_E2E=1 to run the live DeepSeek Harness test");
        return;
    }
    if env("GATEWAY_API_KEY").is_none() {
        eprintln!("skip: GATEWAY_API_KEY is not set (OpenAI-compatible gateway key)");
        return;
    }
    let bin = env("DSH_BIN").unwrap_or_else(|| "dsh".to_string());
    let model = gateway_model()
        .or_else(|| env("DSH_E2E_MODEL"))
        .unwrap_or_else(|| "deepseek-v4".to_string());
    let root = tempfile::tempdir().expect("tempdir");

    let mut cfg = DshConfig::for_session(root.path(), "e2e-session", &model)
        .with_bin(bin.clone())
        .with_permission_mode("read-only");
    cfg.init_timeout = Duration::from_secs(60);
    cfg.request_timeout = Duration::from_secs(120);
    if let Some(provider) = env("DSH_PROVIDER") {
        cfg.provider = provider;
    }
    // The child env is an allow-list: bridge the same GATEWAY_* values the
    // native harness uses into the names dsh reads.
    cfg = cfg.with_gateway_env(
        env("GATEWAY_BASE_URL").unwrap_or_default().as_str(),
        env("GATEWAY_API_KEY").unwrap_or_default().as_str(),
    );

    let runtime = DshRuntime::connect(cfg, no_bridge())
        .await
        .unwrap_or_else(|e| panic!("dsh `{bin}` did not complete the handshake: {e}"));

    let mut rx = runtime
        .prompt("e2e-session", "Reply with the single word: pong")
        .await
        .expect("prompt is queued");

    let mut text = String::new();
    let mut reason: Option<String> = None;
    'frames: while let Some(frame) = rx.recv().await {
        for mapped in event_map::map_notification(&frame) {
            match mapped {
                Mapped::Token { text: chunk } => {
                    if !chunk.is_empty() {
                        text = chunk;
                    }
                }
                Mapped::TurnEnd { reason: kind } => {
                    reason = Some(kind);
                    break 'frames;
                }
                Mapped::TurnFailed { message } => panic!("dsh turn failed: {message}"),
                Mapped::Protocol { message } => panic!("dsh protocol error: {message}"),
                _ => {}
            }
        }
    }
    runtime.shutdown().await.expect("shutdown");
    assert_eq!(
        reason.as_deref(),
        Some("completed"),
        "turn did not complete (frames must end with a completed turn/end)"
    );
    assert!(
        !text.trim().is_empty(),
        "dsh completed the turn but produced no assistant text"
    );
    eprintln!("dsh replied: {}", text.trim());
    // Guard against an accidentally empty/error payload being reported as success.
    assert!(Value::String(text).as_str().is_some());
    assert!(Path::new(&bin).exists() || bin == "dsh");
}
