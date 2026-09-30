//! `BrowserSandbox` — a sandbox backend that runs a browser inside a container
//! (reusing the existing Docker/Kata sandbox path for container lifecycle) and
//! forwards high‑level browser operations to a `runner.mjs` baked into the
//! image.
//!
//! The agent's *shell* fallback continues to use the regular Docker sandbox; the
//! `BrowserSandbox` is injected as a [`BrowserEngine`] behind a
//! [`ToolHandler`](agent_core::ToolHandler) (see `tools.rs`). It nonetheless
//! implements the `Sandbox` trait (delegating to its inner sandbox) so it can be
//! used as a drop‑in sandbox if ever needed.

use agent_sandbox::{DockerSandbox, ExecOutput, ExecSpec, Sandbox, SandboxError, SandboxHandle};
use async_trait::async_trait;
use std::sync::RwLock;

use crate::engine::*;

/// Path of the runner inside every browser image.
pub const RUNNER_PATH: &str = "/runner/runner.mjs";

/// Keep the browser container alive between operations.
const KEEPALIVE_CMD: &[&str] = &["sleep", "infinity"];

/// Manages a single browser container and translates browser operations into
/// `runner.mjs` invocations.
pub struct BrowserSandbox {
    inner: Box<dyn Sandbox>,
    catalog: BrowserCatalog,
    kind: RwLock<BrowserKind>,
    override_image: RwLock<Option<String>>,
    /// The live browser container handle (lazy; `None` until first op).
    handle: tokio::sync::Mutex<Option<SandboxHandle>>,
}

impl BrowserSandbox {
    /// Build a browser sandbox using the default engine from
    /// `ARIACOMPUTE_BROWSER_ENGINE` (Chromium unless overridden).
    pub fn new() -> Self {
        Self::with_catalog(
            Box::new(DockerSandbox::new()),
            BrowserCatalog::builtin(),
            BrowserCatalog::default_kind(),
        )
    }

    /// Build with an explicit catalog and engine (used by the cloud/FFI runtimes).
    pub fn with_catalog(
        inner: Box<dyn Sandbox>,
        catalog: BrowserCatalog,
        kind: BrowserKind,
    ) -> Self {
        Self {
            inner,
            catalog,
            kind: RwLock::new(kind),
            override_image: RwLock::new(None),
            handle: tokio::sync::Mutex::new(None),
        }
    }

    /// The container image for the active engine (override aware).
    fn image(&self) -> String {
        if let Some(img) = self.override_image.read().unwrap().clone() {
            return img;
        }
        self.catalog.image_for(self.kind())
    }

    /// Spawn the browser container if not already running; returns a clone of the
    /// handle. Never panics — a missing daemon surfaces as [`BrowserError::Spawn`].
    async fn ensure(&self) -> Result<SandboxHandle, BrowserError> {
        let mut guard = self.handle.lock().await;
        if let Some(h) = guard.clone() {
            return Ok(h);
        }
        let handle = self.spawn_browser().await?;
        *guard = Some(handle.clone());
        Ok(handle)
    }

    /// Spawn a fresh browser container from the active image.
    async fn spawn_browser(&self) -> Result<SandboxHandle, BrowserError> {
        let spec = ExecSpec {
            image: Some(self.image()),
            command: KEEPALIVE_CMD.iter().map(|s| s.to_string()).collect(),
            workdir: None,
            env: Vec::new(),
            timeout_ms: 60_000,
        };
        self.inner
            .spawn(&spec)
            .await
            .map_err(|e| BrowserError::Spawn(e.to_string()))
    }

    /// Tear down the active browser container (if any).
    async fn teardown(&self) {
        let mut guard = self.handle.lock().await;
        if let Some(h) = guard.take() {
            let _ = self.inner.destroy(h).await;
        }
    }

    /// Run one browser operation against the live container.
    async fn op(&self, op: BrowserOp) -> Result<BrowserResult, BrowserError> {
        let handle = self.ensure().await?;
        let req = BrowserRequest {
            engine: self.kind(),
            op,
        };
        let json = serde_json::to_string(&req)?;
        let cmd = vec!["node".to_string(), RUNNER_PATH.to_string(), json];
        let out: ExecOutput = self
            .inner
            .exec(&handle, &cmd)
            .await
            .map_err(|e| BrowserError::Exec(e.to_string()))?;
        if out.exit_code != 0 {
            return Err(BrowserError::Exec(format!(
                "runner exited {}: {}",
                out.exit_code,
                out.stderr.trim()
            )));
        }
        let res: BrowserResponse = serde_json::from_str(&out.stdout).map_err(BrowserError::Json)?;
        if !res.ok {
            let reason = res.error.unwrap_or_else(|| "unknown browser error".into());
            if reason.to_ascii_lowercase().contains("unsupported") {
                return Err(BrowserError::Unsupported(
                    self.kind().as_str().into(),
                    reason,
                ));
            }
            return Err(BrowserError::Engine(reason));
        }
        Ok(BrowserResult {
            ok: true,
            data: res.data,
        })
    }
}

impl Default for BrowserSandbox {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Sandbox for BrowserSandbox {
    async fn spawn(&self, spec: &ExecSpec) -> Result<SandboxHandle, SandboxError> {
        self.inner.spawn(spec).await
    }
    async fn exec(
        &self,
        handle: &SandboxHandle,
        cmd: &[String],
    ) -> Result<ExecOutput, SandboxError> {
        self.inner.exec(handle, cmd).await
    }
    async fn destroy(&self, handle: SandboxHandle) -> Result<(), SandboxError> {
        self.inner.destroy(handle).await
    }
}

#[async_trait]
impl BrowserEngine for BrowserSandbox {
    async fn navigate(&self, url: &str) -> Result<BrowserResult, BrowserError> {
        if url.trim().is_empty() {
            return Err(BrowserError::InvalidParam("url must not be empty".into()));
        }
        self.op(BrowserOp::Navigate {
            url: url.to_string(),
        })
        .await
    }

    async fn extract(&self) -> Result<BrowserResult, BrowserError> {
        self.op(BrowserOp::Extract {}).await
    }

    async fn click(&self, selector: &str) -> Result<BrowserResult, BrowserError> {
        if selector.trim().is_empty() {
            return Err(BrowserError::InvalidParam(
                "selector must not be empty".into(),
            ));
        }
        self.op(BrowserOp::Click {
            selector: selector.to_string(),
        })
        .await
    }

    async fn fill(&self, selector: &str, text: &str) -> Result<BrowserResult, BrowserError> {
        if selector.trim().is_empty() {
            return Err(BrowserError::InvalidParam(
                "selector must not be empty".into(),
            ));
        }
        self.op(BrowserOp::Fill {
            selector: selector.to_string(),
            text: text.to_string(),
        })
        .await
    }

    async fn screenshot(&self) -> Result<BrowserResult, BrowserError> {
        self.op(BrowserOp::Screenshot {}).await
    }

    async fn evaluate(&self, js: &str) -> Result<BrowserResult, BrowserError> {
        if js.trim().is_empty() {
            return Err(BrowserError::InvalidParam("js must not be empty".into()));
        }
        self.op(BrowserOp::Evaluate { js: js.to_string() }).await
    }

    async fn solve_captcha(&self) -> Result<BrowserResult, BrowserError> {
        self.op(BrowserOp::SolveCaptcha {}).await
    }

    async fn switch_to(
        &self,
        kind: BrowserKind,
        image: Option<String>,
    ) -> Result<(), BrowserError> {
        // Tear down the current browser container.
        self.teardown().await;
        let prev = self.kind();
        *self.kind.write().unwrap() = kind;
        *self.override_image.write().unwrap() = image;
        // Bring up the new container eagerly; roll back on failure.
        match self.spawn_browser().await {
            Ok(h) => {
                *self.handle.lock().await = Some(h);
                Ok(())
            }
            Err(e) => {
                *self.kind.write().unwrap() = prev;
                *self.override_image.write().unwrap() = None;
                Err(e)
            }
        }
    }

    fn kind(&self) -> BrowserKind {
        *self.kind.read().unwrap()
    }
}
