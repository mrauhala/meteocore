//! Weighted LRU cache for encoded MVT tile bytes.
//!
//! Mirrors the shape of `ds_render::RenderedCache` but keyed on tile coordinates
//! instead of (bbox, width, height, style). Sized by bytes — eviction is driven
//! by total weight, not entry count.

use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use quick_cache::sync::Cache;
use quick_cache::Weighter;

use crate::encode::TmsKind;

/// Cache key for an encoded vector tile.
///
/// `tms` is the strongly-typed `TmsKind` rather than a string so callers
/// can't silently miss the cache by passing a different stringification
/// (e.g. `"WebMercator"` vs `"WebMercatorQuad"`); the compiler enforces a
/// 1:1 match between encode-time and lookup-time. `properties_hash` lets
/// two callers with different property allowlists share the cache safely:
/// a different allowlist hashes differently and lands in a distinct slot.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct VectorTileKey {
    pub collection: String,
    pub tms: TmsKind,
    pub z: u32,
    pub x: u64,
    pub y: u64,
    pub properties_hash: u64,
}

#[derive(Clone)]
struct TileWeighter;

impl Weighter<VectorTileKey, Bytes> for TileWeighter {
    fn weight(&self, key: &VectorTileKey, val: &Bytes) -> u64 {
        // 64-byte fixed overhead per entry + key string length + payload size.
        64u64 + key.collection.len() as u64 + val.len() as u64
    }
}

/// Thread-safe weighted LRU cache for MVT bytes.
pub struct VectorTileCache {
    cache: Cache<VectorTileKey, Bytes, TileWeighter>,
    capacity_bytes: u64,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl VectorTileCache {
    /// Build a cache with the given byte budget. A `capacity_mb` of 0 disables
    /// caching entirely (every `get` is a miss, every `insert` is a no-op).
    pub fn new(capacity_mb: u64) -> Self {
        let capacity_bytes = capacity_mb.saturating_mul(1024 * 1024);
        // quick_cache requires a non-zero estimated item count even when weighted;
        // pick a generous number — actual eviction is driven by the weight budget.
        let estimated_items = ((capacity_bytes / 8192).max(1)) as usize;
        let cache = Cache::with_weighter(estimated_items, capacity_bytes.max(1), TileWeighter);
        Self {
            cache,
            capacity_bytes,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    pub fn get(&self, key: &VectorTileKey) -> Option<Bytes> {
        if self.capacity_bytes == 0 {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        match self.cache.get(key) {
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

    pub fn insert(&self, key: VectorTileKey, val: Bytes) {
        if self.capacity_bytes == 0 {
            return;
        }
        self.cache.insert(key, val);
    }

    pub fn capacity_bytes(&self) -> u64 {
        self.capacity_bytes
    }

    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(z: u32, x: u64, y: u64) -> VectorTileKey {
        VectorTileKey {
            collection: "demo".into(),
            tms: TmsKind::WebMercatorQuad,
            z,
            x,
            y,
            properties_hash: 0,
        }
    }

    #[test]
    fn hit_then_miss_counters() {
        let cache = VectorTileCache::new(1);
        let k = key(0, 0, 0);
        assert!(cache.get(&k).is_none());
        cache.insert(k.clone(), Bytes::from_static(b"abc"));
        let got = cache.get(&k).unwrap();
        assert_eq!(&got[..], b"abc");
        assert_eq!(cache.hits(), 1);
        assert_eq!(cache.misses(), 1);
    }

    #[test]
    fn zero_capacity_disables_cache() {
        let cache = VectorTileCache::new(0);
        let k = key(1, 1, 1);
        cache.insert(k.clone(), Bytes::from_static(b"xyz"));
        assert!(cache.get(&k).is_none());
        assert_eq!(cache.hits(), 0);
        // The insert is silently dropped, and the single `get` is counted as a miss.
        assert_eq!(cache.misses(), 1);
    }

    #[test]
    fn distinct_property_hashes_dont_collide() {
        let cache = VectorTileCache::new(1);
        let mut k1 = key(2, 1, 1);
        let mut k2 = k1.clone();
        k1.properties_hash = 1;
        k2.properties_hash = 2;
        cache.insert(k1.clone(), Bytes::from_static(b"one"));
        cache.insert(k2.clone(), Bytes::from_static(b"two"));
        assert_eq!(&cache.get(&k1).unwrap()[..], b"one");
        assert_eq!(&cache.get(&k2).unwrap()[..], b"two");
    }
}
