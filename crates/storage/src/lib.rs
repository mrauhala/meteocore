//! Shared storage abstraction for the data server.
//!
//! Provides a synchronous `DataStore` over the `object_store` crate,
//! supporting local filesystem, S3, and HTTP backends.

pub mod discovery;
mod error;

use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use ds_core::error::DataServerError;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectMeta, ObjectStore};

pub use bytes;
pub use error::StorageError;
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
    bytes_read: Arc<AtomicU64>,
}

impl DataStore {
    pub fn new(store: Arc<dyn ObjectStore>) -> Self {
        Self {
            inner: store,
            bytes_read: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Get the entire contents of an object.
    #[allow(clippy::needless_question_mark)]
    pub fn get(&self, path: &ObjectPath) -> Result<Bytes, DataServerError> {
        let result = self.block_on(async {
            let result = self.inner.get(path).await?;
            Ok(result.bytes().await?)
        })?;
        self.bytes_read
            .fetch_add(result.len() as u64, Ordering::Relaxed);
        Ok(result)
    }

    /// Get a byte range from an object.
    #[allow(clippy::needless_question_mark)]
    pub fn get_range(
        &self,
        path: &ObjectPath,
        range: Range<usize>,
    ) -> Result<Bytes, DataServerError> {
        let result = self.block_on(async { Ok(self.inner.get_range(path, range).await?) })?;
        self.bytes_read
            .fetch_add(result.len() as u64, Ordering::Relaxed);
        Ok(result)
    }

    /// Like [`Self::get_range`], but drives the fetch on an explicitly-provided
    /// runtime `Handle` (`handle.block_on`). Use this when calling from a thread
    /// that is **not** a Tokio worker and has no current handle — e.g. a `rayon`
    /// pool worker — so the I/O reuses the main runtime instead of `block_on`'s
    /// `try_current()` fallback that constructs a brand-new `Runtime` per call
    /// (#222). Must NOT be called from within an async task (a running future);
    /// a `spawn_blocking` thread or a rayon worker is fine (`handle.block_on`
    /// is valid there).
    pub fn get_range_on(
        &self,
        path: &ObjectPath,
        range: Range<usize>,
        handle: &tokio::runtime::Handle,
    ) -> Result<Bytes, DataServerError> {
        let result = self.block_on_with(Some(handle), async {
            self.inner.get_range(path, range).await
        })?;
        self.bytes_read
            .fetch_add(result.len() as u64, Ordering::Relaxed);
        Ok(result)
    }

    /// Like [`Self::get`], but drives the fetch on an explicitly-provided
    /// runtime `Handle` (`handle.block_on`) instead of `block_in_place`. Use
    /// from a `spawn_blocking` pool thread — where `block_in_place` *panics*
    /// (e.g. the PVOL lazy pixel reader on a render / trajectory request that
    /// runs inside `spawn_blocking`). Must NOT be called from within a running
    /// future on a request worker (an async execution context — `handle.block_on`
    /// panics there); use [`Self::get`] for that.
    #[allow(clippy::needless_question_mark)]
    pub fn get_on(
        &self,
        path: &ObjectPath,
        handle: &tokio::runtime::Handle,
    ) -> Result<Bytes, DataServerError> {
        let result = self.block_on_with(Some(handle), async {
            let result = self.inner.get(path).await?;
            Ok(result.bytes().await?)
        })?;
        self.bytes_read
            .fetch_add(result.len() as u64, Ordering::Relaxed);
        Ok(result)
    }

    /// Return total bytes read from this store since creation.
    pub fn bytes_read(&self) -> u64 {
        self.bytes_read.load(Ordering::Relaxed)
    }

    /// List objects under a prefix.
    #[allow(clippy::needless_question_mark)]
    pub fn list(&self, prefix: &ObjectPath) -> Result<Vec<ObjectMeta>, DataServerError> {
        self.block_on(async {
            use futures::TryStreamExt;
            Ok(self.inner.list(Some(prefix)).try_collect().await?)
        })
    }

    /// Get object metadata (size, last modified, etc.).
    #[allow(clippy::needless_question_mark)]
    pub fn head(&self, path: &ObjectPath) -> Result<ObjectMeta, DataServerError> {
        self.block_on(async { Ok(self.inner.head(path).await?) })
    }

    /// Fetch many objects concurrently, returning one result per input
    /// path **in input order**. A per-object failure (missing, oversized,
    /// network) is carried in that slot's `Err` and does not sink the
    /// batch; the outer `Err` is reserved for a runtime-bridge failure.
    ///
    /// `concurrency` bounds in-flight requests (`buffer_unordered`), which
    /// also bounds peak memory to ~`concurrency × object size` — so call
    /// this with a bounded batch (a chunk), not thousands of paths at once.
    /// When `max_bytes` is set, each object's size is checked with `head`
    /// before `get`, so an object that grew past the cap isn't pulled into
    /// memory.
    ///
    /// Drives the whole batch on ONE bridge call, so — like every other
    /// [`DataStore`] method — it is safe at startup and on a multi-thread
    /// runtime worker (`block_in_place`) but MUST NOT be wrapped in
    /// `spawn_blocking` or called from a rayon worker.
    #[allow(clippy::type_complexity)]
    pub fn get_many(
        &self,
        paths: &[ObjectPath],
        concurrency: usize,
        max_bytes: Option<u64>,
    ) -> Result<Vec<Result<Bytes, DataServerError>>, DataServerError> {
        use futures::StreamExt;

        let conc = concurrency.max(1);
        let inner = &self.inner;
        // Drive the batch with NO overall timeout — the 30s budget is
        // applied PER object below. A whole-batch cap would fail the entire
        // chunk once the combined transfer exceeds 30s (e.g. a dozen
        // multi-MB volumes on a constrained link), losing every object
        // instead of the one that actually stalled.
        let ordered: Vec<(usize, Result<Bytes, DataServerError>)> =
            self.block_on_untimed(async {
                let mut results: Vec<(usize, Result<Bytes, DataServerError>)> =
                    futures::stream::iter(paths.iter().enumerate().map(|(i, p)| async move {
                        let fetch = async {
                            if let Some(cap) = max_bytes {
                                let meta = inner
                                    .head(p)
                                    .await
                                    .map_err(|e| DataServerError::from(StorageError::from(e)))?;
                                if meta.size as u64 > cap {
                                    return Err(DataServerError::Storage(format!(
                                        "object `{p}` is {} bytes — exceeds the {cap}-byte limit",
                                        meta.size
                                    )));
                                }
                            }
                            let res = inner
                                .get(p)
                                .await
                                .map_err(|e| DataServerError::from(StorageError::from(e)))?;
                            let bytes = res
                                .bytes()
                                .await
                                .map_err(|e| DataServerError::from(StorageError::from(e)))?;
                            Ok::<Bytes, DataServerError>(bytes)
                        };
                        let r = match tokio::time::timeout(Self::REQUEST_TIMEOUT, fetch).await {
                            Ok(r) => r,
                            Err(_) => Err(DataServerError::Storage(format!(
                                "fetch of `{p}` timed out after {}s",
                                Self::REQUEST_TIMEOUT.as_secs()
                            ))),
                        };
                        (i, r)
                    }))
                    .buffer_unordered(conc)
                    .collect()
                    .await;
                results.sort_by_key(|(i, _)| *i);
                results
            })?;

        let total: u64 = ordered
            .iter()
            .filter_map(|(_, r)| r.as_ref().ok())
            .map(|b| b.len() as u64)
            .sum();
        self.bytes_read.fetch_add(total, Ordering::Relaxed);
        Ok(ordered.into_iter().map(|(_, r)| r).collect())
    }

    /// Default timeout for individual storage operations (30 seconds).
    const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

    /// Bridge async to sync. Uses `block_in_place` when inside a tokio runtime
    /// (safe from async handlers via multi-threaded scheduler), or creates a
    /// temporary runtime otherwise (for use in non-async contexts like startup).
    /// All operations are subject to a 30-second timeout to prevent hung connections.
    fn block_on<F, T>(&self, future: F) -> Result<T, DataServerError>
    where
        F: std::future::Future<Output = Result<T, object_store::Error>>,
    {
        self.block_on_with(None, future)
    }

    /// Drive `future` to completion on the appropriate runtime — like
    /// [`Self::block_on`] but with **no** overall 30s timeout and an
    /// unconstrained output type. For batch helpers (e.g. [`Self::get_many`])
    /// whose total wall-time legitimately exceeds a single request's budget
    /// and which apply their own per-item timeouts; a batch-wide cap would
    /// wrongly fail the whole batch. Same thread-context rules as
    /// [`Self::block_on_with`] with `None`: valid on a runtime worker
    /// (`block_in_place`) or off-runtime (temporary runtime), never on a
    /// `spawn_blocking`/rayon thread.
    fn block_on_untimed<F, T>(&self, future: F) -> Result<T, DataServerError>
    where
        F: std::future::Future<Output = T>,
    {
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => Ok(tokio::task::block_in_place(|| handle.block_on(future))),
            Err(_) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| DataServerError::Storage(format!("Cannot create runtime: {e}")))?;
                Ok(rt.block_on(future))
            }
        }
    }

    /// Core sync→async bridge. With an explicit `handle` (caller is on a
    /// non-Tokio thread such as a rayon worker) it drives the future on that
    /// runtime via `handle.block_on` — no `block_in_place` (which would panic
    /// off a worker thread) and no per-call `Runtime::new`. With `None` it
    /// uses `block_in_place` when already inside a runtime, else spins up a
    /// temporary runtime (tests / CLI).
    fn block_on_with<F, T>(
        &self,
        handle: Option<&tokio::runtime::Handle>,
        future: F,
    ) -> Result<T, DataServerError>
    where
        F: std::future::Future<Output = Result<T, object_store::Error>>,
    {
        let timed = async {
            match tokio::time::timeout(Self::REQUEST_TIMEOUT, future).await {
                Ok(result) => result,
                Err(_) => Err(object_store::Error::Generic {
                    store: "DataStore",
                    source: "Request timed out after 30s".into(),
                }),
            }
        };
        let result = match handle {
            Some(h) => h.block_on(timed),
            None => match tokio::runtime::Handle::try_current() {
                Ok(handle) => tokio::task::block_in_place(|| handle.block_on(timed)),
                Err(_) => {
                    // No runtime — create a temporary one (e.g., tests / CLI tools)
                    let rt = tokio::runtime::Runtime::new().map_err(|e| {
                        DataServerError::Storage(format!("Cannot create runtime: {e}"))
                    })?;
                    rt.block_on(timed)
                }
            },
        };
        result.map_err(|e| DataServerError::from(StorageError::from(e)))
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

/// Case-insensitively test whether `path` begins with a URL `scheme`
/// prefix (e.g. `"http://"`, `"s3://"`).
///
/// Per RFC 3986 §3.1 URL schemes are case-insensitive (`HTTP://` and
/// `http://` are the same scheme), but the remainder of a URL is not —
/// so only the prefix is folded. The comparison is byte-wise so a
/// non-ASCII `path` can't trip a UTF-8 boundary panic.
pub fn has_scheme(path: &str, scheme: &str) -> bool {
    let scheme = scheme.as_bytes();
    let bytes = path.as_bytes();
    bytes.len() >= scheme.len() && bytes[..scheme.len()].eq_ignore_ascii_case(scheme)
}

/// Case-insensitively strip a URL `scheme` prefix, returning the
/// remainder, or `None` when `path` doesn't start with `scheme`.
/// `scheme` is ASCII, so the matched prefix length is a valid UTF-8
/// boundary to slice at.
fn strip_scheme<'a>(path: &'a str, scheme: &str) -> Option<&'a str> {
    has_scheme(path, scheme).then(|| &path[scheme.len()..])
}

/// Build a `DataStore` from a data path string.
///
/// Auto-detects the backend from the path prefix (schemes are matched
/// case-insensitively, per [`has_scheme`]):
/// - `s3://bucket/prefix/` → Amazon S3 (credentials from AWS standard chain)
/// - `https://...` or `http://...` → HTTP object store
/// - Anything else → Local filesystem
///
/// Returns the store and the base path within that store.
pub fn build_store(data_path: &str) -> Result<(DataStore, ObjectPath), DataServerError> {
    if has_scheme(data_path, "s3://") {
        build_s3_store(data_path)
    } else if is_s3_http_url(data_path) {
        build_s3_from_http_url(data_path)
    } else if has_scheme(data_path, "http://") || has_scheme(data_path, "https://") {
        build_http_store(data_path)
    } else {
        build_local_store(data_path)
    }
}

/// Build a `DataStore` from explicit S3 endpoint and bucket.
///
/// Use this when endpoint and bucket are configured separately (not parsed
/// from a URL). The returned store has no prefix — callers supply the prefix
/// at query time.
///
/// Skips request signing (for public buckets). Add credential support later
/// if needed.
pub fn build_s3_store_from_parts(
    endpoint: &str,
    bucket: &str,
) -> Result<DataStore, DataServerError> {
    let allow_http = has_scheme(endpoint, "http://");

    let store = object_store::aws::AmazonS3Builder::new()
        .with_bucket_name(bucket)
        .with_region("auto")
        .with_endpoint(endpoint)
        .with_allow_http(allow_http)
        .with_skip_signature(true)
        .build()
        .map_err(|e| {
            DataServerError::Storage(format!(
                "Cannot create S3 store for endpoint={endpoint} bucket={bucket}: {e}"
            ))
        })?;

    tracing::info!("S3 store: endpoint={endpoint}, bucket={bucket}");
    Ok(DataStore::new(Arc::new(store)))
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

    let store = object_store::local::LocalFileSystem::new_with_prefix(&abs_path).map_err(|e| {
        DataServerError::Storage(format!("Cannot create local store at {data_path}: {e}"))
    })?;

    Ok((DataStore::new(Arc::new(store)), ObjectPath::from("")))
}

fn build_s3_store(data_path: &str) -> Result<(DataStore, ObjectPath), DataServerError> {
    // Parse s3://bucket/prefix/path/ (scheme matched case-insensitively).
    let without_scheme = strip_scheme(data_path, "s3://")
        .ok_or_else(|| DataServerError::Storage("Expected s3:// prefix".into()))?;

    let (bucket, prefix) = match without_scheme.find('/') {
        Some(idx) => (&without_scheme[..idx], &without_scheme[idx + 1..]),
        None => (without_scheme, ""),
    };

    let store = object_store::aws::AmazonS3Builder::from_env()
        .with_bucket_name(bucket)
        .build()
        .map_err(|e| {
            DataServerError::Storage(format!("Cannot create S3 store for bucket '{bucket}': {e}"))
        })?;

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
    let (endpoint, bucket, prefix, region) =
        if host.starts_with("s3") && host.contains(".amazonaws.com") {
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

    tracing::info!(
        "S3 store: endpoint={}, bucket={}, prefix={}, region={}",
        endpoint,
        bucket,
        prefix,
        region
    );

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

    // Use the URL without the path as the base (preserve port if present)
    let base_url = match url.port() {
        Some(port) => format!(
            "{}://{}:{}",
            url.scheme(),
            url.host_str().unwrap_or(""),
            port
        ),
        None => format!("{}://{}", url.scheme(), url.host_str().unwrap_or("")),
    };

    let store = object_store::http::HttpBuilder::new()
        .with_url(&base_url)
        .build()
        .map_err(|e| {
            DataServerError::Storage(format!("Cannot create HTTP store for {base_url}: {e}"))
        })?;

    let path = url.path().trim_start_matches('/').trim_end_matches('/');
    Ok((DataStore::new(Arc::new(store)), ObjectPath::from(path)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_scheme_is_case_insensitive_on_the_prefix_only() {
        // Scheme matches regardless of case (RFC 3986 §3.1).
        for url in [
            "http://example.com/x",
            "HTTP://example.com/x",
            "HtTp://example.com/x",
        ] {
            assert!(has_scheme(url, "http://"), "{url} should match http://");
        }
        assert!(has_scheme("S3://bucket/key", "s3://"));
        assert!(has_scheme("HTTPS://h/p", "https://"));

        // Non-matches: different scheme, no scheme, or shorter than the prefix.
        assert!(!has_scheme("ftp://h/p", "http://"));
        assert!(!has_scheme("/local/path", "http://"));
        assert!(!has_scheme("htt", "http://"));
        assert!(!has_scheme("", "s3://"));
        // The fold applies to the scheme only — the path keeps its case,
        // which `strip_scheme` must preserve verbatim.
        assert_eq!(strip_scheme("S3://Bucket/Key", "s3://"), Some("Bucket/Key"));
        assert_eq!(strip_scheme("/local", "s3://"), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_many_returns_results_in_input_order_and_isolates_failures() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.bin"), b"aaa").unwrap();
        std::fs::write(dir.path().join("c.bin"), b"cccc").unwrap();
        let (store, _) = build_store(dir.path().to_str().unwrap()).unwrap();

        // Middle key is missing — its slot must be `Err`, the others `Ok`,
        // and the order must match the input.
        let paths = [
            ObjectPath::from("a.bin"),
            ObjectPath::from("missing.bin"),
            ObjectPath::from("c.bin"),
        ];
        let res = store.get_many(&paths, 8, None).unwrap();
        assert_eq!(res.len(), 3);
        assert_eq!(res[0].as_ref().unwrap().as_ref(), b"aaa");
        assert!(res[1].is_err(), "a missing object yields a per-item Err");
        assert_eq!(res[2].as_ref().unwrap().as_ref(), b"cccc");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_many_max_bytes_rejects_oversized_object() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("big.bin"), vec![0u8; 100]).unwrap();
        std::fs::write(dir.path().join("ok.bin"), vec![0u8; 8]).unwrap();
        let (store, _) = build_store(dir.path().to_str().unwrap()).unwrap();

        let res = store
            .get_many(
                &[ObjectPath::from("ok.bin"), ObjectPath::from("big.bin")],
                4,
                Some(10),
            )
            .unwrap();
        assert!(res[0].is_ok(), "8-byte object is under the 10-byte cap");
        assert!(res[1].is_err(), "100-byte object exceeds the cap → Err");
    }

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

    // get_range_on must work when called from a thread that is NOT a Tokio
    // worker (mirrors the rayon tile-fetch pool): it should drive the fetch on
    // the supplied handle, not panic via block_in_place, and not spin up a new
    // Runtime per call (#222).
    #[tokio::test(flavor = "multi_thread")]
    async fn get_range_on_from_foreign_thread() {
        let (store, _prefix) = build_store("testdata/radar")
            .unwrap_or_else(|_| build_store("../../testdata/radar").expect("testdata/radar"));
        let entries = store.list(&ObjectPath::from("")).unwrap();
        let first = entries[0].location.clone();

        let handle = tokio::runtime::Handle::current();
        let store_ref = &store;
        // A plain OS thread has no current Tokio handle (like a rayon worker).
        let header = std::thread::scope(|s| {
            s.spawn(|| store_ref.get_range_on(&first, 0..4, &handle).unwrap())
                .join()
                .unwrap()
        });
        assert_eq!(&header[0..2], b"II", "Expected little-endian TIFF header");
    }
}
