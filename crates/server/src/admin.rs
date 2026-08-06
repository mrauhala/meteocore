use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::Instant;

use arc_swap::ArcSwap;
use axum::extract::{MatchedPath, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use prometheus::{
    Encoder, GaugeVec, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge,
    IntGaugeVec, Opts, Registry, TextEncoder,
};
use serde::Serialize;
use serde_json::json;
use tracing::info;

use api_edr::handlers::EdrState;
use api_features::handlers::FeaturesState;
use api_maps::MapsState;
use api_tiles::TilesState;
use api_wms::WmsState;
use ds_core::config::{CollectionConfig, StyleBundle};

// ---------------------------------------------------------------------------
// Prometheus metrics (global)
// ---------------------------------------------------------------------------

static REGISTRY: LazyLock<Registry> = LazyLock::new(Registry::new);

/// Build and register a plain `IntGauge`.
fn int_gauge(name: &str, help: &str) -> IntGauge {
    let gauge = IntGauge::new(name, help).unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
}

/// A Prometheus counter fed from a *cumulative* in-process value: each
/// [`Self::feed`] emits the delta since the last observed value. A backward
/// step (the underlying cache was replaced on reload, resetting its counters)
/// rebaselines without emitting a spike (`saturating_sub` → delta 0).
struct DeltaCounter {
    counter: IntCounter,
    last: AtomicU64,
}

impl DeltaCounter {
    fn new(name: &str, help: &str) -> Self {
        let counter = IntCounter::new(name, help).unwrap();
        REGISTRY.register(Box::new(counter.clone())).unwrap();
        DeltaCounter {
            counter,
            last: AtomicU64::new(0),
        }
    }

    /// Emit `cumulative - last_seen` (clamped at 0) and remember `cumulative`.
    /// The atomic `swap` keeps concurrent scrapes correct: however feeds
    /// interleave, each increment is emitted exactly once. Always called
    /// (even with delta 0) so the `LazyLock` family registers on the first
    /// scrape and dashboards see the series before any traffic.
    fn feed(&self, cumulative: u64) {
        let prev = self.last.swap(cumulative, Ordering::Relaxed);
        self.counter.inc_by(cumulative.saturating_sub(prev));
    }
}

/// One byte-bounded cache's standard `/metrics` family (#480): delta-tracked
/// `{prefix}_hits_total` / `{prefix}_misses_total` counters plus
/// `{prefix}_bytes` / `{prefix}_capacity_bytes` gauges, and optionally
/// `{prefix}_entries`. Help strings are derived from `display` (the human
/// name, first letter uppercased for the hits/misses text) with optional
/// parenthetical notes — matching the previously hand-written families
/// byte-for-byte.
struct CacheMetricSet {
    hits: DeltaCounter,
    misses: DeltaCounter,
    bytes: IntGauge,
    capacity: IntGauge,
    entries: Option<IntGauge>,
}

impl CacheMetricSet {
    fn new(
        prefix: &str,
        display: &str,
        hits_note: Option<&str>,
        misses_note: Option<&str>,
        with_entries: bool,
    ) -> Self {
        let capitalized = {
            let mut chars = display.chars();
            match chars.next() {
                Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        };
        let note = |n: Option<&str>| n.map(|n| format!(" ({n})")).unwrap_or_default();
        CacheMetricSet {
            hits: DeltaCounter::new(
                &format!("{prefix}_hits_total"),
                &format!("{capitalized} hits{}", note(hits_note)),
            ),
            misses: DeltaCounter::new(
                &format!("{prefix}_misses_total"),
                &format!("{capitalized} misses{}", note(misses_note)),
            ),
            bytes: int_gauge(
                &format!("{prefix}_bytes"),
                &format!("Bytes currently held in the {display}"),
            ),
            capacity: int_gauge(
                &format!("{prefix}_capacity_bytes"),
                &format!("Configured {display} capacity in bytes"),
            ),
            entries: with_entries.then(|| {
                int_gauge(
                    &format!("{prefix}_entries"),
                    &format!("Number of entries currently in the {display}"),
                )
            }),
        }
    }

    /// Feed one scrape's snapshot. `entries` only lands if the set was built
    /// `with_entries`.
    fn update(&self, m: ds_cache::CacheMetrics, entries: Option<u64>) {
        self.hits.feed(m.hits);
        self.misses.feed(m.misses);
        self.bytes.set(m.bytes as i64);
        self.capacity.set(m.capacity_bytes as i64);
        if let (Some(gauge), Some(n)) = (&self.entries, entries) {
            gauge.set(n as i64);
        }
    }
}

static HTTP_REQUESTS_TOTAL: LazyLock<IntCounterVec> = LazyLock::new(|| {
    let counter = IntCounterVec::new(
        Opts::new("http_requests_total", "Total HTTP requests"),
        &["method", "path", "status"],
    )
    .unwrap();
    REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

static HTTP_REQUEST_DURATION: LazyLock<HistogramVec> = LazyLock::new(|| {
    let histogram = HistogramVec::new(
        HistogramOpts::new(
            "http_request_duration_seconds",
            "HTTP request duration in seconds",
        )
        // Buckets include 1.5/2/3/4 between 1s and 5s: without them, any
        // request in (1, 5] is linearly interpolated by histogram_quantile
        // across that wide bucket, so a handful of ~1.5s requests at low
        // traffic read as a ~4-5s p99 in Grafana. 10s separates genuine
        // >5s outliers from the merely-slow.
        .buckets(vec![
            0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 1.5, 2.0, 3.0, 4.0, 5.0, 10.0,
        ]),
        &["method", "path"],
    )
    .unwrap();
    REGISTRY.register(Box::new(histogram.clone())).unwrap();
    histogram
});

static COLLECTIONS_TOTAL: LazyLock<IntGauge> = LazyLock::new(|| {
    let gauge = IntGauge::new("collections_total", "Total configured collections").unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

// Process + allocator memory (#493). RSS/swap come from /proc/self/status
// (Linux only; the series stay 0 elsewhere). The jemalloc_* gauges expose the
// allocator's own accounting so the fragmentation multiplier is visible in
// prod: `allocated` = live bytes the application holds, `resident` = physical
// pages jemalloc keeps — resident far above allocated means allocator
// retention (the glibc failure mode this deployment hit was ~3×).
static PROCESS_MEM_GAUGES: LazyLock<[IntGauge; 2]> = LazyLock::new(|| {
    let mk = |name: &str, help: &str| {
        let g = IntGauge::new(name, help).unwrap();
        REGISTRY.register(Box::new(g.clone())).unwrap();
        g
    };
    [
        mk(
            "process_resident_memory_bytes",
            "Resident set size (VmRSS) of the server process",
        ),
        mk(
            "process_swap_memory_bytes",
            "Swapped-out anonymous memory (VmSwap) of the server process",
        ),
    ]
});

#[cfg(not(target_env = "msvc"))]
static JEMALLOC_GAUGES: LazyLock<[IntGauge; 5]> = LazyLock::new(|| {
    let mk = |name: &str, help: &str| {
        let g = IntGauge::new(name, help).unwrap();
        REGISTRY.register(Box::new(g.clone())).unwrap();
        g
    };
    [
        mk(
            "jemalloc_allocated_bytes",
            "Bytes in live allocations (application-held)",
        ),
        mk(
            "jemalloc_active_bytes",
            "Bytes in active pages backing allocations (>= allocated; gap = internal fragmentation)",
        ),
        mk(
            "jemalloc_resident_bytes",
            "Physical bytes jemalloc keeps mapped (>= active; gap = dirty pages awaiting decay)",
        ),
        mk(
            "jemalloc_mapped_bytes",
            "Total bytes in jemalloc extent mappings",
        ),
        mk(
            "jemalloc_retained_bytes",
            "Virtual address space retained for reuse (not physically backed)",
        ),
    ]
});

/// Refresh the process/allocator memory gauges. Called per `/metrics` scrape.
fn update_memory_gauges() {
    // Touch the family even where /proc is unavailable so dashboards see the
    // series (it just stays 0 off-Linux).
    LazyLock::force(&PROCESS_MEM_GAUGES);
    #[cfg(target_os = "linux")]
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        let kb = |key: &str| -> Option<i64> {
            let line = status.lines().find(|l| l.starts_with(key))?;
            line.split_whitespace().nth(1)?.parse::<i64>().ok()
        };
        if let Some(rss_kb) = kb("VmRSS:") {
            PROCESS_MEM_GAUGES[0].set(rss_kb * 1024);
        }
        if let Some(swap_kb) = kb("VmSwap:") {
            PROCESS_MEM_GAUGES[1].set(swap_kb * 1024);
        }
    }

    #[cfg(not(target_env = "msvc"))]
    {
        use tikv_jemalloc_ctl::{epoch, stats};
        // Stats are snapshotted inside jemalloc; advancing the epoch refreshes
        // them before reading.
        if epoch::advance().is_ok() {
            let stats: [Result<usize, _>; 5] = [
                stats::allocated::read(),
                stats::active::read(),
                stats::resident::read(),
                stats::mapped::read(),
                stats::retained::read(),
            ];
            for (gauge, value) in JEMALLOC_GAUGES.iter().zip(stats) {
                if let Ok(v) = value {
                    gauge.set(v as i64);
                }
            }
        }
    }
}

static COLLECTIONS_HEALTHY: LazyLock<IntGauge> = LazyLock::new(|| {
    let gauge = IntGauge::new("collections_healthy", "Collections in ready state").unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

static COLLECTIONS_DEGRADED: LazyLock<IntGauge> = LazyLock::new(|| {
    let gauge = IntGauge::new("collections_degraded", "Collections in degraded state").unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

static COLLECTIONS_FAILED: LazyLock<IntGauge> = LazyLock::new(|| {
    let gauge = IntGauge::new("collections_failed", "Collections in failed state").unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

static HTTP_RESPONSE_BYTES: LazyLock<IntCounterVec> = LazyLock::new(|| {
    let counter = IntCounterVec::new(
        Opts::new(
            "http_response_bytes_total",
            "Total HTTP response body bytes sent",
        ),
        &["method", "path"],
    )
    .unwrap();
    REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

// Tile cache (per-collection).
static TILE_CACHE_HITS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    let counter = IntCounterVec::new(
        Opts::new("tile_cache_hits_total", "GeoTIFF tile cache hits"),
        &["collection"],
    )
    .unwrap();
    REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

static TILE_CACHE_MISSES: LazyLock<IntCounterVec> = LazyLock::new(|| {
    let counter = IntCounterVec::new(
        Opts::new("tile_cache_misses_total", "GeoTIFF tile cache misses"),
        &["collection"],
    )
    .unwrap();
    REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

static TILE_CACHE_BYTES: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    let gauge = IntGaugeVec::new(
        Opts::new(
            "tile_cache_bytes",
            "Bytes currently held in the GeoTIFF tile cache",
        ),
        &["collection"],
    )
    .unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

static TILE_CACHE_CAPACITY_BYTES: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    let gauge = IntGaugeVec::new(
        Opts::new(
            "tile_cache_capacity_bytes",
            "Configured GeoTIFF tile cache capacity in bytes",
        ),
        &["collection"],
    )
    .unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

static TILE_CACHE_ENTRIES: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    let gauge = IntGaugeVec::new(
        Opts::new(
            "tile_cache_entries",
            "Number of entries currently in the GeoTIFF tile cache",
        ),
        &["collection"],
    )
    .unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

// GeoTIFF decoded-chunk cache (#463) — process-global, byte-bounded LRU of
// decoded (native) source tiles for LOCAL files, so the ~50–190 meta-tile
// renders tiling one viewport don't re-decompress the same source tile ~6×
// per frame.
static GEOTIFF_DECODED_CHUNK_CACHE_METRICS: LazyLock<CacheMetricSet> = LazyLock::new(|| {
    CacheMetricSet::new(
        "geotiff_decoded_chunk_cache",
        "GeoTIFF decoded-chunk cache",
        Some("local sources"),
        Some("tile decompressions"),
        false,
    )
});

// Lightning strike-window cache (#504) — process-global, byte-bounded LRU of
// decoded event windows for the events-shape map layer, so the ~50-190
// meta-tile renders tiling one frame share ONE DB fetch.
static LIGHTNING_STRIKE_CACHE_METRICS: LazyLock<CacheMetricSet> = LazyLock::new(|| {
    CacheMetricSet::new(
        "lightning_strike_cache",
        "Lightning strike-window cache",
        Some("events map layers"),
        Some("window DB fetches"),
        false,
    )
});

// PostGIS engine gauges (#110). All set per-scrape from live engine state — no
// delta-tracking needed (they're true gauges, not cumulative counters; the
// per-query duration/rows/error histograms are a follow-up — see the metrics
// block in metrics_handler).
fn pg_int_gauge(name: &str, help: &str, labels: &[&str]) -> IntGaugeVec {
    let gauge = IntGaugeVec::new(Opts::new(name, help), labels).unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
}
fn pg_int_counter(name: &str, help: &str, labels: &[&str]) -> IntCounterVec {
    let counter = IntCounterVec::new(Opts::new(name, help), labels).unwrap();
    REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
}
static POSTGIS_UP: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    pg_int_gauge(
        "postgis_up",
        "1 if the PostGIS collection's DB is reachable (latest 30s SELECT 1 ping), else 0",
        &["collection"],
    )
});
static POSTGIS_POOL_SIZE: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    pg_int_gauge(
        "postgis_pool_size",
        "Currently open/managed connections in the PostGIS pool",
        &["pool_key"],
    )
});
static POSTGIS_POOL_MAX_SIZE: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    pg_int_gauge(
        "postgis_pool_max_size",
        "Configured PostGIS pool capacity (max connections)",
        &["pool_key"],
    )
});
static POSTGIS_POOL_AVAILABLE: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    pg_int_gauge(
        "postgis_pool_available",
        "Connections acquirable now without waiting (open-idle + unallocated slots up to max)",
        &["pool_key"],
    )
});
static POSTGIS_POOL_WAITING: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    pg_int_gauge(
        "postgis_pool_waiting",
        "Tasks waiting for a PostGIS connection",
        &["pool_key"],
    )
});
// Cumulative counts → real Prometheus counters (monotonic across reloads via the
// rebaseline-on-reset delta-tracking in metrics_handler), so `rate()`/`increase()`
// behave; a gauge would saw-tooth to 0 on reload and clamp the rate to 0.
static POSTGIS_METADATA_REFRESHES_TOTAL: LazyLock<IntCounterVec> = LazyLock::new(|| {
    pg_int_counter(
        "postgis_metadata_refreshes_total",
        "Total metadata refreshes",
        &["collection"],
    )
});
static POSTGIS_METADATA_REFRESH_FAILURES_TOTAL: LazyLock<IntCounterVec> = LazyLock::new(|| {
    pg_int_counter(
        "postgis_metadata_refresh_failures_total",
        "Total failed metadata refreshes",
        &["collection"],
    )
});
static POSTGIS_PINGS_TOTAL: LazyLock<IntCounterVec> = LazyLock::new(|| {
    pg_int_counter(
        "postgis_pings_total",
        "Total SELECT 1 health pings (failure ratio = failures_total / pings_total)",
        &["collection"],
    )
});
static POSTGIS_PING_FAILURES_TOTAL: LazyLock<IntCounterVec> = LazyLock::new(|| {
    pg_int_counter(
        "postgis_ping_failures_total",
        "Total failed SELECT 1 health pings",
        &["collection"],
    )
});
static POSTGIS_METADATA_REFRESH_SECONDS: LazyLock<GaugeVec> = LazyLock::new(|| {
    let gauge = GaugeVec::new(
        Opts::new(
            "postgis_metadata_refresh_seconds",
            "Duration of the most recent PostGIS metadata refresh, seconds",
        ),
        &["collection"],
    )
    .unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

// Rendered image cache (global — shared across all collections that render).
static RENDERED_CACHE_METRICS: LazyLock<CacheMetricSet> = LazyLock::new(|| {
    CacheMetricSet::new("rendered_cache", "rendered image cache", None, None, true)
});

// PVOL lazy pixel cache (#289 / PR #290) — global byte-bounded LRU of decoded
// radar moment arrays, shared across every per-site PVOL collection. The
// failures counter surfaces the otherwise-silent degradation when a moment's
// pixels can't be read/decoded at render time (was a hard catalog rejection
// before lazy loading).
static PVOL_PIXEL_CACHE_METRICS: LazyLock<CacheMetricSet> =
    LazyLock::new(|| CacheMetricSet::new("pvol_pixel_cache", "PVOL pixel cache", None, None, true));

static PVOL_PIXEL_CACHE_INSERTS: LazyLock<DeltaCounter> = LazyLock::new(|| {
    DeltaCounter::new(
        "pvol_pixel_cache_inserts_total",
        "PVOL pixel cache inserts (request-time decodes + poll-time pre-warm). \
         Sustained inserts while entries/bytes sit at capacity = LRU eviction \
         churn: the pre-warmed working set exceeds MC_PVOL_PIXEL_CACHE_MB (#476)",
    )
});

static PVOL_PIXEL_READ_FAILURES: LazyLock<DeltaCounter> = LazyLock::new(|| {
    DeltaCounter::new(
        "pvol_pixel_read_failures_total",
        "PVOL lazy pixel reads that failed (I/O or decode) and degraded to nodata",
    )
});

// 3D Tiles encoded-content cache — global, the final `.pnts`/`.glb` bytes per
// (collection, product, quantity, time, params, data-version), so repeats and
// `If-None-Match` revalidations skip the engine read + encode entirely.
static TILES3D_CONTENT_CACHE_METRICS: LazyLock<CacheMetricSet> = LazyLock::new(|| {
    CacheMetricSet::new(
        "tiles3d_content_cache",
        "3D Tiles encoded-content cache",
        None,
        Some("full engine read + encode"),
        false,
    )
});

// PVOL resampled voxel-grid cache — global, the cylindrical grids the 3D Tiles
// mesh products (isosurface / echo-top / voxels) share per (volume, quantity,
// dims) instead of repeating the multi-million-cell polar resample.
static PVOL_VOXEL_GRID_CACHE_METRICS: LazyLock<CacheMetricSet> = LazyLock::new(|| {
    CacheMetricSet::new(
        "pvol_voxel_grid_cache",
        "PVOL voxel-grid cache",
        None,
        Some("full polar resamples"),
        false,
    )
});

// COMP composite cache (#212) — process-global, byte-bounded LRU of decoded
// ODIM composites, so a concurrent full-viewport WMS animation keeps every
// active timestep resident instead of ping-ponging a single slot and
// re-decoding the same (up to 134 MB OPERA) grid many times.
static ODIM_COMPOSITE_CACHE_METRICS: LazyLock<CacheMetricSet> = LazyLock::new(|| {
    CacheMetricSet::new(
        "odim_composite_cache",
        "ODIM COMP composite cache",
        None,
        Some("full HDF5 decodes"),
        false,
    )
});

// Storm-cell segmentation memo (#367) — per-volume `CellSet`s, so an
// animation window / repeated tracking request re-segments only the newest
// volume.
static PVOL_CELL_SET_CACHE_METRICS: LazyLock<CacheMetricSet> = LazyLock::new(|| {
    CacheMetricSet::new(
        "pvol_cell_set_cache",
        "PVOL storm-cell set cache",
        None,
        Some("full segmentations"),
        false,
    )
});

// Meta-tile pixel cache (#202) — global, decoded-RGBA tiles for the Web
// Mercator WMS meta-tiling path. Distinct from the per-collection GeoTIFF
// compressed-byte tile cache (`tile_cache_*`).
static METATILE_CACHE_METRICS: LazyLock<CacheMetricSet> = LazyLock::new(|| {
    CacheMetricSet::new("metatile_cache", "meta-tile pixel cache", None, None, true)
});

static METATILE_DECLINES: LazyLock<DeltaCounter> = LazyLock::new(|| {
    DeltaCounter::new(
        "metatile_declines_total",
        "WMS renders where meta-tiling declined because the covering tile \
         count exceeded the pixel-proportional budget, falling back to an \
         uncached direct render (#491). A sustained rate means clients are \
         sending viewports outside the cacheable envelope (extreme bbox/pixel \
         aspect mismatch) and re-render every frame",
    )
});

// GRIB grid cache (per-collection).
static GRID_CACHE_HITS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    let counter = IntCounterVec::new(
        Opts::new("grid_cache_hits_total", "GRIB grid cache hits"),
        &["collection"],
    )
    .unwrap();
    REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

static GRID_CACHE_MISSES: LazyLock<IntCounterVec> = LazyLock::new(|| {
    let counter = IntCounterVec::new(
        Opts::new("grid_cache_misses_total", "GRIB grid cache misses"),
        &["collection"],
    )
    .unwrap();
    REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

static GRID_CACHE_BYTES: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    let gauge = IntGaugeVec::new(
        Opts::new(
            "grid_cache_bytes",
            "Bytes currently held in the GRIB grid cache",
        ),
        &["collection"],
    )
    .unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

static GRID_CACHE_CAPACITY_BYTES: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    let gauge = IntGaugeVec::new(
        Opts::new(
            "grid_cache_capacity_bytes",
            "Configured GRIB grid cache capacity in bytes",
        ),
        &["collection"],
    )
    .unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

static GRID_CACHE_ENTRIES: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    let gauge = IntGaugeVec::new(
        Opts::new(
            "grid_cache_entries",
            "Number of entries currently in the GRIB grid cache",
        ),
        &["collection"],
    )
    .unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

/// Tracks the last observed (hits, misses) snapshot per (cache_kind, collection)
/// for the **labelled per-collection** cache families, so that the metrics
/// handler can convert cumulative cache counters into monotonically-increasing
/// Prometheus counters via delta tracking. (The unlabelled global caches use
/// [`CacheMetricSet`]/[`DeltaCounter`], which carry their own last-seen state.)
///
/// On collection reload the underlying cache is replaced with a fresh one
/// (hits/misses reset to 0), which we detect as a decrease and treat as the
/// new baseline.
static CACHE_COUNTER_STATE: LazyLock<Mutex<CacheCounterState>> =
    LazyLock::new(|| Mutex::new(CacheCounterState::default()));

#[derive(Default)]
struct CacheCounterState {
    tile: HashMap<String, (u64, u64)>,
    grid: HashMap<String, (u64, u64)>,
    /// PostGIS per-collection `(refreshes, refresh_failures, pings, ping_failures)`
    /// last-scraped values. Engines are replaced on reload (counts reset), so
    /// the scrape rebaselines on a backward step — see `metrics_handler`.
    postgis: HashMap<String, (u64, u64, u64, u64)>,
    /// Nowcast per-collection `(generations, failures)` last-scraped values —
    /// same reload-rebaseline scheme.
    nowcast: HashMap<String, (u64, u64)>,
}

static NOWCAST_GENERATIONS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    let counter = IntCounterVec::new(
        Opts::new(
            "nowcast_generations_total",
            "Nowcast generations produced (one per new source frame)",
        ),
        &["collection"],
    )
    .unwrap();
    REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

static NOWCAST_GENERATION_FAILURES: LazyLock<IntCounterVec> = LazyLock::new(|| {
    let counter = IntCounterVec::new(
        Opts::new(
            "nowcast_generation_failures_total",
            "Nowcast generations that failed (source fetch or extrapolation error)",
        ),
        &["collection"],
    )
    .unwrap();
    REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

static NOWCAST_LAST_GENERATION_MS: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    let gauge = IntGaugeVec::new(
        Opts::new(
            "nowcast_last_generation_duration_ms",
            "Wall-clock duration of the most recent nowcast generation",
        ),
        &["collection"],
    )
    .unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

static NOWCAST_SOURCE_LAG_SECONDS: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    let gauge = IntGaugeVec::new(
        Opts::new(
            "nowcast_source_lag_seconds",
            "Age of the source anchor frame when the last generation finished",
        ),
        &["collection"],
    )
    .unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

static NOWCAST_RETAINED_GENERATIONS: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    let gauge = IntGaugeVec::new(
        Opts::new(
            "nowcast_retained_generations",
            "Generations currently retained (reference_time pinning window)",
        ),
        &["collection"],
    )
    .unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

static NOWCAST_FRAMES: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    let gauge = IntGaugeVec::new(
        Opts::new(
            "nowcast_frames",
            "Frames (analysis + leads) in the latest nowcast generation",
        ),
        &["collection"],
    )
    .unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

static NOWCAST_LEAD1_CSI: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    let gauge = IntGaugeVec::new(
        Opts::new(
            "nowcast_lead1_csi_permille",
            "Realized lead-1 skill: CSI x1000 of the previous generation's \
             prediction scored against the newest analysis frame (#542)",
        ),
        &["collection"],
    )
    .unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

static NOWCAST_LEAD1_PERSISTENCE_CSI: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    let gauge = IntGaugeVec::new(
        Opts::new(
            "nowcast_lead1_persistence_csi_permille",
            "Persistence baseline for nowcast_lead1_csi_permille: CSI x1000 \
             of the previous analysis frame scored against the newest one",
        ),
        &["collection"],
    )
    .unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

static RENDER_SEMAPHORE_AVAILABLE: LazyLock<IntGauge> = LazyLock::new(|| {
    let gauge = IntGauge::new(
        "render_semaphore_available",
        "Available render semaphore permits",
    )
    .unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

static RENDER_SEMAPHORE_TOTAL: LazyLock<IntGauge> = LazyLock::new(|| {
    let gauge = IntGauge::new("render_semaphore_total", "Total render semaphore permits").unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

static STORAGE_BYTES_READ: LazyLock<IntCounterVec> = LazyLock::new(|| {
    let counter = IntCounterVec::new(
        Opts::new(
            "storage_bytes_read_total",
            "Total bytes read from storage by engine",
        ),
        &["collection", "engine_type"],
    )
    .unwrap();
    REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

// ---------------------------------------------------------------------------
// Health types
// ---------------------------------------------------------------------------

#[derive(Clone, Serialize)]
pub struct CollectionHealth {
    pub id: String,
    pub engine_type: String,
    pub status: CollectionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CollectionStatus {
    Ready,
    Degraded,
    Failed,
}

// ---------------------------------------------------------------------------
// Shared server state for admin operations
// ---------------------------------------------------------------------------

pub struct ServerState {
    pub edr: Arc<ArcSwap<EdrState>>,
    pub features: Arc<ArcSwap<FeaturesState>>,
    pub wms: Arc<ArcSwap<WmsState>>,
    pub maps: Arc<ArcSwap<MapsState>>,
    pub tiles: Arc<ArcSwap<TilesState>>,
    pub tiles_3d: Arc<ArcSwap<api_3dtiles::TilesState3d>>,
    pub config_path: String,
    pub health: RwLock<Vec<CollectionHealth>>,
    pub geotiff_engines: RwLock<Vec<Arc<engine_geotiff::GeoTiffEngine>>>,
    pub querydata_engines: RwLock<Vec<Arc<engine_querydata::QueryDataEngine>>>,
    pub grib_engines: RwLock<Vec<Arc<engine_grib::GribEngine>>>,
    pub zarr_engines: RwLock<Vec<Arc<engine_zarr::ZarrEngine>>>,
    pub odim_engines: RwLock<Vec<Arc<engine_odim::OdimEngine>>>,
    pub odim_volume_engines: RwLock<Vec<Arc<engine_odim::PolarVolumeEngine>>>,
    pub cap_engines: RwLock<Vec<Arc<engine_cap::CapEngine>>>,
    pub postgis_engines: RwLock<Vec<Arc<engine_postgis::PostgisEngine>>>,
    pub nowcast_engines: RwLock<Vec<Arc<engine_nowcast::NowcastEngine>>>,
    /// Serializes reload requests to prevent concurrent reloads from racing.
    pub reload_lock: tokio::sync::Mutex<()>,
    /// Bearer token for admin endpoint authentication.
    /// If None, admin endpoints are disabled (return 403).
    pub admin_token: Option<String>,
    /// Fingerprint of all style-affecting config (colormaps, bundles,
    /// per-collection [wms] blocks, colormaps_dir file bytes) at the last
    /// successful load. A reload whose fingerprint differs drops the
    /// rendered/meta-tile caches instead of reusing them — their keys carry
    /// no style content, so reusing them would serve the OLD colors for
    /// every already-cached tile (as verified live: edit palette → reload →
    /// X-Cache: HIT with stale pixels). A spurious watcher reload leaves
    /// the fingerprint unchanged and keeps the warm caches (#202).
    pub style_fingerprint: std::sync::atomic::AtomicU64,
}

pub type AdminState = Arc<ServerState>;

// ---------------------------------------------------------------------------
// Collection loader (used by startup and reload)
// ---------------------------------------------------------------------------

pub struct LoadResult {
    pub edr_state: EdrState,
    pub features_state: FeaturesState,
    pub wms_state: WmsState,
    pub maps_state: MapsState,
    pub tiles_state: TilesState,
    pub tiles_3d_state: api_3dtiles::TilesState3d,
    pub health: Vec<CollectionHealth>,
    pub geotiff_engines: Vec<Arc<engine_geotiff::GeoTiffEngine>>,
    pub querydata_engines: Vec<Arc<engine_querydata::QueryDataEngine>>,
    pub grib_engines: Vec<Arc<engine_grib::GribEngine>>,
    pub zarr_engines: Vec<Arc<engine_zarr::ZarrEngine>>,
    pub odim_engines: Vec<Arc<engine_odim::OdimEngine>>,
    pub odim_volume_engines: Vec<Arc<engine_odim::PolarVolumeEngine>>,
    pub cap_engines: Vec<Arc<engine_cap::CapEngine>>,
    pub postgis_engines: Vec<Arc<engine_postgis::PostgisEngine>>,
    pub nowcast_engines: Vec<Arc<engine_nowcast::NowcastEngine>>,
}

/// Render caches carried across a reload so a config reload **preserves** the
/// warm cache instead of rebuilding it empty. Rebuilding on every reload dumps
/// a fully-warmed meta-tile cache (multiple GB) and forces a cold re-warm — and
/// a spurious `collections_dir` watcher event could trigger that repeatedly,
/// tanking render latency. `Default` (all `None`, used at startup) builds fresh;
/// [`do_reload`] passes the live caches to reuse.
#[derive(Default)]
pub struct ReusableCaches {
    pub rendered: Option<Arc<ds_render::RenderedCache>>,
    pub tile: Option<Arc<ds_render::TilePixelCache>>,
    pub vector: Option<Arc<ds_mvt::VectorTileCache>>,
}

pub fn load_collections(
    style_ctx: &ds_render::StyleContext,
    collections: &[CollectionConfig],
    style_bundles: &[StyleBundle],
    base_url: &str,
    trust_proxy_headers: bool,
    metatile_cache_mb: u64,
    reuse: ReusableCaches,
) -> LoadResult {
    let bundle_index: HashMap<&str, &StyleBundle> =
        style_bundles.iter().map(|b| (b.id.as_str(), b)).collect();
    // The single config→style resolution path (phase 2 of the styling
    // revamp): every API's style map is resolved through the caller's
    // StyleContext (built-ins + user [[colormaps]]), computed once per
    // collection and shared via `styles_cache`.
    let mut styles_cache: HashMap<String, HashMap<String, HashMap<String, ds_render::StyleInfo>>> =
        HashMap::new();
    let mut edr_engines: HashMap<String, Arc<dyn ds_core::edr_engine::EdrEngine>> = HashMap::new();
    let mut edr_collections: HashMap<String, CollectionConfig> = HashMap::new();
    let mut edr_styles: HashMap<String, HashMap<String, ds_render::StyleInfo>> = HashMap::new();
    let mut feature_engines: HashMap<String, Arc<dyn ds_core::feature_engine::FeatureEngine>> =
        HashMap::new();
    let mut feature_collections: HashMap<String, CollectionConfig> = HashMap::new();
    let mut map_engines: HashMap<String, Arc<dyn ds_core::map_engine::MapEngine>> = HashMap::new();
    let mut map_collections: HashMap<String, CollectionConfig> = HashMap::new();
    let mut map_styles: HashMap<String, HashMap<String, ds_render::StyleInfo>> = HashMap::new();
    let mut maps_engines: HashMap<String, Arc<dyn ds_core::map_engine::MapEngine>> = HashMap::new();
    let mut maps_collections: HashMap<String, CollectionConfig> = HashMap::new();
    let mut maps_styles: HashMap<String, HashMap<String, ds_render::StyleInfo>> = HashMap::new();
    let mut tiles_engines: HashMap<String, Arc<dyn ds_core::map_engine::MapEngine>> =
        HashMap::new();
    let mut tiles_collections: HashMap<String, CollectionConfig> = HashMap::new();
    let mut tiles_styles: HashMap<String, HashMap<String, ds_render::StyleInfo>> = HashMap::new();
    // Vector-tile sources: collections with `apis = [..., "tiles"]` whose
    // engine implements `FeatureEngine` end up here, keyed by collection id.
    // Independent of `tiles_engines` (raster), so a collection can serve one,
    // the other, or both.
    let mut tiles_feature_engines: HashMap<
        String,
        Arc<dyn ds_core::feature_engine::FeatureEngine>,
    > = HashMap::new();
    let mut tiles_feature_collections: HashMap<String, CollectionConfig> = HashMap::new();
    // 3D Tiles sources: collections with `apis = [..., "3dtiles"]` whose engine
    // implements `VolumeEngine`, keyed by collection id.
    let mut volume_engines: HashMap<String, Arc<dyn ds_core::volume::VolumeEngine>> =
        HashMap::new();
    let mut volume_collections: HashMap<String, CollectionConfig> = HashMap::new();
    let mut geotiff_engines: Vec<Arc<engine_geotiff::GeoTiffEngine>> = Vec::new();
    let mut querydata_engines: Vec<Arc<engine_querydata::QueryDataEngine>> = Vec::new();
    let mut grib_engines: Vec<Arc<engine_grib::GribEngine>> = Vec::new();
    let mut zarr_engines: Vec<Arc<engine_zarr::ZarrEngine>> = Vec::new();
    let mut odim_engines: Vec<Arc<engine_odim::OdimEngine>> = Vec::new();
    let mut odim_volume_engines: Vec<Arc<engine_odim::PolarVolumeEngine>> = Vec::new();
    let mut cap_engines: Vec<Arc<engine_cap::CapEngine>> = Vec::new();
    let mut postgis_engines: Vec<Arc<engine_postgis::PostgisEngine>> = Vec::new();
    let mut nowcast_engines: Vec<Arc<engine_nowcast::NowcastEngine>> = Vec::new();
    // Point-event sources (#549): events-shape postgis collections, keyed
    // by collection id — the nowcast second pass looks up its
    // `lightning_source` here.
    let mut event_sources: HashMap<String, Arc<dyn ds_core::events::EventSource>> = HashMap::new();
    // Derived collections wire in a second pass, after every base engine
    // exists (#522). Collected here during the main loop.
    let mut nowcast_pending: Vec<&CollectionConfig> = Vec::new();
    // Pool registry is local to this load: collections sharing a DSN share a
    // pool via Arc<Pool>. Across reloads, pools are rebuilt — documented
    // v1 trade-off; reuse-by-identity across reloads is a follow-up.
    let mut pool_registry = engine_postgis::pool::PoolRegistry::new();
    let mut health: Vec<CollectionHealth> = Vec::new();

    for collection in collections {
        let data_path_display = collection
            .data_path
            .as_deref()
            .unwrap_or("<configured in engine>");
        info!(
            "Loading collection '{}' ({}) from {}",
            collection.id, collection.engine_type, data_path_display
        );

        // Validate engine+API compatibility
        let supported_apis: &[&str] = match collection.engine_type.as_str() {
            "csv" => &["edr", "features"],
            "geojson" => &["features", "tiles"],
            "geotiff" => &["edr", "wms", "maps", "tiles"],
            "querydata" => &["edr", "wms", "maps", "tiles"],
            "grib" => &["edr", "wms", "maps", "tiles"],
            "zarr" => &["edr", "wms", "maps", "tiles"],
            "odim" => &["edr", "wms", "maps", "tiles"],
            "odim-volume" => &["edr", "wms", "maps", "tiles", "3dtiles", "features"],
            "cap" => &["features", "wms", "maps", "tiles"],
            "postgis" => &["edr", "features", "tiles", "wms", "maps"],
            "nowcast" => &["wms", "maps", "tiles", "features"],
            _ => &[],
        };
        let unsupported: Vec<&str> = collection
            .apis
            .iter()
            .map(|s| s.as_str())
            .filter(|api| !supported_apis.contains(api))
            .collect();
        if !unsupported.is_empty() {
            tracing::error!(
                "Collection '{}': engine '{}' does not support APIs {:?}, skipping collection",
                collection.id,
                collection.engine_type,
                unsupported
            );
            health.push(CollectionHealth {
                id: collection.id.clone(),
                engine_type: collection.engine_type.clone(),
                status: CollectionStatus::Failed,
                error: Some(format!(
                    "engine '{}' does not support APIs: {:?}",
                    collection.engine_type, unsupported
                )),
            });
            continue;
        }

        match collection.engine_type.as_str() {
            "csv" => {
                let data_path = match collection.data_path.as_deref() {
                    Some(p) => p,
                    None => {
                        tracing::error!(
                            "Collection '{}': csv engine requires data_path, skipping",
                            collection.id
                        );
                        health.push(CollectionHealth {
                            id: collection.id.clone(),
                            engine_type: "csv".into(),
                            status: CollectionStatus::Failed,
                            error: Some("csv engine requires data_path".into()),
                        });
                        continue;
                    }
                };
                let store = match engine_csv::CsvDataStore::load(data_path) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!(
                            "Collection '{}': failed to load CSV from {}: {}",
                            collection.id,
                            data_path,
                            e
                        );
                        health.push(CollectionHealth {
                            id: collection.id.clone(),
                            engine_type: "csv".into(),
                            status: CollectionStatus::Failed,
                            error: Some(format!("{e}")),
                        });
                        continue;
                    }
                };

                info!(
                    "Loaded {} rows, {} locations, {} parameters",
                    store.rows.len(),
                    store.location_index.len(),
                    store.parameter_names.len()
                );

                let engine = Arc::new(engine_csv::CsvEngine::new(store));

                if collection.apis.contains(&"edr".to_string()) {
                    edr_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::edr_engine::EdrEngine>,
                    );
                    edr_collections.insert(collection.id.clone(), collection.clone());
                }
                if collection.apis.contains(&"features".to_string()) {
                    feature_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::feature_engine::FeatureEngine>,
                    );
                    feature_collections.insert(collection.id.clone(), collection.clone());
                }
                health.push(CollectionHealth {
                    id: collection.id.clone(),
                    engine_type: "csv".into(),
                    status: CollectionStatus::Ready,
                    error: None,
                });
            }
            "geojson" => {
                let data_path = match collection.data_path.as_deref() {
                    Some(p) => p,
                    None => {
                        tracing::error!(
                            "Collection '{}': geojson engine requires data_path, skipping",
                            collection.id
                        );
                        health.push(CollectionHealth {
                            id: collection.id.clone(),
                            engine_type: "geojson".into(),
                            status: CollectionStatus::Failed,
                            error: Some("geojson engine requires data_path".into()),
                        });
                        continue;
                    }
                };
                let engine = match engine_geojson::GeoJsonEngine::load(data_path) {
                    Ok(e) => Arc::new(e),
                    Err(e) => {
                        tracing::error!(
                            "Collection '{}': failed to load GeoJSON from {}: {}",
                            collection.id,
                            data_path,
                            e
                        );
                        health.push(CollectionHealth {
                            id: collection.id.clone(),
                            engine_type: "geojson".into(),
                            status: CollectionStatus::Failed,
                            error: Some(format!("{e}")),
                        });
                        continue;
                    }
                };

                info!(
                    "Loaded {} features, extent: {:?}",
                    engine.feature_count(),
                    engine.spatial_extent()
                );

                if collection.apis.contains(&"features".to_string()) {
                    feature_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::feature_engine::FeatureEngine>,
                    );
                    feature_collections.insert(collection.id.clone(), collection.clone());
                }
                if collection.apis.contains(&"tiles".to_string()) {
                    tiles_feature_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::feature_engine::FeatureEngine>,
                    );
                    tiles_feature_collections.insert(collection.id.clone(), collection.clone());
                }
                health.push(CollectionHealth {
                    id: collection.id.clone(),
                    engine_type: "geojson".into(),
                    status: CollectionStatus::Ready,
                    error: None,
                });
            }
            "geotiff" => {
                let geotiff_config = match collection.geotiff.as_ref() {
                    Some(c) => c,
                    None => {
                        tracing::error!(
                            "Collection '{}': engine_type 'geotiff' but missing [collections.geotiff] config, skipping",
                            collection.id
                        );
                        health.push(CollectionHealth {
                            id: collection.id.clone(),
                            engine_type: "geotiff".into(),
                            status: CollectionStatus::Failed,
                            error: Some("missing [collections.geotiff] config".into()),
                        });
                        continue;
                    }
                };

                let engine = match engine_geotiff::GeoTiffEngine::new(
                    &collection.id,
                    collection.data_path.as_deref(),
                    geotiff_config,
                ) {
                    Ok(e) => Arc::new(e),
                    Err(e) => {
                        tracing::error!(
                            "Collection '{}': failed to initialize GeoTIFF engine: {}",
                            collection.id,
                            e
                        );
                        health.push(CollectionHealth {
                            id: collection.id.clone(),
                            engine_type: "geotiff".into(),
                            status: CollectionStatus::Failed,
                            error: Some(format!("{e}")),
                        });
                        continue;
                    }
                };

                if let Some((start, end)) =
                    ds_core::edr_engine::EdrEngine::get_temporal_extent(engine.as_ref())
                {
                    info!(
                        "Collection '{}': temporal extent {} to {}",
                        collection.id, start, end
                    );
                }

                geotiff_engines.push(engine.clone());

                if collection.apis.contains(&"edr".to_string()) {
                    edr_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::edr_engine::EdrEngine>,
                    );
                    edr_collections.insert(collection.id.clone(), collection.clone());
                    edr_styles.extend(collection_layer_styles(
                        style_ctx,
                        &mut styles_cache,
                        collection,
                        &[],
                        &bundle_index,
                    ));
                }
                if collection.apis.contains(&"wms".to_string()) {
                    map_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    map_collections.insert(collection.id.clone(), collection.clone());

                    map_styles.extend(collection_layer_styles(
                        style_ctx,
                        &mut styles_cache,
                        collection,
                        &[],
                        &bundle_index,
                    ));

                    info!("Collection '{}': wired to WMS API", collection.id);
                }
                if collection.apis.contains(&"maps".to_string()) {
                    maps_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    maps_collections.insert(collection.id.clone(), collection.clone());

                    maps_styles.extend(collection_layer_styles(
                        style_ctx,
                        &mut styles_cache,
                        collection,
                        &[],
                        &bundle_index,
                    ));

                    info!("Collection '{}': wired to Maps API", collection.id);
                }
                if collection.apis.contains(&"tiles".to_string()) {
                    tiles_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    tiles_collections.insert(collection.id.clone(), collection.clone());

                    tiles_styles.extend(collection_layer_styles(
                        style_ctx,
                        &mut styles_cache,
                        collection,
                        &[],
                        &bundle_index,
                    ));

                    info!("Collection '{}': wired to Tiles API", collection.id);
                }

                // GeoTIFF starts degraded (no data yet until first poll), unless
                // the initial scan already found files.
                let has_data =
                    ds_core::edr_engine::EdrEngine::get_temporal_extent(engine.as_ref()).is_some();
                health.push(CollectionHealth {
                    id: collection.id.clone(),
                    engine_type: "geotiff".into(),
                    status: if has_data {
                        CollectionStatus::Ready
                    } else {
                        CollectionStatus::Degraded
                    },
                    error: if has_data {
                        None
                    } else {
                        Some("no data files found yet (waiting for poll)".into())
                    },
                });
            }
            "querydata" => {
                let data_path = match collection.data_path.as_deref() {
                    Some(p) => p,
                    None => {
                        tracing::error!(
                            "Collection '{}': engine_type 'querydata' requires data_path, skipping",
                            collection.id
                        );
                        health.push(CollectionHealth {
                            id: collection.id.clone(),
                            engine_type: "querydata".into(),
                            status: CollectionStatus::Failed,
                            error: Some("missing data_path".into()),
                        });
                        continue;
                    }
                };

                let qd_config = collection.querydata.as_ref();
                let wms_param = qd_config.and_then(|c| c.wms_parameter.as_deref());
                let poll_secs = qd_config.map_or(30, |c| c.poll_interval_secs);
                let max_runs = qd_config.map_or(4, |c| c.max_runs);

                let engine = match engine_querydata::QueryDataEngine::new(
                    std::path::Path::new(data_path),
                    &collection.id,
                    wms_param,
                    poll_secs,
                    max_runs,
                ) {
                    Ok(e) => Arc::new(e),
                    Err(e) => {
                        tracing::error!(
                            "Collection '{}': failed to initialize QueryData engine: {}",
                            collection.id,
                            e
                        );
                        health.push(CollectionHealth {
                            id: collection.id.clone(),
                            engine_type: "querydata".into(),
                            status: CollectionStatus::Failed,
                            error: Some(format!("{e}")),
                        });
                        continue;
                    }
                };

                querydata_engines.push(engine.clone());

                // Get parameter list for per-parameter-layer styles
                let raster_params =
                    ds_core::map_engine::MapEngine::raster_info(engine.as_ref()).parameters;

                if collection.apis.contains(&"edr".to_string()) {
                    edr_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::edr_engine::EdrEngine>,
                    );
                    edr_collections.insert(collection.id.clone(), collection.clone());
                    edr_styles.extend(collection_layer_styles(
                        style_ctx,
                        &mut styles_cache,
                        collection,
                        &raster_params,
                        &bundle_index,
                    ));
                }

                if collection.apis.contains(&"wms".to_string()) {
                    map_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    map_collections.insert(collection.id.clone(), collection.clone());
                    map_styles.extend(collection_layer_styles(
                        style_ctx,
                        &mut styles_cache,
                        collection,
                        &raster_params,
                        &bundle_index,
                    ));
                    info!("Collection '{}': wired to WMS API", collection.id);
                }
                if collection.apis.contains(&"maps".to_string()) {
                    maps_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    maps_collections.insert(collection.id.clone(), collection.clone());
                    maps_styles.extend(collection_layer_styles(
                        style_ctx,
                        &mut styles_cache,
                        collection,
                        &raster_params,
                        &bundle_index,
                    ));
                    info!("Collection '{}': wired to Maps API", collection.id);
                }
                if collection.apis.contains(&"tiles".to_string()) {
                    tiles_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    tiles_collections.insert(collection.id.clone(), collection.clone());
                    tiles_styles.extend(collection_layer_styles(
                        style_ctx,
                        &mut styles_cache,
                        collection,
                        &raster_params,
                        &bundle_index,
                    ));
                    info!("Collection '{}': wired to Tiles API", collection.id);
                }

                let has_data = engine.has_data();
                health.push(CollectionHealth {
                    id: collection.id.clone(),
                    engine_type: "querydata".into(),
                    status: if has_data {
                        CollectionStatus::Ready
                    } else {
                        CollectionStatus::Degraded
                    },
                    error: if has_data {
                        None
                    } else {
                        Some("no .sqd files found yet (waiting for poll)".into())
                    },
                });
            }
            "grib" => {
                let grib_config = match collection.grib.as_ref() {
                    Some(c) => c,
                    None => {
                        tracing::error!(
                            "Collection '{}': engine_type 'grib' but missing [collections.grib] config, skipping",
                            collection.id
                        );
                        health.push(CollectionHealth {
                            id: collection.id.clone(),
                            engine_type: "grib".into(),
                            status: CollectionStatus::Failed,
                            error: Some("missing [collections.grib] config".into()),
                        });
                        continue;
                    }
                };

                let engine = match engine_grib::GribEngine::new(&collection.id, grib_config) {
                    Ok(e) => Arc::new(e),
                    Err(e) => {
                        tracing::error!(
                            "Collection '{}': failed to initialize GRIB engine: {}",
                            collection.id,
                            e
                        );
                        health.push(CollectionHealth {
                            id: collection.id.clone(),
                            engine_type: "grib".into(),
                            status: CollectionStatus::Failed,
                            error: Some(format!("{e}")),
                        });
                        continue;
                    }
                };

                if let Some((start, end)) =
                    ds_core::edr_engine::EdrEngine::get_temporal_extent(engine.as_ref())
                {
                    info!(
                        "Collection '{}': temporal extent {} to {}",
                        collection.id, start, end
                    );
                }

                grib_engines.push(engine.clone());

                // Get parameter list for per-parameter-layer styles
                let raster_params =
                    ds_core::map_engine::MapEngine::raster_info(engine.as_ref()).parameters;

                if collection.apis.contains(&"edr".to_string()) {
                    edr_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::edr_engine::EdrEngine>,
                    );
                    edr_collections.insert(collection.id.clone(), collection.clone());
                    edr_styles.extend(collection_layer_styles(
                        style_ctx,
                        &mut styles_cache,
                        collection,
                        &raster_params,
                        &bundle_index,
                    ));
                }

                if collection.apis.contains(&"wms".to_string()) {
                    map_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    map_collections.insert(collection.id.clone(), collection.clone());
                    map_styles.extend(collection_layer_styles(
                        style_ctx,
                        &mut styles_cache,
                        collection,
                        &raster_params,
                        &bundle_index,
                    ));
                    info!("Collection '{}': wired to WMS API", collection.id);
                }
                if collection.apis.contains(&"maps".to_string()) {
                    maps_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    maps_collections.insert(collection.id.clone(), collection.clone());
                    maps_styles.extend(collection_layer_styles(
                        style_ctx,
                        &mut styles_cache,
                        collection,
                        &raster_params,
                        &bundle_index,
                    ));
                    info!("Collection '{}': wired to Maps API", collection.id);
                }
                if collection.apis.contains(&"tiles".to_string()) {
                    tiles_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    tiles_collections.insert(collection.id.clone(), collection.clone());
                    tiles_styles.extend(collection_layer_styles(
                        style_ctx,
                        &mut styles_cache,
                        collection,
                        &raster_params,
                        &bundle_index,
                    ));
                    info!("Collection '{}': wired to Tiles API", collection.id);
                }

                let has_data =
                    ds_core::edr_engine::EdrEngine::get_temporal_extent(engine.as_ref()).is_some();
                health.push(CollectionHealth {
                    id: collection.id.clone(),
                    engine_type: "grib".into(),
                    status: if has_data {
                        CollectionStatus::Ready
                    } else {
                        CollectionStatus::Degraded
                    },
                    error: if has_data {
                        None
                    } else {
                        Some("no forecast data found yet (waiting for poll)".into())
                    },
                });
            }
            "zarr" => {
                let zarr_config = match collection.zarr.as_ref() {
                    Some(c) => c,
                    None => {
                        tracing::error!(
                            "Collection '{}': engine_type 'zarr' but missing [collections.zarr] config, skipping",
                            collection.id
                        );
                        health.push(CollectionHealth {
                            id: collection.id.clone(),
                            engine_type: "zarr".into(),
                            status: CollectionStatus::Failed,
                            error: Some("missing [collections.zarr] config".into()),
                        });
                        continue;
                    }
                };

                let engine = match engine_zarr::ZarrEngine::new(&collection.id, zarr_config) {
                    Ok(e) => Arc::new(e),
                    Err(e) => {
                        tracing::error!(
                            "Collection '{}': failed to initialize Zarr engine: {}",
                            collection.id,
                            e
                        );
                        health.push(CollectionHealth {
                            id: collection.id.clone(),
                            engine_type: "zarr".into(),
                            status: CollectionStatus::Failed,
                            error: Some(format!("{e}")),
                        });
                        continue;
                    }
                };

                let temporal_extent =
                    ds_core::edr_engine::EdrEngine::get_temporal_extent(engine.as_ref());
                if let Some((start, end)) = temporal_extent {
                    info!(
                        "Collection '{}': temporal extent {} to {}",
                        collection.id, start, end
                    );
                }

                zarr_engines.push(engine.clone());

                // Per-parameter-layer styles (one WMS/Maps/Tiles layer per Zarr
                // variable).
                let raster_params =
                    ds_core::map_engine::MapEngine::raster_info(engine.as_ref()).parameters;

                if collection.apis.contains(&"edr".to_string()) {
                    edr_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::edr_engine::EdrEngine>,
                    );
                    edr_collections.insert(collection.id.clone(), collection.clone());
                    edr_styles.extend(collection_layer_styles(
                        style_ctx,
                        &mut styles_cache,
                        collection,
                        &raster_params,
                        &bundle_index,
                    ));
                    info!("Collection '{}': wired to EDR API", collection.id);
                }

                if collection.apis.contains(&"wms".to_string()) {
                    map_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    map_collections.insert(collection.id.clone(), collection.clone());
                    map_styles.extend(collection_layer_styles(
                        style_ctx,
                        &mut styles_cache,
                        collection,
                        &raster_params,
                        &bundle_index,
                    ));
                    info!("Collection '{}': wired to WMS API", collection.id);
                }
                if collection.apis.contains(&"maps".to_string()) {
                    maps_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    maps_collections.insert(collection.id.clone(), collection.clone());
                    maps_styles.extend(collection_layer_styles(
                        style_ctx,
                        &mut styles_cache,
                        collection,
                        &raster_params,
                        &bundle_index,
                    ));
                    info!("Collection '{}': wired to Maps API", collection.id);
                }
                if collection.apis.contains(&"tiles".to_string()) {
                    tiles_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    tiles_collections.insert(collection.id.clone(), collection.clone());
                    tiles_styles.extend(collection_layer_styles(
                        style_ctx,
                        &mut styles_cache,
                        collection,
                        &raster_params,
                        &bundle_index,
                    ));
                    info!("Collection '{}': wired to Tiles API", collection.id);
                }

                let has_data = temporal_extent.is_some();
                health.push(CollectionHealth {
                    id: collection.id.clone(),
                    engine_type: "zarr".into(),
                    status: if has_data {
                        CollectionStatus::Ready
                    } else {
                        CollectionStatus::Degraded
                    },
                    error: if has_data {
                        None
                    } else {
                        Some("no Zarr data found yet (waiting for poll)".into())
                    },
                });
            }
            "odim" => {
                let odim_cfg = match collection.odim.as_ref() {
                    Some(c) => c,
                    None => {
                        tracing::error!(
                            "Collection '{}': engine_type 'odim' but missing [collections.odim] config, skipping",
                            collection.id
                        );
                        health.push(CollectionHealth {
                            id: collection.id.clone(),
                            engine_type: "odim".into(),
                            status: CollectionStatus::Failed,
                            error: Some("missing [collections.odim] config".into()),
                        });
                        continue;
                    }
                };
                let engine = match engine_odim::OdimEngine::new(
                    &collection.id,
                    collection.data_path.as_deref(),
                    odim_cfg,
                ) {
                    Ok(e) => Arc::new(e),
                    Err(e) => {
                        tracing::error!(
                            "Collection '{}': failed to initialize ODIM engine: {}",
                            collection.id,
                            e
                        );
                        health.push(CollectionHealth {
                            id: collection.id.clone(),
                            engine_type: "odim".into(),
                            status: CollectionStatus::Failed,
                            error: Some(format!("{e}")),
                        });
                        continue;
                    }
                };

                odim_engines.push(engine.clone());

                let raster_params =
                    ds_core::map_engine::MapEngine::raster_info(engine.as_ref()).parameters;

                if collection.apis.contains(&"edr".to_string()) {
                    edr_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::edr_engine::EdrEngine>,
                    );
                    edr_collections.insert(collection.id.clone(), collection.clone());
                    edr_styles.extend(collection_layer_styles(
                        style_ctx,
                        &mut styles_cache,
                        collection,
                        &raster_params,
                        &bundle_index,
                    ));
                    info!("Collection '{}': wired to EDR API", collection.id);
                }

                if collection.apis.contains(&"wms".to_string()) {
                    map_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    map_collections.insert(collection.id.clone(), collection.clone());
                    map_styles.extend(collection_layer_styles(
                        style_ctx,
                        &mut styles_cache,
                        collection,
                        &raster_params,
                        &bundle_index,
                    ));
                    info!("Collection '{}': wired to WMS API", collection.id);
                }
                if collection.apis.contains(&"maps".to_string()) {
                    maps_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    maps_collections.insert(collection.id.clone(), collection.clone());
                    maps_styles.extend(collection_layer_styles(
                        style_ctx,
                        &mut styles_cache,
                        collection,
                        &raster_params,
                        &bundle_index,
                    ));
                    info!("Collection '{}': wired to Maps API", collection.id);
                }
                if collection.apis.contains(&"tiles".to_string()) {
                    tiles_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    tiles_collections.insert(collection.id.clone(), collection.clone());
                    tiles_styles.extend(collection_layer_styles(
                        style_ctx,
                        &mut styles_cache,
                        collection,
                        &raster_params,
                        &bundle_index,
                    ));
                    info!("Collection '{}': wired to Tiles API", collection.id);
                }

                health.push(CollectionHealth {
                    id: collection.id.clone(),
                    engine_type: "odim".into(),
                    status: CollectionStatus::Ready,
                    error: None,
                });
            }
            "odim-volume" => {
                let odim_cfg = match collection.odim.as_ref() {
                    Some(c) => c,
                    None => {
                        tracing::error!(
                            "Collection '{}': engine_type 'odim-volume' but missing [collections.odim] config, skipping",
                            collection.id
                        );
                        health.push(CollectionHealth {
                            id: collection.id.clone(),
                            engine_type: "odim-volume".into(),
                            status: CollectionStatus::Failed,
                            error: Some("missing [collections.odim] config".into()),
                        });
                        continue;
                    }
                };
                let engine = match engine_odim::PolarVolumeEngine::new(
                    &collection.id,
                    collection.data_path.as_deref(),
                    odim_cfg,
                ) {
                    Ok(e) => Arc::new(e),
                    Err(e) => {
                        tracing::error!(
                            "Collection '{}': failed to initialize ODIM polar-volume engine: {}",
                            collection.id,
                            e
                        );
                        health.push(CollectionHealth {
                            id: collection.id.clone(),
                            engine_type: "odim-volume".into(),
                            status: CollectionStatus::Failed,
                            error: Some(format!("{e}")),
                        });
                        continue;
                    }
                };

                odim_volume_engines.push(engine.clone());

                // One PVOL source expands into N per-site OGC
                // collections — one per radar `nod`, parameter = bare
                // quantity. The owning engine keeps the single scan, parse
                // cache, and poll loop; each site gets a cheap
                // `PolarVolumeSiteView` over the shared catalog. The site
                // set is the catalog snapshot taken by `new`'s synchronous
                // scan; sites that appear later surface on the next reload
                // (which re-runs this expansion).
                // `(nod, label)` from one snapshot — enumerates only sites
                // with usable metadata, so no empty/broken collection is
                // registered, and the id+title stay consistent if a poll
                // swaps the catalog mid-registration.
                let sites = engine.sites();
                if sites.is_empty() {
                    // No sites yet (empty/not-yet-populated source). Register
                    // nothing but DO push a `Degraded` health entry so the
                    // server still boots and waits for the first poll —
                    // matching the geotiff/querydata/grib "no data yet"
                    // convention. The startup guard (main.rs) counts
                    // `status != Failed`, so without this entry an all-empty
                    // PVOL deployment would `exit(1)` on boot. The *reload*
                    // guard counts `status == Ready` instead, so this
                    // placeholder does NOT let a transient empty scan replace
                    // a working registry — see `reload_handler`.
                    tracing::warn!(
                        "Collection '{}': PVOL source has no radar sites yet — no per-site \
                         collections registered. Reload once volume files arrive.",
                        collection.id
                    );
                    health.push(CollectionHealth {
                        id: collection.id.clone(),
                        engine_type: "odim-volume".into(),
                        status: CollectionStatus::Degraded,
                        error: Some("no radar sites found yet (waiting for .h5 files)".into()),
                    });
                }
                for (nod, label) in &sites {
                    let site_id = format!("{}-{}", collection.id, nod);

                    // Defence-in-depth against a derived-id collision. NODs
                    // are alphanumeric (so two odim-volume sources can't
                    // derive the same id), but an *inline* `[[collections]]`
                    // entry could be named `{base}-{nod}` by hand. Skip with
                    // an error rather than silently overwriting a registry
                    // entry. NOTE: per-quantity WMS/Maps styles are
                    // snapshotted from `raster_info()` at load — if a poll
                    // later brings in a *new* moment for a site, its layer
                    // falls back to the collection default colormap until the
                    // next `POST /admin/collections/reload` (same load-time
                    // snapshot as GeoTIFF/QueryData; PVOL moment sets are
                    // firmware-dependent, so a reload is required when they
                    // change).
                    if edr_collections.contains_key(&site_id)
                        || map_collections.contains_key(&site_id)
                        || maps_collections.contains_key(&site_id)
                        || tiles_collections.contains_key(&site_id)
                    {
                        tracing::error!(
                            "Collection '{}': per-site id '{site_id}' collides with an \
                             already-registered collection — skipping this site",
                            collection.id
                        );
                        health.push(CollectionHealth {
                            id: site_id,
                            engine_type: "odim-volume".into(),
                            status: CollectionStatus::Failed,
                            error: Some(
                                "derived id collides with an already-registered collection".into(),
                            ),
                        });
                        continue;
                    }

                    let view = Arc::new(engine.site_view(nod, &site_id));

                    // Per-site collection config: inherit the base
                    // (`apis`, `[wms]` styling, …) and override identity.
                    let mut site_cfg = collection.clone();
                    site_cfg.id = site_id.clone();
                    site_cfg.title = format!("{} — {label}", collection.title);
                    site_cfg.description =
                        format!("{} (radar site {label} / {nod})", collection.description);

                    // Per-site, multi-parameter: one layer per bare quantity.
                    let raster_params =
                        ds_core::map_engine::MapEngine::raster_info(view.as_ref()).parameters;

                    if collection.apis.contains(&"edr".to_string()) {
                        edr_engines.insert(
                            site_id.clone(),
                            view.clone() as Arc<dyn ds_core::edr_engine::EdrEngine>,
                        );
                        edr_collections.insert(site_id.clone(), site_cfg.clone());
                        edr_styles.extend(collection_layer_styles(
                            style_ctx,
                            &mut styles_cache,
                            &site_cfg,
                            &raster_params,
                            &bundle_index,
                        ));
                    }

                    if collection.apis.contains(&"wms".to_string()) {
                        map_engines.insert(
                            site_id.clone(),
                            view.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                        );
                        map_collections.insert(site_id.clone(), site_cfg.clone());
                        map_styles.extend(collection_layer_styles(
                            style_ctx,
                            &mut styles_cache,
                            &site_cfg,
                            &raster_params,
                            &bundle_index,
                        ));
                    }
                    if collection.apis.contains(&"maps".to_string()) {
                        maps_engines.insert(
                            site_id.clone(),
                            view.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                        );
                        maps_collections.insert(site_id.clone(), site_cfg.clone());
                        maps_styles.extend(collection_layer_styles(
                            style_ctx,
                            &mut styles_cache,
                            &site_cfg,
                            &raster_params,
                            &bundle_index,
                        ));
                    }
                    if collection.apis.contains(&"tiles".to_string()) {
                        tiles_engines.insert(
                            site_id.clone(),
                            view.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                        );
                        tiles_collections.insert(site_id.clone(), site_cfg.clone());
                        tiles_styles.extend(collection_layer_styles(
                            style_ctx,
                            &mut styles_cache,
                            &site_cfg,
                            &raster_params,
                            &bundle_index,
                        ));
                    }
                    if collection.apis.contains(&"3dtiles".to_string()) {
                        volume_engines.insert(
                            site_id.clone(),
                            view.clone() as Arc<dyn ds_core::volume::VolumeEngine>,
                        );
                        volume_collections.insert(site_id.clone(), site_cfg.clone());
                    }

                    info!(
                        "Collection '{}': wired per-site radar collection '{site_id}' ({label})",
                        collection.id
                    );
                    health.push(CollectionHealth {
                        id: site_id,
                        engine_type: "odim-volume".into(),
                        status: CollectionStatus::Ready,
                        error: None,
                    });
                }

                // Network site inventory as an OGC API - Features collection
                // under the (model-B-freed) base id: the owning engine — not
                // the per-site views — implements `FeatureEngine`, projecting
                // its shared `by_site_meta` into one Point Feature per site.
                // Registered whenever `apis` includes "features", regardless of
                // site count (an empty inventory is a valid empty
                // FeatureCollection, unlike EDR's empty PointSeries). The base
                // config supplies the collection title/description (the network
                // name); the per-site EDR/WMS/etc. collections use `{base}-{nod}`,
                // so the base id is free of those registries.
                if collection.apis.contains(&"features".to_string()) {
                    // Defence-in-depth: the base id must not already name a
                    // collection in ANY registry (e.g. a hand-written inline
                    // collection, or a `{base}-{nod}` per-site id derived from
                    // another odim-volume source). Mirrors the per-site guard
                    // above so the same id can't mean different things across
                    // the EDR / Map / Features services.
                    if feature_collections.contains_key(&collection.id)
                        || edr_collections.contains_key(&collection.id)
                        || map_collections.contains_key(&collection.id)
                        || maps_collections.contains_key(&collection.id)
                        || tiles_collections.contains_key(&collection.id)
                        || tiles_feature_collections.contains_key(&collection.id)
                    {
                        // Log only — no health entry. The id already belongs to
                        // whatever registered it first (which pushed its own
                        // entry), so a second, contradictory `Failed` entry for
                        // the same id would corrupt `/health` and trip "any
                        // Failed" alerts. Only the network-level Features
                        // inventory is skipped; the per-site EDR/WMS/Maps/Tiles
                        // collections registered above are unaffected.
                        tracing::error!(
                            "Collection '{}': base id already registered as another collection \
                             — skipping only the radar-site Features inventory; the per-site \
                             collections are unaffected",
                            collection.id
                        );
                    } else {
                        feature_engines.insert(
                            collection.id.clone(),
                            engine.clone() as Arc<dyn ds_core::feature_engine::FeatureEngine>,
                        );
                        feature_collections.insert(collection.id.clone(), collection.clone());
                        info!(
                            "Collection '{}': wired radar-site inventory Features collection ({} site(s))",
                            collection.id,
                            sites.len()
                        );
                        // Only mark Ready when there are sites: the empty-source
                        // case already pushed a `Degraded` entry for this id
                        // above, so a second entry would contradict it. The
                        // inventory is still registered (an empty
                        // FeatureCollection is valid) and fills on the next poll.
                        if !sites.is_empty() {
                            health.push(CollectionHealth {
                                id: collection.id.clone(),
                                engine_type: "odim-volume".into(),
                                status: CollectionStatus::Ready,
                                error: None,
                            });
                        }
                    }
                }
            }
            "cap" => {
                let cap_config = match collection.cap.as_ref() {
                    Some(c) => c,
                    None => {
                        tracing::error!(
                            "Collection '{}': engine_type 'cap' but missing [collections.cap] config, skipping",
                            collection.id
                        );
                        health.push(CollectionHealth {
                            id: collection.id.clone(),
                            engine_type: "cap".into(),
                            status: CollectionStatus::Failed,
                            error: Some("missing [collections.cap] config".into()),
                        });
                        continue;
                    }
                };

                let engine = match engine_cap::CapEngine::new(cap_config, &collection.id) {
                    Ok(e) => Arc::new(e),
                    Err(e) => {
                        tracing::error!(
                            "Collection '{}': failed to initialize CAP engine: {}",
                            collection.id,
                            e
                        );
                        health.push(CollectionHealth {
                            id: collection.id.clone(),
                            engine_type: "cap".into(),
                            status: CollectionStatus::Failed,
                            error: Some(format!("{e}")),
                        });
                        continue;
                    }
                };

                cap_engines.push(engine.clone());

                if collection.apis.contains(&"features".to_string()) {
                    feature_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::feature_engine::FeatureEngine>,
                    );
                    feature_collections.insert(collection.id.clone(), collection.clone());
                    info!("Collection '{}': wired to Features API", collection.id);
                }
                if collection.apis.contains(&"wms".to_string()) {
                    map_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    map_collections.insert(collection.id.clone(), collection.clone());
                    map_styles.extend(collection_layer_styles(
                        style_ctx,
                        &mut styles_cache,
                        collection,
                        &[],
                        &bundle_index,
                    ));
                    info!("Collection '{}': wired to WMS API", collection.id);
                }
                if collection.apis.contains(&"maps".to_string()) {
                    maps_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    maps_collections.insert(collection.id.clone(), collection.clone());
                    maps_styles.extend(collection_layer_styles(
                        style_ctx,
                        &mut styles_cache,
                        collection,
                        &[],
                        &bundle_index,
                    ));
                    info!("Collection '{}': wired to Maps API", collection.id);
                }
                if collection.apis.contains(&"tiles".to_string()) {
                    tiles_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    tiles_collections.insert(collection.id.clone(), collection.clone());
                    tiles_styles.extend(collection_layer_styles(
                        style_ctx,
                        &mut styles_cache,
                        collection,
                        &[],
                        &bundle_index,
                    ));
                    info!("Collection '{}': wired to Tiles API", collection.id);
                }

                // Ready once the first load *succeeded*, even with zero alerts —
                // a reachable CAP source can legitimately have no active alerts.
                // Degraded only when the initial load never succeeded (e.g. an
                // unreachable feed at startup); the poll loop retries.
                let loaded = engine.is_loaded();
                health.push(CollectionHealth {
                    id: collection.id.clone(),
                    engine_type: "cap".into(),
                    status: if loaded {
                        CollectionStatus::Ready
                    } else {
                        CollectionStatus::Degraded
                    },
                    error: if loaded {
                        None
                    } else {
                        Some("initial load failed (will retry on poll)".into())
                    },
                });
            }
            "postgis" => {
                let postgis_cfg = match collection.postgis.as_ref() {
                    Some(c) => c,
                    None => {
                        tracing::error!(
                            "Collection '{}': engine_type 'postgis' but missing [collections.postgis] config, skipping",
                            collection.id
                        );
                        health.push(CollectionHealth {
                            id: collection.id.clone(),
                            engine_type: "postgis".into(),
                            status: CollectionStatus::Failed,
                            error: Some("missing [collections.postgis] config".into()),
                        });
                        continue;
                    }
                };

                let validated = match engine_postgis::config::PostgisEngineConfig::resolve(
                    postgis_cfg,
                ) {
                    Ok(v) => {
                        if v.dsn_was_literal {
                            tracing::warn!(
                                collection = %collection.id,
                                "postgis DSN is a literal URL in config (MC_ALLOW_INLINE_DB_URL=1); \
                                 use an env var in production — literal URLs end up in config \
                                 artifacts and git history."
                            );
                        }
                        Arc::new(v)
                    }
                    Err(e) => {
                        tracing::error!(
                            "Collection '{}': postgis config resolve failed: {}",
                            collection.id,
                            e
                        );
                        health.push(CollectionHealth {
                            id: collection.id.clone(),
                            engine_type: "postgis".into(),
                            status: CollectionStatus::Failed,
                            error: Some(format!("{e}")),
                        });
                        continue;
                    }
                };

                let pool_size = std::num::NonZeroU32::new(validated.pool_size)
                    .unwrap_or_else(engine_postgis::pool::default_pool_size);
                let pool = match pool_registry.get_or_create(&validated.dsn, pool_size) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::error!(
                            "Collection '{}': pool acquire failed: {}",
                            collection.id,
                            e
                        );
                        health.push(CollectionHealth {
                            id: collection.id.clone(),
                            engine_type: "postgis".into(),
                            status: CollectionStatus::Failed,
                            error: Some(format!("{e}")),
                        });
                        continue;
                    }
                };

                let engine = Arc::new(engine_postgis::PostgisEngine::new(
                    collection.id.clone(),
                    validated.clone(),
                    pool,
                ));
                if validated.events().is_some() {
                    event_sources.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::events::EventSource>,
                    );
                }

                // Bootstrap metadata synchronously. Failure ⇒ degraded status;
                // the engine still gets wired in so requests can retry once
                // the DB is reachable.
                let refresh_result = match tokio::runtime::Handle::try_current() {
                    Ok(handle) => {
                        let e = engine.clone();
                        tokio::task::block_in_place(|| {
                            handle.block_on(async move { e.refresh_metadata().await })
                        })
                    }
                    Err(_) => tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("build tokio runtime")
                        .block_on(async { engine.refresh_metadata().await }),
                };

                let (status, status_err) = match &refresh_result {
                    Ok(()) => {
                        use ds_core::edr_engine::EdrEngine as _;
                        info!(
                            "Collection '{}': postgis engine ready ({} stations, {} parameters)",
                            collection.id,
                            engine.get_locations().map(|l| l.len()).unwrap_or(0),
                            engine.get_parameters().len()
                        );
                        (CollectionStatus::Ready, None)
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Collection '{}': initial metadata refresh failed: {} (serving degraded)",
                            collection.id,
                            e
                        );
                        (CollectionStatus::Degraded, Some(format!("{e}")))
                    }
                };

                // The events shape has a raster surface (#504: the
                // age-colored lightning layer) — wire the MapEngine into the
                // raster APIs. Station shapes keep the MVT feature-tile
                // path; config validation rejects wms/maps for them.
                let is_events = validated.events().is_some();
                if collection.apis.contains(&"edr".to_string()) {
                    edr_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::edr_engine::EdrEngine>,
                    );
                    edr_collections.insert(collection.id.clone(), collection.clone());
                    if is_events {
                        edr_styles.extend(collection_layer_styles(
                            style_ctx,
                            &mut styles_cache,
                            collection,
                            &[],
                            &bundle_index,
                        ));
                    }
                }
                if collection.apis.contains(&"features".to_string()) {
                    feature_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::feature_engine::FeatureEngine>,
                    );
                    feature_collections.insert(collection.id.clone(), collection.clone());
                }
                if collection.apis.contains(&"wms".to_string()) && is_events {
                    map_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    map_collections.insert(collection.id.clone(), collection.clone());
                    map_styles.extend(collection_layer_styles(
                        style_ctx,
                        &mut styles_cache,
                        collection,
                        &[],
                        &bundle_index,
                    ));
                    info!("Collection '{}': wired to WMS API", collection.id);
                }
                if collection.apis.contains(&"maps".to_string()) && is_events {
                    maps_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    maps_collections.insert(collection.id.clone(), collection.clone());
                    maps_styles.extend(collection_layer_styles(
                        style_ctx,
                        &mut styles_cache,
                        collection,
                        &[],
                        &bundle_index,
                    ));
                    info!("Collection '{}': wired to Maps API", collection.id);
                }
                if collection.apis.contains(&"tiles".to_string()) {
                    if is_events {
                        tiles_engines.insert(
                            collection.id.clone(),
                            engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                        );
                        tiles_collections.insert(collection.id.clone(), collection.clone());
                        tiles_styles.extend(collection_layer_styles(
                            style_ctx,
                            &mut styles_cache,
                            collection,
                            &[],
                            &bundle_index,
                        ));
                        info!(
                            "Collection '{}': wired to Tiles API (raster)",
                            collection.id
                        );
                    } else {
                        tiles_feature_engines.insert(
                            collection.id.clone(),
                            engine.clone() as Arc<dyn ds_core::feature_engine::FeatureEngine>,
                        );
                        tiles_feature_collections.insert(collection.id.clone(), collection.clone());
                    }
                }
                postgis_engines.push(engine);
                health.push(CollectionHealth {
                    id: collection.id.clone(),
                    engine_type: "postgis".into(),
                    status,
                    error: status_err,
                });
            }
            // Derived collections defer to the second pass below, after all
            // base engines exist (#522).
            "nowcast" => {
                nowcast_pending.push(collection);
            }
            other => {
                tracing::error!(
                    "Collection '{}': unknown engine type '{}', skipping",
                    collection.id,
                    other
                );
                health.push(CollectionHealth {
                    id: collection.id.clone(),
                    engine_type: other.to_string(),
                    status: CollectionStatus::Failed,
                    error: Some(format!("unknown engine type '{other}'")),
                });
                continue;
            }
        }
    }

    // ------------------------------------------------------------------
    // Second pass: derived nowcast collections (#522). Every base engine
    // now exists; snapshot the raster-engine lookup BEFORE constructing any
    // nowcast engine, so a nowcast can wrap any base collection but never
    // another nowcast (chaining extrapolations compounds error and the
    // derived engine's poll cadence assumptions).
    // ------------------------------------------------------------------
    let base_raster_engines: HashMap<String, Arc<dyn ds_core::map_engine::MapEngine>> = map_engines
        .iter()
        .chain(maps_engines.iter())
        .chain(tiles_engines.iter())
        .map(|(id, e)| (id.clone(), e.clone()))
        .collect();
    for collection in nowcast_pending {
        let mut fail = |error: String| {
            tracing::error!("Collection '{}': {error}, skipping", collection.id);
            health.push(CollectionHealth {
                id: collection.id.clone(),
                engine_type: "nowcast".into(),
                status: CollectionStatus::Failed,
                error: Some(error),
            });
        };
        let Some(nowcast_config) = collection.nowcast.as_ref() else {
            fail("missing [collections.nowcast] config".into());
            continue;
        };
        if collections
            .iter()
            .any(|c| c.id == nowcast_config.source && c.engine_type == "nowcast")
        {
            fail(format!(
                "nowcast source '{}' is itself a nowcast collection (chaining is not supported)",
                nowcast_config.source
            ));
            continue;
        }
        let Some(source) = base_raster_engines.get(&nowcast_config.source) else {
            fail(format!(
                "nowcast source collection '{}' not found (it must exist in the same config \
                 and have at least one of wms/maps/tiles in its `apis`)",
                nowcast_config.source
            ));
            continue;
        };
        let engine = match engine_nowcast::NowcastEngine::new(
            &collection.id,
            &nowcast_config.source,
            source.clone(),
            nowcast_config,
        ) {
            Ok(e) => e,
            Err(e) => {
                fail(format!("failed to initialize nowcast engine: {e}"));
                continue;
            }
        };
        // Lightning join (#549): a named source must exist and be an
        // events-shape postgis collection in the same config — failing the
        // collection beats silently serving cells without flash data.
        let engine = match nowcast_config.lightning_source.as_deref() {
            Some(src_id) => match event_sources.get(src_id) {
                Some(events) => engine.with_lightning_source(events.clone()),
                None => {
                    fail(format!(
                        "lightning_source '{src_id}' not found or not an events-shape postgis \
                         collection (it must be defined in the same config)"
                    ));
                    continue;
                }
            },
            None => engine,
        };
        let engine = Arc::new(engine);
        nowcast_engines.push(engine.clone());

        if collection.apis.contains(&"wms".to_string()) {
            map_engines.insert(
                collection.id.clone(),
                engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
            );
            map_collections.insert(collection.id.clone(), collection.clone());
            map_styles.extend(collection_layer_styles(
                style_ctx,
                &mut styles_cache,
                collection,
                &[],
                &bundle_index,
            ));
            info!("Collection '{}': wired to WMS API", collection.id);
        }
        if collection.apis.contains(&"maps".to_string()) {
            maps_engines.insert(
                collection.id.clone(),
                engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
            );
            maps_collections.insert(collection.id.clone(), collection.clone());
            maps_styles.extend(collection_layer_styles(
                style_ctx,
                &mut styles_cache,
                collection,
                &[],
                &bundle_index,
            ));
            info!("Collection '{}': wired to Maps API", collection.id);
        }
        if collection.apis.contains(&"tiles".to_string()) {
            tiles_engines.insert(
                collection.id.clone(),
                engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
            );
            tiles_collections.insert(collection.id.clone(), collection.clone());
            tiles_styles.extend(collection_layer_styles(
                style_ctx,
                &mut styles_cache,
                collection,
                &[],
                &bundle_index,
            ));
            info!("Collection '{}': wired to Tiles API", collection.id);
        }

        if collection.apis.contains(&"features".to_string()) {
            feature_engines.insert(
                collection.id.clone(),
                engine.clone() as Arc<dyn ds_core::feature_engine::FeatureEngine>,
            );
            feature_collections.insert(collection.id.clone(), collection.clone());
            info!("Collection '{}': wired to Features API", collection.id);
        }

        // A nowcast starts degraded: the first generation needs the source's
        // initial data plus one poll cycle.
        health.push(CollectionHealth {
            id: collection.id.clone(),
            engine_type: "nowcast".into(),
            status: CollectionStatus::Degraded,
            error: Some("waiting for first nowcast generation".into()),
        });
        info!(
            "Collection '{}': nowcast wrapping '{}'",
            collection.id, nowcast_config.source
        );
    }

    // Determine rendered cache size from first WMS collection config, or default
    let rendered_cache_mb = map_collections
        .values()
        .chain(maps_collections.values())
        .filter_map(|c| c.wms.as_ref())
        .map(|w| w.rendered_cache_mb)
        .next()
        .unwrap_or(128);

    // Meta-tile pixel cache size (#202) is a server-wide setting
    // (`[server] metatile_cache_mb`) — the cache is global to all WMS
    // collections, so it is passed in as a single value (no per-collection
    // aggregation). `0` disables meta-tiling.

    // 2× cores (min 8) — the render slot's "ownership" of a CPU is loose
    // because decode/encode interleaves with bilinear passes; configurable
    // knob tracked in #147.
    let render_concurrency = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .saturating_mul(2)
        .max(8);
    tracing::info!("Render concurrency: {render_concurrency} (2× available CPUs, min 8)");
    let render_semaphore = Arc::new(tokio::sync::Semaphore::new(render_concurrency));
    // Reuse the live render caches across a reload when their configured byte
    // size is unchanged, so a reload preserves the warm cache instead of
    // dumping GBs of meta-tiles and forcing a cold re-warm (see
    // [`ReusableCaches`]). A size change (or startup, where `reuse` is empty)
    // builds fresh. Stale entries survive harmlessly: a removed collection's
    // layer 404s before its cached tiles can be served (they then age out via
    // LRU), and a changed colormap re-colors as new timesteps render (the cache
    // key carries `time`); a hard guarantee for an in-place colormap swap still
    // needs a restart.
    let mb = |m: u64| m.saturating_mul(1024 * 1024);
    let rendered_cache = match reuse.rendered {
        Some(c) if c.capacity() == mb(rendered_cache_mb) => c,
        _ => Arc::new(ds_render::RenderedCache::new(rendered_cache_mb)),
    };
    let tile_cache = match reuse.tile {
        Some(c) if c.capacity() == mb(metatile_cache_mb) => c,
        _ => Arc::new(ds_render::TilePixelCache::new(metatile_cache_mb)),
    };
    // Vector-tile (MVT) cache is independent of the raster cache because the
    // workloads differ (1–50 KB vs 30–200 KB per tile). Fixed size, kept in a
    // single constant so the build and the reuse-capacity check can't drift (a
    // mismatch would silently always rebuild). Reused across reloads on the same
    // terms as the raster caches.
    const VECTOR_TILE_CACHE_MB: u64 = 128;
    let vector_tile_cache = match reuse.vector {
        Some(c) if c.capacity_bytes() == mb(VECTOR_TILE_CACHE_MB) => c,
        _ => Arc::new(ds_mvt::VectorTileCache::new(VECTOR_TILE_CACHE_MB)),
    };

    // Set initial render semaphore total gauge
    RENDER_SEMAPHORE_TOTAL.set(render_concurrency as i64);

    LoadResult {
        edr_state: EdrState {
            engines: edr_engines,
            collections: edr_collections,
            styles: edr_styles,
            base_url: base_url.to_string(),
            trust_proxy_headers,
        },
        features_state: FeaturesState {
            engines: feature_engines,
            collections: feature_collections,
            base_url: base_url.to_string(),
            trust_proxy_headers,
        },
        wms_state: WmsState {
            engines: map_engines,
            collections: map_collections,
            styles: map_styles,
            render_semaphore: render_semaphore.clone(),
            rendered_cache: rendered_cache.clone(),
            tile_cache,
            base_url: base_url.to_string(),
            trust_proxy_headers,
        },
        maps_state: MapsState {
            engines: maps_engines,
            collections: maps_collections,
            styles: maps_styles,
            render_semaphore: render_semaphore.clone(),
            rendered_cache: rendered_cache.clone(),
            base_url: base_url.to_string(),
            trust_proxy_headers,
        },
        tiles_state: TilesState {
            map_engines: tiles_engines,
            collections: tiles_collections,
            styles: tiles_styles,
            feature_engines: tiles_feature_engines,
            feature_collections: tiles_feature_collections,
            render_semaphore: render_semaphore.clone(),
            rendered_cache: rendered_cache.clone(),
            vector_tile_cache: vector_tile_cache.clone(),
            base_url: base_url.to_string(),
            trust_proxy_headers,
        },
        tiles_3d_state: api_3dtiles::TilesState3d {
            volume_engines,
            collections: volume_collections,
            // v1: one shared reflectivity ramp for all 3D-Tiles collections (the
            // legend the API advertises samples this same source). Per-collection
            // /per-quantity colormaps from config are a follow-up (#350).
            colormap: api_3dtiles::default_point_colormap(),
            render_semaphore: render_semaphore.clone(),
            base_url: base_url.to_string(),
            trust_proxy_headers,
        },
        health,
        geotiff_engines,
        querydata_engines,
        grib_engines,
        zarr_engines,
        odim_engines,
        odim_volume_engines,
        cap_engines,
        postgis_engines,
        nowcast_engines,
    }
}

/// Legacy-behavior fallback when style resolution fails despite config
/// validation: a default-only style map on viridis 0..1.
fn fallback_default_styles(ctx: &ds_render::StyleContext) -> HashMap<String, ds_render::StyleInfo> {
    let r = ctx
        .build_colormap(&ds_render::StyleSpec::default())
        .expect("viridis is always registered");
    let mut styles = HashMap::new();
    styles.insert(
        "default".to_string(),
        ds_render::StyleInfo {
            name: "default".to_string(),
            title: "Default".to_string(),
            colormap: r.colormap,
            palette: r.palette,
            min: r.min,
            max: r.max,
            parameter: None,
        },
    );
    styles
}

/// Compute (once per collection) the full style-layer map: the collection
/// key plus one "{id}/{param}" key per parameter, resolved through the
/// shared StyleContext. Cached so the WMS, Maps and Tiles registries (and
/// EDR) share identical StyleInfo instances instead of re-resolving per
/// API (previously 3×). The ODIM CELLS overlay wrap is injected here —
/// engine-specific logic that stays out of ds-render (#410).
fn collection_layer_styles(
    ctx: &ds_render::StyleContext,
    cache: &mut HashMap<String, HashMap<String, HashMap<String, ds_render::StyleInfo>>>,
    collection: &CollectionConfig,
    param_names: &[(String, String)],
    bundles: &HashMap<&str, &StyleBundle>,
) -> HashMap<String, HashMap<String, ds_render::StyleInfo>> {
    if let Some(hit) = cache.get(&collection.id) {
        return hit.clone();
    }
    let bundle = resolve_bundle(collection, bundles);
    // The derived storm-cell overlay (#367) paints cell outlines at their
    // dBZ value and track trails at a reserved sentinel; wrap whatever
    // colormap CELLS resolves to so the sentinel renders one neutral
    // colour. Scoped to `odim-volume` — without the engine-type gate a
    // non-ODIM collection with a band coincidentally named `CELLS` would
    // get -9999.0 hijacked to grey (#410 removes this via an OverlaySpec).
    let wrap = |short: &str, cmap: Arc<dyn ds_render::ColorMap>| -> Arc<dyn ds_render::ColorMap> {
        if collection.engine_type == "odim-volume" && short == engine_odim::cells::CELLS_PARAMETER {
            Arc::new(ds_render::OverlayColorMap::new(
                cmap,
                engine_odim::cells::CELLS_TRACK_SENTINEL,
                engine_odim::cells::CELLS_TRACK_COLOR,
            ))
        } else {
            cmap
        }
    };
    let mut layers = HashMap::new();
    match ctx.collection_styles(collection, bundle) {
        Ok(styles) => {
            layers.insert(collection.id.clone(), styles);
        }
        Err(e) => {
            // Unknown colormap names are rejected at config validation
            // (validate_style_colormaps); reaching this is a bug. Keep the
            // legacy viridis fallback so the collection still renders.
            tracing::error!(
                "Collection '{}': style resolution failed ({e}); using viridis fallback",
                collection.id
            );
            layers.insert(collection.id.clone(), fallback_default_styles(ctx));
        }
    }
    if !param_names.is_empty() {
        match ctx.parameter_layer_styles(collection, bundle, param_names, &wrap) {
            Ok(maps) => layers.extend(maps),
            Err(e) => tracing::error!(
                "Collection '{}': parameter style resolution failed ({e})",
                collection.id
            ),
        }
    }
    cache.insert(collection.id.clone(), layers.clone());
    layers
}

/// Resolve a collection's bound bundle; None if unset (validation rejects unresolved refs).
fn resolve_bundle<'a>(
    collection: &CollectionConfig,
    bundles: &'a HashMap<&str, &'a StyleBundle>,
) -> Option<&'a StyleBundle> {
    let bundle_ref = collection.wms.as_ref()?.style_bundle.as_deref()?;
    match bundles.get(bundle_ref).copied() {
        Some(b) => Some(b),
        None => {
            // validate() rejects unresolved refs, so this is defensive: log
            // so the inline-fallback path is observable if a caller somehow
            // bypasses validation.
            tracing::warn!(
                "Collection '{}': style_bundle '{}' not found in index — falling back to inline config",
                collection.id,
                bundle_ref
            );
            None
        }
    }
}

/// Reject unknown `colormap = "..."` names at config load. A typo'd name
/// previously fell back to viridis silently — the config "worked" but
/// rendered wrong. Runs against the same palette registry the styles are
/// built from (built-ins today; [[colormaps]] entries join in a later
/// phase).
pub fn validate_style_colormaps(
    collections: &[CollectionConfig],
    style_bundles: &[StyleBundle],
    registry: &ds_render::PaletteRegistry,
) -> Result<(), ds_core::error::DataServerError> {
    let check = |owner: &str, name: Option<&str>| -> Result<(), ds_core::error::DataServerError> {
        if let Some(n) = name {
            if !registry.contains(n) {
                return Err(ds_core::error::DataServerError::Config(format!(
                    "{owner}: unknown colormap '{n}' (available: {})",
                    registry.names().join(", ")
                )));
            }
        }
        Ok(())
    };
    for b in style_bundles {
        check(
            &format!("style_bundle '{}'", b.id),
            b.default.colormap.as_deref(),
        )?;
        for e in &b.extras {
            check(
                &format!("style_bundle '{}' extra '{}'", b.id, e.name),
                e.colormap.as_deref(),
            )?;
        }
    }
    for c in collections {
        if let Some(w) = &c.wms {
            let owner = format!("collection '{}'", c.id);
            check(&owner, w.colormap.as_deref())?;
            for s in &w.styles {
                check(
                    &format!("{owner} style '{}'", s.name),
                    s.colormap.as_deref(),
                )?;
            }
            for p in &w.parameters {
                check(
                    &format!("{owner} parameter '{}'", p.name),
                    p.colormap.as_deref(),
                )?;
            }
        }
    }
    Ok(())
}

/// Update the health gauges from the current health vector.
pub fn update_health_gauges(health: &[CollectionHealth]) {
    let total = health.len() as i64;
    let healthy = health
        .iter()
        .filter(|h| h.status == CollectionStatus::Ready)
        .count() as i64;
    let degraded = health
        .iter()
        .filter(|h| h.status == CollectionStatus::Degraded)
        .count() as i64;
    let failed = health
        .iter()
        .filter(|h| h.status == CollectionStatus::Failed)
        .count() as i64;

    COLLECTIONS_TOTAL.set(total);
    COLLECTIONS_HEALTHY.set(healthy);
    COLLECTIONS_DEGRADED.set(degraded);
    COLLECTIONS_FAILED.set(failed);
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// POST /admin/collections/reload — re-read config and swap engines atomically.
pub async fn reload_handler(
    State(state): State<AdminState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    // Check admin token authentication
    match &state.admin_token {
        None => {
            return Err((
                StatusCode::FORBIDDEN,
                Json(json!({
                    "error": "Admin endpoint is disabled. Set ADMIN_TOKEN env var or admin_token in [server] config to enable."
                })),
            ));
        }
        Some(expected_token) => {
            let provided = headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "));
            match provided {
                Some(token) if token == expected_token => {} // OK
                _ => {
                    return Err((
                        StatusCode::UNAUTHORIZED,
                        Json(json!({ "error": "Invalid or missing Bearer token" })),
                    ));
                }
            }
        }
    }

    // Serialize reload requests — prevent concurrent reloads from racing
    let _reload_guard = state.reload_lock.try_lock().map_err(|_| {
        (
            StatusCode::CONFLICT,
            Json(json!({ "error": "A reload is already in progress" })),
        )
    })?;

    // Run the (blocking) reload on the background runtime so it never parks a
    // request-serving worker — same as the collections_dir watcher. Use
    // `spawn` (an async task on a poll-runtime worker), NOT `spawn_blocking`:
    // engine construction calls `ds-storage`'s `block_in_place`, which panics
    // on a spawn_blocking pool thread but is valid on a multi-thread worker.
    let state2 = state.clone();
    let outcome = crate::poll_runtime()
        .spawn(async move { do_reload(&state2) })
        .await;
    match outcome {
        Ok(Ok(o)) => Ok(Json(json!({
            "status": "ok",
            "ready": o.ready,
            "degraded": o.degraded,
            "configured": o.configured,
            "collections": o.health,
        }))),
        Ok(Err(ReloadError::ConfigRead(e))) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to read config: {e}") })),
        )),
        Ok(Err(ReloadError::NoReadyCollections { configured })) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": "Reload produced 0 working collections, keeping old state",
                "configured": configured
            })),
        )),
        Err(e) => {
            tracing::error!("Reload task failed to join: {e}");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Reload task failed" })),
            ))
        }
    }
}

/// Outcome of a successful reload — counts + final health for the response/log.
pub(crate) struct ReloadOutcome {
    pub ready: usize,
    pub degraded: usize,
    pub configured: usize,
    pub health: Vec<CollectionHealth>,
}

/// Why a reload was rejected — the live registry is left untouched in both cases.
pub(crate) enum ReloadError {
    /// Re-reading / parsing the config failed (e.g. a malformed collection file).
    ConfigRead(String),
    /// The new config produced zero `Ready` collections; the old state is kept.
    NoReadyCollections { configured: usize },
}

/// Re-read the config from `state.config_path`, rebuild every engine, and
/// atomically swap them into the live registries — the shared core behind both
/// `POST /admin/collections/reload` and the `collections_dir` watcher.
///
/// The caller serializes reloads via `state.reload_lock` (the HTTP handler
/// `try_lock`s → 409 on contention; the watcher `lock().await`s). On `Err` the
/// live state is untouched: old engines and their poll loops keep running, and
/// the freshly-built (rejected) engines are dropped with their loops never
/// spawned. The (blocking) load runs synchronously — call it off the
/// request-serving runtime (the watcher uses `poll_runtime()`).
pub(crate) fn do_reload(state: &AdminState) -> Result<ReloadOutcome, ReloadError> {
    info!(
        "Reloading collections, re-reading config from {}",
        state.config_path
    );

    let (config, config_warnings) = ds_core::config::ServerConfig::from_file(&state.config_path)
        .map_err(|e| {
            tracing::error!("Reload failed: {e}");
            ReloadError::ConfigRead(format!("{e}"))
        })?;
    for warning in &config_warnings {
        tracing::warn!("{warning}");
    }

    // Rebuild the palette registry (built-ins + [[colormaps]] +
    // colormaps_dir) and run the same colormap-name validation as startup:
    // an unknown name or a broken palette file rejects the reload and keeps
    // the old registry serving.
    let config_dir = std::path::Path::new(&state.config_path)
        .parent()
        .map(std::path::Path::to_path_buf);
    let palette_registry = crate::colormaps::build_palette_registry(&config, config_dir.as_deref())
        .map_err(|e| {
            tracing::error!("Reload failed: {e}");
            ReloadError::ConfigRead(format!("{e}"))
        })?;
    validate_style_colormaps(
        &config.collections,
        &config.style_bundles,
        &palette_registry,
    )
    .map_err(|e| {
        tracing::error!("Reload failed: {e}");
        ReloadError::ConfigRead(format!("{e}"))
    })?;
    let style_ctx = ds_render::StyleContext::new(palette_registry);

    let base_url = config.server.base_url();

    // NOTE: old poll loops are shut down *after* the reload guard below, not
    // here. If the guard rejects the reload (no `Ready` collection), the old
    // engines and their poll loops must stay alive — otherwise a rejected
    // reload would freeze the live registry with dead loops and no new ones
    // spawned (the guard returns before the spawn block).

    // Carry the live render caches into the reload so it preserves the warm
    // cache instead of rebuilding it empty — a spurious `collections_dir`
    // watcher event must not dump a multi-GB meta-tile cache. `load_collections`
    // reuses each one iff its configured size is unchanged.
    //
    // EXCEPT when style-affecting config changed (palettes, bundles, [wms]
    // blocks, colormaps_dir files): the rendered / meta-tile keys carry no
    // style content, so reusing those caches would keep serving the OLD
    // colors as X-Cache HITs. The (style-independent) vector-tile cache is
    // always safe to reuse.
    let new_style_fp = crate::colormaps::style_config_fingerprint(&config, config_dir.as_deref());
    let styles_changed = state
        .style_fingerprint
        .load(std::sync::atomic::Ordering::Relaxed)
        != new_style_fp;
    if styles_changed {
        info!("Style configuration changed — dropping rendered and meta-tile caches");
    }
    let reuse = {
        let wms = state.wms.load();
        ReusableCaches {
            rendered: (!styles_changed).then(|| wms.rendered_cache.clone()),
            tile: (!styles_changed).then(|| wms.tile_cache.clone()),
            vector: Some(state.tiles.load().vector_tile_cache.clone()),
        }
    };
    let mut result = load_collections(
        &style_ctx,
        &config.collections,
        &config.style_bundles,
        &base_url,
        config.server.trust_proxy_headers,
        config.server.metatile_cache_mb,
        reuse,
    );

    // Reload protection counts *fully working* (`Ready`) collections, not
    // just non-`Failed` ones. A `Degraded` placeholder — e.g. an
    // odim-volume source that transiently scanned zero sites, or a postgis
    // collection whose DB is momentarily down — wires no servable routes,
    // so it must not satisfy the guard and let an empty/degraded reload
    // replace a working live registry. (Startup in `main.rs` deliberately
    // uses the looser `!= Failed`: at boot there is no live state to
    // protect, so the server should start degraded and wait for the first
    // poll rather than refuse to boot.)
    let ready = result
        .health
        .iter()
        .filter(|h| h.status == CollectionStatus::Ready)
        .count();

    if ready == 0 && !config.collections.is_empty() {
        // Reject the reload and keep the live state. The old engines were
        // NOT shut down (that happens below, only on accept), so their poll
        // loops keep running and recover on their own — e.g. a transiently
        // unreachable PostGIS DB, or an odim-volume source that momentarily
        // scanned zero sites. The newly-built engines in `result` are simply
        // dropped (their poll loops were never spawned).
        tracing::error!(
            "Reload produced 0 working collections from {} configured. Keeping old state.",
            config.collections.len()
        );
        return Err(ReloadError::NoReadyCollections {
            configured: config.collections.len(),
        });
    }

    // Reload accepted: remember the style fingerprint so the NEXT reload
    // can tell whether style config changed again.
    state
        .style_fingerprint
        .store(new_style_fp, std::sync::atomic::Ordering::Relaxed);

    // Surface a vanished per-site radar in `/health`. A site that was
    // `Ready` in the live registry but is absent from the new scan (its
    // files aged out of the time window, or the radar went offline) would
    // otherwise just disappear from `/collections` with no `/health` trace.
    // Push a `Degraded` entry so monitoring can see it — but only when the
    // site's base source is still configured (a fully-removed source is a
    // config change, not a data gap). Degraded, so it doesn't affect the
    // `ready` guard above.
    {
        let old_health = state.health.read().unwrap_or_else(|e| e.into_inner());
        let new_ids: std::collections::HashSet<&str> =
            result.health.iter().map(|h| h.id.as_str()).collect();
        let new_bases: Vec<String> = result
            .odim_volume_engines
            .iter()
            .map(|e| e.collection_id().to_string())
            .collect();
        let vanished: Vec<String> = old_health
            .iter()
            .filter(|h| h.engine_type == "odim-volume" && h.status == CollectionStatus::Ready)
            .filter(|h| !new_ids.contains(h.id.as_str()))
            .filter(|h| {
                // `{base}-{nod}` where `nod` is plain alphanumeric (no `-`).
                // Require the suffix after the base to be exactly `-{nod}`,
                // not merely to start with `-`, so a still-present base
                // `radar` doesn't claim a vanished site from a removed
                // `radar-fi` source: `radar-fi-x`.strip_prefix(`radar`) =
                // `-fi-x`, whose nod part `fi-x` is not alphanumeric.
                new_bases.iter().any(|b| {
                    h.id.strip_prefix(b.as_str()).is_some_and(|r| {
                        r.strip_prefix('-').is_some_and(|nod| {
                            !nod.is_empty() && nod.bytes().all(|c| c.is_ascii_alphanumeric())
                        })
                    })
                })
            })
            .map(|h| h.id.clone())
            .collect();
        drop(old_health);
        for id in vanished {
            tracing::warn!("Reload: radar site collection '{id}' is no longer present in the scan");
            result.health.push(CollectionHealth {
                id,
                engine_type: "odim-volume".into(),
                status: CollectionStatus::Degraded,
                error: Some("site no longer present in the latest scan".into()),
            });
        }
    }

    // Reload accepted — now shut down the old poll loops (the new ones are
    // spawned just below, then state is swapped atomically).
    {
        for engine in state
            .geotiff_engines
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            engine.shutdown();
        }
        for engine in state
            .querydata_engines
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            engine.shutdown();
        }
        for engine in state
            .grib_engines
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            engine.shutdown();
        }
        for engine in state
            .zarr_engines
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            engine.shutdown();
        }
        for engine in state
            .odim_engines
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            engine.shutdown();
        }
        for engine in state
            .odim_volume_engines
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            engine.shutdown();
        }
        for engine in state
            .cap_engines
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            engine.shutdown();
        }
        for engine in state
            .postgis_engines
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            engine.shutdown();
        }
        for engine in state
            .nowcast_engines
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            engine.shutdown();
        }
    }

    // Spawn poll loops for new engines on the dedicated background runtime
    // so their blocking I/O never parks a request-serving worker (#221).
    for engine in &result.geotiff_engines {
        let poller = engine.clone();
        crate::poll_runtime().spawn(async move {
            poller.poll_loop().await;
        });
    }
    for engine in &result.querydata_engines {
        let poller = engine.clone();
        crate::poll_runtime().spawn(async move {
            poller.poll_loop().await;
        });
    }
    for engine in &result.grib_engines {
        let poller = engine.clone();
        crate::poll_runtime().spawn(async move {
            poller.poll_loop().await;
        });
    }
    for engine in &result.zarr_engines {
        let poller = engine.clone();
        crate::poll_runtime().spawn(async move {
            poller.poll_loop().await;
        });
    }
    for engine in &result.odim_engines {
        let poller = engine.clone();
        crate::poll_runtime().spawn(async move {
            poller.poll_loop().await;
        });
    }
    for engine in &result.odim_volume_engines {
        let poller = engine.clone();
        crate::poll_runtime().spawn(async move {
            poller.poll_loop().await;
        });
    }
    for engine in &result.cap_engines {
        let poller = engine.clone();
        crate::poll_runtime().spawn(async move {
            poller.poll_loop().await;
        });
    }
    // PostGIS metadata refresh loop (location list / extents / the
    // `locations_window` "currently reporting" set). Async DB I/O on its own
    // deadpool pool — runs on the background runtime, not a request worker.
    for engine in &result.postgis_engines {
        let poller = engine.clone();
        crate::poll_runtime().spawn(async move {
            poller.poll_loop().await;
        });
    }
    // Nowcast generation loop: watches the source engine and regenerates
    // extrapolated frames. Source fetches do blocking storage I/O internally,
    // so this must stay on the background runtime (#221, Critical Rule 7).
    for engine in &result.nowcast_engines {
        let poller = engine.clone();
        crate::poll_runtime().spawn(async move {
            poller.poll_loop().await;
        });
    }

    // Atomically swap state
    state.edr.store(Arc::new(result.edr_state));
    state.features.store(Arc::new(result.features_state));
    state.wms.store(Arc::new(result.wms_state));
    state.maps.store(Arc::new(result.maps_state));
    state.tiles.store(Arc::new(result.tiles_state));
    state.tiles_3d.store(Arc::new(result.tiles_3d_state));

    // Update health
    update_health_gauges(&result.health);
    *state.health.write().unwrap_or_else(|e| e.into_inner()) = result.health.clone();

    // Update engine lists
    *state
        .geotiff_engines
        .write()
        .unwrap_or_else(|e| e.into_inner()) = result.geotiff_engines;
    *state
        .querydata_engines
        .write()
        .unwrap_or_else(|e| e.into_inner()) = result.querydata_engines;
    *state
        .grib_engines
        .write()
        .unwrap_or_else(|e| e.into_inner()) = result.grib_engines;
    *state
        .zarr_engines
        .write()
        .unwrap_or_else(|e| e.into_inner()) = result.zarr_engines;
    *state
        .odim_engines
        .write()
        .unwrap_or_else(|e| e.into_inner()) = result.odim_engines;
    *state
        .odim_volume_engines
        .write()
        .unwrap_or_else(|e| e.into_inner()) = result.odim_volume_engines;
    *state.cap_engines.write().unwrap_or_else(|e| e.into_inner()) = result.cap_engines;
    *state
        .postgis_engines
        .write()
        .unwrap_or_else(|e| e.into_inner()) = result.postgis_engines;
    *state
        .nowcast_engines
        .write()
        .unwrap_or_else(|e| e.into_inner()) = result.nowcast_engines;

    // Recount from the final `result.health` — the vanished-site block above
    // appends `Degraded` entries after the guard's `ready` was computed, so
    // count here to keep the response in sync with the `collections` array.
    let ready = result
        .health
        .iter()
        .filter(|h| h.status == CollectionStatus::Ready)
        .count();
    let degraded = result
        .health
        .iter()
        .filter(|h| h.status == CollectionStatus::Degraded)
        .count();
    info!(
        "Reload complete: {ready} ready ({degraded} degraded) of {} configured",
        config.collections.len()
    );

    // `ready` = fully-working collections; `degraded` wire no servable routes
    // (e.g. an empty odim-volume source) but aren't failures.
    Ok(ReloadOutcome {
        ready,
        degraded,
        configured: config.collections.len(),
        health: result.health,
    })
}

/// GET /health — per-collection health status with data staleness info.
pub async fn health_handler(State(state): State<AdminState>) -> impl IntoResponse {
    let mut health = state
        .health
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();

    // Reflect LIVE postgis health (the 30s SELECT 1 ping) over the boot snapshot,
    // so a DB that went down after startup flips to `degraded` and one that
    // recovered flips back (#110). A `Failed` collection has no engine (couldn't
    // construct), so it's absent from the live map and keeps its boot status.
    // `live_health()` is `None` until the first ping completes — so a
    // boot-degraded collection keeps that boot status instead of the optimistic
    // `ready` seed during the load→first-ping window.
    {
        let engines = state
            .postgis_engines
            .read()
            .unwrap_or_else(|e| e.into_inner());
        let live: HashMap<&str, engine_postgis::HealthStatus> = engines
            .iter()
            .filter_map(|e| Some((e.collection_id(), e.live_health()?)))
            .collect();
        for h in health.iter_mut().filter(|h| h.engine_type == "postgis") {
            if let Some(&s) = live.get(h.id.as_str()) {
                match s {
                    engine_postgis::HealthStatus::Ready => {
                        h.status = CollectionStatus::Ready;
                        h.error = None;
                    }
                    engine_postgis::HealthStatus::Degraded => {
                        h.status = CollectionStatus::Degraded;
                        h.error.get_or_insert_with(|| {
                            "database unreachable (health ping failed)".into()
                        });
                    }
                }
            }
        }
    }

    // Build per-collection metadata from concrete engine types.
    // Uses EDR-style temporal extent format: { interval, values? }
    let mut data_ages: HashMap<String, i64> = HashMap::new();
    let mut temporal_info: HashMap<String, serde_json::Value> = HashMap::new();

    // Helper: build temporal extent { interval, values? } from a sorted
    // (first..last) timestamp list.
    fn temporal_from_times(times: &[chrono::DateTime<chrono::Utc>]) -> Option<serde_json::Value> {
        let first = times.first()?;
        let last = times.last()?;
        let mut obj = serde_json::Map::new();
        obj.insert(
            "interval".to_string(),
            json!([[first.to_rfc3339(), last.to_rfc3339()]]),
        );
        let values: Vec<String> = times.iter().map(|t| t.to_rfc3339()).collect();
        obj.insert("values".to_string(), json!(values));
        Some(json!(obj))
    }

    // Helper: build temporal extent from any EdrEngine
    fn build_temporal(engine: &dyn ds_core::edr_engine::EdrEngine) -> Option<serde_json::Value> {
        let (start, end) = engine.get_temporal_extent()?;
        let mut obj = serde_json::Map::new();
        obj.insert(
            "interval".to_string(),
            json!([[start.to_rfc3339(), end.to_rfc3339()]]),
        );
        if let Some(times) = engine.get_available_times() {
            let values: Vec<String> = times.iter().map(|t| t.to_rfc3339()).collect();
            obj.insert("values".to_string(), json!(values));
        }
        Some(json!(obj))
    }

    {
        let engines = state
            .geotiff_engines
            .read()
            .unwrap_or_else(|e| e.into_inner());
        for engine in engines.iter() {
            let id = engine.collection_id().to_string();
            if let Some(age) = engine.catalog_age() {
                data_ages.insert(id.clone(), age.num_seconds());
            }
            if let Some(temporal) = build_temporal(engine.as_ref()) {
                temporal_info.insert(id, temporal);
            }
        }
    }
    {
        let engines = state
            .querydata_engines
            .read()
            .unwrap_or_else(|e| e.into_inner());
        for engine in engines.iter() {
            let id = engine.collection_id().to_string();
            if let Some(age) = engine.data_age() {
                data_ages.insert(id.clone(), age.num_seconds());
            }
            if let Some(temporal) = build_temporal(engine.as_ref()) {
                temporal_info.insert(id, temporal);
            }
        }
    }
    {
        let engines = state.grib_engines.read().unwrap_or_else(|e| e.into_inner());
        for engine in engines.iter() {
            let id = engine.collection_id().to_string();
            if let Some(temporal) = build_temporal(engine.as_ref()) {
                temporal_info.insert(id, temporal);
            }
        }
    }
    {
        let engines = state.zarr_engines.read().unwrap_or_else(|e| e.into_inner());
        for engine in engines.iter() {
            let id = engine.collection_id().to_string();
            if let Some(temporal) = build_temporal(engine.as_ref()) {
                temporal_info.insert(id, temporal);
            }
        }
    }
    {
        // PVOL sources expand into one per-site collection each
        // (`{base}-{nod}`); the engine is keyed by `{base}` and does not
        // implement `EdrEngine`. `site_times()` returns every site's
        // timestamps from one catalog snapshot, so the temporal extents are
        // built without allocating a view per site (O(1)-from-a-snapshot per
        // request). Key each by the `{base}-{nod}` id to match the health
        // entries.
        let engines = state
            .odim_volume_engines
            .read()
            .unwrap_or_else(|e| e.into_inner());
        for engine in engines.iter() {
            for (nod, times) in engine.site_times() {
                if let Some(temporal) = temporal_from_times(&times) {
                    temporal_info.insert(format!("{}-{}", engine.collection_id(), nod), temporal);
                }
            }
        }
    }

    {
        // Nowcast engines: `data_age_secs` is the age of the latest
        // generation's anchor frame; the boot `Degraded ("waiting for first
        // nowcast generation")` flips to `Ready` once a generation exists.
        let engines = state
            .nowcast_engines
            .read()
            .unwrap_or_else(|e| e.into_inner());
        let mut has_data: HashMap<&str, bool> = HashMap::new();
        for engine in engines.iter() {
            let id = engine.collection_id().to_string();
            has_data.insert(engine.collection_id(), engine.has_data());
            if let Some(age) = engine.catalog_age() {
                data_ages.insert(id.clone(), age.num_seconds());
            }
            let times = ds_core::map_engine::MapEngine::raster_info(engine.as_ref()).times;
            if let Some(temporal) = temporal_from_times(&times) {
                temporal_info.insert(id, temporal);
            }
        }
        for h in health.iter_mut().filter(|h| h.engine_type == "nowcast") {
            if has_data.get(h.id.as_str()).copied().unwrap_or(false) {
                h.status = CollectionStatus::Ready;
                h.error = None;
            }
        }
    }

    // Enrich health entries with staleness and temporal extent
    let collections: Vec<serde_json::Value> = health
        .iter()
        .map(|h| {
            let mut entry = serde_json::to_value(h).unwrap_or_default();
            if let Some(age_secs) = data_ages.get(&h.id) {
                entry["data_age_secs"] = json!(*age_secs);
            }
            if let Some(temporal) = temporal_info.get(&h.id) {
                entry["extent"] = json!({ "temporal": temporal });
            }
            entry
        })
        .collect();

    let all_failed =
        !health.is_empty() && health.iter().all(|h| h.status == CollectionStatus::Failed);

    let overall = if health.iter().all(|h| h.status == CollectionStatus::Ready) {
        "healthy"
    } else if all_failed {
        // Every collection failed — nothing to serve
        "unhealthy"
    } else {
        // Mix of ready/degraded/failed — server is functional but not fully healthy
        "degraded"
    };

    // Only return 503 when the server can't serve anything at all
    let status_code = if all_failed {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };

    (
        status_code,
        Json(json!({
            "status": overall,
            "collections": collections
        })),
    )
}

/// GET /metrics — Prometheus text format.
///
/// Updates gauge-style metrics from engine/cache state before gathering,
/// so Prometheus always gets a fresh snapshot.
pub async fn metrics_handler(State(state): State<AdminState>) -> impl IntoResponse {
    // Read from current WMS state (survives reloads via ArcSwap)
    let wms = state.wms.load();
    RENDER_SEMAPHORE_AVAILABLE.set(wms.render_semaphore.available_permits() as i64);
    update_memory_gauges();

    // Delta-tracked cache counters: cache implementations expose cumulative
    // (hits, misses) values but may be replaced on reload; each CacheMetricSet
    // converts them to monotonic Prometheus counters (rebaselining on a
    // backward step without emitting a spike). The per-collection labelled
    // families further below still use CACHE_COUNTER_STATE.
    let mut counter_state = CACHE_COUNTER_STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    // Rendered image cache: global (single cache shared across collections).
    RENDERED_CACHE_METRICS.update(
        wms.rendered_cache.metrics(),
        Some(wms.rendered_cache.len() as u64),
    );

    // Meta-tile pixel cache: global, same delta-tracking as the rendered cache.
    METATILE_CACHE_METRICS.update(wms.tile_cache.metrics(), Some(wms.tile_cache.len() as u64));
    METATILE_DECLINES.feed(ds_render::metatile::budget_declines_total());

    // PVOL lazy pixel cache: process-global (never replaced on reload). Only
    // emit when PVOL collections are loaded, so non-radar deployments don't
    // carry empty `pvol_*` series. Recover from a poisoned lock
    // (`into_inner`) — a panic elsewhere must not silently suppress the
    // failure metric.
    let has_pvol = !state
        .odim_volume_engines
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty();
    if has_pvol {
        let pixel = engine_odim::pixel_cache_metrics();
        PVOL_PIXEL_CACHE_METRICS.update(pixel.cache, Some(pixel.entries));
        PVOL_PIXEL_CACHE_INSERTS.feed(pixel.inserts);
        PVOL_PIXEL_READ_FAILURES.feed(pixel.read_failures);

        // Voxel-grid + storm-cell set caches: same process-global shape.
        PVOL_VOXEL_GRID_CACHE_METRICS.update(engine_odim::voxel_grid_cache_metrics(), None);
        PVOL_CELL_SET_CACHE_METRICS.update(engine_odim::cell_set_cache_metrics(), None);
    }

    // COMP composite cache: same process-global shape as the PVOL caches.
    // Only emit when COMP (`engine_type = "odim"`) collections are loaded, so
    // non-radar / PVOL-only deployments don't carry empty `odim_composite_*`
    // series.
    let has_comp = !state
        .odim_engines
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty();
    if has_comp {
        ODIM_COMPOSITE_CACHE_METRICS.update(engine_odim::composite_cache_metrics(), None);
    }

    // 3D Tiles encoded-content cache: process-global, like the PVOL caches.
    // Only emit when 3D Tiles collections are loaded, so other deployments
    // don't carry empty `tiles3d_*` series.
    if !state.tiles_3d.load().volume_engines.is_empty() {
        TILES3D_CONTENT_CACHE_METRICS.update(api_3dtiles::content_cache_metrics(), None);
    }

    // GeoTIFF decoded-chunk cache (#463): process-global, like the ODIM
    // caches. Only emit when geotiff collections are loaded, so other
    // deployments don't carry empty `geotiff_decoded_chunk_*` series.
    let has_geotiff = !state
        .geotiff_engines
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty();
    if has_geotiff {
        GEOTIFF_DECODED_CHUNK_CACHE_METRICS
            .update(engine_geotiff::decoded_chunk_cache_metrics(), None);
    }

    // Lightning strike-window cache (#504): only meaningful once a postgis
    // events collection has rendered, but the family is cheap either way.
    if !state
        .postgis_engines
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty()
    {
        LIGHTNING_STRIKE_CACHE_METRICS.update(engine_postgis::strike_window_cache_metrics(), None);
    }

    // Tile cache: per-collection
    if let Ok(engines) = state.geotiff_engines.read() {
        for engine in engines.iter() {
            let collection = engine.collection_id();
            let (hits, misses) = engine.tile_cache_stats();
            let entry = counter_state
                .tile
                .entry(collection.to_string())
                .or_insert((0, 0));
            if hits < entry.0 || misses < entry.1 {
                *entry = (hits, misses);
            } else {
                let dh = hits - entry.0;
                let dm = misses - entry.1;
                if dh > 0 {
                    TILE_CACHE_HITS.with_label_values(&[collection]).inc_by(dh);
                }
                if dm > 0 {
                    TILE_CACHE_MISSES
                        .with_label_values(&[collection])
                        .inc_by(dm);
                }
                *entry = (hits, misses);
            }

            let (bytes_used, capacity_bytes, entries) = engine.tile_cache_utilization();
            TILE_CACHE_BYTES
                .with_label_values(&[collection])
                .set(bytes_used as i64);
            TILE_CACHE_CAPACITY_BYTES
                .with_label_values(&[collection])
                .set(capacity_bytes as i64);
            TILE_CACHE_ENTRIES
                .with_label_values(&[collection])
                .set(entries as i64);

            // Update per-collection storage bytes
            let bytes = engine.storage_bytes_read();
            if bytes > 0 {
                STORAGE_BYTES_READ
                    .with_label_values(&[collection, "geotiff"])
                    .reset();
                STORAGE_BYTES_READ
                    .with_label_values(&[collection, "geotiff"])
                    .inc_by(bytes);
            }
        }
    }

    // GRIB grid cache: per-collection
    if let Ok(engines) = state.grib_engines.read() {
        for engine in engines.iter() {
            let collection = engine.collection_id();
            let (hits, misses) = engine.grid_cache_stats();
            let entry = counter_state
                .grid
                .entry(collection.to_string())
                .or_insert((0, 0));
            if hits < entry.0 || misses < entry.1 {
                *entry = (hits, misses);
            } else {
                let dh = hits - entry.0;
                let dm = misses - entry.1;
                if dh > 0 {
                    GRID_CACHE_HITS.with_label_values(&[collection]).inc_by(dh);
                }
                if dm > 0 {
                    GRID_CACHE_MISSES
                        .with_label_values(&[collection])
                        .inc_by(dm);
                }
                *entry = (hits, misses);
            }

            let (bytes_used, capacity_bytes, entries) = engine.grid_cache_utilization();
            GRID_CACHE_BYTES
                .with_label_values(&[collection])
                .set(bytes_used as i64);
            GRID_CACHE_CAPACITY_BYTES
                .with_label_values(&[collection])
                .set(capacity_bytes as i64);
            GRID_CACHE_ENTRIES
                .with_label_values(&[collection])
                .set(entries as i64);

            let bytes = engine.storage_bytes_read();
            if bytes > 0 {
                STORAGE_BYTES_READ
                    .with_label_values(&[collection, "grib"])
                    .reset();
                STORAGE_BYTES_READ
                    .with_label_values(&[collection, "grib"])
                    .inc_by(bytes);
            }
        }
    }

    // PostGIS per-collection / per-pool metrics (#110). Gauges (up, pool stats,
    // last-refresh duration) are set live each scrape. The three cumulative
    // counts are real counters with rebaseline-on-reset delta-tracking: engines
    // are replaced on reload (their in-engine counts reset to 0), so a backward
    // step rebaselines the delta to keep the Prometheus counter monotonic — no
    // saw-tooth that would clamp `rate()`/`increase()` to 0. Multiple
    // collections can share a pool_key; they set the same pool gauge (idempotent).
    {
        let engines = state
            .postgis_engines
            .read()
            .unwrap_or_else(|e| e.into_inner());
        for engine in engines.iter() {
            let cid = engine.collection_id();
            let snap = engine.health_snapshot();
            // Only emit postgis_up once the first ping has run — before that the
            // status is the optimistic seed, and emitting `1` would disagree with
            // `/health` (which keeps the boot snapshot until probed).
            if snap.probed {
                POSTGIS_UP.with_label_values(&[cid]).set(match snap.status {
                    engine_postgis::HealthStatus::Ready => 1,
                    engine_postgis::HealthStatus::Degraded => 0,
                });
            }
            POSTGIS_METADATA_REFRESH_SECONDS
                .with_label_values(&[cid])
                .set(snap.last_refresh_secs);

            let cur = (
                snap.refresh_total,
                snap.refresh_failures,
                snap.ping_total,
                snap.ping_failures,
            );
            let last = counter_state
                .postgis
                .get(cid)
                .copied()
                .unwrap_or((0, 0, 0, 0));
            // Rebaseline (treat last as 0) if any count went backward — the engine
            // was replaced on reload — so the delta is never negative.
            let base = if cur.0 < last.0 || cur.1 < last.1 || cur.2 < last.2 || cur.3 < last.3 {
                (0, 0, 0, 0)
            } else {
                last
            };
            POSTGIS_METADATA_REFRESHES_TOTAL
                .with_label_values(&[cid])
                .inc_by(cur.0 - base.0);
            POSTGIS_METADATA_REFRESH_FAILURES_TOTAL
                .with_label_values(&[cid])
                .inc_by(cur.1 - base.1);
            POSTGIS_PINGS_TOTAL
                .with_label_values(&[cid])
                .inc_by(cur.2 - base.2);
            POSTGIS_PING_FAILURES_TOTAL
                .with_label_values(&[cid])
                .inc_by(cur.3 - base.3);
            counter_state.postgis.insert(cid.to_string(), cur);

            let pk = engine.pool_key_label();
            let st = engine.pool().status();
            POSTGIS_POOL_SIZE
                .with_label_values(&[pk])
                .set(st.size as i64);
            POSTGIS_POOL_MAX_SIZE
                .with_label_values(&[pk])
                .set(st.max_size as i64);
            POSTGIS_POOL_AVAILABLE
                .with_label_values(&[pk])
                .set(st.available as i64);
            POSTGIS_POOL_WAITING
                .with_label_values(&[pk])
                .set(st.waiting as i64);
        }
    }

    // Nowcast: per-collection generation counters (delta pattern — engines
    // are replaced on reload, detected as a backward step) and gauges.
    if let Ok(engines) = state.nowcast_engines.read() {
        for engine in engines.iter() {
            let collection = engine.collection_id();
            let (generations, failures, last_ms, lag_secs, retained, frames) = engine.metrics();
            let entry = counter_state
                .nowcast
                .entry(collection.to_string())
                .or_insert((0, 0));
            if generations < entry.0 || failures < entry.1 {
                *entry = (generations, failures);
            } else {
                let dg = generations - entry.0;
                let df = failures - entry.1;
                if dg > 0 {
                    NOWCAST_GENERATIONS
                        .with_label_values(&[collection])
                        .inc_by(dg);
                }
                if df > 0 {
                    NOWCAST_GENERATION_FAILURES
                        .with_label_values(&[collection])
                        .inc_by(df);
                }
                *entry = (generations, failures);
            }
            NOWCAST_LAST_GENERATION_MS
                .with_label_values(&[collection])
                .set(last_ms as i64);
            NOWCAST_SOURCE_LAG_SECONDS
                .with_label_values(&[collection])
                .set(lag_secs as i64);
            NOWCAST_RETAINED_GENERATIONS
                .with_label_values(&[collection])
                .set(retained as i64);
            NOWCAST_FRAMES
                .with_label_values(&[collection])
                .set(frames as i64);
            if let Some((csi, persistence)) = engine.skill_permille() {
                NOWCAST_LEAD1_CSI
                    .with_label_values(&[collection])
                    .set(csi as i64);
                NOWCAST_LEAD1_PERSISTENCE_CSI
                    .with_label_values(&[collection])
                    .set(persistence as i64);
            }
        }
    }

    drop(counter_state);

    let encoder = TextEncoder::new();
    let metric_families = REGISTRY.gather();
    let mut buffer = Vec::new();
    encoder.encode(&metric_families, &mut buffer).unwrap();
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        buffer,
    )
}

/// Middleware that records HTTP request metrics.
pub async fn metrics_middleware(
    matched_path: Option<MatchedPath>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let start = Instant::now();
    let method = req.method().to_string();
    let path = matched_path
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "unmatched".to_string());

    let response = next.run(req).await;

    let duration = start.elapsed().as_secs_f64();
    let status = response.status().as_u16().to_string();

    // Track response body size. Axum handlers like `Json`, `Bytes`, and
    // image responses return buffered bodies whose exact length is known
    // at response-construction time and exposed via `Body::size_hint()`.
    // The `Content-Length` HTTP header is NOT set on the response object
    // at this point — hyper adds it later when serializing to the wire —
    // so reading it here always returns `None`. `size_hint().exact()`
    // works directly on the in-memory body and covers every buffered
    // response (99% of the traffic this server serves). Streaming bodies
    // return `None` from size_hint and are silently skipped, which is
    // acceptable — no handler in this codebase streams.
    if let Some(len) = http_body::Body::size_hint(response.body()).exact() {
        HTTP_RESPONSE_BYTES
            .with_label_values(&[&method, &path])
            .inc_by(len);
    }

    HTTP_REQUESTS_TOTAL
        .with_label_values(&[&method, &path, &status])
        .inc();
    HTTP_REQUEST_DURATION
        .with_label_values(&[&method, &path])
        .observe(duration);

    response
}

/// Derive (api, collection_id, query_type) from a real URI path and the
/// matched route template.
///
/// Examples:
///   `/edr/collections/weather/position`, `/edr/collections/{id}/position`
///     -> ("edr", Some("weather"), "position")
///   `/tiles/collections/radar/tiles/WebMercatorQuad/5/10/20`
///     -> ("tiles", Some("radar"), "tiles")
///   `/features/collections/roads/items/42`
///     -> ("features", Some("roads"), "items")
///   `/health` -> ("", None, "health")
///   `/wms`    -> ("wms", None, "")
fn classify_route<'a>(
    uri_path: &'a str,
    matched: Option<&'a str>,
) -> (&'a str, Option<&'a str>, &'a str) {
    let segs: Vec<&str> = uri_path.trim_matches('/').split('/').collect();
    let api = match segs.first().copied() {
        Some("edr") | Some("features") | Some("wms") | Some("maps") | Some("tiles") => segs[0],
        _ => "",
    };

    let collection_id = if segs.get(1) == Some(&"collections") {
        segs.get(2).copied().filter(|s| !s.is_empty())
    } else {
        None
    };

    // Derive the query/operation type from the matched route template,
    // falling back to the real URI path when no template is available.
    // For e.g. `/edr/collections/{id}/position` this yields "position".
    let template = matched.unwrap_or(uri_path);
    let query_type = template
        .trim_matches('/')
        .rsplit('/')
        .find(|s| !s.is_empty() && !s.starts_with('{'))
        .unwrap_or("");

    (api, collection_id, query_type)
}

/// Request ID attached to an incoming request via extensions so that
/// downstream middleware and handlers can correlate logs with the wire
/// `X-Request-ID` header.
#[derive(Clone, Debug)]
pub struct RequestId(pub String);

/// Maximum length of an incoming `X-Request-ID` header we will trust.
/// Anything longer is replaced with a fresh UUID to bound log-line size.
const MAX_REQUEST_ID_LEN: usize = 128;

/// Returns true if the string is a plausible request ID — only printable
/// ASCII, no control characters, bounded length. This keeps malicious
/// clients from injecting newlines or other log-forgery payloads through
/// the `X-Request-ID` header.
fn is_safe_request_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_REQUEST_ID_LEN
        && s.chars()
            .all(|c| c.is_ascii_graphic() || c == ' ' || c == '-' || c == '_')
}

/// Middleware that assigns a correlation ID to every request.
///
/// - Reads `X-Request-ID` from the incoming headers if present and safe;
///   otherwise generates a fresh UUIDv4.
/// - Stores the ID in request extensions as [`RequestId`] so downstream
///   middleware and handlers can attach it to logs.
/// - Wraps the downstream future in a tracing span carrying the ID, so
///   any log events emitted from inside the request inherit it.
/// - Echoes the final ID back to the client as `X-Request-ID`.
pub async fn correlation_id_middleware(
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use tracing::Instrument;

    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| is_safe_request_id(s))
        .map(|s| s.to_string())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    req.extensions_mut().insert(RequestId(request_id.clone()));

    let span = tracing::info_span!("http_request", request_id = %request_id);
    let mut response = next.run(req).instrument(span).await;

    if let Ok(header_value) = request_id.parse() {
        response.headers_mut().insert("x-request-id", header_value);
    }

    response
}

/// Middleware that emits one structured INFO log line per HTTP request.
///
/// Fields: request_id, method, path, route (template), api, collection,
/// query_type, query (raw query string), status, duration_ms, result_size
/// (bytes from Content-Length header if present).
pub async fn request_logging_middleware(
    matched_path: Option<MatchedPath>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let start = Instant::now();
    let method = req.method().to_string();
    let uri_path = req.uri().path().to_string();
    let query = req.uri().query().map(|q| q.to_string());
    let matched = matched_path.as_ref().map(|p| p.as_str().to_string());
    let request_id = req
        .extensions()
        .get::<RequestId>()
        .map(|id| id.0.clone())
        .unwrap_or_default();

    let response = next.run(req).await;

    let duration_ms = start.elapsed().as_secs_f64() * 1000.0;
    let status = response.status().as_u16();
    let result_size = response
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    let (api, collection_id, query_type) = classify_route(&uri_path, matched.as_deref());

    // For ≥400 responses, API error types attach a ds_core::error::ErrorReason
    // to the response extensions so the reason survives past IntoResponse (which
    // otherwise only leaves it in the response body). The field is omitted on
    // success so 2xx logs aren't padded with `error = ""` — keeps Loki ingest
    // smaller and `| json | error != ""` filters readable.
    let error_reason = response
        .extensions()
        .get::<ds_core::error::ErrorReason>()
        .map(|r| r.0.clone());
    let duration_str = format!("{duration_ms:.3}");
    let route_str = matched.as_deref().unwrap_or("");
    let collection_str = collection_id.unwrap_or("");
    let query_str = query.as_deref().unwrap_or("");

    // Local macro keeps the field list in one place: future fields like
    // `user_agent` or `cache_hit` get added once and apply to both arms.
    // tracing's macro requires the field list at the call site, so we splice
    // an optional `error = …,` token tree depending on whether a reason was
    // attached.
    macro_rules! log_request {
        ($($error_field:tt)*) => {
            tracing::info!(
                request_id = %request_id,
                method = %method,
                path = %uri_path,
                route = route_str,
                api = api,
                collection = collection_str,
                query_type = query_type,
                query = query_str,
                status = status,
                duration_ms = duration_str,
                result_size = result_size,
                $($error_field)*
                "request"
            );
        };
    }

    if let Some(reason) = error_reason {
        log_request!(error = %reason,);
    } else {
        log_request!();
    }

    response
}

#[cfg(test)]
mod tests {
    use super::{classify_route, collection_layer_styles, is_safe_request_id};
    use ds_core::config::{CollectionConfig, StyleBundle};
    use std::collections::HashMap;
    use std::sync::Arc;

    /// Resolve a collection's full style-layer map through a fresh
    /// [`ds_render::StyleContext`] + empty cache — the test-side stand-in for
    /// the per-load resolution `load_collections` performs.
    fn layer_styles(
        collection: &CollectionConfig,
        param_names: &[(String, String)],
        bundles: &HashMap<&str, &StyleBundle>,
    ) -> HashMap<String, HashMap<String, ds_render::StyleInfo>> {
        let ctx = ds_render::StyleContext::with_builtins();
        let mut cache = HashMap::new();
        collection_layer_styles(&ctx, &mut cache, collection, param_names, bundles)
    }

    // --- nowcast second-pass wiring (#522) ---

    /// A minimal collection config for the nowcast wiring tests.
    fn nowcast_test_collection(
        id: &str,
        engine_type: &str,
        source: Option<&str>,
    ) -> CollectionConfig {
        CollectionConfig {
            id: id.to_string(),
            title: id.to_string(),
            description: String::new(),
            data_path: None,
            apis: vec!["wms".to_string()],
            engine_type: engine_type.to_string(),
            keywords: Vec::new(),
            license: None,
            geotiff: None,
            querydata: None,
            wms: None,
            grib: None,
            zarr: None,
            odim: None,
            cap: None,
            postgis: None,
            nowcast: source.map(|s| ds_core::config::NowcastConfig {
                source: s.to_string(),
                horizon: "PT1H".to_string(),
                step: None,
                history_frames: 2,
                poll_interval_secs: 30,
                max_generations: 2,
                max_pixels: 500_000,
                min_echo: 10.0,
                growth_decay: false,
                lightning_source: None,
            }),
            preview: None,
        }
    }

    /// A geotiff source collection over the committed TM35FIN fixture.
    fn tm35_source_collection(id: &str) -> CollectionConfig {
        let mut c = nowcast_test_collection(id, "geotiff", None);
        c.data_path = Some("../../testdata/radar-tm35fin".to_string());
        c.geotiff = Some(ds_core::config::GeoTiffConfig {
            filename_template: Some("radar_tm35_%Y%m%dT%H%MZ.tif".to_string()),
            filename_pattern: None,
            timestamp_format: None,
            parameter: "reflectivity".to_string(),
            unit: "dBZ".to_string(),
            poll_interval_secs: 3600,
            tile_cache_mb: 16,
            band: 1,
            max_files: None,
            nodata: None,
            scale: None,
            offset: None,
            exclude_patterns: vec![],
            endpoint: None,
            bucket: None,
            prefix_pattern: None,
            time_window: None,
            scan_days: None,
            stac_url: None,
            stac_asset_key: "data".to_string(),
            stac_asset_allowlist: None,
        });
        c
    }

    fn health_of<'a>(result: &'a super::LoadResult, id: &str) -> &'a super::CollectionHealth {
        result
            .health
            .iter()
            .find(|h| h.id == id)
            .unwrap_or_else(|| panic!("no health entry for {id}"))
    }

    #[test]
    fn nowcast_unknown_source_fails_that_collection_only() {
        let result = super::load_collections(
            &ds_render::StyleContext::with_builtins(),
            &[nowcast_test_collection("nc", "nowcast", Some("no-such"))],
            &[],
            "http://x",
            false,
            0,
            super::ReusableCaches::default(),
        );
        let h = health_of(&result, "nc");
        assert_eq!(h.status, super::CollectionStatus::Failed);
        assert!(
            h.error.as_deref().unwrap_or("").contains("not found"),
            "error should name the missing source: {:?}",
            h.error
        );
        assert!(!result.wms_state.engines.contains_key("nc"));
    }

    #[test]
    fn nowcast_missing_lightning_source_fails_that_collection() {
        // The #549 safety property: a lightning_source that names no
        // events-shape collection fails the nowcast collection at load —
        // never silently serving cells without flash data. (A station-
        // shape postgis collection takes the same path: it is never
        // entered into the event-source registry, so the lookup misses.)
        let mut nc = nowcast_test_collection("nc", "nowcast", Some("radar"));
        nc.nowcast.as_mut().unwrap().lightning_source = Some("no-such-lightning".into());
        let result = super::load_collections(
            &ds_render::StyleContext::with_builtins(),
            &[tm35_source_collection("radar"), nc],
            &[],
            "http://x",
            false,
            0,
            super::ReusableCaches::default(),
        );
        let h = health_of(&result, "nc");
        assert_eq!(h.status, super::CollectionStatus::Failed);
        assert!(
            h.error
                .as_deref()
                .unwrap_or("")
                .contains("lightning_source"),
            "error should name the missing lightning source: {:?}",
            h.error
        );
        assert!(!result.wms_state.engines.contains_key("nc"));
        // The base collection is unaffected.
        assert!(result.wms_state.engines.contains_key("radar"));
    }

    #[test]
    fn nowcast_of_nowcast_is_rejected() {
        let result = super::load_collections(
            &ds_render::StyleContext::with_builtins(),
            &[
                tm35_source_collection("radar"),
                nowcast_test_collection("nc1", "nowcast", Some("radar")),
                nowcast_test_collection("nc2", "nowcast", Some("nc1")),
            ],
            &[],
            "http://x",
            false,
            0,
            super::ReusableCaches::default(),
        );
        let h = health_of(&result, "nc2");
        assert_eq!(h.status, super::CollectionStatus::Failed);
        assert!(
            h.error.as_deref().unwrap_or("").contains("chaining"),
            "error should name the chaining rejection: {:?}",
            h.error
        );
        assert!(!result.wms_state.engines.contains_key("nc2"));
        // The first-level nowcast is unaffected.
        assert!(result.wms_state.engines.contains_key("nc1"));
    }

    #[test]
    fn nowcast_wires_into_registries_and_boots_degraded() {
        let mut nc = nowcast_test_collection("nc", "nowcast", Some("radar"));
        nc.apis = vec!["wms".to_string(), "features".to_string()];
        let result = super::load_collections(
            &ds_render::StyleContext::with_builtins(),
            &[tm35_source_collection("radar"), nc],
            &[],
            "http://x",
            false,
            0,
            super::ReusableCaches::default(),
        );
        assert!(result.wms_state.engines.contains_key("nc"));
        assert!(
            result.features_state.engines.contains_key("nc"),
            "features API must be wired when listed in apis"
        );
        assert_eq!(result.nowcast_engines.len(), 1);
        assert_eq!(result.nowcast_engines[0].source_id(), "radar");
        let h = health_of(&result, "nc");
        assert_eq!(h.status, super::CollectionStatus::Degraded);
        assert!(
            h.error
                .as_deref()
                .unwrap_or("")
                .contains("waiting for first nowcast generation"),
            "boot health should explain the degraded state: {:?}",
            h.error
        );
    }

    // --- reload preserves the warm render caches (ReusableCaches) ---

    #[test]
    fn reload_reuses_render_caches_when_size_unchanged() {
        // Startup-equivalent: fresh caches (empty collections is fine — the
        // render caches are built unconditionally).
        let first = super::load_collections(
            &ds_render::StyleContext::with_builtins(),
            &[],
            &[],
            "http://x",
            false,
            64,
            super::ReusableCaches::default(),
        );
        let tile0 = first.wms_state.tile_cache.clone();
        let rendered0 = first.wms_state.rendered_cache.clone();
        let vector0 = first.tiles_state.vector_tile_cache.clone();

        // Reload with the SAME meta-tile size → every cache is reused (same
        // `Arc`), so a warm cache survives a reload instead of being dumped.
        let second = super::load_collections(
            &ds_render::StyleContext::with_builtins(),
            &[],
            &[],
            "http://x",
            false,
            64,
            super::ReusableCaches {
                rendered: Some(rendered0.clone()),
                tile: Some(tile0.clone()),
                vector: Some(vector0.clone()),
            },
        );
        assert!(
            Arc::ptr_eq(&tile0, &second.wms_state.tile_cache),
            "tile cache reused"
        );
        assert!(
            Arc::ptr_eq(&rendered0, &second.wms_state.rendered_cache),
            "rendered cache reused"
        );
        assert!(
            Arc::ptr_eq(&vector0, &second.tiles_state.vector_tile_cache),
            "vector cache reused"
        );

        // Reload with a CHANGED meta-tile size → the tile cache is rebuilt (a
        // differently-sized cache can't be reused); the unchanged rendered/
        // vector caches are still reused.
        let third = super::load_collections(
            &ds_render::StyleContext::with_builtins(),
            &[],
            &[],
            "http://x",
            false,
            128,
            super::ReusableCaches {
                rendered: Some(rendered0.clone()),
                tile: Some(tile0.clone()),
                vector: Some(vector0.clone()),
            },
        );
        assert!(
            !Arc::ptr_eq(&tile0, &third.wms_state.tile_cache),
            "tile cache rebuilt on size change"
        );
        assert!(
            Arc::ptr_eq(&rendered0, &third.wms_state.rendered_cache),
            "rendered cache unchanged → reused"
        );
        assert!(
            Arc::ptr_eq(&vector0, &third.tiles_state.vector_tile_cache),
            "vector cache unchanged → reused"
        );
    }

    // (The maybe_wrap_integer_lut (#207) unit tests moved with the function
    // into ds-render — see crates/render/src/style.rs.)

    #[test]
    fn accepts_typical_uuid() {
        assert!(is_safe_request_id("550e8400-e29b-41d4-a716-446655440000"));
    }

    #[test]
    fn accepts_alphanumeric_with_underscores() {
        assert!(is_safe_request_id("trace_12345"));
        assert!(is_safe_request_id("req-abc-xyz"));
    }

    #[test]
    fn rejects_empty() {
        assert!(!is_safe_request_id(""));
    }

    #[test]
    fn rejects_newlines_and_control_chars() {
        // Prevents log forgery via CRLF injection.
        assert!(!is_safe_request_id("abc\ndef"));
        assert!(!is_safe_request_id("abc\rdef"));
        assert!(!is_safe_request_id("abc\tdef"));
        assert!(!is_safe_request_id("abc\x00def"));
    }

    #[test]
    fn rejects_oversized_ids() {
        let huge = "a".repeat(256);
        assert!(!is_safe_request_id(&huge));
    }

    #[test]
    fn classifies_edr_position_query() {
        let (api, coll, qt) = classify_route(
            "/edr/collections/weather/position",
            Some("/edr/collections/{id}/position"),
        );
        assert_eq!(api, "edr");
        assert_eq!(coll, Some("weather"));
        assert_eq!(qt, "position");
    }

    #[test]
    fn classifies_features_item() {
        let (api, coll, qt) = classify_route(
            "/features/collections/roads/items/42",
            Some("/features/collections/{id}/items/{feature_id}"),
        );
        assert_eq!(api, "features");
        assert_eq!(coll, Some("roads"));
        assert_eq!(qt, "items");
    }

    #[test]
    fn classifies_tiles_get_tile() {
        let (api, coll, qt) = classify_route(
            "/tiles/collections/radar/tiles/WebMercatorQuad/5/10/20",
            Some(
                "/tiles/collections/{id}/tiles/{tileMatrixSetId}/{tileMatrix}/{tileRow}/{tileCol}",
            ),
        );
        assert_eq!(api, "tiles");
        assert_eq!(coll, Some("radar"));
        assert_eq!(qt, "tiles");
    }

    #[test]
    fn classifies_maps_get_map() {
        let (api, coll, qt) = classify_route(
            "/maps/collections/radar/map",
            Some("/maps/collections/{id}/map"),
        );
        assert_eq!(api, "maps");
        assert_eq!(coll, Some("radar"));
        assert_eq!(qt, "map");
    }

    #[test]
    fn classifies_wms_root() {
        let (api, coll, qt) = classify_route("/wms", Some("/wms"));
        assert_eq!(api, "wms");
        assert_eq!(coll, None);
        assert_eq!(qt, "wms");
    }

    #[test]
    fn classifies_health() {
        let (api, coll, qt) = classify_route("/health", Some("/health"));
        assert_eq!(api, "");
        assert_eq!(coll, None);
        assert_eq!(qt, "health");
    }

    #[test]
    fn classifies_unmatched_fallback() {
        let (api, coll, qt) = classify_route("/edr/collections/weather/position", None);
        assert_eq!(api, "edr");
        assert_eq!(coll, Some("weather"));
        assert_eq!(qt, "position");
    }

    #[test]
    fn collection_styles_expand_bundle_into_default_plus_extras() {
        let collection: CollectionConfig = toml::from_str(
            r#"
id = "radar-dwd"
title = "DWD"
description = "DWD"
engine_type = "geotiff"

[geotiff]
filename_template = "radar_%Y%m%dT%H%MZ.tif"
parameter = "reflectivity"
unit = "dBZ"
data_path = "/tmp"

[wms]
style_bundle = "radar_multi"
"#,
        )
        .unwrap();

        let bundle: StyleBundle = toml::from_str(
            r#"
id = "radar_multi"

[default]
colormap = "radar_bookbinder"

[[extras]]
name = "radar_dbz"
title = "MeteoCore Radar"
colormap = "radar_dbz"

[[extras]]
name = "radar_fmi"
title = "FMI Radar"
colormap = "radar_fmi"
"#,
        )
        .unwrap();

        let bundles = [bundle];
        let index: HashMap<&str, &StyleBundle> =
            bundles.iter().map(|b| (b.id.as_str(), b)).collect();

        let layers = layer_styles(&collection, &[], &index);
        assert_eq!(layers.len(), 1, "no params → collection layer only");
        let styles = &layers["radar-dwd"];
        assert_eq!(styles.len(), 3, "default + 2 extras expected");
        assert!(styles.contains_key("default"));
        assert_eq!(styles["default"].name, "default");
        assert!(styles.contains_key("radar_dbz"));
        assert_eq!(styles["radar_dbz"].title, "MeteoCore Radar");
        assert!(styles.contains_key("radar_fmi"));
        assert_eq!(styles["radar_fmi"].title, "FMI Radar");
    }

    #[test]
    fn collection_styles_fall_back_to_inline_when_no_bundle_referenced() {
        let collection: CollectionConfig = toml::from_str(
            r#"
id = "radar-fmi"
title = "FMI"
description = "FMI"
engine_type = "geotiff"

[geotiff]
filename_template = "radar_%Y%m%dT%H%MZ.tif"
parameter = "reflectivity"
unit = "dBZ"
data_path = "/tmp"

[wms]
colormap = "radar_dbz"

[[wms.styles]]
name = "alt"
title = "Alt"
colormap = "grayscale"
"#,
        )
        .unwrap();

        let index: HashMap<&str, &StyleBundle> = HashMap::new();

        let styles = &layer_styles(&collection, &[], &index)["radar-fmi"];
        assert_eq!(styles.len(), 2);
        assert!(styles.contains_key("default"));
        assert_eq!(
            (styles["default"].min, styles["default"].max),
            (0.0, 70.0),
            "default min/max come from the radar_dbz stops"
        );
        assert!(styles.contains_key("alt"));
        assert_eq!(styles["alt"].title, "Alt");
    }

    #[test]
    fn collection_styles_fall_back_when_bundle_ref_unknown() {
        // Exercises the defensive path in resolve_bundle: validate() normally
        // rejects unresolved refs, but if a caller skips validation the
        // collection must still load with the inline default (viridis) rather
        // than panic.
        let collection: CollectionConfig = toml::from_str(
            r#"
id = "radar-x"
title = "X"
description = "X"
engine_type = "geotiff"

[geotiff]
filename_template = "radar_%Y%m%dT%H%MZ.tif"
parameter = "reflectivity"
unit = "dBZ"
data_path = "/tmp"

[wms]
style_bundle = "does_not_exist"
"#,
        )
        .unwrap();

        let index: HashMap<&str, &StyleBundle> = HashMap::new();
        let styles = &layer_styles(&collection, &[], &index)["radar-x"];

        // Only the default; no extras; the bundle was silently skipped.
        assert_eq!(styles.len(), 1);
        assert!(styles.contains_key("default"));
        assert_eq!(styles["default"].palette.name, "viridis");
        assert_eq!((styles["default"].min, styles["default"].max), (0.0, 1.0));
    }

    #[test]
    fn collection_styles_parameter_tagged_extras_stay_in_collection_map() {
        // The collection-level map returns every extra — scoping by
        // parameter happens in the per-parameter layer maps. This locks the
        // bundle surface stable AND pins the scoping outcome.
        let collection: CollectionConfig = toml::from_str(
            r#"
id = "multi"
title = "Multi"
description = "Multi"
engine_type = "querydata"

[querydata]

[wms]
style_bundle = "mixed"
"#,
        )
        .unwrap();

        let bundle: StyleBundle = toml::from_str(
            r#"
id = "mixed"

[default]
colormap = "viridis"

[[extras]]
name = "wind_only"
colormap = "wind_speed"
parameter = "wind_speed"

[[extras]]
name = "global"
colormap = "grayscale"
"#,
        )
        .unwrap();

        let bundles = [bundle];
        let index: HashMap<&str, &StyleBundle> =
            bundles.iter().map(|b| (b.id.as_str(), b)).collect();
        let params = vec![
            ("wind_speed".to_string(), "Wind speed".to_string()),
            ("t2m".to_string(), "Temperature".to_string()),
        ];
        let layers = layer_styles(&collection, &params, &index);

        let styles = &layers["multi"];
        assert_eq!(styles.len(), 3);
        assert_eq!(styles["wind_only"].parameter.as_deref(), Some("wind_speed"));
        assert!(styles["global"].parameter.is_none());

        // Parameter scoping: the tagged extra only reaches its own layer;
        // the untagged extra reaches every layer.
        let wind = &layers["multi/wind_speed"];
        assert!(wind.contains_key("wind_only"));
        assert!(wind.contains_key("global"));
        let t2m = &layers["multi/t2m"];
        assert!(!t2m.contains_key("wind_only"));
        assert!(t2m.contains_key("global"));
    }

    #[test]
    fn odim_volume_cells_layer_gets_track_overlay_wrap() {
        // The derived storm-cell overlay (#367): for an odim-volume
        // collection, the CELLS parameter layer's colormaps must render the
        // reserved track sentinel as the fixed neutral colour.
        let collection: CollectionConfig = toml::from_str(
            r#"
id = "pvol-fivih"
title = "Vihti"
description = "Vihti PVOL"
engine_type = "odim-volume"

[odim]

[wms]
colormap = "radar_dbz"
"#,
        )
        .unwrap();
        let index: HashMap<&str, &StyleBundle> = HashMap::new();
        let params = vec![(
            engine_odim::cells::CELLS_PARAMETER.to_string(),
            "Storm cells".to_string(),
        )];
        let layers = layer_styles(&collection, &params, &index);
        let cells_default = &layers["pvol-fivih/CELLS"]["default"];
        assert_eq!(
            cells_default
                .colormap
                .color(Some(engine_odim::cells::CELLS_TRACK_SENTINEL)),
            engine_odim::cells::CELLS_TRACK_COLOR,
            "CELLS layer must render the track sentinel as the overlay colour"
        );

        // A non-ODIM collection with a band coincidentally named CELLS must
        // NOT get the overlay wrap (#410 gate).
        let plain: CollectionConfig = toml::from_str(
            r#"
id = "qd"
title = "QD"
description = "QD"
engine_type = "querydata"

[querydata]

[wms]
colormap = "radar_dbz"
"#,
        )
        .unwrap();
        let layers = layer_styles(&plain, &params, &index);
        let cells_default = &layers["qd/CELLS"]["default"];
        assert_ne!(
            cells_default
                .colormap
                .color(Some(engine_odim::cells::CELLS_TRACK_SENTINEL)),
            engine_odim::cells::CELLS_TRACK_COLOR,
            "non-ODIM engines must not hijack the sentinel value"
        );
    }

    #[test]
    fn edr_state_gets_the_same_resolved_styles() {
        // Task-level wiring pin: a wms+edr collection's resolved styles land
        // in the EDR state (the f=png trajectory plot consumes them).
        let mut c = tm35_source_collection("radar");
        c.apis = vec!["edr".to_string(), "wms".to_string()];
        let result = super::load_collections(
            &ds_render::StyleContext::with_builtins(),
            &[c],
            &[],
            "http://x",
            false,
            0,
            super::ReusableCaches::default(),
        );
        let edr = &result.edr_state.styles["radar"];
        let wms = &result.wms_state.styles["radar"];
        assert!(edr.contains_key("default"));
        assert_eq!(edr.len(), wms.len(), "EDR and WMS share one resolution");
        assert_eq!(edr["default"].min, wms["default"].min);
        assert_eq!(edr["default"].max, wms["default"].max);
    }

    #[test]
    fn validate_style_colormaps_rejects_unknown_names() {
        // A typo'd colormap name is a config error at load, not a silent
        // viridis fallback.
        let bad: CollectionConfig = toml::from_str(
            r#"
id = "radar"
title = "R"
description = "R"
engine_type = "geotiff"

[geotiff]
filename_template = "radar_%Y%m%dT%H%MZ.tif"
parameter = "reflectivity"
unit = "dBZ"
data_path = "/tmp"

[wms]
colormap = "virids"
"#,
        )
        .unwrap();
        let err = super::validate_style_colormaps(
            std::slice::from_ref(&bad),
            &[],
            &ds_render::PaletteRegistry::with_builtins(),
        )
        .expect_err("typo'd colormap must be rejected");
        let msg = format!("{err}");
        assert!(msg.contains("virids"), "error names the typo: {msg}");
        assert!(msg.contains("radar"), "error names the collection: {msg}");

        // A valid name (and no wms at all) passes.
        let mut ok = bad.clone();
        ok.wms.as_mut().unwrap().colormap = Some("radar_dbz".into());
        super::validate_style_colormaps(
            std::slice::from_ref(&ok),
            &[],
            &ds_render::PaletteRegistry::with_builtins(),
        )
        .expect("valid name passes");

        // A bundle extra with an unknown name is rejected too.
        let bundle: StyleBundle = toml::from_str(
            r#"
id = "b"

[default]
colormap = "viridis"

[[extras]]
name = "x"
colormap = "no_such_map"
"#,
        )
        .unwrap();
        let err = super::validate_style_colormaps(
            &[],
            std::slice::from_ref(&bundle),
            &ds_render::PaletteRegistry::with_builtins(),
        )
        .expect_err("unknown bundle extra colormap must be rejected");
        assert!(format!("{err}").contains("no_such_map"));
    }
}
