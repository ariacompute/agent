//! Caller identity, API-key scopes and the Bearer / ApiKey auth gate.
//!
//! Every request resolves to a [`Principal`] (a tenant). Data access is then
//! scoped by `principal_id` — admins read across tenants, users only their own.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::FromRequestParts;
use axum::extract::Request;
use axum::extract::State;
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use sha2::{Digest, Sha256};

use crate::app::AppState;
use crate::store::ResolvedKey;

/// Identity of an API caller (tenant / user). Also doubles as an axum
/// extractor: [`require_auth`] resolves the presented key to a `Principal` and
/// inserts it into the request extensions; handlers pull it back out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    /// Tenant/user id — the shard key for context (`context_fragments`), agents
    /// and sessions.
    pub id: String,
    pub kind: PrincipalKind,
    pub scopes: ApiKeyScopes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrincipalKind {
    Admin,
    User,
}

impl Principal {
    /// Bootstrap admin (resolves from `AGENT_CLOUD_API_KEY`).
    pub fn admin() -> Self {
        Principal {
            id: "admin".into(),
            kind: PrincipalKind::Admin,
            scopes: ApiKeyScopes::all(),
        }
    }

    /// Open-mode fallback tenant when no auth is configured.
    pub fn default_user() -> Self {
        Principal {
            id: "default".into(),
            kind: PrincipalKind::User,
            scopes: ApiKeyScopes::read_write(),
        }
    }

    pub fn is_admin(&self) -> bool {
        self.kind == PrincipalKind::Admin || self.scopes.admin
    }
}

/// Permission scopes, mirroring the OpenAI Agents API project-key model
/// (`api.agents.read` / `api.agents.write` / admin). Stored as a comma-joined
/// token string in Postgres.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ApiKeyScopes {
    pub admin: bool,
    pub read: bool,
    pub write: bool,
}

impl ApiKeyScopes {
    pub fn all() -> Self {
        ApiKeyScopes {
            admin: true,
            read: true,
            write: true,
        }
    }

    pub fn read_write() -> Self {
        ApiKeyScopes {
            admin: false,
            read: true,
            write: true,
        }
    }

    pub fn from_tokens(tokens: &[String]) -> Self {
        let mut s = ApiKeyScopes::default();
        for t in tokens {
            match t.as_str() {
                "admin" => s.admin = true,
                "read" => s.read = true,
                "write" => s.write = true,
                _ => {}
            }
        }
        // Admin implies full read/write.
        if s.admin {
            s.read = true;
            s.write = true;
        }
        s
    }

    pub fn to_tokens(self) -> Vec<String> {
        let mut v = Vec::new();
        if self.admin {
            v.push("admin".into());
        }
        if self.read {
            v.push("read".into());
        }
        if self.write {
            v.push("write".into());
        }
        v
    }
}

/// In-memory cache for resolved API keys, keyed by the sha256 hash. Only
/// **valid** lookups are cached; revocation actively purges an entry so a
/// revoked key stops working immediately. A TTL bounds how long a stale
/// positive entry can survive if a revoke somehow misses (multi-instance).
#[derive(Clone)]
pub struct KeyCache {
    entries: Arc<Mutex<HashMap<String, CachedKey>>>,
    ttl: Duration,
}

#[derive(Clone)]
pub struct CachedKey {
    pub principal_id: String,
    pub scopes: ApiKeyScopes,
    pub kind: PrincipalKind,
    pub expires_at: Instant,
}

impl KeyCache {
    pub fn new(ttl: Duration) -> Self {
        KeyCache {
            entries: Arc::new(Mutex::new(HashMap::new())),
            ttl,
        }
    }

    /// Return a still-valid cached principal, evicting expired entries.
    pub fn get(&self, hash: &str) -> Option<CachedKey> {
        let mut g = self.entries.lock().unwrap();
        match g.get(hash) {
            Some(c) if c.expires_at > Instant::now() => Some(c.clone()),
            Some(_) => {
                g.remove(hash);
                None
            }
            None => None,
        }
    }

    /// Insert a positive lookup (valid until now + ttl).
    pub fn put(&self, hash: &str, pid: &str, scopes: ApiKeyScopes, kind: PrincipalKind) {
        let c = CachedKey {
            principal_id: pid.to_string(),
            scopes,
            kind,
            expires_at: Instant::now() + self.ttl,
        };
        self.entries.lock().unwrap().insert(hash.to_string(), c);
    }

    /// Purge a key (on revocation). Returns whether an entry was removed.
    pub fn purge(&self, hash: &str) -> bool {
        self.entries.lock().unwrap().remove(hash).is_some()
    }
}

#[async_trait::async_trait]
impl<S> FromRequestParts<S> for Principal
where
    S: Send + Sync,
{
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, StatusCode> {
        parts
            .extensions
            .get::<Principal>()
            .cloned()
            .ok_or(StatusCode::UNAUTHORIZED)
    }
}

/// Bearer / ApiKey auth gate.
///
/// Resolves the presented key to a [`Principal`] and inserts it into the
/// request extensions so downstream handlers can scope data per tenant.
/// Precedence:
/// * `Authorization: Bearer <key>` (or `ApiKey <key>`).
/// * If the key equals `AGENT_CLOUD_API_KEY` → bootstrap **Admin** principal
///   (backward compatible with the old single-key gate).
/// * Else the key is looked up (by sha256 hash) in the `api_keys` store → the
///   row's `principal_id` / `scopes` define the principal.
/// * No key presented: if `AGENT_CLOUD_API_KEY` is set the request is rejected
///   (auth is on); otherwise the service runs **open** as the `default` tenant.
pub async fn require_auth(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let token = presented_token(&req);
    let principal = resolve_principal(&state, token).await?;
    req.extensions_mut().insert(principal);
    Ok(next.run(req).await)
}

/// Extract the bearer/apikey token from the `Authorization` header.
pub fn presented_token(req: &Request) -> Option<String> {
    req.headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| {
            h.strip_prefix("Bearer ")
                .or_else(|| h.strip_prefix("ApiKey "))
                .map(|t| t.trim().to_string())
        })
        .filter(|t| !t.is_empty())
}

fn kind_for(scopes: ApiKeyScopes) -> PrincipalKind {
    if scopes.admin {
        PrincipalKind::Admin
    } else {
        PrincipalKind::User
    }
}

fn principal_from(pid: String, scopes: ApiKeyScopes) -> Principal {
    Principal {
        id: pid,
        kind: kind_for(scopes),
        scopes,
    }
}

/// Resolve a presented token into a [`Principal`].
pub async fn resolve_principal(
    state: &AppState,
    token: Option<String>,
) -> Result<Principal, StatusCode> {
    let env_key = std::env::var("AGENT_CLOUD_API_KEY")
        .ok()
        .filter(|k| !k.is_empty());
    match token {
        // Bootstrap admin: matches the legacy single key.
        Some(t) if env_key.as_deref() == Some(t.as_str()) => Ok(Principal::admin()),
        Some(t) => {
            let hash = hash_key(&t);
            // Positive cache hit (valid until TTL): skip the store round-trip.
            if let Some(cached) = state.key_cache.get(&hash) {
                return Ok(Principal {
                    id: cached.principal_id,
                    kind: cached.kind,
                    scopes: cached.scopes,
                });
            }
            let resolved = state
                .keys
                .resolve_key(&hash)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            match resolved {
                Some(ResolvedKey {
                    principal_id,
                    scopes,
                }) => {
                    let principal = principal_from(principal_id, scopes);
                    // Cache the positive lookup for the configured TTL.
                    state
                        .key_cache
                        .put(&hash, &principal.id, scopes, principal.kind);
                    Ok(principal)
                }
                None => Err(StatusCode::UNAUTHORIZED),
            }
        }
        // No token: open mode → default tenant, else reject.
        None => {
            if env_key.is_some() {
                Err(StatusCode::UNAUTHORIZED)
            } else {
                Ok(Principal::default_user())
            }
        }
    }
}

/// sha256 hex digest of a key (never store or log the plaintext).
pub fn hash_key(key: &str) -> String {
    let mut h = Sha256::new();
    h.update(key.as_bytes());
    format!("{:x}", h.finalize())
}

/// Generate a high-entropy API key: `aria-<hex>`, returning the plaintext
/// (shown once), a short display prefix, and the sha256 hash that is stored.
pub fn generate_key() -> (String, String, String) {
    use rand::RngCore;
    let mut bytes = [0u8; 18];
    rand::thread_rng().fill_bytes(&mut bytes);
    let raw: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let key = format!("aria-{raw}");
    let prefix = format!("aria-{}", &raw[..12]);
    let hash = hash_key(&key);
    (key, prefix, hash)
}
