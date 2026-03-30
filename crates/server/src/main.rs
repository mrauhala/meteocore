mod admin;

use std::sync::{Arc, RwLock};

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::middleware;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router, ServiceExt};
use serde_json::json;
use tokio::signal;
use tower::Layer;
use tower_http::cors::CorsLayer;
use tower_http::normalize_path::NormalizePathLayer;
use tracing::info;

use admin::{AdminState, ServerState};

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

    let result = admin::load_collections(&config.collections, &base_url);

    let loaded = result
        .health
        .iter()
        .filter(|h| h.status != admin::CollectionStatus::Failed)
        .count();

    if loaded == 0 {
        tracing::error!(
            "No collections loaded successfully ({} configured). Refusing to start an empty server.",
            config.collections.len()
        );
        std::process::exit(1);
    }
    info!(
        "Loaded {}/{} collections successfully",
        loaded,
        config.collections.len()
    );

    // Spawn GeoTIFF poll loops
    for engine in &result.geotiff_engines {
        let poller = engine.clone();
        tokio::spawn(async move {
            poller.poll_loop().await;
        });
    }

    // Set initial health gauges
    admin::update_health_gauges(&result.health);

    // Build swappable state
    let edr_swap = Arc::new(ArcSwap::from_pointee(result.edr_state));
    let features_swap = Arc::new(ArcSwap::from_pointee(result.features_state));
    let wms_swap = Arc::new(ArcSwap::from_pointee(result.wms_state));
    let maps_swap = Arc::new(ArcSwap::from_pointee(result.maps_state));

    let server_state: AdminState = Arc::new(ServerState {
        edr: edr_swap.clone(),
        features: features_swap.clone(),
        wms: wms_swap.clone(),
        maps: maps_swap.clone(),
        config_path,
        health: RwLock::new(result.health),
        geotiff_engines: RwLock::new(result.geotiff_engines),
        reload_lock: tokio::sync::Mutex::new(()),
    });

    let root_state = server_state.clone();

    let app = Router::new()
        .route("/", get(move || root_landing_page(root_state)))
        .nest("/edr", api_edr::router(edr_swap.clone()))
        .nest("/features", api_features::router(features_swap.clone()))
        .nest("/wms", api_wms::router(wms_swap.clone()))
        .nest("/maps", api_maps::router(maps_swap.clone()))
        // Trailing-slash variants so /edr/, /features/, and /maps/ also work
        .route(
            "/edr/",
            get(api_edr::handlers::landing_page).with_state(edr_swap),
        )
        .route(
            "/features/",
            get(api_features::handlers::landing_page).with_state(features_swap),
        )
        .route(
            "/maps/",
            get(api_maps::handlers::landing_page).with_state(maps_swap),
        )
        // Admin endpoints
        .route(
            "/admin/collections/reload",
            post(admin::reload_handler).with_state(server_state.clone()),
        )
        .route(
            "/health",
            get(admin::health_handler).with_state(server_state.clone()),
        )
        .route("/metrics", get(admin::metrics_handler))
        // Middleware
        .layer(middleware::from_fn(admin::metrics_middleware))
        .layer(CorsLayer::permissive());

    // Normalize trailing slashes (e.g., /wms/ → /wms) before routing
    let app = NormalizePathLayer::trim_trailing_slash().layer(app);

    let addr = format!("{}:{}", config.server.host, config.server.port);
    info!("Starting server on {addr}");

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("Failed to bind");

    axum::serve(
        listener,
        ServiceExt::<axum::http::Request<Body>>::into_make_service(app),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .expect("Server error");

    // Signal all GeoTIFF polling loops to stop
    let engines = server_state.geotiff_engines.read().unwrap();
    for engine in engines.iter() {
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

async fn root_landing_page(state: AdminState) -> impl IntoResponse {
    let edr_state = state.edr.load_full();
    let base = &edr_state.base_url;
    Json(json!({
        "title": "MeteoCore",
        "description": "Metocean Data Server implementing OGC API - EDR, OGC API - Features, OGC API - Maps, and OGC WMS 1.3.0",
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
            },
            {
                "href": format!("{base}/wms?SERVICE=WMS&REQUEST=GetCapabilities"),
                "rel": "service-desc",
                "type": "text/xml",
                "title": "WMS 1.3.0"
            },
            {
                "href": format!("{base}/maps/"),
                "rel": "service-desc",
                "type": "application/json",
                "title": "Maps API"
            },
            {
                "href": format!("{base}/health"),
                "rel": "health",
                "type": "application/json",
                "title": "Health status"
            },
            {
                "href": format!("{base}/metrics"),
                "rel": "metrics",
                "type": "text/plain",
                "title": "Prometheus metrics"
            }
        ]
    }))
}
