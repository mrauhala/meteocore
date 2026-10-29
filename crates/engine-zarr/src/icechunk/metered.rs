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

use std::{
    fmt,
    io::Cursor,
    ops::Range,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    task::{Context, Poll},
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
    /// Every request in order: `(path, range)`, for tests that assert which
    /// objects a read touched.
    #[cfg(test)]
    log: std::sync::Mutex<Vec<(String, Option<Range<u64>>)>>,
}

impl Meter {
    /// A meter whose payload cache keeps up to `payload_bytes`; zero keeps
    /// nothing.
    pub(crate) fn new(payload_bytes: u64) -> Self {
        Self {
            requests: AtomicU64::new(0),
            read_bytes: AtomicU64::new(0),
            payload: ByteBoundedCache::new(payload_bytes, ds_cache::MIB, weigh_payload),
            #[cfg(test)]
            log: Default::default(),
        }
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
        std::mem::take(&mut self.log.lock().unwrap())
    }

    fn request(&self, _path: &str, _range: Option<&Range<u64>>) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        #[cfg(test)]
        self.log
            .lock()
            .unwrap()
            .push((_path.to_owned(), _range.cloned()));
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
        if let Some(bytes) = self.meter.payload.get(&key) {
            // Icechunk ignores a chunk read's version: chunks are immutable.
            return Ok((Box::pin(Cursor::new(bytes)), VersionInfo::for_creation()));
        }
        let (mut reader, version) = self.get_object_concurrently(ctx, path, range).await?;
        let length = range.end.saturating_sub(range.start);
        let mut body = Vec::with_capacity(usize::try_from(length).unwrap_or(0));
        reader
            .read_to_end(&mut body)
            .await
            .map_err(|e| StorageError::capture(StorageErrorKind::IOError(e)))?;
        let bytes = Bytes::from(body);
        self.meter.payload.insert(key, bytes.clone());
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
