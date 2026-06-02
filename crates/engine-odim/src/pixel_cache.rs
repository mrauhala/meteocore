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

/// Thread-safe, byte-bounded LRU of decoded moment arrays.
pub struct PixelCache {
    inner: Cache<PixelKey, Arc<RawPixels>, PixelWeighter>,
    capacity_bytes: u64,
    hits: AtomicU64,
    misses: AtomicU64,
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
            capacity_bytes,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Look up a cached array. Counts a hit/miss for `/metrics`.
    pub fn get(&self, file_id: &str, dataset_path: &str) -> Option<Arc<RawPixels>> {
        if self.capacity_bytes == 0 {
            return None;
        }
        let key: PixelKey = (Arc::from(file_id), Arc::from(dataset_path));
        match self.inner.get(&key) {
            Some(v) => {
                self.hits.fetch_add(1, Ordering::Relaxed);
                Some(v)
            }
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Insert a freshly-decoded array. No-op when disabled.
    pub fn insert(&self, file_id: &str, dataset_path: &str, pixels: Arc<RawPixels>) {
        if self.capacity_bytes == 0 {
            return;
        }
        let key: PixelKey = (Arc::from(file_id), Arc::from(dataset_path));
        self.inner.insert(key, pixels);
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
}
