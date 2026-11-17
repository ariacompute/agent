//! Web seam: search + fetch only.
//!
//! dsh registers several providers (`web-search-deepseek`, `web-fetch-http`) on
//! one seam and lets `tool-web` keep the model-facing names stable. Same shape
//! here, plus a hard SSRF gate: no loopback, private, link-local or
//! single-label host ever reaches a provider ([`is_public_url`]).

use super::{SeamError, SeamResult};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// One search hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebResult {
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub snippet: String,
}

#[async_trait]
pub trait WebSeam: Send + Sync {
    async fn search(&self, query: &str, limit: usize) -> SeamResult<Vec<WebResult>>;
    /// Fetch a URL and return its body as text (already size-capped by the
    /// provider). Must reject non-public URLs.
    async fn fetch(&self, url: &str) -> SeamResult<String>;
}

/// Provider for deployments without network access: fail closed.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoWeb;

#[async_trait]
impl WebSeam for NoWeb {
    async fn search(&self, _query: &str, _limit: usize) -> SeamResult<Vec<WebResult>> {
        Err(SeamError::Unavailable(
            "no web provider is configured for this deployment".into(),
        ))
    }

    async fn fetch(&self, _url: &str) -> SeamResult<String> {
        Err(SeamError::Unavailable(
            "no web provider is configured for this deployment".into(),
        ))
    }
}

/// Maximum accepted response body for a fetch (1 MiB).
pub const MAX_FETCH_BYTES: usize = 1024 * 1024;

/// SSRF gate: allow only absolute `http(s)` URLs with a public, multi-label host.
///
/// Deliberately string-based (no DNS resolution): a provider must not be handed
/// an obviously internal target, and resolution-time rebinding is the provider's
/// own responsibility.
pub fn is_public_url(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return false;
    }
    // Strip an explicit port and IPv6 brackets.
    let host = if let Some(stripped) = authority.strip_prefix('[') {
        match stripped.split_once(']') {
            Some((h, _)) => h,
            None => return false,
        }
    } else {
        authority.split(':').next().unwrap_or("")
    };
    if host.is_empty() {
        return false;
    }
    let host = host.to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") {
        return false;
    }
    if host.ends_with(".local") || host.ends_with(".internal") || host.ends_with(".home.arpa") {
        return false;
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return is_public_ip(ip);
    }
    // Require a dotted domain so `intranet`-style single labels are refused.
    if !host.contains('.') {
        return false;
    }
    // Reject private-looking IPv4 written without dots handling (0x7f.1 etc.).
    if host.starts_with("0x") || host.starts_with("0") {
        return false;
    }
    true
}

fn is_public_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.is_documentation()
                || v4.octets()[0] == 0
                || (v4.octets()[0] == 100 && (64..=127).contains(&v4.octets()[1])))
        }
        std::net::IpAddr::V6(v6) => {
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_unicast_link_local())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_web_is_fail_closed() {
        let web = NoWeb;
        assert!(matches!(
            web.search("q", 3).await,
            Err(SeamError::Unavailable(_))
        ));
        assert!(matches!(
            web.fetch("https://example.com").await,
            Err(SeamError::Unavailable(_))
        ));
    }

    #[test]
    fn public_urls_are_allowed() {
        for url in [
            "https://example.com",
            "https://example.com/a?b=c#d",
            "http://sub.example.co.uk:8080/path",
            "https://93.184.216.34/x",
            "https://[2606:4700:4700::1111]/",
        ] {
            assert!(is_public_url(url), "{url} should be allowed");
        }
    }

    #[test]
    fn internal_targets_are_rejected() {
        for url in [
            "file:///etc/passwd",
            "ftp://example.com",
            "gopher://example.com",
            "https://localhost/admin",
            "https://foo.localhost/",
            "https://printer.local/",
            "https://svc.internal/",
            "https://127.0.0.1/",
            "https://10.1.2.3/",
            "https://172.16.0.1/",
            "https://192.168.1.1/",
            "https://169.254.169.254/latest/meta-data",
            "https://0.0.0.0/",
            "https://[::1]/",
            "https://[fd00::1]/",
            "https://[fe80::1]/",
            "https://intranet/",
            "https://user:pass@example.com/",
            "https:///nohost",
            "https://[::1/nohost]",
            "https://:80/path",
            "https://0x7f.1/",
            "not-a-url",
        ] {
            assert!(!is_public_url(url), "{url} should be rejected");
        }
    }

    #[test]
    fn max_fetch_bytes_is_one_mib() {
        assert_eq!(MAX_FETCH_BYTES, 1024 * 1024);
    }
}
