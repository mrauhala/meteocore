//! Process-wide caches of scans and decoded strips, shared by every
//! satellite collection and bounded in bytes (`ds-cache`, #480).

use std::sync::{Arc, LazyLock};

use crate::frame::Frame;

/// A scan: collection, parameter and scan start (Unix seconds).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct FrameKey {
    pub collection: Arc<str>,
    pub parameter: Arc<str>,
    pub time: i64,
}

/// One decoded strip of a scan.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct StripKey {
    pub frame: FrameKey,
    pub strip: u32,
}

type FrameCache = ds_cache::ByteBoundedCache<FrameKey, Arc<Frame>>;
type StripCache = ds_cache::ByteBoundedCache<StripKey, Arc<[u16]>>;

/// Scans held in memory: the compressed file plus its overview, ~30 MB for
/// a 2 km full disk. The poll loop fills it with each new scan; a scan
/// evicted here is fetched again when a render needs it.
pub(crate) static FRAMES: LazyLock<FrameCache> = LazyLock::new(|| {
    FrameCache::from_env("MC_SATELLITE_FRAME_CACHE_MB", 1024, 30 << 20, |_, frame| {
        frame.weight
    })
});

/// Decoded strips, ~260 KB each for a 24-row 2 km full-disk strip.
pub(crate) static STRIPS: LazyLock<StripCache> = LazyLock::new(|| {
    StripCache::from_env("MC_SATELLITE_STRIP_CACHE_MB", 256, 256 << 10, |_, strip| {
        strip.len() as u64 * 2
    })
});

/// Snapshot of the scan cache for `/metrics`.
pub fn frame_metrics() -> ds_cache::CacheMetrics {
    FRAMES.metrics()
}

/// Snapshot of the decoded-strip cache for `/metrics`.
pub fn strip_metrics() -> ds_cache::CacheMetrics {
    STRIPS.metrics()
}
