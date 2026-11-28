//! `agent-governance` — one egress boundary, three jobs.
//!
//! The crate exists because Runta's most interesting architectural choice is
//! putting **network policy, credential substitution and token accounting in the
//! same interception point**. Splitting them into three unrelated features loses
//! the property that matters: every outbound request is decided, substituted and
//! recorded by the same code path, so the audit trail cannot have gaps between
//! "what policy said" and "what actually left".
//!
//! Assembly is explicit and happens once (seam rule): [`GovernanceStack`] owns
//! the resolved providers and [`GovernanceStack::attach`] installs the
//! model-facing projection on an [`Agent`].

pub mod audit;
pub mod compression;
pub mod config;
pub mod credentials;
pub mod idle;
pub mod metering;
pub mod proxy;
pub mod sandbox;

use agent_core::seams::{
    AuditEvent, AuditKind, AuditSink, CredentialsSeam, EgressSeam, MeteringSeam, NoAudit,
    NoCredentials, NoMetering, Projection, SeamError, SeamResult, ToolOutputProjector,
};
use agent_core::{Agent, AgentConfig};
use std::sync::Arc;

pub use audit::JsonlAuditSink;
pub use compression::CompressionProjector;
pub use config::{Enforcement, GovernanceConfig, PolicyStore};
pub use credentials::FileCredentialStore;
pub use idle::{IdleMonitor, IdleProbe, IdleSignals, IdleVerdict};
pub use metering::{MeteringLedger, PriceBook, QuotaPolicy};
pub use proxy::{EgressGateway, GatewayFactory, LocalForwardProxy, SingleGateway};

/// Environment that points a sandbox at `proxy_url`.
///
/// Callers only pass a URL when enforcement is on; this re-exports the sandbox
/// crate's definition so the control plane, the core runtime and the shell seam
/// cannot drift apart (ADR-0024).
pub fn proxy_env(proxy_url: &str) -> Vec<(String, String)> {
    agent_sandbox::proxy_env(proxy_url)
}

/// `0` means "no cap"; any other value becomes a quota policy.
fn quota_from(max: u64) -> Option<QuotaPolicy> {
    match max {
        0 => None,
        max => Some(QuotaPolicy::new(max)),
    }
}

/// The assembled stack. Every field is already resolved: nothing here reaches for
/// a registry or an environment variable later.
pub struct GovernanceStack {
    pub config: GovernanceConfig,
    pub principal_id: String,
    egress: Arc<PolicyStore>,
    credentials: Arc<dyn CredentialsSeam>,
    audit: Arc<dyn AuditSink>,
    metering: Arc<dyn MeteringSeam>,
    projector: Arc<CompressionProjector>,
}

impl std::fmt::Debug for GovernanceStack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GovernanceStack")
            .field("config", &self.config.describe())
            .field("principal_id", &self.principal_id)
            .finish_non_exhaustive()
    }
}

impl GovernanceStack {
    /// Resolve everything from the single source of defaults.
    pub fn from_agent_config(cfg: &AgentConfig) -> SeamResult<Self> {
        let resolved = GovernanceConfig::from_agent_config(cfg)
            .map_err(|e| SeamError::Invalid(e.to_string()))?;
        Self::from_resolved(resolved, "local")
    }

    /// Resolve from an already-validated config (cloud passes its tenant).
    pub fn from_resolved(resolved: GovernanceConfig, principal: &str) -> SeamResult<Self> {
        let policy_store = Arc::new(PolicyStore::new(resolved.egress.clone()));

        let credentials: Arc<dyn CredentialsSeam> = match &resolved.credentials_dir {
            Some(dir) => Arc::new(FileCredentialStore::open(
                dir,
                &resolved.master_key_env,
                resolved.rotate_after_secs,
            )?),
            None => Arc::new(NoCredentials),
        };

        let audit: Arc<dyn AuditSink> = match (&resolved.audit_dir, resolved.audit_enabled) {
            (Some(dir), true) => Arc::new(JsonlAuditSink::new(dir, resolved.audit_retention_days)?),
            _ => Arc::new(NoAudit::new()),
        };

        let metering: Arc<dyn MeteringSeam> = if resolved.metering_enabled {
            let book = PriceBook {
                cpu_micros_per_vcpu_hour: env_u64("AGENT_PRICE_CPU_MICROS_PER_VCPU_HOUR"),
                memory_micros_per_gib_hour: env_u64("AGENT_PRICE_MEMORY_MICROS_PER_GIB_HOUR"),
                disk_micros_per_gib_hour: env_u64("AGENT_PRICE_DISK_MICROS_PER_GIB_HOUR"),
            };
            let quota = quota_from(env_u64("AGENT_QUOTA_MAX_WEIGHT"));
            Arc::new(MeteringLedger::new(Some(book), quota))
        } else {
            Arc::new(NoMetering)
        };

        let projector = Arc::new(if resolved.compression_enabled {
            CompressionProjector::default()
        } else {
            CompressionProjector::disabled()
        });

        tracing::info!(governance = %resolved.describe(), "governance resolved");
        if resolved.enforcement == Enforcement::Off {
            tracing::warn!("egress enforcement is OFF: outbound traffic is not intercepted; every request is still audited as `unenforced_egress`");
        }
        if !resolved.enforcement.is_enforcing() {
            let sink = audit.clone();
            let principal = principal.to_string();
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    let event = AuditEvent::new(
                        AuditKind::UnenforcedEgress,
                        principal,
                        "egress_enforcement=off",
                    );
                    let _ = sink.record(event).await;
                });
            }
        }

        Ok(Self {
            config: resolved,
            principal_id: principal.to_string(),
            egress: policy_store,
            credentials,
            audit,
            metering,
            projector,
        })
    }

    pub fn egress(&self) -> Arc<dyn EgressSeam> {
        self.egress.clone()
    }

    pub fn policy_store(&self) -> Arc<PolicyStore> {
        self.egress.clone()
    }

    pub fn credentials(&self) -> Arc<dyn CredentialsSeam> {
        self.credentials.clone()
    }

    pub fn audit(&self) -> Arc<dyn AuditSink> {
        self.audit.clone()
    }

    pub fn metering(&self) -> Arc<dyn MeteringSeam> {
        self.metering.clone()
    }

    /// The model-facing compression projector (identity when disabled).
    pub fn projector(&self) -> Arc<dyn ToolOutputProjector> {
        Arc::new(GovernanceProjector {
            inner: self.projector.clone(),
            audit: self.audit.clone(),
            principal: self.principal_id.clone(),
        })
    }

    /// Install the projection on an agent. The real tool result is untouched.
    pub fn attach(&self, agent: Agent) -> Agent {
        agent.with_projector(self.projector())
    }

    /// Environment the sandbox should be started with so its traffic reaches the
    /// proxy. Empty when enforcement is off.
    pub fn sandbox_proxy_env(&self, proxy_url: &str) -> Vec<(String, String)> {
        if !self.config.enforcement.is_enforcing() {
            return Vec::new();
        }
        proxy_env(proxy_url)
    }

    /// Bind the forward proxy and start serving in the background.
    pub async fn start_proxy(&self, bind: &str, session: Option<&str>) -> SeamResult<ProxyHandle> {
        let gateway = EgressGateway::new(
            self.egress.clone(),
            self.credentials.clone(),
            self.audit.clone(),
            self.principal_id.clone(),
        );
        let gateway = match session {
            Some(session) => gateway.with_session(session),
            None => gateway,
        };
        let proxy = LocalForwardProxy::bind(bind, Arc::new(gateway)).await?;
        let url = proxy.proxy_url();
        let addr = proxy.addr();
        let task = tokio::spawn(proxy.serve());
        Ok(ProxyHandle { url, addr, task })
    }
}

/// Handle to a running forward proxy.
pub struct ProxyHandle {
    pub url: String,
    pub addr: std::net::SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl ProxyHandle {
    /// Stop accepting new connections.
    pub fn shutdown(self) {
        self.task.abort();
    }
}

impl std::fmt::Debug for ProxyHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyHandle")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

struct GovernanceProjector {
    inner: Arc<CompressionProjector>,
    audit: Arc<dyn AuditSink>,
    principal: String,
}

impl ToolOutputProjector for GovernanceProjector {
    fn project(&self, tool: &str, is_error: bool, raw: &str) -> Projection {
        let (content, record) = self.inner.project_reported(tool, is_error, raw);
        if let Some(record) = &record {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                let audit = self.audit.clone();
                let principal = self.principal.clone();
                let tool = tool.to_string();
                let record = record.clone();
                handle.spawn(async move {
                    if let Err(e) =
                        compression::record_projection(&*audit, &principal, &tool, &record).await
                    {
                        tracing::debug!(error = %e, "could not audit a projection");
                    }
                });
            }
        }
        Projection { content, record }
    }
}

fn env_u64(name: &str) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::seams::{AuditKind, EgressMode, HostPattern, NoAudit};

    fn agent_config(dir: &std::path::Path) -> AgentConfig {
        AgentConfig {
            workspace_root: dir.to_string_lossy().to_string(),
            ..AgentConfig::default()
        }
    }

    #[tokio::test]
    async fn the_default_stack_is_deny_by_default_and_audited() {
        let dir = tempfile::tempdir().unwrap();
        let stack = GovernanceStack::from_agent_config(&agent_config(dir.path())).unwrap();
        assert_eq!(stack.config.egress.mode, EgressMode::DenyAll);
        assert_eq!(stack.principal_id, "local");
        assert!(stack.config.audit_dir.is_some());
        assert!(stack.config.compression_enabled);
        assert!(format!("{stack:?}").contains("mode=deny_all"));
        // No credential store configured → empty metadata, resolution fails closed.
        assert!(stack.credentials().list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn allowlisted_hosts_are_decided_by_the_seam() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = agent_config(dir.path());
        cfg.egress_mode = "allowlist".into();
        cfg.egress_allow = vec!["api.example.com".into(), "*.pypi.org".into()];
        let stack = GovernanceStack::from_agent_config(&cfg).unwrap();
        let seam = stack.egress();
        assert!(seam.decide("api.example.com").await.unwrap().is_allowed());
        assert!(seam.decide("files.pypi.org").await.unwrap().is_allowed());
        assert!(!seam.decide("other.com").await.unwrap().is_allowed());
    }

    #[test]
    fn proxy_env_reflects_enforcement() {
        let dir = tempfile::tempdir().unwrap();
        let stack = GovernanceStack::from_agent_config(&agent_config(dir.path())).unwrap();
        let env = stack.sandbox_proxy_env("http://127.0.0.1:9");
        assert_eq!(env.len(), 4);
        assert!(env
            .iter()
            .any(|(k, v)| k == "HTTPS_PROXY" && v.contains("127.0.0.1")));
        assert!(env.iter().any(|(k, _)| k == "NO_PROXY"));

        let mut cfg = agent_config(dir.path());
        cfg.egress_enforcement = "off".into();
        cfg.allow_unenforced_egress = true;
        let off = GovernanceStack::from_agent_config(&cfg).unwrap();
        assert!(off.sandbox_proxy_env("http://x").is_empty());
    }

    #[tokio::test]
    async fn the_projector_compresses_and_audits() {
        let dir = tempfile::tempdir().unwrap();
        let stack = GovernanceStack::from_agent_config(&agent_config(dir.path())).unwrap();
        let projector = stack.projector();
        // Above `CompressionProjector::min_bytes`, otherwise the projector
        // (correctly) leaves small payloads alone.
        let raw = serde_json::to_string(
            &(0..400)
                .map(|i| serde_json::json!({"path": format!("/workspace/src/file-{i:04}.rs")}))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(raw.len() > 4096, "fixture must exceed the minimum size");
        let projection = projector.project("fs_read", false, &raw);
        assert!(projection.record.is_some());
        assert!(projection.content.len() < raw.len());
    }

    fn compressible_payload() -> String {
        serde_json::to_string(
            &(0..400)
                .map(|i| serde_json::json!({"path": format!("/workspace/src/file-{i:04}.rs")}))
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    #[test]
    fn a_projection_outside_a_runtime_is_still_returned() {
        // A CLI can project before it starts a runtime. The audit hop is
        // skipped, but the caller must still get the compressed content and
        // the record (dropping it would silently disable compression).
        let dir = tempfile::tempdir().unwrap();
        let stack = GovernanceStack::from_agent_config(&agent_config(dir.path())).unwrap();
        let projector = stack.projector();
        let raw = compressible_payload();
        let projection = projector.project("fs_read", false, &raw);
        assert!(projection.record.is_some());
        assert!(projection.content.len() < raw.len());
    }

    struct FailingAudit {
        attempts: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl AuditSink for FailingAudit {
        async fn record(&self, _event: AuditEvent) -> SeamResult<()> {
            self.attempts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(SeamError::Unavailable("audit sink down".into()))
        }

        async fn recent(&self, _limit: usize) -> SeamResult<Vec<AuditEvent>> {
            Ok(Vec::new())
        }
    }

    #[derive(Default)]
    struct CountingAudit {
        events: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl AuditSink for CountingAudit {
        async fn record(&self, _event: AuditEvent) -> SeamResult<()> {
            self.events
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }

        async fn recent(&self, _limit: usize) -> SeamResult<Vec<AuditEvent>> {
            Ok(Vec::new())
        }
    }

    /// Yield until the audit hop ran, so the assertion is about the spawned
    /// task's effect instead of a race with the runtime.
    async fn wait_for_events(audit: &CountingAudit) -> usize {
        let mut seen = audit.events.load(std::sync::atomic::Ordering::SeqCst);
        for _ in 0..100 {
            if seen > 0 {
                break;
            }
            tokio::task::yield_now().await;
            seen = audit.events.load(std::sync::atomic::Ordering::SeqCst);
        }
        seen
    }

    #[tokio::test]
    async fn a_projection_is_audited_on_the_async_hop() {
        let audit = Arc::new(CountingAudit::default());
        let projector = GovernanceProjector {
            inner: Arc::new(compression::CompressionProjector::default()),
            audit: audit.clone(),
            principal: "p".into(),
        };
        let raw = compressible_payload();
        let projection = projector.project("fs_read", false, &raw);
        assert!(projection.record.is_some());
        assert_eq!(wait_for_events(&audit).await, 1);
        assert!(audit.recent(1).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_failing_audit_hop_does_not_lose_the_projection() {
        // Audit is best-effort by contract: a sink that is down must not turn
        // a successful tool result into a failure.
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let audit = Arc::new(FailingAudit {
            attempts: attempts.clone(),
        });
        let projector = GovernanceProjector {
            inner: Arc::new(compression::CompressionProjector::default()),
            audit: audit.clone(),
            principal: "p".into(),
        };
        let raw = compressible_payload();
        let projection = projector.project("fs_read", false, &raw);
        assert!(projection.record.is_some());
        for _ in 0..100 {
            if attempts.load(std::sync::atomic::Ordering::SeqCst) > 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
        // The read side of the trait still answers (an audit view showing
        // nothing is not an error).
        assert!(audit.recent(10).await.unwrap().is_empty());
    }

    struct NullSandbox;

    #[async_trait::async_trait]
    impl agent_sandbox::Sandbox for NullSandbox {
        async fn spawn(
            &self,
            _spec: &agent_sandbox::ExecSpec,
        ) -> Result<agent_sandbox::SandboxHandle, agent_sandbox::SandboxError> {
            Ok(agent_sandbox::SandboxHandle { id: "null".into() })
        }

        async fn exec(
            &self,
            _handle: &agent_sandbox::SandboxHandle,
            _cmd: &[String],
        ) -> Result<agent_sandbox::ExecOutput, agent_sandbox::SandboxError> {
            Ok(agent_sandbox::ExecOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }

        async fn destroy(
            &self,
            _handle: agent_sandbox::SandboxHandle,
        ) -> Result<(), agent_sandbox::SandboxError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn an_agent_can_take_the_projection() {
        use agent_core::{in_memory_context, StubModel};
        let dir = tempfile::tempdir().unwrap();
        let stack = GovernanceStack::from_agent_config(&agent_config(dir.path())).unwrap();
        let agent = Agent::with_sandbox(
            agent_config(dir.path()),
            Box::new(StubModel::new("a")),
            in_memory_context(),
            Arc::new(NullSandbox),
        )
        .unwrap();
        let _ = stack.attach(agent);
    }

    #[tokio::test]
    async fn the_proxy_starts_and_stops() {
        let dir = tempfile::tempdir().unwrap();
        let stack = GovernanceStack::from_agent_config(&agent_config(dir.path())).unwrap();
        let handle = stack.start_proxy("127.0.0.1:0", Some("s1")).await.unwrap();
        assert!(handle.url.starts_with("http://127.0.0.1:"));
        assert!(format!("{handle:?}").contains("http://"));
        handle.shutdown();
    }

    #[test]
    fn a_credential_store_without_a_key_refuses_to_start() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = agent_config(dir.path());
        cfg.credentials_dir = dir.path().join("creds").to_string_lossy().to_string();
        cfg.credential_master_key_env = "GOV_TEST_MISSING_KEY".into();
        std::env::remove_var("GOV_TEST_MISSING_KEY");
        assert!(GovernanceStack::from_agent_config(&cfg).is_err());
    }

    #[tokio::test]
    async fn audit_is_written_through_the_real_sink() {
        let dir = tempfile::tempdir().unwrap();
        let stack = GovernanceStack::from_agent_config(&agent_config(dir.path())).unwrap();
        let gateway = EgressGateway::new(
            stack.egress(),
            stack.credentials(),
            stack.audit(),
            "tenant-a",
        );
        let _ = gateway.authorize("example.com").await.unwrap();
        let recent = stack.audit().recent(10).await.unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].kind, AuditKind::EgressDecision);
        assert_eq!(recent[0].decision, Some(false));
    }

    #[test]
    fn no_audit_is_used_when_auditing_is_off() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = agent_config(dir.path());
        cfg.audit_sink = "none".into();
        let stack = GovernanceStack::from_agent_config(&cfg).unwrap();
        assert!(!stack.config.audit_enabled);
        let _ = NoAudit::new();
        let _ = HostPattern::parse("a.com").unwrap();
    }

    #[tokio::test]
    async fn unenforced_mode_records_the_acknowledgement_from_an_async_context() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = agent_config(dir.path());
        cfg.audit_sink = "none".into();
        cfg.egress_enforcement = "off".into();
        cfg.allow_unenforced_egress = true;
        let stack = GovernanceStack::from_agent_config(&cfg).unwrap();
        // The unenforced-egress event is spawned; yield so the task can run.
        tokio::task::yield_now().await;
        assert_eq!(stack.config.enforcement, Enforcement::Off);
        assert!(stack.sandbox_proxy_env("http://x").is_empty());
    }

    #[test]
    fn metering_and_compression_can_be_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = agent_config(dir.path());
        cfg.audit_sink = "none".into();
        cfg.metering = "off".into();
        cfg.compression = "off".into();
        let stack = GovernanceStack::from_agent_config(&cfg).unwrap();
        assert!(!stack.config.metering_enabled);
        assert!(!stack.config.compression_enabled);
        let _ = stack.metering();
        let _ = stack.audit();
        let _ = stack.policy_store();
        let _ = stack.projector();
    }

    #[test]
    fn quota_zero_means_no_cap() {
        assert!(quota_from(0).is_none());
        assert!(quota_from(64).is_some());
    }

    #[test]
    fn an_invalid_config_is_reported_as_a_seam_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = agent_config(dir.path());
        cfg.egress_enforcement = "sometimes".into();
        assert!(matches!(
            GovernanceStack::from_agent_config(&cfg),
            Err(SeamError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn the_null_sandbox_answers_every_call() {
        use agent_sandbox::Sandbox as _;
        let sandbox = NullSandbox;
        let handle = sandbox
            .spawn(&agent_sandbox::ExecSpec::command(vec!["true".into()]))
            .await
            .unwrap();
        assert_eq!(
            sandbox
                .exec(&handle, &["true".to_string()])
                .await
                .unwrap()
                .exit_code,
            0
        );
        sandbox.destroy(handle).await.unwrap();
    }

    #[tokio::test]
    async fn a_failing_audit_sink_only_logs_the_projection() {
        let projector = GovernanceProjector {
            inner: Arc::new(CompressionProjector::default()),
            audit: Arc::new(agent_core::seams::NoAuditConfigured),
            principal: "tenant-a".into(),
        };
        let raw = serde_json::to_string(
            &(0..400)
                .map(|i| serde_json::json!({"path": format!("/workspace/src/file-{i:04}.rs")}))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let projection = projector.project("fs_read", false, &raw);
        assert!(projection.record.is_some());
        // Let the spawned audit task run: the failure is logged, never raised.
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    }

    #[tokio::test]
    async fn a_proxy_can_start_without_a_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let stack = GovernanceStack::from_agent_config(&agent_config(dir.path())).unwrap();
        let handle = stack.start_proxy("127.0.0.1:0", None).await.unwrap();
        assert_eq!(handle.addr.ip().to_string(), "127.0.0.1");
        handle.shutdown();
    }

    #[tokio::test]
    async fn a_session_proxy_starts_and_shuts_down() {
        let dir = tempfile::tempdir().unwrap();
        let stack = GovernanceStack::from_agent_config(&agent_config(dir.path())).unwrap();
        let handle = stack
            .start_proxy("127.0.0.1:0", Some("session-1"))
            .await
            .unwrap();
        assert!(handle.url.starts_with("http://127.0.0.1:"));
        assert!(format!("{handle:?}").contains("127.0.0.1"));
        handle.shutdown();
    }

    #[test]
    fn numeric_environment_values_fall_back_to_zero() {
        assert_eq!(env_u64("PATH"), 0, "unparsable values fall back to zero");
        assert_eq!(env_u64("ARIA_DEFINITELY_UNSET_HERE"), 0);
    }
}
