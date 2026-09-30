//! `agent-browser` — browser capability for the agent platform.
//!
//! Provides:
//! * [`BrowserKind`] / [`BrowserCatalog`] — the six browser engines and their
//!   container images (servo, obscura, chromium, gosub, camoufox, lightpanda),
//!   with env‑override and graceful degradation for native engines.
//! * [`BrowserEngine`] — the stable operation contract (navigate / extract /
//!   click / fill / screenshot / evaluate / solve_captcha / switch_to).
//! * [`BrowserSandbox`] — a sandbox backend that runs a browser inside a
//!   container and forwards operations to `runner.mjs`.
//! * [`browser_tools`] / [`BrowserToolHandler`] — the tool family and the
//!   [`ToolHandler`](agent_core::ToolHandler) that routes `browser_*` calls.
//!
//! The agent runtime selects the browser sandbox via `sandbox_provider =
//! "browser"`; the cloud/FFI runtimes inject a `BrowserSandbox` as the
//! `ToolHandler` (see `docs/adr/0012-browser-sandbox.md`).

mod engine;
mod sandbox;
mod tools;

pub use engine::{
    BrowserCatalog, BrowserEngine, BrowserError, BrowserKind, BrowserOp, BrowserRequest,
    BrowserResponse, BrowserResult, BrowserSpec,
};
pub use sandbox::BrowserSandbox;
pub use tools::{browser_tools, BrowserToolHandler};

/// Convenience: build a [`BrowserSandbox`] with the default engine from the
/// `ARIACOMPUTE_BROWSER_ENGINE` env var (Chromium unless overridden).
pub fn default_browser() -> BrowserSandbox {
    BrowserSandbox::new()
}
