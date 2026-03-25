//! Shared storage abstraction for the data server.
//!
//! Provides a synchronous `DataStore` over the `object_store` crate,
//! supporting local filesystem, S3, and HTTP backends.

use std::ops::Range;
use std::sync::Arc;

use bytes::Bytes;
use ds_core::error::DataServerError;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectMeta, ObjectStore};

pub use bytes;
pub use object_store;

/// Synchronous wrapper around an `ObjectStore`.
///
/// Methods use `tokio::runtime::Handle::current().block_on()` to bridge
/// async `ObjectStore` operations into synchronous calls. This is safe
/// when called from a thread with access to a tokio runtime handle
/// (e.g., from `spawn_blocking` or during startup).
#[derive(Clone)]
pub struct DataStore {
    inner: Arc<dyn ObjectStore>,
}

impl DataStore {
    pub fn new(store: Arc<dyn ObjectStore>) -> Self {
        Self { inner: store }
    }

    /// Get the entire contents of an object.
    pub fn get(&self, path: &ObjectPath) -> Result<Bytes, DataServerError> {
        self.block_on(async {
            let result = self.inner.get(path).await?;
            Ok(result.bytes().await?)
        })
    }

    /// Get a byte range from an object.
    pub fn get_range(&self, path: &ObjectPath, range: Range<usize>) -> Result<Bytes, DataServerError> {
        self.block_on(async { Ok(self.inner.get_range(path, range).await?) })
    }

    /// List objects under a prefix.
    pub fn list(&self, prefix: &ObjectPath) -> Result<Vec<ObjectMeta>, DataServerError> {
        self.block_on(async {
            use futures::TryStreamExt;
            Ok(self.inner.list(Some(prefix)).try_collect().await?)
        })
    }

    /// Get object metadata (size, last modified, etc.).
    pub fn head(&self, path: &ObjectPath) -> Result<ObjectMeta, DataServerError> {
        self.block_on(async { Ok(self.inner.head(path).await?) })
    }

    /// Bridge async to sync. Uses `block_in_place` when inside a tokio runtime
    /// (safe from async handlers via multi-threaded scheduler), or creates a
    /// temporary runtime otherwise (for use in non-async contexts like startup).
    fn block_on<F, T>(&self, future: F) -> Result<T, DataServerError>
    where
        F: std::future::Future<Output = Result<T, object_store::Error>>,
    {
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => tokio::task::block_in_place(|| {
                handle.block_on(future)
            })
            .map_err(|e| DataServerError::Storage(format!("{e}"))),
            Err(_) => {
                // No runtime — create a temporary one (e.g., in tests or CLI tools)
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| DataServerError::Storage(format!("Cannot create runtime: {e}")))?;
                rt.block_on(future)
                    .map_err(|e| DataServerError::Storage(format!("{e}")))
            }
        }
    }

    /// Get the underlying async ObjectStore for use in async contexts
    /// (e.g., the catalog poll loop).
    pub fn inner(&self) -> &Arc<dyn ObjectStore> {
        &self.inner
    }
}

impl std::fmt::Debug for DataStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataStore").finish()
    }
}

/// Build a `DataStore` from a data path string.
///
/// Auto-detects the backend from the path prefix:
/// - `s3://bucket/prefix/` → Amazon S3 (credentials from AWS standard chain)
/// - `https://...` or `http://...` → HTTP object store
/// - Anything else → Local filesystem
///
/// Returns the store and the base path within that store.
pub fn build_store(data_path: &str) -> Result<(DataStore, ObjectPath), DataServerError> {
    if data_path.starts_with("s3://") {
        build_s3_store(data_path)
    } else if is_s3_http_url(data_path) {
        build_s3_from_http_url(data_path)
    } else if data_path.starts_with("http://") || data_path.starts_with("https://") {
        build_http_store(data_path)
    } else {
        build_local_store(data_path)
    }
}

/// Detect S3-style HTTP URLs like https://s3-eu-west-1.amazonaws.com/bucket/...
/// or https://bucket.s3.region.amazonaws.com/...
fn is_s3_http_url(url: &str) -> bool {
    url.contains(".amazonaws.com/") || url.contains(".cloudferro.com/")
}

fn build_local_store(data_path: &str) -> Result<(DataStore, ObjectPath), DataServerError> {
    let abs_path = std::path::Path::new(data_path)
        .canonicalize()
        .map_err(|e| DataServerError::Storage(format!("Cannot resolve path {data_path}: {e}")))?;

    let store = object_store::local::LocalFileSystem::new_with_prefix(&abs_path)
        .map_err(|e| DataServerError::Storage(format!("Cannot create local store at {data_path}: {e}")))?;

    Ok((DataStore::new(Arc::new(store)), ObjectPath::from("")))
}

fn build_s3_store(data_path: &str) -> Result<(DataStore, ObjectPath), DataServerError> {
    // Parse s3://bucket/prefix/path/
    let without_scheme = data_path
        .strip_prefix("s3://")
        .ok_or_else(|| DataServerError::Storage("Expected s3:// prefix".into()))?;

    let (bucket, prefix) = match without_scheme.find('/') {
        Some(idx) => (&without_scheme[..idx], &without_scheme[idx + 1..]),
        None => (without_scheme, ""),
    };

    let store = object_store::aws::AmazonS3Builder::from_env()
        .with_bucket_name(bucket)
        .build()
        .map_err(|e| DataServerError::Storage(format!("Cannot create S3 store for bucket '{bucket}': {e}")))?;

    let prefix_path = ObjectPath::from(prefix.trim_end_matches('/'));
    Ok((DataStore::new(Arc::new(store)), prefix_path))
}

/// Parse an S3 HTTP URL into bucket + prefix and build an S3 store.
/// Handles both path-style (s3-region.amazonaws.com/bucket/prefix)
/// and virtual-hosted (bucket.s3.region.amazonaws.com/prefix) formats.
fn build_s3_from_http_url(data_path: &str) -> Result<(DataStore, ObjectPath), DataServerError> {
    let url = url::Url::parse(data_path)
        .map_err(|e| DataServerError::Storage(format!("Invalid URL {data_path}: {e}")))?;

    let host = url.host_str().unwrap_or("");
    let path = url.path().trim_start_matches('/');

    // Determine endpoint, bucket, prefix, and region
    let (endpoint, bucket, prefix, region) = if host.starts_with("s3") && host.contains(".amazonaws.com") {
        // Path-style: s3-eu-west-1.amazonaws.com/bucket/prefix
        // or s3.eu-west-1.amazonaws.com/bucket/prefix
        let region = host
            .trim_start_matches("s3-")
            .trim_start_matches("s3.")
            .trim_end_matches(".amazonaws.com")
            .to_string();
        let parts: Vec<&str> = path.splitn(2, '/').collect();
        let bucket = parts[0].to_string();
        let prefix = if parts.len() > 1 { parts[1] } else { "" };
        let endpoint = format!("{}://{}", url.scheme(), host);
        (endpoint, bucket, prefix.to_string(), region)
    } else if host.contains(".s3.") && host.ends_with(".amazonaws.com") {
        // Virtual-hosted: bucket.s3.region.amazonaws.com/prefix
        let bucket = host.split(".s3.").next().unwrap_or("").to_string();
        let region = host
            .split(".s3.")
            .nth(1)
            .unwrap_or("")
            .trim_end_matches(".amazonaws.com")
            .to_string();
        let endpoint = format!("{}://s3.{}.amazonaws.com", url.scheme(), region);
        (endpoint, bucket, path.to_string(), region)
    } else if host.contains(".cloudferro.com") {
        // CloudFerro S3-compatible: s3.waw3-1.cloudferro.com/bucket/prefix
        let parts: Vec<&str> = path.splitn(2, '/').collect();
        let bucket = parts[0].to_string();
        let prefix = if parts.len() > 1 { parts[1] } else { "" };
        let endpoint = format!("{}://{}", url.scheme(), host);
        (endpoint, bucket, prefix.to_string(), "auto".to_string())
    } else {
        return Err(DataServerError::Storage(format!(
            "Cannot parse S3 URL: {data_path}"
        )));
    };

    tracing::info!("S3 store: endpoint={}, bucket={}, prefix={}, region={}", endpoint, bucket, prefix, region);

    let mut builder = object_store::aws::AmazonS3Builder::new()
        .with_bucket_name(&bucket)
        .with_region(&region)
        .with_endpoint(&endpoint)
        .with_allow_http(url.scheme() == "http");

    // For public buckets, skip signing
    builder = builder.with_skip_signature(true);

    let store = builder
        .build()
        .map_err(|e| DataServerError::Storage(format!("Cannot create S3 store: {e}")))?;

    let prefix_path = ObjectPath::from(prefix.trim_end_matches('/'));
    Ok((DataStore::new(Arc::new(store)), prefix_path))
}

fn build_http_store(data_path: &str) -> Result<(DataStore, ObjectPath), DataServerError> {
    // For HTTP, the URL up to the last '/' is the base, the rest is prefix
    let url = url::Url::parse(data_path)
        .map_err(|e| DataServerError::Storage(format!("Invalid URL {data_path}: {e}")))?;

    // Use the URL without the path as the base
    let base_url = format!("{}://{}", url.scheme(), url.host_str().unwrap_or(""));

    let store = object_store::http::HttpBuilder::new()
        .with_url(&base_url)
        .build()
        .map_err(|e| DataServerError::Storage(format!("Cannot create HTTP store for {base_url}: {e}")))?;

    let path = url.path().trim_start_matches('/').trim_end_matches('/');
    Ok((DataStore::new(Arc::new(store)), ObjectPath::from(path)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn local_store_list() {
        let (store, prefix) = build_store("testdata/radar").unwrap_or_else(|_| {
            // Tests may run from crate directory or workspace root
            build_store("../../testdata/radar").expect("Cannot find testdata/radar")
        });
        let entries = store.list(&prefix).unwrap();
        assert!(!entries.is_empty(), "Should find radar test files");
        for entry in &entries {
            let name = entry.location.filename().unwrap_or_default();
            assert!(name.ends_with(".tif"), "Expected .tif files, got {name}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn local_store_get_range() {
        let (store, _prefix) = build_store("testdata/radar").unwrap_or_else(|_| {
            build_store("../../testdata/radar").expect("Cannot find testdata/radar")
        });

        // List files and read the TIFF header (first 4 bytes) of the first one
        let entries = store.list(&ObjectPath::from("")).unwrap();
        let first = &entries[0].location;
        let header = store.get_range(first, 0..4).unwrap();
        // TIFF magic: II (little-endian) = 0x49 0x49 0x2A 0x00
        assert_eq!(&header[0..2], b"II", "Expected little-endian TIFF header");
    }
}
