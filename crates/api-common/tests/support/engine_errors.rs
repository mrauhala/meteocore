//! One `DataServerError` of every variant, for the suites that check each
//! API answers an engine's error with its class's status through a real
//! handler (#17). Shared, as a `#[path]` module, by api-edr, api-features,
//! api-maps, api-tiles, api-wms, api-3dtiles and api-mcp.
#![allow(dead_code)]

use ds_core::error::DataServerError;

/// The detail every error carries, which no 5xx answer may (root CLAUDE.md,
/// Critical Rule 11).
pub const SECRET: &str = "secret at /var/lib/meteocore, 10.0.0.5:5432";

/// One error of every variant, each carrying [`SECRET`]. A test engine
/// keeps an index into it and makes the error afresh per call
/// (`every_error().swap_remove(i)`): `DataServerError` is not `Clone`.
pub fn every_error() -> Vec<DataServerError> {
    let detail = || SECRET.to_string();
    vec![
        DataServerError::DeadlineExceeded,
        DataServerError::ResourceExhausted,
        DataServerError::CollectionNotFound(detail()),
        DataServerError::LocationNotFound(detail()),
        DataServerError::InvalidDatetime(detail()),
        DataServerError::Engine(detail()),
        DataServerError::Config(detail()),
        DataServerError::FeatureNotFound(detail()),
        DataServerError::ReferenceTimeNotFound(detail()),
        DataServerError::InvalidBbox(detail()),
        DataServerError::QueryTooLarge(detail()),
        DataServerError::InvalidParameter(detail()),
        DataServerError::Storage(detail()),
        DataServerError::Unavailable(detail()),
        DataServerError::Render(detail()),
        DataServerError::Io(std::io::Error::other(detail())),
    ]
}

/// The `Retry-After` a 503 of `error`'s class carries: the busy answer's 1,
/// a data source's 10 (#1355); `None` for any other class.
pub fn retry_after(error: &DataServerError) -> Option<&'static str> {
    use ds_core::error::ErrorClass;
    match error.class() {
        ErrorClass::Busy | ErrorClass::DeadlineExceeded => Some("1"),
        ErrorClass::SourceUnavailable => Some("10"),
        ErrorClass::BadRequest | ErrorClass::NotFound | ErrorClass::Internal => None,
    }
}
