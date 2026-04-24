use thiserror::Error;

#[derive(Debug, Error)]
pub enum DataServerError {
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

    #[error("Invalid bbox: {0}")]
    InvalidBbox(String),

    #[error("Invalid parameter: {0}")]
    InvalidParameter(String),

    #[error("Storage error: {0}")]
    Storage(String),

    #[error("Render error: {0}")]
    Render(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
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
