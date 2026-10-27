//! In-memory tile cache for remote COG byte-range reads.
//!
//! Caches **compressed** tile bytes keyed by (file path, layout generation,
//! IFD, chunk index). The generation is the header read's
//! [`RemoteTileInfo::generation`](crate::reader::RemoteTileInfo::generation):
//! an object republished under its path is read again by the scan, and its
//! new layout never meets the tiles of the bytes it replaced (#1055).
//! Decompression is ~0.2ms vs 50-200ms for an S3 range read, so caching
//! compressed bytes gives 58x better memory efficiency than decoded tiles.
//! The WMS meta-tile loop still decodes each hot tile several times per
//! frame, so decoded tiles are also memoized in the process-global
//! [`crate::decoded_cache`] under this cache's [`TileCache::namespace`] (#468).

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use ds_cache::ByteBoundedCache;

/// Cache key: uniquely identifies a compressed tile across all files and IFD levels.
/// Uses Arc<str> instead of PathBuf to avoid allocation on every lookup.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct TileCacheKey {
    file_path: Arc<str>,
    /// The header read the tile's offset came from (#1055).
    generation: u64,
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

/// Source of [`TileCache::namespace`] ids; never reused within a process.
static NEXT_NAMESPACE: AtomicU64 = AtomicU64::new(1);

/// Share of each cache shard's budget targeted for `quick_cache`'s "hot"
/// ring (#1318, as #992 did for the PVOL pixel cache). The poll-time
/// pre-warm fills this cache ahead of reads. Under the library default,
/// `0.97`, the hot ring stays full of whatever was inserted first, and once
/// the cache is full every pre-warmed frame competes for the 3 % cold ring,
/// where it is evicted before its first view. At `0.5` half the budget is
/// cold, four frames at the pre-warm's per-frame cap of an eighth, and a
/// tile read while cold still moves to the hot ring.
///
/// The hot target also caps the largest tile the cache admits:
/// `0.5 × capacity / shards`. See [`APPROX_TILE_BYTES`].
const HOT_ALLOCATION: f64 = 0.5;

/// A typical compressed tile, the hash map sizing hint. It also sets the
/// shard floor: `quick_cache` halves its shards until each one's estimate
/// reaches 32 tiles, so a shard holds at least about 31 × 32 KiB, and a
/// tile up to about 496 KiB is admitted whatever the CPU count. At a
/// power-of-two `tile_cache_mb`, such as the default, the floor is 1 MiB
/// and the tile 512 KiB. Fewer cores mean fewer, larger shards: at the
/// default 256 MiB the largest tile is 2 MiB on 16 cores and 512 KiB from
/// 64. The largest tile of the radar fixtures is 165 KB of DEFLATE Float32;
/// an uncompressed 512 × 512 Float32 tile, 1 MiB, can miss on a many-core
/// host.
const APPROX_TILE_BYTES: u64 = 32 * 1024;

/// Thread-safe tile cache backed by the shared byte-bounded LRU (#480).
pub struct TileCache {
    inner: ByteBoundedCache<TileCacheKey, Bytes>,
    namespace: u64,
}

impl TileCache {
    /// Create a new tile cache with the given capacity in bytes.
    /// Pass 0 to create a no-op cache that never stores anything.
    pub fn new(max_bytes: u64) -> Self {
        TileCache {
            inner: ByteBoundedCache::new_with_hot_allocation(
                max_bytes,
                APPROX_TILE_BYTES,
                weigh_tile,
                HOT_ALLOCATION,
            ),
            namespace: NEXT_NAMESPACE.fetch_add(1, Ordering::Relaxed),
        }
    }

    /// Process-unique id of this cache. Keys here are an object path and a
    /// layout generation, unique within one engine's store; the
    /// process-global decoded-chunk cache adds this id so engines whose
    /// stores share an object path never share decoded pixels, and a rebuilt
    /// engine starts a fresh namespace.
    pub(crate) fn namespace(&self) -> u64 {
        self.namespace
    }

    /// Look up a cached compressed tile of the layout `generation` read.
    /// `ifd_index` distinguishes full-resolution (0) from overview tiles (1+).
    pub fn get(
        &self,
        file_path: &Path,
        generation: u64,
        chunk_index: u32,
        ifd_index: u16,
    ) -> Option<Bytes> {
        let key = TileCacheKey {
            file_path: Arc::from(file_path.to_string_lossy().as_ref()),
            generation,
            chunk_index,
            ifd_index,
        };
        self.inner.get(&key)
    }

    /// Speculative planning lookup: counters belong to the later worker read.
    pub(crate) fn contains_untracked(
        &self,
        file_path: &Path,
        generation: u64,
        chunk_index: u32,
        ifd_index: u16,
    ) -> bool {
        let key = TileCacheKey {
            file_path: Arc::from(file_path.to_string_lossy().as_ref()),
            generation,
            chunk_index,
            ifd_index,
        };
        self.inner.contains_key(&key)
    }

    /// Insert compressed tile bytes of the layout `generation` read.
    /// `ifd_index` distinguishes full-resolution (0) from overview tiles (1+).
    pub fn insert(
        &self,
        file_path: &Path,
        generation: u64,
        chunk_index: u32,
        ifd_index: u16,
        data: Bytes,
    ) {
        let key = TileCacheKey {
            file_path: Arc::from(file_path.to_string_lossy().as_ref()),
            generation,
            chunk_index,
            ifd_index,
        };
        self.inner.insert(key, data);
    }

    /// Drop every tile not of the object at `file_path`, every tile for
    /// `None`, and return about how many bytes that freed. Visits every
    /// entry: for the poll's pre-warm of an unread collection (#1318), never
    /// per request.
    pub(crate) fn keep_only(&self, file_path: Option<&Path>) -> u64 {
        let keep = file_path.map(|path| path.to_string_lossy().into_owned());
        let before = self.weight();
        self.inner
            .retain(|key, _| keep.as_deref() == Some(&*key.file_path));
        before.saturating_sub(self.weight())
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

        assert!(cache.get(path, 1, 0, 0).is_none());
        cache.insert(path, 1, 0, 0, data.clone());
        assert_eq!(cache.get(path, 1, 0, 0).unwrap(), data);

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

        cache.insert(path, 1, 0, 0, full_res.clone());
        cache.insert(path, 1, 0, 1, overview.clone());

        // Same chunk_index, different IFD — must return different data
        assert_eq!(cache.get(path, 1, 0, 0).unwrap(), full_res);
        assert_eq!(cache.get(path, 1, 0, 1).unwrap(), overview);
    }

    /// #1055: an object republished under its path is read again, and the
    /// tiles of its new layout never hit the bytes it replaced.
    #[test]
    fn cache_separates_layout_generations() {
        let cache = TileCache::new(1024 * 1024);
        let path = Path::new("test/file.tif");
        let old = Bytes::from(vec![1u8; 100]);
        cache.insert(path, 1, 0, 0, old.clone());

        assert!(cache.get(path, 2, 0, 0).is_none());
        assert!(!cache.contains_untracked(path, 2, 0, 0));
        let new = Bytes::from(vec![2u8; 100]);
        cache.insert(path, 2, 0, 0, new.clone());
        assert_eq!(cache.get(path, 2, 0, 0).unwrap(), new);
        assert_eq!(cache.get(path, 1, 0, 0).unwrap(), old);
    }

    #[test]
    fn cache_evicts_by_weight() {
        // 500-byte cache, insert entries that exceed it
        let cache = TileCache::new(500);
        let path = Path::new("test/file.tif");

        for i in 0..20 {
            cache.insert(path, 1, i, 0, Bytes::from(vec![0u8; 100]));
        }

        // Some earlier entries should have been evicted
        let mut found = 0;
        for i in 0..20 {
            if cache.get(path, 1, i, 0).is_some() {
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

    /// #1318, as #992 for the PVOL pixel cache: a freshly pre-warmed frame
    /// stays until it is read. The cache is first filled to twice its budget
    /// with tiles nobody reads (a full cache: the hot ring holds the oldest),
    /// then one frame is pre-warmed at the per-frame cap of an eighth of the
    /// budget, then the next poll's frame. Under the default 0.97 hot target
    /// the cold ring is 3 % of the budget and the first frame is evicted
    /// before its first view; at 0.5 both frames fit the cold ring.
    #[test]
    fn a_full_cache_keeps_a_freshly_prewarmed_frame_until_it_is_read() {
        let budget = 8 * 1024 * 1024;
        let cache = TileCache::new(budget);
        let tile = Bytes::from(vec![0u8; 4096]);
        let tiles = |bytes: u64| 0..(bytes / (tile.len() as u64 + 32)) as u32;
        let old = Path::new("old.tif");
        for chunk in tiles(2 * budget) {
            cache.insert(old, 1, chunk, 0, tile.clone());
        }
        let (fresh, next) = (Path::new("fresh.tif"), Path::new("next.tif"));
        for chunk in tiles(budget / 8) {
            cache.insert(fresh, 1, chunk, 0, tile.clone());
        }
        for chunk in tiles(budget / 8) {
            cache.insert(next, 1, chunk, 0, tile.clone());
        }
        let evicted = tiles(budget / 8)
            .filter(|&chunk| !cache.contains_untracked(fresh, 1, chunk, 0))
            .count();
        assert_eq!(
            evicted, 0,
            "pre-warmed tiles evicted before their first view"
        );
    }

    /// #1318: the hot target caps the largest admitted tile. At a
    /// power-of-two `tile_cache_mb` a shard holds at least 1 MiB, so a tile
    /// just under 512 KiB is admitted whatever the CPU count. At 1 and 4 MiB
    /// every host has its shards at that floor (one and four of 1 MiB), so
    /// a tile one byte heavier is refused there.
    #[test]
    fn a_tile_under_half_a_mebibyte_is_admitted_at_every_size() {
        const MIB: u64 = 1024 * 1024;
        let largest = Bytes::from(vec![0u8; 512 * 1024 - 32]);
        let path = Path::new("large.tif");
        for mb in [1, 4, 16, 64, 256] {
            let cache = TileCache::new(mb * MIB);
            cache.insert(path, 1, 0, 0, largest.clone());
            assert!(
                cache.contains_untracked(path, 1, 0, 0),
                "a {} byte tile is cached at {mb} MiB",
                largest.len()
            );
        }
        let heavier = Bytes::from(vec![0u8; largest.len() + 1]);
        for mb in [1, 4] {
            let cache = TileCache::new(mb * MIB);
            cache.insert(path, 1, 0, 0, heavier.clone());
            assert!(!cache.contains_untracked(path, 1, 0, 0), "{mb} MiB");
        }
    }

    /// #1318: an unread collection's cache keeps one object's tiles, every
    /// level and layout generation of it, and `None` keeps nothing.
    #[test]
    fn keep_only_drops_every_other_object() {
        let cache = TileCache::new(1024 * 1024);
        let (newest, older) = (Path::new("d/newest.tif"), Path::new("d/older.tif"));
        let tile = Bytes::from(vec![1u8; 100]);
        for (path, generation, ifd) in [(newest, 1, 0), (newest, 2, 1), (older, 3, 0)] {
            cache.insert(path, generation, 0, ifd, tile.clone());
        }
        let freed = cache.keep_only(Some(newest));
        assert_eq!(freed, tile.len() as u64 + 32);
        assert!(cache.contains_untracked(newest, 1, 0, 0));
        assert!(cache.contains_untracked(newest, 2, 0, 1));
        assert!(!cache.contains_untracked(older, 3, 0, 0));
        cache.keep_only(None);
        assert_eq!((cache.len(), cache.weight()), (0, 0));
    }

    #[test]
    fn zero_capacity_cache_stores_nothing() {
        let cache = TileCache::new(0);
        let path = Path::new("test/file.tif");
        cache.insert(path, 1, 0, 0, Bytes::from(vec![1u8; 100]));
        assert!(cache.get(path, 1, 0, 0).is_none());
    }
}
