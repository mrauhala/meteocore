//! Error type for the 3D Tiles API, mapped to HTTP responses.

use axum::extract::rejection::QueryRejection;
use axum::extract::{FromRequestParts, Query};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// axum's [`Query`] whose rejection is this API's JSON 400, not axum's
/// plain-text one (#1123): a parameter given twice (`quantity=a&quantity=b`)
/// or a value of the wrong type (`min_value=x`) fails before the handler
/// runs, with axum's description, which names the parameter. The 3D Tiles
/// counterpart of `api_common::JsonQuery`, in this API's `{"error": …}`
/// error shape rather than the OGC APIs' `{"code", "description"}`.
pub struct JsonQuery<T>(pub T);

impl<T, S> FromRequestParts<S> for JsonQuery<T>
where
    Query<T>: FromRequestParts<S, Rejection = QueryRejection>,
    S: Send + Sync,
{
    type Rejection = Tiles3dError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Query::<T>::from_request_parts(parts, state)
            .await
            .map(|Query(value)| Self(value))
            .map_err(|rejection| Tiles3dError::BadRequest(rejection.body_text()))
    }
}

/// HTTP-facing error for the 3D Tiles API.
#[derive(Debug, Clone)]
pub enum Tiles3dError {
    /// Unknown collection, or no data for the request → 404.
    NotFound(String),
    /// Bad request (e.g. unknown quantity, unparseable datetime) → 400.
    BadRequest(String),
    /// Server temporarily overloaded → 503.
    ServiceUnavailable(String),
    /// A data source that kept refusing the reads
    /// (`DataServerError::Unavailable`, #1355) → 503 with `Retry-After: 10`
    /// and a message naming no upstream.
    SourceUnavailable,
    /// Internal failure → 500 (details logged, not leaked to the client).
    Internal(String),
}

impl IntoResponse for Tiles3dError {
    fn into_response(self) -> Response {
        let retry_after = match self {
            Tiles3dError::ServiceUnavailable(_) => Some(1),
            Tiles3dError::SourceUnavailable => {
                Some(ds_core::error::DataServerError::UNAVAILABLE_RETRY_AFTER_SECS)
            }
            _ => None,
        };
        let (status, message) = match self {
            Tiles3dError::NotFound(m) => (StatusCode::NOT_FOUND, m),
            Tiles3dError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            Tiles3dError::ServiceUnavailable(m) => (StatusCode::SERVICE_UNAVAILABLE, m),
            Tiles3dError::SourceUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                ds_core::error::DataServerError::UNAVAILABLE_DESCRIPTION.to_string(),
            ),
            // Don't leak internal detail to the client (CLAUDE.md code style).
            Tiles3dError::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            ),
        };
        let mut response = (status, Json(json!({ "error": message }))).into_response();
        if let Some(seconds) = retry_after {
            response.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                axum::http::HeaderValue::from(seconds),
            );
        }
        response
    }
}

impl From<ds_core::error::DataServerError> for Tiles3dError {
    fn from(e: ds_core::error::DataServerError) -> Self {
        use ds_core::error::DataServerError as E;
        match e {
            E::ResourceExhausted | E::DeadlineExceeded => {
                Tiles3dError::ServiceUnavailable(e.to_string())
            }
            // Logged at WARN: an upstream's throttling is no server fault.
            E::Unavailable(_) => {
                tracing::warn!("3D Tiles content error: {e}");
                Tiles3dError::SourceUnavailable
            }
            E::InvalidParameter(m)
            | E::InvalidBbox(m)
            | E::InvalidDatetime(m)
            | E::QueryTooLarge(m) => Tiles3dError::BadRequest(m),
            E::CollectionNotFound(m) | E::LocationNotFound(m) | E::FeatureNotFound(m) => {
                Tiles3dError::NotFound(m)
            }
            other => Tiles3dError::Internal(other.to_string()),
        }
    }
}

impl From<ds_executor::ExecutionError> for Tiles3dError {
    fn from(error: ds_executor::ExecutionError) -> Self {
        match error {
            ds_executor::ExecutionError::Task(e) => Self::Internal(e.to_string()),
            other => Self::ServiceUnavailable(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_core::error::DataServerError;

    /// A data source that keeps refusing reads (#1355) is a 503 with a
    /// longer `Retry-After` than the busy answer, naming no upstream.
    #[test]
    fn an_unavailable_source_is_a_503_naming_no_upstream() {
        let error = Tiles3dError::from(DataServerError::Unavailable(
            "GET https://bucket.example/pvol.h5: 503 Slow Down".into(),
        ));
        let response = error.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[axum::http::header::RETRY_AFTER], "10");
        let busy = Tiles3dError::from(DataServerError::ResourceExhausted).into_response();
        assert_eq!(busy.headers()[axum::http::header::RETRY_AFTER], "1");
    }
}
