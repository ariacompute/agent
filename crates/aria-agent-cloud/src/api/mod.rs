//! The OpenAI **beta Agents** resource surface.
//!
//! * [`agents`] — `POST/GET/POST{id}/DELETE /v1/agents[/{id}]`
//! * [`sessions`] — `…/v1/agents/sessions[/{id}]`, `…/events`, `…/events/stream`,
//!   plus the read-only `items` / `turns` / `subagents` listings
//! * [`stubs`] — resources that are recognised but not implemented (501):
//!   vaults (+credentials), environments (+files/templates), artifacts

pub mod agents;
pub mod sessions;
pub mod stubs;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::AppError;

/// Error body in the OpenAI shape (`{"error": {"message": …}}` plus a flat
/// `message` so existing Aria clients keep working).
pub fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
    let message = message.into();
    (
        status,
        Json(json!({ "error": { "message": message.clone(), "type": status.as_str() }, "message": message })),
    )
        .into_response()
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            AppError::Db(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("db: {e}")),
            AppError::Core(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("core: {e}")),
            AppError::BadRequest(e) => (StatusCode::BAD_REQUEST, e),
            AppError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
        };
        error_response(status, msg)
    }
}
