//! Error type for the 3D Tiles API, mapped to HTTP responses.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// HTTP-facing error for the 3D Tiles API.
#[derive(Debug, Clone)]
pub enum Tiles3dError {
    /// Unknown collection, or no data for the request → 404.
    NotFound(String),
    /// Bad request (e.g. unknown quantity, unparseable datetime) → 400.
    BadRequest(String),
    /// Server temporarily overloaded → 503.
    ServiceUnavailable(String),
    /// Internal failure → 500 (details logged, not leaked to the client).
    Internal(String),
}

impl IntoResponse for Tiles3dError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Tiles3dError::NotFound(m) => (StatusCode::NOT_FOUND, m),
            Tiles3dError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            Tiles3dError::ServiceUnavailable(m) => (StatusCode::SERVICE_UNAVAILABLE, m),
            // Don't leak internal detail to the client (CLAUDE.md code style).
            Tiles3dError::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            ),
        };
        let mut response = (status, Json(json!({ "error": message }))).into_response();
        if status == StatusCode::SERVICE_UNAVAILABLE {
            response.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                axum::http::HeaderValue::from_static("1"),
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
