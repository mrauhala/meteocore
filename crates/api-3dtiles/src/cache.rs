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
//! (a per-key shared result). Without it, the viewer's frame preload could run
//! the identical multi-second resample N times in parallel.
//!
//! Process-global (`LazyLock`, like the engine-side pixel cache) rather than
//! per-`TilesState3d`: the key carries the collection id + data version, so
//! entries stay correct across config reloads, and one byte budget bounds the
//! whole server.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, LazyLock, Mutex};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use ds_cache::{ByteBoundedCache, CacheMetrics};

use crate::error::Tiles3dError;

/// Default encoded-content cache size (MB) when `MC_3DTILES_CONTENT_CACHE_MB`
/// is unset. Encoded tiles are a few hundred KB (echo-top) to tens of MB
/// (dense point clouds); 512 MB comfortably holds an animation window of one
/// busy collection. `0` disables retention, but concurrent callers still share
/// the same computation and result (including errors).
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
fn weigh_content(key: &ContentKey, val: &CachedContent) -> u64 {
    (val.bytes.len() + val.etag.len() + key.collection.len() + key.quantity.len() + 128) as u64
}

type ContentResult = Result<CachedContent, Tiles3dError>;
type Flights = Arc<Mutex<HashMap<ContentKey, tokio::sync::watch::Receiver<Option<ContentResult>>>>>;

/// Byte-bounded content cache with computations owned by the cache, not by
/// individual HTTP requests. Disconnecting a waiter cannot cancel an encode.
#[derive(Clone)]
pub struct ContentCache {
    cache: Arc<ByteBoundedCache<ContentKey, CachedContent>>,
    inflight: Flights,
}

/// Remove the flight even when its compute panics or the runtime shuts down.
/// Dropping the sender also wakes waiters with a closed-channel error.
struct FlightGuard {
    key: ContentKey,
    inflight: Flights,
}

impl Drop for FlightGuard {
    fn drop(&mut self) {
        self.inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.key);
    }
}

impl ContentCache {
    fn new(capacity_mb: u64) -> Self {
        ContentCache {
            cache: Arc::new(ByteBoundedCache::new(
                capacity_mb.saturating_mul(ds_cache::MIB),
                2 * ds_cache::MIB,
                weigh_content,
            )),
            inflight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Return cached content or join a shared computation. All callers in a
    /// flight receive its result, even for errors or entries too large to cache.
    /// Errors are not retained after the flight, so subsequent requests retry.
    pub async fn get_or_compute<F, Fut>(&self, key: ContentKey, compute: F) -> ContentResult
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<(Vec<u8>, String), Tiles3dError>> + Send + 'static,
    {
        if let Some(hit) = self.cache.get_untracked(&key) {
            self.cache.record_hit();
            return Ok(hit);
        }
        let mut receiver = {
            let mut map = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
            // A compute can finish between the first lookup and taking the lock.
            if let Some(hit) = self.cache.get_untracked(&key) {
                self.cache.record_hit();
                return Ok(hit);
            }
            if let Some(receiver) = map.get(&key) {
                self.cache.record_hit();
                receiver.clone()
            } else {
                self.cache.record_miss();
                let (sender, receiver) = tokio::sync::watch::channel(None);
                map.insert(key.clone(), receiver.clone());
                let cache = self.cache.clone();
                let guard = FlightGuard {
                    key: key.clone(),
                    inflight: self.inflight.clone(),
                };
                // No await between registering and spawning: cancellation cannot
                // leave a flight without an owner. Keep encoding and populate the
                // cache even if every HTTP waiter disconnects.
                tokio::spawn(async move {
                    let _guard = guard;
                    let result = compute().await.map(|(bytes, etag)| {
                        let content = CachedContent {
                            bytes: Bytes::from(bytes),
                            etag: Arc::from(etag),
                        };
                        cache.insert(key, content.clone());
                        content
                    });
                    sender.send_replace(Some(result));
                });
                receiver
            }
        };
        loop {
            let result = receiver.borrow_and_update().clone();
            if let Some(result) = result {
                return result;
            }
            receiver.changed().await.map_err(|_| {
                Tiles3dError::Internal("content computation stopped before completion".into())
            })?;
        }
    }

    /// Snapshot for `/metrics`.
    pub fn metrics(&self) -> CacheMetrics {
        self.cache.metrics()
    }
}

/// The process-global content cache, sized once from the environment.
pub static CONTENT_CACHE: LazyLock<ContentCache> = LazyLock::new(|| {
    ContentCache::new(ds_cache::env_mb(
        "MC_3DTILES_CONTENT_CACHE_MB",
        DEFAULT_CONTENT_CACHE_MB,
    ))
});

/// Snapshot of the global cache for `/metrics`.
pub fn content_cache_metrics() -> CacheMetrics {
    CONTENT_CACHE.metrics()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

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
                .get_or_compute(key(1), move || async move {
                    c.fetch_add(1, Ordering::Relaxed);
                    Ok((vec![1, 2, 3], "\"abc\"".to_string()))
                })
                .await
                .unwrap();
            assert_eq!(&got.bytes[..], &[1, 2, 3]);
            assert_eq!(&*got.etag, "\"abc\"");
        }
        assert_eq!(computes.load(Ordering::Relaxed), 1, "one compute, two hits");
        let m = cache.metrics();
        assert_eq!((m.hits, m.misses), (2, 1));
        assert!(m.bytes > 0);
    }

    #[tokio::test]
    async fn version_change_is_a_new_entry() {
        let cache = ContentCache::new(64);
        for v in [1u64, 2] {
            let got = cache
                .get_or_compute(key(v), move || async move {
                    Ok((vec![v as u8], format!("\"{v}\"")))
                })
                .await
                .unwrap();
            assert_eq!(&got.bytes[..], &[v as u8]);
        }
        let m = cache.metrics();
        assert_eq!((m.hits, m.misses), (0, 2));
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
                    .get_or_compute(key(7), move || async move {
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
        let m = cache.metrics();
        assert_eq!((m.hits, m.misses), (0, 2), "nothing admitted at capacity 0");
    }
    #[tokio::test]
    async fn canceled_waiter_does_not_cancel_compute_or_leak_flight() {
        let cache = ContentCache::new(64);
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let c = cache.clone();
        let s = started.clone();
        let r = release.clone();
        let request = tokio::spawn(async move {
            c.get_or_compute(key(99), move || async move {
                s.notify_one();
                r.notified().await;
                Ok((vec![42], "etag".into()))
            })
            .await
        });
        started.notified().await;
        request.abort();
        let _ = request.await;
        assert_eq!(
            cache.inflight.lock().unwrap().len(),
            1,
            "compute still owns the flight"
        );
        release.notify_one();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            cache.get_or_compute(key(99), || async {
                panic!("must join the original compute")
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(&result.bytes[..], &[42]);
        assert!(cache.inflight.lock().unwrap().is_empty());
        assert_eq!(cache.metrics().misses, 1);
    }

    #[tokio::test]
    async fn concurrent_errors_share_one_result_and_later_request_retries() {
        let cache = ContentCache::new(0);
        let computes = Arc::new(AtomicU64::new(0));
        let release = Arc::new(tokio::sync::Notify::new());
        let mut requests = Vec::new();
        for _ in 0..8 {
            let c = cache.clone();
            let n = computes.clone();
            let r = release.clone();
            requests.push(tokio::spawn(async move {
                c.get_or_compute(key(100), move || async move {
                    n.fetch_add(1, Ordering::Relaxed);
                    r.notified().await;
                    Err(Tiles3dError::NotFound("no echo".into()))
                })
                .await
            }));
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while cache.metrics().hits < 7 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        release.notify_one();
        for request in requests {
            assert!(matches!(
                request.await.unwrap(),
                Err(Tiles3dError::NotFound(_))
            ));
        }
        assert_eq!(computes.load(Ordering::Relaxed), 1);
        assert!(cache.inflight.lock().unwrap().is_empty());
        assert!(cache
            .get_or_compute(key(100), || async { Ok((vec![1], "retry".into())) })
            .await
            .is_ok());
        assert_eq!(cache.metrics().misses, 2);
    }

    #[tokio::test]
    async fn panic_wakes_waiters_and_releases_flight() {
        let cache = ContentCache::new(1);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            cache.get_or_compute(key(101), || async { panic!("injected compute panic") }),
        )
        .await
        .unwrap();
        assert!(matches!(result, Err(Tiles3dError::Internal(_))));
        assert!(cache.inflight.lock().unwrap().is_empty());
    }
}
