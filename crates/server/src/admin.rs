use std::collections::HashMap;
use std::sync::{Arc, LazyLock, RwLock};
use std::time::Instant;

use arc_swap::ArcSwap;
use axum::extract::{MatchedPath, State};
use axum::http::{header, StatusCode};
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
    pub config_path: String,
    pub health: RwLock<Vec<CollectionHealth>>,
    pub geotiff_engines: RwLock<Vec<Arc<engine_geotiff::GeoTiffEngine>>>,
    /// Serializes reload requests to prevent concurrent reloads from racing.
    pub reload_lock: tokio::sync::Mutex<()>,
}

pub type AdminState = Arc<ServerState>;

// ---------------------------------------------------------------------------
// Collection loader (used by startup and reload)
// ---------------------------------------------------------------------------

pub struct LoadResult {
    pub edr_state: EdrState,
    pub features_state: FeaturesState,
    pub wms_state: WmsState,
    pub health: Vec<CollectionHealth>,
    pub geotiff_engines: Vec<Arc<engine_geotiff::GeoTiffEngine>>,
}

pub fn load_collections(collections: &[CollectionConfig], base_url: &str) -> LoadResult {
    let mut edr_engines: HashMap<String, Arc<dyn ds_core::engine::Engine>> = HashMap::new();
    let mut edr_collections: HashMap<String, CollectionConfig> = HashMap::new();
    let mut feature_engines: HashMap<String, Arc<dyn ds_core::feature_engine::FeatureEngine>> =
        HashMap::new();
    let mut feature_collections: HashMap<String, CollectionConfig> = HashMap::new();
    let mut map_engines: HashMap<String, Arc<dyn ds_core::map_engine::MapEngine>> = HashMap::new();
    let mut map_collections: HashMap<String, CollectionConfig> = HashMap::new();
    let mut map_colormaps: HashMap<String, Arc<dyn ds_render::ColorMap>> = HashMap::new();
    let mut geotiff_engines: Vec<Arc<engine_geotiff::GeoTiffEngine>> = Vec::new();
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
                if collection.apis.contains(&"edr".to_string()) {
                    info!(
                        "Warning: GeoJSON engine does not support EDR API, \
                         skipping EDR wiring for collection '{}'",
                        collection.id
                    );
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
                if collection.apis.contains(&"features".to_string()) {
                    info!(
                        "Warning: GeoTIFF engine does not support Features API, \
                         skipping Features wiring for collection '{}'",
                        collection.id
                    );
                }
                if collection.apis.contains(&"wms".to_string()) {
                    map_engines.insert(
                        collection.id.clone(),
                        engine.clone() as Arc<dyn ds_core::map_engine::MapEngine>,
                    );
                    map_collections.insert(collection.id.clone(), collection.clone());

                    // Build colormap from config
                    let colormap = build_colormap(collection);
                    map_colormaps.insert(collection.id.clone(), colormap);

                    info!(
                        "Collection '{}': wired to WMS API",
                        collection.id
                    );
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
        .filter_map(|c| c.wms.as_ref())
        .map(|w| w.rendered_cache_mb)
        .next()
        .unwrap_or(128);

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
            colormaps: map_colormaps,
            render_semaphore: Arc::new(tokio::sync::Semaphore::new(8)),
            rendered_cache: Arc::new(api_wms::handlers::RenderedCache::new(rendered_cache_mb)),
            base_url: base_url.to_string(),
        },
        health,
        geotiff_engines,
    }
}

/// Build a colormap for a WMS-enabled collection from its config.
fn build_colormap(collection: &CollectionConfig) -> Arc<dyn ds_render::ColorMap> {
    if let Some(wms_config) = &collection.wms {
        // Custom color stops take priority
        if !wms_config.color_stops.is_empty() {
            let stops: Vec<ds_render::ColorStop> = wms_config
                .color_stops
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
                return Arc::new(ds_render::LinearColorMap::new(stops));
            }
        }
        // Fall back to built-in colormap name
        if let Some(builtin) = ds_render::colormap::resolve_builtin(&wms_config.colormap) {
            // Use the value range from the colormap's own stops
            let stops = ds_render::colormap::builtin_stops(&builtin);
            let min = stops.first().map(|s| s.value).unwrap_or(0.0);
            let max = stops.last().map(|s| s.value).unwrap_or(1.0);
            return Arc::new(ds_render::LutColorMap::from_builtin(builtin, min, max));
        }
    }
    // Default: viridis 0..1
    Arc::new(ds_render::LutColorMap::from_builtin(
        ds_render::BuiltinColormap::Viridis,
        0.0,
        1.0,
    ))
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
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
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

    // Shut down old GeoTIFF poll loops
    {
        let old_engines = state.geotiff_engines.read().unwrap();
        for engine in old_engines.iter() {
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

    // Spawn poll loops for new GeoTIFF engines
    for engine in &result.geotiff_engines {
        let poller = engine.clone();
        tokio::spawn(async move {
            poller.poll_loop().await;
        });
    }

    // Atomically swap state
    state.edr.store(Arc::new(result.edr_state));
    state.features.store(Arc::new(result.features_state));
    state.wms.store(Arc::new(result.wms_state));

    // Update health
    update_health_gauges(&result.health);
    *state.health.write().unwrap() = result.health.clone();

    // Update GeoTIFF engine list
    *state.geotiff_engines.write().unwrap() = result.geotiff_engines;

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

/// GET /health — per-collection health status.
pub async fn health_handler(State(state): State<AdminState>) -> impl IntoResponse {
    let health = state.health.read().unwrap().clone();

    let overall = if health.iter().all(|h| h.status == CollectionStatus::Ready) {
        "healthy"
    } else if health.iter().any(|h| h.status == CollectionStatus::Failed) {
        "unhealthy"
    } else {
        "degraded"
    };

    let status_code = match overall {
        "healthy" => StatusCode::OK,
        "degraded" => StatusCode::OK,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    };

    (
        status_code,
        Json(json!({
            "status": overall,
            "collections": health
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
        .unwrap_or_else(|| req.uri().path().to_string());

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
