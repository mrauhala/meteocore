//! A remote GeoTIFF collection whose object store keeps answering 503
//! through every retry (#1523): EDR position and area, WMS GetMap, direct
//! and meta-tiled, and an OGC API Maps map answer 503 `SourceUnavailable`
//! with `Retry-After: 10` and a description naming no upstream, as every
//! other engine's unavailable source does (#1355), never the busy answer's
//! `Retry-After: 1` or a red WMS image, and nothing logs at ERROR. A store
//! answering 500 through its retries keeps the busy answer. Nothing is
//! cached for either: once the store answers, the same requests draw the
//! data, WMS as a cache miss.
//!
//! The collection reads `testdata/radar`'s 23:15 frame by range from
//! `ds_storage::test_util::FailingReads`, which fails a GET as object_store
//! does when the store answered every attempt with one status.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Once};

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use ds_core::config::{CollectionConfig, GeoTiffConfig};
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::map_engine::MapEngine;
use ds_core::web_mercator::{lat_to_y, lon_to_x};
use ds_render::{RenderedCache, StyleContext};
use ds_storage::test_util::FailingReads;
use ds_storage::DataStore;
use engine_geotiff::GeoTiffEngine;

const ID: &str = "radar-remote";

/// Every ERROR line any thread logs, the query and render workers' included.
static ERRORS: Mutex<Vec<u8>> = Mutex::new(Vec::new());

struct ErrorSink;

impl std::io::Write for ErrorSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        ERRORS.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn capture_errors() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::ERROR)
            .with_writer(|| ErrorSink)
            .finish();
        tracing::subscriber::set_global_default(subscriber).unwrap();
    });
}

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/radar/radar_20260324T2315Z.tif")
}

/// The EDR, WMS and Maps routers over one GeoTIFF collection read from
/// `store`, as the server registers it.
struct Apis {
    edr: axum::Router,
    wms: axum::Router,
    maps: axum::Router,
}

fn apis(store: Arc<FailingReads>) -> Apis {
    let config: GeoTiffConfig = serde_json::from_value(serde_json::json!({
        "filename_template": "radar_%Y%m%dT%H%MZ.tif",
        "parameter": "reflectivity",
        "unit": "dBZ",
        "poll_interval_secs": 3600,
    }))
    .unwrap();
    let engine = Arc::new(
        GeoTiffEngine::remote_for_test(ID, &config, DataStore::new(store), "d")
            .expect("the engine catalogues the frame"),
    );
    assert_eq!(engine.raster_info().times.len(), 1, "one frame");
    let collection: CollectionConfig = serde_json::from_value(serde_json::json!({
        "id": ID, "title": "Radar", "description": "Radar reflectivity",
        "engine_type": "geotiff", "apis": ["edr", "wms", "maps"],
    }))
    .unwrap();
    let styles = HashMap::from([(
        ID.to_string(),
        StyleContext::with_builtins()
            .collection_styles(&collection, None)
            .unwrap(),
    )]);
    let collections = HashMap::from([(ID.to_string(), collection)]);
    let map_engine: Arc<dyn MapEngine> = engine.clone();
    let edr_engine: Arc<dyn EdrEngine> = engine;
    let edr = api_edr::router(Arc::new(ArcSwap::from_pointee(
        api_edr::handlers::EdrState {
            engines: HashMap::from([(ID.to_string(), edr_engine.clone())]),
            feature_engines: HashMap::new(),
            collections: collections.clone(),
            discovery: Default::default(),
            styles: HashMap::new(),
            base_url: String::new(),
            trust_proxy_headers: false,
        },
    )));
    let wms = api_wms::router(Arc::new(ArcSwap::from_pointee(api_wms::WmsState {
        engines: HashMap::from([(ID.to_string(), map_engine.clone())]),
        point_engines: HashMap::from([(ID.to_string(), edr_engine)]),
        collections: collections.clone(),
        styles: styles.clone(),
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        tile_cache: Arc::new(ds_render::TilePixelCache::new(64)),
        base_url: String::new(),
        trust_proxy_headers: false,
    })));
    let maps = api_maps::router(Arc::new(ArcSwap::from_pointee(api_maps::MapsState {
        engines: HashMap::from([(ID.to_string(), map_engine)]),
        map_tileset_ids: HashSet::new(),
        collections,
        discovery: Default::default(),
        styles,
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        base_url: String::new(),
        trust_proxy_headers: false,
    })));
    Apis { edr, wms, maps }
}

struct Answer {
    status: StatusCode,
    retry_after: Option<String>,
    x_cache: Option<String>,
    body: String,
}

async fn get(app: &axum::Router, uri: &str) -> Answer {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .map(|v| v.to_str().unwrap().to_string())
    };
    let (retry_after, x_cache) = (header("retry-after"), header("x-cache"));
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    Answer {
        status,
        retry_after,
        x_cache,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

/// A view around a source pixel of the frame, lon/lat.
const WEST: f64 = 10.9;
const SOUTH: f64 = 67.8;
const EAST: f64 = 11.3;
const NORTH: f64 = 68.0;

/// Each request the test makes, with the API that answers it.
fn requests() -> Vec<(&'static str, String)> {
    let crs84 = format!("{WEST},{SOUTH},{EAST},{NORTH}");
    let mercator = format!(
        "{},{},{},{}",
        lon_to_x(WEST),
        lat_to_y(SOUTH),
        lon_to_x(EAST),
        lat_to_y(NORTH)
    );
    let get_map = |crs: &str, bbox: &str| {
        format!(
            "/?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS={ID}&STYLES=&CRS={crs}\
             &BBOX={bbox}&WIDTH=64&HEIGHT=64&FORMAT=image/png"
        )
    };
    vec![
        (
            "edr",
            format!("/collections/{ID}/position?coords=POINT(11.0765%2067.8999)"),
        ),
        (
            "edr",
            format!(
                "/collections/{ID}/area?coords=POLYGON((11%2067.85,11.1%2067.85,\
                 11.1%2067.95,11%2067.95,11%2067.85))"
            ),
        ),
        ("wms", get_map("CRS:84", &crs84)),
        ("wms", get_map("EPSG:3857", &mercator)),
        (
            "maps",
            format!("/collections/{ID}/map?bbox={crs84}&width=64&height=64"),
        ),
    ]
}

impl Apis {
    async fn get(&self, api: &str, uri: &str) -> Answer {
        let app = match api {
            "edr" => &self.edr,
            "wms" => &self.wms,
            _ => &self.maps,
        };
        get(app, uri).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_store_that_stays_unavailable_answers_source_unavailable_on_every_api() {
    capture_errors();
    let store = FailingReads::default();
    store.put(
        "d/radar_20260324T2315Z.tif",
        std::fs::read(fixture()).unwrap(),
    );
    let store = Arc::new(store);
    let apis = apis(store.clone());

    // Every GET answers 503, through every retry.
    store.fail_with(Some(503));
    for (api, uri) in requests() {
        let reads = store.reads();
        let answer = apis.get(api, &uri).await;
        assert_eq!(answer.status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        assert_eq!(answer.retry_after.as_deref(), Some("10"), "{uri}");
        assert!(
            answer
                .body
                .contains(DataServerError::UNAVAILABLE_DESCRIPTION),
            "{uri}: {}",
            answer.body
        );
        assert!(
            !answer.body.contains("non-2xx") && !answer.body.contains("radar_2026"),
            "{uri}: names no upstream: {}",
            answer.body
        );
        if api != "wms" {
            let body: serde_json::Value = serde_json::from_str(&answer.body).unwrap();
            assert_eq!(body["code"], "SourceUnavailable", "{uri}");
        }
        assert!(store.reads() > reads, "{uri}: the store was read");
    }
    let logged = String::from_utf8(ERRORS.lock().unwrap().clone()).unwrap();
    assert!(logged.is_empty(), "nothing is logged at ERROR: {logged}");

    // Every GET answers 500 through its retries: a fetch that fails
    // otherwise, the busy answer as before.
    store.fail_with(Some(500));
    for (api, uri) in requests() {
        let answer = apis.get(api, &uri).await;
        assert_eq!(answer.status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        assert_eq!(answer.retry_after.as_deref(), Some("1"), "{uri}");
        if api != "wms" {
            let body: serde_json::Value = serde_json::from_str(&answer.body).unwrap();
            assert_eq!(body["code"], "ServerBusy", "{uri}");
        }
    }

    // The store answers: nothing was cached for the failures.
    store.fail_with(None);
    for (api, uri) in requests() {
        let answer = apis.get(api, &uri).await;
        assert_eq!(answer.status, StatusCode::OK, "{uri}: {}", answer.body);
        if api == "wms" {
            assert_eq!(answer.x_cache.as_deref(), Some("MISS"), "{uri}");
        }
    }
}
