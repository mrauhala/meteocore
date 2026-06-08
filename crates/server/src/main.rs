mod admin;
mod preview;
mod watcher;

use std::sync::{Arc, OnceLock, RwLock};

/// Dedicated multi-thread runtime for background collection poll loops.
///
/// Poll/scan loops do blocking I/O via `ds_storage::DataStore`, which parks the
/// calling worker thread (`block_in_place`) for the whole network round-trip.
/// Running them on the main request-serving runtime lets a slow/heavy poll
/// (e.g. a GRIB new-run probe doing dozens of sequential byte-range reads)
/// starve the worker pool and spike WMS latency for every collection (#221).
/// Isolating polls on their own runtime keeps that blocking off the request
/// path. `block_in_place` still works here because this is a multi-thread
/// runtime; it would panic on a `spawn_blocking` pool thread, which is why we
/// use a separate runtime rather than wrapping the sync scan in `spawn_blocking`.
///
/// The runtime lives in a `OnceLock` for the whole process. On exit it is
/// leaked (statics are not dropped), so poll tasks are aborted rather than
/// drained — which is safe here: a poll only reads the (read-only) data store
/// and swaps an in-memory `ArcSwap` catalog atomically, so an aborted scan
/// leaves no partial or corrupt state and the catalog is simply rebuilt on the
/// next start. Poll loops also observe the per-engine shutdown watch channel
/// and exit their `select!` cleanly when signalled before exit.
///
/// `worker_threads(4)` gives headroom so several collections polling at once
/// (or a reload spawning fresh loops while old scans are still blocked) don't
/// serialise on too few threads; `block_in_place` additionally lets Tokio spin
/// up temporary replacement workers while a poll blocks.
pub(crate) fn poll_runtime() -> &'static tokio::runtime::Handle {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .thread_name("mc-poll")
            .enable_all()
            .build()
            .expect("failed to build background poll runtime")
    })
    .handle()
}

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::middleware;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router, ServiceExt};
use serde_json::json;
use tokio::signal;
use tower::Layer;
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::normalize_path::NormalizePathLayer;
use tracing::info;
use tracing_subscriber::EnvFilter;

use admin::{AdminState, ServerState};

/// Initialize tracing based on the `LOG_FORMAT` env var.
///
/// * `LOG_FORMAT=json` — newline-delimited JSON, one object per event.
///   Use this in production so Alloy / Promtail / Loki can parse fields
///   without regex stages.
/// * anything else (or unset) — human-readable ANSI output for dev.
///
/// Filter is controlled by `RUST_LOG` (defaults to `info`).
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let format = std::env::var("LOG_FORMAT").unwrap_or_default();
    if format.eq_ignore_ascii_case("json") {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(false)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
}

/// Parsed command-line flags.
struct CliArgs {
    /// Optional allowlist of collection IDs to load. When present, only
    /// collections whose `id` matches one of these values are instantiated;
    /// all others are silently skipped. Useful for smoke-testing a single
    /// collection without editing `config.toml`.
    collections: Option<Vec<String>>,
}

/// Very small hand-rolled argv parser — the server only has a couple of
/// flags, so pulling in clap would be overkill.
///
/// Supported forms:
///   --collections=id1,id2
///   --collections id1,id2
///   -h / --help
fn parse_cli_args() -> CliArgs {
    let mut collections: Option<Vec<String>> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                eprintln!(
                    "Usage: server [OPTIONS]\n\
                     \n\
                     Options:\n  \
                       --collections <id1,id2,...>   Only load collections with these IDs\n  \
                       -h, --help                    Show this help\n\
                     \n\
                     Environment:\n  \
                       CONFIG_PATH    Path to config.toml (default: ./config.toml)\n  \
                       LOG_FORMAT     'json' for structured logs (default: human-readable)\n  \
                       RUST_LOG       Log filter (default: info)\n  \
                       ADMIN_TOKEN    Bearer token for admin endpoints"
                );
                std::process::exit(0);
            }
            "--collections" => {
                let Some(list) = args.next() else {
                    eprintln!("error: --collections requires a value");
                    std::process::exit(2);
                };
                collections = Some(parse_collection_list(&list));
            }
            s if s.starts_with("--collections=") => {
                let list = &s["--collections=".len()..];
                collections = Some(parse_collection_list(list));
            }
            other => {
                eprintln!("error: unknown argument '{other}' (try --help)");
                std::process::exit(2);
            }
        }
    }
    CliArgs { collections }
}

fn parse_collection_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

#[tokio::main]
async fn main() {
    init_tracing();

    let cli = parse_cli_args();

    let config_path = std::env::var("CONFIG_PATH").unwrap_or_else(|_| "config.toml".to_string());
    let (mut config, config_warnings) = match ds_core::config::ServerConfig::from_file(&config_path)
    {
        Ok(result) => result,
        Err(e) => {
            tracing::error!("Failed to load {}: {}", config_path, e);
            std::process::exit(1);
        }
    };
    for warning in &config_warnings {
        tracing::warn!("{warning}");
    }

    // Apply --collections filter if provided. Unknown IDs are a hard error —
    // typing `--collections noa-gfs` should not silently load nothing.
    if let Some(allow) = &cli.collections {
        let configured: std::collections::HashSet<&str> =
            config.collections.iter().map(|c| c.id.as_str()).collect();
        let unknown: Vec<&str> = allow
            .iter()
            .filter(|id| !configured.contains(id.as_str()))
            .map(|s| s.as_str())
            .collect();
        if !unknown.is_empty() {
            tracing::error!(
                "--collections: unknown collection ID(s) {:?} (known: {:?})",
                unknown,
                configured
            );
            std::process::exit(2);
        }
        let before = config.collections.len();
        config
            .collections
            .retain(|c| allow.iter().any(|id| id == &c.id));
        info!(
            "--collections filter: kept {}/{} collections ({:?})",
            config.collections.len(),
            before,
            allow
        );
    }

    let base_url = config.server.base_url();
    info!("Base URL: {base_url}");

    // Bind the listen socket before loading collections: engine
    // construction runs synchronous S3/disk scans that can take
    // minutes, so a port conflict must fail fast rather than after
    // that whole load. The listener is held until `axum::serve`.
    let addr = format!("{}:{}", config.server.host, config.server.port);
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(listener) => listener,
        Err(e) => {
            tracing::error!("Failed to bind {addr}: {e}");
            std::process::exit(1);
        }
    };
    // The socket is bound + listening from here, but HTTP is not served
    // until `axum::serve` below — keep the message explicit so a
    // log-watching operator isn't misled. (A TCP-only readiness probe
    // will see the port open early; probe `/health` over HTTP instead.)
    info!("Socket bound to {addr} — loading collections, not yet serving");

    let result = admin::load_collections(
        &config.collections,
        &config.style_bundles,
        &base_url,
        config.server.metatile_cache_mb,
    );

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

    // Spawn poll loops on the dedicated background runtime so their blocking
    // I/O never parks a request-serving worker (#221).
    for engine in &result.geotiff_engines {
        let poller = engine.clone();
        poll_runtime().spawn(async move {
            poller.poll_loop().await;
        });
    }
    for engine in &result.querydata_engines {
        let poller = engine.clone();
        poll_runtime().spawn(async move {
            poller.poll_loop().await;
        });
    }
    for engine in &result.grib_engines {
        let poller = engine.clone();
        poll_runtime().spawn(async move {
            poller.poll_loop().await;
        });
    }
    for engine in &result.zarr_engines {
        let poller = engine.clone();
        poll_runtime().spawn(async move {
            poller.poll_loop().await;
        });
    }
    for engine in &result.odim_engines {
        let poller = engine.clone();
        poll_runtime().spawn(async move {
            poller.poll_loop().await;
        });
    }
    for engine in &result.odim_volume_engines {
        let poller = engine.clone();
        poll_runtime().spawn(async move {
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
    let tiles_3d_swap = Arc::new(ArcSwap::from_pointee(result.tiles_3d_state));

    // Resolve admin token: ADMIN_TOKEN env var takes priority over config
    let admin_token = std::env::var("ADMIN_TOKEN")
        .ok()
        .or(config.server.admin_token.clone());
    if admin_token.is_some() {
        info!("Admin endpoint authentication enabled");
    } else {
        info!("Admin endpoint authentication disabled (no ADMIN_TOKEN set)");
    }

    // Resolve the collections_dir to watch (issue #318) before `config_path` is
    // moved into `server_state` below. `from_file` already canonicalized and
    // validated the dir at config load, so it exists here.
    let watch_dir: Option<std::path::PathBuf> = if config.server.watch_collections_dir {
        match config.server.collections_dir.as_deref() {
            Some(dir) => {
                let parent = std::path::Path::new(&config_path)
                    .parent()
                    .unwrap_or_else(|| std::path::Path::new("."));
                match parent.join(dir).canonicalize() {
                    Ok(p) => Some(p),
                    Err(e) => {
                        tracing::warn!(
                            "watch_collections_dir: cannot resolve collections_dir '{dir}': {e} \
                             — auto-reload disabled"
                        );
                        None
                    }
                }
            }
            None => {
                tracing::warn!(
                    "watch_collections_dir is set but collections_dir is not — nothing to watch"
                );
                None
            }
        }
    } else {
        None
    };

    let server_state: AdminState = Arc::new(ServerState {
        edr: edr_swap.clone(),
        features: features_swap.clone(),
        wms: wms_swap.clone(),
        maps: maps_swap.clone(),
        tiles: tiles_swap.clone(),
        tiles_3d: tiles_3d_swap.clone(),
        config_path,
        health: RwLock::new(result.health),
        geotiff_engines: RwLock::new(result.geotiff_engines),
        querydata_engines: RwLock::new(result.querydata_engines),
        grib_engines: RwLock::new(result.grib_engines),
        zarr_engines: RwLock::new(result.zarr_engines),
        odim_engines: RwLock::new(result.odim_engines),
        odim_volume_engines: RwLock::new(result.odim_volume_engines),
        postgis_engines: RwLock::new(result.postgis_engines),
        reload_lock: tokio::sync::Mutex::new(()),
        admin_token,
    });

    // Start the collections_dir watcher (issue #318) if enabled. Best-effort:
    // a watcher init failure logs and the server runs without auto-reload.
    if let Some(dir) = watch_dir {
        // Trust-model note: watch-triggered reloads are gated by write access to
        // `collections_dir` (a local-filesystem control plane), NOT the HTTP
        // `ADMIN_TOKEN` that gates `POST /admin/collections/reload`. Make the
        // asymmetry explicit when both are in play (e.g. a shared/NFS dir).
        if server_state.admin_token.is_some() {
            tracing::warn!(
                "collections_dir watcher is enabled and an admin token is set: \
                 filesystem-triggered reloads do NOT require the token — they are \
                 authorized by write access to collections_dir. Ensure only trusted \
                 principals can write there."
            );
        }
        let debounce = std::time::Duration::from_millis(config.server.watch_debounce_ms);
        if let Err(e) = watcher::spawn_collections_watcher(server_state.clone(), dir, debounce) {
            tracing::warn!("Failed to start collections_dir watcher: {e}");
        }
    }

    let root_state = server_state.clone();

    // Public routes get permissive CORS (OGC APIs, health, metrics)
    let public = Router::new()
        .route("/", get(move || root_landing_page(root_state)))
        .nest("/edr", api_edr::router(edr_swap.clone()))
        .nest("/features", api_features::router(features_swap.clone()))
        .nest("/wms", api_wms::router(wms_swap.clone()))
        .nest("/maps", api_maps::router(maps_swap.clone()))
        .nest("/tiles", api_tiles::router(tiles_swap.clone()))
        .nest("/3dtiles", api_3dtiles::router(tiles_3d_swap.clone()))
        // Trailing-slash variants so /edr/, /features/, /maps/, /tiles/, /3dtiles/ also work
        .route(
            "/3dtiles/",
            get(api_3dtiles::handlers::landing_page).with_state(tiles_3d_swap),
        )
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
        .route(
            "/metrics",
            get(admin::metrics_handler).with_state(server_state.clone()),
        )
        .route(
            "/preview/manifest.json",
            get(preview::manifest_handler).with_state(server_state.clone()),
        )
        // `NormalizePathLayer::trim_trailing_slash()` is wrapped around
        // the whole app a few lines below — it rewrites `/preview/` to
        // `/preview` before the router runs, so a single literal route
        // handles both forms.
        .route("/preview", get(preview::index_handler))
        .route("/preview/{*path}", get(preview::asset_handler));

    // Admin routes (protected by bearer token auth, not CORS)
    let admin_routes = Router::new().route(
        "/admin/collections/reload",
        post(admin::reload_handler).with_state(server_state.clone()),
    );

    let app = public
        .merge(admin_routes)
        .layer(middleware::from_fn(admin::metrics_middleware))
        .layer(middleware::from_fn(admin::request_logging_middleware))
        .layer(middleware::from_fn(admin::correlation_id_middleware))
        .layer(CorsLayer::permissive())
        .layer(CompressionLayer::new());

    // Normalize trailing slashes (e.g., /wms/ → /wms) before routing
    let app = NormalizePathLayer::trim_trailing_slash().layer(app);

    info!("Server ready, accepting requests");

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
    let zarr = server_state
        .zarr_engines
        .read()
        .unwrap_or_else(|e| e.into_inner());
    for engine in zarr.iter() {
        engine.shutdown();
    }
    let odim = server_state
        .odim_engines
        .read()
        .unwrap_or_else(|e| e.into_inner());
    for engine in odim.iter() {
        engine.shutdown();
    }
    let odim_volume = server_state
        .odim_volume_engines
        .read()
        .unwrap_or_else(|e| e.into_inner());
    for engine in odim_volume.iter() {
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
                "href": format!("{base}/3dtiles/"),
                "rel": "child",
                "type": "application/json",
                "title": "3D Tiles API"
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
