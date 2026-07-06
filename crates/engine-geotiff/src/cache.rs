//! In-memory tile cache for remote COG byte-range reads.
//!
//! Caches **compressed** tile bytes keyed by (file path, chunk index).
//! Decompression is ~0.2ms vs 50-200ms for an S3 range read, so caching
//! compressed bytes gives 58x better memory efficiency than decoded tiles
//! with negligible CPU overhead on cache hits.

use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use ds_cache::ByteBoundedCache;

/// Cache key: uniquely identifies a compressed tile across all files and IFD levels.
/// Uses Arc<str> instead of PathBuf to avoid allocation on every lookup.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct TileCacheKey {
    file_path: Arc<str>,
    chunk_index: u32,
    /// IFD index (0 = full resolution, 1+ = overview levels).
    /// Prevents cache collisions between full-res and overview tiles.
    ifd_index: u16,
}

/// Weight function: count the compressed byte size of each entry
/// (plus ~32 bytes of `Bytes` overhead).
fn weigh_tile(_key: &TileCacheKey, val: &Bytes) -> u64 {
    val.len() as u64 + 32
}

/// Thread-safe tile cache backed by the shared byte-bounded LRU (#480).
pub struct TileCache {
    inner: ByteBoundedCache<TileCacheKey, Bytes>,
}

impl TileCache {
    /// Create a new tile cache with the given capacity in bytes.
    /// Pass 0 to create a no-op cache that never stores anything.
    pub fn new(max_bytes: u64) -> Self {
        // ~16 KB per compressed tile for initial hash map sizing.
        TileCache {
            inner: ByteBoundedCache::new(max_bytes, 16 * 1024, weigh_tile),
        }
    }

    /// Look up a cached compressed tile.
    /// `ifd_index` distinguishes full-resolution (0) from overview tiles (1+).
    pub fn get(&self, file_path: &Path, chunk_index: u32, ifd_index: u16) -> Option<Bytes> {
        let key = TileCacheKey {
            file_path: Arc::from(file_path.to_string_lossy().as_ref()),
            chunk_index,
            ifd_index,
        };
        self.inner.get(&key)
    }

    /// Insert compressed tile bytes into the cache.
    /// `ifd_index` distinguishes full-resolution (0) from overview tiles (1+).
    pub fn insert(&self, file_path: &Path, chunk_index: u32, ifd_index: u16, data: Bytes) {
        let key = TileCacheKey {
            file_path: Arc::from(file_path.to_string_lossy().as_ref()),
            chunk_index,
            ifd_index,
        };
        self.inner.insert(key, data);
    }

    /// Return (hits, misses) counters for logging.
    pub fn stats(&self) -> (u64, u64) {
        self.inner.stats()
    }

    /// Current weight (bytes used) of the cache.
    pub fn weight(&self) -> u64 {
        self.inner.weight()
    }

    /// Maximum weight (bytes) the cache will hold.
    pub fn capacity(&self) -> u64 {
        self.inner.capacity_bytes()
    }

    /// Number of entries currently in the cache.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.inner.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_hit_and_miss() {
        let cache = TileCache::new(1024 * 1024); // 1 MB
        let path = Path::new("test/file.tif");
        let data = Bytes::from(vec![1u8; 100]);

        assert!(cache.get(path, 0, 0).is_none());
        cache.insert(path, 0, 0, data.clone());
        assert_eq!(cache.get(path, 0, 0).unwrap(), data);

        let (hits, misses) = cache.stats();
        assert_eq!(hits, 1);
        assert_eq!(misses, 1);
    }

    #[test]
    fn cache_separates_ifd_levels() {
        let cache = TileCache::new(1024 * 1024);
        let path = Path::new("test/file.tif");
        let full_res = Bytes::from(vec![1u8; 100]);
        let overview = Bytes::from(vec![2u8; 50]);

        cache.insert(path, 0, 0, full_res.clone());
        cache.insert(path, 0, 1, overview.clone());

        // Same chunk_index, different IFD — must return different data
        assert_eq!(cache.get(path, 0, 0).unwrap(), full_res);
        assert_eq!(cache.get(path, 0, 1).unwrap(), overview);
    }

    #[test]
    fn cache_evicts_by_weight() {
        // 500-byte cache, insert entries that exceed it
        let cache = TileCache::new(500);
        let path = Path::new("test/file.tif");

        for i in 0..20 {
            cache.insert(path, i, 0, Bytes::from(vec![0u8; 100]));
        }

        // Some earlier entries should have been evicted
        let mut found = 0;
        for i in 0..20 {
            if cache.get(path, i, 0).is_some() {
                found += 1;
            }
        }
        assert!(
            found < 20,
            "Expected some evictions, but all {} entries survived",
            found
        );
        assert!(found > 0, "Expected some entries to survive");
    }

    #[test]
    fn zero_capacity_cache_stores_nothing() {
        let cache = TileCache::new(0);
        let path = Path::new("test/file.tif");
        cache.insert(path, 0, 0, Bytes::from(vec![1u8; 100]));
        assert!(cache.get(path, 0, 0).is_none());
    }
}
