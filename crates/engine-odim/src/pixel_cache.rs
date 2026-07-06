//! Bounded LRU cache of decoded PVOL moment pixel arrays (#289).
//!
//! The polar-volume catalog holds only metadata (see
//! [`crate::pvol::PolarMoment`]); a moment's raw `RawPixels` array is read
//! lazily on first use and cached here, byte-weighted, so the full sweep
//! stack of a whole radar network is never resident at once. Mirrors the
//! GeoTIFF/GRIB `quick_cache` byte-weighted caches.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use quick_cache::sync::Cache;

use crate::reader::RawPixels;

/// Cache key: the file identity (local path string or S3 object key) plus
/// the HDF5 dataset path of the moment (`/datasetN/dataM/data`). File ids
/// are globally unique, so one cache can be shared across every collection.
type PixelKey = (Arc<str>, Arc<str>);

/// Byte-weights each entry by its decoded array size (plus key/Arc overhead).
#[derive(Clone)]
struct PixelWeighter;

impl quick_cache::Weighter<PixelKey, Arc<RawPixels>> for PixelWeighter {
    fn weight(&self, key: &PixelKey, val: &Arc<RawPixels>) -> u64 {
        // Decoded array bytes + the two key strings + Arc/control overhead.
        val.size_bytes() as u64 + key.0.len() as u64 + key.1.len() as u64 + 64
    }
}

/// Count cap on the negative (known-bad) cache. Bounds the memory a burst of
/// distinct failing keys can pin; entries age out by LRU so a transient
/// failure is retried once evicted.
const NEGATIVE_CAPACITY_ITEMS: usize = 4096;

/// Thread-safe, byte-bounded LRU of decoded moment arrays.
pub struct PixelCache {
    inner: Cache<PixelKey, Arc<RawPixels>, PixelWeighter>,
    /// Count-bounded LRU of keys whose pixel read/decode failed. A per-cell
    /// sampler loop (e.g. `volume_section`, thousands of cells) would otherwise
    /// re-fetch and re-count a failed moment on every cell, since a failure
    /// caches nothing — one transient S3 error inflated `pvol_pixel_read_
    /// failures_total` by thousands and hammered the store (PR #290 review).
    /// Recording the failure here makes the retry a single no-op lookup and
    /// the metric increment happen once.
    negative: Cache<PixelKey, ()>,
    capacity_bytes: u64,
    hits: AtomicU64,
    misses: AtomicU64,
    inserts: AtomicU64,
}

impl PixelCache {
    /// Build a cache capped at `capacity_mb` megabytes. A cache below ~1 MB
    /// (or 0) is treated as disabled — `get` always misses and `insert` is a
    /// no-op — so a misconfiguration can't silently hold one giant entry.
    pub fn new(capacity_mb: u64) -> Self {
        let capacity_bytes = capacity_mb.saturating_mul(1024 * 1024);
        // One FMI moment ≈ 360×500×2 ≈ 360 KB; estimate items at ~256 KB.
        let estimated_items = ((capacity_bytes / (256 * 1024)).max(16)) as usize;
        PixelCache {
            inner: Cache::with_weighter(estimated_items, capacity_bytes.max(1), PixelWeighter),
            negative: Cache::new(NEGATIVE_CAPACITY_ITEMS),
            capacity_bytes,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            inserts: AtomicU64::new(0),
        }
    }

    /// Look up a cached array, counting a hit for `/metrics`. A miss is **not**
    /// counted here: the loader records a genuine miss via [`Self::record_miss`]
    /// only after ruling out a negative-cache (known-bad) short-circuit, so a
    /// bad-key skip doesn't inflate `pvol_pixel_cache_misses_total` (PR #290
    /// review r4). Keeping `is_known_bad` *after* this on the loader's hot path
    /// means a good-key access stays a single lookup.
    pub fn get(&self, file_id: &str, dataset_path: &str) -> Option<Arc<RawPixels>> {
        if self.capacity_bytes == 0 {
            return None;
        }
        let key: PixelKey = (Arc::from(file_id), Arc::from(dataset_path));
        let hit = self.inner.get(&key);
        if hit.is_some() {
            self.hits.fetch_add(1, Ordering::Relaxed);
        }
        hit
    }

    /// Count one genuine positive-cache miss — i.e. a key that is neither
    /// cached nor known-bad, so the loader is about to fetch + decode it.
    pub fn record_miss(&self) {
        self.misses.fetch_add(1, Ordering::Relaxed);
    }

    /// Whether `(file_id, dataset_path)` is already resident — a presence
    /// check that, unlike [`Self::get`], does **not** count a hit or bump LRU
    /// recency. Used by the poll-time pre-warm to skip re-decoding a moment
    /// that is already cached (e.g. a beyond-`max_files` volume re-fetched on a
    /// later poll) without polluting the hit metric or keeping evicted volumes
    /// artificially warm. Always `false` when disabled (`capacity_mb == 0`).
    pub fn contains(&self, file_id: &str, dataset_path: &str) -> bool {
        if self.capacity_bytes == 0 {
            return false;
        }
        let key: PixelKey = (Arc::from(file_id), Arc::from(dataset_path));
        self.inner.contains_key(&key)
    }

    /// Insert a freshly-decoded array. No-op when disabled.
    pub fn insert(&self, file_id: &str, dataset_path: &str, pixels: Arc<RawPixels>) {
        if self.capacity_bytes == 0 {
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
        self.capacity_bytes
    }

    /// Cumulative `(hits, misses)`.
    pub fn stats(&self) -> (u64, u64) {
        (
            self.hits.load(Ordering::Relaxed),
            self.misses.load(Ordering::Relaxed),
        )
    }

    /// Cumulative inserts (request-time decodes + poll-time pre-warm).
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
        c.record_miss();
        assert_eq!(c.stats(), (h0, m0 + 1), "record_miss() counts the miss");
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
}
