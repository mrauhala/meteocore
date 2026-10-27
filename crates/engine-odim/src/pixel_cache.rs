//! Bounded LRU cache of decoded PVOL moment pixel arrays (#289).
//!
//! The polar-volume catalog holds only metadata (see
//! [`crate::pvol::PolarMoment`]); a moment's raw `RawPixels` array is read
//! lazily on first use and cached here, byte-weighted, so the full sweep
//! stack of a whole radar network is never resident at once. Mirrors the
//! GeoTIFF/GRIB `quick_cache` byte-weighted caches.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError, Weak};
use std::time::Instant;

use ds_cache::ByteBoundedCache;
use ds_core::error::DataServerError;
use ds_storage::bytes::Bytes;
use ds_storage::PendingGet;
use quick_cache::sync::Cache;

use crate::reader::RawPixels;

/// Cache key: the file identity (local path string or S3 object key) plus
/// the HDF5 dataset path of the moment (`/datasetN/dataM/data`). File ids
/// are globally unique, so one cache can be shared across every collection.
pub(crate) type MomentPixels = HashMap<String, Arc<RawPixels>>;
pub(crate) type MomentRequest<'a> = (&'a str, usize, usize);

type PixelKey = (Arc<str>, Arc<str>);

/// Byte-weights each entry by its decoded array size (plus key/Arc overhead).
fn weigh_pixels(key: &PixelKey, val: &Arc<RawPixels>) -> u64 {
    // Decoded array bytes + the two key strings + Arc/control overhead.
    val.size_bytes() as u64 + key.0.len() as u64 + key.1.len() as u64 + 64
}

/// Share of each cache shard's budget targeted for `quick_cache`'s "hot"
/// ring (#992). The library default, `0.97`, suits caches filled by reads,
/// but this one is filled ahead of reads by the poll-time pre-warm: under the
/// default the hot ring stays full of whatever was inserted first after
/// start, and every pre-warmed sweep competes for the remaining 3 % cold
/// ring, where it was evicted within minutes. At `0.5` half the budget is
/// cold, so a pre-warmed sweep stays resident about 16× longer, and entries
/// read while cold still move to the hot ring and displace the stale ones.
///
/// Not lower, because the hot target also caps the largest entry the cache
/// admits: `0.5 × capacity / shards`. Shards are halved until each holds
/// about 31 × 256 KiB, so for any capacity of 8 MiB or more an entry up to
/// about 3.9 MiB is admitted whatever the CPU count, which covers a full
/// 720 × 1000 f32 sweep (2.75 MiB). At `0.3` that floor would be 2.3 MiB,
/// and such a sweep would miss on every request on a many-core host.
const PIXEL_CACHE_HOT_ALLOCATION: f64 = 0.5;

/// Count cap on the negative (known-bad) cache. Bounds the memory a burst of
/// distinct failing keys can pin; entries age out by LRU so a transient
/// failure is retried once evicted. Also the cap on remembered refusals.
const NEGATIVE_CAPACITY_ITEMS: usize = 4096;

/// Thread-safe, byte-bounded LRU of decoded moment arrays.
pub struct PixelCache {
    inner: ByteBoundedCache<PixelKey, Arc<RawPixels>>,
    /// Count-bounded LRU of keys whose pixel read/decode failed. A per-cell
    /// sampler loop (e.g. `sample_section`, thousands of cells) would otherwise
    /// re-fetch and re-count a failed moment on every cell, since a failure
    /// caches nothing — one transient S3 error inflated `pvol_pixel_read_
    /// failures_total` by thousands and hammered the store (PR #290 review).
    /// Recording the failure here makes the retry a single no-op lookup and
    /// the metric increment happen once.
    negative: Cache<PixelKey, ()>,
    inserts: AtomicU64,
    /// Only active file loads own locks; expired weak entries are pruned on
    /// the next miss. No raw file bytes or decoded arrays are retained here.
    loading: Mutex<HashMap<String, Weak<FileLoad>>>,
    /// Downloaded volume files a background sibling decode is still working
    /// through (#994), keyed like `loading`. A cold miss on such a file
    /// decodes from these bytes instead of downloading it again. Each entry
    /// lives only as long as its [`HeldBytes`] guard, and their number is
    /// capped by the caller of [`Self::hold_bytes`].
    held: Mutex<HashMap<String, Bytes>>,
    /// Remote volume file downloads (#1127), keyed like `loading`, so a
    /// request that needs a file another request is downloading joins that
    /// read instead of starting its own. Weak: a read lives only while some
    /// request holds it, which is until the file's bytes are decoded at
    /// most, and the last holder dropping it aborts it; the downloaded bytes
    /// are never kept here. Dead entries are pruned on the next
    /// registration.
    reads: Mutex<Reads>,
    /// Volume files the store refused for good (#1046): a 401/403, or a
    /// local file this process may not read. Keyed like `loading`, each
    /// until the instant it may be read again, so the requests meanwhile
    /// fail at once instead of reading it again. Count-bounded like
    /// `negative`.
    refused: Cache<Arc<str>, Instant>,
    /// Files whose read the request deadline cut off and a background task
    /// is finishing (#1046), keyed like `loading`: at most one per file,
    /// and their number capped by the caller of [`Self::claim_finish`].
    finishing: Mutex<HashSet<String>>,
}

/// A volume file download from [`PixelCache::join_or_start_read`].
pub(crate) struct VolumeRead {
    pub(crate) read: Arc<PendingGet>,
    /// Another request started the read. Its failure is then that request's,
    /// and the joiner reads the file again.
    pub(crate) joined: bool,
}

type Reads = HashMap<String, Weak<PendingGet>>;

/// The lock [`PixelCache::with_file_load`] holds over one file's cold load.
/// Its mutex guards only the flag, never the load: a load can download the
/// file through the runtime a waiter runs on, so a waiter must not hold that
/// runtime's core while it waits (#1200), and gives up at its deadline.
#[derive(Default)]
struct FileLoad {
    busy: Mutex<bool>,
    freed: Condvar,
}

impl FileLoad {
    /// Take the lock: at once when no load is in flight, else after waiting
    /// for it off this thread's runtime core ([`off_core`]). `false`, with
    /// the lock not taken, when `deadline` passes first.
    fn acquire(&self, deadline: Option<Instant>) -> bool {
        {
            let mut busy = self.busy.lock().unwrap_or_else(PoisonError::into_inner);
            if !*busy {
                *busy = true;
                return true;
            }
        }
        off_core(|| {
            let mut busy = self.busy.lock().unwrap_or_else(PoisonError::into_inner);
            while *busy {
                busy = match deadline {
                    None => self
                        .freed
                        .wait(busy)
                        .unwrap_or_else(PoisonError::into_inner),
                    Some(deadline) => {
                        let left = deadline.saturating_duration_since(Instant::now());
                        if left.is_zero() {
                            return false;
                        }
                        let waited = self.freed.wait_timeout(busy, left);
                        waited.unwrap_or_else(PoisonError::into_inner).0
                    }
                };
            }
            *busy = true;
            true
        })
    }
}

/// A taken [`FileLoad`], released when dropped, unwinding included.
struct FileLoadGuard<'a>(&'a FileLoad);

impl Drop for FileLoadGuard<'_> {
    fn drop(&mut self) {
        *self.0.busy.lock().unwrap_or_else(PoisonError::into_inner) = false;
        self.0.freed.notify_all();
    }
}

/// Run `wait`, a wait for work another thread does, without holding this
/// thread's runtime core: inside `block_in_place` on a multi-thread runtime,
/// so its other workers keep running tasks and driving I/O and timers, the
/// download the other thread waits on included (#1200); directly elsewhere.
fn off_core<T>(wait: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) if runtime.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(wait)
        }
        _ => wait(),
    }
}

/// The registered read of `file_id` (#1127), unless none is held or it is
/// known to have failed. Prunes the reads nobody holds any more.
fn join_read(reads: &mut Reads, file_id: &str) -> Option<VolumeRead> {
    reads.retain(|_, read| read.strong_count() != 0);
    let read = reads
        .get(file_id)
        .and_then(Weak::upgrade)
        .filter(|read| !read.failed())?;
    Some(VolumeRead { read, joined: true })
}

/// Begin a read of `file_id` with `start`, registered for later callers.
fn start_read(reads: &mut Reads, file_id: &str, start: impl FnOnce() -> PendingGet) -> VolumeRead {
    let read = Arc::new(start());
    reads.insert(file_id.to_owned(), Arc::downgrade(&read));
    VolumeRead {
        read,
        joined: false,
    }
}

/// A file's claim on finishing its cut-off read in the background (#1046),
/// from [`PixelCache::claim_finish`]; dropping it lets the file be claimed
/// again.
pub(crate) struct FinishClaim<'a> {
    cache: &'a PixelCache,
    file_id: String,
}

impl Drop for FinishClaim<'_> {
    fn drop(&mut self) {
        self.cache
            .finishing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.file_id);
    }
}

/// One file's bytes registered in [`PixelCache::held_bytes`] for the life of
/// a background decode: a cold miss's siblings (#994), or the moments of a
/// read the request deadline cut off (#1046). Dropping it unregisters them; the
/// decoder drops it only after its last insert, so a miss that finds no held
/// bytes is guaranteed to see every array the decode produced.
pub(crate) struct HeldBytes<'a> {
    cache: &'a PixelCache,
    file_id: String,
}

impl Drop for HeldBytes<'_> {
    fn drop(&mut self) {
        self.cache
            .held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.file_id);
    }
}

impl PixelCache {
    /// Build a cache capped at `capacity_mb` megabytes. A cache below ~1 MB
    /// (or 0) is treated as disabled — `get` always misses and `insert` is a
    /// no-op — so a misconfiguration can't silently hold one giant entry.
    pub fn new(capacity_mb: u64) -> Self {
        // One FMI moment ≈ 360×500×2 ≈ 360 KB; estimate items at ~256 KB.
        // The estimate also sets the shard floor that bounds the largest
        // admitted entry (see `PIXEL_CACHE_HOT_ALLOCATION`).
        // Filled ahead of reads by the pre-warm (#992), with a ghost ring
        // to tell whether a larger cache would hit more (#1308).
        PixelCache {
            inner: ByteBoundedCache::new_with_hot_allocation(
                capacity_mb.saturating_mul(ds_cache::MIB),
                256 * 1024,
                weigh_pixels,
                PIXEL_CACHE_HOT_ALLOCATION,
            )
            .with_ghosts()
            .prewarmed(),
            negative: Cache::new(NEGATIVE_CAPACITY_ITEMS),
            inserts: AtomicU64::new(0),
            loading: Mutex::new(HashMap::new()),
            held: Mutex::new(HashMap::new()),
            reads: Mutex::new(HashMap::new()),
            refused: Cache::new(NEGATIVE_CAPACITY_ITEMS),
            finishing: Mutex::new(HashSet::new()),
        }
    }

    /// Enter the decoded-pixel cache in the memory plan (#1306), sized from
    /// `source`.
    pub(crate) fn register(&self, source: ds_cache::plan::SizeSource) {
        self.inner.register(
            "pvol_pixel_cache",
            ds_cache::plan::Scope::Engine("odim-volume"),
            source,
        );
    }

    /// Remember that the store refused `file_id` for good (#1046), until
    /// `until`. `true` when it was not refused already, so the caller logs
    /// a refusal once per period rather than once per request.
    pub(crate) fn refuse(&self, file_id: &str, until: Instant) -> bool {
        let newly = !self.is_refused(file_id);
        self.refused.insert(Arc::from(file_id), until);
        newly
    }

    /// Whether a refusal of `file_id` is still remembered: a read of it
    /// then fails at once, without asking the store (#1046).
    pub(crate) fn is_refused(&self, file_id: &str) -> bool {
        self.refused
            .get(file_id)
            .is_some_and(|until| Instant::now() < until)
    }

    /// Claim the finishing of `file_id`'s cut-off read (#1046): `None`, and
    /// nothing claimed, when another read of the file is being finished
    /// (one per file) or `max` files are, which bounds the reads, and their
    /// bytes, kept alive after their requests returned.
    pub(crate) fn claim_finish(&self, file_id: &str, max: usize) -> Option<FinishClaim<'_>> {
        let mut finishing = self.finishing.lock().unwrap_or_else(|e| e.into_inner());
        if finishing.len() >= max || !finishing.insert(file_id.to_owned()) {
            return None;
        }
        Some(FinishClaim {
            cache: self,
            file_id: file_id.to_owned(),
        })
    }

    /// Join the download of `file_id` another request holds, or else begin
    /// one with `start` and register it for the requests that follow
    /// (#1127). A read known to have failed is not joined, so the next caller
    /// starts another.
    pub(crate) fn join_or_start_read(
        &self,
        file_id: &str,
        start: impl FnOnce() -> PendingGet,
    ) -> VolumeRead {
        let mut reads = self.reads.lock().unwrap_or_else(|e| e.into_inner());
        join_read(&mut reads, file_id).unwrap_or_else(|| start_read(&mut reads, file_id, start))
    }

    /// The bytes of `file_id` a background sibling decode still holds, if
    /// any (#994): a cold miss on that file decodes from them rather than
    /// fetching the file again.
    pub(crate) fn held_bytes(&self, file_id: &str) -> Option<Bytes> {
        self.held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(file_id)
            .cloned()
    }

    /// Register `bytes` as `file_id`'s held copy for a background decode
    /// (#994, #1046). `None`, and nothing registered, when that file already
    /// has a decode in flight (single flight per file) or `max_held` files are
    /// already held, which bounds the downloaded bytes kept alive after their
    /// requests returned.
    pub(crate) fn hold_bytes(
        &self,
        file_id: &str,
        bytes: Bytes,
        max_held: usize,
    ) -> Option<HeldBytes<'_>> {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        if held.len() >= max_held || held.contains_key(file_id) {
            return None;
        }
        held.insert(file_id.to_owned(), bytes);
        Some(HeldBytes {
            cache: self,
            file_id: file_id.to_owned(),
        })
    }

    /// Look up a cached array, counting a hit for `/metrics`. A miss is **not**
    /// counted here: the loader records a genuine miss via [`Self::record_miss`]
    /// only after ruling out a negative-cache (known-bad) short-circuit, so a
    /// bad-key skip doesn't inflate `pvol_pixel_cache_misses_total` (PR #290
    /// review r4). Keeping `is_known_bad` *after* this on the loader's hot path
    /// means a good-key access stays a single lookup.
    pub fn get(&self, file_id: &str, dataset_path: &str) -> Option<Arc<RawPixels>> {
        if self.inner.capacity_bytes() == 0 {
            return None;
        }
        let key: PixelKey = (Arc::from(file_id), Arc::from(dataset_path));
        let hit = self.inner.get_untracked(&key);
        if hit.is_some() {
            self.inner.record_hit();
        }
        hit
    }

    /// Resolve a volume's requested moments, fetching/decoding all missing
    /// datasets in one batch. Retain the returned Arcs for the whole render:
    /// LRU eviction (or a disabled cache) must not cause rereads mid-volume.
    /// Returns the number of newly recorded read/decode failures as well.
    /// A `load` error (a deadline or transient fetch failure, #993) is
    /// returned as is and marks nothing bad, so the next request retries,
    /// as is a deadline that passes while another request loads the file
    /// ([`Self::with_file_load`]).
    ///
    /// `load` receives the file's [held bytes](Self::held_bytes), if any,
    /// snapshotted **before** the cache recheck (#994): a background sibling
    /// decode drops its bytes only after its last insert, so with `None` the
    /// recheck has already seen every array it decoded, and with `Some` the
    /// loader decodes from them instead of downloading the file again.
    pub(crate) fn load_many(
        &self,
        file_id: &str,
        requests: &[MomentRequest<'_>],
        load: impl FnOnce(
            &[MomentRequest<'_>],
            Option<Bytes>,
        ) -> Result<Vec<(String, RawPixels)>, DataServerError>,
    ) -> Result<(MomentPixels, usize), DataServerError> {
        self.load_many_rechecked(file_id, requests, || {}, load)
    }

    /// [`Self::load_many`], running `rechecked` once the cache recheck has
    /// found what is missing and before anything is loaded: what a
    /// background decode finishing just then looks like to the batch. A
    /// test seam for the #994 ordering, a no-op otherwise.
    fn load_many_rechecked(
        &self,
        file_id: &str,
        requests: &[MomentRequest<'_>],
        rechecked: impl FnOnce(),
        load: impl FnOnce(
            &[MomentRequest<'_>],
            Option<Bytes>,
        ) -> Result<Vec<(String, RawPixels)>, DataServerError>,
    ) -> Result<(MomentPixels, usize), DataServerError> {
        self.with_file_load(file_id, || {
            // Before the recheck: see `load_many`.
            let held = self.held_bytes(file_id);
            let mut pixels = HashMap::new();
            let mut missing = Vec::new();
            for &(path, nrays, nbins) in requests {
                if pixels.contains_key(path) || missing.iter().any(|(p, _, _)| *p == path) {
                    continue;
                }
                if let Some(raw) = self.get(file_id, path) {
                    pixels.insert(path.to_string(), raw);
                } else if !self.is_known_bad(file_id, path) {
                    self.record_miss(file_id, path);
                    missing.push((path, nrays, nbins));
                }
            }
            rechecked();
            let mut failures = 0;
            if !missing.is_empty() {
                for (path, raw) in load(&missing, held)? {
                    let raw = Arc::new(raw);
                    self.insert(file_id, &path, raw.clone());
                    pixels.insert(path, raw);
                }
                for (path, _, _) in missing {
                    if !pixels.contains_key(path) && self.mark_bad(file_id, path) {
                        failures += 1;
                    }
                }
            }
            Ok((pixels, failures))
        })?
    }

    /// Count one genuine positive-cache miss — i.e. a key that is neither
    /// cached nor known-bad, so the loader is about to fetch + decode it —
    /// and look it up among recent evictions (#1308).
    pub fn record_miss(&self, file_id: &str, dataset_path: &str) {
        let key: PixelKey = (Arc::from(file_id), Arc::from(dataset_path));
        self.inner.record_miss_for(&key);
    }

    /// Serialize cold loads of sibling datasets in a source-qualified file.
    /// The caller rechecks the pixel/negative caches inside the lock.
    ///
    /// A caller that finds another's load of the file in flight waits for
    /// it without holding its runtime core (#1200): that load may be
    /// downloading the file through the same runtime, and callers parked on
    /// every worker would leave nothing to drive the download or the
    /// deadline timers. It waits until the request deadline in scope at
    /// most: `Err(DeadlineExceeded)`, without running `load`, once that
    /// passes first.
    pub fn with_file_load<T>(
        &self,
        file_id: &str,
        load: impl FnOnce() -> T,
    ) -> Result<T, DataServerError> {
        let lock = {
            let mut loading = self.loading.lock().unwrap_or_else(|e| e.into_inner());
            loading.retain(|_, lock| lock.strong_count() != 0);
            if let Some(lock) = loading.get(file_id).and_then(Weak::upgrade) {
                lock
            } else {
                let lock = Arc::new(FileLoad::default());
                loading.insert(file_id.to_owned(), Arc::downgrade(&lock));
                lock
            }
        };
        if !lock.acquire(ds_core::deadline::current()) {
            return Err(DataServerError::DeadlineExceeded);
        }
        let _guard = FileLoadGuard(&lock);
        Ok(load())
    }

    /// Whether `(file_id, dataset_path)` is already resident — a presence
    /// check that, unlike [`Self::get`], does **not** count a hit or bump LRU
    /// recency. Used by the poll-time pre-warm to skip re-decoding a moment
    /// that is already cached (e.g. a beyond-`max_files` volume re-fetched on a
    /// later poll) without polluting the hit metric or keeping evicted volumes
    /// artificially warm. Always `false` when disabled (`capacity_mb == 0`).
    pub fn contains(&self, file_id: &str, dataset_path: &str) -> bool {
        if self.inner.capacity_bytes() == 0 {
            return false;
        }
        let key: PixelKey = (Arc::from(file_id), Arc::from(dataset_path));
        self.inner.contains_key(&key)
    }

    /// Insert a freshly-decoded array. No-op when disabled.
    pub fn insert(&self, file_id: &str, dataset_path: &str, pixels: Arc<RawPixels>) {
        if self.inner.capacity_bytes() == 0 {
            return;
        }
        let key: PixelKey = (Arc::from(file_id), Arc::from(dataset_path));
        self.inserts.fetch_add(1, Ordering::Relaxed);
        self.inner.insert(key, pixels);
    }

    /// Whether `(file_id, dataset_path)` recently failed to read/decode and
    /// should be treated as nodata without re-fetching. The negative cache is
    /// independent of the byte capacity, so this stays effective even in the
    /// disabled (`capacity_mb == 0`) diagnostic mode — otherwise a failing
    /// moment would re-storm the store / re-flood logs there (PR #290 review).
    pub fn is_known_bad(&self, file_id: &str, dataset_path: &str) -> bool {
        let key: PixelKey = (Arc::from(file_id), Arc::from(dataset_path));
        self.negative.get(&key).is_some()
    }

    /// Record a failed read for `(file_id, dataset_path)`. Returns `true` when
    /// this call was the first to record the key — the caller increments the
    /// failure metric only then, so one logical failure counts once instead of
    /// once per cell. The check-and-insert is atomic via the cache's
    /// placeholder guard (`get_or_insert_with` runs the closure only for the
    /// winning caller), so concurrent failures of the same key still count once
    /// (no get/insert TOCTOU race — PR #290 review).
    pub fn mark_bad(&self, file_id: &str, dataset_path: &str) -> bool {
        let key: PixelKey = (Arc::from(file_id), Arc::from(dataset_path));
        let mut newly = false;
        let _: Result<(), std::convert::Infallible> =
            self.negative.get_or_insert_with(&key, || {
                newly = true;
                Ok(())
            });
        newly
    }

    /// Current resident weight (bytes) — for metrics.
    pub fn weight(&self) -> u64 {
        self.inner.weight()
    }

    /// Configured byte capacity.
    pub fn capacity(&self) -> u64 {
        self.inner.capacity_bytes()
    }

    /// Cumulative `(hits, misses)`.
    pub fn stats(&self) -> (u64, u64) {
        self.inner.stats()
    }

    /// Snapshot for `/metrics`: counters, churn (#1308) and occupancy.
    pub fn metrics(&self) -> ds_cache::CacheMetrics {
        self.inner.metrics()
    }

    /// Cumulative inserts (request-time decodes, their background sibling
    /// decodes, and poll-time pre-warm).
    /// With `entries()` this makes LRU eviction pressure observable:
    /// sustained inserts while `entries`/`weight` stay flat at capacity ⇒
    /// the working set exceeds the cache and pre-warmed pixels are being
    /// churned out (#476 — the 11 s burst-S3-redownload failure mode).
    pub fn inserts(&self) -> u64 {
        self.inserts.load(Ordering::Relaxed)
    }

    /// Number of resident entries — for metrics.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.inner.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_file_misses_recheck_after_one_load() {
        let cache = PixelCache::new(1);
        let loads = AtomicU64::new(0);
        let start = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    start.wait();
                    cache
                        .with_file_load("same-source/file", || {
                            if cache.is_known_bad("same-source/file", "moment") {
                                return;
                            }
                            loads.fetch_add(1, Ordering::Relaxed);
                            cache.mark_bad("same-source/file", "moment");
                        })
                        .unwrap();
                });
            }
        });
        assert_eq!(loads.load(Ordering::Relaxed), 1);
        cache.with_file_load("next-file", || {}).unwrap();
        assert!(cache.loading.lock().unwrap().len() <= 1);
    }

    /// #1200: a caller that finds the file's load in flight waits for it
    /// until its own deadline at most, and then fails without running its
    /// load. A free lock is taken at once, a passed deadline or not: the
    /// load inside it checks the deadline before any read.
    #[test]
    fn file_load_waiter_gives_up_at_its_deadline() {
        use std::time::Duration;
        let cache = PixelCache::new(1);
        let (release, released) = std::sync::mpsc::channel::<()>();
        let (held, holding) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let cache = &cache;
            scope.spawn(move || {
                cache
                    .with_file_load("file", || {
                        held.send(()).unwrap();
                        released.recv().unwrap();
                    })
                    .unwrap();
            });
            holding.recv().unwrap();
            let wait = Duration::from_millis(50);
            let started = Instant::now();
            let _deadline = ds_core::deadline::enter(Some(started + wait));
            let waited = cache.with_file_load("file", || panic!("the lock is held"));
            assert!(matches!(waited, Err(DataServerError::DeadlineExceeded)));
            assert!(started.elapsed() >= wait);
            release.send(()).unwrap();
        });
        let _passed = ds_core::deadline::enter(Some(Instant::now()));
        assert!(matches!(cache.with_file_load("file", || 7), Ok(7)));
    }

    /// #1127: a file's read is joined while some caller holds it and
    /// started anew once none does, and the registry keeps no dead reads.
    #[test]
    fn reads_are_joined_while_held_and_dead_ones_pruned() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let store =
            ds_storage::DataStore::new(Arc::new(ds_storage::object_store::memory::InMemory::new()));
        let starts = AtomicU64::new(0);
        let start = || {
            starts.fetch_add(1, Ordering::Relaxed);
            let object = ds_storage::object_store::path::Path::from("volume");
            store.start_get_or_refusal(&object, runtime.handle())
        };
        let cache = PixelCache::new(1);
        let first = cache.join_or_start_read("volume", start);
        let second = cache.join_or_start_read("volume", start);
        assert!(!first.joined && second.joined);
        assert!(Arc::ptr_eq(&first.read, &second.read));
        assert_eq!(starts.load(Ordering::Relaxed), 1, "one read for both");
        drop(first);
        let third = cache.join_or_start_read("volume", start);
        assert!(third.joined, "joined while any caller holds it");
        drop((second, third));
        let fourth = cache.join_or_start_read("volume", start);
        assert!(!fourth.joined, "a read nobody holds is gone");
        assert_eq!(starts.load(Ordering::Relaxed), 2);
        drop(fourth);
        drop(cache.join_or_start_read("other", start));
        assert_eq!(cache.reads.lock().unwrap().len(), 1, "dead reads pruned");
    }

    #[test]
    fn held_bytes_are_single_flight_bounded_and_released_on_drop() {
        let cache = PixelCache::new(64);
        let bytes = Bytes::from_static(b"volume");
        assert!(cache.held_bytes("a").is_none());
        let a = cache.hold_bytes("a", bytes.clone(), 2).expect("first hold");
        assert_eq!(cache.held_bytes("a").as_deref(), Some(&b"volume"[..]));
        assert!(
            cache.hold_bytes("a", bytes.clone(), 2).is_none(),
            "one background decode per file"
        );
        let b = cache
            .hold_bytes("b", bytes.clone(), 2)
            .expect("second file");
        assert!(
            cache.hold_bytes("c", bytes.clone(), 2).is_none(),
            "at most `max_held` files are held"
        );
        drop(a);
        assert!(cache.held_bytes("a").is_none());
        assert!(cache.hold_bytes("c", bytes, 2).is_some());
        drop(b);
    }

    #[test]
    fn negative_cache_dedups_failures() {
        let c = PixelCache::new(64);
        assert!(!c.is_known_bad("file-a", "/d"));
        // First failure for a key is "new" (caller counts it once); the key is
        // then known-bad and a repeat mark is a no-op.
        assert!(c.mark_bad("file-a", "/d"), "first mark is new");
        assert!(c.is_known_bad("file-a", "/d"));
        assert!(!c.mark_bad("file-a", "/d"), "repeat mark must not recount");
        // A different key (or dataset) is tracked independently.
        assert!(c.mark_bad("file-b", "/d"));
        assert!(c.mark_bad("file-a", "/other"));
    }

    #[test]
    fn negative_cache_stays_active_when_positive_disabled() {
        // Capacity 0 disables the *positive* pixel cache (every successful
        // sample re-reads), but the negative cache must stay active so a
        // failing moment isn't re-fetched / re-logged per cell in diagnostic
        // mode (PR #290 review r3).
        let c = PixelCache::new(0);
        assert!(c.mark_bad("x", "/d"), "first failure is new");
        assert!(
            !c.mark_bad("x", "/d"),
            "repeat deduped even with the positive cache off"
        );
        assert!(c.is_known_bad("x", "/d"));
    }

    #[test]
    fn miss_counted_only_by_record_miss_not_by_get() {
        // A `get` that misses must NOT bump the miss counter — a known-bad skip
        // would otherwise count as a phantom positive-cache miss (PR #290 r4).
        let c = PixelCache::new(64);
        let (h0, m0) = c.stats();
        assert!(c.get("absent", "/d").is_none());
        assert_eq!(c.stats(), (h0, m0), "get() miss must not be counted");
        c.record_miss("absent", "/d");
        assert_eq!(c.stats(), (h0, m0 + 1), "record_miss() counts the miss");
    }

    /// The loader's miss of an evicted moment is a ghost hit (#1308): the
    /// pixel cache keeps a ghost ring, and says it is pre-warmed.
    #[test]
    fn a_recorded_miss_of_an_evicted_moment_is_a_ghost_hit() {
        use ndarray::Array2;
        let c = PixelCache::new(4);
        let paths: Vec<String> = (0..32).map(|i| format!("/d{i}")).collect();
        for path in &paths {
            c.insert(
                "f",
                path,
                Arc::new(RawPixels::U8(Array2::zeros((512, 512)))),
            );
        }
        let evicted = paths.iter().find(|path| !c.contains("f", path)).unwrap();
        c.record_miss("f", evicted);
        let churn = c.metrics().churn;
        assert!(churn.evictions() > 0, "{churn:?}");
        assert_eq!(churn.ghost_hits.map(|hits| hits[2]), Some(1), "{churn:?}");
        assert!(churn.prewarmed);
    }

    #[test]
    fn contains_checks_presence_without_counting_a_hit() {
        use ndarray::Array2;
        let c = PixelCache::new(64);
        let (h0, m0) = c.stats();
        // Absent → false, and no hit/miss counted (mirrors `get`).
        assert!(!c.contains("f", "/d"));
        assert_eq!(c.stats(), (h0, m0), "contains on a miss counts nothing");
        // Present → true, but — unlike `get` — still no hit counted, so the
        // pre-warm's existence checks don't inflate the cache hit metric.
        c.insert("f", "/d", Arc::new(RawPixels::U8(Array2::zeros((2, 2)))));
        assert!(c.contains("f", "/d"));
        assert_eq!(c.stats(), (h0, m0), "contains must not bump hits");
        // Contrast: `get` on the same key *does* count a hit.
        assert!(c.get("f", "/d").is_some());
        assert_eq!(c.stats().0, h0 + 1, "get bumps hits where contains did not");
    }

    #[test]
    fn contains_false_when_positive_cache_disabled() {
        use ndarray::Array2;
        // capacity 0 disables the positive cache: insert is a no-op and
        // contains reports nothing resident (the pre-warm skips entirely).
        let c = PixelCache::new(0);
        c.insert("f", "/d", Arc::new(RawPixels::U8(Array2::zeros((2, 2)))));
        assert!(!c.contains("f", "/d"));
    }

    #[test]
    fn mark_bad_counts_once_under_repeated_calls() {
        // The winning caller gets "new"; every later call for the same key is
        // a dedup (no double-count of the failure metric).
        let c = PixelCache::new(64);
        assert!(c.mark_bad("k", "/d"));
        for _ in 0..100 {
            assert!(!c.mark_bad("k", "/d"));
        }
    }
    /// A full 720-ray × 1000-bin f32 sweep (2.75 MiB), the largest moment
    /// the pixel cache must hold.
    fn large_f32_sweep() -> Arc<RawPixels> {
        Arc::new(RawPixels::F32(ndarray::Array2::zeros((720, 1000))))
    }

    /// #992: the hot target caps the largest admitted entry, so a lower hot
    /// allocation must still admit a full f32 sweep, at the default size and
    /// down to 8 MiB. From 8 MiB up the item estimate keeps every shard at
    /// 8 MiB or more, so 64 and 256 MiB are the worst case of a many-core
    /// host on any machine.
    #[test]
    fn full_f32_sweep_is_admitted_at_every_sensible_size() {
        let sweep = large_f32_sweep();
        assert_eq!(sweep.size_bytes(), 720 * 1000 * 4);
        for mb in [
            8,
            16,
            64,
            256,
            crate::volume_engine::DEFAULT_PIXEL_CACHE_MB,
            4096,
        ] {
            let cache = PixelCache::new(mb);
            cache.insert("volume", "/dataset1/data1/data", sweep.clone());
            assert!(
                cache.contains("volume", "/dataset1/data1/data"),
                "a 720×1000 f32 sweep must be cached at {mb} MiB"
            );
        }
    }

    /// #992: pre-warmed sweeps must survive the inserts that follow them. The
    /// cache is first filled to twice its budget with unread 360×500 u8
    /// sweeps (the steady state in production: nothing promoted, the hot ring
    /// full of old entries), then one site-hour of pre-warm (12 sweeps) is
    /// inserted, then another 10 % of the budget. Under the default 0.97 hot
    /// target each 8 MiB shard has a cold ring of about one such sweep, so the
    /// later inserts evict the pre-warmed ones; at 0.5 the cold ring holds
    /// about 23 per shard against about 6 expected arrivals.
    #[test]
    fn prewarmed_sweeps_survive_later_inserts() {
        let mb = 64;
        let cache = PixelCache::new(mb);
        let sweep = Arc::new(RawPixels::U8(ndarray::Array2::zeros((360, 500))));
        let budget = mb * ds_cache::MIB;
        let per_entry = sweep.size_bytes() as u64;
        let insert_run = |prefix: &str, bytes: u64| {
            for i in 0..bytes / per_entry {
                cache.insert(
                    &format!("{prefix}-{i}"),
                    "/dataset1/data1/data",
                    sweep.clone(),
                );
            }
        };
        insert_run("old", 2 * budget);
        for i in 0..12 {
            cache.insert(
                &format!("prewarm-{i}"),
                "/dataset1/data1/data",
                sweep.clone(),
            );
        }
        insert_run("later", budget / 10);
        let resident = (0..12)
            .filter(|i| cache.contains(&format!("prewarm-{i}"), "/dataset1/data1/data"))
            .count();
        assert_eq!(resident, 12, "pre-warmed sweeps evicted by later inserts");
    }

    fn raw() -> RawPixels {
        RawPixels::U8(ndarray::Array2::zeros((2, 2)))
    }

    #[test]
    fn batch_reads_missing_sweeps_once_and_retains_arrays_without_cache() {
        let cache = PixelCache::new(0);
        let requests = [("/sweep1", 2, 2), ("/sweep2", 2, 2)];
        let mut reads = 0;
        let (pixels, failures) = cache
            .load_many("volume", &requests, |missing, _| {
                reads += 1;
                assert_eq!(missing.len(), 2);
                Ok::<_, DataServerError>(
                    missing
                        .iter()
                        .map(|(path, _, _)| (path.to_string(), raw()))
                        .collect(),
                )
            })
            .unwrap();
        assert_eq!(reads, 1);
        assert_eq!(pixels.len(), 2);
        assert_eq!(failures, 0);
        assert_eq!(cache.weight(), 0);
        assert_eq!(pixels["/sweep1"].size_bytes(), 4);
    }

    #[test]
    fn concurrent_volume_batches_share_cached_decodes() {
        let cache = Arc::new(PixelCache::new(1));
        let reads = Arc::new(AtomicU64::new(0));
        let start = Arc::new(std::sync::Barrier::new(8));
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let cache = cache.clone();
                let reads = reads.clone();
                let start = start.clone();
                scope.spawn(move || {
                    start.wait();
                    let (pixels, failures) = cache
                        .load_many("volume", &[("/a", 2, 2), ("/b", 2, 2)], |missing, _| {
                            reads.fetch_add(1, Ordering::Relaxed);
                            Ok::<_, DataServerError>(
                                missing
                                    .iter()
                                    .map(|(path, _, _)| (path.to_string(), raw()))
                                    .collect(),
                            )
                        })
                        .unwrap();
                    assert_eq!(pixels.len(), 2);
                    assert_eq!(failures, 0);
                });
            }
        });
        assert_eq!(reads.load(Ordering::Relaxed), 1);
        assert_eq!(cache.stats().1, 2);
    }

    #[test]
    fn partial_batch_failure_is_not_refetched() {
        let cache = PixelCache::new(1);
        let requests = [("/good", 2, 2), ("/bad", 2, 2)];
        let (pixels, failures) = cache
            .load_many("volume", &requests, |_, _| {
                Ok::<_, DataServerError>(vec![("/good".into(), raw())])
            })
            .unwrap();
        assert_eq!(pixels.len(), 1);
        assert_eq!(failures, 1);
        let (pixels, failures) = cache
            .load_many("volume", &requests, |_, _| -> Result<_, DataServerError> {
                panic!("cached or known bad")
            })
            .unwrap();
        assert_eq!(pixels.len(), 1);
        assert_eq!(failures, 0);
    }

    /// #994: the loader gets the file's held bytes, snapshotted before the
    /// recheck, and nothing once the guard is gone.
    #[test]
    fn batch_loader_receives_held_bytes() {
        let cache = PixelCache::new(1);
        let mut seen: Vec<Option<Bytes>> = Vec::new();
        let mut load = |file: &str| {
            cache
                .load_many(file, &[("/a", 2, 2)], |missing, held| {
                    seen.push(held);
                    Ok::<_, DataServerError>(
                        missing
                            .iter()
                            .map(|(path, _, _)| (path.to_string(), raw()))
                            .collect(),
                    )
                })
                .unwrap();
        };
        let guard = cache
            .hold_bytes("volume", Bytes::from_static(b"held"), 1)
            .unwrap();
        load("volume");
        drop(guard);
        load("other");
        assert_eq!(seen[0].as_deref(), Some(&b"held"[..]));
        assert!(seen[1].is_none());
    }

    /// #994 review, #1046: the batch snapshots the held bytes before its
    /// cache recheck. A background decode that finishes just after the
    /// recheck found `/a` missing (its last insert, then its bytes go) must
    /// still leave the loader its bytes: snapshotted after the recheck they
    /// would be gone, and the loader would download the file again.
    #[test]
    fn batch_snapshots_held_bytes_before_its_recheck() {
        let cache = PixelCache::new(1);
        let guard = cache
            .hold_bytes("volume", Bytes::from_static(b"held"), 1)
            .unwrap();
        let decode_finishes = || {
            cache.insert("volume", "/a", Arc::new(raw()));
            drop(guard);
        };
        let mut seen = None;
        cache
            .load_many_rechecked(
                "volume",
                &[("/a", 2, 2)],
                decode_finishes,
                |missing, held| {
                    seen = Some(held);
                    Ok::<_, DataServerError>(
                        missing
                            .iter()
                            .map(|(path, _, _)| (path.to_string(), raw()))
                            .collect(),
                    )
                },
            )
            .unwrap();
        assert!(cache.held_bytes("volume").is_none(), "the decode let go");
        assert_eq!(
            seen.expect("the recheck missed `/a`").as_deref(),
            Some(&b"held"[..]),
            "the snapshot precedes the recheck"
        );
    }

    /// #1046: a refusal is remembered until its instant, logged once per
    /// period, and kept per file.
    #[test]
    fn refusals_are_remembered_until_they_expire() {
        use std::time::Duration;
        let cache = PixelCache::new(1);
        assert!(!cache.is_refused("volume"));
        let later = Instant::now() + Duration::from_secs(60);
        assert!(cache.refuse("volume", later), "a new refusal");
        assert!(cache.is_refused("volume"));
        assert!(!cache.refuse("volume", later), "already refused");
        assert!(!cache.is_refused("other"));
        assert!(cache.refuse("expired", Instant::now()));
        assert!(!cache.is_refused("expired"), "read again once it expires");
        assert!(cache.refuse("expired", later), "and refused anew");
    }

    /// #1046: one finish per file, at most `max` files, released on drop.
    #[test]
    fn finish_claims_are_single_flight_bounded_and_released_on_drop() {
        let cache = PixelCache::new(1);
        let a = cache.claim_finish("a", 2).expect("first claim");
        assert!(cache.claim_finish("a", 2).is_none(), "one per file");
        let b = cache.claim_finish("b", 2).expect("second file");
        assert!(cache.claim_finish("c", 2).is_none(), "at most `max` files");
        drop(a);
        assert!(cache.claim_finish("a", 2).is_some());
        drop(b);
        assert!(cache.finishing.lock().unwrap().is_empty());
    }

    /// #993: a load that fails (a deadline or transient fetch error) is the
    /// caller's error and marks no moment bad, so the next request loads.
    #[test]
    fn failed_load_marks_nothing_bad_and_retries() {
        let cache = PixelCache::new(1);
        let requests = [("/a", 2, 2), ("/b", 2, 2)];
        let result = cache.load_many("volume", &requests, |_, _| {
            Err(DataServerError::DeadlineExceeded)
        });
        assert!(matches!(result, Err(DataServerError::DeadlineExceeded)));
        assert!(!cache.is_known_bad("volume", "/a"));
        assert!(!cache.is_known_bad("volume", "/b"));
        let (pixels, failures) = cache
            .load_many("volume", &requests, |missing, _| {
                assert_eq!(missing.len(), 2, "both moments are retried");
                Ok::<_, DataServerError>(
                    missing
                        .iter()
                        .map(|(path, _, _)| (path.to_string(), raw()))
                        .collect(),
                )
            })
            .unwrap();
        assert_eq!(pixels.len(), 2);
        assert_eq!(failures, 0);
    }
}
