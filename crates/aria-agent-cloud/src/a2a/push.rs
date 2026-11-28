//! Outbound push notifications for A2A tasks.
//!
//! A caller registers a URL (`tasks/pushNotification/set` or the `configuration`
//! block of `message/send`) and gets a JSON body whenever a task reaches a
//! settled state. The rules are deliberately small:
//!
//! * `http` / `https` only, and no `user:pass@` in the authority — the URL is
//!   caller-supplied, so it must not become a place to smuggle credentials;
//! * redirects are **not** followed (a redirect would move the target after the
//!   check);
//! * 5 s connect/overall budget and exactly one retry;
//! * a failure is logged and recorded (`push_last`), never rolled back into the
//!   task state.

use std::time::Duration;

use aria_agent_a2a::types::PushNotificationConfig;
use async_trait::async_trait;
use serde_json::Value;

/// One delivery attempt plus one retry.
pub const MAX_ATTEMPTS: u32 = 2;

/// Connect and overall budget for a single attempt.
pub const PUSH_TIMEOUT: Duration = Duration::from_secs(5);

/// Delivery of one state change.
#[async_trait]
pub trait PushNotifier: Send + Sync {
    async fn notify(&self, config: &PushNotificationConfig, body: &Value) -> Result<(), String>;
}

/// Accepts everything, sends nothing (A2A disabled, or tests that do not care).
#[derive(Default)]
pub struct NoopPush;

#[async_trait]
impl PushNotifier for NoopPush {
    async fn notify(&self, _config: &PushNotificationConfig, _body: &Value) -> Result<(), String> {
        Ok(())
    }
}

/// HTTP(S) push over a client that refuses redirects.
pub struct HttpPushNotifier {
    client: reqwest::Client,
}

impl HttpPushNotifier {
    pub fn new() -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(PUSH_TIMEOUT)
            .timeout(PUSH_TIMEOUT)
            .build()
            // Only an unusable TLS backend gets here, and there is no hermetic
            // way to provoke it. Booting without a client would be worse: the
            // caller must hear that its push target cannot be served.
            .map_err(|e| format!("a2a push client: {e}"))?; // LCOV_EXCL_LINE
        Ok(Self { client })
    }
}

/// Reject anything that is not a plain `http(s)` URL without credentials.
pub fn validate_push_url(raw: &str) -> Result<(), String> {
    let trimmed = raw.trim();
    let lower = trimmed.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err("push url must be http(s)".to_string());
    }
    let authority = trimmed
        .split_once("://")
        .map(|(_, rest)| rest.split('/').next().unwrap_or(""))
        .unwrap_or("");
    if authority.is_empty() {
        return Err("push url has no host".to_string());
    }
    if authority.contains('@') {
        return Err("push url must not carry credentials".to_string());
    }
    Ok(())
}

#[async_trait]
impl PushNotifier for HttpPushNotifier {
    async fn notify(&self, config: &PushNotificationConfig, body: &Value) -> Result<(), String> {
        validate_push_url(&config.url)?;
        let mut attempt = 0;
        let mut last = String::new();
        while attempt < MAX_ATTEMPTS {
            attempt += 1;
            let mut request = self.client.post(&config.url).json(body);
            if let Some(token) = &config.token {
                request = request.bearer_auth(token);
            }
            match request.send().await {
                Ok(response) if response.status().is_success() => return Ok(()),
                Ok(response) => last = format!("push endpoint answered {}", response.status()),
                Err(e) => last = format!("push failed: {e}"),
            }
        }
        Err(last)
    }
}
