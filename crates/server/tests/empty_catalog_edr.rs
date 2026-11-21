//! EDR on gridded collections with no data yet (#1499): a fresh source, a
//! store unreachable since the start, a mistyped prefix. A GRIB directory
//! without files, a Zarr store whose time axis is still empty and a NetCDF
//! directory without files answer every query type with the 404 JSON body
//! of `DataServerError::no_data_yet`, a `datetime` list included, and
//! nothing on the way logs at ERROR. GRIB and Zarr used to answer 500 and
//! log `Engine error: No forecast data available` at ERROR.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, Once};

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use ds_core::config::CollectionConfig;
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;

/// Every ERROR line any thread logs, the query runtime's included.
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

fn collection(id: &str, engine_type: &str, settings: Value) -> CollectionConfig {
    let mut config = json!({
        "id": id, "title": id, "description": "No data yet", "engine_type": engine_type,
    });
    config[engine_type] = settings;
    serde_json::from_value(config).unwrap()
}

/// A Zarr v2 store of `temp` on a 2 × 2 lat/lon grid whose `time` axis has
/// length zero, as a store waiting for its first append: metadata only.
fn write_empty_zarr(path: &Path) {
    std::fs::write(path.join(".zgroup"), br#"{"zarr_format":2}"#).unwrap();
    let array = |name: &str, shape: &[u64], chunks: &[u64], dims: &[&str], units: &str| {
        let dir = path.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let metadata = json!({
            "zarr_format": 2, "shape": shape, "chunks": chunks,
            "dtype": if name == "temp" { "<f4" } else { "<f8" },
            "compressor": null, "fill_value": "NaN", "order": "C",
            "filters": null, "dimension_separator": "."
        });
        let attributes = json!({"_ARRAY_DIMENSIONS": dims, "units": units});
        std::fs::write(dir.join(".zarray"), metadata.to_string()).unwrap();
        std::fs::write(dir.join(".zattrs"), attributes.to_string()).unwrap();
    };
    array("time", &[0], &[1], &["time"], "hours since 2026-01-01");
    for (name, values, units) in [
        ("lat", [59.0f64, 61.0], "degrees_north"),
        ("lon", [9.0, 11.0], "degrees_east"),
    ] {
        array(name, &[2], &[2], &[name], units);
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        std::fs::write(path.join(name).join("0"), bytes).unwrap();
    }
    array("temp", &[0, 2, 2], &[1, 2, 2], &["time", "lat", "lon"], "K");
}

/// The EDR router over the three empty collections; the directories live
/// as long as the router is used.
fn router(grib: &Path, zarr: &Path, netcdf: &Path) -> axum::Router {
    let configs = [
        collection(
            "nwp-empty",
            "grib",
            json!({"data_path": grib.to_str().unwrap()}),
        ),
        collection(
            "zarr-empty",
            "zarr",
            json!({"data_path": zarr.to_str().unwrap()}),
        ),
        collection(
            "netcdf-empty",
            "netcdf",
            json!({"data_path": netcdf.to_str().unwrap()}),
        ),
    ];
    let mut engines: HashMap<String, Arc<dyn EdrEngine>> = HashMap::new();
    for config in &configs {
        let engine: Arc<dyn EdrEngine> = match config.engine_type.as_str() {
            "grib" => Arc::new(
                engine_grib::GribEngine::new(&config.id, config.grib.as_ref().unwrap())
                    .expect("an empty GRIB directory loads"),
            ),
            "zarr" => Arc::new(
                engine_zarr::ZarrEngine::new(&config.id, config.zarr.as_ref().unwrap())
                    .expect("a Zarr store with an empty time axis loads"),
            ),
            "netcdf" => Arc::new(
                engine_netcdf::NetcdfEngine::new(&config.id, config.netcdf.as_ref().unwrap())
                    .expect("an empty NetCDF directory loads"),
            ),
            other => panic!("no engine {other}"),
        };
        engines.insert(config.id.clone(), engine);
    }
    let state = api_edr::handlers::EdrState {
        engines,
        collections: configs.into_iter().map(|c| (c.id.clone(), c)).collect(),
        discovery: Default::default(),
        styles: HashMap::new(),
        feature_engines: HashMap::new(),
        base_url: String::new(),
        trust_proxy_headers: false,
    };
    api_edr::router(Arc::new(ArcSwap::from_pointee(state)))
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_collections_answer_every_query_404_without_logging_an_error() {
    capture_errors();
    let (grib, zarr, netcdf) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    write_empty_zarr(zarr.path());
    let app = router(grib.path(), zarr.path(), netcdf.path());
    let expected = json!({
        "code": "NotFound",
        "description": DataServerError::no_data_yet().to_string(),
    });
    assert_eq!(expected["description"], "No data available yet");

    let queries = [
        "position?coords=POINT(10%2060)",
        "area?coords=POLYGON((9%2059,11%2059,11%2061,9%2061,9%2059))",
        "radius?coords=POINT(10%2060)&within=10&within-units=km",
        "trajectory?coords=LINESTRING(9%2060,11%2060)",
        // An M path selects its run by its first time, as a map does.
        "trajectory?coords=LINESTRING%20M(9%2060%201792044000,11%2060%201792065600)",
        // A list skips each instant without data: none has any.
        "position?coords=POINT(10%2060)&datetime=2026-10-15T06:00:00Z,2026-10-15T09:00:00Z",
        "area?coords=POLYGON((9%2059,11%2059,11%2061,9%2061,9%2059))\
         &datetime=2026-10-15T06:00:00Z,2026-10-15T09:00:00Z",
        "position?coords=POINT(10%2060)&datetime=2026-10-15T06:00:00Z/2026-10-15T12:00:00Z",
    ];
    for id in ["nwp-empty", "zarr-empty", "netcdf-empty"] {
        let mut uris: Vec<String> = queries
            .iter()
            .map(|query| format!("/collections/{id}/{query}"))
            .collect();
        if id == "netcdf-empty" {
            uris.push(format!("/collections/{id}/cube?bbox=9,59,11,61"));
        }
        for uri in uris {
            let (status, body) = get(&app, &uri).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{uri}: {body}");
            assert_eq!(body, expected, "{uri}");
        }
    }
    let logged = String::from_utf8(ERRORS.lock().unwrap().clone()).unwrap();
    assert!(logged.is_empty(), "nothing is logged at ERROR: {logged}");
}
