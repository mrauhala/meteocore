//! Remote files read in place by HTTP range (#1067 decision 4): an
//! `hdf5_reader::Storage` over ds-storage's conditional range reads, with
//! the engine-wide caches beneath the reader.
//!
//! hdf5-reader parses a file's metadata in thousands of reads of a few
//! bytes to 4 KiB, then reads one whole chunk per field (about 1.5 MB in
//! MEPS). [`RemoteFile::read_range`] serves them two ways:
//!
//! - **Blocks:** a read of up to [`Tuning::bypass_over`] bytes comes from
//!   [`Tuning::block`]-sized blocks of the file in [`BLOCKS`]
//!   (`MC_NETCDF_BLOCK_CACHE_MB`), keyed by the file's identity
//!   ([`FileId`]: URL and version) and the block's index. A read's missing
//!   blocks are fetched in one GET, each block single-flight. Opening a
//!   file fetches its first [`Tuning::first_read`] bytes in one GET
//!   ([`RemoteFile::prefetch`]): MEPS keeps every variable's metadata in
//!   its first 1 MiB. A second open of the same identity, after a reload
//!   or when a closed file is opened again, finds them cached.
//! - **Chunks:** a larger read, a chunk, comes whole from [`CHUNKS`]
//!   (`MC_NETCDF_CHUNK_CACHE_MB`), keyed by identity, offset and length,
//!   single-flight: N concurrent renders of one cold field make one GET.
//!   A file read through OPeNDAP keeps each field's `.dods` reply there
//!   instead ([`dap_reply`], keyed by identity and constraint), the same
//!   way (#1264).
//!
//! Every GET is conditional on the version ingested
//! ([`ds_storage::DataStore::get_range_if_unmodified_on`]). A file found
//! replaced (#1067 decision 7) has its blocks, chunks and decoded fields
//! dropped, is reported to its collection's poll ([`Changes`]) to be
//! probed and read again, and fails the read: a request gets a 503.
//!
//! GETs run on the I/O runtime ([`crate::io`]) through the host's request
//! limit ([`Host`]), under the calling thread's [`Mode`].

use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use ds_cache::{ByteBoundedCache, CacheEntry};
use ds_core::deadline;
use ds_core::error::DataServerError;
use ds_storage::bytes::Bytes;
use ds_storage::object_store::path::Path as ObjectPath;
use ds_storage::{DataStore, ObjectVersion, RangeRead};
use netcdf_reader::{Storage, StorageBuffer};

use crate::io::{self, Mode};

/// Requests in flight to one host at once, over every collection reading
/// it (#1067: thredds.met.no's fileServer is not a documented interface).
/// ds-storage halves it while the host answers 429 and grows it back when
/// the host is quiet (#1235).
pub(crate) const HOST_REQUEST_LIMIT: usize = 8;

/// Of [`HOST_REQUEST_LIMIT`], the requests ingest and prewarm may hold, so
/// a poll leaves the rest to requests.
pub(crate) const BACKGROUND_SHARE: usize = 2;

/// The sizes of a remote file's reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Tuning {
    /// Bytes of one cached metadata block.
    pub block: u64,
    /// Bytes fetched in one GET when a file is opened, a whole number of
    /// blocks.
    pub first_read: u64,
    /// Reads longer than this skip the blocks and are cached whole as
    /// chunks.
    pub bypass_over: u64,
}

impl Tuning {
    /// 64 KiB blocks, a 1 MiB first read (all of a MEPS file's metadata,
    /// measured), and chunks over 256 KiB (a MEPS field's is ~1.5 MB).
    pub(crate) const DEFAULT: Tuning = Tuning {
        block: 64 * 1024,
        first_read: 1024 * 1024,
        bypass_over: 256 * 1024,
    };
}

/// A host's stores, as one collection reads them: requests read through
/// the host's whole limit, ingest and prewarm through a share of it. The
/// limits, the share and the counters are one per host for the process,
/// shared by every collection on it; the stores name the collection on its
/// `storage` spans (#1233).
pub(crate) struct Host {
    pub requests: DataStore,
    pub background: DataStore,
    /// Range GETs made to the host, by every collection on it.
    pub gets: Arc<AtomicU64>,
}

impl Host {
    /// The host's stores, naming collection `id`.
    fn for_collection(&self, id: &str) -> Host {
        Host {
            requests: self.requests.clone().for_collection(id),
            background: self.background.clone().for_collection(id),
            gets: self.gets.clone(),
        }
    }
}

/// The host stores for `data_path`, read for collection `collection_id`,
/// and the key prefix under them.
pub(crate) fn host(
    collection_id: &str,
    data_path: &str,
) -> Result<(Arc<Host>, ObjectPath), DataServerError> {
    static HOSTS: LazyLock<Mutex<Vec<Host>>> = LazyLock::new(Default::default);
    let (requests, base) = ds_storage::build_http_store_limited(data_path, HOST_REQUEST_LIMIT)?;
    let mut hosts = HOSTS.lock().unwrap_or_else(|e| e.into_inner());
    // `build_http_store_limited` returns one store per host: the share
    // stacks on that one.
    let index = match hosts
        .iter()
        .position(|host| host.requests.same_store(&requests))
    {
        Some(index) => index,
        None => {
            hosts.push(Host {
                background: requests.with_request_limit(BACKGROUND_SHARE),
                requests,
                gets: Arc::default(),
            });
            hosts.len() - 1
        }
    };
    Ok((Arc::new(hosts[index].for_collection(collection_id)), base))
}

/// The scheme and authority of a URL, `https://thredds.met.no`.
pub(crate) fn origin(url: &str) -> &str {
    let after = url.find("://").map_or(0, |i| i + 3);
    url[after..]
        .find('/')
        .map_or(url, |slash| &url[..after + slash])
}

/// What a remote file is: its URL and the version ingested (through
/// OPeNDAP, the version its catalog lists). Every cache keys on it, so a
/// replaced file never reads the old one's bytes.
#[derive(Debug, PartialEq, Eq, Hash)]
pub(crate) struct FileId {
    pub url: String,
    pub version: ObjectVersion,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BlockKey {
    file: Arc<FileId>,
    index: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ChunkKey {
    file: Arc<FileId>,
    part: Part,
}

/// What of a file a [`CHUNKS`] entry holds.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Part {
    /// The `len` bytes at `offset`.
    Range { offset: u64, len: u64 },
    /// The OPeNDAP reply to this constraint.
    Dap(Arc<str>),
}

/// The block cache never goes below this: ingest opens up to eight files
/// at once, each walking its 1 MiB of metadata many times over. Without
/// the cache every metadata read would be a request (thousands per file).
const MIN_BLOCK_CACHE_MB: u64 = 8;

/// Metadata blocks, `MC_NETCDF_BLOCK_CACHE_MB` (default 64 MiB, at least
/// [`MIN_BLOCK_CACHE_MB`]), shared by every NetCDF collection.
static BLOCKS: LazyLock<ByteBoundedCache<BlockKey, Bytes>> = LazyLock::new(|| {
    let mb = ds_cache::env_mb("MC_NETCDF_BLOCK_CACHE_MB", 64).max(MIN_BLOCK_CACHE_MB);
    ByteBoundedCache::new(
        mb.saturating_mul(ds_cache::MIB),
        Tuning::DEFAULT.block,
        |key, bytes| bytes.len() as u64 + key.file.url.len() as u64 + 96,
    )
});

/// Compressed chunks, `MC_NETCDF_CHUNK_CACHE_MB` (default 512 MiB, about
/// 340 MEPS fields), shared by every NetCDF collection; and the OPeNDAP
/// replies of fields, uncompressed (about 126 MEPS fields), and of the
/// windows EDR queries read (#1171), each under its own constraint and
/// weighted by its size (428 bytes for a 2 × 2 MEPS window).
static CHUNKS: LazyLock<ByteBoundedCache<ChunkKey, Bytes>> = LazyLock::new(|| {
    ByteBoundedCache::from_env(
        "MC_NETCDF_CHUNK_CACHE_MB",
        512,
        3 * ds_cache::MIB / 2,
        |key, bytes| bytes.len() as u64 + key.file.url.len() as u64 + 96,
    )
});

/// The block cache's counters and occupancy.
pub fn block_cache_metrics() -> ds_cache::CacheMetrics {
    BLOCKS.metrics()
}

/// The chunk cache's counters and occupancy.
pub fn chunk_cache_metrics() -> ds_cache::CacheMetrics {
    CHUNKS.metrics()
}

/// Why a remote read failed. The reader sees it as an I/O error; the
/// engine finds it again in the error chain ([`ReadFailure::find`]).
#[derive(Debug, thiserror::Error)]
pub(crate) enum ReadFailure {
    /// The key no longer names the version ingested.
    #[error("the file was replaced after it was opened")]
    Replaced,
    #[error(transparent)]
    Failed(#[from] DataServerError),
}

impl ReadFailure {
    /// The remote read failure behind `error`, if one caused it.
    pub(crate) fn find<'a>(
        error: &'a (dyn std::error::Error + 'static),
    ) -> Option<&'a ReadFailure> {
        let mut next = Some(error);
        while let Some(error) = next {
            let failure = error
                .downcast_ref::<std::io::Error>()
                .and_then(std::io::Error::get_ref)
                .and_then(|inner| inner.downcast_ref::<ReadFailure>());
            if failure.is_some() {
                return failure;
            }
            next = error.source();
        }
        None
    }

    /// What a request is answered: a replaced file and a storage deadline
    /// or admission failure are 503s; any other failure is a storage error.
    pub(crate) fn to_error(&self) -> DataServerError {
        match self {
            ReadFailure::Replaced => DataServerError::ResourceExhausted,
            ReadFailure::Failed(DataServerError::DeadlineExceeded) => {
                DataServerError::DeadlineExceeded
            }
            ReadFailure::Failed(DataServerError::ResourceExhausted) => {
                DataServerError::ResourceExhausted
            }
            ReadFailure::Failed(e) => DataServerError::Storage(e.to_string()),
        }
    }
}

/// What a collection's remote reads report to its poll.
#[derive(Debug, Default)]
pub(crate) struct Changes {
    /// Files found replaced, by key and the version that was read.
    stale: Mutex<Vec<(String, ObjectVersion)>>,
    /// `content_version` moves made where a request found a file replaced.
    bumps: AtomicU64,
}

impl Changes {
    /// The files found replaced since the last call.
    pub(crate) fn take_stale(&self) -> Vec<(String, ObjectVersion)> {
        std::mem::take(&mut *self.stale.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub(crate) fn bumps(&self) -> u64 {
        self.bumps.load(Ordering::Relaxed)
    }
}

/// One remote file, opened at one version.
pub(crate) struct RemoteFile {
    host: Arc<Host>,
    key: ObjectPath,
    id: Arc<FileId>,
    tuning: Tuning,
    /// The ingest that opened it ([`crate::catalog::OpenFile::id`]): its
    /// decoded fields go when the file is found replaced.
    ingest: u64,
    changes: Arc<Changes>,
    /// Found replaced: every later read fails without a request.
    replaced: AtomicBool,
    /// A read failed (network, deadline, replaced): an open that failed
    /// then is tried again, not skipped as unreadable.
    failed: AtomicBool,
}

impl RemoteFile {
    /// `key` under `host`, at `version`, for the ingest `ingest`. `origin`
    /// is the source URL's scheme and authority ([`origin`]).
    pub(crate) fn new(
        host: Arc<Host>,
        origin: &str,
        key: &str,
        version: ObjectVersion,
        ingest: u64,
        tuning: Tuning,
        changes: Arc<Changes>,
    ) -> Self {
        Self {
            host,
            key: ObjectPath::from(key),
            id: Arc::new(FileId {
                url: format!("{origin}/{key}"),
                version,
            }),
            tuning,
            ingest,
            changes,
            replaced: AtomicBool::new(false),
            failed: AtomicBool::new(false),
        }
    }

    /// Whether a read of this file has failed.
    pub(crate) fn read_failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    /// Fetch the file's first [`Tuning::first_read`] bytes in one GET into
    /// the block cache, before the reader parses them; nothing when they
    /// are cached.
    pub(crate) fn prefetch(&self) -> Result<(), ReadFailure> {
        let end = self.tuning.first_read.min(self.len());
        if end > 0 {
            self.blocks(0, end)?;
        }
        Ok(())
    }

    /// The blocks covering `[offset, end)`, from the cache or fetched: the
    /// missing ones this caller claims in one GET, any another caller is
    /// fetching waited for.
    fn blocks(&self, offset: u64, end: u64) -> Result<Vec<Bytes>, ReadFailure> {
        let size = self.tuning.block;
        let (first, last) = (offset / size, (end - 1) / size);
        let key = |index| BlockKey {
            file: self.id.clone(),
            index,
        };
        let block_range = |index: u64| index * size..((index + 1) * size).min(self.len());
        let mut found: Vec<Option<Bytes>> = vec![None; (last - first + 1) as usize];
        let mut claimed = Vec::new();
        let mut busy = Vec::new();
        for index in first..=last {
            match BLOCKS.get_value_or_guard_untracked(&key(index), Duration::ZERO) {
                CacheEntry::Value(bytes) => {
                    BLOCKS.record_hit();
                    found[(index - first) as usize] = Some(bytes);
                }
                CacheEntry::Vacant(guard) => claimed.push((index, guard)),
                CacheEntry::Timeout => busy.push(index),
            }
        }
        if let (Some(&(low, _)), Some(&(high, _))) = (claimed.first(), claimed.last()) {
            let span = block_range(low).start..block_range(high).end;
            // An error drops the claims: a waiting reader fetches its own.
            let bytes = self.fetch(span.clone())?;
            for (index, guard) in claimed {
                let range = block_range(index);
                let at = (range.start - span.start) as usize;
                // A copy, so no block keeps the whole span allocated.
                let block =
                    Bytes::copy_from_slice(&bytes[at..at + (range.end - range.start) as usize]);
                BLOCKS.record_miss();
                let _ = guard.insert(block.clone());
                found[(index - first) as usize] = Some(block);
            }
        }
        for index in busy {
            let block = BLOCKS.get_or_insert_with_timeout(
                &key(index),
                wait(),
                || {
                    self.fetch(block_range(index))
                        .map(|bytes| Bytes::copy_from_slice(&bytes))
                },
                timed_out,
            )?;
            found[(index - first) as usize] = Some(block);
        }
        Ok(found
            .into_iter()
            .map(|block| block.expect("every block was found, fetched or waited for"))
            .collect())
    }

    /// The `len` bytes at `offset` as one cached chunk.
    fn chunk(&self, offset: u64, len: u64) -> Result<Bytes, ReadFailure> {
        let key = ChunkKey {
            file: self.id.clone(),
            part: Part::Range { offset, len },
        };
        CHUNKS.get_or_insert_with_timeout(
            &key,
            wait(),
            || self.fetch(offset..offset + len),
            timed_out,
        )
    }

    /// One conditional GET of `range`, through the host's stores under the
    /// calling thread's mode.
    fn fetch(&self, range: Range<u64>) -> Result<Bytes, ReadFailure> {
        if self.replaced.load(Ordering::Relaxed) {
            return Err(ReadFailure::Replaced);
        }
        let mode = io::mode();
        let store = match mode {
            Mode::Background => &self.host.background,
            Mode::Request | Mode::Detached => &self.host.requests,
        };
        let wanted = range.end - range.start;
        let range = range.start as usize..range.end as usize;
        self.host.gets.fetch_add(1, Ordering::Relaxed);
        let read = io::blocking(|| {
            store.get_range_if_unmodified_on(
                &self.key,
                range,
                &self.id.version,
                io::handle(),
                mode.budget(),
            )
        });
        match read {
            Ok(RangeRead::Read(bytes)) if bytes.len() as u64 == wanted => Ok(bytes),
            Ok(RangeRead::Read(bytes)) => {
                self.failed.store(true, Ordering::Relaxed);
                Err(ReadFailure::Failed(DataServerError::Storage(format!(
                    "{}: {} bytes read for a range of {wanted}",
                    self.id.url,
                    bytes.len()
                ))))
            }
            Ok(RangeRead::Changed) => {
                self.found_replaced(mode);
                Err(ReadFailure::Replaced)
            }
            Err(e) => {
                self.failed.store(true, Ordering::Relaxed);
                Err(ReadFailure::Failed(e))
            }
        }
    }

    /// Drop what was read from this version and report it, once.
    fn found_replaced(&self, mode: Mode) {
        self.failed.store(true, Ordering::Relaxed);
        if !self.replaced.swap(true, Ordering::Relaxed) {
            forget_replaced(
                &self.id,
                self.ingest,
                self.key.as_ref(),
                &self.changes,
                mode,
            );
        }
    }
}

/// Drop what was read from `file`, opened by the ingest `ingest` at `key`,
/// and report it to its collection's poll ([`Changes`]): it is no longer
/// the version opened. A request or detached fill found it in a file being
/// served: the content version moves now. Ingest and prewarm leave that to
/// the poll. Called once per file.
pub(crate) fn forget_replaced(
    file: &FileId,
    ingest: u64,
    key: &str,
    changes: &Changes,
    mode: Mode,
) {
    tracing::warn!(
        "{} is no longer the version opened ({} bytes, modified {:?}): dropping what was \
         read from it, to be read again",
        file.url,
        file.version.size,
        file.version.last_modified
    );
    BLOCKS.retain(|key, _| *key.file != *file);
    CHUNKS.retain(|key, _| *key.file != *file);
    crate::field::evict_file(ingest);
    changes
        .stale
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((key.to_string(), file.version.clone()));
    if mode != Mode::Background {
        changes.bumps.fetch_add(1, Ordering::Relaxed);
    }
}

/// The OPeNDAP reply of `file` to `constraint` (#1264), from the chunk
/// cache, or fetched once for every concurrent reader by `fetch`, as a
/// chunk is: a reply replaces the compressed chunk a range read keeps, so
/// a query past its deadline still leaves its fields fetched, and a
/// prewarm fetches what renders then find.
pub(crate) fn dap_reply(
    file: &Arc<FileId>,
    constraint: &str,
    fetch: impl FnOnce() -> Result<Bytes, ReadFailure>,
) -> Result<Bytes, ReadFailure> {
    let key = ChunkKey {
        file: file.clone(),
        part: Part::Dap(constraint.into()),
    };
    CHUNKS.get_or_insert_with_timeout(&key, wait(), fetch, timed_out)
}

/// Whether `file`'s OPeNDAP reply to `constraint` is in the chunk cache
/// now, without a recency bump or a count: an EDR query decodes a whole
/// field a render, a prewarm or another query left there rather than fetch
/// a window of it (#1171).
pub(crate) fn dap_cached(file: &Arc<FileId>, constraint: &str) -> bool {
    CHUNKS.contains_key(&ChunkKey {
        file: file.clone(),
        part: Part::Dap(constraint.into()),
    })
}

/// How long a reader waits for another's fetch of a block or chunk: until
/// its deadline, else its mode's budget.
fn wait() -> Duration {
    match deadline::current() {
        Some(end) => end.saturating_duration_since(Instant::now()),
        None => io::mode().budget().timeout(),
    }
}

fn timed_out() -> ReadFailure {
    ReadFailure::Failed(if deadline::current().is_some() {
        DataServerError::DeadlineExceeded
    } else {
        DataServerError::Storage("timed out waiting for another read of a NetCDF block".into())
    })
}

impl Storage for RemoteFile {
    fn len(&self) -> u64 {
        self.id.version.size
    }

    fn read_range(&self, offset: u64, len: usize) -> hdf5_reader::error::Result<StorageBuffer> {
        let end = offset
            .checked_add(len as u64)
            .filter(|&end| end <= self.len())
            .ok_or(hdf5_reader::error::Error::UnexpectedEof {
                offset,
                needed: len as u64,
                available: self.len().saturating_sub(offset),
            })?;
        if len == 0 {
            return Ok(StorageBuffer::from_vec(Vec::new()));
        }
        let read = if len as u64 > self.tuning.bypass_over {
            self.chunk(offset, len as u64).map(|chunk| chunk.to_vec())
        } else {
            self.blocks(offset, end).map(|blocks| {
                let size = self.tuning.block;
                let mut out = Vec::with_capacity(len);
                for (i, block) in blocks.iter().enumerate() {
                    let start = (offset / size + i as u64) * size;
                    let from = offset.max(start) - start;
                    let to = end.min(start + block.len() as u64) - start;
                    out.extend_from_slice(&block[from as usize..to as usize]);
                }
                out
            })
        };
        read.map(StorageBuffer::from_vec)
            .map_err(|failure| hdf5_reader::error::Error::Io(std::io::Error::other(failure)))
    }
}

#[cfg(test)]
mod tests;
