//! Process-global cache of **encoded 3D Tiles content bytes**, with
//! single-flight coalescing.
//!
//! Why this exists (hot-path audit, 2026-06): every `content.pnts` /
//! `content.glb` / voxel-content request used to pay the full engine read +
//! resample + encode pipeline — *including* `If-None-Match` revalidations,
//! whose ETag was only known after a complete recompute. The bundled viewer
//! preloads up to 48 animation frames concurrently, multiplying that cost by
//! the frame count on every load, reload, and control change.
//!
//! The cache stores the final encoded bytes + their strong ETag, keyed by
//! everything that determines them (collection, product, quantity, time,
//! product parameters, resolution) **plus a data-version** derived from the
//! collection's `VolumeInfo` time axis — when the engine ingests or drops a
//! volume the version changes, so "latest" requests and nearest-time
//! selection changes invalidate naturally without duplicating the engine's
//! selection logic here.
//!
//! Single-flight: concurrent requests for the same key share one compute
//! (a per-key async mutex). Without it, the viewer's frame preload could run
//! the identical multi-second resample N times in parallel.
//!
//! Process-global (`LazyLock`, like the engine-side pixel cache) rather than
//! per-`TilesState3d`: the key carries the collection id + data version, so
//! entries stay correct across config reloads, and one byte budget bounds the
//! whole server.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use bytes::Bytes;
use chrono::{DateTime, Utc};

use crate::error::Tiles3dError;

/// Default encoded-content cache size (MB) when `MC_3DTILES_CONTENT_CACHE_MB`
/// is unset. Encoded tiles are a few hundred KB (echo-top) to tens of MB
/// (dense point clouds); 512 MB comfortably holds an animation window of one
/// busy collection. `0` disables caching (every request recomputes —
/// diagnostic only; single-flight still coalesces concurrent duplicates).
const DEFAULT_CONTENT_CACHE_MB: u64 = 512;

/// Which encoded product the bytes are — part of the key so two products with
/// otherwise-identical parameters can't collide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContentKind {
    /// `.pnts` point cloud.
    Pnts,
    /// Isosurface mesh `.glb`.
    Isosurface,
    /// Echo-top columns `.glb`.
    EchoTop,
    /// `EXT_primitive_voxels` `.glb`.
    Voxels,
}

/// Everything that determines the encoded bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContentKey {
    pub collection: String,
    pub kind: ContentKind,
    /// Resolved quantity — callers resolve an absent `?quantity=` to the
    /// collection default so both forms share one entry.
    pub quantity: String,
    /// Requested valid time (`None` = latest). The engine's nearest-volume
    /// selection for a *pinned* time can change when data arrives; that (and
    /// "latest" advancing) is covered by `version`, not by this field.
    pub datetime: Option<DateTime<Utc>>,
    /// Product parameter bits: `min_value` (points) or `threshold` (meshes),
    /// `f64::to_bits` for `Eq`/`Hash`. `None` when the request had none (the
    /// callers pass the applied default explicitly, so a defaulted and an
    /// explicit-default request share an entry).
    pub param_bits: Option<u64>,
    /// Voxel-grid dims for the mesh/voxel products; `[0; 3]` for points
    /// (native resolution, no grid).
    pub dims: [usize; 3],
    /// Data version of the collection — see [`module docs`](self). Computed
    /// by the handler from `VolumeInfo`.
    pub version: u64,
}

/// A cached response body: cheap-clone bytes + the strong ETag computed over
/// them. Returned by value (both fields are refcounted).
#[derive(Clone)]
pub struct CachedContent {
    pub bytes: Bytes,
    pub etag: Arc<str>,
}

/// Byte-weights an entry by its encoded payload.
#[derive(Clone)]
struct ContentWeighter;

impl quick_cache::Weighter<ContentKey, CachedContent> for ContentWeighter {
    fn weight(&self, key: &ContentKey, val: &CachedContent) -> u64 {
        (val.bytes.len() + val.etag.len() + key.collection.len() + key.quantity.len() + 128) as u64
    }
}

/// Byte-bounded LRU of encoded content + per-key single-flight gates.
pub struct ContentCache {
    cache: quick_cache::sync::Cache<ContentKey, CachedContent, ContentWeighter>,
    /// One async mutex per in-flight key. The first requester computes while
    /// holding the gate; coalesced requesters block on it, then find the
    /// cache populated. Entries are removed when their compute finishes, so
    /// the map only ever holds currently-computing keys.
    inflight: Mutex<HashMap<ContentKey, Arc<tokio::sync::Mutex<()>>>>,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl ContentCache {
    fn new(capacity_mb: u64) -> Self {
        let capacity_bytes = capacity_mb.saturating_mul(1024 * 1024);
        // Estimate item slots at ~2 MB each; `.max(1)` keeps capacity 0 valid
        // (entries heavier than capacity are simply never admitted — the same
        // "disabled" idiom as the engine's pixel cache).
        let estimated_items = ((capacity_bytes / (2 * 1024 * 1024)).max(16)) as usize;
        ContentCache {
            cache: quick_cache::sync::Cache::with_weighter(
                estimated_items,
                capacity_bytes.max(1),
                ContentWeighter,
            ),
            inflight: Mutex::new(HashMap::new()),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Cached bytes for `key`, or run `compute` (which returns the encoded
    /// bytes + their ETag) and cache its result. Concurrent callers with the
    /// same key share one compute; errors are not cached (coalesced waiters
    /// then retry serially, which bounds an error stampede without pinning a
    /// transient failure).
    pub async fn get_or_compute<F, Fut>(
        &self,
        key: ContentKey,
        compute: F,
    ) -> Result<CachedContent, Tiles3dError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(Vec<u8>, String), Tiles3dError>>,
    {
        if let Some(hit) = self.cache.get(&key) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(hit);
        }
        let gate = {
            let mut map = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
            map.entry(key.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _held = gate.lock().await;
        // Re-check under the gate: if a coalesced compute just finished, the
        // bytes are in the cache and this request cost two lookups.
        if let Some(hit) = self.cache.get(&key) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            self.release(&key, &gate);
            return Ok(hit);
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        let result = compute().await;
        let out = match result {
            Ok((bytes, etag)) => {
                let content = CachedContent {
                    bytes: Bytes::from(bytes),
                    etag: Arc::from(etag.as_str()),
                };
                self.cache.insert(key.clone(), content.clone());
                Ok(content)
            }
            Err(e) => Err(e),
        };
        self.release(&key, &gate);
        out
    }

    /// Drop this key's in-flight gate — but only if it is still *our* gate
    /// (`ptr_eq`): after an error, a later request may have installed a fresh
    /// gate for the same key, which must not be removed from under it.
    fn release(&self, key: &ContentKey, gate: &Arc<tokio::sync::Mutex<()>>) {
        let mut map = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        if map.get(key).is_some_and(|g| Arc::ptr_eq(g, gate)) {
            map.remove(key);
        }
    }

    /// Snapshot for `/metrics`: `(hits, misses, resident_bytes, capacity_bytes)`.
    pub fn metrics(&self) -> (u64, u64, u64, u64) {
        (
            self.hits.load(Ordering::Relaxed),
            self.misses.load(Ordering::Relaxed),
            self.cache.weight(),
            self.cache.capacity(),
        )
    }
}

/// The process-global content cache, sized once from the environment.
pub static CONTENT_CACHE: LazyLock<ContentCache> = LazyLock::new(|| {
    let mb = std::env::var("MC_3DTILES_CONTENT_CACHE_MB")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_CONTENT_CACHE_MB);
    ContentCache::new(mb)
});

/// Snapshot of the global cache for `/metrics`.
pub fn content_cache_metrics() -> (u64, u64, u64, u64) {
    CONTENT_CACHE.metrics()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u64) -> ContentKey {
        ContentKey {
            collection: "c".into(),
            kind: ContentKind::Pnts,
            quantity: "DBZH".into(),
            datetime: None,
            param_bits: None,
            dims: [0; 3],
            version: n,
        }
    }

    #[tokio::test]
    async fn caches_and_serves_without_recompute() {
        let cache = ContentCache::new(64);
        let computes = Arc::new(AtomicU64::new(0));
        for _ in 0..3 {
            let c = computes.clone();
            let got = cache
                .get_or_compute(key(1), || async move {
                    c.fetch_add(1, Ordering::Relaxed);
                    Ok((vec![1, 2, 3], "\"abc\"".to_string()))
                })
                .await
                .unwrap();
            assert_eq!(&got.bytes[..], &[1, 2, 3]);
            assert_eq!(&*got.etag, "\"abc\"");
        }
        assert_eq!(computes.load(Ordering::Relaxed), 1, "one compute, two hits");
        let (hits, misses, bytes, _cap) = cache.metrics();
        assert_eq!((hits, misses), (2, 1));
        assert!(bytes > 0);
    }

    #[tokio::test]
    async fn version_change_is_a_new_entry() {
        let cache = ContentCache::new(64);
        for v in [1u64, 2] {
            let got = cache
                .get_or_compute(
                    key(v),
                    || async move { Ok((vec![v as u8], format!("\"{v}\""))) },
                )
                .await
                .unwrap();
            assert_eq!(&got.bytes[..], &[v as u8]);
        }
        let (hits, misses, _, _) = cache.metrics();
        assert_eq!((hits, misses), (0, 2));
    }

    #[tokio::test]
    async fn concurrent_same_key_coalesces_to_one_compute() {
        let cache = Arc::new(ContentCache::new(64));
        let computes = Arc::new(AtomicU64::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let cache = cache.clone();
            let computes = computes.clone();
            handles.push(tokio::spawn(async move {
                cache
                    .get_or_compute(key(7), || async move {
                        computes.fetch_add(1, Ordering::Relaxed);
                        // Linger so the other tasks pile onto the gate.
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        Ok((vec![9], "\"x\"".to_string()))
                    })
                    .await
            }));
        }
        for h in handles {
            let got = h.await.unwrap().unwrap();
            assert_eq!(&got.bytes[..], &[9]);
        }
        assert_eq!(computes.load(Ordering::Relaxed), 1, "coalesced");
        // The gate map must not leak finished keys.
        assert!(cache.inflight.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn error_is_not_cached_and_gate_is_released() {
        let cache = ContentCache::new(64);
        let err = cache
            .get_or_compute(key(3), || async {
                Err(Tiles3dError::Internal("boom".into()))
            })
            .await;
        assert!(err.is_err());
        assert!(cache.inflight.lock().unwrap().is_empty());
        // A later request recomputes and succeeds.
        let got = cache
            .get_or_compute(key(3), || async { Ok((vec![4], "\"y\"".to_string())) })
            .await
            .unwrap();
        assert_eq!(&got.bytes[..], &[4]);
    }

    #[tokio::test]
    async fn capacity_zero_disables_storage_but_still_serves() {
        let cache = ContentCache::new(0);
        for _ in 0..2 {
            let got = cache
                .get_or_compute(key(5), || async { Ok((vec![1], "\"z\"".to_string())) })
                .await
                .unwrap();
            assert_eq!(&got.bytes[..], &[1]);
        }
        let (hits, misses, _, _) = cache.metrics();
        assert_eq!((hits, misses), (0, 2), "nothing admitted at capacity 0");
    }
}
