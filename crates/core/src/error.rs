use thiserror::Error;

#[derive(Debug, Error)]
pub enum DataServerError {
    #[error("Collection not found: {0}")]
    CollectionNotFound(String),

    #[error("Location not found: {0}")]
    LocationNotFound(String),

    #[error("Invalid datetime format: {0}")]
    InvalidDatetime(String),

    #[error("CSV error: {0}")]
    Csv(String),

    #[error("Config error: {0}")]
    Config(String),

    #[error("Feature not found: {0}")]
    FeatureNotFound(String),

    #[error("Invalid bbox: {0}")]
    InvalidBbox(String),

    #[error("Invalid parameter: {0}")]
    InvalidParameter(String),

    #[error("GeoJSON error: {0}")]
    GeoJson(String),

    #[error("GeoTIFF error: {0}")]
    GeoTiff(String),

    #[error("Storage error: {0}")]
    Storage(String),

    #[error("Render error: {0}")]
    Render(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
