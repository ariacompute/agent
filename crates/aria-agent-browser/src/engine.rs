//! `BrowserEngine` abstraction, the op/result wire protocol, and the browser
//! image catalog.
//!
//! The agent supports **six** browser engines, each shipped as its own sandbox
//! container image: `servo`, `obscura`, `chromium` (Playwright + playwright‑captcha),
//! `gosub`, `camoufox`, `lightpanda`. A single `runner.mjs` (baked into every
//! image) reads a JSON op on its command line and emits a JSON result, so the
//! Rust side only has to spawn a container and forward ops — no engine‑specific
//! code path is needed here. Native engines (Servo/Gosub) return
//! `Unsupported` for operations they cannot perform instead of panicking.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;

/// The set of browser engines the platform can drive.
///
/// Each maps to a container image (see [`BrowserCatalog`]). The string form is
/// stable and is what the model passes to the `browser_use` tool / the
/// `ARIACOMPUTE_BROWSER_ENGINE` env var.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserKind {
    /// Rust browser engine (research/WIP). Navigate + extract only.
    Servo,
    /// Anti‑bot / stealth layer (Chromium‑based fingerprint evasion + captcha).
    Obscura,
    /// Chromium via Playwright, with the playwright‑captcha integration. Default.
    Chromium,
    /// Rust browser engine (WIP). Navigate + extract only.
    Gosub,
    /// Firefox‑based anti‑detect browser (Playwright compatible).
    Camoufox,
    /// Lightweight headless browser purpose‑built for AI agents.
    Lightpanda,
}

impl BrowserKind {
    /// Parse the stable string form (case‑insensitive).
    pub fn parse(s: &str) -> Option<BrowserKind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "servo" => Some(BrowserKind::Servo),
            "obscura" => Some(BrowserKind::Obscura),
            "chromium" => Some(BrowserKind::Chromium),
            "gosub" => Some(BrowserKind::Gosub),
            "camoufox" => Some(BrowserKind::Camoufox),
            "lightpanda" => Some(BrowserKind::Lightpanda),
            _ => None,
        }
    }

    /// The stable string form.
    pub fn as_str(&self) -> &'static str {
        match self {
            BrowserKind::Servo => "servo",
            BrowserKind::Obscura => "obscura",
            BrowserKind::Chromium => "chromium",
            BrowserKind::Gosub => "gosub",
            BrowserKind::Camoufox => "camoufox",
            BrowserKind::Lightpanda => "lightpanda",
        }
    }

    /// Whether this engine is driven through the Playwright protocol (Chromium,
    /// Camoufox, Lightpanda, Obscura). Native engines (Servo, Gosub) are not.
    pub fn is_playwright(&self) -> bool {
        matches!(
            self,
            BrowserKind::Chromium
                | BrowserKind::Camoufox
                | BrowserKind::Lightpanda
                | BrowserKind::Obscura
        )
    }
}

/// Errors surfaced by the browser engine / sandbox.
#[derive(Debug, Error)]
pub enum BrowserError {
    #[error("operation unsupported by engine {0}: {1}")]
    Unsupported(String, String),
    #[error("invalid parameter: {0}")]
    InvalidParam(String),
    #[error("browser spawn failed: {0}")]
    Spawn(String),
    #[error("browser exec failed: {0}")]
    Exec(String),
    #[error("engine error: {0}")]
    Engine(String),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// A structured result returned by a browser operation. `data` carries the
/// engine‑specific payload (title, text, html, screenshot base64, evaluated
/// value, …). Callers format it into the tool result string.
#[derive(Debug, Clone)]
pub struct BrowserResult {
    pub ok: bool,
    pub data: serde_json::Value,
}

impl BrowserResult {
    pub fn data(&self) -> &serde_json::Value {
        &self.data
    }

    /// A compact, single‑line summary for tool output (no page content is logged).
    pub fn summary(&self) -> String {
        if let Some(title) = self.data.get("title").and_then(|v| v.as_str()) {
            return format!("title={title}");
        }
        if let Some(text) = self.data.get("text").and_then(|v| v.as_str()) {
            let t = text.trim();
            let snippet: String = t.chars().take(200).collect();
            return format!("text={snippet}");
        }
        if self
            .data
            .get("screenshot")
            .and_then(|v| v.as_str())
            .is_some()
        {
            return "screenshot=captured".to_string();
        }
        serde_json::to_string(&self.data).unwrap_or_else(|_| "{}".to_string())
    }
}

/// A single browser operation. Serialized as the `op` field of [`BrowserRequest`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum BrowserOp {
    Navigate { url: String },
    Extract {},
    Click { selector: String },
    Fill { selector: String, text: String },
    Screenshot {},
    Evaluate { js: String },
    SolveCaptcha {},
}

/// The request envelope the runner receives as its first CLI argument.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserRequest {
    pub engine: BrowserKind,
    #[serde(flatten)]
    pub op: BrowserOp,
}

/// The response envelope the runner writes to stdout as a single JSON line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserResponse {
    pub ok: bool,
    #[serde(default)]
    pub data: serde_json::Value,
    #[serde(default)]
    pub error: Option<String>,
}

/// Capability/metadata descriptor for one browser engine.
#[derive(Debug, Clone)]
pub struct BrowserSpec {
    pub kind: BrowserKind,
    /// Container image used to spawn the browser sandbox.
    pub image: String,
    /// Drives the runner via the Playwright protocol.
    pub playwright_based: bool,
    /// Enables fingerprint / anti‑bot stealth.
    pub stealth: bool,
    /// Enables captcha solving (playwright‑captcha / obscura).
    pub captcha: bool,
}

/// The catalog of supported browser engines and their container images.
///
/// Default images are overridable per‑kind via
/// `ARIACOMPUTE_BROWSER_IMAGE_<KIND>` (e.g. `ARIACOMPUTE_BROWSER_IMAGE_CHROMIUM`).
/// Parsing failures fall back to the conservative built‑in image so construction
/// never panics.
#[derive(Debug, Clone)]
pub struct BrowserCatalog {
    specs: HashMap<BrowserKind, BrowserSpec>,
}

impl Default for BrowserCatalog {
    fn default() -> Self {
        Self::builtin()
    }
}

impl BrowserCatalog {
    /// The built‑in catalog with conservative default images.
    pub fn builtin() -> Self {
        use BrowserKind::*;
        let mut specs: HashMap<BrowserKind, BrowserSpec> = HashMap::new();
        specs.insert(
            Chromium,
            BrowserSpec {
                kind: Chromium,
                image: "ghcr.io/ariacompute/browser-playwright:chromium".into(),
                playwright_based: true,
                stealth: false,
                captcha: true,
            },
        );
        specs.insert(
            Camoufox,
            BrowserSpec {
                kind: Camoufox,
                image: "ghcr.io/ariacompute/browser-playwright:camoufox".into(),
                playwright_based: true,
                stealth: true,
                captcha: true,
            },
        );
        specs.insert(
            Lightpanda,
            BrowserSpec {
                kind: Lightpanda,
                image: "ghcr.io/ariacompute/browser-playwright:lightpanda".into(),
                playwright_based: true,
                stealth: false,
                captcha: false,
            },
        );
        specs.insert(
            Obscura,
            BrowserSpec {
                kind: Obscura,
                image: "ghcr.io/ariacompute/browser-obscura:latest".into(),
                playwright_based: true,
                stealth: true,
                captcha: true,
            },
        );
        specs.insert(
            Servo,
            BrowserSpec {
                kind: Servo,
                image: "ghcr.io/ariacompute/browser-servo:latest".into(),
                playwright_based: false,
                stealth: false,
                captcha: false,
            },
        );
        specs.insert(
            Gosub,
            BrowserSpec {
                kind: Gosub,
                image: "ghcr.io/ariacompute/browser-gosub:latest".into(),
                playwright_based: false,
                stealth: false,
                captcha: false,
            },
        );
        Self { specs }
    }

    /// Look up the spec for an engine.
    pub fn spec(&self, kind: BrowserKind) -> &BrowserSpec {
        // `builtin` always inserts every variant, so this is infallible.
        self.specs.get(&kind).expect("catalog missing kind")
    }

    /// Resolve the container image for `kind`, applying the
    /// `ARIACOMPUTE_BROWSER_IMAGE_<KIND>` override when set and non‑empty.
    /// Falls back to the built‑in image on any parse/empty issue.
    pub fn image_for(&self, kind: BrowserKind) -> String {
        let env_key = format!(
            "ARIACOMPUTE_BROWSER_IMAGE_{}",
            kind.as_str().to_ascii_uppercase()
        );
        if let Ok(v) = std::env::var(&env_key) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return v;
            }
        }
        self.spec(kind).image.clone()
    }

    /// The default engine, read from `ARIACOMPUTE_BROWSER_ENGINE` (case‑
    /// insensitive); falls back to Chromium when unset or unrecognized.
    pub fn default_kind() -> BrowserKind {
        match std::env::var("ARIACOMPUTE_BROWSER_ENGINE") {
            Ok(v) if !v.trim().is_empty() => {
                BrowserKind::parse(&v).unwrap_or(BrowserKind::Chromium)
            }
            _ => BrowserKind::Chromium,
        }
    }
}

/// The stable engine contract used by the tool handler. Every browser method
/// returns a [`BrowserResult`] or a [`BrowserError`]; native engines return
/// [`BrowserError::Unsupported`] for unimplemented operations rather than
/// panicking.
#[async_trait::async_trait]
pub trait BrowserEngine: Send + Sync {
    async fn navigate(&self, url: &str) -> Result<BrowserResult, BrowserError>;
    async fn extract(&self) -> Result<BrowserResult, BrowserError>;
    async fn click(&self, selector: &str) -> Result<BrowserResult, BrowserError>;
    async fn fill(&self, selector: &str, text: &str) -> Result<BrowserResult, BrowserError>;
    async fn screenshot(&self) -> Result<BrowserResult, BrowserError>;
    async fn evaluate(&self, js: &str) -> Result<BrowserResult, BrowserError>;
    async fn solve_captcha(&self) -> Result<BrowserResult, BrowserError>;
    /// Switch the active engine (destroys the current browser container and
    /// spawns the requested one). An optional explicit `image` overrides the
    /// catalog/default for this switch.
    async fn switch_to(&self, kind: BrowserKind, image: Option<String>)
        -> Result<(), BrowserError>;
    /// The currently active engine.
    fn kind(&self) -> BrowserKind;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_roundtrip_parse_and_str() {
        for k in [
            BrowserKind::Servo,
            BrowserKind::Obscura,
            BrowserKind::Chromium,
            BrowserKind::Gosub,
            BrowserKind::Camoufox,
            BrowserKind::Lightpanda,
        ] {
            assert_eq!(BrowserKind::parse(k.as_str()), Some(k));
            assert_eq!(
                BrowserKind::parse(&k.as_str().to_ascii_uppercase()),
                Some(k)
            );
        }
        assert_eq!(BrowserKind::parse("nope"), None);
        assert_eq!(BrowserKind::parse(""), None);
    }

    #[test]
    fn is_playwright_flags() {
        assert!(BrowserKind::Chromium.is_playwright());
        assert!(BrowserKind::Camoufox.is_playwright());
        assert!(BrowserKind::Lightpanda.is_playwright());
        assert!(BrowserKind::Obscura.is_playwright());
        assert!(!BrowserKind::Servo.is_playwright());
        assert!(!BrowserKind::Gosub.is_playwright());
    }

    #[test]
    fn catalog_contains_all_kinds() {
        let cat = BrowserCatalog::builtin();
        for k in [
            BrowserKind::Servo,
            BrowserKind::Obscura,
            BrowserKind::Chromium,
            BrowserKind::Gosub,
            BrowserKind::Camoufox,
            BrowserKind::Lightpanda,
        ] {
            assert!(!cat.image_for(k).is_empty(), "image for {k:?} must exist");
        }
    }

    #[test]
    fn catalog_image_env_override() {
        std::env::set_var("ARIACOMPUTE_BROWSER_IMAGE_CHROMIUM", "my/chromium:dev");
        let cat = BrowserCatalog::builtin();
        assert_eq!(
            cat.image_for(BrowserKind::Chromium),
            "my/chromium:dev".to_string()
        );
        std::env::remove_var("ARIACOMPUTE_BROWSER_IMAGE_CHROMIUM");
    }

    #[test]
    fn catalog_default_kind_env() {
        std::env::set_var("ARIACOMPUTE_BROWSER_ENGINE", "camoufox");
        assert_eq!(BrowserCatalog::default_kind(), BrowserKind::Camoufox);
        std::env::set_var("ARIACOMPUTE_BROWSER_ENGINE", "bogus");
        assert_eq!(BrowserCatalog::default_kind(), BrowserKind::Chromium);
        std::env::remove_var("ARIACOMPUTE_BROWSER_ENGINE");
    }

    #[test]
    fn op_serde_roundtrip() {
        let req = BrowserRequest {
            engine: BrowserKind::Chromium,
            op: BrowserOp::Navigate {
                url: "https://example.com".into(),
            },
        };
        let j = serde_json::to_string(&req).unwrap();
        assert!(j.contains("\"engine\":\"chromium\""));
        assert!(j.contains("\"op\":\"navigate\""));
        let back: BrowserRequest = serde_json::from_str(&j).unwrap();
        assert_eq!(back.engine, BrowserKind::Chromium);
        assert_eq!(
            back.op,
            BrowserOp::Navigate {
                url: "https://example.com".into()
            }
        );
    }
}
