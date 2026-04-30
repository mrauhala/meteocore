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
    /// Server too busy (render semaphore exhausted).
    ServiceUnavailable(String),
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
            // Like Internal, drop the inner string structurally — the only
            // current call site is render_semaphore-exhausted with a fixed
            // string, but ignoring the payload here means a future caller
            // can't accidentally leak internal state via this variant.
            MapsError::ServiceUnavailable(_) => (
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
    /// every MapsError → response must carry an `ErrorReason` extension. A
    /// future refactor that drops the `extensions_mut().insert(...)` call
    /// would silently re-empty the `error` field in production Loki logs.
    #[test]
    fn into_response_attaches_error_reason_extension() {
        let err = MapsError::NotFound("collection 'foo' not found".to_string());
        let response = err.into_response();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let reason = response
            .extensions()
            .get::<ErrorReason>()
            .expect("ErrorReason must be attached so request_logging_middleware can pick it up");
        assert_eq!(reason.0, "NotFound: collection 'foo' not found");
    }

    #[test]
    fn into_response_redacts_internal_message_in_reason() {
        // The XML body shows a generic "Internal server error" so we don't
        // leak panic messages to clients; the reason carries the same text.
        // (Engineers triaging these read the underlying tracing::error from
        // the handler itself, not this field.)
        let err = MapsError::Internal("oops, secret detail".to_string());
        let response = err.into_response();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let reason = response
            .extensions()
            .get::<ErrorReason>()
            .expect("attached");
        assert_eq!(reason.0, "ServerError: Internal server error");
    }

    #[test]
    fn into_response_redacts_service_unavailable_message() {
        // The only current call site passes a safe fixed string, but the
        // variant must structurally drop its payload — defence-in-depth
        // against a future caller passing internal state.
        let err = MapsError::ServiceUnavailable("semaphore poisoned: secret".to_string());
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
