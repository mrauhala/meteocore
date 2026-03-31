use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;

/// Error type for OGC API Tiles endpoints.
/// Returns JSON error responses.
#[derive(Debug)]
pub enum TilesError {
    /// Resource not found (collection, tileset, tile outside bounds).
    NotFound(String),
    /// Invalid request parameters.
    BadRequest(String),
    /// Internal server error.
    Internal(String),
}

impl IntoResponse for TilesError {
    fn into_response(self) -> axum::response::Response {
        let (status, code, description) = match &self {
            TilesError::NotFound(msg) => (StatusCode::NOT_FOUND, "NotFound", msg.as_str()),
            TilesError::BadRequest(msg) => (StatusCode::BAD_REQUEST, "BadRequest", msg.as_str()),
            TilesError::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "ServerError",
                "Internal server error",
            ),
        };

        (
            status,
            Json(json!({ "code": code, "description": description })),
        )
            .into_response()
    }
}
