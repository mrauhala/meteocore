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

    /// A store failure the store did not classify as worth a retry: an
    /// internal error (HTTP 500), not a 503 (#17). One it did classify is
    /// [`Self::Unavailable`].
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

    /// The [`ErrorClass`] that decides how every API answers this error
    /// (#17). A [`Self::Storage`] error is internal: only a failure the
    /// store classified as worth a retry is answered as one, and that is an
    /// [`Self::Unavailable`].
    pub const fn class(&self) -> ErrorClass {
        match self {
            Self::InvalidDatetime(_)
            | Self::InvalidBbox(_)
            | Self::QueryTooLarge(_)
            | Self::InvalidParameter(_) => ErrorClass::BadRequest,
            Self::CollectionNotFound(_)
            | Self::LocationNotFound(_)
            | Self::FeatureNotFound(_)
            | Self::ReferenceTimeNotFound(_) => ErrorClass::NotFound,
            Self::ResourceExhausted => ErrorClass::Busy,
            Self::DeadlineExceeded => ErrorClass::DeadlineExceeded,
            Self::Unavailable(_) => ErrorClass::SourceUnavailable,
            Self::Engine(_)
            | Self::Config(_)
            | Self::Storage(_)
            | Self::Render(_)
            | Self::Io(_) => ErrorClass::Internal,
        }
    }
}

/// How every API answers a [`DataServerError`] (#17): each API crate's error
/// mapping matches on [`DataServerError::class`], never on the variants, so
/// a variant cannot answer one status in one API and another elsewhere.
/// ds-core has no HTTP dependency, so the status is a number
/// ([`Self::status_code`]); each API keeps its own body format, and a 5xx
/// body never carries the error's detail (root CLAUDE.md, Critical Rule 11).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorClass {
    /// A request the client can remedy: an invalid parameter, bbox or
    /// datetime, or a result over a hard server cap. 400, with the error's
    /// message, which is safe to return.
    BadRequest,
    /// The collection, location, feature or model run the request names is
    /// absent: 404.
    NotFound,
    /// Shared transient resource admission failed: 503 with
    /// `Retry-After: 1`.
    Busy,
    /// The request exhausted its absolute time budget: 503 with
    /// `Retry-After: 1`, except from an EDR query: 504.
    DeadlineExceeded,
    /// A data source kept refusing the read: 503 with `Retry-After`
    /// [`DataServerError::UNAVAILABLE_RETRY_AFTER_SECS`] and
    /// [`DataServerError::UNAVAILABLE_DESCRIPTION`]. The detail names the
    /// upstream, so it goes to the log only, at WARN.
    SourceUnavailable,
    /// Anything else, a store failure the store did not classify included:
    /// 500 with a generic message, the detail logged.
    Internal,
}

impl ErrorClass {
    /// The HTTP status of an answer in this class. An EDR query answers a
    /// [`Self::DeadlineExceeded`] 504 instead of this 503.
    pub const fn status_code(self) -> u16 {
        match self {
            Self::BadRequest => 400,
            Self::NotFound => 404,
            Self::Busy | Self::DeadlineExceeded | Self::SourceUnavailable => 503,
            Self::Internal => 500,
        }
    }

    /// Whether the same request may succeed later: the server or a data
    /// source is busy, or the deadline ran out. A WMS GetMap answers such an
    /// error with its 503, never the red error tile.
    pub const fn is_transient(self) -> bool {
        matches!(
            self,
            Self::Busy | Self::DeadlineExceeded | Self::SourceUnavailable
        )
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

    /// Every variant's class and status, as `DataServerError`'s docs state
    /// them (#17). A storage failure the store did not classify is a 500,
    /// not a 503: only an `Unavailable` source asks for a retry.
    #[test]
    fn every_variant_has_its_documented_class_and_status() {
        use ErrorClass as C;
        let s = || String::from("x");
        let cases = [
            (DataServerError::DeadlineExceeded, C::DeadlineExceeded, 503),
            (DataServerError::ResourceExhausted, C::Busy, 503),
            (DataServerError::CollectionNotFound(s()), C::NotFound, 404),
            (DataServerError::LocationNotFound(s()), C::NotFound, 404),
            (DataServerError::InvalidDatetime(s()), C::BadRequest, 400),
            (DataServerError::Engine(s()), C::Internal, 500),
            (DataServerError::Config(s()), C::Internal, 500),
            (DataServerError::FeatureNotFound(s()), C::NotFound, 404),
            (
                DataServerError::ReferenceTimeNotFound(s()),
                C::NotFound,
                404,
            ),
            (DataServerError::InvalidBbox(s()), C::BadRequest, 400),
            (DataServerError::QueryTooLarge(s()), C::BadRequest, 400),
            (DataServerError::InvalidParameter(s()), C::BadRequest, 400),
            (DataServerError::Storage(s()), C::Internal, 500),
            (DataServerError::Unavailable(s()), C::SourceUnavailable, 503),
            (DataServerError::Render(s()), C::Internal, 500),
            (
                DataServerError::Io(std::io::Error::other("x")),
                C::Internal,
                500,
            ),
        ];
        for (error, class, status) in cases {
            assert_eq!(error.class(), class, "{error:?}");
            assert_eq!(error.class().status_code(), status, "{error:?}");
            assert_eq!(
                error.class().is_transient(),
                status == 503,
                "{error:?}: only a 503 asks for a retry"
            );
        }
        // An upstream's refusal keeps its class through an engine's
        // context; any other store failure is internal.
        let wrapped = DataServerError::Unavailable(s()).with_storage_context("GRIB");
        assert_eq!(wrapped.class(), C::SourceUnavailable);
        let wrapped = DataServerError::LocationNotFound(s()).with_storage_context("GRIB");
        assert_eq!(wrapped.class(), C::Internal);
    }

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
