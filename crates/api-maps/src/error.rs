use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use ds_core::error::{DataServerError, ErrorClass};
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
    /// Server busy: the render was not admitted or ran out of time, or the
    /// engine was busy (`ErrorClass::Busy`, `ErrorClass::DeadlineExceeded`).
    /// A 503 with `Retry-After: 1` and a fixed message: it carries nothing a
    /// caller could fill with internal state (#122).
    ServiceUnavailable,
    /// A data source that kept refusing the render's reads
    /// (`DataServerError::Unavailable`, #1355): a 503 with `Retry-After: 10`.
    SourceUnavailable,
}

impl MapsError {
    /// The answer to the engine's `error` rendering `collection`: its
    /// [`ErrorClass`], as every API answers it (#17). A client mistake (a
    /// multi-parameter PVOL collection rendered without a
    /// `<site>:<quantity>` parameter, a bad bbox or datetime) is a 400 with
    /// the engine's message, and an absent resource a 404 (a pinned run
    /// pruned between resolution and render, routine for nowcast
    /// generations, #522): both traced at DEBUG, so a misconfigured client
    /// stays diagnosable without flooding the WARN stream. A data source's
    /// refusal and an internal failure are logged at WARN, the detail never
    /// in the body.
    pub(crate) fn from_engine(e: DataServerError, collection: &str) -> Self {
        match e.class() {
            ErrorClass::Busy | ErrorClass::DeadlineExceeded => MapsError::ServiceUnavailable,
            ErrorClass::SourceUnavailable => {
                tracing::warn!("Maps render error for collection '{collection}': {e}");
                MapsError::SourceUnavailable
            }
            ErrorClass::BadRequest => {
                tracing::debug!("Maps render bad-request for collection '{collection}': {e}");
                MapsError::BadRequest(e.to_string())
            }
            ErrorClass::NotFound => {
                tracing::debug!("Maps render not-found for collection '{collection}': {e}");
                MapsError::NotFound(e.to_string())
            }
            ErrorClass::Internal => {
                tracing::warn!("Maps render error for collection '{collection}': {e}");
                MapsError::Internal(format!("Render failed: {e}"))
            }
        }
    }
}

impl IntoResponse for MapsError {
    fn into_response(self) -> axum::response::Response {
        let retry_after = match &self {
            MapsError::ServiceUnavailable => Some(1),
            MapsError::SourceUnavailable => Some(DataServerError::UNAVAILABLE_RETRY_AFTER_SECS),
            _ => None,
        };
        let (status, code, description) = match &self {
            MapsError::NotFound(msg) => (StatusCode::NOT_FOUND, "NotFound", msg.as_str()),
            MapsError::BadRequest(msg) => (StatusCode::BAD_REQUEST, "BadRequest", msg.as_str()),
            MapsError::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "ServerError",
                "Internal server error",
            ),
            MapsError::ServiceUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "ServerBusy",
                "Server busy, try again later",
            ),
            // Names no upstream: the handler logs the detail at WARN.
            MapsError::SourceUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                api_common::SOURCE_UNAVAILABLE,
                DataServerError::UNAVAILABLE_DESCRIPTION,
            ),
        };

        let reason = ds_core::error::ErrorReason(format!("{code}: {description}"));
        let mut response = (
            status,
            Json(json!({ "code": code, "description": description })),
        )
            .into_response();
        if let Some(seconds) = retry_after {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, axum::http::HeaderValue::from(seconds));
        }
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
    fn service_unavailable_is_a_fixed_busy_answer() {
        let response = MapsError::ServiceUnavailable.into_response();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let reason = response
            .extensions()
            .get::<ErrorReason>()
            .expect("attached");
        assert_eq!(reason.0, "ServerBusy: Server busy, try again later");
        assert_eq!(response.headers()[header::RETRY_AFTER], "1");
    }

    /// A data source that keeps refusing reads (#1355) is its own 503: a
    /// longer `Retry-After` than the busy answer, and a description naming
    /// no upstream.
    #[test]
    fn source_unavailable_is_a_503_with_a_longer_retry_after() {
        let response = MapsError::SourceUnavailable.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "10");
        let reason = response.extensions().get::<ErrorReason>().unwrap();
        assert_eq!(
            reason.0,
            "SourceUnavailable: Data source temporarily unavailable, try again later"
        );
    }
}
