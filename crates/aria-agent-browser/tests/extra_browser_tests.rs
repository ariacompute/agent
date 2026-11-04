//! Operation-level coverage for the browser tool family: every `browser_*`
//! command, its argument validation, the image override path, and the
//! `Unsupported` / `Engine` degradation branches.

use agent_browser::{
    browser_tools, default_browser, BrowserCatalog, BrowserEngine, BrowserError, BrowserKind,
    BrowserSandbox, BrowserToolHandler,
};
use agent_core::context::{ContextStore, MemoryContextStore};
use agent_core::{ToolCall, ToolHandler};
use agent_sandbox::{ExecOutput, ExecSpec, Sandbox, SandboxError, SandboxHandle};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// A sandbox double that answers every exec with a canned `runner.mjs` payload
/// and records the images it was asked to spawn.
struct FakeSandbox {
    response: String,
    exit_code: i32,
    stderr: String,
    spawned_images: Mutex<Vec<Option<String>>>,
    calls: AtomicUsize,
}

impl FakeSandbox {
    fn new(response: &str) -> Arc<Self> {
        Arc::new(Self {
            response: response.to_string(),
            exit_code: 0,
            stderr: String::new(),
            spawned_images: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        })
    }

    /// A runner that exits non-zero and reports on stderr.
    fn failing(stderr: &str) -> Arc<Self> {
        Arc::new(Self {
            response: String::new(),
            exit_code: 2,
            stderr: stderr.to_string(),
            spawned_images: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        })
    }

    fn images(&self) -> Vec<Option<String>> {
        self.spawned_images.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Sandbox for FakeSandbox {
    async fn spawn(&self, spec: &ExecSpec) -> Result<SandboxHandle, SandboxError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.spawned_images.lock().unwrap().push(spec.image.clone());
        Ok(SandboxHandle { id: "fake".into() })
    }
    async fn exec(
        &self,
        _handle: &SandboxHandle,
        _cmd: &[String],
    ) -> Result<ExecOutput, SandboxError> {
        Ok(ExecOutput {
            exit_code: self.exit_code,
            stdout: self.response.clone(),
            stderr: self.stderr.clone(),
        })
    }
    async fn destroy(&self, _handle: SandboxHandle) -> Result<(), SandboxError> {
        Ok(())
    }
}

/// `Box<dyn Sandbox>` adapter so a test keeps a handle on the double.
struct Shared(Arc<FakeSandbox>);

#[async_trait::async_trait]
impl Sandbox for Shared {
    async fn spawn(&self, spec: &ExecSpec) -> Result<SandboxHandle, SandboxError> {
        self.0.spawn(spec).await
    }
    async fn exec(
        &self,
        handle: &SandboxHandle,
        cmd: &[String],
    ) -> Result<ExecOutput, SandboxError> {
        self.0.exec(handle, cmd).await
    }
    async fn destroy(&self, handle: SandboxHandle) -> Result<(), SandboxError> {
        self.0.destroy(handle).await
    }
}

fn sandbox(response: &str, kind: BrowserKind) -> Arc<BrowserSandbox> {
    Arc::new(BrowserSandbox::with_catalog(
        Box::new(Shared(FakeSandbox::new(response))),
        BrowserCatalog::builtin(),
        kind,
    ))
}

fn ctx() -> Arc<dyn ContextStore> {
    MemoryContextStore::new()
}

fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: "c".into(),
        name: name.into(),
        arguments,
    }
}

#[test]
fn default_browser_is_constructible() {
    let browser = default_browser();
    // The kind comes from the env (Chromium by default).
    assert_eq!(browser.kind(), BrowserCatalog::default_kind());
}

#[tokio::test]
async fn every_engine_operation_forwards_its_op() {
    let bs = sandbox(
        r#"{"ok":true,"data":{"title":"T","text":"body","screenshot":"AA"}}"#,
        BrowserKind::Chromium,
    );
    assert_eq!(bs.extract().await.unwrap().summary(), "title=T");
    assert_eq!(bs.click("#go").await.unwrap().data()["text"], "body");
    assert_eq!(bs.fill("#q", "rust").await.unwrap().data()["text"], "body");
    assert_eq!(bs.screenshot().await.unwrap().data()["screenshot"], "AA");
    assert_eq!(bs.evaluate("1+1").await.unwrap().data()["text"], "body");
    assert_eq!(bs.solve_captcha().await.unwrap().data()["text"], "body");
    // A second call reuses the cached container handle.
    assert_eq!(bs.extract().await.unwrap().data()["title"], "T");
}

#[tokio::test]
async fn empty_parameters_are_rejected_before_touching_the_engine() {
    let inner = FakeSandbox::new(r#"{"ok":true,"data":{}}"#);
    let bs = BrowserSandbox::with_catalog(
        Box::new(Shared(inner.clone())),
        BrowserCatalog::builtin(),
        BrowserKind::Chromium,
    );
    assert!(matches!(
        bs.click("  ").await,
        Err(BrowserError::InvalidParam(_))
    ));
    assert!(matches!(
        bs.fill("", "x").await,
        Err(BrowserError::InvalidParam(_))
    ));
    assert!(matches!(
        bs.evaluate("").await,
        Err(BrowserError::InvalidParam(_))
    ));
    // Nothing was spawned for rejected calls.
    assert!(inner.images().is_empty());
}

#[tokio::test]
async fn switch_to_records_an_image_override() {
    let inner = FakeSandbox::new(r#"{"ok":true,"data":{}}"#);
    let bs = BrowserSandbox::with_catalog(
        Box::new(Shared(inner.clone())),
        BrowserCatalog::builtin(),
        BrowserKind::Chromium,
    );
    bs.switch_to(BrowserKind::Camoufox, Some("custom/camoufox:test".into()))
        .await
        .unwrap();
    assert_eq!(
        inner.images().last().cloned().unwrap(),
        Some("custom/camoufox:test".to_string())
    );
    assert_eq!(bs.kind(), BrowserKind::Camoufox);
}

#[tokio::test]
async fn engine_errors_round_trip_from_the_runner() {
    let bs = sandbox(r#"{"ok":false,"error":"boom"}"#, BrowserKind::Chromium);
    assert!(matches!(
        bs.extract().await,
        Err(BrowserError::Engine(msg)) if msg == "boom"
    ));
    // An error without a message still degrades to a readable reason.
    let bs = sandbox(r#"{"ok":false}"#, BrowserKind::Chromium);
    assert!(matches!(bs.extract().await, Err(BrowserError::Engine(_))));
    // Unsupported ops name the engine.
    let bs = sandbox(
        r#"{"ok":false,"error":"unsupported op for gosub"}"#,
        BrowserKind::Gosub,
    );
    assert!(matches!(
        bs.screenshot().await,
        Err(BrowserError::Unsupported(kind, _)) if kind == "gosub"
    ));
}

#[tokio::test]
async fn handler_covers_the_whole_tool_family() {
    let bs = sandbox(
        r#"{"ok":true,"data":{"title":"ACME","screenshot":"QUJD"}}"#,
        BrowserKind::Chromium,
    );
    let handler = BrowserToolHandler::new(bs.clone());
    let ctx = ctx();

    let navigated = handler
        .run(
            &call("browser_navigate", json!({"url": "https://acme.test"})),
            &ctx,
            "s",
        )
        .await;
    assert!(!navigated.is_error, "{}", navigated.content);
    assert!(navigated.content.contains("navigated to https://acme.test"));

    let extracted = handler
        .run(&call("browser_extract", json!({})), &ctx, "s")
        .await;
    assert!(!extracted.is_error, "{}", extracted.content);
    assert!(extracted.content.starts_with("extracted: title=ACME"));

    let clicked = handler
        .run(
            &call("browser_click", json!({"selector": "#go"})),
            &ctx,
            "s",
        )
        .await;
    assert!(!clicked.is_error, "{}", clicked.content);

    let filled = handler
        .run(
            &call("browser_fill", json!({"selector": "#q", "text": "rust"})),
            &ctx,
            "s",
        )
        .await;
    assert!(!filled.is_error, "{}", filled.content);
    assert!(filled.content.starts_with("filled #q"));

    let shot = handler
        .run(&call("browser_screenshot", json!({})), &ctx, "s")
        .await;
    assert!(!shot.is_error, "{}", shot.content);
    assert!(shot
        .content
        .contains("screenshot captured (4 base64 chars)"));
    // Short payloads are not truncated into the tool output.
    assert!(!shot.content.contains(": QUJD"));

    let evaluated = handler
        .run(&call("browser_evaluate", json!({"js": "1+1"})), &ctx, "s")
        .await;
    assert!(!evaluated.is_error, "{}", evaluated.content);

    let captcha = handler
        .run(&call("browser_solve_captcha", json!({})), &ctx, "s")
        .await;
    assert!(!captcha.is_error, "{}", captcha.content);
    assert!(captcha.content.starts_with("captcha: "));

    let switched = handler
        .run(
            &call(
                "browser_use",
                json!({"engine": "servo", "image": "custom/servo:1"}),
            ),
            &ctx,
            "s",
        )
        .await;
    assert!(!switched.is_error, "{}", switched.content);
    assert_eq!(bs.kind(), BrowserKind::Servo);

    // `browser_use` without an engine keeps the active one.
    let kept = handler
        .run(&call("browser_use", json!({})), &ctx, "s")
        .await;
    assert!(!kept.is_error, "{}", kept.content);
    assert_eq!(bs.kind(), BrowserKind::Servo);
}

#[tokio::test]
async fn handler_reports_bad_arguments_and_unknown_tools() {
    let bs = sandbox(r#"{"ok":true,"data":{}}"#, BrowserKind::Chromium);
    let handler = BrowserToolHandler::new(bs);
    let ctx = ctx();

    for (name, args, needle) in [
        ("browser_navigate", json!({}), "url required"),
        ("browser_click", json!({}), "selector required"),
        ("browser_fill", json!({"selector": "#q"}), "text required"),
        ("browser_evaluate", json!({}), "js required"),
        (
            "browser_use",
            json!({"engine": "netscape"}),
            "unknown engine",
        ),
        ("browser_nope", json!({}), "unknown browser tool"),
    ] {
        let res = handler.run(&call(name, args), &ctx, "s").await;
        assert!(res.is_error, "{name} must fail");
        assert!(res.content.contains(needle), "{name}: {}", res.content);
        assert!(res.content.starts_with("ERROR: "), "{}", res.content);
    }
}

#[tokio::test]
async fn handler_renders_a_long_screenshot_snippet() {
    let payload = "A".repeat(80);
    let bs = sandbox(
        &format!(r#"{{"ok":true,"data":{{"screenshot":"{payload}"}}}}"#),
        BrowserKind::Chromium,
    );
    let handler = BrowserToolHandler::new(bs);
    let res = handler
        .run(&call("browser_screenshot", json!({})), &ctx(), "s")
        .await;
    assert!(!res.is_error, "{}", res.content);
    assert!(res.content.contains("(80 base64 chars)"), "{}", res.content);
    assert!(res.content.contains(&payload[..64]), "{}", res.content);
}

#[tokio::test]
async fn a_crashing_runner_surfaces_its_exit_status() {
    let bs = BrowserSandbox::with_catalog(
        Box::new(Shared(FakeSandbox::failing("node: not found\n"))),
        BrowserCatalog::builtin(),
        BrowserKind::Chromium,
    );
    let err = bs
        .navigate("https://x.test")
        .await
        .expect_err("runner failed");
    assert!(
        matches!(err, BrowserError::Exec(msg) if msg.contains("exited 2") && msg.contains("node: not found"))
    );
}

#[tokio::test]
async fn browser_sandbox_delegates_the_sandbox_trait() {
    let inner = FakeSandbox::new(r#"{"ok":true,"data":{}}"#);
    let bs = BrowserSandbox::with_catalog(
        Box::new(Shared(inner.clone())),
        BrowserCatalog::builtin(),
        BrowserKind::Chromium,
    );
    let spec = ExecSpec::command(vec!["node".into()]);
    let handle = bs.spawn(&spec).await.unwrap();
    assert_eq!(handle.id, "fake");
    let out = bs.exec(&handle, &["node".into()]).await.unwrap();
    assert_eq!(out.exit_code, 0);
    bs.destroy(handle).await.unwrap();
    assert_eq!(inner.images().len(), 1);
    // `Default` builds the same lazily-connected sandbox as `new`.
    let _ = BrowserSandbox::default();
}

#[test]
fn tool_schemas_are_exposed() {
    let tools = browser_tools();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"browser_navigate"));
    assert!(names.contains(&"browser_screenshot"));
    for tool in &tools {
        assert!(tool.parameters.is_object(), "{}", tool.name);
    }
}
