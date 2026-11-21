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

    /// A location, or data at it or in a query's window, that the engine
    /// does not have (404). [`Self::no_data_yet`]'s collection-wide
    /// message reads without the `Location not found` prefix: no location
    /// is involved, as in a Maps map or a Tiles tile (#1500).
    #[error("{}", location_not_found(.0))]
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

/// [`DataServerError::LocationNotFound`]'s message: `Location not found: `
/// and the detail, or [`DataServerError::NO_DATA_YET`] alone.
fn location_not_found(detail: &str) -> std::borrow::Cow<'_, str> {
    if detail == DataServerError::NO_DATA_YET {
        detail.into()
    } else {
        format!("Location not found: {detail}").into()
    }
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

    /// The message of [`Self::no_data_yet`], one wording for every engine.
    pub const NO_DATA_YET: &'static str = "No data available yet";

    /// A collection that holds no data yet (#1499): a fresh source, a store
    /// unreachable since the start, a mistyped prefix. The collection exists
    /// but has nothing to serve, so this is a [`Self::LocationNotFound`]
    /// (404), which a `datetime` list skips like any instant without data,
    /// never an [`Self::Engine`] error, whose 500 is logged at ERROR as a
    /// server fault. Its health reports the condition itself.
    pub fn no_data_yet() -> Self {
        Self::LocationNotFound(Self::NO_DATA_YET.into())
    }

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

    /// Every [`Self::failure_reason`], a bounded metrics label set.
    pub const FAILURE_REASONS: [&'static str; 4] =
        ["not_found", "invalid_request", "storage", "internal"];

    /// Why a request failed, as a bounded metrics label: the data it named
    /// is absent (`not_found`, [`ErrorClass::NotFound`]), the engine refused
    /// the request (`invalid_request`, [`ErrorClass::BadRequest`]), a source
    /// read failed (`storage`: [`Self::Storage`], [`Self::Io`]) or anything
    /// else (`internal`). Of the errors that are not transient, `storage`
    /// and `internal` are exactly [`ErrorClass::Internal`]; a transient one
    /// is `internal`. `storage` is finer than the class, which is why this
    /// is a mapping of its own; it follows the class otherwise, so a new
    /// variant needs only its class (#1484).
    ///
    /// WMS labels its error tiles with it (`wms_error_tiles_total`, #1375),
    /// and the `WmsErrorTiles` alert counts `storage` and `internal`
    /// (#1476): a data fault an engine reports with a client class is a
    /// tile the alert misses (#1506).
    pub const fn failure_reason(&self) -> &'static str {
        match self.class() {
            ErrorClass::NotFound => "not_found",
            ErrorClass::BadRequest => "invalid_request",
            ErrorClass::Internal => match self {
                Self::Storage(_) | Self::Io(_) => "storage",
                _ => "internal",
            },
            ErrorClass::Busy | ErrorClass::DeadlineExceeded | ErrorClass::SourceUnavailable => {
                "internal"
            }
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

    /// Every variant's failure reason (#1375, #1484), the labels
    /// `wms_error_tiles_total` has always had: `storage` and `internal` are
    /// exactly the class-`Internal` errors among those that are not
    /// transient, which the `WmsErrorTiles` alert relies on (#1476).
    #[test]
    fn every_variant_has_its_failure_reason() {
        use DataServerError as E;
        let s = || String::from("x");
        let cases = [
            (E::DeadlineExceeded, "internal"),
            (E::ResourceExhausted, "internal"),
            (E::CollectionNotFound(s()), "not_found"),
            (E::LocationNotFound(s()), "not_found"),
            (E::InvalidDatetime(s()), "invalid_request"),
            (E::Engine(s()), "internal"),
            (E::Config(s()), "internal"),
            (E::FeatureNotFound(s()), "not_found"),
            (E::ReferenceTimeNotFound(s()), "not_found"),
            (E::InvalidBbox(s()), "invalid_request"),
            (E::QueryTooLarge(s()), "invalid_request"),
            (E::InvalidParameter(s()), "invalid_request"),
            (E::Storage(s()), "storage"),
            (E::Unavailable(s()), "internal"),
            (E::Render(s()), "internal"),
            (E::Io(std::io::Error::other("x")), "storage"),
        ];
        for (error, reason) in cases {
            assert_eq!(error.failure_reason(), reason, "{error:?}");
            assert!(
                DataServerError::FAILURE_REASONS.contains(&reason),
                "{error:?}"
            );
            if !error.class().is_transient() {
                assert_eq!(
                    matches!(reason, "storage" | "internal"),
                    error.class() == ErrorClass::Internal,
                    "{error:?}"
                );
            }
        }
    }

    /// A collection without data yet is a not-found, which a `datetime`
    /// list skips, never an internal error (#1499). Its message names no
    /// location, since none is involved (#1500); every other not-found
    /// location keeps the prefix.
    #[test]
    fn no_data_yet_is_a_location_not_found() {
        let error = DataServerError::no_data_yet();
        assert!(
            matches!(&error, DataServerError::LocationNotFound(m) if m == "No data available yet"),
            "{error:?}"
        );
        assert_eq!(error.class(), ErrorClass::NotFound);
        assert_eq!(error.to_string(), "No data available yet");
        assert_eq!(
            DataServerError::LocationNotFound("no data then".into()).to_string(),
            "Location not found: no data then"
        );
        assert_eq!(
            DataServerError::LocationNotFound(String::new()).to_string(),
            "Location not found: "
        );
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
