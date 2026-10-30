//! Process-wide caches of scans and decoded blocks, shared by every
//! satellite collection and bounded in bytes (`ds-cache`, #480), and the
//! bookkeeping that keeps each collection's time window resident.
//!
//! The block cache keeps its phase-2 name, `STRIPS`, and its env var and
//! metric family: a GOES-R block is a strip.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use ds_cache::plan;

use crate::frame::Frame;

/// A scan: the engine that ingested it, its collection, parameter and scan
/// start (Unix seconds).
///
/// `engine` is the owning [`SatelliteEngine`](crate::SatelliteEngine)
/// instance, not the collection: a reload that rebuilds a collection, or a
/// rejected reload's candidate, builds a second engine with the same id.
/// Each engine sweeps and releases only its own keys, so neither can drop
/// the scans the live engine serves.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct FrameKey {
    pub engine: u64,
    pub collection: Arc<str>,
    pub parameter: Arc<str>,
    pub time: i64,
}

/// One decoded block of a scan.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct BlockKey {
    pub frame: FrameKey,
    pub block: u32,
}

type FrameCache = ds_cache::ByteBoundedCache<FrameKey, Arc<Frame>>;
type StripCache = ds_cache::ByteBoundedCache<BlockKey, Arc<[u16]>>;

/// Scans held in memory: the compressed file plus its overview, ~30 MB for
/// a 2 km full disk. The poll loop fills it with each new scan, removes the
/// scans that left the time window, and downloads again the in-window scans
/// it evicted while every collection's window fits
/// (`SatelliteEngine::keep_resident`). A scan still missing is fetched when
/// a request needs it.
///
/// One shard: `quick_cache` would otherwise give each shard an equal slice
/// of the budget, and windows that fit the whole budget could still
/// overflow one slice and evict each other on every poll. Few, large
/// entries at a low lookup rate make one lock free.
pub(crate) static FRAMES: LazyLock<FrameCache> = LazyLock::new(|| {
    let (bytes, source) = plan::env_bytes("MC_SATELLITE_FRAME_CACHE_MB", 1024);
    // A demand cache (#1311): its shard admits at most its hot share, so a
    // resize below its scans would refuse every scan.
    FrameCache::new_single_shard(bytes, 30 << 20, |_, frame| frame.weight)
        .with_entry_floor()
        .registered(
            "satellite_frame_cache",
            plan::Scope::Engine("satellite"),
            source,
        )
});

/// Decoded blocks, ~260 KB each for a 24-row 2 km full-disk strip.
pub(crate) static STRIPS: LazyLock<StripCache> = LazyLock::new(|| {
    let (bytes, source) = plan::env_bytes("MC_SATELLITE_STRIP_CACHE_MB", 256);
    StripCache::new(bytes, 256 << 10, |_, strip| strip.len() as u64 * 2).registered(
        "satellite_strip_cache",
        plan::Scope::Engine("satellite"),
        source,
    )
});

/// Snapshot of the scan cache for `/metrics`.
pub fn frame_metrics() -> ds_cache::CacheMetrics {
    FRAMES.metrics()
}

/// Resize the scan cache to `bytes` in place, keeping the scans that fit,
/// and never below four of its largest scan (#1311). Its size comes from
/// the environment, which cannot change without a restart: nothing calls
/// this on reload yet.
///
/// Returns the capacity applied: a shrink stops where the cache's shards
/// still hold 32 entries each, and a growth from 0 takes `quick_cache`'s
/// default hot share ([`ds_cache::ByteBoundedCache::set_capacity`]).
pub fn resize_frame_cache(bytes: u64) -> u64 {
    FRAMES.set_capacity(bytes)
}

/// Snapshot of the decoded-block cache for `/metrics`.
pub fn strip_metrics() -> ds_cache::CacheMetrics {
    STRIPS.metrics()
}

/// Resize the decoded-block cache to `bytes` in place, keeping the blocks
/// that fit (#1311). Its size comes from the environment, which cannot
/// change without a restart: nothing calls this on reload yet.
///
/// Returns the capacity applied: a shrink stops where the cache's shards
/// still hold 32 entries each, and a growth from 0 takes `quick_cache`'s
/// default hot share ([`ds_cache::ByteBoundedCache::set_capacity`]).
pub fn resize_strip_cache(bytes: u64) -> u64 {
    STRIPS.set_capacity(bytes)
}

static NEXT_ENGINE: AtomicU64 = AtomicU64::new(1);

/// A fresh [`FrameKey::engine`] for a new engine.
pub(crate) fn next_engine() -> u64 {
    NEXT_ENGINE.fetch_add(1, Ordering::Relaxed)
}

/// Scans the poll downloaded again after the cache evicted them.
static REINGESTS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn count_reingest() {
    REINGESTS.fetch_add(1, Ordering::Relaxed);
}

/// Scans downloaded again by a poll because the cache had evicted them,
/// since start, for `/metrics`. A steady rise means the windows do not fit
/// `MC_SATELLITE_FRAME_CACHE_MB` with room to spare.
pub fn frame_reingests() -> u64 {
    REINGESTS.load(Ordering::Relaxed)
}

/// Per live engine: its collection and the bytes of the scans in its time
/// window, as of its last poll. What `FRAMES` must hold for no request to
/// download a scan.
type Windows = HashMap<u64, (Arc<str>, u64)>;
static WINDOWS: LazyLock<Mutex<Windows>> = LazyLock::new(Default::default);

fn windows_lock() -> std::sync::MutexGuard<'static, Windows> {
    WINDOWS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Record `engine`'s window after a poll.
pub(crate) fn record_window(engine: u64, collection: &Arc<str>, bytes: u64) {
    windows_lock().insert(engine, (collection.clone(), bytes));
}

/// Every live engine's window, by collection id.
pub(crate) fn windows() -> Vec<(Arc<str>, u64)> {
    let mut windows: Vec<_> = windows_lock().values().cloned().collect();
    windows.sort();
    windows
}

/// Remove `engine`'s scans that `keep` rejects, and the decoded blocks of
/// each one still resident, by key (its block count). Blocks of a scan the
/// cache had already evicted are left to age out of `STRIPS`: finding them
/// would mean visiting every block.
///
/// Visits every scan in `FRAMES`: a few hundred at most, each MBs.
pub(crate) fn sweep(engine: u64, keep: impl Fn(&FrameKey) -> bool) {
    let dropped = RefCell::new(Vec::new());
    FRAMES.retain(|key, frame| {
        if key.engine != engine || keep(key) {
            return true;
        }
        dropped
            .borrow_mut()
            .push((key.clone(), frame.block_count()));
        false
    });
    for (frame, blocks) in dropped.into_inner() {
        for block in 0..blocks {
            STRIPS.remove(&BlockKey {
                frame: frame.clone(),
                block,
            });
        }
    }
}

/// Everything `engine` holds: its scans, all their decoded blocks, and its
/// window. Once, when the engine is dropped, so a full pass over `STRIPS`
/// is fine.
pub(crate) fn release(engine: u64) {
    FRAMES.retain(|key, _| key.engine != engine);
    STRIPS.retain(|key, _| key.frame.engine != engine);
    windows_lock().remove(&engine);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Grow a process-wide cache by 1 MiB through `resize`, check `metrics`
    /// reports it, and give it its size back: other tests share it.
    fn resizes_in_place(metrics: impl Fn() -> ds_cache::CacheMetrics, resize: fn(u64) -> u64) {
        let before = metrics().capacity_bytes;
        resize(before + ds_cache::MIB);
        assert_eq!(metrics().capacity_bytes, before + ds_cache::MIB);
        resize(before);
        assert_eq!(metrics().capacity_bytes, before);
    }

    /// #1311: the process-wide scan and block caches resize in place and
    /// report their new capacities.
    #[test]
    fn the_process_wide_caches_resize_in_place() {
        resizes_in_place(frame_metrics, resize_frame_cache);
        resizes_in_place(strip_metrics, resize_strip_cache);
    }
}
