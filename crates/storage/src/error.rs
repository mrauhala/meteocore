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

/// The refusal a failed range read stands for, if it is one no retry can
/// change (#1074). object_store types a 404 as `NotFound` on every store
/// and a 401/403 as `PermissionDenied`/`Unauthenticated` on the cloud
/// stores. Anything else, a 416 included and a 403 from its HTTP store,
/// arrives as `Generic`, with the status only in the text of its private
/// request error, which [`http_status`] reads.
pub(crate) fn range_refusal(err: &object_store::Error) -> Option<crate::RangeRefusal> {
    use crate::RangeRefusal;
    match err {
        object_store::Error::NotFound { .. } => Some(RangeRefusal::NotFound),
        object_store::Error::PermissionDenied { .. }
        | object_store::Error::Unauthenticated { .. } => Some(RangeRefusal::Forbidden),
        _ => match http_status(err)? {
            404 => Some(RangeRefusal::NotFound),
            401 | 403 => Some(RangeRefusal::Forbidden),
            416 => Some(RangeRefusal::Truncated),
            _ => None,
        },
    }
}

/// The HTTP status in an object_store error's source chain, whose request
/// error reads "Server returned non-2xx status code: 416 Range Not
/// Satisfiable: …". That type is private, so this reads its text;
/// `range_tests` pins the format against a real HTTP server.
fn http_status(err: &(dyn std::error::Error + 'static)) -> Option<u16> {
    let mut next = Some(err);
    while let Some(err) = next {
        if let Some(rest) = err
            .to_string()
            .strip_prefix("Server returned non-2xx status code: ")
        {
            return rest.get(..3)?.parse().ok();
        }
        next = err.source();
    }
    None
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
