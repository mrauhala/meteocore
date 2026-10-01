//! EDR cube queries (#925) on engine-grib, end to end over the committed
//! ECMWF fixture (a global 0.25° `q` field at 150 hPa, served as a pressure
//! level collection): the collection advertises cube, the CoverageJSON
//! validates against `schemas/coveragejson.json`, resampling picks the
//! native nodes, and the budget and antimeridian limits are 400s.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use ds_core::config::{CollectionConfig, GribConfig, GribLevelType};
use ds_core::edr_engine::EdrEngine;
use engine_grib::GribEngine;

const ID: &str = "forecast-pressure";

fn grib_config() -> GribConfig {
    GribConfig {
        level_types: Some(vec![GribLevelType::Pressure]),
        data_path: Some(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../testdata/grib-local")
                .to_string_lossy()
                .into_owned(),
        ),
        endpoint: None,
        bucket: None,
        prefix_pattern: None,
        index_suffix: None,
        data_suffix: None,
        poll_interval_secs: 600,
        max_runs: None,
        time_window: None,
        parameters: None,
        grid_cache_mb: 64,
        message_cache_mb: 0,
        run_hours: None,
        index_format: Some("ecmwf-json".to_string()),
        filename_contains: None,
    }
}

fn app() -> axum::Router {
    let owner = GribEngine::new("forecast", &grib_config()).unwrap();
    let view = owner
        .level_collections()
        .into_iter()
        .find(|v| v.collection_id() == ID)
        .expect("the fixture's 150 hPa field makes a pressure collection");
    let config = CollectionConfig {
        id: ID.into(),
        title: "GRIB pressure levels".into(),
        description: "ECMWF q at 150 hPa".into(),
        data_path: None,
        apis: vec!["edr".into()],
        engine_type: "grib".into(),
        keywords: Vec::new(),
        license: None,
        geotiff: None,
        querydata: None,
        wms: None,
        grib: Some(grib_config()),
        zarr: None,
        odim: None,
        cap: None,
        postgis: None,
        nowcast: None,
        bufr: None,
        satellite: None,
        preview: None,
    };
    let state = api_edr::handlers::EdrState {
        engines: HashMap::from([(ID.to_string(), Arc::new(view) as Arc<dyn EdrEngine>)]),
        collections: HashMap::from([(ID.to_string(), config)]),
        styles: HashMap::new(),
        base_url: String::new(),
        trust_proxy_headers: false,
    };
    api_edr::router(Arc::new(ArcSwap::from_pointee(state)))
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn schema(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../schemas")
        .join(name);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn assert_valid(schema: &Value, json: &Value, what: &str) {
    let validator = jsonschema::Validator::new(schema).unwrap();
    let errors: Vec<String> = validator
        .iter_errors(json)
        .map(|e| format!("- {e} (at {})", e.instance_path()))
        .collect();
    assert!(errors.is_empty(), "{what}:\n{}", errors.join("\n"));
}

fn numbers(value: &Value) -> Vec<f64> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_f64().unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grib_pressure_collection_serves_cube_queries() {
    let app = app();
    let covjson = schema("coveragejson.json");

    let (status, collection) = get(&app, &format!("/collections/{ID}")).await;
    assert_eq!(status, StatusCode::OK);
    let link = &collection["data_queries"]["cube"]["link"];
    assert_eq!(link["href"], format!("/edr/collections/{ID}/cube"));
    assert_eq!(link["variables"]["height_units"], json!(["hPa"]));
    let edr = schema("ogcapi-edr-1.1-bundled.json");
    assert_valid(
        &edr["paths"]["/collections/{collectionId}"]["get"]["responses"]["200"]["content"]
            ["application/json"]["schema"],
        &collection,
        "EDR collection",
    );

    // Native resolution: every 0.25° node of the bbox, one step, one level.
    let (status, native) = get(
        &app,
        &format!("/collections/{ID}/cube?bbox=20,55,30,65&z=150"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{native}");
    assert_valid(&covjson, &native, "native cube");
    let axes = &native["domain"]["axes"];
    let (x, y) = (numbers(&axes["x"]["values"]), numbers(&axes["y"]["values"]));
    assert_eq!((x.len(), y.len()), (41, 41));
    assert_eq!((x[0], x[40], y[0], y[40]), (20.0, 30.0, 55.0, 65.0));
    assert_eq!(axes["z"]["values"], json!([150.0]));
    assert_eq!(axes["t"]["values"], json!(["2026-04-05T00:00:00+00:00"]));
    let range = &native["ranges"]["q"];
    assert_eq!(range["shape"], json!([1, 1, 41, 41]));
    assert_eq!(range["axisNames"], json!(["t", "z", "y", "x"]));
    let native_values = range["values"].as_array().unwrap();
    assert!(native_values.iter().all(Value::is_number));

    // Resampled: the positions fall on native nodes, so each value is the
    // native one at the same coordinate.
    let (status, coarse) = get(
        &app,
        &format!("/collections/{ID}/cube?bbox=20,55,30,65&z=150&resolution-x=5&resolution-y=3"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{coarse}");
    assert_valid(&covjson, &coarse, "resampled cube");
    let axes = &coarse["domain"]["axes"];
    assert_eq!(
        numbers(&axes["x"]["values"]),
        [20.0, 22.5, 25.0, 27.5, 30.0]
    );
    assert_eq!(numbers(&axes["y"]["values"]), [55.0, 60.0, 65.0]);
    let values = coarse["ranges"]["q"]["values"].as_array().unwrap();
    assert_eq!(values.len(), 15);
    for (yi, row) in [0, 20, 40].into_iter().enumerate() {
        for (xi, col) in [0, 10, 20, 30, 40].into_iter().enumerate() {
            assert_eq!(values[yi * 5 + xi], native_values[row * 41 + col]);
        }
    }

    // The whole globe at native resolution is over the 1M-value budget; a
    // 1° resampling is not, and its 180° column is the −180° meridian.
    let (status, body) = get(
        &app,
        &format!("/collections/{ID}/cube?bbox=-180,-90,180,90"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["description"]
        .as_str()
        .unwrap()
        .contains("Cube query would return"));
    let (status, globe) = get(
        &app,
        &format!("/collections/{ID}/cube?bbox=-180,-90,180,90&resolution-x=361&resolution-y=181"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{globe}");
    let values = globe["ranges"]["q"]["values"].as_array().unwrap();
    assert_eq!(values.len(), 181 * 361);
    assert!(values.iter().all(Value::is_number));
    for row in 0..181 {
        assert_eq!(values[row * 361], values[row * 361 + 360], "row {row}");
    }

    // GRIB grid subsets do not cross the antimeridian (#667): a clear 400.
    let (status, body) = get(&app, &format!("/collections/{ID}/cube?bbox=170,10,-170,20")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["description"]
        .as_str()
        .unwrap()
        .contains("antimeridian"));

    // The run's instance route answers the same query.
    let (status, instance) = get(
        &app,
        &format!("/collections/{ID}/instances/20260405T0000Z/cube?bbox=20,55,30,65&z=150"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{instance}");
    assert_eq!(instance["ranges"], native["ranges"]);
    // Unavailable level: 400 naming the levels there are.
    let (status, body) = get(
        &app,
        &format!("/collections/{ID}/cube?bbox=20,55,30,65&z=850"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["description"].as_str().unwrap().contains("150"),
        "{body}"
    );
}
