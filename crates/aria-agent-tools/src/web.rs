//! Web tool family (`web_*`).
//!
//! The seam is fail-closed (`NoWeb`), and every provider must run the SSRF gate
//! ([`agent_core::seams::is_public_url`]) before touching the network.
//!
//! The default build registers no provider (the family is not offered at all).
//! With `--features web` the [`HttpWeb`] provider is available: it fetches text
//! bodies through `reqwest` and answers `web_search` only when a search endpoint
//! is configured through `ARIA_WEB_SEARCH_URL` (a URL template containing
//! `{query}` / `{limit}` that returns a JSON array of `{title,url,snippet}`).

use crate::{arg_str, arg_usize, family_handler, Family};
use agent_core::seams::{EgressDecision, EgressSeam, SeamError, WebSeam};
use agent_core::{Tool, ToolCall};
use serde_json::json;
use std::sync::Arc;

#[cfg(feature = "web")]
pub use http_web::HttpWeb;

pub struct WebToolHandler {
    web: Arc<dyn WebSeam>,
    /// When present, every fetch is decided **before** any request is made.
    egress: Option<Arc<dyn EgressSeam>>,
}

impl WebToolHandler {
    pub fn new(web: Arc<dyn WebSeam>) -> Self {
        Self { web, egress: None }
    }

    /// Gate fetches through the egress seam (deny-by-default policies refuse
    /// everything that was not named).
    pub fn with_egress(mut self, egress: Arc<dyn EgressSeam>) -> Self {
        self.egress = Some(egress);
        self
    }

    /// Decide one URL. A denial is fed back to the model with the policy revision
    /// so the fix ("add the host to the allowlist") is obvious.
    async fn gate(&self, url: &str) -> Result<(), SeamError> {
        let Some(egress) = &self.egress else {
            return Ok(());
        };
        let host =
            host_of(url).ok_or_else(|| SeamError::Invalid(format!("no host in URL: {url}")))?;
        let policy = egress.policy().await?;
        match policy.decide(&host) {
            EgressDecision::Allow => Ok(()),
            EgressDecision::Deny { reason } => Err(SeamError::Denied(reason)),
        }
    }
}

/// Extract the host from an `http(s)` URL without pulling in a URL crate.
pub fn host_of(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.is_empty() {
        return None;
    }
    // Drop userinfo, then port; keep IPv6 literals as-is.
    let authority = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    let host = if let Some(stripped) = authority.strip_prefix('[') {
        stripped.split_once(']')?.0
    } else {
        authority.split(':').next()?
    };
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

pub fn web_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "web_search".into(),
            description: "Search the public web and return ranked results.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 20 }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "web_fetch".into(),
            description: "Fetch a public http(s) URL and return its text body.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "url": { "type": "string" } },
                "required": ["url"],
                "additionalProperties": false
            }),
        },
    ]
}

impl Family for WebToolHandler {
    fn prefix(&self) -> &'static str {
        "web_"
    }

    async fn dispatch(&self, call: &ToolCall, _session: &str) -> Result<String, SeamError> {
        match call.name.as_str() {
            "web_search" => {
                let query = arg_str(call, "query")?;
                let limit = arg_usize(call, "limit").unwrap_or(5).clamp(1, 20);
                let hits = self.web.search(&query, limit).await?;
                if hits.is_empty() {
                    return Ok("(no results)".into());
                }
                Ok(hits
                    .into_iter()
                    .map(|h| format!("{} — {}\n{}", h.title, h.url, h.snippet))
                    .collect::<Vec<_>>()
                    .join("\n\n"))
            }
            "web_fetch" => {
                let url = arg_str(call, "url")?;
                if !agent_core::seams::is_public_url(&url) {
                    return Err(SeamError::Denied(format!(
                        "refusing to fetch a non-public URL: {url}"
                    )));
                }
                self.gate(&url).await?;
                self.web.fetch(&url).await
            }
            other => Err(SeamError::Invalid(format!("unknown web tool: {other}"))),
        }
    }
}

family_handler!(WebToolHandler);

/// `reqwest`-backed provider (`--features web`).
///
/// Deliberately narrow: it only speaks plain HTTP(S), refuses non-public targets,
/// caps redirects and body size, and returns `Unavailable` for `web_search` when
/// no search endpoint is configured — the deployment must opt in explicitly,
/// there is no "search the internet for me" default.
#[cfg(feature = "web")]
mod http_web {
    use super::*;
    use agent_core::seams::web::MAX_FETCH_BYTES;
    use agent_core::seams::{is_public_url, SeamResult, WebResult};
    use async_trait::async_trait;
    use std::time::Duration;

    /// Redirects are followed, but only this many times.
    pub const MAX_REDIRECTS: usize = 5;
    /// Wall-clock cap for a single request.
    pub const REQUEST_TIMEOUT_SECS: u64 = 20;

    /// Environment variable holding the search URL template.
    pub const SEARCH_ENDPOINT_ENV: &str = "ARIA_WEB_SEARCH_URL";

    /// Whether a `Content-Type` may be fed to the model as text.
    pub fn is_text_content_type(content_type: &str) -> bool {
        let base = content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        base.starts_with("text/")
            || matches!(
                base.as_str(),
                "application/json"
                    | "application/ld+json"
                    | "application/xml"
                    | "application/xhtml+xml"
            )
    }

    /// Expand the configured template, percent-encoding the query.
    ///
    /// Returns `None` when the template lacks a `{query}` placeholder; a missing
    /// `{limit}` is fine (the endpoint keeps its own default).
    pub fn search_url(endpoint: &str, query: &str, limit: usize) -> Option<String> {
        if !endpoint.contains("{query}") {
            return None;
        }
        let encoded = percent_encode(query);
        Some(
            endpoint
                .replace("{query}", &encoded)
                .replace("{limit}", &limit.to_string()),
        )
    }

    /// Minimal RFC 3986 unreserved-set encoding (no extra dependency).
    fn percent_encode(raw: &str) -> String {
        let mut out = String::with_capacity(raw.len());
        for byte in raw.as_bytes() {
            let c = *byte as char;
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
                out.push(c);
            } else {
                out.push_str(&format!("%{byte:02X}"));
            }
        }
        out
    }

    /// HTTP provider used when the `web` feature is enabled.
    pub struct HttpWeb {
        client: reqwest::Client,
        search_endpoint: Option<String>,
    }

    impl HttpWeb {
        /// Build from the environment ([`SEARCH_ENDPOINT_ENV`]).
        pub fn from_env() -> Result<Self, SeamError> {
            Self::new(std::env::var(SEARCH_ENDPOINT_ENV).ok())
        }

        pub fn new(search_endpoint: Option<String>) -> Result<Self, SeamError> {
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
                .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
                .user_agent(concat!("aria-agent/", env!("CARGO_PKG_VERSION")))
                .build()
                .map_err(|e| SeamError::Io(format!("http client: {e}")))?;
            let endpoint = search_endpoint
                .map(|e| e.trim().to_string())
                .filter(|e| !e.is_empty());
            Ok(Self {
                client,
                search_endpoint: endpoint,
            })
        }

        pub fn search_endpoint(&self) -> Option<&str> {
            self.search_endpoint.as_deref()
        }

        /// Read at most [`MAX_FETCH_BYTES`], aborting as soon as the cap is hit.
        async fn read_capped(&self, mut resp: reqwest::Response, url: &str) -> SeamResult<String> {
            let mut body: Vec<u8> = Vec::new();
            while let Some(chunk) = resp
                .chunk()
                .await
                .map_err(|e| SeamError::Io(format!("fetch {url}: {e}")))?
            {
                if body.len() + chunk.len() > MAX_FETCH_BYTES {
                    return Err(SeamError::Invalid(format!(
                        "response body exceeds {MAX_FETCH_BYTES} bytes: {url}"
                    )));
                }
                body.extend_from_slice(&chunk);
            }
            Ok(String::from_utf8_lossy(&body).to_string())
        }
    }

    #[async_trait]
    impl WebSeam for HttpWeb {
        async fn search(&self, query: &str, limit: usize) -> SeamResult<Vec<WebResult>> {
            let endpoint = self.search_endpoint.as_deref().ok_or_else(|| {
                SeamError::Unavailable(format!(
                    "no web search endpoint is configured (set {SEARCH_ENDPOINT_ENV})"
                ))
            })?;
            let url = search_url(endpoint, query, limit).ok_or_else(|| {
                SeamError::Invalid(format!(
                    "search endpoint must contain a `{{query}}` placeholder: {endpoint}"
                ))
            })?;
            if !is_public_url(&url) {
                return Err(SeamError::Denied(format!(
                    "refusing to call a non-public search endpoint: {url}"
                )));
            }
            let resp = self
                .client
                .get(&url)
                .send()
                .await
                .map_err(|e| SeamError::Io(format!("search {query}: {e}")))?;
            if !resp.status().is_success() {
                return Err(SeamError::Io(format!(
                    "search {query}: HTTP {}",
                    resp.status()
                )));
            }
            let body = self.read_capped(resp, &url).await?;
            let hits: Vec<WebResult> = serde_json::from_str(&body)
                .map_err(|e| SeamError::Invalid(format!("search response is not JSON: {e}")))?;
            Ok(hits.into_iter().take(limit.max(1)).collect())
        }

        async fn fetch(&self, url: &str) -> SeamResult<String> {
            if !is_public_url(url) {
                return Err(SeamError::Denied(format!(
                    "refusing to fetch a non-public URL: {url}"
                )));
            }
            let resp = self
                .client
                .get(url)
                .send()
                .await
                .map_err(|e| SeamError::Io(format!("fetch {url}: {e}")))?;
            if !resp.status().is_success() {
                return Err(SeamError::Io(format!(
                    "fetch {url}: HTTP {}",
                    resp.status()
                )));
            }
            let content_type = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            if !is_text_content_type(&content_type) {
                return Err(SeamError::Unsupported(format!(
                    "refusing non-text content-type `{content_type}` from {url}"
                )));
            }
            self.read_capped(resp, url).await
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn text_content_types_are_whitelisted() {
            for ok in [
                "text/html",
                "text/plain; charset=utf-8",
                "application/json",
                "application/xhtml+xml",
            ] {
                assert!(is_text_content_type(ok), "{ok}");
            }
            for bad in [
                "image/png",
                "application/pdf",
                "application/octet-stream",
                "",
            ] {
                assert!(!is_text_content_type(bad), "{bad}");
            }
        }

        #[test]
        fn search_template_expands_and_encodes() {
            let url = search_url(
                "https://search.example.com/q?q={query}&n={limit}",
                "rust async",
                3,
            )
            .expect("template with placeholder");
            assert_eq!(url, "https://search.example.com/q?q=rust%20async&n=3");
            assert!(search_url("https://search.example.com/q", "x", 3).is_none());
        }

        #[tokio::test]
        async fn search_without_endpoint_is_unavailable() {
            let web = HttpWeb::new(None).unwrap();
            assert!(web.search_endpoint().is_none());
            assert!(matches!(
                web.search("q", 3).await,
                Err(SeamError::Unavailable(_))
            ));
            let web = HttpWeb::new(Some("   ".into())).unwrap();
            assert!(web.search_endpoint().is_none(), "blank endpoint is unset");
        }

        #[tokio::test]
        async fn internal_targets_are_refused_before_any_request() {
            let web = HttpWeb::new(Some("https://127.0.0.1/search?q={query}".into())).unwrap();
            assert!(matches!(
                web.search("q", 1).await,
                Err(SeamError::Denied(_))
            ));
            assert!(matches!(
                web.fetch("http://169.254.169.254/latest").await,
                Err(SeamError::Denied(_))
            ));
        }

        #[tokio::test]
        async fn a_template_without_placeholder_is_invalid() {
            let web = HttpWeb::new(Some("https://search.example.com/q".into())).unwrap();
            assert!(matches!(
                web.search("q", 1).await,
                Err(SeamError::Invalid(_))
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::seams::{NoWeb, WebResult};
    use agent_core::ToolHandler;
    use async_trait::async_trait;

    struct Fixed;

    #[async_trait]
    impl WebSeam for Fixed {
        async fn search(&self, query: &str, limit: usize) -> Result<Vec<WebResult>, SeamError> {
            Ok(vec![WebResult {
                title: format!("hit for {query} ({limit})"),
                url: "https://example.com".into(),
                snippet: "snippet".into(),
            }])
        }

        async fn fetch(&self, url: &str) -> Result<String, SeamError> {
            Ok(format!("body of {url}"))
        }
    }

    async fn run(
        h: &WebToolHandler,
        name: &str,
        args: serde_json::Value,
    ) -> agent_core::ToolResult {
        h.run(
            &ToolCall {
                id: "c".into(),
                name: name.into(),
                arguments: args,
            },
            &agent_core::in_memory_context(),
            "s",
        )
        .await
    }

    struct Empty;

    #[async_trait]
    impl WebSeam for Empty {
        async fn search(&self, _query: &str, _limit: usize) -> Result<Vec<WebResult>, SeamError> {
            Ok(Vec::new())
        }
        async fn fetch(&self, _url: &str) -> Result<String, SeamError> {
            Ok(String::new())
        }
    }

    #[test]
    fn hosts_are_extracted_without_a_url_crate() {
        assert_eq!(
            host_of("https://api.example.com/v1?q=1").as_deref(),
            Some("api.example.com")
        );
        assert_eq!(
            host_of("http://user@A.Example.com:8080/x").as_deref(),
            Some("a.example.com")
        );
        assert_eq!(host_of("http://[::1]:80/x").as_deref(), Some("::1"));
        assert_eq!(host_of("nonsense"), None);
        assert_eq!(host_of("https://"), None);
    }

    struct FixedPolicy(agent_core::seams::EgressPolicy);

    #[async_trait]
    impl agent_core::seams::EgressSeam for FixedPolicy {
        async fn policy(&self) -> Result<agent_core::seams::EgressPolicy, SeamError> {
            Ok(self.0.clone())
        }
    }

    fn policy(
        mode: agent_core::seams::EgressMode,
        allow: &[&str],
    ) -> agent_core::seams::EgressPolicy {
        agent_core::seams::EgressPolicy::new(
            3,
            mode,
            allow
                .iter()
                .map(|h| agent_core::seams::HostPattern::parse(h).unwrap())
                .collect(),
            Vec::new(),
        )
    }

    #[tokio::test]
    async fn egress_denials_happen_before_the_provider_is_called() {
        let handler = WebToolHandler::new(Arc::new(Fixed)).with_egress(Arc::new(FixedPolicy(
            policy(agent_core::seams::EgressMode::DenyAll, &[]),
        )));
        let denied = run(
            &handler,
            "web_fetch",
            json!({"url": "https://example.com/x"}),
        )
        .await;
        assert!(denied.is_error);
        assert!(denied.content.contains("denied"), "{}", denied.content);
        assert!(
            denied.content.contains("deny-by-default"),
            "{}",
            denied.content
        );

        let allowed = WebToolHandler::new(Arc::new(Fixed)).with_egress(Arc::new(FixedPolicy(
            policy(agent_core::seams::EgressMode::Allowlist, &["example.com"]),
        )));
        let ok = run(
            &allowed,
            "web_fetch",
            json!({"url": "https://example.com/x"}),
        )
        .await;
        assert!(!ok.is_error, "{}", ok.content);
        assert!(ok.content.contains("body of"));

        // A host outside the allowlist is refused.
        let other = run(&allowed, "web_fetch", json!({"url": "https://other.com/x"})).await;
        assert!(other.is_error);
        assert!(
            other.content.contains("not on the egress allowlist"),
            "{}",
            other.content
        );
    }

    #[test]
    fn prefix_matches_the_web_family() {
        let h = WebToolHandler::new(Arc::new(Fixed));
        assert!(h.handles("web_search"));
        assert!(!h.handles("other"));
    }

    #[tokio::test]
    async fn no_provider_is_fail_closed() {
        let h = WebToolHandler::new(Arc::new(NoWeb));
        let res = run(&h, "web_search", json!({"query": "rust"})).await;
        assert!(res.is_error);
        assert!(res.content.contains("unavailable"), "{}", res.content);
    }

    #[tokio::test]
    async fn empty_search_results_are_reported_plainly() {
        let h = WebToolHandler::new(Arc::new(Empty));
        let res = run(&h, "web_search", json!({"query": "rust"})).await;
        assert!(!res.is_error);
        assert_eq!(res.content, "(no results)");
        assert_eq!(Empty.fetch("https://example.test").await.unwrap(), "");
    }

    #[tokio::test]
    async fn search_renders_and_fetch_guards_ssrf() {
        let h = WebToolHandler::new(Arc::new(Fixed));
        let listed = run(&h, "web_search", json!({"query": "rust", "limit": 3})).await;
        assert!(
            listed.content.contains("hit for rust (3)"),
            "{}",
            listed.content
        );
        assert!(listed.content.contains("https://example.com"));
        let fetched = run(&h, "web_fetch", json!({"url": "https://example.com/x"})).await;
        assert!(fetched.content.contains("body of"));
        for bad in ["http://169.254.169.254/latest", "file:///etc/passwd"] {
            let denied = run(&h, "web_fetch", json!({"url": bad})).await;
            assert!(denied.is_error, "{bad}");
            assert!(denied.content.contains("denied"), "{}", denied.content);
        }
        assert!(run(&h, "web_fetch", json!({})).await.is_error);
        assert!(run(&h, "web_nope", json!({})).await.is_error);
    }
}
