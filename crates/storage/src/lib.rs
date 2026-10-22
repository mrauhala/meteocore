//! Shared storage abstraction for the data server.
//!
//! Provides a synchronous `DataStore` over the `object_store` crate,
//! supporting local filesystem, S3, and HTTP backends.

mod admitted;
pub mod discovery;
mod document;
mod error;
mod gauge;
#[cfg(test)]
mod test_store;
mod throttle;
mod traced;

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
pub use gauge::HostRequests;
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

/// Which version of an object a key names, as a HEAD or a GET reports it:
/// its size, its `Last-Modified` and its ETag where the store sends one.
/// With the key it is a file's identity (#1067): a file replaced under the
/// same key has another version.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ObjectVersion {
    pub size: u64,
    /// `None` when the store sent no `Last-Modified`, which object_store
    /// reports as the Unix epoch.
    pub last_modified: Option<chrono::DateTime<chrono::Utc>>,
    pub e_tag: Option<String>,
}

impl ObjectVersion {
    /// A version known only by its size, for a caller that compares no
    /// modification time (engine-geotiff's local readiness check).
    pub fn sized(size: u64) -> Self {
        Self {
            size,
            last_modified: None,
            e_tag: None,
        }
    }

    /// Whether a response naming `other` can be this version. The sizes
    /// must be equal; `Last-Modified` and the ETag are compared when both
    /// sides carry one, since a header only one response sent is no
    /// evidence of a change.
    pub fn matches(&self, other: &ObjectVersion) -> bool {
        fn agree<T: PartialEq>(a: &Option<T>, b: &Option<T>) -> bool {
            match (a, b) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            }
        }
        self.size == other.size
            && agree(&self.last_modified, &other.last_modified)
            && agree(&self.e_tag, &other.e_tag)
    }
}

impl From<&ObjectMeta> for ObjectVersion {
    fn from(meta: &ObjectMeta) -> Self {
        Self {
            size: meta.size,
            last_modified: (meta.last_modified != chrono::DateTime::UNIX_EPOCH)
                .then_some(meta.last_modified),
            e_tag: meta.e_tag.clone(),
        }
    }
}

/// What [`DataStore::get_range_if_unmodified`] read.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub enum RangeRead {
    /// The bytes, from the expected version.
    Read(Bytes),
    /// The key no longer names the expected version: the store answered
    /// 412 Precondition Failed or 404, or its response named another size,
    /// `Last-Modified` or ETag. No bytes are returned.
    Changed,
}

/// Why a store refused a range read in a way that no retry of the same read
/// can change (#1074). [`DataStore::get_range_or_refusal`] answers it as
/// data, not as an error, so a caller tells "the object is gone" from "this
/// read failed" without matching error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeRefusal {
    /// 404: no object under the key.
    NotFound,
    /// 401 or 403: the store will not serve the key to these credentials.
    /// Often the credentials or a bucket policy, not this one object.
    Forbidden,
    /// 416, or a response naming a total size that ends before the range:
    /// the object is now shorter than the range, so it was truncated or
    /// replaced.
    Truncated,
}

impl std::fmt::Display for RangeRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RangeRefusal::NotFound => "not found",
            RangeRefusal::Forbidden => "forbidden",
            RangeRefusal::Truncated => "shorter than the range",
        })
    }
}

/// What [`DataStore::get_range_or_refusal`] read.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub enum RangeAnswer {
    /// The bytes of the range.
    Read(Bytes),
    /// The store refused the range for good.
    Refused(RangeRefusal),
}

/// Synchronous wrapper around an `ObjectStore`.
///
/// Methods use `tokio::runtime::Handle::current().block_on()` to bridge
/// async `ObjectStore` operations into synchronous calls. This is safe
/// when called from a thread with access to a tokio runtime handle
/// (e.g., from `spawn_blocking` or during startup).
///
/// Every operation on a remote store runs inside a `storage` span naming
/// the source, the object key and, once [`Self::for_collection`] has named
/// it, the collection (#1233), so object_store's retry lines say where they
/// come from.
#[derive(Clone)]
pub struct DataStore {
    /// The store every operation goes through: for a remote store, its
    /// `base` inside the `storage` span ([`traced`]).
    inner: Arc<dyn ObjectStore>,
    /// `None` for a local or in-memory store, whose `inner` has no span.
    remote: Option<Arc<Remote>>,
    bytes_read: Arc<AtomicU64>,
}

/// A remote store's store without the `storage` span, and what the span
/// names.
struct Remote {
    /// [`DataStore::with_request_limit`] and [`DataStore::for_collection`]
    /// build on it, so a span never nests in another.
    base: Arc<dyn ObjectStore>,
    labels: traced::Labels,
}

impl DataStore {
    /// A store whose operations run in no span of their own: a local or
    /// in-memory one, which never retries.
    pub fn new(store: Arc<dyn ObjectStore>) -> Self {
        Self::layered(store, None, Arc::new(AtomicU64::new(0)))
    }

    /// A store reading a remote `source`, an HTTP server or an S3 bucket:
    /// each operation runs inside a `storage` span with `store = source`.
    /// `source` is what a log line shows, `host[:port]` of a server or
    /// `host[:port]/bucket` of a bucket, so it must carry no credentials
    /// or query string. Every `build_*` function of this crate that makes
    /// a remote store labels it.
    pub fn remote(store: Arc<dyn ObjectStore>, source: &str) -> Self {
        let labels = traced::Labels {
            store: source.into(),
            collection: None,
        };
        Self::layered(store, Some(labels), Arc::new(AtomicU64::new(0)))
    }

    /// `base`, inside the span `labels` name when there are any.
    fn layered(
        base: Arc<dyn ObjectStore>,
        labels: Option<traced::Labels>,
        bytes_read: Arc<AtomicU64>,
    ) -> Self {
        let Some(labels) = labels else {
            return Self {
                inner: base,
                remote: None,
                bytes_read,
            };
        };
        Self {
            inner: traced::layer(base.clone(), labels.clone()),
            remote: Some(Arc::new(Remote { base, labels })),
            bytes_read,
        }
    }

    /// The store without the `storage` span.
    fn base(&self) -> &Arc<dyn ObjectStore> {
        self.remote
            .as_ref()
            .map_or(&self.inner, |remote| &remote.base)
    }

    /// This store, reading for collection `id`: the `storage` span of each
    /// of its operations names `collection = id`. The engine calls it once,
    /// where it builds its store.
    ///
    /// The span is rebuilt on the store under it, never nested in the one
    /// it had, so an operation has exactly one `storage` span. Everything
    /// else is kept: the store, its request limits, and its byte counter.
    /// Two collections calling it on stores [`build_http_store_limited`]
    /// built for one host still share that host's limit. A local or
    /// in-memory store has no span and is returned as it is.
    #[must_use]
    pub fn for_collection(self, id: &str) -> DataStore {
        let Some(remote) = &self.remote else {
            return self;
        };
        let labels = traced::Labels {
            store: remote.labels.store.clone(),
            collection: Some(id.into()),
        };
        Self::layered(remote.base.clone(), Some(labels), self.bytes_read.clone())
    }

    /// Whether `self` and `other` read through one store, whatever
    /// collection each names: the stores [`build_http_store_limited`]
    /// built for one host do, and so does a store and the result of
    /// [`Self::for_collection`] on it.
    pub fn same_store(&self, other: &DataStore) -> bool {
        Arc::ptr_eq(self.base(), other.base())
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
        self.get_range_on_with_budget(path, range, handle, FetchBudget::Request)
    }

    /// [`Self::get_range_on`] under an explicit `budget`. Ingest on the
    /// poll runtime passes [`FetchBudget::Background`]; a read a request
    /// waits on, directly or through a single-flight fill, keeps
    /// [`FetchBudget::Request`]. A request deadline in scope still applies.
    pub fn get_range_on_with_budget(
        &self,
        path: &ObjectPath,
        range: Range<usize>,
        handle: &tokio::runtime::Handle,
        budget: FetchBudget,
    ) -> Result<Bytes, DataServerError> {
        let result = self.block_on_with(Some(handle), budget, async {
            self.inner.get_range(path, to_u64(range)).await
        })?;
        self.bytes_read
            .fetch_add(result.len() as u64, Ordering::Relaxed);
        Ok(result)
    }

    /// Several byte `ranges` of one object, one buffer per range in input
    /// order, under [`FetchBudget::Request`]. Each range is a GET of its
    /// own, up to ten at once (object_store's `coalesce_ranges`), and only
    /// ranges that overlap or touch are merged: the caller decides which
    /// near ranges are worth one GET. object_store's own `get_ranges` would
    /// merge any within 1 MiB. A missing object is `Ok(None)`, as in
    /// [`Self::get_opt`]; every other failure, the deadline included, is an
    /// `Err`. A range past the object's end may come back short, so a
    /// caller that needs the exact length checks it.
    pub fn get_ranges_opt(
        &self,
        path: &ObjectPath,
        ranges: &[Range<usize>],
    ) -> Result<Option<Vec<Bytes>>, DataServerError> {
        self.get_ranges_opt_with(None, path, ranges)
    }

    /// [`Self::get_ranges_opt`] driven on an explicitly-provided runtime
    /// `Handle`, as [`Self::get_opt_on`] is to [`Self::get_opt`].
    pub fn get_ranges_opt_on(
        &self,
        path: &ObjectPath,
        ranges: &[Range<usize>],
        handle: &tokio::runtime::Handle,
    ) -> Result<Option<Vec<Bytes>>, DataServerError> {
        self.get_ranges_opt_with(Some(handle), path, ranges)
    }

    fn get_ranges_opt_with(
        &self,
        handle: Option<&tokio::runtime::Handle>,
        path: &ObjectPath,
        ranges: &[Range<usize>],
    ) -> Result<Option<Vec<Bytes>>, DataServerError> {
        let ranges: Vec<Range<u64>> = ranges.iter().cloned().map(to_u64).collect();
        let result = self.block_on_with(handle, FetchBudget::Request, async {
            let fetch = |range| self.inner.get_range(path, range);
            match object_store::coalesce_ranges(&ranges, fetch, 0).await {
                Ok(parts) => Ok(Some(parts)),
                Err(object_store::Error::NotFound { .. }) => Ok(None),
                Err(e) => Err(e),
            }
        })?;
        if let Some(parts) = &result {
            let total: usize = parts.iter().map(Bytes::len).sum();
            self.bytes_read.fetch_add(total as u64, Ordering::Relaxed);
        }
        Ok(result)
    }

    /// Like [`Self::get_range`], but a refusal that no retry can change is a
    /// [`RangeAnswer::Refused`] rather than an error (#1074): a missing
    /// object (404), a key these credentials may not read (401/403), and a
    /// range past the object's end (416, or a response whose total size
    /// ends before the range, which object_store would otherwise clamp into
    /// a short success). Every other failure, the deadline included, stays
    /// an `Err`. Under [`FetchBudget::Request`].
    pub fn get_range_or_refusal(
        &self,
        path: &ObjectPath,
        range: Range<usize>,
    ) -> Result<RangeAnswer, DataServerError> {
        self.range_or_refusal(None, path, range)
    }

    /// [`Self::get_range_or_refusal`] driven on an explicitly-provided
    /// runtime `Handle`, as [`Self::get_range_on`] is to [`Self::get_range`].
    pub fn get_range_or_refusal_on(
        &self,
        path: &ObjectPath,
        range: Range<usize>,
        handle: &tokio::runtime::Handle,
    ) -> Result<RangeAnswer, DataServerError> {
        self.range_or_refusal(Some(handle), path, range)
    }

    fn range_or_refusal(
        &self,
        handle: Option<&tokio::runtime::Handle>,
        path: &ObjectPath,
        range: Range<usize>,
    ) -> Result<RangeAnswer, DataServerError> {
        let end = range.end as u64;
        let options = object_store::GetOptions::new().with_range(Some(to_u64(range)));
        let answer = self.block_on_result(handle, FetchBudget::Request, async {
            let storage = |e| DataServerError::from(StorageError::from(e));
            let result = match self.inner.get_opts(path, options).await {
                Ok(result) => result,
                Err(e) => {
                    return match error::range_refusal(&e) {
                        Some(refusal) => Ok(RangeAnswer::Refused(refusal)),
                        None => Err(storage(e)),
                    }
                }
            };
            if result.meta.size < end {
                return Ok(RangeAnswer::Refused(RangeRefusal::Truncated));
            }
            Ok(RangeAnswer::Read(result.bytes().await.map_err(storage)?))
        })?;
        if let RangeAnswer::Read(bytes) = &answer {
            self.bytes_read
                .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        }
        Ok(answer)
    }

    /// Read `range` of `path` only if the key still names `version`.
    ///
    /// The GET carries `If-Unmodified-Since` with the version's
    /// `Last-Modified` and, for a strong ETag, `If-Match`, so a store that
    /// honours them answers 412 for a replaced object. The response's own
    /// size (the total from `Content-Range`), `Last-Modified` and ETag are
    /// then compared with `version` ([`ObjectVersion::matches`]), which
    /// catches a store that ignores the preconditions and a replacement
    /// within the same second. Either way, and on a 404, the answer is
    /// [`RangeRead::Changed`] rather than an error, so the caller can drop
    /// what it holds for that version and re-ingest the file. Any other
    /// failure is an `Err`.
    ///
    /// Same runtime rules as [`Self::get_range_with_budget`];
    /// [`Self::get_range_if_unmodified_on`] drives it on a supplied handle.
    pub fn get_range_if_unmodified(
        &self,
        path: &ObjectPath,
        range: Range<usize>,
        version: &ObjectVersion,
        budget: FetchBudget,
    ) -> Result<RangeRead, DataServerError> {
        self.range_if_unmodified(None, path, range, version, budget)
    }

    /// [`Self::get_range_if_unmodified`] driven on an explicitly-provided
    /// runtime `Handle`, as [`Self::get_range_on`] is to
    /// [`Self::get_range`].
    pub fn get_range_if_unmodified_on(
        &self,
        path: &ObjectPath,
        range: Range<usize>,
        version: &ObjectVersion,
        handle: &tokio::runtime::Handle,
        budget: FetchBudget,
    ) -> Result<RangeRead, DataServerError> {
        self.range_if_unmodified(Some(handle), path, range, version, budget)
    }

    fn range_if_unmodified(
        &self,
        handle: Option<&tokio::runtime::Handle>,
        path: &ObjectPath,
        range: Range<usize>,
        version: &ObjectVersion,
        budget: FetchBudget,
    ) -> Result<RangeRead, DataServerError> {
        let options = object_store::GetOptions {
            range: Some(object_store::GetRange::Bounded(to_u64(range))),
            if_unmodified_since: version.last_modified,
            // `If-Match` compares strongly: a weak ETag would never match.
            if_match: version.e_tag.clone().filter(|tag| !tag.starts_with("W/")),
            ..Default::default()
        };
        let read = self.block_on_result(handle, budget, async {
            let storage = |e| DataServerError::from(StorageError::from(e));
            let result = match self.inner.get_opts(path, options).await {
                Ok(result) => result,
                Err(
                    object_store::Error::Precondition { .. } | object_store::Error::NotFound { .. },
                ) => return Ok(RangeRead::Changed),
                Err(e) => return Err(storage(e)),
            };
            if !version.matches(&ObjectVersion::from(&result.meta)) {
                return Ok(RangeRead::Changed);
            }
            Ok(RangeRead::Read(result.bytes().await.map_err(storage)?))
        })?;
        if let RangeRead::Read(bytes) = &read {
            self.bytes_read
                .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        }
        Ok(read)
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
        let result = self.block_on_with(handle, FetchBudget::Request, self.whole_opt(path))?;
        if let Some(b) = &result {
            self.bytes_read.fetch_add(b.len() as u64, Ordering::Relaxed);
        }
        Ok(result)
    }

    /// The whole object at `path`, or `None` when it is missing.
    async fn whole_opt(&self, path: &ObjectPath) -> Result<Option<Bytes>, object_store::Error> {
        match self.inner.get(path).await {
            Ok(res) => Ok(Some(res.bytes().await?)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Start [`Self::get_opt`] of `path` as a task on `handle`'s runtime and
    /// return at once; [`PendingGet::wait`] collects it. Reads started before
    /// the first is collected run concurrently, so synchronous code walking
    /// many objects can start the next few while it works on the current
    /// one, instead of paying one round trip per object in turn (root
    /// CLAUDE.md Critical Rule 9, #999). The caller bounds how many it
    /// starts, and with them how many objects it holds.
    ///
    /// Several threads can wait for one read (#1127): share the
    /// [`PendingGet`] behind an `Arc`, and the read runs until it finishes
    /// or the last holder drops it. So the read is limited by
    /// [`FetchBudget::Request`] alone, and each [`PendingGet::wait`] by the
    /// request deadline in scope on the waiting thread: one request's
    /// deadline ends its own wait, never the read another request waits on.
    /// A deadline that has already passed when the read is asked for fails
    /// it without sending it.
    ///
    /// `handle` must be a multi-thread runtime: a current-thread runtime
    /// runs the task only while something drives it, so the reads would not
    /// progress while the caller works.
    pub fn start_get_opt(&self, path: &ObjectPath, handle: &tokio::runtime::Handle) -> PendingGet {
        use futures::FutureExt;
        debug_assert_eq!(
            handle.runtime_flavor(),
            tokio::runtime::RuntimeFlavor::MultiThread,
            "start_get_opt needs a multi-thread runtime"
        );
        if ds_core::deadline::check().is_err() {
            return PendingGet {
                path: path.clone(),
                read: PendingRead::Expired,
            };
        }
        let store = self.clone();
        let object = path.clone();
        let task = handle.spawn(async move {
            let result = within(FetchBudget::Request, None, async {
                store
                    .whole_opt(&object)
                    .await
                    .map_err(|e| DataServerError::from(StorageError::from(e)))
            })
            .await?;
            if let Some(b) = &result {
                store
                    .bytes_read
                    .fetch_add(b.len() as u64, Ordering::Relaxed);
            }
            Ok(result)
        });
        let abort = task.abort_handle();
        let result = async move {
            match task.await {
                Ok(Ok(bytes)) => Ok(bytes),
                Ok(Err(DataServerError::Storage(e))) => Err(e),
                Ok(Err(e)) => Err(e.to_string()),
                Err(e) => Err(format!("read task failed: {e}")),
            }
        };
        PendingGet {
            path: path.clone(),
            read: PendingRead::Running {
                result: result.boxed().shared(),
                abort,
            },
        }
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

    /// Read one byte range of each of many objects, at most `concurrency`
    /// in flight, returning one slot per read **in input order** (#1042).
    /// Each read is limited to the request deadline when one is in scope,
    /// else to `budget`; a failed read is its slot's `Err` and does not
    /// sink the batch.
    ///
    /// The batch ends at `until`. A read that has not finished by then is
    /// `None`: no read starts after it, and one still in flight is dropped.
    /// A poll that reads the headers of a backlog keeps what arrived within
    /// its time and leaves the rest to the next poll, so order `reads` by
    /// priority. The outer `Err` is a runtime-bridge failure or an expired
    /// request deadline.
    ///
    /// Drives the whole batch on ONE bridge call, with the thread-context
    /// rules of [`Self::get_many`]: call it from the poll runtime, never
    /// from a rayon worker. Every range is held until the batch returns, so
    /// call it with a bounded batch.
    #[allow(clippy::type_complexity)]
    pub fn get_range_many(
        &self,
        reads: &[(ObjectPath, Range<usize>)],
        concurrency: usize,
        budget: FetchBudget,
        until: std::time::Instant,
    ) -> Result<Vec<Option<Result<Bytes, DataServerError>>>, DataServerError> {
        let deadline = ds_core::deadline::current();
        ds_core::deadline::check()?;
        let results =
            self.block_on_untimed(self.read_ranges(reads, concurrency, budget, deadline, until))?;
        let total: u64 = results
            .iter()
            .filter_map(|r| r.as_ref()?.as_ref().ok())
            .map(|b| b.len() as u64)
            .sum();
        self.bytes_read.fetch_add(total, Ordering::Relaxed);
        Ok(results)
    }

    /// The batch [`Self::get_range_many`] drives: one slot per read, in
    /// input order, `None` for a read `until` cut off or never started.
    async fn read_ranges(
        &self,
        reads: &[(ObjectPath, Range<usize>)],
        concurrency: usize,
        budget: FetchBudget,
        deadline: Option<std::time::Instant>,
        until: std::time::Instant,
    ) -> Vec<Option<Result<Bytes, DataServerError>>> {
        use futures::StreamExt;

        // The runtime's clock, so a test with paused time moves it too.
        let until = tokio::time::Instant::from_std(until);
        let mut results: Vec<(usize, Option<Result<Bytes, DataServerError>>)> =
            futures::stream::iter(
                reads
                    .iter()
                    .enumerate()
                    .map(|(i, (path, range))| async move {
                        if tokio::time::Instant::now() >= until {
                            return (i, None);
                        }
                        let read = within(budget, deadline, async {
                            self.inner
                                .get_range(path, to_u64(range.clone()))
                                .await
                                .map_err(|e| DataServerError::from(StorageError::from(e)))
                        });
                        (i, tokio::time::timeout_at(until, read).await.ok())
                    }),
            )
            .buffer_unordered(concurrency.max(1))
            .collect()
            .await;
        results.sort_by_key(|(i, _)| *i);
        results.into_iter().map(|(_, r)| r).collect()
    }

    /// Get whole documents, one result per path **in input order**, at
    /// most `concurrency` in flight on one storage-bridge call: generated
    /// files such as THREDDS catalogs (#1170). Unlike [`Self::get_many`],
    /// an HTTP response without `Content-Length` (a chunked body) is read,
    /// by the store's own transport and under all its layers: its request
    /// limit and share, its host's `Retry-After` and 429 back-off, its
    /// `storage` span, no redirect, no decompression (`document.rs`). Each
    /// document is at most `max_bytes`, which fails it as soon as its
    /// declared length or its body read so far passes it, and is limited
    /// to the request deadline when one is in scope, else to `budget`. A
    /// 429, a 5xx or a failed connection is retried twice, as object_store
    /// retries. Any other store answers a plain GET, capped as it streams.
    /// The outer `Err` is a runtime-bridge failure or an expired deadline.
    #[allow(clippy::type_complexity)]
    pub fn get_documents(
        &self,
        paths: &[ObjectPath],
        concurrency: usize,
        max_bytes: u64,
        budget: FetchBudget,
    ) -> Result<Vec<Result<Bytes, DataServerError>>, DataServerError> {
        use futures::StreamExt;

        let deadline = ds_core::deadline::current();
        ds_core::deadline::check()?;
        let mut results: Vec<(usize, Result<Bytes, DataServerError>)> = self.block_on_untimed(
            futures::stream::iter(paths.iter().enumerate().map(|(i, path)| async move {
                (
                    i,
                    within(budget, deadline, self.document(path, max_bytes)).await,
                )
            }))
            .buffer_unordered(concurrency.max(1))
            .collect(),
        )?;
        results.sort_by_key(|(i, _)| *i);
        let total: u64 = results
            .iter()
            .filter_map(|(_, r)| r.as_ref().ok())
            .map(|b| b.len() as u64)
            .sum();
        self.bytes_read.fetch_add(total, Ordering::Relaxed);
        Ok(results.into_iter().map(|(_, r)| r).collect())
    }

    /// One document of [`Self::get_documents`].
    async fn document(&self, path: &ObjectPath, max_bytes: u64) -> Result<Bytes, DataServerError> {
        use futures::StreamExt;

        let storage = |e| DataServerError::from(StorageError::from(e));
        let mut extensions = object_store::Extensions::new();
        extensions.insert(document::Document { max_bytes });
        let options = object_store::GetOptions {
            extensions,
            ..Default::default()
        };
        let result = self.inner.get_opts(path, options).await.map_err(storage)?;
        let over = |bytes: u64| {
            DataServerError::Storage(format!(
                "document `{path}` is {bytes} bytes or more, over the {max_bytes}-byte limit"
            ))
        };
        if result.meta.size > max_bytes {
            return Err(over(result.meta.size));
        }
        let mut stream = result.into_stream();
        let mut read = bytes::BytesMut::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(storage)?;
            if (read.len() + chunk.len()) as u64 > max_bytes {
                return Err(over((read.len() + chunk.len()) as u64));
            }
            read.extend_from_slice(&chunk);
        }
        Ok(read.freeze())
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
        block_on_ambient(future)
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
    /// (e.g., the catalog poll loop). A remote store's operations still run
    /// inside its `storage` span.
    pub fn inner(&self) -> &Arc<dyn ObjectStore> {
        &self.inner
    }

    /// This store with at most `max_requests` (at least 1) of its
    /// operations in flight at once, through object_store's `LimitStore`.
    ///
    /// Clones of the result share the limit, and it stacks on any limit
    /// this store already has: a share of 2 for background traffic within
    /// a host's limit of 8 is `host_store.with_request_limit(2)`. A GET
    /// holds its permit until its body has been read. The wait for a permit
    /// counts against the operation's budget and the request deadline, like
    /// the request itself. The byte counter and the `storage` span's labels
    /// are this store's.
    pub fn with_request_limit(&self, max_requests: usize) -> DataStore {
        let base = Arc::new(object_store::limit::LimitStore::new(
            self.base().clone(),
            max_requests.max(1),
        ));
        let labels = self.remote.as_ref().map(|remote| remote.labels.clone());
        Self::layered(base, labels, self.bytes_read.clone())
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

/// Drive `future` to completion from synchronous code on the ambient
/// runtime: a multi-threaded async worker yields via `block_in_place`, a
/// blocking-pool worker runs it directly, and a foreign thread without a
/// runtime creates a temporary one. Current-thread async tasks and
/// `LocalSet` are unsupported.
fn block_on_ambient<F: std::future::Future>(future: F) -> Result<F::Output, DataServerError> {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => Ok(tokio::task::block_in_place(|| handle.block_on(future))),
        Err(_) => {
            let rt = tokio::runtime::Runtime::new()
                .map_err(|e| DataServerError::Storage(format!("Cannot create runtime: {e}")))?;
            Ok(rt.block_on(future))
        }
    }
}

/// A whole-object read running on a runtime ahead of the synchronous code
/// that needs its bytes, from [`DataStore::start_get_opt`]. Collect it with
/// [`Self::wait`], from as many threads as hold it (#1127); dropping it
/// aborts the read, so its last holder going away unread stops it.
#[must_use = "dropping a PendingGet aborts its read"]
pub struct PendingGet {
    path: ObjectPath,
    read: PendingRead,
}

/// What a started read ends with, cloned to each waiter: the bytes, `None`
/// for a missing object, or the failure's message. Every failure of the
/// read itself is a [`DataServerError::Storage`], which is not `Clone`.
type ReadOutcome = Result<Option<Bytes>, String>;

enum PendingRead {
    /// The request deadline had passed when the read was asked for, so it
    /// was never sent.
    Expired,
    Running {
        /// The task's outcome, which every waiter polls.
        result: futures::future::Shared<futures::future::BoxFuture<'static, ReadOutcome>>,
        abort: tokio::task::AbortHandle,
    },
}

impl PendingGet {
    /// Wait for the read and return what [`DataStore::get_opt`] would have:
    /// `Ok(None)` for a missing object, and `Err` for every other failure.
    /// The wait is limited by the request deadline in scope on this thread,
    /// `DeadlineExceeded` when it passes; the read goes on for any other
    /// holder.
    ///
    /// `handle` picks the bridge by the caller's context, as
    /// [`DataStore::get_opt_on`] and [`DataStore::get_opt`] do: `Some` drives
    /// the wait with `handle.block_on`, for a `spawn_blocking` worker or a
    /// foreign thread; `None` uses `block_in_place` around the ambient
    /// runtime, for a multi-thread runtime worker.
    pub fn wait(
        &self,
        handle: Option<&tokio::runtime::Handle>,
    ) -> Result<Option<Bytes>, DataServerError> {
        let result = match &self.read {
            PendingRead::Expired => return Err(DataServerError::DeadlineExceeded),
            PendingRead::Running { result, .. } => result.clone(),
        };
        let deadline = ds_core::deadline::current();
        let waited = async move {
            match deadline {
                Some(deadline) => tokio::time::timeout_at(deadline.into(), result)
                    .await
                    .map_err(|_| DataServerError::DeadlineExceeded),
                None => Ok(result.await),
            }
        };
        let outcome = match handle {
            Some(handle) => handle.block_on(waited),
            None => block_on_ambient(waited)?,
        }?;
        outcome.map_err(DataServerError::Storage)
    }

    /// Whether the read is known to have failed: it ended with an error a
    /// waiter has seen, or it was never sent. A caller that wants the object
    /// starts another read rather than waiting for this one.
    pub fn failed(&self) -> bool {
        match &self.read {
            PendingRead::Expired => true,
            PendingRead::Running { result, .. } => matches!(result.peek(), Some(Err(_))),
        }
    }
}

impl Drop for PendingGet {
    fn drop(&mut self) {
        if let PendingRead::Running { abort, .. } = &self.read {
            abort.abort();
        }
    }
}

impl std::fmt::Debug for PendingGet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingGet")
            .field("path", &self.path)
            .finish_non_exhaustive()
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
    let source = traced::s3_label(Some(endpoint), None, bucket);
    Ok(DataStore::remote(Arc::new(store), &source))
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
    use object_store::aws::AmazonS3ConfigKey;
    // Parse s3://bucket/prefix/path/ (scheme matched case-insensitively).
    let without_scheme = strip_scheme(data_path, "s3://")
        .ok_or_else(|| DataServerError::Storage("Expected s3:// prefix".into()))?;

    let (bucket, prefix) = match without_scheme.find('/') {
        Some(idx) => (&without_scheme[..idx], &without_scheme[idx + 1..]),
        None => (without_scheme, ""),
    };

    let builder = object_store::aws::AmazonS3Builder::from_env().with_bucket_name(bucket);
    // The endpoint and region object_store will use (`AWS_ENDPOINT_URL_S3`
    // over `AWS_ENDPOINT`), for the span's label only.
    let config = |key| builder.get_config_value(&key);
    let source = traced::s3_label(
        config(AmazonS3ConfigKey::S3Endpoint)
            .or_else(|| config(AmazonS3ConfigKey::Endpoint))
            .as_deref(),
        config(AmazonS3ConfigKey::Region).as_deref(),
        bucket,
    );
    let store = builder.build().map_err(|e| {
        DataServerError::Storage(format!("Cannot create S3 store for bucket '{bucket}': {e}"))
    })?;

    let prefix_path = ObjectPath::from(prefix.trim_end_matches('/'));
    Ok((DataStore::remote(Arc::new(store), &source), prefix_path))
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
    let source = traced::s3_label(Some(&endpoint), None, &bucket);
    Ok((DataStore::remote(Arc::new(store), &source), prefix_path))
}

/// The generic HTTP backend must not follow an operator-trusted URL to an
/// unvalidated host. object_store's default connector follows redirects;
/// its injectable connector lets us enforce the policy for every operation,
/// and send every request through its host's throttle (#1235). `S` is the
/// transport: the no-redirect `reqwest::Client` [`http_object_store`]
/// builds, or a scripted one in tests.
#[derive(Debug)]
struct NoRedirectConnector<S>(throttle::Throttled<S>);

impl<S: object_store::client::HttpService + Clone> object_store::client::HttpConnector
    for NoRedirectConnector<S>
{
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
///
/// It has no request limit, but it honours its host's `Retry-After` with
/// every other store on the host ([`build_http_store_limited`]).
pub fn build_http_store(data_path: &str) -> Result<(DataStore, ObjectPath), DataServerError> {
    http_store(data_path, http_object_store)
}

/// [`build_http_store`] on the store `connect` makes for the base URL:
/// [`http_object_store`], or one on a scripted transport in tests.
fn http_store(
    data_path: &str,
    connect: impl Fn(&url::Url) -> Result<Arc<dyn ObjectStore>, DataServerError>,
) -> Result<(DataStore, ObjectPath), DataServerError> {
    let (base_url, path) = http_base(data_path)?;
    let store = connect(&base_url)?;
    Ok((DataStore::remote(store, &http_label(&base_url)), path))
}

/// A host store [`build_http_store_limited`] built: its configured limit,
/// the limited store, its adaptive limit, and its request counters.
#[derive(Clone)]
struct HostStore {
    limit: usize,
    adaptive: Arc<throttle::AdaptiveLimit>,
    store: Arc<dyn ObjectStore>,
    gauges: Arc<gauge::HostGauges>,
}

/// The HTTP stores [`build_http_store_limited`] has built, by base URL.
static HOST_STORES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::BTreeMap<String, HostStore>>,
> = std::sync::LazyLock::new(Default::default);

/// [`build_http_store`] with at most `max_requests` (at least 1) requests
/// in flight to the URL's host at once.
///
/// The limit is shared by every store built through this function for the
/// same scheme, host and port, whatever their paths, so collections reading
/// one server together stay under it. A URL's query string is part of its
/// base (every request carries it), so URLs that differ in it get limits
/// of their own. The first call for a base fixes its limit for the life of
/// the process: a later call naming another limit shares the first and
/// logs a WARN. [`host_requests`] reports the requests in flight and
/// waiting under each limit. A GET holds its permit until its body has
/// been read, and [`DataStore::with_request_limit`] stacks a share on it.
///
/// The limit adapts to a host that throttles (#1235): a 429 Too Many
/// Requests halves it, never below 1 and at most once a second, and each
/// 30 s without one adds one back, up to the configured limit. Requests in
/// flight when it is cut finish. A 429's `Retry-After` pauses new requests
/// to the host, from every store on it, for at most a minute
/// (`throttle.rs`).
pub fn build_http_store_limited(
    data_path: &str,
    max_requests: usize,
) -> Result<(DataStore, ObjectPath), DataServerError> {
    limited_http_store(data_path, max_requests, http_object_store)
}

/// [`build_http_store_limited`] on the store `connect` makes for a base
/// URL the first time it is named: [`http_object_store`], or one on a
/// scripted transport in tests.
fn limited_http_store(
    data_path: &str,
    max_requests: usize,
    connect: impl Fn(&url::Url) -> Result<Arc<dyn ObjectStore>, DataServerError>,
) -> Result<(DataStore, ObjectPath), DataServerError> {
    let (base_url, path) = http_base(data_path)?;
    let max_requests = max_requests.max(1);
    let mut hosts = HOST_STORES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let host = match hosts.get(base_url.as_str()) {
        Some(host) => host.clone(),
        None => {
            let gauges = Arc::new(gauge::HostGauges::default());
            let counted: Arc<dyn ObjectStore> = Arc::new(gauge::Gauged {
                inner: connect(&base_url)?,
                count: gauges.in_flight.clone(),
            });
            let adaptive = Arc::new(throttle::AdaptiveLimit::new(
                http_label(&base_url),
                max_requests,
            ));
            throttle::for_url(&base_url).add_limit(adaptive.clone());
            let limited: Arc<dyn ObjectStore> = Arc::new(throttle::Limited {
                inner: counted,
                limit: adaptive.clone(),
            });
            let host = HostStore {
                limit: max_requests,
                adaptive,
                store: Arc::new(gauge::Gauged {
                    inner: limited,
                    count: gauges.requested.clone(),
                }),
                gauges,
            };
            hosts.insert(base_url.to_string(), host.clone());
            host
        }
    };
    if host.limit != max_requests {
        tracing::warn!(
            "{base_url} is already limited to {} concurrent requests; \
             keeping that limit, not {max_requests}",
            host.limit
        );
    }
    Ok((DataStore::remote(host.store, &http_label(&base_url)), path))
}

/// The requests in flight and waiting under each limit
/// [`build_http_store_limited`] has made, and the limit as it is now: the
/// configured one, or less while the host backs off from a 429 (#1235).
/// One entry per host and port: the bases of one host that differ in
/// their query strings are summed. One entry per configured host, so a
/// bounded label set for `/metrics`.
pub fn host_requests() -> Vec<HostRequests> {
    requests_to(|_| true)
}

/// [`host_requests`] of the hosts whose `host[:port]` is `wanted`. Reading
/// a limit applies the growth it has earned by the caller's clock, so
/// paused-time tests read only their own host: another test's clock would
/// grow it.
fn requests_to(wanted: impl Fn(&str) -> bool) -> Vec<HostRequests> {
    let hosts = HOST_STORES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut by_host: std::collections::BTreeMap<String, HostRequests> = Default::default();
    for (base, host) in hosts.iter() {
        let label = url::Url::parse(base)
            .ok()
            .and_then(|url| traced::host_port(&url))
            .unwrap_or_default();
        if !wanted(&label) {
            continue;
        }
        let (in_flight, queued) = host.gauges.read();
        let entry = by_host
            .entry(label.clone())
            .or_insert_with(|| HostRequests {
                host: label,
                limit: 0,
                in_flight: 0,
                queued: 0,
            });
        entry.limit += host.adaptive.limits(tokio::time::Instant::now()).0;
        entry.in_flight += in_flight;
        entry.queued += queued;
    }
    by_host.into_values().collect()
}

/// The `storage` span's `store` label of an HTTP store on `base_url`: its
/// `host[:port]`, without the userinfo or query string the URL may carry.
fn http_label(base_url: &url::Url) -> String {
    traced::host_port(base_url).unwrap_or_default()
}

/// The base URL an HTTP store is built on (the URL without its path or
/// fragment) and the path within it.
fn http_base(data_path: &str) -> Result<(url::Url, ObjectPath), DataServerError> {
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
    let path = url.path().trim_start_matches('/').trim_end_matches('/');
    Ok((base_url, ObjectPath::from(path)))
}

/// A no-redirect HTTP object store on `base_url`, under its host's
/// throttle.
fn http_object_store(base_url: &url::Url) -> Result<Arc<dyn ObjectStore>, DataServerError> {
    // Own all transport options here rather than using ClientOptions (whose
    // reqwest builder is private). Disable transparent decompression to keep
    // GRIB/COG byte ranges and Content-Length exact even with feature unification.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .https_only(base_url.scheme() != "http")
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(DataStore::ATTEMPT_TIMEOUT)
        .no_gzip()
        .no_brotli()
        .no_zstd()
        .no_deflate()
        .build()
        .map_err(|e| DataServerError::Storage(format!("Cannot create HTTP client: {e}")))?;
    throttled_http_store(base_url, client)
}

/// An HTTP object store on `base_url` sending every request, object_store's
/// retries included, through `transport` under the throttle of the base's
/// scheme, host and port ([`throttle`]).
fn throttled_http_store<S: object_store::client::HttpService + Clone>(
    base_url: &url::Url,
    transport: S,
) -> Result<Arc<dyn ObjectStore>, DataServerError> {
    let throttled = throttle::Throttled {
        inner: transport,
        host: throttle::for_url(base_url),
    };
    let connector = NoRedirectConnector(throttled.clone());
    let store = object_store::http::HttpBuilder::new()
        .with_url(base_url.as_str())
        .with_http_connector(connector)
        .build()
        .map_err(|e| {
            DataServerError::Storage(format!("Cannot create HTTP store for {base_url}: {e}"))
        })?;
    // Documents without `Content-Length` go through the same transport
    // (`DataStore::get_documents`, #1170).
    Ok(Arc::new(document::Documents {
        http: Arc::new(store),
        transport: throttled,
        base_url: base_url.clone(),
    }))
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

    /// #1047: `get_ranges_opt[_on]` returns each range's bytes in input
    /// order and counts them, keeps a missing object (`Ok(None)`) apart from
    /// a failed read, and answers an expired deadline without reading.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_ranges_opt_reads_ranges_and_separates_missing_from_deadline() {
        use object_store::ObjectStoreExt;
        let inner = object_store::memory::InMemory::new();
        let present = ObjectPath::from("present.h5");
        let data: Vec<u8> = (0..=255).collect();
        inner
            .put(&present, object_store::PutPayload::from(data))
            .await
            .unwrap();
        let store = DataStore::new(Arc::new(inner));
        let handle = tokio::runtime::Handle::current();
        let ranges = [200..203, 4..6];
        let parts = store.get_ranges_opt(&present, &ranges).unwrap().unwrap();
        assert_eq!(parts, [&[200u8, 201, 202][..], &[4, 5][..]]);
        assert_eq!(store.bytes_read(), 5);
        tokio::task::spawn_blocking(move || {
            let missing = ObjectPath::from("missing.h5");
            assert!(store
                .get_ranges_opt_on(&missing, &ranges, &handle)
                .unwrap()
                .is_none());
            let parts = store.get_ranges_opt_on(&present, &ranges, &handle).unwrap();
            assert_eq!(parts.unwrap()[1].as_ref(), &[4, 5]);
            let _scope = ds_core::deadline::enter(Some(std::time::Instant::now()));
            assert!(matches!(
                store.get_ranges_opt_on(&missing, &ranges, &handle),
                Err(DataServerError::DeadlineExceeded)
            ));
            assert!(matches!(
                store.get_ranges_opt(&present, &ranges),
                Err(DataServerError::DeadlineExceeded)
            ));
        })
        .await
        .unwrap();
    }

    /// #999: reads started with `start_get_opt` run concurrently on the
    /// runtime and are collected in any order; a missing object stays
    /// `Ok(None)`, apart from a failed read, which `failed` reports. #1127:
    /// threads holding one read share its GET; a waiter's deadline ends its
    /// own wait, not the read; and a read asked for past the deadline is
    /// never sent.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn started_reads_overlap_are_shared_and_waits_keep_their_own_deadline() {
        use crate::test_store::MetaStore;
        use std::time::{Duration, Instant};
        let meta = Arc::new(MetaStore {
            delay: Duration::from_millis(200),
            ..MetaStore::default()
        });
        for key in ["a", "b", "c", "failing"] {
            meta.set(key, 0, None);
        }
        meta.failing.lock().unwrap().insert("failing".to_string());
        let store = DataStore::new(meta.clone());
        let handle = tokio::runtime::Handle::current();
        let on_worker = (store.clone(), handle.clone());
        tokio::task::spawn_blocking(move || {
            let start = |key: &str| store.start_get_opt(&ObjectPath::from(key), &handle);
            let reads: Vec<_> = ["a", "b", "c", "missing", "failing"]
                .into_iter()
                .map(start)
                .collect();
            let mut results: Vec<_> = reads
                .iter()
                .rev()
                .map(|read| read.wait(Some(&handle)))
                .collect();
            results.reverse();
            assert_eq!(
                meta.peak.load(Ordering::SeqCst),
                5,
                "every read was in flight before the first was collected"
            );
            for result in &results[..3] {
                assert_eq!(result.as_ref().unwrap().as_deref(), Some(&b""[..]));
            }
            assert!(matches!(results[3], Ok(None)), "missing: {:?}", results[3]);
            assert!(
                matches!(results[4], Err(DataServerError::Storage(_))),
                "a failed read is an error, not a missing object: {:?}",
                results[4]
            );
            let failed: Vec<_> = reads.iter().map(PendingGet::failed).collect();
            assert_eq!(failed, [false, false, false, false, true]);
            drop(reads);

            meta.gets.store(0, Ordering::SeqCst);
            let read = start("a");
            std::thread::scope(|scope| {
                for _ in 0..3 {
                    scope.spawn(|| {
                        assert_eq!(read.wait(Some(&handle)).unwrap().as_deref(), Some(&b""[..]));
                    });
                }
            });
            assert_eq!(
                meta.gets.load(Ordering::SeqCst),
                1,
                "three waiters, one GET"
            );

            let read = start("b");
            let scope = ds_core::deadline::enter(Some(Instant::now() + Duration::from_millis(50)));
            assert!(matches!(
                read.wait(Some(&handle)),
                Err(DataServerError::DeadlineExceeded)
            ));
            drop(scope);
            assert_eq!(
                read.wait(Some(&handle)).unwrap().as_deref(),
                Some(&b""[..]),
                "the read went on past the first waiter's deadline"
            );

            meta.gets.store(0, Ordering::SeqCst);
            let _scope = ds_core::deadline::enter(Some(Instant::now()));
            let read = start("a");
            assert!(read.failed(), "an expired read is not joined");
            assert!(matches!(
                read.wait(Some(&handle)),
                Err(DataServerError::DeadlineExceeded)
            ));
            assert_eq!(
                meta.gets.load(Ordering::SeqCst),
                0,
                "an expired read is not sent"
            );
        })
        .await
        .unwrap();

        // A runtime worker collects through `block_in_place` (`None`).
        let (store, handle) = on_worker;
        let read = tokio::spawn(async move {
            store
                .start_get_opt(&ObjectPath::from("b"), &handle)
                .wait(None)
        });
        assert!(matches!(read.await.unwrap(), Ok(Some(_))));
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

    /// `count` objects `k0`, `k1`, …, object `i` holding ten bytes of `i`,
    /// every GET taking `delay`.
    async fn slow_reads(count: usize, delay: Duration) -> Arc<crate::test_store::SlowReads> {
        let store = crate::test_store::SlowReads {
            delay,
            ..Default::default()
        };
        for i in 0..count {
            store
                .inner
                .put(
                    &ObjectPath::from(format!("k{i}")),
                    PutPayload::from(vec![i as u8; 10]),
                )
                .await
                .unwrap();
        }
        Arc::new(store)
    }

    /// Bytes 2..5 of each of `count` objects.
    fn header_reads(count: usize) -> Vec<(ObjectPath, Range<usize>)> {
        (0..count)
            .map(|i| (ObjectPath::from(format!("k{i}")), 2..5))
            .collect()
    }

    /// A range batch keeps `concurrency` reads in flight and never more, so
    /// ten one-second reads three at a time take four seconds, not ten
    /// (#1042). Each slot holds its own read's range, in input order.
    #[tokio::test(start_paused = true)]
    async fn a_range_batch_overlaps_its_reads_up_to_the_bound() {
        let slow = slow_reads(10, Duration::from_secs(1)).await;
        let store = DataStore::new(slow.clone());
        let started = tokio::time::Instant::now();
        let until = (started + Duration::from_secs(3600)).into_std();

        let read = store
            .read_ranges(&header_reads(10), 3, FetchBudget::Background, None, until)
            .await;
        let elapsed = started.elapsed();
        assert!(
            (Duration::from_secs(4)..Duration::from_secs(5)).contains(&elapsed),
            "{elapsed:?}"
        );
        assert_eq!(slow.peak.load(Ordering::SeqCst), 3);
        for (i, slot) in read.iter().enumerate() {
            let bytes = slot.as_ref().expect("finished").as_ref().unwrap();
            assert_eq!(bytes, &[i as u8; 3][..]);
        }
    }

    /// A range batch ends at `until`: the reads that finished keep their
    /// bytes, a read still in flight is dropped and one not yet started
    /// never starts, both `None`.
    #[tokio::test(start_paused = true)]
    async fn a_range_batch_ends_at_until() {
        let slow = slow_reads(6, Duration::from_secs(1)).await;
        let store = DataStore::new(slow.clone());
        let started = tokio::time::Instant::now();
        let until = (started + Duration::from_millis(1500)).into_std();

        let read = store
            .read_ranges(&header_reads(6), 2, FetchBudget::Background, None, until)
            .await;
        assert_eq!(started.elapsed(), Duration::from_millis(1500));
        let finished: Vec<bool> = read
            .iter()
            .map(|slot| matches!(slot, Some(Ok(_))))
            .collect();
        assert_eq!(finished, [true, true, false, false, false, false]);
        assert!(read[2..].iter().all(Option::is_none));
        // k2 and k3 were in flight at `until`; k4 and k5 never started.
        assert_eq!(*slow.started.lock().unwrap(), ["k0", "k1", "k2", "k3"]);
        assert_eq!(slow.active.load(Ordering::SeqCst), 0);
    }

    /// The public entry point reads on the ambient runtime, counts the bytes
    /// of the reads that finished and keeps a failed read in its slot; once
    /// `until` has passed it starts nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_range_batch_counts_bytes_and_keeps_failures_in_their_slot() {
        let slow = slow_reads(2, Duration::ZERO).await;
        let store = DataStore::new(slow.clone());
        let mut reads = header_reads(2);
        reads.push((ObjectPath::from("missing"), 0..4));
        let later = std::time::Instant::now() + Duration::from_secs(60);

        let read = store
            .get_range_many(&reads, 4, FetchBudget::Background, later)
            .unwrap();
        assert_eq!(read[0].as_ref().unwrap().as_ref().unwrap(), &[0u8; 3][..]);
        assert_eq!(read[1].as_ref().unwrap().as_ref().unwrap(), &[1u8; 3][..]);
        assert!(matches!(read[2], Some(Err(_))), "{:?}", read[2]);
        assert_eq!(store.bytes_read(), 6);

        slow.started.lock().unwrap().clear();
        let read = store
            .get_range_many(
                &reads,
                4,
                FetchBudget::Background,
                std::time::Instant::now(),
            )
            .unwrap();
        assert!(read.iter().all(Option::is_none));
        assert!(slow.started.lock().unwrap().is_empty());
        assert_eq!(store.bytes_read(), 6);
    }
}

/// Range reads for remote files read in place (#1067): budgeted reads on a
/// supplied runtime, reads conditional on the object's version, and
/// per-host request limits.
#[cfg(test)]
mod range_tests {
    use super::*;
    use crate::test_store::{JsonLog, MetaStore, RangeServer};
    use chrono::{DateTime, Utc};
    use object_store::PutPayload;

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    /// The budgeted range read works from a thread without a runtime and
    /// counts its bytes; a request deadline still bounds it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_budgeted_range_read_runs_on_the_supplied_handle() {
        let inner = object_store::memory::InMemory::new();
        let path = ObjectPath::from("meps.nc");
        inner
            .put(&path, PutPayload::from_static(b"0123456789"))
            .await
            .unwrap();
        let store = DataStore::new(Arc::new(inner));
        let handle = tokio::runtime::Handle::current();
        let read = std::thread::scope(|s| {
            s.spawn(|| {
                let read = store
                    .get_range_on_with_budget(&path, 2..6, &handle, FetchBudget::Background)
                    .unwrap();
                let _scope = ds_core::deadline::enter(Some(std::time::Instant::now()));
                let expired =
                    store.get_range_on_with_budget(&path, 2..6, &handle, FetchBudget::Background);
                assert!(matches!(expired, Err(DataServerError::DeadlineExceeded)));
                read
            })
            .join()
            .unwrap()
        });
        assert_eq!(read, &b"2345"[..]);
        assert_eq!(store.bytes_read(), 4);
    }

    /// An object store that honours the preconditions: a rewrite of the
    /// same size or a delete is `Changed`, never the new bytes.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_conditional_read_of_a_replaced_object_is_changed() {
        let store = DataStore::new(Arc::new(object_store::memory::InMemory::new()));
        let path = ObjectPath::from("meps.nc");
        store
            .inner()
            .put(&path, PutPayload::from_static(b"0123456789"))
            .await
            .unwrap();
        let version = ObjectVersion::from(&store.head(&path).unwrap());
        let read = |version: &ObjectVersion| {
            store
                .get_range_if_unmodified(&path, 2..5, version, FetchBudget::Request)
                .unwrap()
        };
        assert_eq!(read(&version), RangeRead::Read(Bytes::from_static(b"234")));
        assert_eq!(store.bytes_read(), 3);

        store
            .inner()
            .put(&path, PutPayload::from_static(b"abcdefghij"))
            .await
            .unwrap();
        assert_eq!(read(&version), RangeRead::Changed, "If-Match");
        let undated = ObjectVersion {
            e_tag: None,
            ..version.clone()
        };
        assert_eq!(read(&undated), RangeRead::Changed, "If-Unmodified-Since");
        store.inner().delete(&path).await.unwrap();
        assert_eq!(read(&version), RangeRead::Changed, "gone");
        assert_eq!(
            store.bytes_read(),
            3,
            "a changed object's bytes are not read"
        );
    }

    /// Over HTTP: the GET carries `If-Unmodified-Since`, a 412 is
    /// `Changed`, and so is a 206 from a server that ignores the header but
    /// names another `Last-Modified` or size. A 404 is `Changed` too.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_conditional_read_over_http_sends_and_checks_the_version() {
        let modified = at("2026-10-17T08:00:00Z");
        let server = RangeServer::new(b"0123456789", modified);
        let (store, path) = build_http_store(&format!("{}/object", server.url)).unwrap();
        let version = ObjectVersion::from(&store.head(&path).unwrap());
        assert_eq!(
            version,
            ObjectVersion {
                size: 10,
                last_modified: Some(modified),
                e_tag: None
            }
        );
        let read = |path: &ObjectPath| {
            store
                .get_range_if_unmodified(path, 2..5, &version, FetchBudget::Request)
                .unwrap()
        };

        assert_eq!(read(&path), RangeRead::Read(Bytes::from_static(b"234")));
        let request = server.object.lock().unwrap().last_request.clone();
        assert!(request.contains("range: bytes=2-4"), "{request}");
        let since = modified.format("%a, %d %b %Y %H:%M:%S gmt").to_string();
        assert!(
            request.contains(&format!("if-unmodified-since: {}", since.to_lowercase())),
            "{request}"
        );

        server.replace(b"abcdefghij", at("2026-10-17T09:00:00Z"));
        assert_eq!(read(&path), RangeRead::Changed, "412");
        server.object.lock().unwrap().ignore_preconditions = true;
        assert_eq!(read(&path), RangeRead::Changed, "another Last-Modified");
        server.replace(b"abcdefghijk", modified);
        assert_eq!(read(&path), RangeRead::Changed, "another size");
        server.replace(b"abcdefghij", modified);
        assert_eq!(
            read(&path),
            RangeRead::Read(Bytes::from_static(b"cde")),
            "a same-second rewrite of the same size is beyond any HTTP check"
        );
        assert_eq!(
            read(&ObjectPath::from("missing")),
            RangeRead::Changed,
            "404"
        );
    }

    /// A range read that no retry can change is an answer, not an error
    /// (#1074): a missing key, and a range past the object's end, which
    /// the store clamps into a short body.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_range_of_a_gone_or_shorter_object_is_refused() {
        let store = DataStore::new(Arc::new(object_store::memory::InMemory::new()));
        let path = ObjectPath::from("cog.tif");
        store
            .inner()
            .put(&path, PutPayload::from_static(b"0123456789"))
            .await
            .unwrap();
        let read = |path: &ObjectPath, range| store.get_range_or_refusal(path, range).unwrap();
        assert_eq!(
            read(&path, 2..5),
            RangeAnswer::Read(Bytes::from_static(b"234"))
        );
        assert_eq!(
            read(&path, 8..12),
            RangeAnswer::Refused(RangeRefusal::Truncated)
        );
        assert_eq!(
            read(&ObjectPath::from("gone.tif"), 2..5),
            RangeAnswer::Refused(RangeRefusal::NotFound)
        );
        assert_eq!(store.bytes_read(), 3, "a refused range reads nothing");
    }

    /// Over HTTP, by status: 404 and 403 are refusals, and so is a 416 for
    /// a range starting past the end, and a 206 naming a total size short
    /// of the range. object_store keeps 403 and 416 only in its error text
    /// there, so this pins that text. Any other client error stays an
    /// error.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_refused_range_over_http_is_typed_by_status() {
        let server = RangeServer::new(b"0123456789", at("2026-10-17T08:00:00Z"));
        let (store, path) = build_http_store(&format!("{}/object", server.url)).unwrap();
        let read = |path: &ObjectPath, range| store.get_range_or_refusal(path, range);

        assert_eq!(
            read(&path, 2..5).unwrap(),
            RangeAnswer::Read(Bytes::from_static(b"234"))
        );
        assert_eq!(
            read(&ObjectPath::from("missing"), 2..5).unwrap(),
            RangeAnswer::Refused(RangeRefusal::NotFound)
        );
        assert_eq!(
            read(&path, 12..20).unwrap(),
            RangeAnswer::Refused(RangeRefusal::Truncated),
            "416"
        );
        assert_eq!(
            read(&path, 8..12).unwrap(),
            RangeAnswer::Refused(RangeRefusal::Truncated),
            "a shorter total size"
        );
        server.object.lock().unwrap().status = Some("403 Forbidden");
        assert_eq!(
            read(&path, 2..5).unwrap(),
            RangeAnswer::Refused(RangeRefusal::Forbidden)
        );
        server.object.lock().unwrap().status = Some("400 Bad Request");
        assert!(read(&path, 2..5).is_err(), "not a refusal");
        assert_eq!(store.bytes_read(), 3);
    }

    /// A request limit bounds the operations in flight, and a limit taken
    /// from a limited store stacks on it: a background share of 2 within a
    /// host's 4.
    #[tokio::test(flavor = "multi_thread")]
    async fn request_limits_bound_and_stack() {
        let meta = Arc::new(MetaStore {
            delay: std::time::Duration::from_millis(20),
            ..MetaStore::default()
        });
        let paths: Vec<ObjectPath> = (0..12).map(|i| ObjectPath::from(format!("k{i}"))).collect();
        for path in &paths {
            meta.set(path.as_ref(), 1, None);
        }
        let host = DataStore::new(meta.clone()).with_request_limit(4);
        let background = host.with_request_limit(2);
        let peak = |store: &DataStore| {
            meta.peak.store(0, Ordering::SeqCst);
            assert!(store
                .head_many(&paths, 16)
                .unwrap()
                .iter()
                .all(|head| matches!(head, Ok(Some(_)))));
            meta.peak.load(Ordering::SeqCst)
        };
        assert_eq!(peak(&host), 4);
        assert_eq!(peak(&background), 2);

        meta.peak.store(0, Ordering::SeqCst);
        let both = [host.clone(), background.clone()].map(|store| {
            let paths = paths.clone();
            tokio::task::spawn_blocking(move || store.head_many(&paths, 16).unwrap())
        });
        for task in both {
            task.await.unwrap();
        }
        assert_eq!(meta.peak.load(Ordering::SeqCst), 4, "the host limit holds");
    }

    /// Stores built for one host share its limit, whatever their paths and
    /// whichever collection each names; the first limit named for a host is
    /// kept.
    #[test]
    fn stores_for_one_host_share_its_limit() {
        let (a, path) = build_http_store_limited("http://127.0.0.1:9/meps/a.nc", 8).unwrap();
        let (b, _) = build_http_store_limited("http://127.0.0.1:9/other/", 3).unwrap();
        let (c, _) = build_http_store_limited("http://127.0.0.2:9/meps/a.nc", 8).unwrap();
        assert_eq!(path.as_ref(), "meps/a.nc");
        assert!(a.same_store(&b));
        assert!(!a.same_store(&c));
        let (a, b) = (a.for_collection("meps-sfc"), b.for_collection("meps-pl"));
        assert!(a.same_store(&b), "a collection keeps the host's store");
        assert!(!Arc::ptr_eq(a.inner(), b.inner()), "each names its own");
        let limited = HOST_STORES.lock().unwrap();
        assert_eq!(limited["http://127.0.0.1:9/"].limit, 8);
        drop(limited);
        let idle = requests_to(|host| host == "127.0.0.1:9")
            .into_iter()
            .next()
            .unwrap();
        assert_eq!((idle.limit, idle.in_flight, idle.queued), (8, 0, 0));
    }

    /// Collections naming themselves on one store keep sharing its limits,
    /// the host's and a background share stacked on it (#1233).
    #[tokio::test(flavor = "multi_thread")]
    async fn collections_on_one_store_share_its_limits() {
        let meta = Arc::new(MetaStore {
            delay: std::time::Duration::from_millis(20),
            ..MetaStore::default()
        });
        let paths: Vec<ObjectPath> = (0..12).map(|i| ObjectPath::from(format!("k{i}"))).collect();
        for path in &paths {
            meta.set(path.as_ref(), 1, None);
        }
        let host = DataStore::remote(meta.clone(), "meta.test").with_request_limit(3);
        let background = host.with_request_limit(1);
        let peak = |stores: [DataStore; 2]| {
            meta.peak.store(0, Ordering::SeqCst);
            let paths = paths.clone();
            async move {
                let tasks = stores.map(|store| {
                    let paths = paths.clone();
                    tokio::task::spawn_blocking(move || store.head_many(&paths, 16).unwrap())
                });
                for task in tasks {
                    task.await.unwrap();
                }
            }
        };
        peak([
            host.clone().for_collection("a"),
            host.clone().for_collection("b"),
        ])
        .await;
        assert_eq!(meta.peak.load(Ordering::SeqCst), 3, "the host limit holds");
        peak([
            background.clone().for_collection("a"),
            background.clone().for_collection("b"),
        ])
        .await;
        assert_eq!(meta.peak.load(Ordering::SeqCst), 1, "the share holds");
    }

    /// The lines `read` logs, as the server's `LOG_FORMAT=json` writes them.
    fn logged(read: impl FnOnce()) -> Vec<serde_json::Value> {
        let log = JsonLog::default();
        tracing::subscriber::with_default(log.subscriber(), read);
        log.lines()
    }

    /// object_store's retry lines among `lines`.
    fn retries(lines: &[serde_json::Value]) -> Vec<&serde_json::Value> {
        lines
            .iter()
            .filter(|line| line["target"] == "object_store::client::retry")
            .collect()
    }

    /// The `storage` spans `line` is in, outermost first.
    fn storage_spans(line: &serde_json::Value) -> Vec<&serde_json::Value> {
        line["spans"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|span| span["name"] == "storage")
            .collect()
    }

    /// object_store's line for a retried 429 runs inside the `storage` span
    /// (#1233), which names the collection, the server's host and port and
    /// the object key, never the URL's userinfo or query string.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_retry_line_names_the_collection_source_and_key() {
        let server = RangeServer::new(b"0123456789", at("2026-10-17T08:00:00Z"));
        server
            .object
            .lock()
            .unwrap()
            .fail_next
            .push_back("429 Too Many Requests");
        let address = server.url.trim_start_matches("http://").to_string();
        let url = format!("http://reader:secret@{address}/object?token=abc");
        let (store, path) = build_http_store(&url).unwrap();
        let store = store.for_collection("meps");

        let lines = logged(|| assert_eq!(store.get_range(&path, 2..5).unwrap(), &b"234"[..]));
        let request = server.object.lock().unwrap().last_request.clone();
        assert!(request.starts_with("get /object?token=abc "), "{request}");
        let retries = retries(&lines);
        assert_eq!(retries.len(), 1, "{lines:#?}");
        let retry = retries[0];
        assert!(
            retry["message"].as_str().unwrap().contains("429"),
            "{retry:#}"
        );
        assert_eq!(
            retry["span"],
            serde_json::json!({
                "name": "storage",
                "collection": "meps",
                "store": address,
                "path": "object",
            }),
            "{retry:#}"
        );
        assert_eq!(storage_spans(retry).len(), 1, "{retry:#}");
    }

    /// Two collections on one limited host, one on a background share of
    /// it: each retry line is in one `storage` span naming its own
    /// collection, never in a nested one without it (#1233).
    #[tokio::test(flavor = "multi_thread")]
    async fn retry_lines_of_collections_on_one_host_name_each_once() {
        let server = RangeServer::new(b"0123456789", at("2026-10-17T08:00:00Z"));
        let url = format!("{}/object", server.url);
        let (sfc, path) = build_http_store_limited(&url, 4).unwrap();
        let (pl, _) = build_http_store_limited(&url, 4).unwrap();
        let sfc = sfc.for_collection("meps-sfc");
        let pl = pl.with_request_limit(2).for_collection("meps-pl");
        for (store, collection) in [(&sfc, "meps-sfc"), (&pl, "meps-pl")] {
            server
                .object
                .lock()
                .unwrap()
                .fail_next
                .push_back("503 Service Unavailable");
            let lines = logged(|| assert_eq!(store.head(&path).unwrap().size, 10));
            let retries = retries(&lines);
            assert_eq!(retries.len(), 1, "{lines:#?}");
            let spans = storage_spans(retries[0]);
            assert_eq!(spans.len(), 1, "{:#}", retries[0]);
            assert_eq!(spans[0]["collection"], collection);
            assert_eq!(retries[0]["span"]["collection"], collection);
        }
    }

    /// A remote store's `store` label is the server's or the bucket
    /// endpoint's host and port, and the bucket: never credentials, a path
    /// or a query string. A local store has none.
    #[test]
    fn store_labels_name_the_host_and_bucket_only() {
        let label = |store: &DataStore| {
            let remote = store.remote.as_ref();
            remote.map(|remote| remote.labels.store.to_string())
        };
        let (virtual_hosted, _) =
            build_store("https://noaa-goes19.s3.us-east-1.amazonaws.com/ABI-L2-CMIPF/").unwrap();
        assert_eq!(
            label(&virtual_hosted).as_deref(),
            Some("s3.us-east-1.amazonaws.com/noaa-goes19")
        );
        let (path_style, _) = build_store("https://s3.waw3-1.cloudferro.com/radar/odim/").unwrap();
        assert_eq!(
            label(&path_style).as_deref(),
            Some("s3.waw3-1.cloudferro.com/radar")
        );
        let parts = build_s3_store_from_parts("http://minio.example:9000", "goes").unwrap();
        assert_eq!(label(&parts).as_deref(), Some("minio.example:9000/goes"));
        let (http, _) =
            build_http_store("https://user:pw@thredds.met.no/thredds/x.nc?token=t").unwrap();
        assert_eq!(label(&http).as_deref(), Some("thredds.met.no"));
        let (limited, _) =
            build_http_store_limited("http://user:pw@127.0.0.3:8080/x.nc?sig=s", 2).unwrap();
        assert_eq!(label(&limited).as_deref(), Some("127.0.0.3:8080"));

        for (endpoint, region, expected) in [
            (None, None, "s3.us-east-1.amazonaws.com/b"),
            (None, Some("eu-north-1"), "s3.eu-north-1.amazonaws.com/b"),
            (Some("minio.example:9000"), None, "minio.example:9000/b"),
            (
                Some("https://key:secret@minio.example:9000/x?X-Amz-Signature=s"),
                None,
                "minio.example:9000/b",
            ),
            (
                Some("key:secret@minio.example/x?sig=s"),
                None,
                "minio.example/b",
            ),
        ] {
            assert_eq!(traced::s3_label(endpoint, region, "b"), expected);
        }

        let dir = tempfile::tempdir().unwrap();
        let (local, _) = build_store(dir.path().to_str().unwrap()).unwrap();
        assert_eq!(label(&local), None);
        let local = local.for_collection("radar");
        assert!(local.remote.is_none(), "no span layer");
    }
}
