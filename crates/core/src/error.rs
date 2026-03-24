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

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
