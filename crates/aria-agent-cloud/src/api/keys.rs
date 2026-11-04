//! Self-serve API keys: `POST|GET /v1/api-keys`, `DELETE /v1/api-keys/{id}`.
//!
//! Admin-only. The plaintext key is returned exactly once (on create); only its
//! sha256 hash is persisted. Revocation purges the in-memory resolution cache
//! so a revoked key stops working immediately.

use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::app::AppState;
use crate::auth::{generate_key, ApiKeyScopes, Principal};
use crate::store::NewApiKeyRow;
use crate::AppError;

#[derive(Deserialize)]
pub struct CreateApiKeyBody {
    /// Human label for the key (optional).
    pub label: Option<String>,
    /// Tenant/user this key authenticates as. Generated when omitted.
    pub principal_id: Option<String>,
    /// Permission tokens: `read`, `write`, `admin`. Defaults to `read,write`.
    pub scopes: Option<Vec<String>>,
}

#[derive(Serialize)]
pub struct ApiKeyView {
    pub id: String,
    pub key_prefix: String,
    pub principal_id: String,
    pub label: Option<String>,
    pub scopes: Vec<String>,
    pub created_at: String,
}

#[derive(Serialize)]
pub struct CreatedApiKey {
    pub id: String,
    /// Plaintext key — shown ONLY in this response. Store it now.
    pub key: String,
    pub key_prefix: String,
    pub principal_id: String,
    pub scopes: Vec<String>,
}

/// Reject principal ids that are not safe to use as a shard key (context /
/// agents / sessions are filtered by `principal_id`, so a malformed id must
/// never reach a query). Returns `400` rather than panicking.
pub fn validate_pid(pid: &str) -> Result<(), AppError> {
    let ok = !pid.is_empty()
        && pid.len() <= 64
        && pid
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!(
            "invalid principal_id: {pid:?} (expected 1-64 chars of [A-Za-z0-9_-])"
        )))
    }
}

/// Create a new API key for a principal. Admin scope required.
pub async fn create_api_key(
    State(state): State<AppState>,
    principal: Principal,
    Json(body): Json<CreateApiKeyBody>,
) -> Result<Json<CreatedApiKey>, AppError> {
    if !principal.is_admin() {
        return Err(AppError::Forbidden(
            "admin scope required to create API keys".into(),
        ));
    }
    let pid = body
        .principal_id
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    validate_pid(&pid)?;
    let scopes = ApiKeyScopes::from_tokens(
        &body
            .scopes
            .unwrap_or_else(|| vec!["read".into(), "write".into()]),
    );
    let (key, prefix, hash) = generate_key();
    let id = Uuid::new_v4().to_string();
    state
        .keys
        .insert_key(&NewApiKeyRow {
            id: id.clone(),
            key_prefix: prefix.clone(),
            key_hash: hash,
            principal_id: pid.clone(),
            label: body.label,
            scopes: scopes.to_tokens(),
        })
        .await?;
    Ok(Json(CreatedApiKey {
        id,
        key,
        key_prefix: prefix,
        principal_id: pid,
        scopes: scopes.to_tokens(),
    }))
}

/// List active (non-revoked) API keys. Admin scope required.
pub async fn list_api_keys(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<Json<Vec<ApiKeyView>>, AppError> {
    if !principal.is_admin() {
        return Err(AppError::Forbidden(
            "admin scope required to list API keys".into(),
        ));
    }
    let rows = state.keys.list_keys().await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| ApiKeyView {
                id: r.id,
                key_prefix: r.key_prefix,
                principal_id: r.principal_id,
                label: r.label,
                scopes: r.scopes,
                created_at: r.created_at,
            })
            .collect(),
    ))
}

/// Revoke an API key. Admin scope required.
pub async fn revoke_api_key(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    if !principal.is_admin() {
        return Err(AppError::Forbidden(
            "admin scope required to revoke API keys".into(),
        ));
    }
    let hash = state
        .keys
        .revoke_key(&id)
        .await?
        .ok_or(AppError::NotFound)?;
    // Drop any cached resolution for this key so revocation is immediate.
    state.key_cache.purge(&hash);
    Ok(Json(json!({ "ok": true })))
}
