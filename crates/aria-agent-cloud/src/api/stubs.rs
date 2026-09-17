//! Recognised-but-unimplemented beta Agents resources.
//!
//! These routes exist so an OpenAI client enumerating the surface gets a clear
//! `501 Not Implemented` instead of a confusing `404`. Each body names the
//! resource so the response is self-describing.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

fn not_implemented(resource: &str) -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "error": {
                "message": format!("{resource} is not implemented by aria-agent-cloud"),
                "type": "not_implemented",
                "resource": resource,
            }
        })),
    )
        .into_response()
}

pub async fn vaults() -> Response {
    not_implemented("vaults")
}

pub async fn vault_credentials() -> Response {
    not_implemented("vaults.credentials")
}

pub async fn environments() -> Response {
    not_implemented("agents.environments")
}

pub async fn environment_files() -> Response {
    not_implemented("agents.environments.files")
}

pub async fn environment_templates() -> Response {
    not_implemented("agents.environments.templates")
}

pub async fn session_artifacts() -> Response {
    not_implemented("agents.sessions.artifacts")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stub_returns_501() {
        let resp = environments().await.into_response();
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    }
}
