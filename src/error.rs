//! OpenSearch-shaped error responses: `{"error": {"type", "reason"}, "status"}`.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub kind: &'static str,
    pub reason: String,
}

impl ApiError {
    pub fn new(status: StatusCode, kind: &'static str, reason: impl Into<String>) -> Self {
        ApiError {
            status,
            kind,
            reason: reason.into(),
        }
    }

    pub fn bad_request(kind: &'static str, reason: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, kind, reason)
    }

    pub fn index_not_found(index: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "index_not_found_exception",
            format!("no such index [{index}]"),
        )
    }

    pub fn invalid_index_name(index: &str, why: &str) -> Self {
        Self::bad_request(
            "invalid_index_name_exception",
            format!("Invalid index name [{index}], {why}"),
        )
    }

    pub fn internal(reason: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "exception", reason)
    }

    pub fn io(err: std::io::Error) -> Self {
        Self::internal(format!("storage error: {err}"))
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}): {}", self.kind, self.status, self.reason)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.status.is_server_error() {
            tracing::error!(kind = self.kind, reason = %self.reason, "request failed");
        }
        let body = json!({
            "error": {
                "root_cause": [{"type": self.kind, "reason": self.reason}],
                "type": self.kind,
                "reason": self.reason,
            },
            "status": self.status.as_u16(),
        });
        (self.status, Json(body)).into_response()
    }
}

impl std::error::Error for ApiError {}
