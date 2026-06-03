use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::Instant;

use arc_swap::ArcSwap;
use axum::extract::{MatchedPath, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Opts,
    Registry, TextEncoder,
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

// Rendered image cache (global — shared across all collections that render).
static RENDERED_CACHE_HITS: LazyLock<IntCounter> = LazyLock::new(|| {
    let counter =
        IntCounter::new("rendered_cache_hits_total", "Rendered image cache hits").unwrap();
    REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

static RENDERED_CACHE_MISSES: LazyLock<IntCounter> = LazyLock::new(|| {
    let counter =
        IntCounter::new("rendered_cache_misses_total", "Rendered image cache misses").unwrap();
    REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

static RENDERED_CACHE_BYTES: LazyLock<IntGauge> = LazyLock::new(|| {
    let gauge = IntGauge::new(
        "rendered_cache_bytes",
        "Bytes currently held in the rendered image cache",
    )
    .unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

static RENDERED_CACHE_CAPACITY_BYTES: LazyLock<IntGauge> = LazyLock::new(|| {
    let gauge = IntGauge::new(
        "rendered_cache_capacity_bytes",
        "Configured rendered image cache capacity in bytes",
    )
    .unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

static RENDERED_CACHE_ENTRIES: LazyLock<IntGauge> = LazyLock::new(|| {
    let gauge = IntGauge::new(
        "rendered_cache_entries",
        "Number of entries currently in the rendered image cache",
    )
    .unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

// PVOL lazy pixel cache (#289 / PR #290) — global byte-bounded LRU of decoded
// radar moment arrays, shared across every per-site PVOL collection. The
// failures counter surfaces the otherwise-silent degradation when a moment's
// pixels can't be read/decoded at render time (was a hard catalog rejection
// before lazy loading).
static PVOL_PIXEL_CACHE_HITS: LazyLock<IntCounter> = LazyLock::new(|| {
    let counter = IntCounter::new("pvol_pixel_cache_hits_total", "PVOL pixel cache hits").unwrap();
    REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

static PVOL_PIXEL_CACHE_MISSES: LazyLock<IntCounter> = LazyLock::new(|| {
    let counter =
        IntCounter::new("pvol_pixel_cache_misses_total", "PVOL pixel cache misses").unwrap();
    REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

static PVOL_PIXEL_CACHE_BYTES: LazyLock<IntGauge> = LazyLock::new(|| {
    let gauge = IntGauge::new(
        "pvol_pixel_cache_bytes",
        "Bytes currently held in the PVOL pixel cache",
    )
    .unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

static PVOL_PIXEL_CACHE_CAPACITY_BYTES: LazyLock<IntGauge> = LazyLock::new(|| {
    let gauge = IntGauge::new(
        "pvol_pixel_cache_capacity_bytes",
        "Configured PVOL pixel cache capacity in bytes",
    )
    .unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

static PVOL_PIXEL_READ_FAILURES: LazyLock<IntCounter> = LazyLock::new(|| {
    let counter = IntCounter::new(
        "pvol_pixel_read_failures_total",
        "PVOL lazy pixel reads that failed (I/O or decode) and degraded to nodata",
    )
    .unwrap();
    REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

// Meta-tile pixel cache (#202) — global, decoded-RGBA tiles for the Web
// Mercator WMS meta-tiling path. Distinct from the per-collection GeoTIFF
// compressed-byte tile cache (`tile_cache_*`).
static METATILE_CACHE_HITS: LazyLock<IntCounter> = LazyLock::new(|| {
    let counter =
        IntCounter::new("metatile_cache_hits_total", "Meta-tile pixel cache hits").unwrap();
    REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

static METATILE_CACHE_MISSES: LazyLock<IntCounter> = LazyLock::new(|| {
    let counter = IntCounter::new(
        "metatile_cache_misses_total",
        "Meta-tile pixel cache misses",
    )
    .unwrap();
    REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

static METATILE_CACHE_BYTES: LazyLock<IntGauge> = LazyLock::new(|| {
    let gauge = IntGauge::new(
        "metatile_cache_bytes",
        "Bytes currently held in the meta-tile pixel cache",
    )
    .unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

static METATILE_CACHE_CAPACITY_BYTES: LazyLock<IntGauge> = LazyLock::new(|| {
    let gauge = IntGauge::new(
        "metatile_cache_capacity_bytes",
        "Configured meta-tile pixel cache capacity in bytes",
    )
    .unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

static METATILE_CACHE_ENTRIES: LazyLock<IntGauge> = LazyLock::new(|| {
    let gauge = IntGauge::new(
        "metatile_cache_entries",
        "Number of entries currently in the meta-tile pixel cache",
    )
    .unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
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
/// so that the metrics handler can convert cumulative cache counters into
/// monotonically-increasing Prometheus counters via delta tracking.
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
    rendered: (u64, u64),
    metatile: (u64, u64),
    /// PVOL pixel cache `(hits, misses, read_failures)` — global cache, never
    /// replaced on reload, so always monotonic.
    pvol_pixel: (u64, u64, u64),
}

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

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
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
    pub config_path: String,
    pub health: RwLock<Vec<CollectionHealth>>,
    pub geotiff_engines: RwLock<Vec<Arc<engine_geotiff::GeoTiffEngine>>>,
    pub querydata_engines: RwLock<Vec<Arc<engine_querydata::QueryDataEngine>>>,
    pub grib_engines: RwLock<Vec<Arc<engine_grib::GribEngine>>>,
    pub odim_engines: RwLock<Vec<Arc<engine_odim::OdimEngine>>>,
    pub odim_volume_engines: RwLock<Vec<Arc<engine_odim::PolarVolumeEngine>>>,
    pub postgis_engines: RwLock<Vec<Arc<engine_postgis::PostgisEngine>>>,
    /// Serializes reload requests to prevent concurrent reloads from racing.
    pub reload_lock: tokio::sync::Mutex<()>,
    /// Bearer token for admin endpoint authentication.
    /// If None, admin endpoints are disabled (return 403).
    pub admin_token: Option<String>,
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
    pub health: Vec<CollectionHealth>,
    pub geotiff_engines: Vec<Arc<engine_geotiff::GeoTiffEngine>>,
    pub querydata_engines: Vec<Arc<engine_querydata::QueryDataEngine>>,
    pub grib_engines: Vec<Arc<engine_grib::GribEngine>>,
    pub odim_engines: Vec<Arc<engine_odim::OdimEngine>>,
    pub odim_volume_engines: Vec<Arc<engine_odim::PolarVolumeEngine>>,
    pub postgis_engines: Vec<Arc<engine_postgis::PostgisEngine>>,
}

pub fn load_collections(
    collections: &[CollectionConfig],
    style_bundles: &[StyleBundle],
    base_url: &str,
    metatile_cache_mb: u64,
) -> LoadResult {
    let bundle_index: HashMap<&str, &StyleBundle> =
        style_bundles.iter().map(|b| (b.id.as_str(), b)).collect();
    let mut edr_engines: HashMap<String, Arc<dyn ds_core::edr_engine::EdrEngine>> = HashMap::new();
    let mut edr_collections: HashMap<String, CollectionConfig> = HashMap::new();
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
    let mut geotiff_engines: Vec<Arc<engine_geotiff::GeoTiffEngine>> = Vec::new();
    let mut querydata_engines: Vec<Arc<engine_querydata::QueryDataEngine>> = Vec::new();
    let mut grib_engines: Vec<Arc<engine_grib::GribEngine>> = Vec::new();
    let mut odim_engines: Vec<Arc<engine_odim::OdimEngine>> = Vec::new();
    let mut odim_volume_engines: Vec<Arc<engine_odim::PolarVolumeEngine>> = Vec::new();
    let mut postgis_engines: Vec<Arc<engine_postgis::PostgisEngine>> = Vec::new();
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
            "odim" => &["edr", "wms", "maps", "tiles"],
            "odim-volume" => &["edr", "wms", "maps", "tiles", "features"],
            "postgis" => &["edr", "features", "tiles"],
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
                }
                if collection.apis.contains(&"wms".to_string()) {
                    map_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    map_collections.insert(collection.id.clone(), collection.clone());

                    // Build styles from config
                    let styles = build_styles(collection, &bundle_index);
                    map_styles.insert(collection.id.clone(), styles);

                    info!("Collection '{}': wired to WMS API", collection.id);
                }
                if collection.apis.contains(&"maps".to_string()) {
                    maps_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    maps_collections.insert(collection.id.clone(), collection.clone());

                    let styles = build_styles(collection, &bundle_index);
                    maps_styles.insert(collection.id.clone(), styles);

                    info!("Collection '{}': wired to Maps API", collection.id);
                }
                if collection.apis.contains(&"tiles".to_string()) {
                    tiles_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    tiles_collections.insert(collection.id.clone(), collection.clone());

                    let styles = build_styles(collection, &bundle_index);
                    tiles_styles.insert(collection.id.clone(), styles);

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

                let engine = match engine_querydata::QueryDataEngine::new(
                    std::path::Path::new(data_path),
                    &collection.id,
                    wms_param,
                    poll_secs,
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

                if collection.apis.contains(&"edr".to_string()) {
                    edr_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::edr_engine::EdrEngine>,
                    );
                    edr_collections.insert(collection.id.clone(), collection.clone());
                }
                // Get parameter list for per-parameter-layer styles
                let raster_params =
                    ds_core::map_engine::MapEngine::raster_info(engine.as_ref()).parameters;

                if collection.apis.contains(&"wms".to_string()) {
                    map_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    map_collections.insert(collection.id.clone(), collection.clone());
                    let styles = build_styles(collection, &bundle_index);
                    map_styles.insert(collection.id.clone(), styles);
                    if !raster_params.is_empty() {
                        register_parameter_layer_styles(
                            collection,
                            &raster_params,
                            &mut map_styles,
                            &bundle_index,
                        );
                    }
                    info!("Collection '{}': wired to WMS API", collection.id);
                }
                if collection.apis.contains(&"maps".to_string()) {
                    maps_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    maps_collections.insert(collection.id.clone(), collection.clone());
                    let styles = build_styles(collection, &bundle_index);
                    maps_styles.insert(collection.id.clone(), styles);
                    if !raster_params.is_empty() {
                        register_parameter_layer_styles(
                            collection,
                            &raster_params,
                            &mut maps_styles,
                            &bundle_index,
                        );
                    }
                    info!("Collection '{}': wired to Maps API", collection.id);
                }
                if collection.apis.contains(&"tiles".to_string()) {
                    tiles_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    tiles_collections.insert(collection.id.clone(), collection.clone());
                    let styles = build_styles(collection, &bundle_index);
                    tiles_styles.insert(collection.id.clone(), styles);
                    if !raster_params.is_empty() {
                        register_parameter_layer_styles(
                            collection,
                            &raster_params,
                            &mut tiles_styles,
                            &bundle_index,
                        );
                    }
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

                if collection.apis.contains(&"edr".to_string()) {
                    edr_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::edr_engine::EdrEngine>,
                    );
                    edr_collections.insert(collection.id.clone(), collection.clone());
                }
                // Get parameter list for per-parameter-layer styles
                let raster_params =
                    ds_core::map_engine::MapEngine::raster_info(engine.as_ref()).parameters;

                if collection.apis.contains(&"wms".to_string()) {
                    map_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    map_collections.insert(collection.id.clone(), collection.clone());
                    let styles = build_styles(collection, &bundle_index);
                    map_styles.insert(collection.id.clone(), styles);
                    if !raster_params.is_empty() {
                        register_parameter_layer_styles(
                            collection,
                            &raster_params,
                            &mut map_styles,
                            &bundle_index,
                        );
                    }
                    info!("Collection '{}': wired to WMS API", collection.id);
                }
                if collection.apis.contains(&"maps".to_string()) {
                    maps_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    maps_collections.insert(collection.id.clone(), collection.clone());
                    let styles = build_styles(collection, &bundle_index);
                    maps_styles.insert(collection.id.clone(), styles);
                    if !raster_params.is_empty() {
                        register_parameter_layer_styles(
                            collection,
                            &raster_params,
                            &mut maps_styles,
                            &bundle_index,
                        );
                    }
                    info!("Collection '{}': wired to Maps API", collection.id);
                }
                if collection.apis.contains(&"tiles".to_string()) {
                    tiles_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    tiles_collections.insert(collection.id.clone(), collection.clone());
                    let styles = build_styles(collection, &bundle_index);
                    tiles_styles.insert(collection.id.clone(), styles);
                    if !raster_params.is_empty() {
                        register_parameter_layer_styles(
                            collection,
                            &raster_params,
                            &mut tiles_styles,
                            &bundle_index,
                        );
                    }
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

                if collection.apis.contains(&"edr".to_string()) {
                    edr_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::edr_engine::EdrEngine>,
                    );
                    edr_collections.insert(collection.id.clone(), collection.clone());
                    info!("Collection '{}': wired to EDR API", collection.id);
                }

                let raster_params =
                    ds_core::map_engine::MapEngine::raster_info(engine.as_ref()).parameters;

                if collection.apis.contains(&"wms".to_string()) {
                    map_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    map_collections.insert(collection.id.clone(), collection.clone());
                    let styles = build_styles(collection, &bundle_index);
                    map_styles.insert(collection.id.clone(), styles);
                    if !raster_params.is_empty() {
                        register_parameter_layer_styles(
                            collection,
                            &raster_params,
                            &mut map_styles,
                            &bundle_index,
                        );
                    }
                    info!("Collection '{}': wired to WMS API", collection.id);
                }
                if collection.apis.contains(&"maps".to_string()) {
                    maps_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    maps_collections.insert(collection.id.clone(), collection.clone());
                    let styles = build_styles(collection, &bundle_index);
                    maps_styles.insert(collection.id.clone(), styles);
                    if !raster_params.is_empty() {
                        register_parameter_layer_styles(
                            collection,
                            &raster_params,
                            &mut maps_styles,
                            &bundle_index,
                        );
                    }
                    info!("Collection '{}': wired to Maps API", collection.id);
                }
                if collection.apis.contains(&"tiles".to_string()) {
                    tiles_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    tiles_collections.insert(collection.id.clone(), collection.clone());
                    let styles = build_styles(collection, &bundle_index);
                    tiles_styles.insert(collection.id.clone(), styles);
                    if !raster_params.is_empty() {
                        register_parameter_layer_styles(
                            collection,
                            &raster_params,
                            &mut tiles_styles,
                            &bundle_index,
                        );
                    }
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

                    if collection.apis.contains(&"edr".to_string()) {
                        edr_engines.insert(
                            site_id.clone(),
                            view.clone() as Arc<dyn ds_core::edr_engine::EdrEngine>,
                        );
                        edr_collections.insert(site_id.clone(), site_cfg.clone());
                    }

                    // Per-site, multi-parameter: one layer per bare quantity.
                    let raster_params =
                        ds_core::map_engine::MapEngine::raster_info(view.as_ref()).parameters;

                    if collection.apis.contains(&"wms".to_string()) {
                        map_engines.insert(
                            site_id.clone(),
                            view.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                        );
                        map_collections.insert(site_id.clone(), site_cfg.clone());
                        let styles = build_styles(&site_cfg, &bundle_index);
                        map_styles.insert(site_id.clone(), styles);
                        if !raster_params.is_empty() {
                            register_parameter_layer_styles(
                                &site_cfg,
                                &raster_params,
                                &mut map_styles,
                                &bundle_index,
                            );
                        }
                    }
                    if collection.apis.contains(&"maps".to_string()) {
                        maps_engines.insert(
                            site_id.clone(),
                            view.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                        );
                        maps_collections.insert(site_id.clone(), site_cfg.clone());
                        let styles = build_styles(&site_cfg, &bundle_index);
                        maps_styles.insert(site_id.clone(), styles);
                        if !raster_params.is_empty() {
                            register_parameter_layer_styles(
                                &site_cfg,
                                &raster_params,
                                &mut maps_styles,
                                &bundle_index,
                            );
                        }
                    }
                    if collection.apis.contains(&"tiles".to_string()) {
                        tiles_engines.insert(
                            site_id.clone(),
                            view.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                        );
                        tiles_collections.insert(site_id.clone(), site_cfg.clone());
                        let styles = build_styles(&site_cfg, &bundle_index);
                        tiles_styles.insert(site_id.clone(), styles);
                        if !raster_params.is_empty() {
                            register_parameter_layer_styles(
                                &site_cfg,
                                &raster_params,
                                &mut tiles_styles,
                                &bundle_index,
                            );
                        }
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
                        tracing::error!(
                            "Collection '{}': base id already registered as another \
                             collection — skipping radar-site inventory",
                            collection.id
                        );
                        health.push(CollectionHealth {
                            id: collection.id.clone(),
                            engine_type: "odim-volume".into(),
                            status: CollectionStatus::Failed,
                            error: Some(
                                "base id collides with an already-registered collection \
                                 (EDR, Map, Maps, Tiles, or Features)"
                                    .into(),
                            ),
                        });
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
                if collection.apis.contains(&"tiles".to_string()) {
                    tiles_feature_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::feature_engine::FeatureEngine>,
                    );
                    tiles_feature_collections.insert(collection.id.clone(), collection.clone());
                }
                postgis_engines.push(engine);
                health.push(CollectionHealth {
                    id: collection.id.clone(),
                    engine_type: "postgis".into(),
                    status,
                    error: status_err,
                });
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
    let rendered_cache = Arc::new(ds_render::RenderedCache::new(rendered_cache_mb));
    let tile_cache = Arc::new(ds_render::TilePixelCache::new(metatile_cache_mb));
    // Vector-tile (MVT) cache is independent of the raster cache because the
    // workloads differ (1–50 KB vs 30–200 KB per tile). 128 MB matches the
    // raster default; a config knob lands when an operator asks for it.
    let vector_tile_cache = Arc::new(ds_mvt::VectorTileCache::new(128));

    // Set initial render semaphore total gauge
    RENDER_SEMAPHORE_TOTAL.set(render_concurrency as i64);

    LoadResult {
        edr_state: EdrState {
            engines: edr_engines,
            collections: edr_collections,
            base_url: base_url.to_string(),
        },
        features_state: FeaturesState {
            engines: feature_engines,
            collections: feature_collections,
            base_url: base_url.to_string(),
        },
        wms_state: WmsState {
            engines: map_engines,
            collections: map_collections,
            styles: map_styles,
            render_semaphore: render_semaphore.clone(),
            rendered_cache: rendered_cache.clone(),
            tile_cache,
            base_url: base_url.to_string(),
        },
        maps_state: MapsState {
            engines: maps_engines,
            collections: maps_collections,
            styles: maps_styles,
            render_semaphore: render_semaphore.clone(),
            rendered_cache: rendered_cache.clone(),
            base_url: base_url.to_string(),
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
        },
        health,
        geotiff_engines,
        querydata_engines,
        grib_engines,
        odim_engines,
        odim_volume_engines,
        postgis_engines,
    }
}

/// Build all styles for a WMS-enabled collection (bundle if bound, inline otherwise).
fn build_styles(
    collection: &CollectionConfig,
    bundles: &HashMap<&str, &StyleBundle>,
) -> HashMap<String, ds_render::StyleInfo> {
    build_styles_inner(collection, resolve_bundle(collection, bundles))
}

fn build_styles_inner(
    collection: &CollectionConfig,
    bundle: Option<&StyleBundle>,
) -> HashMap<String, ds_render::StyleInfo> {
    let mut styles = HashMap::new();

    // Build default style (either from the bundle or from inline wms config)
    let (default_colormap, default_min, default_max) =
        build_collection_default_colormap(collection, bundle);
    styles.insert(
        "default".to_string(),
        ds_render::StyleInfo {
            name: "default".to_string(),
            title: "Default".to_string(),
            colormap: default_colormap,
            min: default_min,
            max: default_max,
            parameter: None,
        },
    );

    // Build additional named styles — prefer bundle extras when bound
    if let Some(bundle) = bundle {
        for extra in &bundle.extras {
            let (colormap, min, max) = build_colormap_from_wms_config(
                extra.colormap.as_deref(),
                &extra.color_stops,
                extra.min,
                extra.max,
            );
            styles.insert(
                extra.name.clone(),
                ds_render::StyleInfo {
                    name: extra.name.clone(),
                    title: extra.title.clone().unwrap_or_else(|| extra.name.clone()),
                    colormap,
                    min,
                    max,
                    parameter: extra.parameter.clone(),
                },
            );
        }
    } else if let Some(wms_config) = &collection.wms {
        for style_config in &wms_config.styles {
            let (colormap, min, max) = build_colormap_from_wms_config(
                style_config.colormap.as_deref(),
                &style_config.color_stops,
                style_config.min,
                style_config.max,
            );
            styles.insert(
                style_config.name.clone(),
                ds_render::StyleInfo {
                    name: style_config.name.clone(),
                    title: style_config
                        .title
                        .clone()
                        .unwrap_or_else(|| style_config.name.clone()),
                    colormap,
                    min,
                    max,
                    parameter: style_config.parameter.clone(),
                },
            );
        }
    }

    styles
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

/// Build the collection-level default colormap from the bundle or inline `[wms]` fields.
fn build_collection_default_colormap(
    collection: &CollectionConfig,
    bundle: Option<&StyleBundle>,
) -> (Arc<dyn ds_render::ColorMap>, f64, f64) {
    if let Some(bundle) = bundle {
        return build_colormap_from_wms_config(
            bundle.default.colormap.as_deref(),
            &bundle.default.color_stops,
            bundle.default.min,
            bundle.default.max,
        );
    }
    build_colormap_from_wms_config(
        collection.wms.as_ref().and_then(|w| w.colormap.as_deref()),
        collection
            .wms
            .as_ref()
            .map(|w| &w.color_stops[..])
            .unwrap_or(&[]),
        collection.wms.as_ref().and_then(|w| w.min),
        collection.wms.as_ref().and_then(|w| w.max),
    )
}

/// Register per-parameter-layer styles for multi-parameter engines.
///
/// For each parameter in `param_names`, creates a style set under the layer
/// name `"collection-id/param-short-name"`. The default style uses the
/// per-parameter colormap from `[[collections.wms.parameters]]` if configured,
/// or falls back to the collection-level default. Named styles are shared
/// across all parameter layers.
fn register_parameter_layer_styles(
    collection: &CollectionConfig,
    param_names: &[(String, String)],
    style_map: &mut HashMap<String, HashMap<String, ds_render::StyleInfo>>,
    bundles: &HashMap<&str, &StyleBundle>,
) {
    let wms_config = match &collection.wms {
        Some(c) => c,
        None => return,
    };

    let bundle = resolve_bundle(collection, bundles);
    let shared_named_styles = build_styles_inner(collection, bundle);

    // When a bundle is bound, inline per-parameter overrides are rejected by validation.
    let param_configs: HashMap<&str, &ds_core::config::WmsParameterConfig> = wms_config
        .parameters
        .iter()
        .map(|p| (p.name.as_str(), p))
        .collect();

    let (fallback_colormap, fallback_min, fallback_max) =
        build_collection_default_colormap(collection, bundle);

    for (short_name, _title) in param_names {
        let layer_key = format!("{}/{}", collection.id, short_name);
        let mut layer_styles = HashMap::new();

        // Build this parameter's default style
        let (colormap, min, max) = if let Some(pc) = param_configs.get(short_name.as_str()) {
            build_colormap_from_wms_config(pc.colormap.as_deref(), &pc.color_stops, pc.min, pc.max)
        } else {
            (fallback_colormap.clone(), fallback_min, fallback_max)
        };

        layer_styles.insert(
            "default".to_string(),
            ds_render::StyleInfo {
                name: "default".to_string(),
                title: "Default".to_string(),
                colormap,
                min,
                max,
                parameter: Some(short_name.clone()),
            },
        );

        // Add shared named styles (excluding "default" which we just built).
        // Styles tagged with a specific `parameter` are scoped to that layer only —
        // otherwise a bundle extra with `parameter = "wind_speed"` would leak into
        // every parameter layer's style map.
        for (name, style) in &shared_named_styles {
            if name == "default" {
                continue;
            }
            if let Some(p) = style.parameter.as_deref() {
                if p != short_name {
                    continue;
                }
            }
            layer_styles.insert(name.clone(), style.clone());
        }

        style_map.insert(layer_key, layer_styles);
    }
}

/// Wrap `cmap` in [`ds_render::IntegerLutColorMap`] when the value range fits
/// a small precomputed LUT (#207). Skipped for non-finite/inverted bounds,
/// spans below 16 integer steps (≥1-unit-per-stop is too coarse for sub-unit
/// gradients like viridis 0..1), or spans over the 65 536-entry cap.
fn maybe_wrap_integer_lut(
    cmap: Arc<dyn ds_render::ColorMap>,
    min: f64,
    max: f64,
) -> Arc<dyn ds_render::ColorMap> {
    if !min.is_finite() || !max.is_finite() {
        return cmap;
    }
    let lo = min.floor() as i64;
    let hi = max.ceil() as i64;
    let span = match hi.checked_sub(lo) {
        Some(s) if (16..65_536).contains(&s) => s,
        _ => return cmap,
    };
    match ds_render::IntegerLutColorMap::from_colormap(cmap.as_ref(), lo, hi) {
        Some(lut) => {
            tracing::debug!(
                "Wired IntegerLutColorMap [{lo}..{hi}] ({} entries) for colorize",
                span + 1
            );
            Arc::new(lut)
        }
        // Currently unreachable given the (16..65_536) gate above (max span 65 535
        // → range 65 536, which `from_colormap` accepts since its check is
        // `range > MAX_INTEGER_LUT_SIZE`). Kept as a safe fallback for the day
        // either bound is loosened.
        None => cmap,
    }
}

/// Build a colormap and value range from WMS config fields.
fn build_colormap_from_wms_config(
    colormap_name: Option<&str>,
    color_stops: &[ds_core::config::ColorStop],
    min_override: Option<f64>,
    max_override: Option<f64>,
) -> (Arc<dyn ds_render::ColorMap>, f64, f64) {
    // Custom color stops take priority
    if !color_stops.is_empty() {
        let stops: Vec<ds_render::ColorStop> = color_stops
            .iter()
            .filter_map(|s| {
                ds_render::parse_hex_color(&s.color)
                    .ok()
                    .map(|c| ds_render::ColorStop {
                        value: s.value,
                        color: c,
                    })
            })
            .collect();
        if !stops.is_empty() {
            let min = min_override.unwrap_or_else(|| stops.first().map(|s| s.value).unwrap_or(0.0));
            let max = max_override.unwrap_or_else(|| stops.last().map(|s| s.value).unwrap_or(1.0));
            let cmap: Arc<dyn ds_render::ColorMap> =
                Arc::new(ds_render::LinearColorMap::new(stops));
            return (maybe_wrap_integer_lut(cmap, min, max), min, max);
        }
    }

    // Fall back to built-in colormap name
    let name = colormap_name.unwrap_or("viridis");
    if let Some(builtin) = ds_render::colormap::resolve_builtin(name) {
        let stops = ds_render::colormap::builtin_stops(&builtin);
        let min = min_override.unwrap_or_else(|| stops.first().map(|s| s.value).unwrap_or(0.0));
        let max = max_override.unwrap_or_else(|| stops.last().map(|s| s.value).unwrap_or(1.0));
        let cmap: Arc<dyn ds_render::ColorMap> =
            Arc::new(ds_render::LutColorMap::from_builtin(builtin, min, max));
        return (maybe_wrap_integer_lut(cmap, min, max), min, max);
    }

    // Default: viridis 0..1
    let min = min_override.unwrap_or(0.0);
    let max = max_override.unwrap_or(1.0);
    let cmap: Arc<dyn ds_render::ColorMap> = Arc::new(ds_render::LutColorMap::from_builtin(
        ds_render::BuiltinColormap::Viridis,
        min,
        max,
    ));
    (maybe_wrap_integer_lut(cmap, min, max), min, max)
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

    info!(
        "Reload requested, re-reading config from {}",
        state.config_path
    );

    let (config, config_warnings) = ds_core::config::ServerConfig::from_file(&state.config_path)
        .map_err(|e| {
            tracing::error!("Reload failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to read config: {e}") })),
            )
        })?;
    for warning in &config_warnings {
        tracing::warn!("{warning}");
    }

    let base_url = config.server.base_url();

    // NOTE: old poll loops are shut down *after* the reload guard below, not
    // here. If the guard rejects the reload (no `Ready` collection), the old
    // engines and their poll loops must stay alive — otherwise a rejected
    // reload would freeze the live registry with dead loops and no new ones
    // spawned (the guard returns before the spawn block).

    let mut result = load_collections(
        &config.collections,
        &config.style_bundles,
        &base_url,
        config.server.metatile_cache_mb,
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
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": "Reload produced 0 working collections, keeping old state",
                "configured": config.collections.len()
            })),
        ));
    }

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

    // Atomically swap state
    state.edr.store(Arc::new(result.edr_state));
    state.features.store(Arc::new(result.features_state));
    state.wms.store(Arc::new(result.wms_state));
    state.maps.store(Arc::new(result.maps_state));
    state.tiles.store(Arc::new(result.tiles_state));

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
        .odim_engines
        .write()
        .unwrap_or_else(|e| e.into_inner()) = result.odim_engines;
    *state
        .odim_volume_engines
        .write()
        .unwrap_or_else(|e| e.into_inner()) = result.odim_volume_engines;
    *state
        .postgis_engines
        .write()
        .unwrap_or_else(|e| e.into_inner()) = result.postgis_engines;

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

    Ok(Json(json!({
        "status": "ok",
        // `ready` = fully-working collections; `degraded` wire no servable
        // routes (e.g. an empty odim-volume source) but aren't failures.
        "ready": ready,
        "degraded": degraded,
        "configured": config.collections.len(),
        "collections": result.health
    })))
}

/// GET /health — per-collection health status with data staleness info.
pub async fn health_handler(State(state): State<AdminState>) -> impl IntoResponse {
    let health = state
        .health
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();

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

    // Delta-tracked cache counters: cache implementations expose cumulative
    // (hits, misses) values but may be replaced on reload. Convert to
    // monotonic Prometheus counters.
    let mut counter_state = CACHE_COUNTER_STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    // Rendered image cache: global (single cache shared across collections).
    // Always call inc_by() (even with delta 0) to ensure the LazyLock counter
    // is registered on the first scrape, so dashboards see the series even
    // before any rendering has happened.
    let (r_hits, r_misses) = wms.rendered_cache.stats();
    let (last_h, last_m) = counter_state.rendered;
    if r_hits < last_h || r_misses < last_m {
        // Cache was replaced (reload) — rebaseline without emitting a spike.
        counter_state.rendered = (r_hits, r_misses);
        RENDERED_CACHE_HITS.inc_by(0);
        RENDERED_CACHE_MISSES.inc_by(0);
    } else {
        RENDERED_CACHE_HITS.inc_by(r_hits - last_h);
        RENDERED_CACHE_MISSES.inc_by(r_misses - last_m);
        counter_state.rendered = (r_hits, r_misses);
    }
    RENDERED_CACHE_BYTES.set(wms.rendered_cache.weight() as i64);
    RENDERED_CACHE_CAPACITY_BYTES.set(wms.rendered_cache.capacity() as i64);
    RENDERED_CACHE_ENTRIES.set(wms.rendered_cache.len() as i64);

    // Meta-tile pixel cache: global, same delta-tracking as the rendered cache.
    let (m_hits, m_misses) = wms.tile_cache.stats();
    let (last_mh, last_mm) = counter_state.metatile;
    if m_hits < last_mh || m_misses < last_mm {
        counter_state.metatile = (m_hits, m_misses);
        METATILE_CACHE_HITS.inc_by(0);
        METATILE_CACHE_MISSES.inc_by(0);
    } else {
        METATILE_CACHE_HITS.inc_by(m_hits - last_mh);
        METATILE_CACHE_MISSES.inc_by(m_misses - last_mm);
        counter_state.metatile = (m_hits, m_misses);
    }
    METATILE_CACHE_BYTES.set(wms.tile_cache.weight() as i64);
    METATILE_CACHE_CAPACITY_BYTES.set(wms.tile_cache.capacity() as i64);
    METATILE_CACHE_ENTRIES.set(wms.tile_cache.len() as i64);

    // PVOL lazy pixel cache: process-global (never replaced on reload), so the
    // cumulative counters are strictly monotonic — a per-counter
    // `saturating_sub` delta is correct without the rebaseline branch the
    // replaceable rendered/metatile caches above need (and `saturating_sub`
    // can't underflow if a counter ever does appear backward). Only emit when
    // PVOL collections are loaded, so non-radar deployments don't carry empty
    // `pvol_*` series. Recover from a poisoned lock (`into_inner`) — a panic
    // elsewhere must not silently suppress the failure metric.
    let has_pvol = !state
        .odim_volume_engines
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty();
    if has_pvol {
        let (p_hits, p_misses, p_bytes, p_cap, p_fail) = engine_odim::pixel_cache_metrics();
        let (last_ph, last_pm, last_pf) = counter_state.pvol_pixel;
        PVOL_PIXEL_CACHE_HITS.inc_by(p_hits.saturating_sub(last_ph));
        PVOL_PIXEL_CACHE_MISSES.inc_by(p_misses.saturating_sub(last_pm));
        PVOL_PIXEL_READ_FAILURES.inc_by(p_fail.saturating_sub(last_pf));
        counter_state.pvol_pixel = (p_hits, p_misses, p_fail);
        PVOL_PIXEL_CACHE_BYTES.set(p_bytes as i64);
        PVOL_PIXEL_CACHE_CAPACITY_BYTES.set(p_cap as i64);
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
    use super::{build_styles, classify_route, is_safe_request_id, maybe_wrap_integer_lut};
    use ds_core::config::{CollectionConfig, StyleBundle};
    use std::collections::HashMap;
    use std::sync::Arc;

    // --- maybe_wrap_integer_lut (#207) ---

    #[test]
    fn integer_lut_wraps_when_range_fits_and_is_wide_enough() {
        let src: Arc<dyn ds_render::ColorMap> = Arc::new(ds_render::LutColorMap::from_builtin(
            ds_render::BuiltinColormap::RadarDbz,
            -32.0,
            95.0,
        ));
        let wrapped = maybe_wrap_integer_lut(src.clone(), -32.0, 95.0);
        // It was replaced (no longer the same Arc).
        assert!(
            !Arc::ptr_eq(&src, &wrapped),
            "expected wrap on the radar_dbz -32..95 range"
        );
        // Colour at integer values matches the source — the LUT just precomputes.
        for v in [-32i64, -16, 0, 25, 50, 95] {
            assert_eq!(
                wrapped.color(Some(v as f64)),
                src.color(Some(v as f64)),
                "colour mismatch at v={v}"
            );
        }
        // Out-of-range saturates to the boundary entry (not transparent),
        // matching the float path's clamp — at integer endpoints we CAN
        // compare to src by construction.
        assert_eq!(wrapped.color(Some(-100.0)), src.color(Some(-32.0)));
        assert_eq!(wrapped.color(Some(200.0)), src.color(Some(95.0)));
        // (The non-integer rounding direction — round-nearest, not toward
        // zero — is pinned in ds-render's IntegerLutColorMap tests where the
        // colormap has distinct colours per integer. radar_dbz's low end is
        // transparent so it can't distinguish the directions here.)
    }

    #[test]
    fn integer_lut_skips_narrow_range() {
        // viridis 0..1 → only 2 integer entries → truncation would collapse the
        // gradient. Must NOT wrap.
        let src: Arc<dyn ds_render::ColorMap> = Arc::new(ds_render::LutColorMap::from_builtin(
            ds_render::BuiltinColormap::Viridis,
            0.0,
            1.0,
        ));
        let wrapped = maybe_wrap_integer_lut(src.clone(), 0.0, 1.0);
        assert!(
            Arc::ptr_eq(&src, &wrapped),
            "expected no wrap for a narrow (<16-entry) range"
        );
    }

    #[test]
    fn integer_lut_skips_huge_range() {
        // > 65 535 entries can't fit the integer LUT; fall back to the source.
        let src: Arc<dyn ds_render::ColorMap> = Arc::new(ds_render::LutColorMap::from_builtin(
            ds_render::BuiltinColormap::Viridis,
            -100_000.0,
            100_000.0,
        ));
        let wrapped = maybe_wrap_integer_lut(src.clone(), -100_000.0, 100_000.0);
        assert!(
            Arc::ptr_eq(&src, &wrapped),
            "expected no wrap for an over-cap range"
        );
    }

    #[test]
    fn integer_lut_skips_non_finite_bounds() {
        let src: Arc<dyn ds_render::ColorMap> = Arc::new(ds_render::LutColorMap::from_builtin(
            ds_render::BuiltinColormap::Viridis,
            0.0,
            1.0,
        ));
        assert!(Arc::ptr_eq(
            &src,
            &maybe_wrap_integer_lut(src.clone(), f64::NAN, 1.0)
        ));
        assert!(Arc::ptr_eq(
            &src,
            &maybe_wrap_integer_lut(src.clone(), 0.0, f64::INFINITY)
        ));
    }

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
    fn build_styles_expands_bundle_into_default_plus_extras() {
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

        let styles = build_styles(&collection, &index);
        assert_eq!(styles.len(), 3, "default + 2 extras expected");
        assert!(styles.contains_key("default"));
        assert_eq!(styles["default"].name, "default");
        assert!(styles.contains_key("radar_dbz"));
        assert_eq!(styles["radar_dbz"].title, "MeteoCore Radar");
        assert!(styles.contains_key("radar_fmi"));
        assert_eq!(styles["radar_fmi"].title, "FMI Radar");
    }

    #[test]
    fn build_styles_falls_back_to_inline_when_no_bundle_referenced() {
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

        let styles = build_styles(&collection, &index);
        assert_eq!(styles.len(), 2);
        assert!(styles.contains_key("default"));
        assert!(styles.contains_key("alt"));
        assert_eq!(styles["alt"].title, "Alt");
    }

    #[test]
    fn build_styles_falls_back_when_bundle_ref_unknown() {
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
        let styles = build_styles(&collection, &index);

        // Only the default; no extras; the bundle was silently skipped.
        assert_eq!(styles.len(), 1);
        assert!(styles.contains_key("default"));
    }

    #[test]
    fn build_styles_parameter_tagged_extras_stay_in_map() {
        // build_styles itself returns every extra — scoping by parameter
        // happens downstream in register_parameter_layer_styles. This test
        // locks the current behaviour so the bundle surface stays stable.
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
        let styles = build_styles(&collection, &index);

        assert_eq!(styles.len(), 3);
        assert_eq!(styles["wind_only"].parameter.as_deref(), Some("wind_speed"));
        assert!(styles["global"].parameter.is_none());
    }
}
