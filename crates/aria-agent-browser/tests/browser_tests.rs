//! Integration tests for the browser capability: engine/catalog, sandbox op
//! forwarding, graceful `Unsupported` degradation, no‑daemon safety, dynamic
//! `switch_to` (success + rollback), and `BrowserToolHandler` routing.

use agent_browser::{
    BrowserCatalog, BrowserEngine, BrowserError, BrowserKind, BrowserSandbox, BrowserToolHandler,
};
use agent_core::context::MemoryContextStore;
use agent_core::{ToolCall, ToolHandler, ToolResult};
use agent_sandbox::{ExecOutput, ExecSpec, Sandbox, SandboxError, SandboxHandle};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// A sandbox double that returns a canned `runner.mjs` JSON response for every
/// exec, and whose spawn can be made to fail (to simulate a missing daemon or a
/// failed `switch_to`).
struct FakeSandbox {
    response: String,
    fail_from: usize,
    calls: AtomicUsize,
}

impl FakeSandbox {
    fn ok(response: &str) -> Self {
        Self {
            response: response.to_string(),
            fail_from: usize::MAX,
            calls: AtomicUsize::new(0),
        }
    }
    /// Spawn fails once `spawn` has been called `fail_from` times or more.
    fn fail_after(response: &str, fail_from: usize) -> Self {
        Self {
            response: response.to_string(),
            fail_from,
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl Sandbox for FakeSandbox {
    async fn spawn(&self, _spec: &ExecSpec) -> Result<SandboxHandle, SandboxError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n >= self.fail_from {
            Err(SandboxError::NotConfigured("no daemon".into()))
        } else {
            Ok(SandboxHandle { id: "fake".into() })
        }
    }
    async fn exec(
        &self,
        _handle: &SandboxHandle,
        _cmd: &[String],
    ) -> Result<ExecOutput, SandboxError> {
        Ok(ExecOutput {
            exit_code: 0,
            stdout: self.response.clone(),
            stderr: String::new(),
        })
    }
    async fn destroy(&self, _handle: SandboxHandle) -> Result<(), SandboxError> {
        Ok(())
    }
}

fn sandbox_with(response: &str, kind: BrowserKind) -> Arc<BrowserSandbox> {
    Arc::new(BrowserSandbox::with_catalog(
        Box::new(FakeSandbox::ok(response)),
        BrowserCatalog::builtin(),
        kind,
    ))
}

#[test]
fn construct_without_daemon_never_panics() {
    // Construction is lazy; no Docker connection is attempted.
    let _ = BrowserSandbox::new();
}

#[tokio::test]
async fn navigate_forwards_op_and_parses_result() {
    let bs = sandbox_with(
        r#"{"ok":true,"data":{"title":"Example Domain"}}"#,
        BrowserKind::Chromium,
    );
    let r = bs
        .navigate("https://example.com")
        .await
        .expect("navigate ok");
    assert!(r.summary().contains("Example Domain"));
}

#[tokio::test]
async fn empty_url_is_invalid_param() {
    let bs = sandbox_with(r#"{"ok":true,"data":{}}"#, BrowserKind::Chromium);
    assert!(matches!(
        bs.navigate("").await,
        Err(BrowserError::InvalidParam(_))
    ));
}

#[tokio::test]
async fn unsupported_op_degrades_gracefully() {
    // Servo/Gosub return `unsupported` for interactive ops.
    let bs = sandbox_with(
        r#"{"ok":false,"error":"unsupported op click for servo"}"#,
        BrowserKind::Servo,
    );
    let err = bs
        .click("button")
        .await
        .expect_err("servo click unsupported");
    assert!(matches!(err, BrowserError::Unsupported(_, _)));
}

#[tokio::test]
async fn no_daemon_surfaces_spawn_error() {
    let bs = BrowserSandbox::with_catalog(
        Box::new(FakeSandbox::fail_after("", 0)),
        BrowserCatalog::builtin(),
        BrowserKind::Chromium,
    );
    let err = bs.navigate("https://x.com").await.expect_err("no daemon");
    assert!(matches!(err, BrowserError::Spawn(_)));
}

#[tokio::test]
async fn switch_to_changes_active_engine() {
    let bs = sandbox_with(r#"{"ok":true,"data":{}}"#, BrowserKind::Chromium);
    bs.switch_to(BrowserKind::Camoufox, None)
        .await
        .expect("switch ok");
    assert_eq!(bs.kind(), BrowserKind::Camoufox);
}

#[tokio::test]
async fn switch_to_rolls_back_on_spawn_failure() {
    // First spawn (initial ensure) succeeds; the spawn triggered by switch_to
    // fails, so the engine must remain Chromium.
    let bs = BrowserSandbox::with_catalog(
        Box::new(FakeSandbox::fail_after("", 1)),
        BrowserCatalog::builtin(),
        BrowserKind::Chromium,
    );
    // Prime the initial container.
    let _ = bs.navigate("https://x.com").await;
    let err = bs
        .switch_to(BrowserKind::Gosub, None)
        .await
        .expect_err("switch must fail");
    assert!(matches!(err, BrowserError::Spawn(_)));
    // Rollback: engine unchanged.
    assert_eq!(bs.kind(), BrowserKind::Chromium);
}

#[test]
fn handler_routes_browser_tools_only() {
    let h = BrowserToolHandler::new(Arc::new(BrowserSandbox::new()));
    assert!(h.handles("browser_navigate"));
    assert!(h.handles("browser_use"));
    assert!(h.handles("browser_solve_captcha"));
    assert!(!h.handles("shell"));
    assert!(!h.handles("fs_write"));
}

#[tokio::test]
async fn handler_runs_navigate_and_persists() {
    let bs = sandbox_with(
        r#"{"ok":true,"data":{"title":"ACME"}}"#,
        BrowserKind::Chromium,
    );
    let ctx: Arc<dyn agent_core::context::ContextStore> = MemoryContextStore::new();
    let handler = BrowserToolHandler::new(bs.clone());
    let call = ToolCall {
        id: "c1".into(),
        name: "browser_navigate".into(),
        arguments: json!({ "url": "https://acme.test" }),
    };
    let res: ToolResult = handler.run(&call, &ctx, "sess").await;
    assert!(!res.is_error, "got error: {}", res.content);
    assert!(res.content.contains("ACME"));
}

#[tokio::test]
async fn handler_switch_via_browser_use() {
    let bs = sandbox_with(r#"{"ok":true,"data":{}}"#, BrowserKind::Chromium);
    let kind_before = bs.kind();
    let ctx: Arc<dyn agent_core::context::ContextStore> = MemoryContextStore::new();
    let handler = BrowserToolHandler::new(bs.clone());
    let call = ToolCall {
        id: "c2".into(),
        name: "browser_use".into(),
        arguments: json!({ "engine": "lightpanda" }),
    };
    let res = handler.run(&call, &ctx, "sess").await;
    assert!(!res.is_error, "switch failed: {}", res.content);
    assert_eq!(bs.kind(), BrowserKind::Lightpanda);
    assert_eq!(kind_before, BrowserKind::Chromium);
}

#[tokio::test]
async fn handler_bubbles_engine_error() {
    let bs = sandbox_with(r#"{"ok":false,"error":"boom"}"#, BrowserKind::Chromium);
    let ctx: Arc<dyn agent_core::context::ContextStore> = MemoryContextStore::new();
    let handler = BrowserToolHandler::new(bs.clone());
    let call = ToolCall {
        id: "c3".into(),
        name: "browser_extract".into(),
        arguments: json!({}),
    };
    let res = handler.run(&call, &ctx, "sess").await;
    assert!(res.is_error);
    assert!(res.content.starts_with("ERROR:"));
}
