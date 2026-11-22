//! One place that turns [`AgentConfig`] + environment into the governance
//! stack's settings.
//!
//! The rule this module enforces is the fix for Runta's "dashboard enables
//! compression, CLI does not" problem: **defaults live in `AgentConfig` only**.
//! Environment variables may *override* a value, never introduce a different
//! default, and every override is recorded in [`GovernanceConfig::describe`] so
//! startup logs show what is actually in force.

use agent_core::seams::{EgressDecision, EgressMode, EgressPolicy, EgressSeam, HostPattern};
use agent_core::{AgentConfig, CoreError};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::RwLock;

/// How egress policy is physically enforced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Enforcement {
    /// Forward proxy injected through `HTTP(S)_PROXY` (default).
    Proxy,
    /// Proxy container on an internal Docker network (strongest).
    Network,
    /// No enforcement. Requires an explicit acknowledgement and is audited.
    Off,
}

impl Enforcement {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "proxy" => Some(Enforcement::Proxy),
            "network" => Some(Enforcement::Network),
            "off" => Some(Enforcement::Off),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Enforcement::Proxy => "proxy",
            Enforcement::Network => "network",
            Enforcement::Off => "off",
        }
    }

    /// Enforcement modes that actually intercept traffic.
    pub fn is_enforcing(&self) -> bool {
        !matches!(self, Enforcement::Off)
    }
}

/// Resolved governance settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GovernanceConfig {
    pub egress: EgressPolicy,
    pub enforcement: Enforcement,
    pub allow_unenforced: bool,
    pub audit_enabled: bool,
    pub audit_dir: Option<std::path::PathBuf>,
    pub audit_retention_days: u64,
    pub metering_enabled: bool,
    pub compression_enabled: bool,
    pub idle_timeout_secs: u64,
    pub idle_scan_secs: u64,
    pub credentials_dir: Option<std::path::PathBuf>,
    pub master_key_env: String,
    pub rotate_after_secs: u64,
    /// Which values came from the environment (for the startup log).
    pub env_overrides: Vec<String>,
}

impl GovernanceConfig {
    /// Build from the single source of defaults, then apply env overrides.
    pub fn from_agent_config(cfg: &AgentConfig) -> Result<Self, CoreError> {
        cfg.validate_governance()?;
        let mut resolved = Self {
            egress: cfg.egress_policy()?,
            enforcement: enforcement_from(&cfg.egress_enforcement)?,
            allow_unenforced: cfg.allow_unenforced_egress,
            audit_enabled: cfg.audit_sink.trim() == "jsonl",
            audit_dir: non_empty(&cfg.audit_dir)
                .map(std::path::PathBuf::from)
                .or_else(|| default_audit_dir(cfg)),
            audit_retention_days: cfg.audit_retention_days,
            metering_enabled: cfg.metering.trim() == "on",
            compression_enabled: cfg.compression.trim() == "on",
            idle_timeout_secs: cfg.idle_timeout_secs,
            idle_scan_secs: cfg.idle_scan_secs,
            credentials_dir: non_empty(&cfg.credentials_dir).map(std::path::PathBuf::from),
            master_key_env: cfg.credential_master_key_env.trim().to_string(),
            rotate_after_secs: cfg.credential_rotate_secs,
            env_overrides: Vec::new(),
        };
        resolved.apply_env();
        resolved.validate()?;
        Ok(resolved)
    }

    fn apply_env(&mut self) {
        self.apply_from(env_non_empty);
    }

    /// Testable half of [`Self::apply_env`]: `get` returns a trimmed, non-empty
    /// value, or `None` when unset. Keeping it injected means the tests do not
    /// have to mutate process-global state.
    fn apply_from(&mut self, get: impl Fn(&str) -> Option<String>) {
        if let Some(raw) = get("AGENT_EGRESS_MODE") {
            if let Some(mode) = EgressMode::parse(&raw) {
                self.egress.mode = mode;
                self.env_overrides.push("AGENT_EGRESS_MODE".into());
            } else {
                // Leave the value untouched; `validate` reports it via the
                // recorded override so the error names the variable.
                self.env_overrides
                    .push(format!("AGENT_EGRESS_MODE={raw} (invalid)"));
            }
        }
        if let Some(raw) = get("AGENT_EGRESS_ALLOW") {
            match parse_hosts(&raw) {
                Ok(hosts) => {
                    self.egress.allowed = hosts;
                    self.env_overrides.push("AGENT_EGRESS_ALLOW".into());
                }
                Err(bad) => self
                    .env_overrides
                    .push(format!("AGENT_EGRESS_ALLOW={bad} (invalid)")),
            }
        }
        if let Some(raw) = get("AGENT_EGRESS_DENY") {
            match parse_hosts(&raw) {
                Ok(hosts) => {
                    self.egress.denied = hosts;
                    self.env_overrides.push("AGENT_EGRESS_DENY".into());
                }
                Err(bad) => self
                    .env_overrides
                    .push(format!("AGENT_EGRESS_DENY={bad} (invalid)")),
            }
        }
        if let Some(raw) = get("AGENT_EGRESS_ENFORCEMENT") {
            match Enforcement::parse(&raw) {
                Some(mode) => {
                    self.enforcement = mode;
                    self.env_overrides.push("AGENT_EGRESS_ENFORCEMENT".into());
                }
                None => self
                    .env_overrides
                    .push(format!("AGENT_EGRESS_ENFORCEMENT={raw} (invalid)")),
            }
        }
        if let Some(raw) = get("AGENT_ALLOW_UNENFORCED_EGRESS") {
            self.allow_unenforced = raw == "1" || raw.eq_ignore_ascii_case("true");
            self.env_overrides
                .push("AGENT_ALLOW_UNENFORCED_EGRESS".into());
        }
        if let Some(raw) = get("AGENT_AUDIT_DIR") {
            self.audit_dir = Some(std::path::PathBuf::from(raw));
            self.env_overrides.push("AGENT_AUDIT_DIR".into());
        }
        if let Some(raw) = get("AGENT_CREDENTIALS_DIR") {
            self.credentials_dir = Some(std::path::PathBuf::from(raw));
            self.env_overrides.push("AGENT_CREDENTIALS_DIR".into());
        }
        if let Some(raw) = get("AGENT_CREDENTIAL_MASTER_KEY_ENV") {
            self.master_key_env = raw;
            self.env_overrides
                .push("AGENT_CREDENTIAL_MASTER_KEY_ENV".into());
        }
        if let Some(raw) = get("AGENT_AUDIT_RETENTION_DAYS") {
            match raw.parse::<u64>() {
                Ok(days) => {
                    self.audit_retention_days = days;
                    self.env_overrides.push("AGENT_AUDIT_RETENTION_DAYS".into());
                }
                Err(_) => self
                    .env_overrides
                    .push(format!("AGENT_AUDIT_RETENTION_DAYS={raw} (invalid)")),
            }
        }
        if let Some(raw) = get("AGENT_IDLE_TIMEOUT_SECS") {
            match raw.parse::<u64>() {
                Ok(secs) => {
                    self.idle_timeout_secs = secs;
                    self.env_overrides.push("AGENT_IDLE_TIMEOUT_SECS".into());
                }
                Err(_) => self
                    .env_overrides
                    .push(format!("AGENT_IDLE_TIMEOUT_SECS={raw} (invalid)")),
            }
        }
    }

    /// Fail closed on anything ambiguous.
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.env_overrides.iter().any(|o| o.contains("(invalid)")) {
            return Err(CoreError::Config(format!(
                "invalid governance environment overrides: {}",
                self.env_overrides.join(", ")
            )));
        }
        self.egress
            .validate()
            .map_err(|e| CoreError::Config(format!("egress policy: {e}")))?;
        if self.enforcement == Enforcement::Off && !self.allow_unenforced {
            return Err(CoreError::Config(
                "egress enforcement is off but AGENT_ALLOW_UNENFORCED_EGRESS is not set; \
                 refusing to start"
                    .into(),
            ));
        }
        if self.audit_enabled && self.audit_dir.is_none() {
            return Err(CoreError::Config(
                "audit_sink=jsonl needs an audit directory (AGENT_AUDIT_DIR)".into(),
            ));
        }
        if self.idle_scan_secs == 0 && self.idle_timeout_secs > 0 {
            return Err(CoreError::Config(
                "idle_scan_secs must be > 0 when idle_timeout_secs is set".into(),
            ));
        }
        Ok(())
    }

    /// Startup log line. Contains no secrets and no user content.
    pub fn describe(&self) -> String {
        format!(
            "egress[{}] enforcement={} audit={} metering={} compression={} idle={}s/scan={}s overrides=[{}]",
            self.egress.describe(),
            self.enforcement.as_str(),
            if self.audit_enabled { "jsonl" } else { "off" },
            if self.metering_enabled { "on" } else { "off" },
            if self.compression_enabled { "on" } else { "off" },
            self.idle_timeout_secs,
            self.idle_scan_secs,
            self.env_overrides.join(",")
        )
    }
}

/// Parse the enforcement mode, naming the offending value on failure.
fn enforcement_from(raw: &str) -> Result<Enforcement, CoreError> {
    Enforcement::parse(raw)
        .ok_or_else(|| CoreError::Config(format!("unknown egress_enforcement: {raw}")))
}

/// `<workspace>/.aria/audit` when auditing is on and no directory was given.
fn default_audit_dir(cfg: &AgentConfig) -> Option<std::path::PathBuf> {
    if cfg.audit_sink.trim() != "jsonl" {
        return None;
    }
    let root = if cfg.workspace_root.trim().is_empty() {
        std::env::current_dir().ok()?
    } else {
        std::path::PathBuf::from(cfg.workspace_root.trim())
    };
    Some(root.join(".aria").join("audit"))
}

fn non_empty(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn env_non_empty(name: &str) -> Option<String> {
    std::env::var(name).ok().and_then(|v| non_empty(&v))
}

/// Comma/whitespace separated host list.
fn parse_hosts(raw: &str) -> Result<Vec<HostPattern>, String> {
    raw.split([',', ' ', '\t', '\n'])
        .filter(|s| !s.trim().is_empty())
        .map(|s| HostPattern::parse(s).map_err(|e| format!("{s}: {e}")))
        .collect()
}

/// A mutable policy holder: every mutation bumps the revision, and a patch that
/// carries a stale revision is refused instead of silently overwriting.
#[derive(Debug)]
pub struct PolicyStore {
    policy: RwLock<EgressPolicy>,
}

impl PolicyStore {
    pub fn new(policy: EgressPolicy) -> Self {
        Self {
            policy: RwLock::new(policy),
        }
    }

    pub fn snapshot(&self) -> EgressPolicy {
        self.policy
            .read()
            .map(|p| p.clone())
            .unwrap_or_else(|_| EgressPolicy::deny_all())
    }

    /// Replace the whole policy. Returns the new snapshot (with a new revision).
    pub fn replace(
        &self,
        mut next: EgressPolicy,
    ) -> Result<EgressPolicy, agent_core::seams::SeamError> {
        next.validate()?;
        let mut slot = self
            .policy
            .write()
            .map_err(|_| agent_core::seams::SeamError::Io("egress policy lock poisoned".into()))?;
        next.revision = slot.revision.saturating_add(1);
        slot.allowed = next.allowed.clone();
        slot.mode = next.mode;
        slot.denied = next.denied.clone();
        slot.revision = next.revision;
        Ok(slot.clone())
    }

    /// Incremental patch bound to `expected_revision`.
    ///
    /// A mismatch is a conflict (`SeamError::Invalid`), never a last-writer-wins
    /// overwrite — that is the Runta `set_policy` footgun this avoids.
    pub fn patch(
        &self,
        expected_revision: u64,
        allow_add: Vec<HostPattern>,
        allow_remove: Vec<HostPattern>,
        deny_add: Vec<HostPattern>,
        deny_remove: Vec<HostPattern>,
    ) -> Result<EgressPolicy, agent_core::seams::SeamError> {
        let mut slot = self
            .policy
            .write()
            .map_err(|_| agent_core::seams::SeamError::Io("egress policy lock poisoned".into()))?;
        if slot.revision != expected_revision {
            return Err(agent_core::seams::SeamError::Invalid(format!(
                "egress policy revision conflict: expected r{expected_revision}, current r{}",
                slot.revision
            )));
        }
        let mut allowed = slot.allowed.clone();
        let mut denied = slot.denied.clone();
        let mut extra: Vec<_> = allow_add
            .into_iter()
            .filter(|host| !allowed.contains(host))
            .collect();
        allowed.append(&mut extra);
        for host in allow_remove {
            allowed.retain(|h| h != &host);
        }
        let mut extra: Vec<_> = deny_add
            .into_iter()
            .filter(|host| !denied.contains(host))
            .collect();
        denied.append(&mut extra);
        for host in deny_remove {
            denied.retain(|h| h != &host);
        }
        let next = EgressPolicy::new(slot.revision.saturating_add(1), slot.mode, allowed, denied);
        next.validate()?;
        *slot = next.clone();
        Ok(next)
    }
}

#[async_trait]
impl EgressSeam for PolicyStore {
    async fn policy(&self) -> agent_core::seams::SeamResult<EgressPolicy> {
        Ok(self.snapshot())
    }

    async fn decide(&self, host: &str) -> agent_core::seams::SeamResult<EgressDecision> {
        Ok(self.snapshot().decide(host))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AgentConfig {
        AgentConfig::default()
    }

    #[test]
    fn defaults_are_deny_by_default_and_consistent() {
        let resolved = GovernanceConfig::from_agent_config(&cfg()).unwrap();
        assert_eq!(resolved.egress.mode, EgressMode::DenyAll);
        assert_eq!(resolved.enforcement, Enforcement::Proxy);
        assert!(resolved.audit_enabled);
        assert!(resolved.metering_enabled);
        assert!(resolved.compression_enabled);
        assert_eq!(resolved.idle_timeout_secs, 900);
        assert_eq!(resolved.idle_scan_secs, 60);
        assert!(resolved.describe().contains("mode=deny_all"));
    }

    #[test]
    fn allowlist_without_hosts_is_refused() {
        let mut c = cfg();
        c.egress_mode = "allowlist".into();
        let err = GovernanceConfig::from_agent_config(&c).unwrap_err();
        assert!(err.to_string().contains("allowlist"), "{err}");
    }

    #[test]
    fn unknown_modes_are_configuration_errors() {
        let mut c = cfg();
        c.egress_mode = "open".into();
        assert!(GovernanceConfig::from_agent_config(&c).is_err());

        let mut c = cfg();
        c.egress_enforcement = "trust".into();
        assert!(GovernanceConfig::from_agent_config(&c).is_err());

        let mut c = cfg();
        c.metering = "sometimes".into();
        assert!(GovernanceConfig::from_agent_config(&c).is_err());
    }

    #[test]
    fn unenforced_egress_needs_an_explicit_acknowledgement() {
        let mut c = cfg();
        c.egress_enforcement = "off".into();
        assert!(GovernanceConfig::from_agent_config(&c).is_err());
        c.allow_unenforced_egress = true;
        let resolved = GovernanceConfig::from_agent_config(&c).unwrap();
        assert_eq!(resolved.enforcement, Enforcement::Off);
        assert!(!resolved.enforcement.is_enforcing());
        assert!(resolved.describe().contains("enforcement=off"));
    }

    #[test]
    fn audit_defaults_under_the_workspace_when_no_dir_is_given() {
        let mut c = cfg();
        c.workspace_root = "/tmp/ws".into();
        let resolved = GovernanceConfig::from_agent_config(&c).unwrap();
        assert_eq!(
            resolved.audit_dir.as_deref(),
            Some(std::path::Path::new("/tmp/ws/.aria/audit"))
        );

        c.audit_dir = "/tmp/audit".into();
        let resolved = GovernanceConfig::from_agent_config(&c).unwrap();
        assert_eq!(
            resolved.audit_dir.as_deref(),
            Some(std::path::Path::new("/tmp/audit"))
        );

        c.audit_sink = "none".into();
        c.audit_dir = String::new();
        let resolved = GovernanceConfig::from_agent_config(&c).unwrap();
        assert!(resolved.audit_dir.is_none());
        assert!(!resolved.audit_enabled);
    }

    #[test]
    fn idle_scan_must_be_positive_when_a_timeout_is_set() {
        let mut c = cfg();
        c.idle_scan_secs = 0;
        assert!(GovernanceConfig::from_agent_config(&c).is_err());
        c.idle_timeout_secs = 0;
        assert!(GovernanceConfig::from_agent_config(&c).is_ok());
    }

    #[test]
    fn host_lists_parse_and_report_the_offending_entry() {
        let hosts = parse_hosts("a.com, *.b.com  c.com").unwrap();
        assert_eq!(hosts.len(), 3);
        let bad = parse_hosts("https://a.com").unwrap_err();
        assert!(bad.contains("a.com"), "{bad}");
    }

    #[test]
    fn policy_store_bumps_revisions_and_refuses_stale_patches() {
        let store = PolicyStore::new(EgressPolicy::new(
            1,
            EgressMode::Allowlist,
            vec![HostPattern::parse("a.com").unwrap()],
            Vec::new(),
        ));
        let next = store
            .replace(EgressPolicy::new(
                0,
                EgressMode::Allowlist,
                vec![HostPattern::parse("b.com").unwrap()],
                Vec::new(),
            ))
            .unwrap();
        assert_eq!(next.revision, 2);
        assert_eq!(next.allowed[0].as_str(), "b.com");

        let patched = store
            .patch(
                2,
                vec![HostPattern::parse("c.com").unwrap()],
                vec![HostPattern::parse("b.com").unwrap()],
                vec![HostPattern::parse("evil.com").unwrap()],
                Vec::new(),
            )
            .unwrap();
        assert_eq!(patched.revision, 3);
        assert!(patched.decide("c.com").is_allowed());
        assert!(!patched.decide("b.com").is_allowed());
        assert!(!patched.decide("evil.com").is_allowed());

        let stale = store.patch(2, Vec::new(), Vec::new(), Vec::new(), Vec::new());
        assert!(stale.is_err(), "stale revision must conflict");

        // Removing a deny rule is a distinct path from adding one.
        let cleaned = store
            .patch(
                3,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                vec![HostPattern::parse("evil.com").unwrap()],
            )
            .unwrap();
        assert_eq!(cleaned.revision, 4);
        assert!(cleaned.denied.iter().all(|h| h.as_str() != "evil.com"));
    }

    #[test]
    fn environment_overrides_are_applied_and_invalid_ones_fail_closed() {
        let mut resolved = GovernanceConfig::from_agent_config(&cfg()).unwrap();
        let values = std::collections::HashMap::from([
            ("AGENT_EGRESS_MODE", "allowlist"),
            ("AGENT_EGRESS_ALLOW", "api.openai.com, *.example.com"),
            ("AGENT_EGRESS_DENY", "tracker.example"),
            ("AGENT_EGRESS_ENFORCEMENT", "network"),
            ("AGENT_ALLOW_UNENFORCED_EGRESS", "TRUE"),
            ("AGENT_AUDIT_DIR", "/tmp/audit"),
            ("AGENT_CREDENTIALS_DIR", "/tmp/creds"),
            ("AGENT_CREDENTIAL_MASTER_KEY_ENV", "MASTER_KEY"),
        ]);
        resolved.apply_from(|key| values.get(key).map(|v| v.to_string()));
        assert_eq!(resolved.enforcement, Enforcement::Network);
        assert_eq!(resolved.enforcement.as_str(), "network");
        assert_eq!(resolved.egress.mode, EgressMode::Allowlist);
        assert!(resolved.egress.decide("api.openai.com").is_allowed());
        assert!(!resolved.egress.decide("tracker.example").is_allowed());
        assert!(resolved.allow_unenforced);
        assert_eq!(
            resolved.audit_dir.as_deref(),
            Some(std::path::Path::new("/tmp/audit"))
        );
        assert_eq!(
            resolved.credentials_dir.as_deref(),
            Some(std::path::Path::new("/tmp/creds"))
        );
        assert_eq!(resolved.master_key_env, "MASTER_KEY");
        assert!(resolved.describe().contains("overrides=[AGENT_EGRESS_MODE"));
        resolved.validate().unwrap();

        let mut bad = GovernanceConfig::from_agent_config(&cfg()).unwrap();
        let values = std::collections::HashMap::from([
            ("AGENT_EGRESS_MODE", "bogus"),
            ("AGENT_EGRESS_ALLOW", "a*b.com"),
            ("AGENT_EGRESS_DENY", "a*b.com"),
            ("AGENT_EGRESS_ENFORCEMENT", "sometimes"),
        ]);
        bad.apply_from(|key| values.get(key).map(|v| v.to_string()));
        let err = bad.validate().unwrap_err();
        assert!(
            err.to_string().contains("invalid governance environment"),
            "{err}"
        );
    }

    #[test]
    fn audit_retention_and_idle_timeout_are_env_overridable() {
        let mut resolved = GovernanceConfig::from_agent_config(&cfg()).unwrap();
        let values = std::collections::HashMap::from([
            ("AGENT_AUDIT_RETENTION_DAYS", "7"),
            ("AGENT_IDLE_TIMEOUT_SECS", "120"),
        ]);
        resolved.apply_from(|key| values.get(key).map(|v| v.to_string()));
        assert_eq!(resolved.audit_retention_days, 7);
        assert_eq!(resolved.idle_timeout_secs, 120);
        assert!(resolved.describe().contains("AGENT_AUDIT_RETENTION_DAYS"));
        resolved.validate().unwrap();

        // Garbage is recorded and fails closed instead of silently keeping the
        // default, so a typo cannot disable retention or idle suspension.
        let mut bad = GovernanceConfig::from_agent_config(&cfg()).unwrap();
        let values = std::collections::HashMap::from([
            ("AGENT_AUDIT_RETENTION_DAYS", "thirty"),
            ("AGENT_IDLE_TIMEOUT_SECS", "-5"),
        ]);
        bad.apply_from(|key| values.get(key).map(|v| v.to_string()));
        let err = bad.validate().unwrap_err();
        assert!(
            err.to_string()
                .contains("AGENT_AUDIT_RETENTION_DAYS=thirty (invalid)"),
            "{err}"
        );
    }

    #[test]
    fn validate_refuses_off_without_ack_and_jsonl_without_a_dir() {
        let mut resolved = GovernanceConfig::from_agent_config(&cfg()).unwrap();
        resolved.enforcement = Enforcement::Off;
        resolved.allow_unenforced = false;
        let err = resolved.validate().unwrap_err();
        assert!(
            err.to_string().contains("AGENT_ALLOW_UNENFORCED_EGRESS"),
            "{err}"
        );

        resolved.allow_unenforced = true;
        resolved.audit_enabled = true;
        resolved.audit_dir = None;
        let err = resolved.validate().unwrap_err();
        assert!(err.to_string().contains("audit directory"), "{err}");

        // An invalid policy is reported through the same validation entry point.
        resolved.audit_enabled = false;
        resolved.egress = EgressPolicy::new(1, EgressMode::Allowlist, Vec::new(), Vec::new());
        let err = resolved.validate().unwrap_err();
        assert!(err.to_string().contains("egress policy"), "{err}");
    }

    #[test]
    fn enforcement_parsing_names_the_offending_value() {
        assert_eq!(Enforcement::parse("proxy"), Some(Enforcement::Proxy));
        assert_eq!(Enforcement::parse("network"), Some(Enforcement::Network));
        assert_eq!(Enforcement::parse("off"), Some(Enforcement::Off));
        assert!(Enforcement::parse("sometimes").is_none());
        assert_eq!(Enforcement::Proxy.as_str(), "proxy");
        let err = enforcement_from("sometimes").unwrap_err();
        assert!(err.to_string().contains("sometimes"), "{err}");
    }

    #[test]
    fn a_poisoned_policy_lock_degrades_to_deny_all_and_reports_io() {
        let store = PolicyStore::new(EgressPolicy::new(
            1,
            EgressMode::Allowlist,
            vec![HostPattern::parse("a.com").unwrap()],
            Vec::new(),
        ));
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = store.policy.write().unwrap();
            panic!("poison the policy lock");
        }));
        assert!(poisoned.is_err());
        // Reads must not propagate the poison: deny-all is the safe answer.
        assert_eq!(store.snapshot().mode, EgressMode::DenyAll);
        assert!(matches!(
            store.replace(EgressPolicy::deny_all()),
            Err(agent_core::seams::SeamError::Io(_))
        ));
        assert!(matches!(
            store.patch(1, Vec::new(), Vec::new(), Vec::new(), Vec::new()),
            Err(agent_core::seams::SeamError::Io(_))
        ));
    }

    #[test]
    fn replacing_with_an_empty_allowlist_is_refused() {
        let store = PolicyStore::new(EgressPolicy::new(
            1,
            EgressMode::Allowlist,
            vec![HostPattern::parse("a.com").unwrap()],
            Vec::new(),
        ));
        assert!(store
            .replace(EgressPolicy::new(
                0,
                EgressMode::Allowlist,
                Vec::new(),
                Vec::new()
            ))
            .is_err());
        assert_eq!(store.snapshot().revision, 1, "refused change is a no-op");
    }

    #[tokio::test]
    async fn policy_store_is_the_egress_seam() {
        let store = PolicyStore::new(EgressPolicy::deny_all());
        assert!(!store.decide("a.com").await.unwrap().is_allowed());
        assert_eq!(store.policy().await.unwrap().mode, EgressMode::DenyAll);
    }
}
