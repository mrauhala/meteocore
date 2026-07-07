mod admin;
mod auto;
mod preview;
mod watcher;

use std::sync::{Arc, OnceLock, RwLock};

/// jemalloc as the global allocator (#493). The default glibc malloc
/// fragments badly under this workload — many threads cycling 1–32 MB
/// decode/render buffers interleaved with long-lived byte-bounded cache
/// entries pins nearly every 64 MB arena heap: prod reached a ~52 GB
/// anonymous footprint holding only ~16 GiB of live cache (~3× retention).
/// jemalloc's size-classed extents and dirty-page decay return freed memory
/// to the OS instead. Allocator stats are exported on `/metrics` as the
/// `jemalloc_*` gauges (see `admin.rs`) to watch the multiplier in prod.
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

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
    /// Override `[server].host`. Takes priority over config.
    host: Option<String>,
    /// Override `[server].port`. Takes priority over config. When neither this
    /// nor a config file is present, the binary auto-scans for a free port
    /// starting at the default (8000).
    port: Option<u16>,
    /// Path to the config file. Takes priority over the `CONFIG_PATH` env var
    /// and the `./config.toml` default. A path given here that does not exist
    /// is a hard error (unlike the default path, which falls back to built-in
    /// defaults when absent).
    config: Option<String>,
    /// Directories to auto-discover collections from (`--auto-collections`,
    /// repeatable). Each is scanned and turned into synthesized collections
    /// (#411); the results are merged with any config-file collections and
    /// validated together.
    auto_collections: Vec<String>,
}

/// Very small hand-rolled argv parser — the server only has a handful of
/// flags, so pulling in clap would be overkill.
///
/// Supported forms (each flag also accepts the `--flag=value` spelling):
///   --collections id1,id2
///   --host 0.0.0.0
///   --port 8011
///   --config /etc/meteocore/config.toml
///   -h / --help
fn parse_cli_args() -> CliArgs {
    let mut collections: Option<Vec<String>> = None;
    let mut host: Option<String> = None;
    let mut port: Option<u16> = None;
    let mut config: Option<String> = None;
    let mut auto_collections: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                eprintln!(
                    "Usage: server [OPTIONS]\n\
                     \n\
                     Options:\n  \
                       --collections <id1,id2,...>   Only load collections with these IDs\n  \
                       --host <HOST>                 Bind host (overrides [server].host)\n  \
                       --port <PORT>                 Bind port (overrides [server].port)\n  \
                       --config <PATH>               Config file path (overrides CONFIG_PATH)\n  \
                       --auto-collections <DIR>      Auto-discover collections from a directory\n                                \
                                 (repeatable; zarr/grib/querydata/csv/geojson)\n  \
                       -h, --help                    Show this help\n\
                     \n\
                     With no config file and no --port, the server binds localhost\n  \
                     and auto-selects the first free port at or above 8000.\n\
                     \n\
                     Environment:\n  \
                       CONFIG_PATH    Path to config.toml (default: ./config.toml; --config wins)\n  \
                       BASE_URL       External base URL for links (wins over --host/--port)\n  \
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
            "--host" => {
                let Some(value) = args.next() else {
                    eprintln!("error: --host requires a value");
                    std::process::exit(2);
                };
                host = Some(value);
            }
            s if s.starts_with("--host=") => {
                host = Some(s["--host=".len()..].to_string());
            }
            "--port" => {
                let Some(value) = args.next() else {
                    eprintln!("error: --port requires a value");
                    std::process::exit(2);
                };
                port = Some(parse_port(&value));
            }
            s if s.starts_with("--port=") => {
                port = Some(parse_port(&s["--port=".len()..]));
            }
            "--config" => {
                let Some(value) = args.next() else {
                    eprintln!("error: --config requires a value");
                    std::process::exit(2);
                };
                config = Some(value);
            }
            s if s.starts_with("--config=") => {
                config = Some(s["--config=".len()..].to_string());
            }
            "--auto-collections" => {
                let Some(value) = args.next() else {
                    eprintln!("error: --auto-collections requires a directory");
                    std::process::exit(2);
                };
                auto_collections.push(value);
            }
            s if s.starts_with("--auto-collections=") => {
                auto_collections.push(s["--auto-collections=".len()..].to_string());
            }
            other => {
                eprintln!("error: unknown argument '{other}' (try --help)");
                std::process::exit(2);
            }
        }
    }
    CliArgs {
        collections,
        host,
        port,
        config,
        auto_collections,
    }
}

/// Parse a `--port` value, exiting with code 2 on anything that isn't a valid
/// TCP port (1..=65535). `u16::from_str` already rejects negatives and values
/// above 65535; we additionally reject 0 (not a bindable listen port).
fn parse_port(s: &str) -> u16 {
    match s.trim().parse::<u16>() {
        Ok(p) if p > 0 => p,
        _ => {
            eprintln!("error: --port must be a number in 1..=65535 (got '{s}')");
            std::process::exit(2);
        }
    }
}

/// Number of consecutive ports to try when auto-selecting a free one.
const AUTO_PORT_SCAN: u16 = 100;

/// Bind `host` on the first free port at or above `start`, scanning up to
/// [`AUTO_PORT_SCAN`] ports. Returns the listener and the address it bound.
///
/// Only "address in use" advances to the next port; any other bind error
/// (permission denied, unresolvable host, …) won't be fixed by trying another
/// port, so it stops and returns `None`.
async fn bind_auto_port(host: &str, start: u16) -> Option<(tokio::net::TcpListener, String)> {
    for offset in 0..AUTO_PORT_SCAN {
        let port = start.checked_add(offset)?;
        let addr = format!("{host}:{port}");
        match tokio::net::TcpListener::bind(&addr).await {
            Ok(listener) => return Some((listener, addr)),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                tracing::debug!("Port {port} in use, trying next");
            }
            Err(e) => {
                tracing::error!("Failed to bind {addr}: {e}");
                return None;
            }
        }
    }
    None
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

    // Resolve the config path: --config flag > CONFIG_PATH env > ./config.toml.
    let config_path = cli
        .config
        .clone()
        .or_else(|| std::env::var("CONFIG_PATH").ok())
        .unwrap_or_else(|| "config.toml".to_string());

    // Whether a config file is actually present drives the no-config boot path
    // (built-in defaults + auto-port) and whether an all-failed load is fatal.
    let config_present = std::path::Path::new(&config_path).exists();

    let (mut config, config_warnings) = if config_present {
        match ds_core::config::ServerConfig::from_file(&config_path) {
            Ok(result) => result,
            Err(e) => {
                tracing::error!("Failed to load {}: {}", config_path, e);
                std::process::exit(1);
            }
        }
    } else if cli.config.is_some() {
        // An explicitly requested config file that doesn't exist is a hard
        // error — don't silently ignore a typo'd --config path.
        tracing::error!("Config file not found: {}", config_path);
        std::process::exit(1);
    } else {
        // No config file at the default path and none requested: boot from
        // built-in defaults (localhost + auto-selected port). Collections come
        // from --auto-collections, if given (#411); otherwise the server starts
        // empty.
        let source = if cli.auto_collections.is_empty() {
            "no collections"
        } else {
            "collections from --auto-collections"
        };
        tracing::warn!(
            "No config file at '{config_path}'; starting with built-in defaults \
             (localhost, auto-selected port, {source})."
        );
        (
            ds_core::config::ServerConfig::default_for_auto(),
            Vec::new(),
        )
    };
    for warning in &config_warnings {
        tracing::warn!("{warning}");
    }

    // Auto-discover collections from any --auto-collections directories (#411)
    // and merge them with the config-file collections. The merged set goes
    // through the same validate() as TOML collections (so duplicate ids across
    // config + auto, or auto + auto, are rejected uniformly).
    if !cli.auto_collections.is_empty() {
        let discovered = auto::scan_roots(&cli.auto_collections);
        info!(
            "--auto-collections: discovered {} collection(s) across {} director(ies)",
            discovered.len(),
            cli.auto_collections.len()
        );
        config.collections.extend(discovered);
        if let Err(e) = config.validate() {
            tracing::error!("Auto-discovered collections failed validation: {e}");
            std::process::exit(1);
        }
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

    // CLI host/port overrides take priority over config.
    if let Some(host) = &cli.host {
        config.server.host = host.clone();
    }
    if let Some(port) = cli.port {
        config.server.port = port;
    }

    // The port is "pinned" when it comes from a config file or an explicit
    // --port: bind exactly that, and a conflict is fatal. With neither (the
    // no-config-file boot), auto-scan upward from the default for a free port.
    let port_pinned = config_present || cli.port.is_some();

    // Bind the listen socket before loading collections: engine
    // construction runs synchronous S3/disk scans that can take
    // minutes, so a port conflict must fail fast rather than after
    // that whole load. The listener is held until `axum::serve`.
    let host = config.server.host.clone();
    let (listener, addr) = if port_pinned {
        let addr = format!("{host}:{}", config.server.port);
        match tokio::net::TcpListener::bind(&addr).await {
            Ok(listener) => (listener, addr),
            Err(e) => {
                tracing::error!("Failed to bind {addr}: {e}");
                std::process::exit(1);
            }
        }
    } else {
        match bind_auto_port(&host, config.server.port).await {
            Some(bound) => bound,
            None => {
                tracing::error!(
                    "Failed to find a free port for {host} starting at {} (tried {AUTO_PORT_SCAN})",
                    config.server.port
                );
                std::process::exit(1);
            }
        }
    };
    // Reflect the actually-bound port back into config so base_url() and any
    // downstream use see the real value (the auto-scan may have moved it). A
    // just-bound listener always has a local address (plain getsockname), but
    // don't swallow the impossible case: a wrong write-back would make
    // base_url()/logs/reload silently report the wrong port.
    config.server.port = listener
        .local_addr()
        .expect("bound TcpListener must have a local address")
        .port();

    let base_url = config.server.base_url();
    info!("Base URL: {base_url}");
    // The socket is bound + listening from here, but HTTP is not served
    // until `axum::serve` below — keep the message explicit so a
    // log-watching operator isn't misled. (A TCP-only readiness probe
    // will see the port open early; probe `/health` over HTTP instead.)
    info!("Socket bound to {addr} — loading collections, not yet serving");

    let result = admin::load_collections(
        &config.collections,
        &config.style_bundles,
        &base_url,
        config.server.trust_proxy_headers,
        config.server.metatile_cache_mb,
        // Startup builds the render caches fresh; reloads reuse them.
        admin::ReusableCaches::default(),
    );

    let loaded = result
        .health
        .iter()
        .filter(|h| h.status != admin::CollectionStatus::Failed)
        .count();

    if loaded == 0 {
        if config.collections.is_empty() {
            // Nothing was configured to fail — a legitimately empty server
            // (e.g. the no-config-file boot). Start it: it answers /health and
            // an empty /collections, and can be populated via reload.
            tracing::warn!(
                "Starting with no collections. The server responds to /health and an empty \
                 /collections; add collections via config + POST /admin/collections/reload."
            );
        } else {
            tracing::error!(
                "No collections loaded successfully ({} configured). Refusing to start an empty server.",
                config.collections.len()
            );
            std::process::exit(1);
        }
    } else {
        info!(
            "Loaded {}/{} collections successfully",
            loaded,
            config.collections.len()
        );
    }

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
    // PostGIS metadata refresh loop — keeps the location list / extents / the
    // `locations_window` "currently reporting" set current without a reload.
    for engine in &result.postgis_engines {
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
        // Loopback default is safe; a non-loopback bind (e.g. `--host 0.0.0.0`)
        // with no token exposes POST /admin/collections/reload to the network.
        let host = config.server.host.as_str();
        if !matches!(host, "127.0.0.1" | "::1" | "localhost") {
            tracing::warn!(
                "Admin endpoint is UNAUTHENTICATED and bound to a non-loopback host ({host}): \
                 POST /admin/collections/reload is reachable from the network. Set ADMIN_TOKEN, \
                 or bind to 127.0.0.1."
            );
        }
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
        cap_engines: RwLock::new(result.cap_engines),
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
        .route(
            "/",
            get(move |headers: axum::http::HeaderMap| {
                root_landing_page(root_state.clone(), headers)
            }),
        )
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

async fn root_landing_page(state: AdminState, headers: axum::http::HeaderMap) -> impl IntoResponse {
    let edr_state = state.edr.load_full();
    let base = &ds_core::proxy::resolve_base_url(
        &edr_state.base_url,
        edr_state.trust_proxy_headers,
        |name| headers.get(name).and_then(|v| v.to_str().ok()),
    );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_collection_list_trims_and_drops_empties() {
        assert_eq!(parse_collection_list("a, b ,,c"), vec!["a", "b", "c"]);
        assert!(parse_collection_list("  ,  ").is_empty());
    }

    #[test]
    fn parse_port_accepts_valid() {
        assert_eq!(parse_port("8080"), 8080);
        assert_eq!(parse_port(" 1 "), 1);
        assert_eq!(parse_port("65535"), 65535);
    }

    #[tokio::test]
    async fn bind_auto_port_picks_start_when_free() {
        // Grab a free port from the OS as the scan start. We can't release and
        // re-bind it atomically, so don't assert an exact match (another process
        // could claim it in the gap); instead assert the scan lands within the
        // window starting at `free`. The skips-used-port test covers advancing
        // past a held port.
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let free = probe.local_addr().unwrap().port();
        drop(probe);

        let (listener, _addr) = bind_auto_port("127.0.0.1", free)
            .await
            .expect("should find a free port in the scan window");
        let chosen = listener.local_addr().unwrap().port();
        assert!(
            chosen >= free && chosen < free.saturating_add(AUTO_PORT_SCAN),
            "expected a port in [{free}, {}), got {chosen}",
            free.saturating_add(AUTO_PORT_SCAN)
        );
    }

    #[tokio::test]
    async fn bind_auto_port_skips_used_port() {
        // Hold a port, then confirm the scan moves past it to a higher one.
        let held = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let start = held.local_addr().unwrap().port();

        let (listener, _addr) = bind_auto_port("127.0.0.1", start)
            .await
            .expect("should find a free port above the held one");
        let chosen = listener.local_addr().unwrap().port();
        assert!(
            chosen > start,
            "expected a port above {start}, got {chosen}"
        );
    }
}
