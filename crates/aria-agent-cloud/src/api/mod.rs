//! The OpenAI **beta Agents** resource surface.
//!
//! * [`agents`] — `POST/GET/POST{id}/DELETE /v1/agents[/{id}]`
//! * [`sessions`] — `…/v1/agents/sessions[/{id}]`, `…/events`, `…/events/stream`,
//!   plus the read-only `items` / `turns` / `subagents` listings
//! * [`keys`] — self-serve API keys (`/v1/api-keys`)
//! * [`stubs`] — resources that are recognised but not implemented (501):
//!   vaults (+credentials), environments (+files/templates), artifacts

pub mod agents;
pub mod keys;
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
            AppError::Forbidden(e) => (StatusCode::FORBIDDEN, e),
            AppError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
        };
        error_response(status, msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::CoreError;

    async fn body(resp: Response) -> serde_json::Value {
        let bytes = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .unwrap()
            .to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn error_body_is_openai_shaped() {
        let resp = error_response(StatusCode::BAD_REQUEST, "nope");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let v = body(resp).await;
        assert_eq!(v["error"]["message"], "nope");
        assert_eq!(v["error"]["type"], "400");
        assert_eq!(v["message"], "nope");
    }

    #[test]
    fn app_error_maps_to_status_codes() {
        let cases: Vec<(AppError, StatusCode)> = vec![
            (AppError::BadRequest("bad".into()), StatusCode::BAD_REQUEST),
            (AppError::Forbidden("no".into()), StatusCode::FORBIDDEN),
            (AppError::NotFound, StatusCode::NOT_FOUND),
            (
                AppError::Core(CoreError::Config("boom".into())),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                AppError::Db(sqlx::Error::RowNotFound),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ];
        for (err, want) in cases {
            assert_eq!(err.into_response().status(), want);
        }
    }
}
