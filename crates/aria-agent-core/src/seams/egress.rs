//! Egress seam: the single boundary that decides whether an outbound
//! connection is allowed to happen.
//!
//! **Deny-by-default is the only default** (ADR-0020). A deployment that wants
//! network access must name the hosts it wants; an `allowlist` policy that names
//! no hosts is a configuration error, never a silent "allow everything".
//!
//! The seam deliberately knows nothing about credentials or token accounting —
//! the governance crate implements one provider that answers all three questions
//! at the same interception point.

use super::{SeamError, SeamResult};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Longest host name accepted by [`HostPattern::parse`] (DNS limit).
pub const MAX_HOST_PATTERN_LEN: usize = 253;

/// How a [`EgressPolicy`] interprets its host lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EgressMode {
    /// Nothing may leave the sandbox.
    DenyAll,
    /// Only `allowed` hosts may be reached.
    Allowlist,
    /// Every host except `denied` may be reached.
    Denylist,
}

impl EgressMode {
    /// Parse the wire/config spelling. Unknown values return `None` so callers
    /// fail loudly instead of guessing a policy.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "deny_all" | "deny-all" | "denyall" => Some(EgressMode::DenyAll),
            "allowlist" | "allow" => Some(EgressMode::Allowlist),
            "denylist" | "deny" => Some(EgressMode::Denylist),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            EgressMode::DenyAll => "deny_all",
            EgressMode::Allowlist => "allowlist",
            EgressMode::Denylist => "denylist",
        }
    }
}

impl fmt::Display for EgressMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A host the policy can talk about: `api.openai.com`, or `*.pythonhosted.org`.
///
/// Accepted form is a **host name only** — no scheme, path, port, query or user
/// info. A wildcard is only accepted as a leading `*.` label and it matches
/// sub-domains only; `*.example.com` deliberately does **not** match
/// `example.com`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HostPattern {
    raw: String,
}

impl HostPattern {
    /// Parse and normalise a host pattern (lowercased, trimmed).
    pub fn parse(raw: &str) -> Result<Self, SeamError> {
        let host = raw.trim().trim_end_matches('.').to_ascii_lowercase();
        if host.is_empty() {
            return Err(SeamError::Invalid("egress host must not be empty".into()));
        }
        if host.len() > MAX_HOST_PATTERN_LEN {
            return Err(SeamError::Invalid(format!(
                "egress host exceeds {MAX_HOST_PATTERN_LEN} bytes: {host}"
            )));
        }
        for banned in ["://", "/", "?", "#", "@", ":"] {
            if host.contains(banned) {
                return Err(SeamError::Invalid(format!(
                    "egress hosts are host names, not URLs (found `{banned}` in `{host}`)"
                )));
            }
        }
        let body = match host.strip_prefix("*.") {
            Some(rest) => rest,
            None => host.as_str(),
        };
        if body.is_empty() || body.starts_with('.') || body.ends_with('.') {
            return Err(SeamError::Invalid(format!(
                "malformed egress host pattern: {host}"
            )));
        }
        if host.contains('*') && !host.starts_with("*.") {
            return Err(SeamError::Invalid(format!(
                "wildcards are only supported as a leading `*.` label: {host}"
            )));
        }
        Ok(Self { raw: host })
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }

    pub fn is_wildcard(&self) -> bool {
        self.raw.starts_with("*.")
    }

    /// Does `host` match this pattern?
    pub fn matches(&self, host: &str) -> bool {
        let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
        match self.raw.strip_prefix("*.") {
            Some(suffix) => {
                host.len() > suffix.len()
                    && host.ends_with(suffix)
                    && host.as_bytes()[host.len() - suffix.len() - 1] == b'.'
            }
            None => host == self.raw,
        }
    }
}

impl TryFrom<String> for HostPattern {
    type Error = SeamError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        HostPattern::parse(&value)
    }
}

impl From<HostPattern> for String {
    fn from(value: HostPattern) -> Self {
        value.raw
    }
}

impl fmt::Display for HostPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

/// The answer for one destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EgressDecision {
    Allow,
    Deny { reason: String },
}

impl EgressDecision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, EgressDecision::Allow)
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            EgressDecision::Allow => None,
            EgressDecision::Deny { reason } => Some(reason),
        }
    }
}

/// An immutable, versioned policy snapshot.
///
/// Every mutation produces a new `revision`; callers hand the revision they read
/// back when patching, so a concurrent writer cannot be silently overwritten
/// (ADR-0020 §禁静默覆盖).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EgressPolicy {
    pub revision: u64,
    pub mode: EgressMode,
    pub allowed: Vec<HostPattern>,
    pub denied: Vec<HostPattern>,
}

impl EgressPolicy {
    pub fn new(
        revision: u64,
        mode: EgressMode,
        allowed: Vec<HostPattern>,
        denied: Vec<HostPattern>,
    ) -> Self {
        Self {
            revision,
            mode,
            allowed: normalize(allowed),
            denied: normalize(denied),
        }
    }

    /// The default: nothing leaves.
    pub fn deny_all() -> Self {
        Self {
            revision: 1,
            mode: EgressMode::DenyAll,
            allowed: Vec::new(),
            denied: Vec::new(),
        }
    }

    /// Decide one destination. `denied` always wins over `allowed`, so a typo in
    /// an allowlist can never re-open a host an operator explicitly blocked.
    pub fn decide(&self, host: &str) -> EgressDecision {
        if self.denied.iter().any(|p| p.matches(host)) {
            return EgressDecision::Deny {
                reason: format!(
                    "host `{host}` is explicitly denied by egress policy r{}",
                    self.revision
                ),
            };
        }
        match self.mode {
            EgressMode::DenyAll => EgressDecision::Deny {
                reason: format!(
                    "egress is deny-by-default and `{host}` is not on the allowlist (policy r{}); \
                     add it to AGENT_EGRESS_ALLOW",
                    self.revision
                ),
            },
            EgressMode::Allowlist => {
                if self.allowed.iter().any(|p| p.matches(host)) {
                    EgressDecision::Allow
                } else {
                    EgressDecision::Deny {
                        reason: format!(
                            "host `{host}` is not on the egress allowlist (policy r{})",
                            self.revision
                        ),
                    }
                }
            }
            EgressMode::Denylist => EgressDecision::Allow,
        }
    }

    /// Boot-time validation: an allowlist that names nothing is a mistake, not a
    /// deny-all request (use `deny_all` for that).
    pub fn validate(&self) -> SeamResult<()> {
        if self.mode == EgressMode::Allowlist && self.allowed.is_empty() {
            return Err(SeamError::Invalid(
                "egress mode is `allowlist` but no hosts were listed; \
                 use `deny_all` to block everything"
                    .into(),
            ));
        }
        Ok(())
    }

    /// Human-readable one-liner for startup logs (no secrets involved).
    pub fn describe(&self) -> String {
        format!(
            "mode={} revision={} allow={} deny={}",
            self.mode,
            self.revision,
            join(&self.allowed),
            join(&self.denied)
        )
    }
}

fn normalize(mut hosts: Vec<HostPattern>) -> Vec<HostPattern> {
    hosts.sort_by(|a, b| a.raw.cmp(&b.raw));
    hosts.dedup();
    hosts
}

fn join(hosts: &[HostPattern]) -> String {
    if hosts.is_empty() {
        "-".to_string()
    } else {
        hosts
            .iter()
            .map(|h| h.as_str())
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// The seam. Providers may answer from memory or from a shared store; the
/// consumer only ever sees a policy snapshot and a decision.
#[async_trait]
pub trait EgressSeam: Send + Sync {
    /// Current policy snapshot.
    async fn policy(&self) -> SeamResult<EgressPolicy>;

    /// Decide one destination against the current snapshot.
    async fn decide(&self, host: &str) -> SeamResult<EgressDecision> {
        Ok(self.policy().await?.decide(host))
    }
}

/// Fail-closed default: no provider configured means nothing leaves.
#[derive(Debug, Clone, Copy, Default)]
pub struct DenyAllEgress;

#[async_trait]
impl EgressSeam for DenyAllEgress {
    async fn policy(&self) -> SeamResult<EgressPolicy> {
        Ok(EgressPolicy::deny_all())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_parse_and_display() {
        for (raw, mode) in [
            ("deny_all", EgressMode::DenyAll),
            ("deny-all", EgressMode::DenyAll),
            ("allowlist", EgressMode::Allowlist),
            ("denylist", EgressMode::Denylist),
        ] {
            assert_eq!(EgressMode::parse(raw), Some(mode), "{raw}");
        }
        assert_eq!(EgressMode::parse("open"), None, "no silent open mode");
        assert_eq!(EgressMode::Allowlist.to_string(), "allowlist");
        assert_eq!(EgressMode::as_str(&EgressMode::DenyAll), "deny_all");
        assert_eq!(EgressMode::as_str(&EgressMode::Denylist), "denylist");
    }

    #[test]
    fn host_patterns_are_normalised_and_validated() {
        let host = HostPattern::parse("  API.OpenAI.com. ").unwrap();
        assert_eq!(host.as_str(), "api.openai.com");
        assert!(!host.is_wildcard());
        assert_eq!(String::from(host.clone()), "api.openai.com");
        assert_eq!(host.to_string(), "api.openai.com");
    }

    #[test]
    fn host_patterns_reject_urls_and_malformed_wildcards() {
        for bad in [
            "https://api.openai.com",
            "api.openai.com/v1",
            "api.openai.com:443",
            "user@api.openai.com",
            "",
            "*.",
            "api.*.com",
            "a?b",
            ".api.openai.com",
        ] {
            assert!(HostPattern::parse(bad).is_err(), "{bad}");
        }
        let too_long = "a".repeat(MAX_HOST_PATTERN_LEN + 1);
        assert!(HostPattern::parse(&too_long).is_err());
    }

    #[test]
    fn wildcards_match_subdomains_only() {
        let wildcard = HostPattern::parse("*.pythonhosted.org").unwrap();
        assert!(wildcard.matches("files.pythonhosted.org"));
        assert!(wildcard.matches("a.b.pythonhosted.org"));
        assert!(
            !wildcard.matches("pythonhosted.org"),
            "apex is not a subdomain"
        );
        assert!(!wildcard.matches("evilpythonhosted.org"));
        assert!(wildcard.matches("FILES.PythonHosted.org."));
    }

    #[test]
    fn deny_all_is_the_default_and_says_why() {
        let policy = EgressPolicy::deny_all();
        let denied = policy.decide("api.openai.com");
        assert!(!denied.is_allowed());
        assert!(denied.reason().unwrap().contains("deny-by-default"));
        assert_eq!(
            policy.decide("api.openai.com").reason(),
            denied.reason(),
            "decisions are stable"
        );
        assert!(EgressDecision::Allow.reason().is_none());
        assert!(policy.describe().contains("mode=deny_all"));
        assert!(policy.describe().contains("allow=-"));
    }

    #[test]
    fn allowlist_is_exact_and_deny_wins() {
        let policy = EgressPolicy::new(
            7,
            EgressMode::Allowlist,
            vec![
                HostPattern::parse("api.openai.com").unwrap(),
                HostPattern::parse("*.pypi.org").unwrap(),
            ],
            vec![HostPattern::parse("plugins.pypi.org").unwrap()],
        );
        assert!(policy.decide("api.openai.com").is_allowed());
        assert!(policy.decide("files.pypi.org").is_allowed());
        assert!(!policy.decide("pypi.org").is_allowed(), "apex not allowed");
        assert!(!policy.decide("openai.com").is_allowed());
        assert!(!policy.decide("plugins.pypi.org").is_allowed(), "deny wins");
        assert!(policy.validate().is_ok());
    }

    #[test]
    fn denylist_allows_everything_else() {
        let policy = EgressPolicy::new(
            2,
            EgressMode::Denylist,
            Vec::new(),
            vec![HostPattern::parse("169.254.169.254").unwrap()],
        );
        assert!(!policy.decide("169.254.169.254").is_allowed());
        assert!(policy.decide("example.com").is_allowed());
    }

    #[test]
    fn allowlist_without_hosts_is_a_configuration_error() {
        let policy = EgressPolicy::new(1, EgressMode::Allowlist, Vec::new(), Vec::new());
        assert!(matches!(policy.validate(), Err(SeamError::Invalid(_))));
        assert!(policy.describe().contains("mode=allowlist"));
    }

    #[test]
    fn policy_snapshots_normalise_host_lists() {
        let policy = EgressPolicy::new(
            3,
            EgressMode::Allowlist,
            vec![
                HostPattern::parse("b.example.com").unwrap(),
                HostPattern::parse("a.example.com").unwrap(),
                HostPattern::parse("a.example.com").unwrap(),
            ],
            Vec::new(),
        );
        assert_eq!(policy.allowed.len(), 2, "sorted and deduplicated");
        assert_eq!(policy.allowed[0].as_str(), "a.example.com");
        assert!(policy.describe().contains("a.example.com,b.example.com"));
    }

    #[test]
    fn policy_round_trips_through_json_and_revalidates() {
        let policy = EgressPolicy::new(
            4,
            EgressMode::Allowlist,
            vec![HostPattern::parse("*.example.com").unwrap()],
            Vec::new(),
        );
        let json = serde_json::to_string(&policy).unwrap();
        let back: EgressPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, policy);
        let bad = r#"{"revision":1,"mode":"allowlist","allowed":["https://x"],"denied":[]}"#;
        assert!(serde_json::from_str::<EgressPolicy>(bad).is_err());
    }

    #[tokio::test]
    async fn default_provider_denies_everything() {
        let seam = DenyAllEgress;
        let policy = seam.policy().await.unwrap();
        assert_eq!(policy.mode, EgressMode::DenyAll);
        assert!(!seam.decide("example.com").await.unwrap().is_allowed());
    }
}
