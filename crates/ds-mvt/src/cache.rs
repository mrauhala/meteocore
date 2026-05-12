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
use crate::hash::{fnv1a_mix, FNV1A_OFFSET};

/// Cache key for an encoded vector tile.
///
/// `tms` is the strongly-typed `TmsKind` rather than a string so callers
/// can't silently miss the cache by passing a different stringification
/// (e.g. `"WebMercator"` vs `"WebMercatorQuad"`); the compiler enforces a
/// 1:1 match between encode-time and lookup-time. `properties_hash` lets
/// two callers with different property allowlists share the cache safely:
/// a different allowlist hashes differently and lands in a distinct slot.
/// `data_version` is an opaque token (file mtime, refresh counter, …)
/// supplied by the source engine: bumping it after a reload invalidates
/// previously-issued ETags so clients re-fetch instead of stalling on
/// `304 Not Modified`.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct VectorTileKey {
    pub collection: String,
    pub tms: TmsKind,
    pub z: u32,
    pub x: u64,
    pub y: u64,
    pub properties_hash: u64,
    pub data_version: u64,
}

/// Cached tile payload plus its content-derived ETag.
///
/// The ETag hashes the bytes themselves so two cache entries under the same
/// `VectorTileKey` with different content (e.g. data refresh, encoder change)
/// produce different ETags. A stable key-derived ETag would let stale browser
/// caches survive a server-side fix indefinitely, since `If-None-Match` would
/// keep returning 304 against fresh-but-empty content.
#[derive(Debug, Clone)]
pub struct CachedTile {
    pub bytes: Bytes,
    /// Private so the only path that sets it is `CachedTile::new`, which seals
    /// the invariant `etag == FNV-1a(bytes)`. Without this seal, a workspace
    /// crate could construct `CachedTile { bytes, etag: "wrong".into() }` and
    /// silently break `If-None-Match` for that entry (a browser holding the
    /// real ETag would get a full 200 response instead of 304, or vice
    /// versa).
    etag: String,
}

impl CachedTile {
    /// Build a cache entry from encoded bytes, deriving the ETag from the
    /// content via FNV-1a. Format matches `ds_render::CacheKey::etag()` so the
    /// same `etag_matches` helper works for both raster and vector responses.
    /// FNV-1a (not `DefaultHasher`) so the ETag is stable across rustc
    /// versions — a binary rebuild against unchanged content keeps the same
    /// ETag and browser caches survive the redeploy.
    pub fn new(bytes: Bytes) -> Self {
        let mut h = FNV1A_OFFSET;
        fnv1a_mix(&mut h, bytes.as_ref());
        let etag = format!("\"{h:016x}\"");
        Self { bytes, etag }
    }

    /// Quoted hex16 ETag string suitable for the `ETag` response header.
    pub fn etag(&self) -> &str {
        &self.etag
    }
}

#[derive(Clone)]
struct TileWeighter;

impl Weighter<VectorTileKey, CachedTile> for TileWeighter {
    fn weight(&self, key: &VectorTileKey, val: &CachedTile) -> u64 {
        // 64-byte fixed overhead per entry + key string length + payload size
        // + 18 bytes for the quoted hex64 ETag string.
        64u64 + key.collection.len() as u64 + val.bytes.len() as u64 + val.etag.len() as u64
    }
}

/// Thread-safe weighted LRU cache for MVT bytes.
pub struct VectorTileCache {
    cache: Cache<VectorTileKey, CachedTile, TileWeighter>,
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

    pub fn get(&self, key: &VectorTileKey) -> Option<CachedTile> {
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

    pub fn insert(&self, key: VectorTileKey, val: CachedTile) {
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
            data_version: 0,
        }
    }

    #[test]
    fn hit_then_miss_counters() {
        let cache = VectorTileCache::new(1);
        let k = key(0, 0, 0);
        assert!(cache.get(&k).is_none());
        cache.insert(k.clone(), CachedTile::new(Bytes::from_static(b"abc")));
        let got = cache.get(&k).unwrap();
        assert_eq!(&got.bytes[..], b"abc");
        assert_eq!(cache.hits(), 1);
        assert_eq!(cache.misses(), 1);
    }

    #[test]
    fn zero_capacity_disables_cache() {
        let cache = VectorTileCache::new(0);
        let k = key(1, 1, 1);
        cache.insert(k.clone(), CachedTile::new(Bytes::from_static(b"xyz")));
        assert!(cache.get(&k).is_none());
        assert_eq!(cache.hits(), 0);
        // The insert is silently dropped, and the single `get` is counted as a miss.
        assert_eq!(cache.misses(), 1);
    }

    #[test]
    fn etag_is_stable_for_same_bytes() {
        let a = CachedTile::new(Bytes::from_static(b"hello"));
        let b = CachedTile::new(Bytes::from_static(b"hello"));
        assert_eq!(a.etag, b.etag);
    }

    #[test]
    fn etag_differs_for_distinct_bytes() {
        let a = CachedTile::new(Bytes::from_static(b"hello"));
        let b = CachedTile::new(Bytes::from_static(b"world"));
        assert_ne!(a.etag, b.etag);
    }

    #[test]
    fn etag_uses_stable_fnv1a_not_default_hasher() {
        // Golden value pinned to the FNV-1a algorithm. If this changes you've
        // either rotated the hashing algorithm (every outstanding client ETag
        // is invalidated — coordinate with operators) or accidentally
        // regressed to `DefaultHasher`, which mutates silently across rustc
        // versions and would re-key every browser cache on a binary upgrade.
        let t = CachedTile::new(Bytes::from_static(b"hello"));
        assert_eq!(t.etag, "\"a430d84680aabd0b\"");
    }

    #[test]
    fn etag_rotates_after_data_refresh() {
        // Production failure mode the old key-derived ETag was vulnerable to:
        // a data refresh re-encodes the same tile coordinates with new content,
        // but the key didn't change → ETag didn't change → browsers with the
        // previous ETag kept getting 304 Not Modified and rendered stale tiles
        // until their entries aged out. With content-derived ETags the property
        // holds by construction (different bytes ⇒ different hash), but pin it
        // as a guard against any future refactor that resurrects key-derived
        // semantics.
        let before = CachedTile::new(Bytes::from_static(b"\x1a\x05before"));
        let after = CachedTile::new(Bytes::from_static(b"\x1a\x04after"));
        assert_ne!(
            before.etag, after.etag,
            "post-refresh ETag must differ from pre-refresh — otherwise If-None-Match \
             returns 304 and the client never sees the refreshed tile"
        );
    }

    #[test]
    fn distinct_property_hashes_dont_collide() {
        let cache = VectorTileCache::new(1);
        let mut k1 = key(2, 1, 1);
        let mut k2 = k1.clone();
        k1.properties_hash = 1;
        k2.properties_hash = 2;
        cache.insert(k1.clone(), CachedTile::new(Bytes::from_static(b"one")));
        cache.insert(k2.clone(), CachedTile::new(Bytes::from_static(b"two")));
        assert_eq!(&cache.get(&k1).unwrap().bytes[..], b"one");
        assert_eq!(&cache.get(&k2).unwrap().bytes[..], b"two");
    }
}
