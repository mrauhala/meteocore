//! engine-netcdf through the EDR API (#1067), end to end on the committed
//! MEPS sample (`testdata/meps-netcdf`: two runs on MET Norway's Lambert
//! grid), wrapped in `DerivedWind` as the server registers it: every
//! CoverageJSON body validates against `schemas/coveragejson.json`
//! (Critical Rule 12), and the collection and its instances against the EDR
//! 1.1 and 1.2 schemas.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use ds_core::config::CollectionConfig;
use ds_core::edr_engine::EdrEngine;
use ds_core::wind::DerivedWind;
use engine_netcdf::NetcdfEngine;

#[path = "../../api-edr/tests/support/edr_schema.rs"]
mod edr_schema;

const ID: &str = "meps";

fn router() -> axum::Router {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/meps-netcdf");
    let config: CollectionConfig = serde_json::from_value(serde_json::json!({
        "id": ID, "title": "MEPS", "description": "MEPS sample", "engine_type": "netcdf",
        "netcdf": {
            "data_path": dir.to_str().unwrap(),
            "filename_template": "meps_sfc_{lead}_%Y%m%dT{run}Z.nc",
            "variables": ["air_temperature_2m", "x_wind_10m", "y_wind_10m",
                          "air_pressure_at_sea_level"],
        },
    }))
    .unwrap();
    assert_eq!(config.apis, ["edr"], "the default apis");
    let netcdf = Arc::new(NetcdfEngine::new(ID, config.netcdf.as_ref().unwrap()).unwrap());
    let engine = Arc::new(DerivedWind::new(
        ID,
        netcdf,
        Arc::new(|_: &str, _: &ds_core::wind::PairOutcome| {}),
    ));
    let state = Arc::new(ArcSwap::from_pointee(api_edr::handlers::EdrState {
        engines: HashMap::from([(ID.to_string(), engine as Arc<dyn EdrEngine>)]),
        feature_engines: HashMap::new(),
        collections: HashMap::from([(ID.to_string(), config)]),
        styles: HashMap::new(),
        base_url: String::new(),
        trust_proxy_headers: false,
    }));
    api_edr::router(state)
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

fn coverage_json_schema() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/coveragejson.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn assert_valid(schema: &Value, json: &Value, what: &str) {
    let validator = jsonschema::Validator::new(schema).unwrap();
    let errors: Vec<String> = validator
        .iter_errors(json)
        .map(|e| format!("- {e} (at {})", e.instance_path()))
        .collect();
    assert!(
        errors.is_empty(),
        "{what}:\n{}\n{}",
        errors.join("\n"),
        serde_json::to_string_pretty(json).unwrap()
    );
}

fn times(json: &Value) -> Vec<String> {
    json["domain"]["axes"]["t"]["values"]
        .as_array()
        .map(|t| t.iter().map(|v| v.as_str().unwrap().to_string()).collect())
        .unwrap_or_default()
}

/// Built outside any runtime, as the server builds engines, and served by
/// a multi-thread runtime: the EDR executor runs each query on the shared
/// query runtime, where the field jobs `block_in_place`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn netcdf_edr_responses_validate() {
    let app = tokio::task::spawn_blocking(router).await.unwrap();
    let covjson = coverage_json_schema();
    let base = format!("/collections/{ID}");

    // Every query shape, each valid CoverageJSON.
    let queries = [
        ("position series", "position?coords=POINT(24.94%2060.17)&parameter-name=air_temperature_2m,wind_speed_10m"),
        ("position instant", "position?coords=POINT(10.75%2059.91)&datetime=2026-10-15T08:00:00Z"),
        ("multipoint", "position?coords=MULTIPOINT((24.94%2060.17),(10.75%2059.91))&datetime=2026-10-15T07:00:00Z/2026-10-15T09:00:00Z&parameter-name=air_temperature_2m"),
        ("datetime list", "position?coords=POINT(24.94%2060.17)&datetime=2026-10-15T07:00:00Z,2026-10-15T07:30:00Z,2026-10-15T10:00:00Z&parameter-name=wind_speed_10m"),
        ("area", "area?coords=POLYGON((5%2055,30%2055,5%2068,5%2055))&parameter-name=air_pressure_at_sea_level"),
        ("area interval", "area?coords=10,58,20,64&datetime=2026-10-15T07:00:00Z/2026-10-15T08:00:00Z&parameter-name=air_temperature_2m,wind_speed_10m"),
        ("radius", "radius?coords=POINT(15%2063)&within=400&within-units=km&parameter-name=wind_speed_10m"),
        ("instance position", "instances/2026-10-15T06:00:00Z/position?coords=POINT(24.94%2060.17)&parameter-name=air_temperature_2m"),
        ("instance area", "instances/2026-10-15T06:00:00Z/area?coords=10,58,20,64&datetime=2026-10-15T09:00:00Z"),
    ];
    let mut answers = HashMap::new();
    for (what, query) in queries {
        let (status, json) = get(&app, &format!("{base}/{query}")).await;
        assert_eq!(status, StatusCode::OK, "{what}: {json}");
        assert_valid(&covjson, &json, what);
        answers.insert(what, json);
    }
    assert_eq!(
        times(&answers["position series"]),
        [
            "2026-10-15T09:00:00+00:00",
            "2026-10-15T10:00:00+00:00",
            "2026-10-15T11:00:00+00:00"
        ],
        "the newest run's steps"
    );
    let instant = &answers["position instant"];
    assert_eq!(times(instant), ["2026-10-15T08:00:00+00:00"]);
    assert!(instant["ranges"]["wind_speed_10m"]["values"][0].is_number());
    assert!(instant["ranges"]["air_temperature_2m"]["values"][0].is_number());
    // The instant without a step contributes nothing to the list.
    assert_eq!(
        times(&answers["datetime list"]),
        ["2026-10-15T07:00:00+00:00", "2026-10-15T10:00:00+00:00"]
    );
    assert_eq!(answers["multipoint"]["type"], "CoverageCollection");
    let area = &answers["area interval"];
    assert_eq!(area["domain"]["domainType"], "Grid");
    assert_eq!(times(area).len(), 2);
    assert_eq!(
        area["ranges"]["wind_speed_10m"]["axisNames"],
        serde_json::json!(["t", "y", "x"])
    );
    assert_eq!(
        times(&answers["instance position"]).len(),
        4,
        "the 06Z run's four steps"
    );
    let unit = &answers["position series"]["parameters"]["air_temperature_2m"];
    assert_eq!(
        unit["observedProperty"]["id"],
        "https://vocab.nerc.ac.uk/standard_name/air_temperature"
    );

    // No step at the instant, or outside the grid's extent: 404.
    for query in [
        "position?coords=POINT(24.94%2060.17)&datetime=2026-10-15T08:30:00Z",
        "area?coords=100,0,110,10",
        "instances/2026-10-15T09:00:00Z/position?coords=POINT(24.94%2060.17)&datetime=2026-10-15T08:00:00Z",
        "instances/2026-10-15T03:00:00Z/position?coords=POINT(24.94%2060.17)",
    ] {
        let (status, json) = get(&app, &format!("{base}/{query}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{query}: {json}");
    }
    // Not served: trajectory and cube.
    for query in [
        "trajectory?coords=LINESTRING(10%2060,20%2062)",
        "cube?bbox=10,58,20,64",
    ] {
        let (status, _) = get(&app, &format!("{base}/{query}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{query}");
    }

    let (status, collection) = get(&app, &base).await;
    assert_eq!(status, StatusCode::OK);
    let queries: Vec<&str> = collection["data_queries"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(queries, ["position", "area", "radius", "instances"]);
    assert!(collection["parameter_names"]["wind_speed_10m"].is_object());
    edr_schema::assert_valid(
        "/collections/{collectionId}",
        edr_schema::JSON,
        &collection,
        "EDR collection",
    );
    let (status, instances) = get(&app, &format!("{base}/instances")).await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = instances["instances"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["2026-10-15T09:00:00Z", "2026-10-15T06:00:00Z"]);
    edr_schema::assert_valid(
        "/collections/{collectionId}/instances",
        edr_schema::JSON,
        &instances,
        "EDR instances",
    );
}
