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
    /// Server too busy (render semaphore exhausted).
    ServiceUnavailable(String),
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
            TilesError::ServiceUnavailable(msg) => {
                (StatusCode::SERVICE_UNAVAILABLE, "ServerBusy", msg.as_str())
            }
        };

        let reason = ds_core::error::ErrorReason(format!("{code}: {description}"));
        let mut response = (
            status,
            Json(json!({ "code": code, "description": description })),
        )
            .into_response();
        response.extensions_mut().insert(reason);
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_core::error::ErrorReason;

    /// Locks in the contract that the request-logging middleware depends on:
    /// every TilesError → response must carry an `ErrorReason` extension. A
    /// future refactor that drops the `extensions_mut().insert(...)` call
    /// would silently re-empty the `error` field in production Loki logs.
    #[test]
    fn into_response_attaches_error_reason_extension() {
        let err = TilesError::BadRequest("z=22 out of range".to_string());
        let response = err.into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let reason = response
            .extensions()
            .get::<ErrorReason>()
            .expect("ErrorReason must be attached so request_logging_middleware can pick it up");
        assert_eq!(reason.0, "BadRequest: z=22 out of range");
    }
}
