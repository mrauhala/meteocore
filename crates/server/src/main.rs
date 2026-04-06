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

    // Spawn poll loops
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

    // Set initial health gauges
    admin::update_health_gauges(&result.health);

    // Build swappable state
    let edr_swap = Arc::new(ArcSwap::from_pointee(result.edr_state));
    let features_swap = Arc::new(ArcSwap::from_pointee(result.features_state));
    let wms_swap = Arc::new(ArcSwap::from_pointee(result.wms_state));
    let maps_swap = Arc::new(ArcSwap::from_pointee(result.maps_state));
    let tiles_swap = Arc::new(ArcSwap::from_pointee(result.tiles_state));

    // Resolve admin token: ADMIN_TOKEN env var takes priority over config
    let admin_token = std::env::var("ADMIN_TOKEN")
        .ok()
        .or(config.server.admin_token.clone());
    if admin_token.is_some() {
        info!("Admin endpoint authentication enabled");
    } else {
        info!("Admin endpoint authentication disabled (no ADMIN_TOKEN set)");
    }

    let server_state: AdminState = Arc::new(ServerState {
        edr: edr_swap.clone(),
        features: features_swap.clone(),
        wms: wms_swap.clone(),
        maps: maps_swap.clone(),
        tiles: tiles_swap.clone(),
        config_path,
        health: RwLock::new(result.health),
        geotiff_engines: RwLock::new(result.geotiff_engines),
        querydata_engines: RwLock::new(result.querydata_engines),
        grib_engines: RwLock::new(result.grib_engines),
        reload_lock: tokio::sync::Mutex::new(()),
        admin_token,
    });

    let root_state = server_state.clone();

    // Public routes get permissive CORS (OGC APIs, health, metrics)
    let public = Router::new()
        .route("/", get(move || root_landing_page(root_state)))
        .nest("/edr", api_edr::router(edr_swap.clone()))
        .nest("/features", api_features::router(features_swap.clone()))
        .nest("/wms", api_wms::router(wms_swap.clone()))
        .nest("/maps", api_maps::router(maps_swap.clone()))
        .nest("/tiles", api_tiles::router(tiles_swap.clone()))
        // Trailing-slash variants so /edr/, /features/, /maps/, and /tiles/ also work
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
        .route(
            "/tiles/",
            get(api_tiles::handlers::landing_page).with_state(tiles_swap),
        )
        .route(
            "/health",
            get(admin::health_handler).with_state(server_state.clone()),
        )
        .route("/metrics", get(admin::metrics_handler));

    // Admin routes (protected by bearer token auth, not CORS)
    let admin_routes = Router::new().route(
        "/admin/collections/reload",
        post(admin::reload_handler).with_state(server_state.clone()),
    );

    let app = public
        .merge(admin_routes)
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

    // Signal all polling loops to stop
    let geotiff = server_state
        .geotiff_engines
        .read()
        .unwrap_or_else(|e| e.into_inner());
    for engine in geotiff.iter() {
        engine.shutdown();
    }
    let querydata = server_state
        .querydata_engines
        .read()
        .unwrap_or_else(|e| e.into_inner());
    for engine in querydata.iter() {
        engine.shutdown();
    }
    let grib = server_state
        .grib_engines
        .read()
        .unwrap_or_else(|e| e.into_inner());
    for engine in grib.iter() {
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
        "description": "Metocean Data Server implementing OGC API - EDR, OGC API - Features, OGC API - Maps, OGC API - Tiles, and OGC WMS 1.3.0",
        "links": [
            {
                "href": format!("{base}/"),
                "rel": "self",
                "type": "application/json",
                "title": "This document"
            },
            {
                "href": format!("{base}/edr/"),
                "rel": "child",
                "type": "application/json",
                "title": "EDR API"
            },
            {
                "href": format!("{base}/edr/api"),
                "rel": "service-desc",
                "type": "application/vnd.oai.openapi+json;version=3.0",
                "title": "EDR API definition"
            },
            {
                "href": format!("{base}/edr/api/docs"),
                "rel": "service-doc",
                "type": "text/html",
                "title": "EDR API documentation"
            },
            {
                "href": format!("{base}/features/"),
                "rel": "child",
                "type": "application/json",
                "title": "Features API"
            },
            {
                "href": format!("{base}/features/api"),
                "rel": "service-desc",
                "type": "application/vnd.oai.openapi+json;version=3.0",
                "title": "Features API definition"
            },
            {
                "href": format!("{base}/features/api/docs"),
                "rel": "service-doc",
                "type": "text/html",
                "title": "Features API documentation"
            },
            {
                "href": format!("{base}/wms?SERVICE=WMS&REQUEST=GetCapabilities"),
                "rel": "service-desc",
                "type": "text/xml",
                "title": "WMS 1.3.0 Capabilities"
            },
            {
                "href": format!("{base}/maps/"),
                "rel": "child",
                "type": "application/json",
                "title": "Maps API"
            },
            {
                "href": format!("{base}/maps/api"),
                "rel": "service-desc",
                "type": "application/vnd.oai.openapi+json;version=3.0",
                "title": "Maps API definition"
            },
            {
                "href": format!("{base}/maps/api/docs"),
                "rel": "service-doc",
                "type": "text/html",
                "title": "Maps API documentation"
            },
            {
                "href": format!("{base}/tiles/"),
                "rel": "child",
                "type": "application/json",
                "title": "Tiles API"
            },
            {
                "href": format!("{base}/tiles/api"),
                "rel": "service-desc",
                "type": "application/vnd.oai.openapi+json;version=3.0",
                "title": "Tiles API definition"
            },
            {
                "href": format!("{base}/tiles/api/docs"),
                "rel": "service-doc",
                "type": "text/html",
                "title": "Tiles API documentation"
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
