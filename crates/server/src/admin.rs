use std::collections::HashMap;
use std::sync::{Arc, LazyLock, RwLock};
use std::time::Instant;

use arc_swap::ArcSwap;
use axum::extract::{MatchedPath, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounterVec, IntGauge, Opts, Registry, TextEncoder,
};
use serde::Serialize;
use serde_json::json;
use tracing::info;

use api_edr::handlers::EdrState;
use api_features::handlers::FeaturesState;
use api_maps::MapsState;
use api_tiles::TilesState;
use api_wms::WmsState;
use ds_core::config::CollectionConfig;

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
        .buckets(vec![
            0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 5.0,
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
}

pub fn load_collections(collections: &[CollectionConfig], base_url: &str) -> LoadResult {
    let mut edr_engines: HashMap<String, Arc<dyn ds_core::engine::Engine>> = HashMap::new();
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
    let mut geotiff_engines: Vec<Arc<engine_geotiff::GeoTiffEngine>> = Vec::new();
    let mut querydata_engines: Vec<Arc<engine_querydata::QueryDataEngine>> = Vec::new();
    let mut grib_engines: Vec<Arc<engine_grib::GribEngine>> = Vec::new();
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
            "geojson" => &["features"],
            "geotiff" => &["edr", "wms", "maps", "tiles"],
            "querydata" => &["edr", "wms", "maps", "tiles"],
            "grib" => &["edr", "wms", "maps", "tiles"],
            _ => &[],
        };
        let mut has_unsupported = false;
        for api in &collection.apis {
            if !supported_apis.contains(&api.as_str()) {
                tracing::error!(
                    "Collection '{}': engine '{}' does not support '{}' API, skipping collection",
                    collection.id,
                    collection.engine_type,
                    api
                );
                has_unsupported = true;
            }
        }
        if has_unsupported {
            health.push(CollectionHealth {
                id: collection.id.clone(),
                engine_type: collection.engine_type.clone(),
                status: CollectionStatus::Failed,
                error: Some(format!(
                    "engine '{}' does not support requested APIs: {:?}",
                    collection.engine_type, collection.apis
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
                        engine.clone() as Arc<dyn ds_core::engine::Engine>,
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
                    ds_core::engine::Engine::get_temporal_extent(engine.as_ref())
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
                        engine.clone() as Arc<dyn ds_core::engine::Engine>,
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
                    let styles = build_styles(collection);
                    map_styles.insert(collection.id.clone(), styles);

                    info!("Collection '{}': wired to WMS API", collection.id);
                }
                if collection.apis.contains(&"maps".to_string()) {
                    maps_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    maps_collections.insert(collection.id.clone(), collection.clone());

                    let styles = build_styles(collection);
                    maps_styles.insert(collection.id.clone(), styles);

                    info!("Collection '{}': wired to Maps API", collection.id);
                }
                if collection.apis.contains(&"tiles".to_string()) {
                    tiles_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    tiles_collections.insert(collection.id.clone(), collection.clone());

                    let styles = build_styles(collection);
                    tiles_styles.insert(collection.id.clone(), styles);

                    info!("Collection '{}': wired to Tiles API", collection.id);
                }

                // GeoTIFF starts degraded (no data yet until first poll), unless
                // the initial scan already found files.
                let has_data =
                    ds_core::engine::Engine::get_temporal_extent(engine.as_ref()).is_some();
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
                        engine.clone() as Arc<dyn ds_core::engine::Engine>,
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
                    let styles = build_styles(collection);
                    map_styles.insert(collection.id.clone(), styles);
                    if !raster_params.is_empty() {
                        register_parameter_layer_styles(
                            collection,
                            &raster_params,
                            &mut map_styles,
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
                    let styles = build_styles(collection);
                    maps_styles.insert(collection.id.clone(), styles);
                    if !raster_params.is_empty() {
                        register_parameter_layer_styles(
                            collection,
                            &raster_params,
                            &mut maps_styles,
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
                    let styles = build_styles(collection);
                    tiles_styles.insert(collection.id.clone(), styles);
                    if !raster_params.is_empty() {
                        register_parameter_layer_styles(
                            collection,
                            &raster_params,
                            &mut tiles_styles,
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
                    ds_core::engine::Engine::get_temporal_extent(engine.as_ref())
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
                        engine.clone() as Arc<dyn ds_core::engine::Engine>,
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
                    let styles = build_styles(collection);
                    map_styles.insert(collection.id.clone(), styles);
                    if !raster_params.is_empty() {
                        register_parameter_layer_styles(
                            collection,
                            &raster_params,
                            &mut map_styles,
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
                    let styles = build_styles(collection);
                    maps_styles.insert(collection.id.clone(), styles);
                    if !raster_params.is_empty() {
                        register_parameter_layer_styles(
                            collection,
                            &raster_params,
                            &mut maps_styles,
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
                    let styles = build_styles(collection);
                    tiles_styles.insert(collection.id.clone(), styles);
                    if !raster_params.is_empty() {
                        register_parameter_layer_styles(
                            collection,
                            &raster_params,
                            &mut tiles_styles,
                        );
                    }
                    info!("Collection '{}': wired to Tiles API", collection.id);
                }

                let has_data =
                    ds_core::engine::Engine::get_temporal_extent(engine.as_ref()).is_some();
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

    // Shared render semaphore and cache between WMS, Maps, and Tiles APIs.
    // Size to available CPU cores — render tasks are CPU-bound (colorization + PNG encoding).
    // Minimum 4 to avoid starving on small machines; excess requests queue via acquire().await.
    let render_concurrency = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8)
        .max(4);
    tracing::info!("Render concurrency: {render_concurrency} (from available CPUs)");
    let render_semaphore = Arc::new(tokio::sync::Semaphore::new(render_concurrency));
    let rendered_cache = Arc::new(ds_render::RenderedCache::new(rendered_cache_mb));

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
            render_semaphore,
            rendered_cache,
            base_url: base_url.to_string(),
        },
        health,
        geotiff_engines,
        querydata_engines,
        grib_engines,
    }
}

/// Build all styles for a WMS-enabled collection from its config.
fn build_styles(collection: &CollectionConfig) -> HashMap<String, ds_render::StyleInfo> {
    let mut styles = HashMap::new();

    // Build default style from top-level wms config
    let (default_colormap, default_min, default_max) =
        build_collection_default_colormap(collection);
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

    // Build additional named styles
    if let Some(wms_config) = &collection.wms {
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

/// Build the collection-level default colormap (from top-level [collections.wms]).
fn build_collection_default_colormap(
    collection: &CollectionConfig,
) -> (Arc<dyn ds_render::ColorMap>, f64, f64) {
    build_colormap_from_wms_config(
        collection.wms.as_ref().map(|w| w.colormap.as_str()),
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
) {
    let wms_config = match &collection.wms {
        Some(c) => c,
        None => return,
    };

    // Build named styles (shared across all param layers)
    let shared_named_styles = build_styles(collection);

    // Index per-parameter configs by name
    let param_configs: HashMap<&str, &ds_core::config::WmsParameterConfig> = wms_config
        .parameters
        .iter()
        .map(|p| (p.name.as_str(), p))
        .collect();

    // Collection-level default colormap (fallback)
    let (fallback_colormap, fallback_min, fallback_max) =
        build_collection_default_colormap(collection);

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

        // Add shared named styles (excluding "default" which we just built)
        for (name, style) in &shared_named_styles {
            if name != "default" {
                layer_styles.insert(name.clone(), style.clone());
            }
        }

        style_map.insert(layer_key, layer_styles);
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
            return (Arc::new(ds_render::LinearColorMap::new(stops)), min, max);
        }
    }

    // Fall back to built-in colormap name
    let name = colormap_name.unwrap_or("viridis");
    if let Some(builtin) = ds_render::colormap::resolve_builtin(name) {
        let stops = ds_render::colormap::builtin_stops(&builtin);
        let min = min_override.unwrap_or_else(|| stops.first().map(|s| s.value).unwrap_or(0.0));
        let max = max_override.unwrap_or_else(|| stops.last().map(|s| s.value).unwrap_or(1.0));
        return (
            Arc::new(ds_render::LutColorMap::from_builtin(builtin, min, max)),
            min,
            max,
        );
    }

    // Default: viridis 0..1
    let min = min_override.unwrap_or(0.0);
    let max = max_override.unwrap_or(1.0);
    (
        Arc::new(ds_render::LutColorMap::from_builtin(
            ds_render::BuiltinColormap::Viridis,
            min,
            max,
        )),
        min,
        max,
    )
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

    let config = ds_core::config::ServerConfig::from_file(&state.config_path).map_err(|e| {
        tracing::error!("Reload failed: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to read config: {e}") })),
        )
    })?;

    let base_url = config.server.base_url();

    // Shut down old poll loops
    {
        let old_geotiff = state
            .geotiff_engines
            .read()
            .unwrap_or_else(|e| e.into_inner());
        for engine in old_geotiff.iter() {
            engine.shutdown();
        }
        let old_querydata = state
            .querydata_engines
            .read()
            .unwrap_or_else(|e| e.into_inner());
        for engine in old_querydata.iter() {
            engine.shutdown();
        }
        let old_grib = state.grib_engines.read().unwrap_or_else(|e| e.into_inner());
        for engine in old_grib.iter() {
            engine.shutdown();
        }
    }

    let result = load_collections(&config.collections, &base_url);

    let loaded = result
        .health
        .iter()
        .filter(|h| h.status != CollectionStatus::Failed)
        .count();

    if loaded == 0 && !config.collections.is_empty() {
        // Restore old GeoTIFF engines (they were shut down)
        // This is a best-effort recovery — the old engines' poll loops won't restart,
        // but cached data is still servable.
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

    // Spawn poll loops for new engines
    for engine in &result.geotiff_engines {
        let poller = engine.clone();
        tokio::spawn(async move {
            poller.poll_loop().await;
        });
    }
    for engine in &result.querydata_engines {
        let poller = engine.clone();
        tokio::spawn(async move {
            poller.poll_loop().await;
        });
    }
    for engine in &result.grib_engines {
        let poller = engine.clone();
        tokio::spawn(async move {
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

    info!(
        "Reload complete: {}/{} collections loaded",
        loaded,
        config.collections.len()
    );

    Ok(Json(json!({
        "status": "ok",
        "loaded": loaded,
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

    // Helper: build temporal extent from any Engine
    fn build_temporal(engine: &dyn ds_core::engine::Engine) -> Option<serde_json::Value> {
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
pub async fn metrics_handler() -> impl IntoResponse {
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

    HTTP_REQUESTS_TOTAL
        .with_label_values(&[&method, &path, &status])
        .inc();
    HTTP_REQUEST_DURATION
        .with_label_values(&[&method, &path])
        .observe(duration);

    response
}
