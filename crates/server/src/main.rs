use std::sync::Arc;

use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use tower_http::cors::CorsLayer;
use tracing::info;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let config = ds_core::config::ServerConfig::from_file("config.toml")
        .expect("Failed to load config.toml");

    let collection = &config.collections[0];
    info!("Loading collection '{}' from {}", collection.id, collection.data_path);

    let store = engine_csv::CsvDataStore::load(&collection.data_path)
        .expect("Failed to load CSV data");

    info!(
        "Loaded {} rows, {} locations, {} parameters",
        store.rows.len(),
        store.location_index.len(),
        store.parameter_names.len()
    );

    let engine = Arc::new(engine_csv::CsvEngine::new(store));
    let edr_engine = engine.clone() as Arc<dyn ds_core::engine::Engine>;
    let feature_engine = engine.clone() as Arc<dyn ds_core::feature_engine::FeatureEngine>;

    let app = Router::new()
        .route("/", get(root_landing_page))
        .nest("/edr", api_edr::router(edr_engine.clone()))
        .nest("/features", api_features::router(feature_engine.clone()))
        // Trailing-slash variants so /edr/ and /features/ also work
        .route("/edr/", get(api_edr::handlers::landing_page))
        .route("/features/", get(api_features::handlers::landing_page))
        .layer(CorsLayer::permissive());

    let addr = format!("{}:{}", config.server.host, config.server.port);
    info!("Starting server on {addr}");

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("Failed to bind");

    axum::serve(listener, app).await.expect("Server error");
}

async fn root_landing_page() -> impl IntoResponse {
    Json(json!({
        "title": "Metocean Data Server",
        "description": "OGC API server providing EDR and Features access to metocean data",
        "links": [
            {
                "href": "/",
                "rel": "self",
                "type": "application/json",
                "title": "This document"
            },
            {
                "href": "/edr/",
                "rel": "service-desc",
                "type": "application/json",
                "title": "EDR API"
            },
            {
                "href": "/features/",
                "rel": "service-desc",
                "type": "application/json",
                "title": "Features API"
            }
        ]
    }))
}
