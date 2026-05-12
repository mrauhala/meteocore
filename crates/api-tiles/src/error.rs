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
    /// Request is well-formed but the server can't satisfy it as-is — e.g.
    /// a tile whose feature count exceeds the density cap. Maps to HTTP 422
    /// (Unprocessable Content), the closest match in lieu of an OGC-specific
    /// status.
    Unprocessable(String),
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
            TilesError::Unprocessable(msg) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "Unprocessable",
                msg.as_str(),
            ),
            TilesError::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "ServerError",
                "Internal server error",
            ),
            // Like Internal, drop the inner string structurally — the only
            // current call site is render_semaphore-exhausted with a fixed
            // string, but ignoring the payload here means a future caller
            // can't accidentally leak internal state via this variant.
            TilesError::ServiceUnavailable(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "ServerBusy",
                "Server busy, try again later",
            ),
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

    #[test]
    fn into_response_redacts_internal_message() {
        let err = TilesError::Internal("connection refused at 10.0.0.5:5432".to_string());
        let response = err.into_response();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let reason = response
            .extensions()
            .get::<ErrorReason>()
            .expect("attached");
        assert_eq!(reason.0, "ServerError: Internal server error");
        assert!(
            !reason.0.contains("10.0.0.5"),
            "inner detail must not leak via ErrorReason, got {:?}",
            reason.0
        );
    }

    #[test]
    fn into_response_redacts_service_unavailable_message() {
        // The only current call site passes a safe fixed string, but the
        // variant must structurally drop its payload — defence-in-depth
        // against a future caller passing internal state.
        let err = TilesError::ServiceUnavailable("semaphore poisoned: secret".to_string());
        let response = err.into_response();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let reason = response
            .extensions()
            .get::<ErrorReason>()
            .expect("attached");
        assert_eq!(reason.0, "ServerBusy: Server busy, try again later");
        assert!(
            !reason.0.contains("secret"),
            "inner detail must not leak via ErrorReason, got {:?}",
            reason.0
        );
    }
}
