//! engine-satellite through the EDR API, end to end on the cropped GOES-19
//! fixtures: the CoverageJSON of position and area queries validates against
//! `schemas/coveragejson.json` (Critical Rule 12), and the collection
//! metadata — each product with its own `extent.temporal` — against the EDR
//! 1.1 schema.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use ds_core::config::{CollectionConfig, SatelliteConfig, SatelliteProductConfig};
use ds_core::edr_engine::EdrEngine;
use engine_satellite::SatelliteEngine;

const C13: &str = "OR_ABI-L2-CMIPF-M6C13_G19_s20262681900199_e20262681909519_c20262681909592.nc";
const ACHT: &str = "OR_ABI-L2-ACHTF-M6_G19_s20262681900199_e20262681909507_c20262681912337.nc";
const C13_LATER: &str =
    "OR_ABI-L2-CMIPF-M6C13_G19_s20262681910199_e20262681919519_c20262681919592.nc";

fn satellite_config(dir: &Path) -> SatelliteConfig {
    let product = |parameter: &str, title: &str, product: &str, band, variable: &str| {
        SatelliteProductConfig {
            parameter: parameter.into(),
            title: title.into(),
            unit: "K".into(),
            product: product.into(),
            band,
            variable: variable.into(),
        }
    };
    SatelliteConfig {
        provider: "goes-r".into(),
        data_path: Some(dir.to_string_lossy().into_owned()),
        endpoint: None,
        bucket: None,
        time_window: None,
        poll_interval_secs: 60,
        composites: Vec::new(),
        products: vec![
            product(
                "ir_10_3",
                "IR 10.3 µm brightness temperature",
                "ABI-L2-CMIPF",
                Some(13),
                "CMI",
            ),
            product(
                "cloud_top_temperature",
                "Cloud top temperature",
                "ABI-L2-ACHTF",
                None,
                "TEMP",
            ),
        ],
    }
}

fn engine() -> (Arc<SatelliteEngine>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/goes19-abi");
    for (from, to) in [(C13, C13), (ACHT, ACHT), (C13, C13_LATER)] {
        std::fs::copy(fixtures.join(from), dir.path().join(to)).unwrap();
    }
    let engine = SatelliteEngine::new("goes19-fd", &satellite_config(dir.path())).unwrap();
    engine.poll_once();
    (Arc::new(engine), dir)
}

fn router(engine: Arc<SatelliteEngine>, dir: &Path) -> axum::Router {
    let config = CollectionConfig {
        id: "goes19-fd".into(),
        title: "GOES-19 full disk".into(),
        description: "Cropped GOES-19 fixtures".into(),
        data_path: None,
        apis: vec!["edr".into()],
        engine_type: "satellite".into(),
        keywords: Vec::new(),
        license: None,
        geotiff: None,
        querydata: None,
        wms: None,
        grib: None,
        zarr: None,
        odim: None,
        cap: None,
        postgis: None,
        nowcast: None,
        bufr: None,
        satellite: Some(satellite_config(dir)),
        preview: None,
    };
    let state = Arc::new(ArcSwap::from_pointee(api_edr::handlers::EdrState {
        engines: HashMap::from([("goes19-fd".to_string(), engine as Arc<dyn EdrEngine>)]),
        collections: HashMap::from([("goes19-fd".to_string(), config)]),
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
    assert!(
        errors.is_empty(),
        "{what}:\n{}\n{}",
        errors.join("\n"),
        serde_json::to_string_pretty(json).unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn satellite_edr_responses_validate() {
    let (engine, dir) = engine();
    let app = router(engine.clone(), dir.path());
    let covjson = schema("coveragejson.json");

    // A point on the disk inside the IR crop (-25.4..3.7°E, 37.1..46.6°N,
    // cut across the north-east limb): the first grid point with a value.
    let mut point = None;
    'search: for j in 0..20 {
        for i in 0..20 {
            let lon = -25.0 + 28.0 * (i as f64 + 0.5) / 20.0;
            let lat = 37.5 + 9.0 * (j as f64 + 0.5) / 20.0;
            let uri = format!(
                "/collections/goes19-fd/position?coords=POINT({lon}%20{lat})&parameter-name=ir_10_3"
            );
            let (status, json) = get(&app, &uri).await;
            if status == StatusCode::OK && json["ranges"]["ir_10_3"]["values"][0].is_number() {
                point = Some((uri, json));
                break 'search;
            }
        }
    }
    let (_, position) = point.expect("a position in the IR crop");
    assert_valid(&covjson, &position, "position CoverageJSON");
    assert_eq!(
        position["domain"]["axes"]["t"]["values"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // An area around the IR crop's centre, both products: the cloud crop
    // lies elsewhere, so its values are null — still valid CoverageJSON.
    let (cx, cy) = (
        position["domain"]["axes"]["x"]["values"][0]
            .as_f64()
            .unwrap(),
        position["domain"]["axes"]["y"]["values"][0]
            .as_f64()
            .unwrap(),
    );
    let d = 0.2;
    let polygon = format!(
        "POLYGON(({w}%20{s},{e}%20{s},{e}%20{n},{w}%20{n},{w}%20{s}))",
        w = cx - d,
        e = cx + d,
        s = cy - d,
        n = cy + d
    );
    let (status, area) = get(
        &app,
        &format!("/collections/goes19-fd/area?coords={polygon}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{area}");
    assert_valid(&covjson, &area, "area CoverageJSON");
    assert_eq!(area["domain"]["domainType"], "Grid");

    // Behind the Earth: 404.
    let (status, _) = get(
        &app,
        "/collections/goes19-fd/position?coords=POINT(100%200)",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Collection metadata: each product's own temporal extent.
    let (status, collection) = get(&app, "/collections/goes19-fd").await;
    assert_eq!(status, StatusCode::OK);
    let names = &collection["parameter_names"];
    assert_eq!(
        names["cloud_top_temperature"]["extent"]["temporal"]["values"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    assert_eq!(
        names["ir_10_3"]["extent"]["temporal"]["values"]
            .as_array()
            .map(Vec::len),
        Some(2)
    );
    let edr = schema("ogcapi-edr-1.1-bundled.json");
    assert_valid(
        &edr["paths"]["/collections/{collectionId}"]["get"]["responses"]["200"]["content"]
            ["application/json"]["schema"],
        &collection,
        "EDR collection",
    );
}

/// RGB composites (#819) are map layers only: EDR lists the products alone,
/// and naming a composite is a 400, since it has no numeric values.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn satellite_edr_leaves_composites_out() {
    let dir = tempfile::tempdir().unwrap();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/goes19-abi");
    for name in [C13, ACHT] {
        std::fs::copy(fixtures.join(name), dir.path().join(name)).unwrap();
    }
    let mut config = satellite_config(dir.path());
    let with_composite: SatelliteConfig = toml::from_str(
        r#"
        products = []

        [[composites]]
        name = "ir_grey"
        red = { parameter = "ir_10_3", min = 330.0, max = 180.0 }
        green = { parameter = "ir_10_3", min = 330.0, max = 180.0 }
        blue = { parameter = "ir_10_3", min = 330.0, max = 180.0 }
        "#,
    )
    .unwrap();
    config.composites = with_composite.composites;
    let engine = SatelliteEngine::new("goes19-fd", &config).unwrap();
    engine.poll_once();
    assert_eq!(ds_core::map_engine::MapEngine::composites(&engine).len(), 1);
    let app = router(Arc::new(engine), dir.path());

    let (status, collection) = get(&app, "/collections/goes19-fd").await;
    assert_eq!(status, StatusCode::OK);
    let mut names: Vec<&str> = collection["parameter_names"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["cloud_top_temperature", "ir_10_3"]);

    let (status, _) = get(
        &app,
        "/collections/goes19-fd/position?coords=POINT(-10%2042)&parameter-name=ir_grey",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
