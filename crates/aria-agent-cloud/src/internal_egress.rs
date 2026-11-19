//! Internal API for the network-enforcement proxy (ADR-0024).
//!
//! These routes are **not** part of the tenant API. They are authenticated with a
//! shared token (`EGRESS_PROXY_TOKEN`) and must only be reachable on the control
//! plane's Docker network.
//!
//! The proxy holds no policy and no secrets; it asks here. Three rules make that
//! safe:
//!
//! 1. the token is compared in constant time and never logged;
//! 2. a request names its peer IP, and the principal it asks about **must** be the
//!    one the Docker labels attribute to that IP — a compromised proxy cannot read
//!    another tenant's policy by guessing an id;
//! 3. an unknown peer resolves to `None` → `404`, never to a default tenant.

use crate::governance::GovernanceStore;
use crate::{AppError, AppResult};
use agent_core::seams::{AuditEvent, CredentialsSeam, EgressPolicy, SecretStub};
use agent_sandbox::{DockerSandbox, EgressConfig, EgressNetwork, PeerIdentity, SandboxError};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Header carrying the shared token.
pub const TOKEN_HEADER: &str = "x-aria-proxy-token";

/// Resolves a source IP to the sandbox that owns it.
#[async_trait::async_trait]
pub trait PeerLookup: Send + Sync {
    async fn resolve(&self, ip: &str) -> AppResult<Option<PeerIdentity>>;
}

/// Docker-backed lookup: labels written at spawn time are the only identity
/// source.
pub struct DockerPeerLookup {
    sandbox: Arc<DockerSandbox>,
    network: String,
}

impl DockerPeerLookup {
    pub fn new(sandbox: Arc<DockerSandbox>, network: impl Into<String>) -> Self {
        Self {
            sandbox,
            network: network.into(),
        }
    }
}

#[async_trait::async_trait]
impl PeerLookup for DockerPeerLookup {
    async fn resolve(&self, ip: &str) -> AppResult<Option<PeerIdentity>> {
        EgressNetwork::resolve_peer(&self.sandbox, &self.network, ip)
            .await
            .map_err(|e| AppError::BadRequest(format!("peer lookup: {e}")))
    }
}

/// Bounded, short-TTL cache in front of a [`PeerLookup`].
///
/// Resolving costs a Docker list + inspect, and the proxy resolves once per
/// connection. The TTL is short because Docker recycles IP addresses, and the
/// capacity keeps a flood of unknown IPs from growing memory without bound.
pub struct PeerIndex {
    inner: Arc<dyn PeerLookup>,
    ttl: Duration,
    capacity: usize,
    entries: Mutex<HashMap<String, (PeerIdentity, Instant)>>,
}

impl PeerIndex {
    pub fn new(inner: Arc<dyn PeerLookup>, ttl: Duration, capacity: usize) -> Self {
        Self {
            inner,
            ttl,
            capacity: capacity.max(1),
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn hit(&self, ip: &str) -> Option<PeerIdentity> {
        let mut entries = self.entries.lock().expect("peer cache poisoned");
        match entries.get(ip) {
            Some((identity, at)) if at.elapsed() < self.ttl => Some(identity.clone()),
            Some(_) => {
                entries.remove(ip);
                None
            }
            None => None,
        }
    }

    fn store(&self, ip: &str, identity: &PeerIdentity) {
        let mut entries = self.entries.lock().expect("peer cache poisoned");
        entries.retain(|_, (_, at)| at.elapsed() < self.ttl);
        if entries.len() >= self.capacity {
            // Drop the oldest entries, leaving room for this one. A burst of new
            // peers trims the cache instead of growing it past the bound.
            let mut oldest: Vec<(Instant, String)> = entries
                .iter()
                .map(|(key, (_, at))| (*at, key.clone()))
                .collect();
            oldest.sort();
            let excess = entries.len() + 1 - self.capacity;
            for (_, key) in oldest.into_iter().take(excess) {
                entries.remove(&key);
            }
        }
        entries.insert(ip.to_string(), (identity.clone(), Instant::now()));
    }
}

#[async_trait::async_trait]
impl PeerLookup for PeerIndex {
    async fn resolve(&self, ip: &str) -> AppResult<Option<PeerIdentity>> {
        if let Some(identity) = self.hit(ip) {
            return Ok(Some(identity));
        }
        // Unknown peers are deliberately not cached: a container created between
        // two connections must become known immediately.
        let resolved = self.inner.resolve(ip).await?;
        if let Some(identity) = &resolved {
            self.store(ip, identity);
        }
        Ok(resolved)
    }
}

#[derive(Clone)]
pub struct InternalEgressState {
    pub token: String,
    pub store: Arc<dyn GovernanceStore>,
    pub credentials: Option<Arc<dyn CredentialsSeam>>,
    pub peers: Arc<dyn PeerLookup>,
}

/// The Docker work `bootstrap` performs. Injectable so the orchestration can be
/// tested without a live daemon (the same trade-off as the sandbox's runner).
#[async_trait::async_trait]
trait BootBackend: Send + Sync {
    /// Networks of the control plane container, discovered from its hostname.
    async fn control_networks(&self) -> Result<Vec<String>, SandboxError>;
    /// Create/start the network and proxy; return the health it reports.
    async fn bring_up(&self, cfg: &EgressConfig) -> Result<String, SandboxError>;
}

/// Reads and publishes the values boot derives.
trait EnvSource: Send + Sync {
    fn get(&self, key: &str) -> Option<String>;
    fn set(&self, key: &str, value: &str);
}

struct ProcessEnv;

impl EnvSource for ProcessEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
    fn set(&self, key: &str, value: &str) {
        std::env::set_var(key, value);
    }
}

struct DockerBackend {
    docker: DockerSandbox,
}

impl DockerBackend {
    fn new() -> Self {
        Self {
            docker: DockerSandbox::new(),
        }
    }
}

#[async_trait::async_trait]
impl BootBackend for DockerBackend {
    async fn control_networks(&self) -> Result<Vec<String>, SandboxError> {
        EgressNetwork::control_networks(&self.docker).await
    }

    async fn bring_up(&self, cfg: &EgressConfig) -> Result<String, SandboxError> {
        let network = EgressNetwork::ensure(&self.docker, cfg).await?;
        Ok(match network.health(&self.docker).await {
            Some(status) => format!("{status:?}"),
            None => "no healthcheck reported".to_string(),
        })
    }
}

/// Bring up network enforcement, or do nothing when the mode is not `network`.
///
/// Fail boot on purpose: a service that starts with `enforcement=network` and no
/// working proxy would either deny everything (outage) or leak (bypass). Every
/// error carries the next action.
pub async fn bootstrap() -> Result<Option<crate::governance::ProxyInfo>, SandboxError> {
    let enforcement = std::env::var("AGENT_EGRESS_ENFORCEMENT").unwrap_or_default();
    bootstrap_with(&enforcement, &ProcessEnv, &DockerBackend::new()).await
}

async fn bootstrap_with(
    enforcement: &str,
    env: &dyn EnvSource,
    backend: &dyn BootBackend,
) -> Result<Option<crate::governance::ProxyInfo>, SandboxError> {
    if enforcement.trim() != "network" {
        return Ok(None);
    }
    let missing = |value: Option<String>| value.map(|v| v.trim().is_empty()).unwrap_or(true);
    if missing(env.get("EGRESS_PROXY_TOKEN")) {
        // Generated once per boot and shared with the proxy container through its
        // environment; never written to disk or logged.
        let token = generate_token();
        env.set("EGRESS_PROXY_TOKEN", &token);
        tracing::info!("generated an egress proxy token for this boot");
    }
    let mut cfg = EgressConfig::from_lookup(|key| env.get(key))?;
    cfg.control_networks = backend.control_networks().await?;

    // Single source of truth: the derived values are published back into the
    // environment so the per-agent config reads exactly what was brought up.
    if missing(env.get("AGENT_EGRESS_NETWORK")) {
        env.set("AGENT_EGRESS_NETWORK", &cfg.network);
    }
    if missing(env.get("AGENT_EGRESS_PROXY_URL")) {
        let url = cfg.proxy_url_for_sandbox();
        env.set("AGENT_EGRESS_PROXY_URL", &url);
    }

    let health = backend.bring_up(&cfg).await?;
    let summary = format!(
        "network egress enforcement is up: network={} proxy={} image={} health={}",
        cfg.network, cfg.proxy_container, cfg.proxy_image, health
    );
    tracing::info!("{summary}");
    Ok(Some(crate::governance::ProxyInfo {
        container: cfg.proxy_container,
        network: cfg.network,
        image: cfg.proxy_image,
        health,
    }))
}

fn generate_token() -> String {
    use rand::Rng;
    let bytes: [u8; 32] = rand::thread_rng().gen();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Build the internal surface from the environment, or `None` when network
/// enforcement is off. The routes must not exist otherwise.
pub fn state_from_env() -> Option<InternalEgressState> {
    state_from(&|key| std::env::var(key).ok())
}

/// Testable half of [`state_from_env`].
fn state_from(get: &dyn Fn(&str) -> Option<String>) -> Option<InternalEgressState> {
    let enforcement = get("AGENT_EGRESS_ENFORCEMENT")?;
    if enforcement.trim() != "network" {
        return None;
    }
    let token = get("EGRESS_PROXY_TOKEN").filter(|v| !v.trim().is_empty())?;
    let network = get("AGENT_EGRESS_NETWORK")
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| agent_sandbox::egress_net::DEFAULT_NETWORK.to_string());
    let sandbox = Arc::new(DockerSandbox::new());
    // Resolving is one Docker list + inspect; cache it briefly (Docker recycles
    // IPs, so the TTL stays short) instead of paying it on every connection.
    let peers = Arc::new(PeerIndex::new(
        Arc::new(DockerPeerLookup::new(sandbox, network)),
        Duration::from_secs(10),
        4096,
    ));
    Some(InternalEgressState {
        token,
        store: crate::governance::global_store(),
        credentials: crate::governance::credentials_from_env(),
        peers,
    })
}

/// Mount only when network enforcement is on; otherwise the proxy has nothing to
/// call and the routes must not exist.
pub fn router(state: InternalEgressState) -> Router {
    Router::new()
        .route("/internal/egress/whoami", post(whoami))
        .route("/internal/egress/policy", post(policy))
        .route("/internal/egress/stubs", post(stubs))
        .route("/internal/egress/secret", post(secret))
        .route("/internal/egress/audit", post(audit))
        .with_state(state)
}

/// Constant-time comparison. Length is not secret (both are fixed-size tokens).
fn token_ok(provided: Option<&str>, expected: &str) -> bool {
    let Some(provided) = provided else {
        return false;
    };
    let a = provided.as_bytes();
    let b = expected.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn authorise(headers: &HeaderMap, state: &InternalEgressState) -> AppResult<()> {
    let provided = headers.get(TOKEN_HEADER).and_then(|v| v.to_str().ok());
    if token_ok(provided, &state.token) {
        Ok(())
    } else {
        // No detail: a wrong token learns nothing about the expected value.
        Err(AppError::Forbidden("invalid egress proxy token".into()))
    }
}

/// Verify that `principal_id` really owns `peer_ip`.
async fn bind_principal(
    state: &InternalEgressState,
    principal_id: &str,
    peer_ip: &str,
) -> AppResult<PeerIdentity> {
    let identity = state
        .peers
        .resolve(peer_ip)
        .await?
        .ok_or_else(|| AppError::NotFound)?;
    if identity.principal_id != principal_id {
        return Err(AppError::Forbidden(format!(
            "peer {peer_ip} does not belong to principal `{principal_id}`"
        )));
    }
    Ok(identity)
}

#[derive(Debug, Deserialize)]
pub struct WhoamiBody {
    pub peer_ip: String,
}

#[derive(Debug, Serialize)]
pub struct WhoamiResponse {
    pub principal_id: String,
    pub sandbox_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

pub async fn whoami(
    State(state): State<InternalEgressState>,
    headers: HeaderMap,
    Json(body): Json<WhoamiBody>,
) -> AppResult<Json<WhoamiResponse>> {
    authorise(&headers, &state)?;
    let identity = state
        .peers
        .resolve(&body.peer_ip)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(WhoamiResponse {
        principal_id: identity.principal_id,
        sandbox_id: identity.sandbox_id,
        session_id: identity.session_id,
    }))
}

#[derive(Debug, Deserialize)]
pub struct PrincipalBody {
    pub principal_id: String,
    pub peer_ip: String,
}

pub async fn policy(
    State(state): State<InternalEgressState>,
    headers: HeaderMap,
    Json(body): Json<PrincipalBody>,
) -> AppResult<Json<EgressPolicy>> {
    authorise(&headers, &state)?;
    bind_principal(&state, &body.principal_id, &body.peer_ip).await?;
    Ok(Json(state.store.policy(&body.principal_id).await?))
}

pub async fn stubs(
    State(state): State<InternalEgressState>,
    headers: HeaderMap,
    Json(body): Json<PrincipalBody>,
) -> AppResult<Json<Vec<SecretStub>>> {
    authorise(&headers, &state)?;
    bind_principal(&state, &body.principal_id, &body.peer_ip).await?;
    let stubs = match &state.credentials {
        Some(credentials) => credentials
            .stubs()
            .await
            .map_err(|e| AppError::BadRequest(e.to_string()))?,
        None => Vec::new(),
    };
    Ok(Json(stubs))
}

#[derive(Debug, Deserialize)]
pub struct SecretBody {
    pub principal_id: String,
    pub peer_ip: String,
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct SecretResponse {
    pub value: String,
}

pub async fn secret(
    State(state): State<InternalEgressState>,
    headers: HeaderMap,
    Json(body): Json<SecretBody>,
) -> AppResult<Json<SecretResponse>> {
    authorise(&headers, &state)?;
    bind_principal(&state, &body.principal_id, &body.peer_ip).await?;
    let credentials = state
        .credentials
        .as_ref()
        .ok_or_else(|| AppError::BadRequest("no credential store is configured".into()))?;
    let value = credentials
        .resolve(&body.name)
        .await
        .map_err(|e| AppError::BadRequest(e.to_string()))?;
    Ok(Json(SecretResponse {
        value: value.expose().to_string(),
    }))
}

#[derive(Debug, Deserialize)]
pub struct AuditBody {
    pub events: Vec<AuditEvent>,
}

pub async fn audit(
    State(state): State<InternalEgressState>,
    headers: HeaderMap,
    Json(body): Json<AuditBody>,
) -> AppResult<axum::http::StatusCode> {
    authorise(&headers, &state)?;
    if body.events.len() > 512 {
        return Err(AppError::BadRequest(
            "at most 512 audit events per batch".into(),
        ));
    }
    for event in body.events {
        state.store.append_audit(event).await?;
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::governance::MemoryGovernanceStore;
    use agent_core::seams::{
        AuditKind, EgressMode, HostPattern, SeamError, SeamResult, SecretMeta, SecretValue,
        SubstitutionReport,
    };
    use axum::http::HeaderValue;

    fn identity(principal: &str) -> PeerIdentity {
        PeerIdentity {
            principal_id: principal.into(),
            session_id: Some("s1".into()),
            sandbox_id: "sbx-1".into(),
        }
    }

    struct FixedPeers(Option<PeerIdentity>);

    #[async_trait::async_trait]
    impl PeerLookup for FixedPeers {
        async fn resolve(&self, _ip: &str) -> AppResult<Option<PeerIdentity>> {
            Ok(self.0.clone())
        }
    }

    /// Counts how often the lookup underneath the cache was consulted.
    struct CountingPeers {
        calls: std::sync::atomic::AtomicUsize,
        identity: Option<PeerIdentity>,
    }

    impl CountingPeers {
        fn new(identity: Option<PeerIdentity>) -> Self {
            Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
                identity,
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl PeerLookup for CountingPeers {
        async fn resolve(&self, _ip: &str) -> AppResult<Option<PeerIdentity>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self.identity.clone())
        }
    }

    #[tokio::test]
    async fn peer_identity_is_cached_but_unknown_peers_are_not() {
        let inner = Arc::new(CountingPeers::new(Some(identity("tenant-a"))));
        let cache = PeerIndex::new(inner.clone(), Duration::from_secs(10), 16);
        assert_eq!(
            cache.resolve("10.0.0.5").await.unwrap(),
            Some(identity("tenant-a"))
        );
        assert_eq!(
            cache.resolve("10.0.0.5").await.unwrap(),
            Some(identity("tenant-a"))
        );
        assert_eq!(inner.calls(), 1, "a hit must not inspect Docker again");

        // A negative answer is not cached: a container created between two
        // connections has to become known immediately.
        let unknown = Arc::new(CountingPeers::new(None));
        let cache = PeerIndex::new(unknown.clone(), Duration::from_secs(10), 16);
        assert!(cache.resolve("10.0.0.9").await.unwrap().is_none());
        assert!(cache.resolve("10.0.0.9").await.unwrap().is_none());
        assert_eq!(unknown.calls(), 2);
    }

    #[tokio::test]
    async fn the_peer_cache_expires_and_stays_bounded() {
        let inner = Arc::new(CountingPeers::new(Some(identity("tenant-a"))));
        // Zero TTL: every lookup goes through (the IP may already be recycled).
        let cache = PeerIndex::new(inner.clone(), Duration::ZERO, 16);
        cache.resolve("10.0.0.5").await.unwrap();
        cache.resolve("10.0.0.5").await.unwrap();
        assert_eq!(inner.calls(), 2);

        let cache = PeerIndex::new(inner.clone(), Duration::from_secs(60), 2);
        for ip in ["10.0.0.1", "10.0.0.2", "10.0.0.3"] {
            cache.resolve(ip).await.unwrap();
        }
        assert_eq!(
            cache.entries.lock().unwrap().len(),
            2,
            "capacity is enforced"
        );
    }

    struct OneSecret;

    #[async_trait::async_trait]
    impl CredentialsSeam for OneSecret {
        async fn list(&self) -> SeamResult<Vec<SecretMeta>> {
            Ok(Vec::new())
        }
        async fn resolve(&self, name: &str) -> SeamResult<SecretValue> {
            if name == "openai" {
                Ok(SecretValue::new("sk-live"))
            } else {
                Err(SeamError::NotFound(name.into()))
            }
        }
        async fn rotate(&self, _n: &str, _v: SecretValue) -> SeamResult<SecretMeta> {
            Err(SeamError::Unsupported("read-only".into()))
        }
        async fn delete(&self, _n: &str) -> SeamResult<()> {
            Err(SeamError::Unsupported("read-only".into()))
        }
        async fn stubs(&self) -> SeamResult<Vec<SecretStub>> {
            Ok(vec![SecretStub {
                id: "s1".into(),
                host: HostPattern::parse("api.openai.com").unwrap(),
                path_prefix: Some("/v1".into()),
                credential: "openai".into(),
                header: "authorization".into(),
                value_template: "Bearer ${credential}".into(),
            }])
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

    async fn state(peers: Option<PeerIdentity>, credentials: bool) -> InternalEgressState {
        InternalEgressState {
            token: "tok".into(),
            store: Arc::new(MemoryGovernanceStore::new()),
            credentials: if credentials {
                Some(Arc::new(OneSecret))
            } else {
                None
            },
            peers: Arc::new(FixedPeers(peers)),
        }
    }

    fn headers(token: &str) -> HeaderMap {
        let mut map = HeaderMap::new();
        map.insert(TOKEN_HEADER, HeaderValue::from_str(token).unwrap());
        map
    }

    #[tokio::test]
    async fn a_missing_or_wrong_token_is_forbidden() {
        let state = state(Some(identity("tenant-a")), false).await;
        for token in ["", "nope", "tok-longer"] {
            let err = whoami(
                State(state.clone()),
                headers(token),
                Json(WhoamiBody {
                    peer_ip: "10.0.0.5".into(),
                }),
            )
            .await
            .unwrap_err();
            assert!(matches!(err, AppError::Forbidden(_)), "{token}");
        }
        let mut empty = HeaderMap::new();
        empty.insert(TOKEN_HEADER, HeaderValue::from_static(""));
        assert!(matches!(
            whoami(
                State(state),
                empty,
                Json(WhoamiBody {
                    peer_ip: "10.0.0.5".into()
                })
            )
            .await,
            Err(AppError::Forbidden(_))
        ));
    }

    #[tokio::test]
    async fn an_unknown_peer_is_not_found() {
        let state = state(None, false).await;
        let response = whoami(
            State(state.clone()),
            headers("tok"),
            Json(WhoamiBody {
                peer_ip: "10.0.0.9".into(),
            }),
        )
        .await;
        assert!(matches!(response, Err(AppError::NotFound)));
        assert!(matches!(
            policy(
                State(state),
                headers("tok"),
                Json(PrincipalBody {
                    principal_id: "tenant-a".into(),
                    peer_ip: "10.0.0.9".into()
                })
            )
            .await,
            Err(AppError::NotFound)
        ));
    }

    #[tokio::test]
    async fn a_principal_cannot_read_another_tenants_policy() {
        let state = state(Some(identity("tenant-a")), false).await;
        assert!(matches!(
            policy(
                State(state),
                headers("tok"),
                Json(PrincipalBody {
                    principal_id: "tenant-b".into(),
                    peer_ip: "10.0.0.5".into()
                })
            )
            .await,
            Err(AppError::Forbidden(_))
        ));
    }

    #[tokio::test]
    async fn policy_stubs_and_secrets_come_from_the_control_plane() {
        let state = state(Some(identity("tenant-a")), true).await;
        let policy = policy(
            State(state.clone()),
            headers("tok"),
            Json(PrincipalBody {
                principal_id: "tenant-a".into(),
                peer_ip: "10.0.0.5".into(),
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(policy.mode, EgressMode::DenyAll);

        let stubs = stubs(
            State(state.clone()),
            headers("tok"),
            Json(PrincipalBody {
                principal_id: "tenant-a".into(),
                peer_ip: "10.0.0.5".into(),
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(stubs.len(), 1);

        let resolved = secret(
            State(state.clone()),
            headers("tok"),
            Json(SecretBody {
                principal_id: "tenant-a".into(),
                peer_ip: "10.0.0.5".into(),
                name: "openai".into(),
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(resolved.value, "sk-live");

        let missing = secret(
            State(state),
            headers("tok"),
            Json(SecretBody {
                principal_id: "tenant-a".into(),
                peer_ip: "10.0.0.5".into(),
                name: "nope".into(),
            }),
        )
        .await;
        assert!(matches!(missing, Err(AppError::BadRequest(_))));
    }

    #[tokio::test]
    async fn without_a_credential_store_the_endpoints_say_so() {
        let state = state(Some(identity("tenant-a")), false).await;
        assert!(matches!(
            secret(
                State(state.clone()),
                headers("tok"),
                Json(SecretBody {
                    principal_id: "tenant-a".into(),
                    peer_ip: "10.0.0.5".into(),
                    name: "openai".into()
                })
            )
            .await,
            Err(AppError::BadRequest(_))
        ));
        let empty = stubs(
            State(state),
            headers("tok"),
            Json(PrincipalBody {
                principal_id: "tenant-a".into(),
                peer_ip: "10.0.0.5".into(),
            }),
        )
        .await
        .unwrap()
        .0;
        assert!(empty.is_empty(), "no store means no stubs, not an error");
    }

    #[tokio::test]
    async fn audit_batches_are_written_once_and_bounded() {
        let state = state(Some(identity("tenant-a")), false).await;
        let event =
            AuditEvent::new(AuditKind::EgressDecision, "tenant-a", "a.com").with_decision(false);
        let status = audit(
            State(state.clone()),
            headers("tok"),
            Json(AuditBody {
                events: vec![event],
            }),
        )
        .await
        .unwrap();
        assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
        let stored = state.store.list_audit("tenant-a", 10, None).await.unwrap();
        assert_eq!(stored.len(), 1);

        let too_many = vec![AuditEvent::new(AuditKind::ToolExecuted, "tenant-a", "x"); 513];
        assert!(matches!(
            audit(
                State(state),
                headers("tok"),
                Json(AuditBody { events: too_many })
            )
            .await,
            Err(AppError::BadRequest(_))
        ));
    }

    #[tokio::test]
    async fn the_token_comparison_is_exact() {
        assert!(token_ok(Some("abc"), "abc"));
        assert!(!token_ok(Some("abc"), "abd"));
        assert!(!token_ok(Some("ab"), "abc"));
        assert!(!token_ok(None, "abc"));
    }

    #[test]
    fn the_token_header_name_is_stable() {
        assert_eq!(TOKEN_HEADER, "x-aria-proxy-token");
    }

    struct FakeEnv(Mutex<HashMap<String, String>>);

    impl FakeEnv {
        fn new(values: &[(&str, &str)]) -> Self {
            let map = values
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            Self(Mutex::new(map))
        }
        fn value(&self, key: &str) -> Option<String> {
            self.0.lock().unwrap().get(key).cloned()
        }
    }

    impl EnvSource for FakeEnv {
        fn get(&self, key: &str) -> Option<String> {
            self.value(key)
        }
        fn set(&self, key: &str, value: &str) {
            self.0
                .lock()
                .unwrap()
                .insert(key.to_string(), value.to_string());
        }
    }

    struct FakeBackend {
        networks: Vec<String>,
        health: String,
        fail: bool,
    }

    impl FakeBackend {
        fn ok() -> Self {
            Self {
                networks: vec!["aria_default".into()],
                health: "running".into(),
                fail: false,
            }
        }
    }

    #[async_trait::async_trait]
    impl BootBackend for FakeBackend {
        async fn control_networks(&self) -> Result<Vec<String>, SandboxError> {
            if self.fail {
                return Err(SandboxError::NotConfigured("no daemon".into()));
            }
            Ok(self.networks.clone())
        }
        async fn bring_up(&self, cfg: &EgressConfig) -> Result<String, SandboxError> {
            if cfg.control_networks.is_empty() {
                return Err(SandboxError::NotConfigured("no control network".into()));
            }
            Ok(self.health.clone())
        }
    }

    #[tokio::test]
    async fn boot_is_a_noop_unless_the_mode_is_network() {
        let env = FakeEnv::new(&[]);
        let backend = FakeBackend::ok();
        let info = bootstrap_with("proxy", &env, &backend).await.unwrap();
        assert!(info.is_none());
        // `bootstrap` itself, in the default (proxy) mode.
        assert!(bootstrap().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn boot_publishes_the_derived_values() {
        let env = FakeEnv::new(&[
            ("EGRESS_PROXY_IMAGE", "aria-egress-proxy:local"),
            ("CONTROL_PLANE_URL", "http://cloud:3000"),
        ]);
        let info = bootstrap_with("network", &env, &FakeBackend::ok())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            info.container,
            agent_sandbox::egress_net::DEFAULT_PROXY_CONTAINER
        );
        assert_eq!(info.network, agent_sandbox::egress_net::DEFAULT_NETWORK);
        assert_eq!(info.image, "aria-egress-proxy:local");
        assert_eq!(info.health, "running");
        let token = env.value("EGRESS_PROXY_TOKEN").unwrap();
        assert_eq!(token.len(), 64, "32 random bytes as hex");
        assert_eq!(
            env.value("AGENT_EGRESS_NETWORK").as_deref(),
            Some(agent_sandbox::egress_net::DEFAULT_NETWORK)
        );
        assert_eq!(
            env.value("AGENT_EGRESS_PROXY_URL").as_deref(),
            Some("http://aria-egress-proxy:8080")
        );
    }

    #[tokio::test]
    async fn boot_keeps_explicit_values_and_surfaces_failures() {
        let env = FakeEnv::new(&[
            ("EGRESS_PROXY_TOKEN", "given-token"),
            ("EGRESS_PROXY_IMAGE", "img"),
            ("CONTROL_PLANE_URL", "http://cloud:3000"),
            ("AGENT_EGRESS_NETWORK", "custom-net"),
            ("AGENT_EGRESS_PROXY_URL", "http://custom:9"),
        ]);
        let info = bootstrap_with("network", &env, &FakeBackend::ok())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(info.network, "custom-net");
        assert_eq!(
            env.value("EGRESS_PROXY_TOKEN").as_deref(),
            Some("given-token")
        );
        assert_eq!(
            env.value("AGENT_EGRESS_PROXY_URL").as_deref(),
            Some("http://custom:9")
        );

        let env = FakeEnv::new(&[
            ("EGRESS_PROXY_IMAGE", "img"),
            ("CONTROL_PLANE_URL", "http://cloud:3000"),
        ]);
        let failing = FakeBackend {
            fail: true,
            ..FakeBackend::ok()
        };
        assert!(matches!(
            bootstrap_with("network", &env, &failing).await,
            Err(SandboxError::NotConfigured(_))
        ));

        // Missing required values fail before any Docker call.
        let env = FakeEnv::new(&[]);
        assert!(bootstrap_with("network", &env, &FakeBackend::ok())
            .await
            .is_err());
    }

    #[test]
    fn the_process_env_source_reads_and_writes() {
        let env = ProcessEnv;
        env.set("ARIA_COVERAGE_PROBE", "1");
        assert_eq!(env.get("ARIA_COVERAGE_PROBE").as_deref(), Some("1"));
        std::env::remove_var("ARIA_COVERAGE_PROBE");
        assert!(env.get("ARIA_COVERAGE_PROBE").is_none());
    }

    #[test]
    fn the_internal_surface_only_exists_under_network_enforcement() {
        let values = |enforcement: &'static str, token: Option<&'static str>| {
            move |key: &str| match key {
                "AGENT_EGRESS_ENFORCEMENT" => Some(enforcement.to_string()),
                "EGRESS_PROXY_TOKEN" => token.map(str::to_string),
                "AGENT_EGRESS_NETWORK" => Some("  ".to_string()),
                _ => None,
            }
        };
        assert!(values("proxy", None)("SOMETHING_ELSE").is_none());
        assert!(state_from(&values("proxy", Some("t"))).is_none());
        assert!(state_from(&values("network", None)).is_none());
        assert!(state_from(&values("network", Some("  "))).is_none());
        let state = state_from(&values("network", Some("t"))).unwrap();
        assert_eq!(state.token, "t");
    }

    #[tokio::test]
    async fn the_docker_backend_surfaces_daemon_problems() {
        let backend = DockerBackend::new();
        // Smoke call: covered either way, and a host without a daemon must not
        // panic. (Inside a container this can legitimately succeed.)
        let _ = backend.control_networks().await;

        // An empty control-network list is refused before Docker is touched, so
        // this cannot create anything even on a live daemon.
        let cfg = EgressConfig::from_lookup(|key| match key {
            "EGRESS_PROXY_IMAGE" => Some("img".into()),
            "CONTROL_PLANE_URL" => Some("http://cp:3000".into()),
            "EGRESS_PROXY_TOKEN" => Some("t".into()),
            _ => None,
        })
        .unwrap();
        assert!(matches!(
            backend.bring_up(&cfg).await,
            Err(SandboxError::NotConfigured(_))
        ));
    }

    #[tokio::test]
    async fn a_successful_whoami_reports_the_labels() {
        let state = state(Some(identity("tenant-a")), false).await;
        // Building the router is part of the surface: it must not panic.
        let _ = router(state.clone());
        let who = whoami(
            State(state),
            headers("tok"),
            Json(WhoamiBody {
                peer_ip: "10.0.0.5".into(),
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(who.principal_id, "tenant-a");
        assert_eq!(who.sandbox_id, "sbx-1");
        assert_eq!(who.session_id.as_deref(), Some("s1"));
    }

    struct FailingCredentials;

    #[async_trait::async_trait]
    impl CredentialsSeam for FailingCredentials {
        async fn list(&self) -> SeamResult<Vec<SecretMeta>> {
            Err(SeamError::Io("down".into()))
        }
        async fn resolve(&self, name: &str) -> SeamResult<SecretValue> {
            Err(SeamError::NotFound(name.into()))
        }
        async fn rotate(&self, _name: &str, _value: SecretValue) -> SeamResult<SecretMeta> {
            Err(SeamError::Unsupported("read-only".into()))
        }
        async fn delete(&self, _name: &str) -> SeamResult<()> {
            Err(SeamError::Unsupported("read-only".into()))
        }
        async fn stubs(&self) -> SeamResult<Vec<SecretStub>> {
            Err(SeamError::Io("down".into()))
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
            Err(SeamError::Io("down".into()))
        }
    }

    #[tokio::test]
    async fn a_failing_credential_store_is_reported_as_a_bad_request() {
        // Every method of the failing store answers an error of its own kind.
        let creds = FailingCredentials;
        assert!(matches!(
            creds.resolve("x").await,
            Err(SeamError::NotFound(_))
        ));
        assert!(matches!(creds.list().await, Err(SeamError::Io(_))));
        assert!(matches!(
            creds.rotate("x", SecretValue::new("v")).await,
            Err(SeamError::Unsupported(_))
        ));
        assert!(matches!(
            creds.delete("x").await,
            Err(SeamError::Unsupported(_))
        ));
        assert!(matches!(
            creds
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
            creds.remove_stub("s").await,
            Err(SeamError::Unsupported(_))
        ));
        let mut header_list = Vec::new();
        assert!(matches!(
            creds.substitute("a.com", "/", &mut header_list).await,
            Err(SeamError::Io(_))
        ));

        let mut state = state(Some(identity("tenant-a")), false).await;
        state.credentials = Some(Arc::new(FailingCredentials));
        assert!(matches!(
            stubs(
                State(state.clone()),
                headers("tok"),
                Json(PrincipalBody {
                    principal_id: "tenant-a".into(),
                    peer_ip: "10.0.0.5".into(),
                })
            )
            .await,
            Err(AppError::BadRequest(_))
        ));
        assert!(matches!(
            secret(
                State(state),
                headers("tok"),
                Json(SecretBody {
                    principal_id: "tenant-a".into(),
                    peer_ip: "10.0.0.5".into(),
                    name: "openai".into(),
                })
            )
            .await,
            Err(AppError::BadRequest(_))
        ));
    }

    #[tokio::test]
    async fn a_docker_lookup_failure_is_mapped_not_swallowed() {
        let lookup = DockerPeerLookup::new(
            Arc::new(DockerSandbox::new()),
            agent_sandbox::egress_net::DEFAULT_NETWORK,
        );
        match lookup.resolve("10.0.0.5").await {
            // No daemon here: the error must be a BadRequest, never a panic.
            Err(AppError::BadRequest(message)) => {
                assert!(message.contains("peer lookup"), "{message}")
            }
            // A live daemon with no such container is also a valid answer.
            Ok(None) | Ok(Some(_)) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_test_credential_store_is_read_only() {
        let credentials = OneSecret;
        assert!(credentials.list().await.unwrap().is_empty());
        assert!(matches!(
            credentials.rotate("x", SecretValue::new("v")).await,
            Err(SeamError::Unsupported(_))
        ));
        assert!(matches!(
            credentials.delete("x").await,
            Err(SeamError::Unsupported(_))
        ));
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
