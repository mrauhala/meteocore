//! A counting wrapper around a repository's object storage, which also holds
//! its compressed payload cache (#1353).
//!
//! Icechunk does its own I/O, so nothing else sees its requests. Every
//! storage operation the repository issues is counted here, and so are the
//! bytes object bodies yield: a ranged read Icechunk splits into parts is
//! one request per part, and an Icechunk retry is another request. Retries
//! inside the HTTP client happen below this trait and are not counted.
//!
//! The payload cache is this wrapper's, not Icechunk's: Icechunk's chunk
//! cache exposes no hits, misses or occupancy, so the repository opens with
//! it disabled and the byte ranges it reads from chunk objects are kept
//! here. Chunk objects are content-addressed (`chunks/<id>`) and never
//! rewritten, so a range keyed by path stays valid across snapshots, as
//! Icechunk's own key (chunk id, range) did. Icechunk still coalesces
//! concurrent reads of one range before they reach this cache.
//!
//! A range a query fetches ahead of its decode ([`PREFETCH`]) waits in
//! [`staging`], outside the payload cache, and touches the cache only as
//! the decode's read of it does: that read misses, takes the staged range,
//! and fills the cache as if it had just read it from storage. Each range a
//! query plans to fetch ahead is counted once, in one [`Prefetched`]
//! outcome.

use std::{
    fmt,
    io::Cursor,
    ops::Range,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, AtomicU8, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Instant,
};

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use ds_cache::{ByteBoundedCache, CacheMetrics};
use futures::{stream::BoxStream, Stream, StreamExt as _};
use icechunk::storage::{
    DeleteObjectsResult, GetModifiedResult, ListInfo, RepositoryCreation, Settings, Storage,
    StorageContext, StorageError, StorageErrorKind, StorageInfo, StorageResult, VersionInfo,
    VersionedUpdateResult,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncRead, AsyncReadExt as _, ReadBuf};

/// Object paths of chunk payloads, below the repository root.
const CHUNKS: &str = icechunk::format::CHUNKS_FILE_PATH;

tokio::task_local! {
    /// Set around each range a query fetches ahead of its decodes (#1353):
    /// the read stages it for its decode, not in the payload cache.
    pub(super) static PREFETCH: Arc<Ahead>;
}

mod staging;

/// What staging charges a range of `length` bytes.
#[cfg(test)]
pub(crate) fn staging_weight(length: u64) -> u64 {
    staging::weight(length)
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct PayloadKey {
    path: Arc<str>,
    start: u64,
    end: u64,
}

fn weigh_payload(key: &PayloadKey, bytes: &Bytes) -> u64 {
    (bytes.len() + key.path.len()) as u64 + 80
}

/// One repository's counters and payload cache, shared by its snapshots.
pub(crate) struct Meter {
    requests: AtomicU64,
    read_bytes: AtomicU64,
    payload: ByteBoundedCache<PayloadKey, Bytes>,
    /// Ranges fetched ahead, until their decodes take them.
    staging: staging::Staging,
    /// Ranges planned to be fetched ahead, by [`Prefetched`].
    prefetched: [AtomicU64; 6],
    /// Every request in order: `(path, range)`, for tests that assert which
    /// objects a read touched.
    #[cfg(test)]
    log: std::sync::Mutex<Vec<(String, Option<Range<u64>>)>>,
    /// When each request was issued, in the same order.
    #[cfg(test)]
    issued: std::sync::Mutex<Vec<std::time::Instant>>,
}

/// What became of one range a query planned to fetch ahead (#1353): each
/// planned range ends in exactly one.
#[derive(Clone, Copy)]
pub(crate) enum Prefetched {
    /// Fetched, staged, then taken by a decode.
    Used = 0,
    /// Fetched and staged, then dropped after waiting [`staging::HELD_FOR`]
    /// with no decode asking for it.
    Unused = 1,
    /// Nothing to fetch: the payload cache or staging already had it, or
    /// Icechunk answered from its manifest (an inline or absent chunk).
    Skipped = 2,
    /// Not fetched ahead: no room in staging, or less than half the read
    /// budget free. Its decode reads it.
    Serial = 3,
    /// The fetch failed; its decode reads it again.
    Failed = 4,
    /// The request's deadline ended the fetch first.
    Expired = 5,
}

/// One range a query fetches ahead (#1353): the staging room reserved for
/// it, what its read found and how its fetch ended. Dropping it, however
/// the fetch ended, a panic included, counts the range and gives back the
/// room it did not stage into, exactly once.
pub(crate) struct Ahead {
    meter: Arc<Meter>,
    weight: u64,
    /// [`Self::NOT_READ`], [`Self::STAGED`] or [`Self::ON_HAND`].
    found: AtomicU8,
    /// `None` until the fetch ends, else its outcome: `Skipped` for a fetch
    /// that ended without staging, `Serial` or `Failed`.
    ended: std::sync::Mutex<Option<Prefetched>>,
}

impl Ahead {
    const NOT_READ: u8 = 0;
    const STAGED: u8 = 1;
    const ON_HAND: u8 = 2;

    /// How the fetch ended, short of staging.
    pub(crate) fn end(&self, outcome: Prefetched) {
        *self.ended.lock().unwrap_or_else(|e| e.into_inner()) = Some(outcome);
    }

    fn found(&self, found: u8) {
        self.found.store(found, Ordering::Relaxed);
    }
}

/// A staged range is counted when a decode takes it, or it is dropped
/// unread; any other ends here.
impl Drop for Ahead {
    fn drop(&mut self) {
        if self.found.load(Ordering::Relaxed) == Ahead::STAGED {
            return;
        }
        self.meter.staging.release(self.weight);
        let ended = *self.ended.get_mut().unwrap_or_else(|e| e.into_inner());
        let unfinished = if std::thread::panicking() {
            Prefetched::Failed
        } else {
            Prefetched::Expired
        };
        self.meter.count(ended.unwrap_or(unfinished), 1);
    }
}

impl Meter {
    /// A meter whose payload cache keeps up to `payload_bytes`; zero keeps
    /// nothing.
    pub(crate) fn new(payload_bytes: u64) -> Self {
        Self {
            requests: AtomicU64::new(0),
            read_bytes: AtomicU64::new(0),
            payload: ByteBoundedCache::new(payload_bytes, ds_cache::MIB, weigh_payload),
            staging: Default::default(),
            prefetched: Default::default(),
            #[cfg(test)]
            log: Default::default(),
            #[cfg(test)]
            issued: Default::default(),
        }
    }

    /// The most bytes staged at once, and so the most one query fetches
    /// ahead: what one shard of the payload cache holds. Zero, so nothing
    /// fetched ahead, when the cache keeps nothing.
    pub(crate) fn prefetch_room(&self) -> u64 {
        self.payload.shard_capacity_bytes()
    }

    fn count(&self, outcome: Prefetched, ranges: u64) {
        self.prefetched[outcome as usize].fetch_add(ranges, Ordering::Relaxed);
    }

    /// Ranges planned to be fetched ahead, cumulative, in [`Prefetched`]
    /// order. A range still being fetched or staged is in none yet.
    pub(crate) fn prefetched(&self) -> [u64; 6] {
        self.prefetched
            .each_ref()
            .map(|count| count.load(Ordering::Relaxed))
    }

    /// Plan to fetch a range of `length` bytes ahead: reserve its room in
    /// staging, beside what other queries staged and reserved, until the
    /// returned [`Ahead`] is dropped. `None`, counted `Serial`, when it does
    /// not fit.
    pub(crate) fn plan_ahead(self: &Arc<Self>, length: u64) -> Option<Ahead> {
        let weight = staging::weight(length);
        if !self.staging.reserve(weight, self.prefetch_room()) {
            self.count(Prefetched::Serial, 1);
            return None;
        }
        Some(Ahead {
            meter: self.clone(),
            weight,
            found: AtomicU8::new(Ahead::NOT_READ),
            ended: Default::default(),
        })
    }

    /// Staging room reserved for ranges being fetched.
    #[cfg(test)]
    pub(crate) fn staging_reserved(&self) -> u64 {
        self.staging.reserved()
    }

    /// Bytes of ranges fetched ahead and not yet taken: held memory outside
    /// the payload cache, at most [`Self::prefetch_room`]. Drops, first,
    /// those that have waited out [`staging::HELD_FOR`], so an idle
    /// repository's staging empties at its scrapes.
    pub(crate) fn staged_bytes(&self) -> u64 {
        self.staged_bytes_at(Instant::now())
    }

    fn staged_bytes_at(&self, now: Instant) -> u64 {
        self.count(Prefetched::Unused, self.staging.expire(now));
        self.staging.weight()
    }

    /// A range a decode wants, taken from staging if fetched ahead.
    fn take_staged(&self, key: &PayloadKey) -> Option<Bytes> {
        let (bytes, dropped) = self.staging.take(key, Instant::now());
        self.count(Prefetched::Unused, dropped);
        if bytes.is_some() {
            self.count(Prefetched::Used, 1);
        }
        bytes
    }

    /// Storage requests issued and object-body bytes received, cumulative.
    pub(crate) fn counts(&self) -> (u64, u64) {
        (
            self.requests.load(Ordering::Relaxed),
            self.read_bytes.load(Ordering::Relaxed),
        )
    }

    pub(crate) fn payload_metrics(&self) -> CacheMetrics {
        self.payload.metrics()
    }

    #[cfg(test)]
    pub(crate) fn take_log(&self) -> Vec<(String, Option<Range<u64>>)> {
        self.issued.lock().unwrap().clear();
        std::mem::take(&mut self.log.lock().unwrap())
    }

    /// When each request since the last [`Self::take_log`] was issued.
    #[cfg(test)]
    pub(crate) fn issued(&self) -> Vec<std::time::Instant> {
        self.issued.lock().unwrap().clone()
    }

    fn request(&self, _path: &str, _range: Option<&Range<u64>>) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        #[cfg(test)]
        {
            let mut log = self.log.lock().unwrap();
            log.push((_path.to_owned(), _range.cloned()));
            self.issued.lock().unwrap().push(std::time::Instant::now());
        }
    }

    fn read(&self, bytes: usize) {
        self.read_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

impl Default for Meter {
    fn default() -> Self {
        Self::new(0)
    }
}

impl fmt::Debug for Meter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (requests, read_bytes) = self.counts();
        f.debug_struct("Meter")
            .field("requests", &requests)
            .field("read_bytes", &read_bytes)
            .finish_non_exhaustive()
    }
}

/// Icechunk serialises a repository's storage only to hand it to another
/// process; the meter stays with this one.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct MeteredStorage {
    backend: Arc<dyn Storage + Send + Sync>,
    #[serde(skip)]
    meter: Arc<Meter>,
}

impl MeteredStorage {
    pub(crate) fn new(backend: Arc<dyn Storage + Send + Sync>, meter: Arc<Meter>) -> Self {
        Self { backend, meter }
    }

    /// Read a chunk payload range whole from storage.
    async fn read_payload(
        &self,
        ctx: &StorageContext<'_>,
        path: &str,
        range: &Range<u64>,
    ) -> StorageResult<(Bytes, VersionInfo)> {
        let (mut reader, version) = self.get_object_concurrently(ctx, path, range).await?;
        let length = range.end.saturating_sub(range.start);
        let mut body = Vec::with_capacity(usize::try_from(length).unwrap_or(0));
        reader
            .read_to_end(&mut body)
            .await
            .map_err(|e| StorageError::capture(StorageErrorKind::IOError(e)))?;
        Ok((Bytes::from(body), version))
    }
}

impl fmt::Display for MeteredStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MeteredStorage(backend={})", self.backend)
    }
}

impl icechunk_storage::sealed::Sealed for MeteredStorage {}

#[async_trait]
#[typetag::serde]
impl Storage for MeteredStorage {
    fn storage_info(&self) -> StorageInfo {
        self.backend.storage_info()
    }

    async fn default_settings(&self) -> StorageResult<Settings> {
        self.backend.default_settings().await
    }

    async fn can_write(&self) -> StorageResult<bool> {
        self.backend.can_write().await
    }

    async fn can_create_repository(&self) -> StorageResult<RepositoryCreation> {
        self.backend.can_create_repository().await
    }

    async fn create_location_if_needed(&self) -> StorageResult<()> {
        self.backend.create_location_if_needed().await
    }

    /// A chunk payload range is served from the cache, or read and kept.
    /// Everything else takes the trait's own path, whose ranged parts come
    /// back through [`Self::get_object_range`] and are counted there.
    async fn get_object(
        &self,
        ctx: &StorageContext<'_>,
        path: &str,
        range: Option<&Range<u64>>,
    ) -> StorageResult<(Pin<Box<dyn AsyncBufRead + Send>>, VersionInfo)> {
        let payload = range.filter(|_| {
            path.strip_prefix(CHUNKS)
                .is_some_and(|id| id.starts_with('/'))
        });
        let Some(range) = payload else {
            return match range {
                Some(range) => self.get_object_concurrently(ctx, path, range).await,
                None => self.get_object_range_read(ctx, path, None).await,
            };
        };
        let key = PayloadKey {
            path: path.into(),
            start: range.start,
            end: range.end,
        };
        let meter = &self.meter;
        // Icechunk ignores a chunk read's version: chunks are immutable.
        let cached = |bytes: Bytes| -> (Pin<Box<dyn AsyncBufRead + Send>>, VersionInfo) {
            (Box::pin(Cursor::new(bytes)), VersionInfo::for_creation())
        };
        if let Ok(ahead) = PREFETCH.try_with(Arc::clone) {
            // Already at hand: the decode finds it, and counts it, itself.
            if let Some(bytes) = meter
                .payload
                .peek(&key)
                .or_else(|| meter.staging.peek(&key))
            {
                ahead.found(Ahead::ON_HAND);
                return Ok(cached(bytes));
            }
            let (bytes, version) = self.read_payload(ctx, path, range).await?;
            let staged = meter
                .staging
                .stage(key, bytes.clone(), ahead.weight, Instant::now());
            ahead.found(if staged {
                Ahead::STAGED
            } else {
                Ahead::ON_HAND
            });
            return Ok((Box::pin(Cursor::new(bytes)), version));
        }
        if let Some(bytes) = meter.payload.get(&key) {
            return Ok(cached(bytes));
        }
        // A miss either way; a range fetched ahead fills the cache as one
        // read now does.
        if let Some(bytes) = meter.take_staged(&key) {
            meter.payload.insert(key, bytes.clone());
            return Ok(cached(bytes));
        }
        let (bytes, version) = self.read_payload(ctx, path, range).await?;
        meter.payload.insert(key, bytes.clone());
        Ok((Box::pin(Cursor::new(bytes)), version))
    }

    async fn get_object_range(
        &self,
        ctx: &StorageContext<'_>,
        path: &str,
        range: Option<&Range<u64>>,
    ) -> StorageResult<(
        Pin<Box<dyn Stream<Item = Result<Bytes, StorageError>> + Send>>,
        VersionInfo,
    )> {
        self.meter.request(path, range);
        let (stream, version) = self.backend.get_object_range(ctx, path, range).await?;
        let meter = Arc::clone(&self.meter);
        let stream = stream.inspect(move |item| {
            if let Ok(bytes) = item {
                meter.read(bytes.len());
            }
        });
        Ok((Box::pin(stream), version))
    }

    async fn put_object(
        &self,
        ctx: &StorageContext<'_>,
        path: &str,
        bytes: Bytes,
        content_type: Option<&str>,
        metadata: Vec<(String, String)>,
        previous_version: Option<&VersionInfo>,
    ) -> StorageResult<VersionedUpdateResult> {
        self.meter.request(path, None);
        self.backend
            .put_object(ctx, path, bytes, content_type, metadata, previous_version)
            .await
    }

    async fn copy_object(
        &self,
        ctx: &StorageContext<'_>,
        from: &str,
        to: &str,
        content_type: Option<&str>,
        version: &VersionInfo,
    ) -> StorageResult<VersionedUpdateResult> {
        self.meter.request(from, None);
        self.backend
            .copy_object(ctx, from, to, content_type, version)
            .await
    }

    async fn list_objects<'a>(
        &'a self,
        ctx: &StorageContext<'_>,
        prefix: &str,
    ) -> StorageResult<BoxStream<'a, StorageResult<ListInfo<String>>>> {
        self.meter.request(prefix, None);
        self.backend.list_objects(ctx, prefix).await
    }

    async fn list_objects_with_id_prefixes<'a>(
        &'a self,
        ctx: &StorageContext<'_>,
        prefix: &str,
        id_prefixes: &[String],
    ) -> StorageResult<BoxStream<'a, StorageResult<ListInfo<String>>>> {
        self.meter.request(prefix, None);
        self.backend
            .list_objects_with_id_prefixes(ctx, prefix, id_prefixes)
            .await
    }

    fn lists_id_prefixes_natively(&self) -> bool {
        self.backend.lists_id_prefixes_natively()
    }

    async fn delete_batch(
        &self,
        ctx: &StorageContext<'_>,
        prefix: &str,
        batch: Vec<(String, u64)>,
    ) -> StorageResult<DeleteObjectsResult> {
        self.meter.request(prefix, None);
        self.backend.delete_batch(ctx, prefix, batch).await
    }

    async fn get_object_last_modified(
        &self,
        ctx: &StorageContext<'_>,
        path: &str,
    ) -> StorageResult<DateTime<Utc>> {
        self.meter.request(path, None);
        self.backend.get_object_last_modified(ctx, path).await
    }

    async fn get_object_conditional(
        &self,
        ctx: &StorageContext<'_>,
        path: &str,
        previous_version: Option<&VersionInfo>,
    ) -> StorageResult<GetModifiedResult> {
        self.meter.request(path, None);
        Ok(
            match self
                .backend
                .get_object_conditional(ctx, path, previous_version)
                .await?
            {
                GetModifiedResult::Modified { data, new_version } => GetModifiedResult::Modified {
                    data: Box::pin(Counted {
                        inner: data,
                        meter: Arc::clone(&self.meter),
                    }),
                    new_version,
                },
                unchanged => unchanged,
            },
        )
    }
}

/// Counts the bytes a conditional read's body yields.
struct Counted {
    inner: Pin<Box<dyn AsyncBufRead + Send>>,
    meter: Arc<Meter>,
}

impl AsyncRead for Counted {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let polled = self.inner.as_mut().poll_read(cx, buf);
        self.meter.read(buf.filled().len() - before);
        polled
    }
}

impl AsyncBufRead for Counted {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<&[u8]>> {
        self.get_mut().inner.as_mut().poll_fill_buf(cx)
    }

    fn consume(mut self: Pin<&mut Self>, amt: usize) {
        self.meter.read(amt);
        self.inner.as_mut().consume(amt);
    }
}

#[cfg(test)]
mod tests;
