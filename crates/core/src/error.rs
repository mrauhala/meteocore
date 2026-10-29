use thiserror::Error;

#[derive(Debug, Error)]
pub enum DataServerError {
    /// An interactive request exhausted its absolute time budget
    /// (HTTP 503 for renders, HTTP 504 for EDR queries).
    #[error("Render deadline exceeded, try again later")]
    DeadlineExceeded,
    /// Shared transient resource admission failed; retry later (HTTP 503).
    #[error("Server busy, try again later")]
    ResourceExhausted,
    #[error("Collection not found: {0}")]
    CollectionNotFound(String),

    #[error("Location not found: {0}")]
    LocationNotFound(String),

    #[error("Invalid datetime format: {0}")]
    InvalidDatetime(String),

    #[error("Engine error: {0}")]
    Engine(String),

    #[error("Config error: {0}")]
    Config(String),

    #[error("Feature not found: {0}")]
    FeatureNotFound(String),

    /// A syntactically valid forecast model run (reference time / EDR instance)
    /// that the engine does not have. Maps to HTTP 404 (the resource is absent),
    /// not 400 (the request is well-formed). See `ds_core::instances`.
    #[error("Reference time not found: {0}")]
    ReferenceTimeNotFound(String),

    #[error("Invalid bbox: {0}")]
    InvalidBbox(String),

    /// A well-formed request whose result set would exceed a hard server cap
    /// (station count, row count). Maps to HTTP 400 — the client remediates by
    /// narrowing the polygon, bbox, or time range — never 500: the server is
    /// healthy and the message is safe to return verbatim.
    #[error("Query too large: {0}")]
    QueryTooLarge(String),

    #[error("Invalid parameter: {0}")]
    InvalidParameter(String),

    #[error("Storage error: {0}")]
    Storage(String),

    /// An upstream data source that kept refusing a read through all of the
    /// store's retries: its last answer was a 429, 502, 503 or 504 (#1355).
    /// Every API answers it with HTTP 503 and `Retry-After`
    /// ([`Self::UNAVAILABLE_RETRY_AFTER_SECS`]) and the generic
    /// [`Self::UNAVAILABLE_DESCRIPTION`]: the detail names the upstream, so
    /// it goes to the log only, at WARN.
    #[error("Data source unavailable: {0}")]
    Unavailable(String),

    #[error("Render error: {0}")]
    Render(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

impl DataServerError {
    /// The `Retry-After` of an [`Self::Unavailable`] answer, in seconds.
    /// Longer than the busy answer's 1 s: an upstream that throttles keeps
    /// at it for minutes.
    pub const UNAVAILABLE_RETRY_AFTER_SECS: u64 = 10;

    /// The client-facing description of an [`Self::Unavailable`] answer. It
    /// names no upstream (root CLAUDE.md, Critical Rule 11).
    pub const UNAVAILABLE_DESCRIPTION: &'static str =
        "Data source temporarily unavailable, try again later";

    /// A store read's error with `context` added, as an engine that wraps
    /// one reports it: an [`Self::Unavailable`] stays one, so the APIs still
    /// answer it with their 503 (#1355), and any other error becomes a
    /// [`Self::Storage`] naming it.
    pub fn with_storage_context(self, context: impl std::fmt::Display) -> Self {
        match self {
            Self::Unavailable(detail) => Self::Unavailable(format!("{context}: {detail}")),
            other => Self::Storage(format!("{context}: {other}")),
        }
    }
}

/// Carries a human-readable error reason across the axum middleware stack via
/// response extensions. Each API crate attaches one of these when mapping an
/// internal error to a ≥400 HTTP response; the request-logging middleware in
/// the server crate reads it back out so the reason ends up in Loki as an
/// `error` field alongside the status code.
///
/// Lives in ds-core because it straddles multiple API crates — but has no
/// framework dependency itself, which keeps ds-core framework-free.
#[derive(Debug, Clone)]
pub struct ErrorReason(pub String);

#[cfg(test)]
mod tests {
    use super::*;

    /// An upstream's refusal keeps its variant through an engine's context;
    /// any other error becomes the storage error it always was.
    #[test]
    fn storage_context_keeps_an_unavailable_source() {
        let wrapped = DataServerError::Unavailable("503 Slow Down".into())
            .with_storage_context("Failed to fetch GRIB message 10u/10");
        match wrapped {
            DataServerError::Unavailable(detail) => {
                assert_eq!(detail, "Failed to fetch GRIB message 10u/10: 503 Slow Down")
            }
            other => panic!("{other:?}"),
        }
        let wrapped = DataServerError::Storage("connection reset".into())
            .with_storage_context("Failed to fetch GRIB message 10u/10");
        match wrapped {
            DataServerError::Storage(detail) => assert_eq!(
                detail,
                "Failed to fetch GRIB message 10u/10: Storage error: connection reset"
            ),
            other => panic!("{other:?}"),
        }
    }
}
