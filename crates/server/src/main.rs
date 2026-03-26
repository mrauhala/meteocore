use std::collections::HashMap;
use std::sync::Arc;

use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use tokio::signal;
use tower_http::cors::CorsLayer;
use tracing::info;

use api_edr::handlers::EdrState;
use api_features::handlers::FeaturesState;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let config_path = std::env::var("CONFIG_PATH").unwrap_or_else(|_| "config.toml".to_string());
    let config = match ds_core::config::ServerConfig::from_file(&config_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to load {}: {}", config_path, e);
            std::process::exit(1);
        }
    };

    let base_url = config.server.base_url();
    info!("Base URL: {base_url}");

    let mut edr_engines: HashMap<String, Arc<dyn ds_core::engine::Engine>> = HashMap::new();
    let mut edr_collections: HashMap<String, ds_core::config::CollectionConfig> = HashMap::new();
    let mut feature_engines: HashMap<String, Arc<dyn ds_core::feature_engine::FeatureEngine>> =
        HashMap::new();
    let mut feature_collections: HashMap<String, ds_core::config::CollectionConfig> =
        HashMap::new();
    let mut geotiff_engines: Vec<Arc<engine_geotiff::GeoTiffEngine>> = Vec::new();
    let mut loaded_count: usize = 0;

    for collection in &config.collections {
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
                loaded_count += 1;
            }
            "geojson" => {
                let data_path = match collection.data_path.as_deref() {
                    Some(p) => p,
                    None => {
                        tracing::error!(
                            "Collection '{}': geojson engine requires data_path, skipping",
                            collection.id
                        );
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
                loaded_count += 1;
            }
            "geotiff" => {
                let geotiff_config = match collection.geotiff.as_ref() {
                    Some(c) => c,
                    None => {
                        tracing::error!(
                            "Collection '{}': engine_type 'geotiff' but missing [collections.geotiff] config, skipping",
                            collection.id
                        );
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

                // Spawn the background polling task
                let poller = engine.clone();
                tokio::spawn(async move {
                    poller.poll_loop().await;
                });

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
                loaded_count += 1;
            }
            other => {
                tracing::error!(
                    "Collection '{}': unknown engine type '{}', skipping",
                    collection.id,
                    other
                );
                continue;
            }
        }
    }

    if loaded_count == 0 {
        tracing::error!(
            "No collections loaded successfully ({} configured). Refusing to start an empty server.",
            config.collections.len()
        );
        std::process::exit(1);
    }
    info!(
        "Loaded {}/{} collections successfully",
        loaded_count,
        config.collections.len()
    );

    let edr_state = Arc::new(EdrState {
        engines: edr_engines,
        collections: edr_collections,
        base_url: base_url.clone(),
    });

    let features_state = Arc::new(FeaturesState {
        engines: feature_engines,
        collections: feature_collections,
        base_url: base_url.clone(),
    });

    let root_base_url = Arc::new(base_url);

    let app = Router::new()
        .route(
            "/",
            get({
                let base = root_base_url.clone();
                move || root_landing_page(base)
            }),
        )
        .nest("/edr", api_edr::router(edr_state.clone()))
        .nest("/features", api_features::router(features_state.clone()))
        // Trailing-slash variants so /edr/ and /features/ also work
        .route(
            "/edr/",
            get(api_edr::handlers::landing_page).with_state(edr_state),
        )
        .route(
            "/features/",
            get(api_features::handlers::landing_page).with_state(features_state),
        )
        .layer(CorsLayer::permissive());

    let addr = format!("{}:{}", config.server.host, config.server.port);
    info!("Starting server on {addr}");

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("Failed to bind");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("Server error");

    // Signal all GeoTIFF polling loops to stop
    for engine in &geotiff_engines {
        engine.shutdown();
    }
    info!("Server shut down gracefully");
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("Received Ctrl+C, shutting down..."),
        _ = terminate => info!("Received SIGTERM, shutting down..."),
    }
}

async fn root_landing_page(base_url: Arc<String>) -> impl IntoResponse {
    let base = &*base_url;
    Json(json!({
        "title": "Metocean Data Server",
        "description": "OGC API server providing EDR and Features access to metocean data",
        "links": [
            {
                "href": format!("{base}/"),
                "rel": "self",
                "type": "application/json",
                "title": "This document"
            },
            {
                "href": format!("{base}/edr/"),
                "rel": "service-desc",
                "type": "application/json",
                "title": "EDR API"
            },
            {
                "href": format!("{base}/features/"),
                "rel": "service-desc",
                "type": "application/json",
                "title": "Features API"
            }
        ]
    }))
}
