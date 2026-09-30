//! The browser tool family exposed to the agentic loop, and the
//! [`BrowserToolHandler`] that routes `browser_*` tool calls to a
//! [`BrowserSandbox`].

use agent_core::context::{ContextFragment, ContextStore, FragmentKind};
use agent_core::{Tool, ToolCall, ToolHandler, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::engine::*;
use crate::sandbox::BrowserSandbox;

/// Build the eight browser tools the model can call. Mirrors the frozen tool
/// contract (`Tool { name, description, parameters }`) used elsewhere for the
/// `shell` tool. These are appended to (not replacing) the shell tool.
pub fn browser_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "browser_navigate".into(),
            description: "Navigate the embedded browser to a URL.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "Absolute URL to open" }
                },
                "required": ["url"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "browser_extract".into(),
            description: "Extract the current page's text / HTML / accessibility tree.".into(),
            parameters: json!({ "type": "object", "properties": {}, "required": [] }),
        },
        Tool {
            name: "browser_click".into(),
            description: "Click an element matching a CSS selector.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "selector": { "type": "string", "description": "CSS selector to click" }
                },
                "required": ["selector"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "browser_fill".into(),
            description: "Fill an input element (CSS selector) with text.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "selector": { "type": "string", "description": "CSS selector of the input" },
                    "text": { "type": "string", "description": "Text to enter" }
                },
                "required": ["selector", "text"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "browser_screenshot".into(),
            description: "Capture a screenshot (returned as base64 PNG).".into(),
            parameters: json!({ "type": "object", "properties": {}, "required": [] }),
        },
        Tool {
            name: "browser_evaluate".into(),
            description: "Run a JavaScript expression in the page and return its value.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "js": { "type": "string", "description": "JavaScript expression" }
                },
                "required": ["js"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "browser_solve_captcha".into(),
            description: "Attempt to solve a CAPTCHA / anti-bot challenge on the current page.".into(),
            parameters: json!({ "type": "object", "properties": {}, "required": [] }),
        },
        Tool {
            name: "browser_use".into(),
            description:
                "Switch the active browser engine at runtime (servo|obscura|chromium|gosub|camoufox|lightpanda). \
                 Optionally override the container image."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "engine": { "type": "string", "description": "Target engine name" },
                    "image": { "type": "string", "description": "Optional explicit container image" }
                },
                "required": [],
                "additionalProperties": false
            }),
        },
    ]
}

/// Routes `browser_*` tool calls to the injected [`BrowserSandbox`].
pub struct BrowserToolHandler {
    browser: Arc<BrowserSandbox>,
}

impl BrowserToolHandler {
    pub fn new(browser: Arc<BrowserSandbox>) -> Self {
        Self { browser }
    }

    /// Execute a browser tool, returning a plain string (errors prefixed with
    /// `ERROR:` for the caller to detect).
    async fn exec(&self, call: &ToolCall) -> Result<String, BrowserError> {
        let a: &Value = &call.arguments;
        let arg_str = |k: &str| a.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
        match call.name.as_str() {
            "browser_navigate" => {
                let url = arg_str("url")
                    .ok_or_else(|| BrowserError::InvalidParam("url required".into()))?;
                let r = self.browser.navigate(&url).await?;
                Ok(format!("navigated to {url}: {}", r.summary()))
            }
            "browser_extract" => {
                let r = self.browser.extract().await?;
                Ok(format!("extracted: {}", r.summary()))
            }
            "browser_click" => {
                let sel = arg_str("selector")
                    .ok_or_else(|| BrowserError::InvalidParam("selector required".into()))?;
                let r = self.browser.click(&sel).await?;
                Ok(format!("clicked {sel}: {}", r.summary()))
            }
            "browser_fill" => {
                let sel = arg_str("selector")
                    .ok_or_else(|| BrowserError::InvalidParam("selector required".into()))?;
                let text = arg_str("text")
                    .ok_or_else(|| BrowserError::InvalidParam("text required".into()))?;
                let r = self.browser.fill(&sel, &text).await?;
                Ok(format!("filled {sel}: {}", r.summary()))
            }
            "browser_screenshot" => {
                let r = self.browser.screenshot().await?;
                let b64 = r
                    .data()
                    .get("screenshot")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                Ok(format!(
                    "screenshot captured ({} base64 chars){}",
                    b64.len(),
                    if b64.len() > 64 {
                        format!(": {}", &b64[..64])
                    } else {
                        String::new()
                    }
                ))
            }
            "browser_evaluate" => {
                let js = arg_str("js")
                    .ok_or_else(|| BrowserError::InvalidParam("js required".into()))?;
                let r = self.browser.evaluate(&js).await?;
                Ok(format!("evaluate result: {}", r.summary()))
            }
            "browser_solve_captcha" => {
                let r = self.browser.solve_captcha().await?;
                Ok(format!("captcha: {}", r.summary()))
            }
            "browser_use" => {
                let engine = arg_str("engine");
                let image = arg_str("image");
                let kind = match engine {
                    Some(e) => BrowserKind::parse(&e).ok_or_else(|| {
                        BrowserError::InvalidParam(format!("unknown engine: {e}"))
                    })?,
                    None => self.browser.kind(),
                };
                self.browser.switch_to(kind, image).await?;
                Ok(format!("switched browser engine to {}", kind.as_str()))
            }
            other => Err(BrowserError::InvalidParam(format!(
                "unknown browser tool: {other}"
            ))),
        }
    }
}

#[async_trait]
impl ToolHandler for BrowserToolHandler {
    fn handles(&self, name: &str) -> bool {
        name == "browser_use" || name.starts_with("browser_")
    }

    async fn run(&self, call: &ToolCall, ctx: &Arc<dyn ContextStore>, session: &str) -> ToolResult {
        let (content, is_error) = match self.exec(call).await {
            Ok(s) => (s, false),
            Err(e) => (format!("ERROR: {e}"), true),
        };
        // Persist the tool result to memo so later turns can recall it.
        let _ = ctx
            .memorize(ContextFragment::new(
                session,
                FragmentKind::ToolResult,
                content.clone(),
            ))
            .await;
        ToolResult {
            call_id: call.id.clone(),
            content,
            is_error,
        }
    }
}
