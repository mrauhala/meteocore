//! Shared storage abstraction for the data server.
//!
//! Provides a synchronous `DataStore` over the `object_store` crate,
//! supporting local filesystem, S3, and HTTP backends.

mod admitted;
pub mod discovery;
mod error;
#[cfg(test)]
mod test_store;

use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use ds_core::error::DataServerError;
use object_store::path::Path as ObjectPath;
// `get` / `head` / `get_range` are convenience wrappers over `get_opts`, and
// object_store 0.14 moved them out of the `ObjectStore` trait itself into this
// extension trait (keeping `dyn ObjectStore` object-safe with fewer methods).
// They must be in scope even though we only ever hold an `Arc<dyn ObjectStore>`.
use object_store::{ObjectMeta, ObjectStore, ObjectStoreExt};

pub use bytes;
pub use error::StorageError;
pub use object_store;

/// Widen a byte range for `object_store`, which switched `get_range` from
/// `Range<usize>` to `Range<u64>` in 0.12 so 32-bit targets can still address
/// large objects. `DataStore`'s own signatures stay `Range<usize>` — every
/// caller computes offsets in `usize` from in-memory structures (COG tile
/// offsets, GRIB message extents), and the widening is lossless on every
/// platform this server builds for.
fn to_u64(range: Range<usize>) -> Range<u64> {
    range.start as u64..range.end as u64
}

/// How long one whole-object fetch may take overall, object_store's own
/// retries included. The caller picks it by where it runs (#1011).
///
/// object_store gives each attempt its own 30 s timeout (its `ClientOptions`
/// default, and what [`build_http_store`]'s client sets). When a body read
/// stalls, that timeout fails the attempt and object_store retries after a
/// short backoff, asking only for the bytes it is still missing. An overall
/// limit of 30 s fires at the same moment as the first attempt's timeout,
/// so the retry never runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchBudget {
    /// A request waits on the fetch: 30 s, or the request deadline when one
    /// is in scope. Every [`DataStore`] method without a budget argument
    /// uses this one.
    Request,
    /// A background fetch on the poll runtime, which nobody waits on:
    /// 120 s. That is room for the first attempt to stall until its 30 s
    /// timeout and for object_store's retry to finish, with margin for one
    /// more. A request deadline in scope still applies.
    Background,
}

impl FetchBudget {
    /// The overall limit when no request deadline is in scope.
    pub const fn timeout(self) -> std::time::Duration {
        match self {
            FetchBudget::Request => DataStore::REQUEST_TIMEOUT,
            FetchBudget::Background => DataStore::BACKGROUND_TIMEOUT,
        }
    }
}

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

    /// Get the entire contents of an object, under [`FetchBudget::Request`].
    pub fn get(&self, path: &ObjectPath) -> Result<Bytes, DataServerError> {
        self.get_with_budget(path, FetchBudget::Request)
    }

    /// [`Self::get`] under an explicit `budget`. A poll-runtime caller
    /// downloading a whole object passes [`FetchBudget::Background`], so a
    /// stalled attempt is retried instead of failing the fetch.
    pub fn get_with_budget(
        &self,
        path: &ObjectPath,
        budget: FetchBudget,
    ) -> Result<Bytes, DataServerError> {
        let result = self.block_on_with(None, budget, self.whole(path))?;
        self.bytes_read
            .fetch_add(result.len() as u64, Ordering::Relaxed);
        Ok(result)
    }

    /// The whole object at `path`, body included.
    async fn whole(&self, path: &ObjectPath) -> Result<Bytes, object_store::Error> {
        self.inner.get(path).await?.bytes().await
    }

    /// Get a byte range from an object, under [`FetchBudget::Request`].
    pub fn get_range(
        &self,
        path: &ObjectPath,
        range: Range<usize>,
    ) -> Result<Bytes, DataServerError> {
        self.get_range_with_budget(path, range, FetchBudget::Request)
    }

    /// [`Self::get_range`] under an explicit `budget`. A poll-runtime caller
    /// reading a header before an object is catalogued passes
    /// [`FetchBudget::Background`], so a stalled attempt is retried instead
    /// of leaving the object out until the next poll.
    pub fn get_range_with_budget(
        &self,
        path: &ObjectPath,
        range: Range<usize>,
        budget: FetchBudget,
    ) -> Result<Bytes, DataServerError> {
        let result = self.block_on_with(None, budget, self.inner.get_range(path, to_u64(range)))?;
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
        let result = self.block_on_with(Some(handle), FetchBudget::Request, async {
            self.inner.get_range(path, to_u64(range)).await
        })?;
        self.bytes_read
            .fetch_add(result.len() as u64, Ordering::Relaxed);
        Ok(result)
    }

    /// Like [`Self::get`], but drives the fetch on an explicitly-provided
    /// runtime `Handle` (`handle.block_on`) instead of selecting an ambient
    /// handle. Prefer this for blocking-pool or foreign workers when the I/O
    /// runtime is supplied by their caller. `block_in_place` does not inherently
    /// panic on `spawn_blocking`; it runs the closure directly there.
    /// Must NOT be called from within a running
    /// future on a request worker (an async execution context — `handle.block_on`
    /// panics there); use [`Self::get`] for that.
    pub fn get_on(
        &self,
        path: &ObjectPath,
        handle: &tokio::runtime::Handle,
    ) -> Result<Bytes, DataServerError> {
        let result = self.block_on_with(Some(handle), FetchBudget::Request, self.whole(path))?;
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

    /// Like [`Self::get`], but a **missing** object (`NotFound` / HTTP 404)
    /// maps to `Ok(None)` rather than an error. Zarr stores treat an absent key
    /// (an unwritten chunk, an optional metadata file) as "use the fill value"
    /// / "not present", so the engine needs this distinction.
    pub fn get_opt(&self, path: &ObjectPath) -> Result<Option<Bytes>, DataServerError> {
        self.get_opt_with(path, None)
    }

    /// [`Self::get_opt`] driven on an explicitly-provided runtime `Handle`, as
    /// [`Self::get_on`] is to [`Self::get`]. A missing object is `Ok(None)`;
    /// every other failure (deadline, network, 5xx) stays an `Err`, so a
    /// caller tells "the object is gone" from "this read failed" without
    /// matching error strings.
    pub fn get_opt_on(
        &self,
        path: &ObjectPath,
        handle: &tokio::runtime::Handle,
    ) -> Result<Option<Bytes>, DataServerError> {
        self.get_opt_with(path, Some(handle))
    }

    fn get_opt_with(
        &self,
        path: &ObjectPath,
        handle: Option<&tokio::runtime::Handle>,
    ) -> Result<Option<Bytes>, DataServerError> {
        let result = self.block_on_with(handle, FetchBudget::Request, async {
            match self.inner.get(path).await {
                Ok(res) => Ok(Some(res.bytes().await?)),
                Err(object_store::Error::NotFound { .. }) => Ok(None),
                Err(e) => Err(e),
            }
        })?;
        if let Some(b) = &result {
            self.bytes_read.fetch_add(b.len() as u64, Ordering::Relaxed);
        }
        Ok(result)
    }

    /// One-level (delimiter) listing under `prefix`, returning `(objects,
    /// common_prefixes)` — the immediate child objects and the immediate child
    /// "directories". Used for Zarr group/child discovery, which only needs the
    /// next path segment, not a recursive walk of every chunk key.
    pub fn list_dir(
        &self,
        prefix: &ObjectPath,
    ) -> Result<(Vec<ObjectMeta>, Vec<ObjectPath>), DataServerError> {
        self.block_on(async {
            let res = self.inner.list_with_delimiter(Some(prefix)).await?;
            Ok((res.objects, res.common_prefixes))
        })
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
    /// runtime worker (`block_in_place`). On a `spawn_blocking` worker the
    /// bridge runs directly using its attached runtime. Foreign threads without
    /// a handle create a temporary runtime; parallel callers should instead
    /// drive async storage with a supplied handle. Current-thread async tasks
    /// and `LocalSet` are unsupported.
    ///
    /// Each object gets [`FetchBudget::Request`]'s 30 s;
    /// [`Self::get_many_with_budget`] picks another budget.
    #[allow(clippy::type_complexity)]
    pub fn get_many(
        &self,
        paths: &[ObjectPath],
        concurrency: usize,
        max_bytes: Option<u64>,
    ) -> Result<Vec<Result<Bytes, DataServerError>>, DataServerError> {
        self.get_many_with_budget(paths, concurrency, max_bytes, FetchBudget::Request)
    }

    /// [`Self::get_many`] with `budget`'s timeout on each object. A
    /// poll-runtime caller downloading whole objects passes
    /// [`FetchBudget::Background`], so a stalled attempt is retried instead
    /// of failing its slot. The request deadline does not apply here, as
    /// in [`Self::get_many`].
    #[allow(clippy::type_complexity)]
    pub fn get_many_with_budget(
        &self,
        paths: &[ObjectPath],
        concurrency: usize,
        max_bytes: Option<u64>,
        budget: FetchBudget,
    ) -> Result<Vec<Result<Bytes, DataServerError>>, DataServerError> {
        // Drive the batch with NO overall timeout — the budget applies PER
        // object. A whole-batch cap would fail the entire chunk once the
        // combined transfer exceeds it (e.g. a dozen multi-MB volumes on a
        // constrained link), losing every object instead of the one that
        // actually stalled.
        let results =
            self.block_on_untimed(self.fetch_many(paths, concurrency, max_bytes, budget))?;
        let total: u64 = results
            .iter()
            .filter_map(|r| r.as_ref().ok())
            .map(|b| b.len() as u64)
            .sum();
        self.bytes_read.fetch_add(total, Ordering::Relaxed);
        Ok(results)
    }

    /// The batch [`Self::get_many_with_budget`] drives: one result per
    /// path, in input order, each object limited to `budget`.
    async fn fetch_many(
        &self,
        paths: &[ObjectPath],
        concurrency: usize,
        max_bytes: Option<u64>,
        budget: FetchBudget,
    ) -> Vec<Result<Bytes, DataServerError>> {
        use futures::StreamExt;

        let conc = concurrency.max(1);
        let inner = &self.inner;
        let mut results: Vec<(usize, Result<Bytes, DataServerError>)> =
            futures::stream::iter(paths.iter().enumerate().map(|(i, p)| async move {
                let fetch = async {
                    if let Some(cap) = max_bytes {
                        let meta = inner
                            .head(p)
                            .await
                            .map_err(|e| DataServerError::from(StorageError::from(e)))?;
                        if meta.size > cap {
                            return Err(DataServerError::Storage(format!(
                                "object `{p}` is {} bytes — exceeds the {cap}-byte limit",
                                meta.size
                            )));
                        }
                    }
                    self.whole(p)
                        .await
                        .map_err(|e| DataServerError::from(StorageError::from(e)))
                };
                let r = match tokio::time::timeout(budget.timeout(), fetch).await {
                    Ok(r) => r,
                    Err(_) => Err(DataServerError::Storage(format!(
                        "fetch of `{p}` timed out after {}s",
                        budget.timeout().as_secs()
                    ))),
                };
                (i, r)
            }))
            .buffer_unordered(conc)
            .collect()
            .await;
        results.sort_by_key(|(i, _)| *i);
        results.into_iter().map(|(_, r)| r).collect()
    }

    /// Probe many object keys concurrently with `head`, returning one
    /// result per input path **in input order**. A **missing** object
    /// (`NotFound` / HTTP 404) maps to `Ok(None)` — "absent", not an
    /// error — so a caller probing candidate keys can keep the ones that
    /// exist; any other failure (timeout, 403, network) is the slot's
    /// `Err`. The outer `Err` is reserved for a runtime-bridge failure.
    ///
    /// This is the listing-free discovery primitive for HTTP stores that
    /// don't support `list` (a plain Apache/nginx autoindex answers a
    /// direct `HEAD` but not WebDAV `PROPFIND`). Same concurrency,
    /// per-object 30s timeout, and thread-context rules as
    /// [`Self::get_many`].
    #[allow(clippy::type_complexity)]
    pub fn head_many(
        &self,
        paths: &[ObjectPath],
        concurrency: usize,
    ) -> Result<Vec<Result<Option<ObjectMeta>, DataServerError>>, DataServerError> {
        use futures::StreamExt;

        let conc = concurrency.max(1);
        let inner = &self.inner;
        let ordered: Vec<(usize, Result<Option<ObjectMeta>, DataServerError>)> = self
            .block_on_untimed(async {
                let mut results: Vec<(usize, Result<Option<ObjectMeta>, DataServerError>)> =
                    futures::stream::iter(paths.iter().enumerate().map(|(i, p)| async move {
                        let probe = async {
                            match inner.head(p).await {
                                Ok(meta) => Ok(Some(meta)),
                                // A 404 is the expected answer for a candidate
                                // key that doesn't exist — not a failure.
                                Err(object_store::Error::NotFound { .. }) => Ok(None),
                                Err(e) => Err(DataServerError::from(StorageError::from(e))),
                            }
                        };
                        let r = match tokio::time::timeout(Self::REQUEST_TIMEOUT, probe).await {
                            Ok(r) => r,
                            Err(_) => Err(DataServerError::Storage(format!(
                                "head of `{p}` timed out after {}s",
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

        Ok(ordered.into_iter().map(|(_, r)| r).collect())
    }

    /// [`Self::list`] many prefixes concurrently, returning one result per
    /// prefix **in input order**. A prefix that fails to list (a missing
    /// partition on an HTTP store, a timeout) carries its error in its slot
    /// and does not sink the batch. The outer `Err` is reserved for a
    /// runtime-bridge failure or an already-expired request deadline.
    ///
    /// `concurrency` bounds the LISTs in flight (`buffer_unordered`). Each
    /// LIST gets the budget [`Self::list`] has: the request deadline when
    /// one is set, else 30 s of its own, so a long batch is never failed by
    /// a batch-wide cap.
    ///
    /// Drives the whole batch on ONE bridge call, with the same
    /// thread-context rules as [`Self::get_many`]. Catalog scans list their
    /// date-expanded prefixes through this, via
    /// [`discovery::list_prefixes`], instead of one blocking `list` after
    /// another (Critical Rule 9).
    #[allow(clippy::type_complexity)]
    pub fn list_many(
        &self,
        prefixes: &[ObjectPath],
        concurrency: usize,
    ) -> Result<Vec<Result<Vec<ObjectMeta>, DataServerError>>, DataServerError> {
        use futures::{StreamExt, TryStreamExt};

        let conc = concurrency.max(1);
        let deadline = ds_core::deadline::current();
        ds_core::deadline::check()?;
        let inner = &self.inner;
        let ordered: Vec<(usize, Result<Vec<ObjectMeta>, DataServerError>)> = self
            .block_on_untimed(async {
                let mut results: Vec<(usize, Result<Vec<ObjectMeta>, DataServerError>)> =
                    futures::stream::iter(prefixes.iter().enumerate().map(|(i, p)| async move {
                        let end = deadline
                            .unwrap_or_else(|| std::time::Instant::now() + Self::REQUEST_TIMEOUT);
                        let listed = inner.list(Some(p)).try_collect::<Vec<_>>();
                        let r = match tokio::time::timeout_at(end.into(), listed).await {
                            Ok(r) => r.map_err(|e| DataServerError::from(StorageError::from(e))),
                            Err(_) if deadline.is_some() => Err(DataServerError::DeadlineExceeded),
                            Err(_) => Err(DataServerError::Storage(format!(
                                "Request timed out after {}s",
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

        Ok(ordered.into_iter().map(|(_, r)| r).collect())
    }

    /// object_store's timeout on each attempt: its `ClientOptions` default,
    /// and what [`build_http_store`]'s client sets. A retry gets a fresh one.
    const ATTEMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

    /// [`FetchBudget::Request`]: the overall limit on a storage operation a
    /// request waits on (30 seconds).
    const REQUEST_TIMEOUT: std::time::Duration = Self::ATTEMPT_TIMEOUT;

    /// [`FetchBudget::Background`]: four attempts' worth, so a first attempt
    /// that stalls until its timeout leaves room for object_store's retry
    /// (after a backoff that starts at 0.1 s) and for one more.
    pub const BACKGROUND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

    /// Bridge async to sync. Uses `block_in_place` when inside a tokio runtime
    /// (releases a multi-threaded scheduler worker), or creates a
    /// temporary runtime otherwise (for use in non-async contexts like startup).
    /// All operations are subject to [`FetchBudget::Request`]'s 30-second
    /// timeout to prevent hung connections.
    fn block_on<F, T>(&self, future: F) -> Result<T, DataServerError>
    where
        F: std::future::Future<Output = Result<T, object_store::Error>>,
    {
        self.block_on_with(None, FetchBudget::Request, future)
    }

    /// Drive `future` to completion on the appropriate runtime — like
    /// [`Self::block_on`] but with **no** overall timeout and an
    /// unconstrained output type. For batch helpers (e.g. [`Self::get_many`])
    /// whose total wall-time legitimately exceeds a single request's budget
    /// and which apply their own per-item timeouts; a batch-wide cap would
    /// wrongly fail the whole batch. Same thread-context rules as
    /// [`Self::block_on_with`] with `None`: a multi-threaded async worker yields
    /// via `block_in_place`, a blocking-pool worker runs directly, and a foreign
    /// thread without a handle creates a temporary runtime. Current-thread
    /// async tasks and `LocalSet` are unsupported.
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
    /// runtime via `handle.block_on`, without ambient runtime lookup or per-call
    /// `Runtime::new`. With `None` it uses `block_in_place` around the ambient
    /// handle: this yields an async worker or runs directly on a blocking-pool
    /// worker. With no handle it creates a temporary runtime (tests / CLI).
    /// The future is limited to the request deadline in scope, else to
    /// `budget` ([`within`]).
    fn block_on_with<F, T>(
        &self,
        handle: Option<&tokio::runtime::Handle>,
        budget: FetchBudget,
        future: F,
    ) -> Result<T, DataServerError>
    where
        F: std::future::Future<Output = Result<T, object_store::Error>>,
    {
        self.block_on_result(handle, budget, async {
            future
                .await
                .map_err(|e| DataServerError::from(StorageError::from(e)))
        })
    }

    fn block_on_result<F, T>(
        &self,
        handle: Option<&tokio::runtime::Handle>,
        budget: FetchBudget,
        future: F,
    ) -> Result<T, DataServerError>
    where
        F: std::future::Future<Output = Result<T, DataServerError>>,
    {
        let deadline = ds_core::deadline::current();
        ds_core::deadline::check()?;
        let timed = within(budget, deadline, future);
        match handle {
            Some(h) => h.block_on(timed),
            None => self.block_on_untimed(timed)?,
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

/// `future`, limited to the request `deadline` when one is in scope (an
/// expiry is [`DataServerError::DeadlineExceeded`]), else to `budget`.
async fn within<F, T>(
    budget: FetchBudget,
    deadline: Option<std::time::Instant>,
    future: F,
) -> Result<T, DataServerError>
where
    F: std::future::Future<Output = Result<T, DataServerError>>,
{
    let end = match deadline {
        Some(deadline) => deadline.into(),
        None => tokio::time::Instant::now() + budget.timeout(),
    };
    match tokio::time::timeout_at(end, future).await {
        Ok(result) => result,
        Err(_) if deadline.is_some() => Err(DataServerError::DeadlineExceeded),
        Err(_) => Err(DataServerError::Storage(format!(
            "Request timed out after {}s",
            budget.timeout().as_secs()
        ))),
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
fn is_s3_http_url(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    // Query-bearing links (including pre-signed object URLs) must preserve
    // their exact HTTP query rather than becoming S3 prefix discovery.
    if !matches!(url.scheme(), "http" | "https") || url.query().is_some() {
        return false;
    }
    url.host_str()
        .is_some_and(|host| host.ends_with(".amazonaws.com") || host.ends_with(".cloudferro.com"))
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

/// The generic HTTP backend must not follow an operator-trusted URL to an
/// unvalidated host. object_store's default connector follows redirects;
/// its injectable connector lets us enforce the policy for every operation.
#[derive(Debug)]
struct NoRedirectConnector(reqwest::Client);

impl object_store::client::HttpConnector for NoRedirectConnector {
    fn connect(
        &self,
        _options: &object_store::ClientOptions,
    ) -> object_store::Result<object_store::client::HttpClient> {
        Ok(object_store::client::HttpClient::new(self.0.clone()))
    }
}

/// Build a no-redirect HTTP object store without S3 URL auto-detection.
/// Use for feeds and allowlisted document URLs, including S3-hosted objects.
/// The source query string is preserved on each request.
pub fn build_http_store(data_path: &str) -> Result<(DataStore, ObjectPath), DataServerError> {
    // For HTTP, the URL up to the last '/' is the base, the rest is prefix
    let url = url::Url::parse(data_path)
        .map_err(|e| DataServerError::Storage(format!("Invalid URL {data_path}: {e}")))?;

    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(DataServerError::Config(
            "HTTP source requires an http(s) URL".into(),
        ));
    }
    let mut base_url = url.clone();
    base_url.set_path("");
    base_url.set_fragment(None);

    // Own all transport options here rather than using ClientOptions (whose
    // reqwest builder is private). Disable transparent decompression to keep
    // GRIB/COG byte ranges and Content-Length exact even with feature unification.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .https_only(url.scheme() != "http")
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(DataStore::ATTEMPT_TIMEOUT)
        .no_gzip()
        .no_brotli()
        .no_zstd()
        .no_deflate()
        .build()
        .map_err(|e| DataServerError::Storage(format!("Cannot create HTTP client: {e}")))?;
    let store = object_store::http::HttpBuilder::new()
        .with_url(base_url.as_str())
        .with_http_connector(NoRedirectConnector(client))
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
    fn s3_detection_uses_only_host_and_never_loses_signed_queries() {
        assert!(is_s3_http_url(
            "https://bucket.s3.eu-west-1.amazonaws.com/prefix"
        ));
        assert!(is_s3_http_url(
            "https://s3.waw3-1.cloudferro.com/bucket/prefix"
        ));
        for value in [
            "https://example.com/x?next=https://s3.eu-west-1.amazonaws.com/bucket",
            "https://example.com/path/.cloudferro.com/file",
            "https://s3.amazonaws.com.evil.test/file",
            "https://bucket.s3.eu-west-1.amazonaws.com/doc.xml?X-Amz-Signature=abc",
        ] {
            assert!(!is_s3_http_url(value), "{value}");
        }
    }

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
    async fn head_many_reports_presence_in_input_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.bin"), b"aaa").unwrap();
        std::fs::write(dir.path().join("c.bin"), b"cccc").unwrap();
        let (store, _) = build_store(dir.path().to_str().unwrap()).unwrap();

        // Middle key is absent — a missing object is `Ok(None)`, not an Err.
        let paths = [
            ObjectPath::from("a.bin"),
            ObjectPath::from("missing.bin"),
            ObjectPath::from("c.bin"),
        ];
        let res = store.head_many(&paths, 8).unwrap();
        assert_eq!(res.len(), 3);
        assert_eq!(res[0].as_ref().unwrap().as_ref().unwrap().size, 3);
        assert!(
            matches!(res[1], Ok(None)),
            "a missing object is Ok(None), got {:?}",
            res[1]
        );
        assert_eq!(res[2].as_ref().unwrap().as_ref().unwrap().size, 4);
    }

    /// `list_many` keeps at most `concurrency` LISTs in flight, answers in
    /// input order even when the first prefix finishes last, and isolates a
    /// failing prefix in its own slot.
    #[tokio::test(flavor = "multi_thread")]
    async fn list_many_bounds_lists_and_answers_in_input_order() {
        use std::time::Duration;
        let probe = crate::test_store::ListProbe {
            delay: Duration::from_millis(20),
            delays: [("p0".to_string(), Duration::from_millis(80))].into(),
            failing: ["p3".to_string()].into(),
            ..Default::default()
        }
        .with_objects(&["p0/a", "p1/b", "p2/c", "p3/d", "p4/e"])
        .await;
        let peak = probe.peak.clone();
        let store = DataStore::new(Arc::new(probe));
        let prefixes: Vec<ObjectPath> = (0..6).map(|i| ObjectPath::from(format!("p{i}"))).collect();

        let listed = store.list_many(&prefixes, 3).unwrap();
        assert_eq!(peak.load(Ordering::SeqCst), 3);
        let slots: Vec<Option<Vec<String>>> = listed
            .into_iter()
            .map(|r| {
                r.ok()
                    .map(|objects| objects.iter().map(|o| o.location.to_string()).collect())
            })
            .collect();
        let some = |key: &str| Some(vec![key.to_string()]);
        assert_eq!(
            slots,
            [
                some("p0/a"),
                some("p1/b"),
                some("p2/c"),
                None,
                some("p4/e"),
                Some(vec![]),
            ]
        );
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
    // the supplied handle, without ambient runtime lookup or spinning up a new
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

#[cfg(test)]
mod deadline_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn interactive_deadline_cancels_storage_future_and_leaves_background_unrestricted() {
        let store = DataStore::new(Arc::new(object_store::memory::InMemory::new()));
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let cancelled = Arc::new(AtomicBool::new(false));
            struct Dropped(Arc<AtomicBool>);
            impl Drop for Dropped {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::SeqCst);
                }
            }
            let scope = ds_core::deadline::enter(Some(
                std::time::Instant::now() + std::time::Duration::from_millis(30),
            ));
            let dropped = Dropped(cancelled.clone());
            let result: Result<(), _> =
                store.block_on_with(Some(&handle), FetchBudget::Request, async {
                    let _dropped = dropped;
                    std::future::pending().await
                });
            assert!(matches!(result, Err(DataServerError::DeadlineExceeded)));
            assert!(cancelled.load(Ordering::SeqCst));
            // Expired requests must not start another source operation/retry.
            let result: Result<(), _> =
                store.block_on_with(Some(&handle), FetchBudget::Request, async {
                    panic!("expired I/O was polled")
                });
            assert!(matches!(result, Err(DataServerError::DeadlineExceeded)));
            drop(scope);
            assert!(ds_core::deadline::current().is_none());
            let result: Result<u8, _> =
                store.block_on_with(Some(&handle), FetchBudget::Request, async {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    Ok(42)
                });
            assert_eq!(result.unwrap(), 42);
        })
        .await
        .unwrap();
    }

    /// `get_opt_on` keeps "the object is gone" (`Ok(None)`) apart from "this
    /// read failed" (`Err`): an expired deadline is `DeadlineExceeded`, never
    /// a missing object, so a caller can negatively cache only the former.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_opt_on_separates_missing_from_deadline() {
        use object_store::ObjectStoreExt;
        let inner = object_store::memory::InMemory::new();
        let present = ObjectPath::from("present.h5");
        inner
            .put(&present, object_store::PutPayload::from_static(b"data"))
            .await
            .unwrap();
        let store = DataStore::new(Arc::new(inner));
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let missing = ObjectPath::from("missing.h5");
            assert!(store.get_opt_on(&missing, &handle).unwrap().is_none());
            assert_eq!(
                store.get_opt_on(&present, &handle).unwrap().as_deref(),
                Some(&b"data"[..])
            );
            let _scope = ds_core::deadline::enter(Some(std::time::Instant::now()));
            assert!(matches!(
                store.get_opt_on(&missing, &handle),
                Err(DataServerError::DeadlineExceeded)
            ));
            assert!(matches!(
                store.get_opt_on(&present, &handle),
                Err(DataServerError::DeadlineExceeded)
            ));
        })
        .await
        .unwrap();
    }
}

/// Fetch budgets (#1011): a background fetch outlasts object_store's
/// retry of a stalled attempt; a request fetch does not wait for it.
#[cfg(test)]
mod budget_tests {
    use super::*;
    use object_store::{BackoffConfig, ClientConfigKey, ClientOptions, PutPayload};
    use std::time::Duration;

    /// `ATTEMPT_TIMEOUT` is object_store's own per-attempt default, and the
    /// background budget holds a stalled attempt plus a whole retry.
    #[test]
    fn background_budget_holds_a_stalled_attempt_and_a_whole_retry() {
        assert_eq!(
            ClientOptions::default()
                .get_config_value(&ClientConfigKey::Timeout)
                .as_deref(),
            Some("30s")
        );
        let backoff = BackoffConfig::default().init_backoff;
        assert!(
            FetchBudget::Background.timeout() >= 2 * DataStore::ATTEMPT_TIMEOUT + backoff,
            "the background budget must outlast a stalled attempt and its retry"
        );
        assert_eq!(FetchBudget::Request.timeout(), Duration::from_secs(30));
    }

    /// A store whose first attempt stalls until object_store's per-attempt
    /// timeout, then backs off and lets the retry deliver the rest in 1 s.
    async fn stalled_store(path: &ObjectPath) -> DataStore {
        let inner = object_store::memory::InMemory::new();
        inner
            .put(path, PutPayload::from_static(b"0123456789"))
            .await
            .unwrap();
        let stall = DataStore::ATTEMPT_TIMEOUT
            + BackoffConfig::default().init_backoff
            + Duration::from_secs(1);
        DataStore::new(Arc::new(crate::test_store::StalledBody { inner, stall }))
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_get_completes_on_the_background_budget_only() {
        let path = ObjectPath::from("scan.nc");
        let store = stalled_store(&path).await;
        let get = |budget| {
            within(budget, None, async {
                store
                    .whole(&path)
                    .await
                    .map_err(|e| DataServerError::from(StorageError::from(e)))
            })
        };

        let started = tokio::time::Instant::now();
        assert_eq!(
            get(FetchBudget::Background).await.unwrap(),
            &b"0123456789"[..]
        );
        assert!(started.elapsed() > DataStore::ATTEMPT_TIMEOUT);

        match get(FetchBudget::Request).await {
            Err(DataServerError::Storage(message)) => {
                assert_eq!(message, "Request timed out after 30s")
            }
            other => panic!("the request budget must time out, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_batch_object_completes_on_the_background_budget_only() {
        let path = ObjectPath::from("volume.h5");
        let store = stalled_store(&path).await;
        let paths = [path.clone()];

        let fetched = store
            .fetch_many(&paths, 1, Some(10), FetchBudget::Background)
            .await;
        assert_eq!(fetched[0].as_ref().unwrap(), &b"0123456789"[..]);

        let fetched = store
            .fetch_many(&paths, 1, None, FetchBudget::Request)
            .await;
        match &fetched[0] {
            Err(DataServerError::Storage(message)) => {
                assert_eq!(message, "fetch of `volume.h5` timed out after 30s")
            }
            other => panic!("the request budget must time out, got {other:?}"),
        }
    }

    /// A request deadline in scope still bounds a background fetch.
    #[tokio::test(start_paused = true)]
    async fn a_request_deadline_bounds_the_background_budget() {
        let path = ObjectPath::from("scan.nc");
        let store = stalled_store(&path).await;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let result = within(FetchBudget::Background, Some(deadline), async {
            store
                .whole(&path)
                .await
                .map_err(|e| DataServerError::from(StorageError::from(e)))
        })
        .await;
        assert!(matches!(result, Err(DataServerError::DeadlineExceeded)));
    }

    /// The public entry points pass their budget through and count bytes.
    #[tokio::test(flavor = "multi_thread")]
    async fn budgeted_entry_points_fetch_and_count_bytes() {
        let inner = object_store::memory::InMemory::new();
        let path = ObjectPath::from("scan.nc");
        inner
            .put(&path, PutPayload::from_static(b"abc"))
            .await
            .unwrap();
        let store = DataStore::new(Arc::new(inner));
        assert_eq!(
            store
                .get_with_budget(&path, FetchBudget::Background)
                .unwrap(),
            &b"abc"[..]
        );
        let many = store
            .get_many_with_budget(
                std::slice::from_ref(&path),
                4,
                None,
                FetchBudget::Background,
            )
            .unwrap();
        assert_eq!(many[0].as_ref().unwrap(), &b"abc"[..]);
        assert_eq!(store.bytes_read(), 6);
    }
}
