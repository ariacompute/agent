//! Governance API: egress policy revisions, audit export, usage summaries,
//! credential metadata and checkpoint records.
//!
//! Design constraints taken from the plan (ADR-0021..0025):
//! * a policy write **must** carry the revision it was based on, so a concurrent
//!   writer loses with `409 Conflict` instead of silently overwriting;
//! * reads are always filtered by `principal_id`;
//! * audit can be exported as NDJSON (one JSON object per line), the same schema
//!   the local sink writes;
//! * usage is returned from recorded intervals only — never from token estimates.

use crate::auth::Principal;
use crate::{AppError, AppResult};
use agent_core::seams::{
    AuditEvent, AuditKind, CredentialsSeam, EgressMode, EgressPolicy, HostPattern, MeterState,
    ResourceUsage, SeamError, UsageRecord, UsageSummary,
};
use agent_sandbox::{CheckpointRef, ExecSpec, Sandbox};
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Persistence for governance state. One implementation per deployment.
#[async_trait::async_trait]
pub trait GovernanceStore: Send + Sync {
    async fn policy(&self, principal_id: &str) -> AppResult<EgressPolicy>;

    /// Replace the policy, guarded by `expected_revision` (`None` = create only).
    async fn replace_policy(
        &self,
        principal_id: &str,
        expected_revision: Option<u64>,
        next: EgressPolicy,
    ) -> AppResult<EgressPolicy>;

    async fn append_audit(&self, event: AuditEvent) -> AppResult<()>;

    async fn list_audit(
        &self,
        principal_id: &str,
        limit: usize,
        kind: Option<AuditKind>,
    ) -> AppResult<Vec<AuditEvent>>;

    async fn record_usage(&self, record: UsageRecord) -> AppResult<()>;

    async fn usage(&self, principal_id: &str) -> AppResult<UsageSummary>;

    async fn put_checkpoint(&self, principal_id: &str, checkpoint: CheckpointRef) -> AppResult<()>;

    async fn list_checkpoints(&self, principal_id: &str) -> AppResult<Vec<CheckpointRef>>;

    async fn get_checkpoint(&self, principal_id: &str, id: &str) -> AppResult<CheckpointRef>;
}

/// In-memory store: used by tests and by single-process deployments.
#[derive(Default)]
pub struct MemoryGovernanceStore {
    policies: Mutex<HashMap<String, EgressPolicy>>,
    audit: Mutex<Vec<AuditEvent>>,
    usage: Mutex<Vec<UsageRecord>>,
    checkpoints: Mutex<HashMap<String, Vec<CheckpointRef>>>,
}

impl MemoryGovernanceStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl GovernanceStore for MemoryGovernanceStore {
    async fn policy(&self, principal_id: &str) -> AppResult<EgressPolicy> {
        Ok(self
            .policies
            .lock()
            .await
            .get(principal_id)
            .cloned()
            .unwrap_or_else(EgressPolicy::deny_all))
    }

    async fn replace_policy(
        &self,
        principal_id: &str,
        expected_revision: Option<u64>,
        next: EgressPolicy,
    ) -> AppResult<EgressPolicy> {
        next.validate()
            .map_err(|e| AppError::BadRequest(e.to_string()))?;
        let mut policies = self.policies.lock().await;
        let current = policies
            .get(principal_id)
            .cloned()
            .unwrap_or_else(EgressPolicy::deny_all);
        if let Some(expected) = expected_revision {
            if expected != current.revision {
                return Err(AppError::Conflict(format!(
                    "policy revision conflict: expected r{expected}, current r{}",
                    current.revision
                )));
            }
        }
        let next = EgressPolicy::new(
            current.revision.saturating_add(1),
            next.mode,
            next.allowed,
            next.denied,
        );
        policies.insert(principal_id.to_string(), next.clone());
        Ok(next)
    }

    async fn append_audit(&self, event: AuditEvent) -> AppResult<()> {
        self.audit.lock().await.push(event);
        Ok(())
    }

    async fn list_audit(
        &self,
        principal_id: &str,
        limit: usize,
        kind: Option<AuditKind>,
    ) -> AppResult<Vec<AuditEvent>> {
        let audit = self.audit.lock().await;
        let mut out: Vec<AuditEvent> = audit
            .iter()
            .filter(|e| e.principal_id == principal_id)
            .filter(|e| kind.map(|k| k == e.kind).unwrap_or(true))
            .cloned()
            .collect();
        out.sort_by_key(|e| e.ts_ms);
        let skip = out.len().saturating_sub(limit);
        Ok(out.split_off(skip))
    }

    async fn record_usage(&self, record: UsageRecord) -> AppResult<()> {
        self.usage.lock().await.push(record);
        Ok(())
    }

    async fn usage(&self, principal_id: &str) -> AppResult<UsageSummary> {
        let usage = self.usage.lock().await;
        let mut summary = UsageSummary {
            principal_id: principal_id.to_string(),
            ..UsageSummary::default()
        };
        for record in usage.iter().filter(|r| r.principal_id == principal_id) {
            summary.records += 1;
            summary.resources.merge(&record.resources);
            if let Some(tokens) = record.tokens {
                summary.tokens.input = summary.tokens.input.saturating_add(tokens.input);
                summary.tokens.output = summary.tokens.output.saturating_add(tokens.output);
            }
        }
        Ok(summary)
    }

    async fn put_checkpoint(&self, principal_id: &str, checkpoint: CheckpointRef) -> AppResult<()> {
        checkpoint
            .validate()
            .map_err(|e| AppError::BadRequest(e.to_string()))?;
        self.checkpoints
            .lock()
            .await
            .entry(principal_id.to_string())
            .or_default()
            .push(checkpoint);
        Ok(())
    }

    async fn list_checkpoints(&self, principal_id: &str) -> AppResult<Vec<CheckpointRef>> {
        Ok(self
            .checkpoints
            .lock()
            .await
            .get(principal_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn get_checkpoint(&self, principal_id: &str, id: &str) -> AppResult<CheckpointRef> {
        self.checkpoints
            .lock()
            .await
            .get(principal_id)
            .and_then(|list| list.iter().find(|c| c.id == id).cloned())
            .ok_or(AppError::NotFound)
    }
}

/// Network-enforcement proxy as observed at boot.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProxyInfo {
    pub container: String,
    pub network: String,
    pub image: String,
    /// Boot-time probe result (`running` / `unknown` / the probe error).
    pub health: String,
}

/// Everything the governance routes need.
#[derive(Clone)]
pub struct GovernanceState {
    pub store: Arc<dyn GovernanceStore>,
    pub credentials: Option<Arc<dyn CredentialsSeam>>,
    pub sandbox: Option<Arc<dyn Sandbox>>,
    pub enforcement: String,
    pub compression: bool,
    pub metering: bool,
    pub proxy: Option<ProxyInfo>,
}

/// Process-wide store. `main` installs the Postgres implementation once at boot;
/// tests and single-process deployments get the in-memory one.
static STORE: std::sync::OnceLock<Arc<dyn GovernanceStore>> = std::sync::OnceLock::new();

/// Install the production store. Returns `false` if one is already installed
/// (the first writer wins, so a second boot path cannot silently swap it).
pub fn install_store(store: Arc<dyn GovernanceStore>) -> bool {
    STORE.set(store).is_ok()
}

/// Credential store from the environment, when one is configured.
///
/// The store is local to the deployment (`AGENT_CREDENTIALS_DIR`) and encrypted
/// with `AGENT_CREDENTIAL_MASTER_KEY`; a missing key is reported, never papered
/// over with a plaintext fallback.
pub fn credentials_from_env() -> Option<Arc<dyn CredentialsSeam>> {
    credentials_from(
        std::env::var("AGENT_CREDENTIALS_DIR").ok(),
        std::env::var("AGENT_CREDENTIAL_MASTER_KEY_ENV").ok(),
        std::env::var("AGENT_CREDENTIAL_ROTATE_SECS").ok(),
    )
}

/// Testable half of [`credentials_from_env`]: a blank directory means "no
/// credential store", and a missing root key is reported rather than papered
/// over with a plaintext fallback.
fn credentials_from(
    dir: Option<String>,
    key_env: Option<String>,
    rotate_secs: Option<String>,
) -> Option<Arc<dyn CredentialsSeam>> {
    let dir = dir
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty())?;
    let key_env = key_env
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .unwrap_or_else(|| "AGENT_CREDENTIAL_MASTER_KEY".to_string());
    let rotate = rotate_secs.and_then(|v| v.trim().parse().ok()).unwrap_or(0);
    match agent_governance::FileCredentialStore::open(&dir, &key_env, rotate) {
        Ok(store) => Some(Arc::new(store)),
        Err(e) => {
            tracing::warn!(error = %e, "credential store unavailable; governance endpoints will report it");
            None
        }
    }
}

/// Boot-time proxy facts, installed by `main` after `EgressNetwork::ensure`.
static PROXY_INFO: std::sync::OnceLock<ProxyInfo> = std::sync::OnceLock::new();

pub fn install_proxy_info(info: ProxyInfo) -> bool {
    PROXY_INFO.set(info).is_ok()
}

pub fn global_store() -> Arc<dyn GovernanceStore> {
    STORE
        .get_or_init(|| Arc::new(MemoryGovernanceStore::new()))
        .clone()
}

impl GovernanceState {
    /// State backed by the process-wide store.
    pub fn from_global(enforcement: impl Into<String>, compression: bool, metering: bool) -> Self {
        Self {
            store: global_store(),
            credentials: None,
            sandbox: None,
            enforcement: enforcement.into(),
            compression,
            metering,
            proxy: PROXY_INFO.get().cloned(),
        }
    }

    /// Attach the credential store (metadata + rotation endpoints).
    pub fn with_credentials(mut self, credentials: Arc<dyn CredentialsSeam>) -> Self {
        self.credentials = Some(credentials);
        self
    }

    /// Attach the sandbox backend used for checkpoint create/restore.
    pub fn with_sandbox(mut self, sandbox: Arc<dyn Sandbox>) -> Self {
        self.sandbox = Some(sandbox);
        self
    }

    pub fn new(store: Arc<dyn GovernanceStore>) -> Self {
        Self {
            store,
            credentials: None,
            sandbox: None,
            enforcement: "proxy".into(),
            compression: true,
            metering: true,
            proxy: None,
        }
    }
}

/// Mount under the authenticated router (the auth middleware inserts `Principal`).
pub fn router(state: GovernanceState) -> Router {
    Router::new()
        .route(
            "/v1/governance/egress-policy",
            get(get_policy).put(put_policy).patch(patch_policy),
        )
        .route("/v1/governance/audit", get(list_audit))
        .route("/v1/governance/audit/export", get(export_audit))
        .route("/v1/governance/usage", get(get_usage))
        .route("/v1/governance/status", get(get_status))
        .route("/v1/governance/credentials", get(list_credentials))
        .route(
            "/v1/governance/credentials/:name",
            put(put_credential).delete(delete_credential),
        )
        .route(
            "/v1/governance/checkpoints",
            get(list_checkpoints).post(create_checkpoint),
        )
        .route(
            "/v1/governance/checkpoints/:id/restore",
            post(restore_checkpoint),
        )
        .with_state(state)
}

fn require_read(principal: &Principal) -> AppResult<()> {
    if principal.scopes.read || principal.is_admin() {
        Ok(())
    } else {
        Err(AppError::Forbidden(
            "governance:read requires a key with `read` scope".into(),
        ))
    }
}

fn require_manage(principal: &Principal) -> AppResult<()> {
    if principal.scopes.write || principal.is_admin() {
        Ok(())
    } else {
        Err(AppError::Forbidden(
            "governance:manage requires a key with `write` scope".into(),
        ))
    }
}

fn parse_hosts(raw: &[String]) -> AppResult<Vec<HostPattern>> {
    raw.iter()
        .map(|h| HostPattern::parse(h).map_err(|e| AppError::BadRequest(format!("{h}: {e}"))))
        .collect()
}

#[derive(Debug, Deserialize)]
pub struct PolicyBody {
    pub mode: String,
    #[serde(default)]
    pub allowed: Vec<String>,
    #[serde(default)]
    pub denied: Vec<String>,
    /// Required for `PUT` (and absent only for the first write).
    #[serde(default)]
    pub expected_revision: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct PatchBody {
    pub expected_revision: u64,
    #[serde(default)]
    pub allow_add: Vec<String>,
    #[serde(default)]
    pub allow_remove: Vec<String>,
    #[serde(default)]
    pub deny_add: Vec<String>,
    #[serde(default)]
    pub deny_remove: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct PolicyResponse {
    pub revision: u64,
    pub mode: String,
    pub allowed: Vec<String>,
    pub denied: Vec<String>,
}

impl From<EgressPolicy> for PolicyResponse {
    fn from(p: EgressPolicy) -> Self {
        Self {
            revision: p.revision,
            mode: p.mode.as_str().to_string(),
            allowed: p.allowed.iter().map(|h| h.as_str().to_string()).collect(),
            denied: p.denied.iter().map(|h| h.as_str().to_string()).collect(),
        }
    }
}

pub async fn get_policy(
    State(state): State<GovernanceState>,
    Extension(principal): Extension<Principal>,
) -> AppResult<Json<PolicyResponse>> {
    require_read(&principal)?;
    let policy = state.store.policy(&principal.id).await?;
    Ok(Json(policy.into()))
}

pub async fn put_policy(
    State(state): State<GovernanceState>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<PolicyBody>,
) -> AppResult<Json<PolicyResponse>> {
    require_manage(&principal)?;
    let mode = EgressMode::parse(&body.mode)
        .ok_or_else(|| AppError::BadRequest(format!("unknown egress mode: {}", body.mode)))?;
    let next = EgressPolicy::new(
        0,
        mode,
        parse_hosts(&body.allowed)?,
        parse_hosts(&body.denied)?,
    );
    let stored = state
        .store
        .replace_policy(&principal.id, body.expected_revision, next)
        .await?;
    Ok(Json(stored.into()))
}

pub async fn patch_policy(
    State(state): State<GovernanceState>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<PatchBody>,
) -> AppResult<Json<PolicyResponse>> {
    require_manage(&principal)?;
    let current = state.store.policy(&principal.id).await?;
    if current.revision != body.expected_revision {
        return Err(AppError::Conflict(format!(
            "policy revision conflict: expected r{}, current r{}",
            body.expected_revision, current.revision
        )));
    }
    let mut allowed: Vec<HostPattern> = current.allowed.clone();
    let mut denied: Vec<HostPattern> = current.denied.clone();
    let add = parse_hosts(&body.allow_add)?;
    let remove = parse_hosts(&body.allow_remove)?;
    // Iterator form: hosts already present are skipped, and the list keeps its
    // order (matching only needs membership, so duplicates are harmless).
    let mut extra: Vec<HostPattern> = add
        .into_iter()
        .filter(|host| !allowed.contains(host))
        .collect();
    allowed.append(&mut extra);
    allowed.retain(|h| !remove.contains(h));
    let add = parse_hosts(&body.deny_add)?;
    let remove = parse_hosts(&body.deny_remove)?;
    let mut extra: Vec<HostPattern> = add
        .into_iter()
        .filter(|host| !denied.contains(host))
        .collect();
    denied.append(&mut extra);
    denied.retain(|h| !remove.contains(h));

    let next = EgressPolicy::new(0, current.mode, allowed, denied);
    let stored = state
        .store
        .replace_policy(&principal.id, Some(body.expected_revision), next)
        .await?;
    Ok(Json(stored.into()))
}

#[derive(Debug, Deserialize)]
pub struct AuditQuery {
    #[serde(default = "default_limit")]
    pub limit: usize,
    pub kind: Option<String>,
}

fn default_limit() -> usize {
    100
}

pub async fn list_audit(
    State(state): State<GovernanceState>,
    Extension(principal): Extension<Principal>,
    Query(query): Query<AuditQuery>,
) -> AppResult<Json<Vec<AuditEvent>>> {
    require_read(&principal)?;
    let kind = match query.kind.as_deref() {
        Some(raw) => Some(
            serde_json::from_value::<AuditKind>(serde_json::Value::String(raw.to_string()))
                .map_err(|_| AppError::BadRequest(format!("unknown audit kind: {raw}")))?,
        ),
        None => None,
    };
    let limit = query.limit.clamp(1, 1000);
    let events = state.store.list_audit(&principal.id, limit, kind).await?;
    Ok(Json(events))
}

pub async fn export_audit(
    State(state): State<GovernanceState>,
    Extension(principal): Extension<Principal>,
    Query(query): Query<AuditQuery>,
) -> AppResult<Response> {
    require_read(&principal)?;
    let events = state
        .store
        .list_audit(&principal.id, query.limit.clamp(1, 10_000), None)
        .await?;
    let mut body = String::new();
    for event in events {
        // `AuditEvent` is a plain struct with string/number fields: serialising
        // it cannot fail, so a map_err here would only be dead code.
        let line = serde_json::to_string(&event).expect("audit events are serialisable");
        body.push_str(&line);
        body.push('\n');
    }
    Ok(([(header::CONTENT_TYPE, "application/x-ndjson")], body).into_response())
}

pub async fn get_usage(
    State(state): State<GovernanceState>,
    Extension(principal): Extension<Principal>,
) -> AppResult<Json<UsageSummary>> {
    require_read(&principal)?;
    Ok(Json(state.store.usage(&principal.id).await?))
}

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub enforcement: String,
    pub compression: bool,
    pub metering: bool,
    pub credentials_configured: bool,
    pub sandbox_configured: bool,
    /// Present only when network enforcement is on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy: Option<ProxyInfo>,
}

pub async fn get_status(
    State(state): State<GovernanceState>,
    Extension(principal): Extension<Principal>,
) -> AppResult<Json<StatusResponse>> {
    require_read(&principal)?;
    Ok(Json(StatusResponse {
        enforcement: state.enforcement.clone(),
        compression: state.compression,
        metering: state.metering,
        credentials_configured: state.credentials.is_some(),
        sandbox_configured: state.sandbox.is_some(),
        proxy: state.proxy.clone(),
    }))
}

#[derive(Debug, Serialize)]
pub struct CredentialMetaResponse {
    pub name: String,
    pub version: u64,
    pub updated_at_ms: u64,
    pub rotate_after_secs: Option<u64>,
}

pub async fn list_credentials(
    State(state): State<GovernanceState>,
    Extension(principal): Extension<Principal>,
) -> AppResult<Json<Vec<CredentialMetaResponse>>> {
    require_read(&principal)?;
    let store = state
        .credentials
        .as_ref()
        .ok_or_else(|| AppError::BadRequest("no credential store is configured".into()))?;
    let metas = store
        .list()
        .await
        .map_err(|e| AppError::BadRequest(e.to_string()))?;
    Ok(Json(
        metas
            .into_iter()
            .map(|m| CredentialMetaResponse {
                name: m.name,
                version: m.version,
                updated_at_ms: m.updated_at_ms,
                rotate_after_secs: m.rotate_after_secs,
            })
            .collect(),
    ))
}

#[derive(Debug, Deserialize)]
pub struct CredentialBody {
    pub value: String,
}

pub async fn put_credential(
    State(state): State<GovernanceState>,
    Extension(principal): Extension<Principal>,
    Path(name): Path<String>,
    Json(body): Json<CredentialBody>,
) -> AppResult<Json<CredentialMetaResponse>> {
    require_manage(&principal)?;
    let store = state
        .credentials
        .as_ref()
        .ok_or_else(|| AppError::BadRequest("no credential store is configured".into()))?;
    let meta = store
        .rotate(&name, agent_core::seams::SecretValue::new(body.value))
        .await
        .map_err(|e| AppError::BadRequest(e.to_string()))?;
    Ok(Json(CredentialMetaResponse {
        name: meta.name,
        version: meta.version,
        updated_at_ms: meta.updated_at_ms,
        rotate_after_secs: meta.rotate_after_secs,
    }))
}

pub async fn delete_credential(
    State(state): State<GovernanceState>,
    Extension(principal): Extension<Principal>,
    Path(name): Path<String>,
) -> AppResult<axum::http::StatusCode> {
    require_manage(&principal)?;
    let store = state
        .credentials
        .as_ref()
        .ok_or_else(|| AppError::BadRequest("no credential store is configured".into()))?;
    store
        .delete(&name)
        .await
        .map_err(|e| AppError::BadRequest(e.to_string()))?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct CreateCheckpointBody {
    pub sandbox_id: String,
}

pub async fn create_checkpoint(
    State(state): State<GovernanceState>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<CreateCheckpointBody>,
) -> AppResult<Json<CheckpointRef>> {
    require_manage(&principal)?;
    let sandbox = state
        .sandbox
        .as_ref()
        .ok_or_else(|| AppError::BadRequest("no sandbox backend is configured".into()))?;
    let handle = agent_sandbox::SandboxHandle {
        id: body.sandbox_id,
    };
    let checkpoint = sandbox.checkpoint(&handle).await?;
    state
        .store
        .put_checkpoint(&principal.id, checkpoint.clone())
        .await?;
    Ok(Json(checkpoint))
}

pub async fn list_checkpoints(
    State(state): State<GovernanceState>,
    Extension(principal): Extension<Principal>,
) -> AppResult<Json<Vec<CheckpointRef>>> {
    require_read(&principal)?;
    Ok(Json(state.store.list_checkpoints(&principal.id).await?))
}

/// Spec used to derive a sandbox from a checkpoint.
///
/// Under network enforcement the derived container must join the governed
/// network too, otherwise restore would be a way back onto the default bridge
/// (and the proxy could not map its IP to a tenant). The tenant label is the
/// authenticated principal — never a value from the request body.
fn restore_spec(
    checkpoint_id: &str,
    principal_id: &str,
    enforcement: &str,
    network: Option<String>,
    proxy_url: Option<String>,
) -> AppResult<ExecSpec> {
    let mut spec = ExecSpec::command(vec!["sleep".into(), "3600".into()]);
    // The proxy environment applies in both modes: in `network` mode the proxy is
    // the only route out, and in `proxy` mode it is what keeps a restored sandbox
    // on the audited path instead of silently going direct.
    spec.env = proxy_url
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .map(|url| agent_governance::proxy_env(&url))
        .unwrap_or_default();
    if enforcement.trim() != "network" {
        return Ok(spec);
    }
    let network = network
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            AppError::BadRequest(
                "network enforcement requires AGENT_EGRESS_NETWORK to restore a checkpoint".into(),
            )
        })?;
    spec.network = Some(network);
    spec.labels = vec![
        ("aria.tenant_id".to_string(), principal_id.to_string()),
        (
            "aria.session_id".to_string(),
            format!("restore::{checkpoint_id}"),
        ),
    ];
    Ok(spec)
}

pub async fn restore_checkpoint(
    State(state): State<GovernanceState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    require_manage(&principal)?;
    let checkpoint = state.store.get_checkpoint(&principal.id, &id).await?;
    let sandbox = state
        .sandbox
        .as_ref()
        .ok_or_else(|| AppError::BadRequest("no sandbox backend is configured".into()))?;
    // Restoring always derives a new sandbox; the quota gate is applied by the
    // wrapped backend (QuotaGuardSandbox), so this path cannot bypass it.
    let spec = restore_spec(
        &id,
        &principal.id,
        &state.enforcement,
        std::env::var("AGENT_EGRESS_NETWORK").ok(),
        std::env::var("AGENT_EGRESS_PROXY_URL").ok(),
    )?;
    let handle = sandbox.restore(&checkpoint, &spec).await?;
    Ok(Json(serde_json::json!({
        "sandbox_id": handle.id,
        "checkpoint_id": checkpoint.id,
        "derived": true,
    })))
}

/// Build a usage record for the API/tests. Kept here so the state split is
/// applied in exactly one place on the server side.
pub fn usage_record(
    principal_id: &str,
    sandbox_id: &str,
    state: MeterState,
    shape: agent_core::seams::ResourceRequest,
    started_at_ms: u64,
    ended_at_ms: u64,
) -> UsageRecord {
    let duration = ended_at_ms.saturating_sub(started_at_ms);
    UsageRecord::new(
        principal_id,
        sandbox_id,
        state,
        started_at_ms,
        ended_at_ms,
        ResourceUsage::for_state(
            state,
            shape.vcpus_x1000,
            shape.memory_mib,
            shape.disk_mib,
            duration,
        ),
    )
}

/// Convenience for tests and for the boot path: a valid default policy.
pub fn default_policy() -> EgressPolicy {
    EgressPolicy::deny_all()
}

impl From<SeamError> for AppError {
    fn from(e: SeamError) -> Self {
        AppError::BadRequest(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{ApiKeyScopes, PrincipalKind};
    use agent_core::seams::now_ms;

    fn principal(scopes: ApiKeyScopes) -> Principal {
        Principal {
            id: "tenant-a".into(),
            kind: PrincipalKind::User,
            scopes,
        }
    }

    fn state() -> GovernanceState {
        GovernanceState::new(Arc::new(MemoryGovernanceStore::new()))
    }

    #[tokio::test]
    async fn an_unset_policy_reads_as_deny_all() {
        let result = get_policy(
            State(state()),
            Extension(principal(ApiKeyScopes::read_write())),
        )
        .await
        .unwrap();
        assert_eq!(result.0.mode, "deny_all");
        assert_eq!(result.0.revision, 1);
    }

    #[tokio::test]
    async fn policy_writes_are_revision_guarded() {
        let state = state();
        let who = principal(ApiKeyScopes::read_write());
        let first = put_policy(
            State(state.clone()),
            Extension(who.clone()),
            Json(PolicyBody {
                mode: "allowlist".into(),
                allowed: vec!["api.example.com".into()],
                denied: vec![],
                expected_revision: Some(1),
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(first.revision, 2);

        let stale = put_policy(
            State(state.clone()),
            Extension(who.clone()),
            Json(PolicyBody {
                mode: "deny_all".into(),
                allowed: vec![],
                denied: vec![],
                expected_revision: Some(1),
            }),
        )
        .await;
        assert!(matches!(stale, Err(AppError::Conflict(_))));

        let patched = patch_policy(
            State(state),
            Extension(who.clone()),
            Json(PatchBody {
                expected_revision: 2,
                allow_add: vec!["*.pypi.org".into()],
                allow_remove: vec!["api.example.com".into()],
                deny_add: vec![],
                deny_remove: vec![],
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(patched.revision, 3);
        assert_eq!(patched.allowed, vec!["*.pypi.org".to_string()]);
    }

    #[tokio::test]
    async fn invalid_modes_and_hosts_are_rejected() {
        let who = principal(ApiKeyScopes::read_write());
        let bad_mode = put_policy(
            State(state()),
            Extension(who.clone()),
            Json(PolicyBody {
                mode: "open".into(),
                allowed: vec![],
                denied: vec![],
                expected_revision: None,
            }),
        )
        .await;
        assert!(matches!(bad_mode, Err(AppError::BadRequest(_))));

        let bad_host = put_policy(
            State(state()),
            Extension(who.clone()),
            Json(PolicyBody {
                mode: "allowlist".into(),
                allowed: vec!["https://x".into()],
                denied: vec![],
                expected_revision: None,
            }),
        )
        .await;
        assert!(matches!(bad_host, Err(AppError::BadRequest(_))));

        let empty_allowlist = put_policy(
            State(state()),
            Extension(who.clone()),
            Json(PolicyBody {
                mode: "allowlist".into(),
                allowed: vec![],
                denied: vec![],
                expected_revision: None,
            }),
        )
        .await;
        assert!(matches!(empty_allowlist, Err(AppError::BadRequest(_))));
    }

    #[tokio::test]
    async fn reads_need_read_scope_and_writes_need_write_scope() {
        let readonly = principal(ApiKeyScopes {
            admin: false,
            read: true,
            write: false,
        });
        assert!(get_policy(State(state()), Extension(readonly.clone()))
            .await
            .is_ok());
        assert!(matches!(
            put_policy(
                State(state()),
                Extension(readonly.clone()),
                Json(PolicyBody {
                    mode: "deny_all".into(),
                    allowed: vec![],
                    denied: vec![],
                    expected_revision: None,
                })
            )
            .await,
            Err(AppError::Forbidden(_))
        ));

        let blind = principal(ApiKeyScopes::default());
        assert!(matches!(
            get_policy(State(state()), Extension(blind)).await,
            Err(AppError::Forbidden(_))
        ));
    }

    #[tokio::test]
    async fn audit_lists_and_exports_as_ndjson() {
        let state = state();
        let who = principal(ApiKeyScopes::read_write());
        for target in ["a.com", "b.com"] {
            state
                .store
                .append_audit(
                    AuditEvent::new(AuditKind::EgressDecision, "tenant-a", target)
                        .with_decision(false),
                )
                .await
                .unwrap();
        }
        state
            .store
            .append_audit(AuditEvent::new(AuditKind::ToolExecuted, "other", "x"))
            .await
            .unwrap();

        let listed = list_audit(
            State(state.clone()),
            Extension(who.clone()),
            Query(AuditQuery {
                limit: 10,
                kind: None,
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(listed.len(), 2, "other tenants do not leak in");

        let filtered = list_audit(
            State(state.clone()),
            Extension(who.clone()),
            Query(AuditQuery {
                limit: 10,
                kind: Some("tool_executed".into()),
            }),
        )
        .await
        .unwrap()
        .0;
        assert!(filtered.is_empty());

        let bad_kind = list_audit(
            State(state.clone()),
            Extension(who.clone()),
            Query(AuditQuery {
                limit: 10,
                kind: Some("nope".into()),
            }),
        )
        .await;
        assert!(matches!(bad_kind, Err(AppError::BadRequest(_))));

        let export = export_audit(
            State(state),
            Extension(who.clone()),
            Query(AuditQuery {
                limit: 10,
                kind: None,
            }),
        )
        .await
        .unwrap();
        let body = axum::body::to_bytes(export.into_body(), 64 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.lines().all(|l| l.starts_with('{')));
    }

    #[tokio::test]
    async fn usage_splits_billing_by_state() {
        let state = state();
        let who = principal(ApiKeyScopes::read_write());
        let shape = agent_core::seams::ResourceRequest {
            vcpus_x1000: 1000,
            memory_mib: 1024,
            disk_mib: 0,
        };
        state
            .store
            .record_usage(usage_record(
                "tenant-a",
                "sbx",
                MeterState::Running,
                shape,
                0,
                3_600_000,
            ))
            .await
            .unwrap();
        state
            .store
            .record_usage(usage_record(
                "tenant-a",
                "sbx",
                MeterState::Paused,
                shape,
                3_600_000,
                7_200_000,
            ))
            .await
            .unwrap();
        let summary = get_usage(State(state), Extension(who)).await.unwrap().0;
        assert_eq!(summary.records, 2);
        assert_eq!(summary.resources.vcpu_millis, 3_600_000);
        assert_eq!(summary.resources.memory_mib_millis, 2 * 1024 * 3_600_000);
    }

    #[tokio::test]
    async fn credentials_report_metadata_only() {
        let state = state();
        let who = principal(ApiKeyScopes::read_write());
        assert!(matches!(
            list_credentials(State(state.clone()), Extension(who.clone())).await,
            Err(AppError::BadRequest(_))
        ));
        assert!(matches!(
            put_credential(
                State(state.clone()),
                Extension(who.clone()),
                Path("openai".into()),
                Json(CredentialBody { value: "v".into() })
            )
            .await,
            Err(AppError::BadRequest(_))
        ));
        assert!(matches!(
            delete_credential(State(state), Extension(who), Path("openai".into())).await,
            Err(AppError::BadRequest(_))
        ));
    }

    #[tokio::test]
    async fn checkpoints_round_trip_and_restore_needs_a_backend() {
        let state = state();
        let who = principal(ApiKeyScopes::read_write());
        let checkpoint = CheckpointRef::new("img:latest", "sbx", now_ms());
        state
            .store
            .put_checkpoint("tenant-a", checkpoint.clone())
            .await
            .unwrap();
        let listed = list_checkpoints(State(state.clone()), Extension(who.clone()))
            .await
            .unwrap()
            .0;
        assert_eq!(listed.len(), 1);

        assert!(matches!(
            restore_checkpoint(
                State(state.clone()),
                Extension(who.clone()),
                Path(checkpoint.id.clone())
            )
            .await,
            Err(AppError::BadRequest(_))
        ));
        assert!(matches!(
            restore_checkpoint(State(state), Extension(who), Path("missing".into())).await,
            Err(AppError::NotFound)
        ));
    }

    #[tokio::test]
    async fn status_reports_what_is_actually_wired() {
        let status = get_status(
            State(state()),
            Extension(principal(ApiKeyScopes::read_write())),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(status.enforcement, "proxy");
        assert!(status.compression);
        assert!(!status.credentials_configured);
        assert!(!status.sandbox_configured);
    }

    #[test]
    fn default_policy_is_deny_all() {
        assert_eq!(default_policy().mode, EgressMode::DenyAll);
    }

    #[test]
    fn restore_spec_follows_the_enforcement_mode() {
        // Proxy mode: nothing extra — the historical behaviour.
        let spec = restore_spec("cp", "tenant-a", "proxy", None, None).unwrap();
        assert!(spec.network.is_none());
        assert!(spec.labels.is_empty());
        assert!(spec.env.is_empty());

        // Network mode without a network name is a hard error, never a
        // default-bridge restore.
        assert!(matches!(
            restore_spec("cp", "tenant-a", "network", None, None),
            Err(AppError::BadRequest(_))
        ));
        assert!(matches!(
            restore_spec("cp", "tenant-a", "network", Some("  ".into()), None),
            Err(AppError::BadRequest(_))
        ));

        let spec = restore_spec(
            "cp-1",
            "tenant-a",
            "network",
            Some(" aria-egress ".into()),
            Some(" http://aria-egress-proxy:8080 ".into()),
        )
        .unwrap();
        assert_eq!(spec.network.as_deref(), Some("aria-egress"));
        assert_eq!(
            spec.labels,
            vec![
                ("aria.tenant_id".to_string(), "tenant-a".to_string()),
                ("aria.session_id".to_string(), "restore::cp-1".to_string()),
            ]
        );
        assert!(spec
            .env
            .iter()
            .any(|(k, v)| k == "HTTP_PROXY" && v == "http://aria-egress-proxy:8080"));
    }

    use agent_core::seams::{SeamResult, SecretMeta, SecretStub, SecretValue, SubstitutionReport};

    /// Credentials seam with controllable failures (the file store is covered by
    /// its own crate; these tests are about the HTTP handlers).
    struct FakeCredentials {
        fail: bool,
    }

    impl FakeCredentials {
        fn ok() -> Arc<dyn CredentialsSeam> {
            Arc::new(Self { fail: false })
        }
        fn failing() -> Arc<dyn CredentialsSeam> {
            Arc::new(Self { fail: true })
        }
    }

    #[async_trait::async_trait]
    impl CredentialsSeam for FakeCredentials {
        async fn list(&self) -> SeamResult<Vec<SecretMeta>> {
            if self.fail {
                return Err(SeamError::Io("list failed".into()));
            }
            Ok(vec![SecretMeta {
                name: "openai".into(),
                version: 2,
                updated_at_ms: 1,
                rotate_after_secs: Some(30),
            }])
        }
        async fn resolve(&self, name: &str) -> SeamResult<SecretValue> {
            Err(SeamError::NotFound(name.into()))
        }
        async fn rotate(&self, name: &str, _value: SecretValue) -> SeamResult<SecretMeta> {
            if self.fail {
                return Err(SeamError::Io("rotate failed".into()));
            }
            Ok(SecretMeta {
                name: name.into(),
                version: 3,
                updated_at_ms: 2,
                rotate_after_secs: None,
            })
        }
        async fn delete(&self, _name: &str) -> SeamResult<()> {
            if self.fail {
                return Err(SeamError::Io("delete failed".into()));
            }
            Ok(())
        }
        async fn stubs(&self) -> SeamResult<Vec<SecretStub>> {
            Ok(Vec::new())
        }
        async fn put_stub(&self, _stub: SecretStub) -> SeamResult<()> {
            Err(SeamError::Unsupported("read-only".into()))
        }
        async fn remove_stub(&self, _id: &str) -> SeamResult<()> {
            Err(SeamError::Unsupported("read-only".into()))
        }
        async fn substitute(
            &self,
            _host: &str,
            _path: &str,
            _headers: &mut Vec<(String, String)>,
        ) -> SeamResult<SubstitutionReport> {
            Ok(SubstitutionReport::default())
        }
    }

    struct StubSandbox;

    #[async_trait::async_trait]
    impl Sandbox for StubSandbox {
        async fn spawn(
            &self,
            _spec: &ExecSpec,
        ) -> Result<agent_sandbox::SandboxHandle, agent_sandbox::SandboxError> {
            Ok(agent_sandbox::SandboxHandle { id: "sbx".into() })
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
        async fn checkpoint(
            &self,
            handle: &agent_sandbox::SandboxHandle,
        ) -> Result<CheckpointRef, agent_sandbox::SandboxError> {
            Ok(CheckpointRef::new("img:latest", &handle.id, now_ms()))
        }
        async fn restore(
            &self,
            checkpoint: &CheckpointRef,
            _spec: &ExecSpec,
        ) -> Result<agent_sandbox::SandboxHandle, agent_sandbox::SandboxError> {
            Ok(agent_sandbox::SandboxHandle {
                id: format!("{}::restored", checkpoint.id),
            })
        }
    }

    #[tokio::test]
    async fn patch_conflicts_and_duplicate_denies_are_handled() {
        let state = state();
        let who = principal(ApiKeyScopes::read_write());
        let stale = patch_policy(
            State(state.clone()),
            Extension(who.clone()),
            Json(PatchBody {
                expected_revision: 99,
                allow_add: Vec::new(),
                allow_remove: Vec::new(),
                deny_add: Vec::new(),
                deny_remove: Vec::new(),
            }),
        )
        .await;
        assert!(matches!(stale, Err(AppError::Conflict(_))));

        let patched = patch_policy(
            State(state),
            Extension(who),
            Json(PatchBody {
                expected_revision: 1,
                allow_add: Vec::new(),
                allow_remove: Vec::new(),
                deny_add: vec!["evil.com".into(), "evil.com".into()],
                deny_remove: Vec::new(),
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(patched.denied, vec!["evil.com".to_string()]);
    }

    #[tokio::test]
    async fn credential_endpoints_report_metas_and_store_failures() {
        let who = principal(ApiKeyScopes::read_write());
        assert_eq!(default_limit(), 100);

        let state = state().with_credentials(FakeCredentials::ok());
        let listed = list_credentials(State(state.clone()), Extension(who.clone()))
            .await
            .unwrap()
            .0;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "openai");
        assert_eq!(listed[0].rotate_after_secs, Some(30));

        let put = put_credential(
            State(state.clone()),
            Extension(who.clone()),
            Path("openai".into()),
            Json(CredentialBody {
                value: "sk-live".into(),
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(put.version, 3);
        assert_eq!(
            delete_credential(State(state), Extension(who.clone()), Path("openai".into()))
                .await
                .unwrap(),
            axum::http::StatusCode::NO_CONTENT
        );

        let broken = self::state().with_credentials(FakeCredentials::failing());
        assert!(matches!(
            list_credentials(State(broken.clone()), Extension(who.clone())).await,
            Err(AppError::BadRequest(_))
        ));
        assert!(matches!(
            put_credential(
                State(broken.clone()),
                Extension(who.clone()),
                Path("openai".into()),
                Json(CredentialBody { value: "x".into() })
            )
            .await,
            Err(AppError::BadRequest(_))
        ));
        assert!(matches!(
            delete_credential(State(broken), Extension(who), Path("openai".into())).await,
            Err(AppError::BadRequest(_))
        ));
    }

    #[tokio::test]
    async fn checkpoints_need_a_sandbox_and_convert_seam_errors() {
        let who = principal(ApiKeyScopes::read_write());
        assert!(matches!(
            create_checkpoint(
                State(state()),
                Extension(who),
                Json(CreateCheckpointBody {
                    sandbox_id: "sbx".into()
                })
            )
            .await,
            Err(AppError::BadRequest(_))
        ));
        let converted: AppError = SeamError::Io("boom".into()).into();
        assert!(matches!(converted, AppError::BadRequest(_)));
    }

    #[tokio::test]
    async fn checkpoints_are_created_and_restored_through_the_sandbox() {
        let state = state().with_sandbox(Arc::new(StubSandbox));
        let who = principal(ApiKeyScopes::read_write());
        // The stub's other trait methods are exercised so the fake is honest.
        let sandbox = StubSandbox;
        let spawn = sandbox.spawn(&ExecSpec::command(vec!["true".into()])).await;
        let handle = spawn.unwrap();
        assert_eq!(
            sandbox
                .exec(&handle, &["true".to_string()])
                .await
                .unwrap()
                .exit_code,
            0
        );
        sandbox.destroy(handle).await.unwrap();
        let checkpoint = create_checkpoint(
            State(state.clone()),
            Extension(who.clone()),
            Json(CreateCheckpointBody {
                sandbox_id: "sbx-1".into(),
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(checkpoint.source, "sbx-1");

        let restored =
            restore_checkpoint(State(state), Extension(who), Path(checkpoint.id.clone()))
                .await
                .unwrap()
                .0;
        assert_eq!(
            restored["sandbox_id"],
            format!("{}::restored", checkpoint.id)
        );
        assert_eq!(restored["derived"], true);
    }

    #[tokio::test]
    async fn invalid_checkpoints_are_refused_by_the_store() {
        let broken = CheckpointRef::new("  ", "sbx", now_ms());
        assert!(matches!(
            state().store.put_checkpoint("tenant-a", broken).await,
            Err(AppError::BadRequest(_))
        ));
    }

    #[tokio::test]
    async fn boot_installers_are_first_writer_wins() {
        let store: Arc<dyn GovernanceStore> = Arc::new(MemoryGovernanceStore::new());
        // Another test may already have initialised the process-wide store (the
        // router does), so the assertion is about the second writer losing.
        install_store(store.clone());
        assert!(!install_store(store), "the first writer wins");
        let info = ProxyInfo {
            container: "aria-egress-proxy".into(),
            network: "aria-egress".into(),
            image: "img".into(),
            health: "running".into(),
        };
        install_proxy_info(info.clone());
        assert!(!install_proxy_info(info), "the first writer wins");
        assert_eq!(
            global_store().policy("x").await.unwrap().mode,
            EgressMode::DenyAll
        );
    }

    #[test]
    fn the_optional_seams_attach_to_the_state() {
        let gov = state()
            .with_credentials(FakeCredentials::ok())
            .with_sandbox(Arc::new(StubSandbox));
        assert!(gov.credentials.is_some());
        assert!(gov.sandbox.is_some());
    }

    #[test]
    fn a_credential_store_needs_a_directory_and_a_root_key() {
        assert!(credentials_from(None, None, None).is_none());
        assert!(credentials_from(Some("  ".into()), None, None).is_none());

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds").to_string_lossy().to_string();
        std::env::remove_var("ARIA_COV_MASTER_KEY");
        // Without the root key the store is reported as unavailable — never a
        // plaintext fallback.
        assert!(credentials_from(
            Some(path.clone()),
            Some("ARIA_COV_MASTER_KEY".into()),
            Some("30".into())
        )
        .is_none());

        std::env::set_var("ARIA_COV_MASTER_KEY", "a-long-enough-master-key-for-tests");
        // A malformed rotation value falls back to zero instead of failing.
        assert!(credentials_from(
            Some(path.clone()),
            Some("ARIA_COV_MASTER_KEY".into()),
            Some("not-a-number".into())
        )
        .is_some());
        std::env::remove_var("ARIA_COV_MASTER_KEY");

        // No key-env override: the documented default name is used.
        std::env::remove_var("AGENT_CREDENTIAL_MASTER_KEY");
        assert!(credentials_from(Some(path), None, None).is_none());
    }

    #[tokio::test]
    async fn the_test_credential_store_never_resolves_or_writes() {
        let credentials = FakeCredentials { fail: false };
        assert!(matches!(
            credentials.resolve("openai").await,
            Err(SeamError::NotFound(_))
        ));
        assert!(credentials.stubs().await.unwrap().is_empty());
        assert!(matches!(
            credentials
                .put_stub(SecretStub {
                    id: "s".into(),
                    host: HostPattern::parse("a.com").unwrap(),
                    path_prefix: None,
                    credential: "c".into(),
                    header: "authorization".into(),
                    value_template: "Bearer ${credential}".into(),
                })
                .await,
            Err(SeamError::Unsupported(_))
        ));
        assert!(matches!(
            credentials.remove_stub("s").await,
            Err(SeamError::Unsupported(_))
        ));
        let mut headers = Vec::new();
        assert!(credentials
            .substitute("a.com", "/", &mut headers)
            .await
            .is_ok());
    }
}
