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

    /// The store kept refusing the request through all of object_store's
    /// retries, its last answer a 429, 502, 503 or 504 ([`unavailable`]):
    /// an upstream that throttles or is down for now (#1355).
    #[error("Storage unavailable: {0}")]
    Unavailable(String),

    #[error("Storage error: {0}")]
    Other(String),
}

/// An `Unavailable` store stays [`DataServerError::Unavailable`], with the
/// store's error as its detail, so the APIs answer it with their 503; every
/// other failure is a [`DataServerError::Storage`].
///
/// [`DataServerError::Unavailable`]: ds_core::error::DataServerError::Unavailable
/// [`DataServerError::Storage`]: ds_core::error::DataServerError::Storage
impl From<StorageError> for ds_core::error::DataServerError {
    fn from(err: StorageError) -> Self {
        match err {
            StorageError::Unavailable(detail) => {
                ds_core::error::DataServerError::Unavailable(detail)
            }
            err => ds_core::error::DataServerError::Storage(err.to_string()),
        }
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

/// Whether a failed request's last answer says the store is unavailable
/// for now rather than refusing it for good (#1355): 429 Too Many Requests
/// (a throttle), 502 Bad Gateway, 503 Service Unavailable (an S3 bucket's
/// `SlowDown`) or 504 Gateway Timeout. object_store retries each of them
/// with back-off, so one that reaches an error outlasted every retry.
fn unavailable(err: &object_store::Error) -> bool {
    matches!(http_status(err), Some(429 | 502 | 503 | 504))
}

impl From<object_store::Error> for StorageError {
    fn from(err: object_store::Error) -> Self {
        match &err {
            object_store::Error::NotFound { path, .. } => StorageError::NotFound(path.clone()),
            _ if unavailable(&err) => StorageError::Unavailable(err.to_string()),
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

#[cfg(test)]
mod tests {
    use ds_core::error::DataServerError;
    use object_store::path::Path;
    use object_store::ObjectStoreExt;

    use super::*;
    use crate::test_store::{RangeServer, QUICK_S3_RETRY};

    /// An S3 bucket holding `object`, on a server answering every request
    /// for it with `status` when one is given.
    fn bucket(status: Option<&'static str>) -> (RangeServer, crate::DataStore) {
        let server = RangeServer::new(b"0123456789", "2026-10-29T08:00:00Z".parse().unwrap());
        {
            let mut object = server.object.lock().unwrap();
            object.path = "/bucket/object".into();
            object.status = status;
        }
        let store = crate::s3_store_from_parts(&server.url, "bucket", QUICK_S3_RETRY).unwrap();
        (server, store)
    }

    /// A bucket that answers every attempt with a 429, 502, 503 (an S3
    /// `SlowDown`) or 504 outlasts object_store's retries: the store is
    /// `Unavailable` (#1355), and so is every read of it, a refusal read
    /// included, with the status in its detail for the log. A 500 that
    /// outlasts them stays a storage error.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_bucket_that_keeps_refusing_is_unavailable() {
        let path = Path::from("object");
        for status in [
            "429 Too Many Requests",
            "502 Bad Gateway",
            "503 Slow Down",
            "504 Gateway Timeout",
        ] {
            let (_server, store) = bucket(Some(status));
            let raw = store.inner().get(&path).await.unwrap_err();
            assert!(
                matches!(StorageError::from(raw), StorageError::Unavailable(_)),
                "{status}"
            );
            let reads = [
                store.get(&path).map(drop),
                store.get_range(&path, 2..5).map(drop),
                store.head(&path).map(drop),
                store.get_range_or_refusal(&path, 2..5).map(drop),
            ];
            for read in reads {
                match read {
                    Err(DataServerError::Unavailable(detail)) => {
                        assert!(detail.contains(&status[..3]), "{detail}")
                    }
                    other => panic!("{status}: {other:?}"),
                }
            }
        }
        let (_server, store) = bucket(Some("500 Internal Server Error"));
        assert!(matches!(store.get(&path), Err(DataServerError::Storage(_))));
    }

    /// A bucket that throttles twice and then answers serves the read:
    /// the retries outlast a passing `SlowDown`.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_passing_slowdown_serves_the_read() {
        let (server, store) = bucket(None);
        server
            .object
            .lock()
            .unwrap()
            .fail_next
            .extend(["503 Slow Down", "503 Slow Down"]);
        let read = store.get_range(&Path::from("object"), 2..5).unwrap();
        assert_eq!(read, &b"234"[..]);
        assert!(server.object.lock().unwrap().fail_next.is_empty());
    }
}
