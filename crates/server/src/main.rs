use std::collections::HashMap;
use std::sync::Arc;

use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use tower_http::cors::CorsLayer;
use tracing::info;

use api_edr::handlers::EdrState;
use api_features::handlers::FeaturesState;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let config = ds_core::config::ServerConfig::from_file("config.toml")
        .expect("Failed to load config.toml");

    let base_url = config.server.base_url();
    info!("Base URL: {base_url}");

    let mut edr_engines: HashMap<String, Arc<dyn ds_core::engine::Engine>> = HashMap::new();
    let mut edr_collections: HashMap<String, ds_core::config::CollectionConfig> = HashMap::new();
    let mut feature_engines: HashMap<String, Arc<dyn ds_core::feature_engine::FeatureEngine>> =
        HashMap::new();
    let mut feature_collections: HashMap<String, ds_core::config::CollectionConfig> =
        HashMap::new();

    for collection in &config.collections {
        info!(
            "Loading collection '{}' ({}) from {}",
            collection.id, collection.engine_type, collection.data_path
        );

        match collection.engine_type.as_str() {
            "csv" => {
                let store = engine_csv::CsvDataStore::load(&collection.data_path)
                    .expect("Failed to load CSV data");

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
            }
            "geojson" => {
                let engine = Arc::new(
                    engine_geojson::GeoJsonEngine::load(&collection.data_path)
                        .expect("Failed to load GeoJSON data"),
                );

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
            }
            "geotiff" => {
                let geotiff_config = collection.geotiff.as_ref().unwrap_or_else(|| {
                    panic!(
                        "Collection '{}' has engine_type 'geotiff' but missing [collections.geotiff] config",
                        collection.id
                    );
                });

                let engine = Arc::new(
                    engine_geotiff::GeoTiffEngine::new(&collection.data_path, geotiff_config)
                        .expect("Failed to initialize GeoTIFF engine"),
                );

                // get_temporal_extent is from the Engine trait, already in scope
                // via ds_core::engine::Engine. Use the catalog's temporal extent
                // to report file count indirectly.
                if let Some((start, end)) = ds_core::engine::Engine::get_temporal_extent(engine.as_ref()) {
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
            }
            other => {
                panic!("Unknown engine type '{other}' for collection '{}'", collection.id);
            }
        }
    }

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
        .route("/edr/", get(api_edr::handlers::landing_page).with_state(edr_state))
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

    axum::serve(listener, app).await.expect("Server error");
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
