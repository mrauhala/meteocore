//! Where analysis grids are computed and kept: one process-wide
//! byte-bounded grid cache, one admission budget for grids being computed,
//! and one bounded pool of dedicated threads that computes them.
//!
//! - **Cache** (`MC_STATION_GRID_CACHE_MB`, default 256): one grid per
//!   (engine instance, parameter, analysis time, revision of its input);
//!   the engine instance stands for its source and method configuration, which
//!   a reload can only change by building a new engine. Single-flight
//!   through the cache's fill guard: concurrent requests for one grid wait
//!   for one computation. One shard, so any grid that fits the budget is
//!   admitted.
//! - **Admission** (`MC_STATION_GRID_MEMORY_MB`, default 512): the bytes of
//!   every grid being computed are reserved before it is queued. A request
//!   that does not fit is turned away at once (`ResourceExhausted`, 503);
//!   the background pre-compute takes only what leaves half the budget to
//!   requests.
//! - **Pool** (`MC_STATION_GRID_WORKERS`, default half the CPUs, 1 to 4):
//!   its own multi-thread Tokio runtime, at most that many grids at once,
//!   never the request-serving runtime or a per-request rayon fan-out. A
//!   job runs inside `block_in_place`, so a source whose sync bridge needs a
//!   runtime handle (PostGIS) finds this one.

use std::sync::{Arc, LazyLock};

use ds_cache::{ByteBoundedCache, CacheMetrics, MIB};
use ds_executor::budget::{ByteBudget, BytePermit};

/// A grid's cache key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct GridKey {
    /// The engine instance (unique per construction).
    pub epoch: u64,
    /// Index into the engine's parameters.
    pub parameter: usize,
    /// Analysis time, Unix seconds.
    pub time: i64,
    /// How many times the input of this (parameter, time) was revised
    /// before the grid was asked for.
    pub revision: u64,
}

/// One computed analysis.
#[derive(Debug)]
pub struct AnalysisGrid {
    /// Row-major cells, row 0 north; NaN where no station is in reach.
    pub values: Vec<f32>,
    /// Stations the analysis was computed from.
    pub stations: usize,
    /// The input slice's fingerprint.
    pub fingerprint: u64,
    /// Each station's leave-one-out residual, in slice order (#1206):
    /// computed with the grid, cached with it.
    pub residuals: Vec<crate::residuals::Residual>,
}

fn weigh(_: &GridKey, grid: &Arc<AnalysisGrid>) -> u64 {
    let residuals = grid.residuals.len() * std::mem::size_of::<crate::residuals::Residual>();
    grid_bytes(grid.values.len()) + residuals as u64 + 128
}

/// Bytes of a grid of `cells` cells.
pub(crate) fn grid_bytes(cells: usize) -> u64 {
    (cells as u64).saturating_mul(4)
}

pub(crate) static CACHE: LazyLock<ByteBoundedCache<GridKey, Arc<AnalysisGrid>>> =
    LazyLock::new(|| {
        ByteBoundedCache::from_env_single_shard("MC_STATION_GRID_CACHE_MB", 256, 4 * MIB, weigh)
    });

/// Admission for grids being computed.
pub(crate) static BUDGET: LazyLock<Arc<ByteBudget>> = LazyLock::new(|| {
    Arc::new(ByteBudget::new(
        ds_cache::env_mb("MC_STATION_GRID_MEMORY_MB", 512).saturating_mul(MIB),
    ))
});

/// Bytes a computation holds besides its grid: the station slice, its
/// index and the source's response.
pub(crate) const COMPUTE_SLACK_BYTES: u64 = 4 * MIB;

/// Snapshot of the grid cache for `/metrics`.
pub fn cache_metrics() -> CacheMetrics {
    CACHE.metrics()
}

/// `(reserved bytes, capacity bytes, rejected admissions)` of the
/// computation budget, for `/metrics`.
pub fn budget_metrics() -> (u64, u64, u64) {
    BUDGET.metrics()
}

/// The grid cache's configured capacity in bytes.
pub(crate) fn cache_capacity() -> u64 {
    CACHE.capacity_bytes()
}

/// Drop one engine instance's grids (it was shut down).
pub(crate) fn evict_epoch(epoch: u64) {
    CACHE.retain(|key, _| key.epoch != epoch);
}

/// Reserve a request's computation, or fail with `ResourceExhausted`.
pub(crate) fn reserve(bytes: u64) -> Result<BytePermit, ds_core::error::DataServerError> {
    BUDGET.reserve(bytes)
}

/// Reserve a background computation if it leaves requests their half.
pub(crate) fn reserve_background(bytes: u64) -> Option<BytePermit> {
    BUDGET.try_reserve_background(bytes)
}

struct Pool {
    runtime: tokio::runtime::Runtime,
    slots: Arc<tokio::sync::Semaphore>,
}

/// Grids computed at once.
pub fn pool_workers() -> usize {
    std::env::var("MC_STATION_GRID_WORKERS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get() / 2)
                .unwrap_or(1)
                .clamp(1, 4)
        })
        .min(64)
}

static POOL: LazyLock<Pool> = LazyLock::new(|| {
    let workers = pool_workers();
    Pool {
        runtime: tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .thread_name("mc-station-grid")
            .enable_all()
            .build()
            .expect("station-grid pool runtime"),
        slots: Arc::new(tokio::sync::Semaphore::new(workers)),
    }
});

/// Run `job` on the pool once a slot is free. It runs to completion even
/// when whoever queued it stops waiting, so its grid still lands in the
/// cache for the retry.
pub(crate) fn spawn(job: impl FnOnce() + Send + 'static) {
    let slots = POOL.slots.clone();
    POOL.runtime.spawn(async move {
        let Ok(_slot) = slots.acquire_owned().await else {
            return;
        };
        tokio::task::block_in_place(job);
    });
}
