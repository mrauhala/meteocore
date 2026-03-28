use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;

/// Error type for OGC API Maps endpoints.
/// Returns JSON error responses (unlike WMS which uses XML).
#[derive(Debug)]
pub enum MapsError {
    /// Resource not found (collection, style).
    NotFound(String),
    /// Invalid request parameters.
    BadRequest(String),
    /// Internal server error.
    Internal(String),
}

impl IntoResponse for MapsError {
    fn into_response(self) -> axum::response::Response {
        let (status, code, description) = match &self {
            MapsError::NotFound(msg) => (StatusCode::NOT_FOUND, "NotFound", msg.as_str()),
            MapsError::BadRequest(msg) => (StatusCode::BAD_REQUEST, "BadRequest", msg.as_str()),
            MapsError::Internal(_) => (
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
