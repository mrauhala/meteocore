use thiserror::Error;

/// Typed error hierarchy for storage operations.
///
/// Callers can match on specific variants to distinguish not-found from
/// timeout from permission-denied, rather than parsing error strings.
#[derive(Debug, Error)]
pub enum StorageError {
    #[error("Object not found: {0}")]
    NotFound(String),

    #[error("Storage request timed out: {0}")]
    Timeout(String),

    #[error("Permission denied: {0}")]
    PermissionDenied(String),

    #[error("Storage error: {0}")]
    Other(String),
}

impl From<StorageError> for ds_core::error::DataServerError {
    fn from(err: StorageError) -> Self {
        ds_core::error::DataServerError::Storage(err.to_string())
    }
}

impl From<object_store::Error> for StorageError {
    fn from(err: object_store::Error) -> Self {
        match &err {
            object_store::Error::NotFound { path, .. } => StorageError::NotFound(path.clone()),
            object_store::Error::Generic { source, .. }
                if source.to_string().contains("timed out") =>
            {
                StorageError::Timeout(err.to_string())
            }
            object_store::Error::Generic { source, .. }
                if source.to_string().contains("403")
                    || source.to_string().to_lowercase().contains("forbidden")
                    || source.to_string().to_lowercase().contains("access denied") =>
            {
                StorageError::PermissionDenied(err.to_string())
            }
            _ => StorageError::Other(err.to_string()),
        }
    }
}
