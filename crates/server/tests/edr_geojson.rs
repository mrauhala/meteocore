//! EDR GeoJSON (#929) end to end on the real station engines: CSV
//! (`testdata/weather.csv`) and BUFR (`testdata/bufr-synop`, eight SYNOP
//! reports). Every feature must be named by its station — the API layer
//! matches each series to the engine's `get_locations` by exact coordinates,
//! the `serves_station_series` contract — and every body validates against
//! the EDR 1.1 and 1.2 bundles' `application/geo+json` schema of its route.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use ds_core::config::{BufrConfig, CollectionConfig};
use ds_core::edr_engine::EdrEngine;

#[path = "../../api-edr/tests/support/edr_schema.rs"]
mod edr_schema;

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn collection(id: &str, engine_type: &str) -> CollectionConfig {
    CollectionConfig {
        id: id.into(),
        title: id.into(),
        description: String::new(),
        data_path: None,
        apis: vec!["edr".into()],
        engine_type: engine_type.into(),
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
        satellite: None,
        preview: None,
        derive_wind: None,
    }
}

fn app() -> axum::Router {
    let csv = engine_csv::CsvEngine::new(
        engine_csv::CsvDataStore::load(repo().join("testdata/weather.csv").to_str().unwrap())
            .unwrap(),
    );
    let bufr = engine_bufr::BufrEngine::new(
        &BufrConfig {
            data_path: Some(repo().join("testdata/bufr-synop").to_string_lossy().into()),
            wis2: None,
            poll_interval_secs: 60,
            // The fixtures are from 2026-09-12; keep them in the window.
            retention: "P36500D".into(),
            max_stations: 50_000,
            stale_after: "PT2H".into(),
            position_radius_km: 25.0,
            builtin_parameters: true,
            parameters: Vec::new(),
        },
        "synop",
    )
    .unwrap();
    let engines: HashMap<String, Arc<dyn EdrEngine>> = HashMap::from([
        ("weather".to_string(), Arc::new(csv) as Arc<dyn EdrEngine>),
        ("synop".to_string(), Arc::new(bufr) as Arc<dyn EdrEngine>),
    ]);
    let collections = HashMap::from([
        ("weather".to_string(), collection("weather", "csv")),
        ("synop".to_string(), collection("synop", "bufr")),
    ]);
    api_edr::router(Arc::new(ArcSwap::from_pointee(
        api_edr::handlers::EdrState {
            engines,
            collections,
            styles: HashMap::new(),
            feature_engines: HashMap::new(),
            base_url: "https://example.org".into(),
            trust_proxy_headers: false,
        },
    )))
}

async fn geojson(app: &axum::Router, uri: &str) -> Value {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let content_type = resp.headers()[header::CONTENT_TYPE].clone();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        status,
        StatusCode::OK,
        "{uri}: {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(content_type, "application/geo+json", "{uri}");
    serde_json::from_slice(&body).unwrap()
}

/// Validate a GeoJSON body against the `application/geo+json` 200 schema
/// of `path` in both the EDR 1.1 and 1.2 bundles (`edr_schema`). The
/// location data path names its parameter `{locationId}` in 1.2 and
/// `{locId}` in 1.1, so that one path is looked up per version.
fn assert_valid(path: &str, json: &Value) {
    if !path.ends_with("/{locationId}") {
        return edr_schema::assert_valid(path, edr_schema::GEOJSON, json, "GeoJSON");
    }
    for version in edr_schema::VERSIONS {
        let path = match version {
            edr_schema::Edr::V1_1 => path.replace("{locationId}", "{locId}"),
            edr_schema::Edr::V1_2 => path.to_string(),
        };
        let errors = edr_schema::errors(version, &path, edr_schema::GEOJSON, json);
        assert!(
            errors.is_empty(),
            "EDR {version:?} {path}:\n{}\n\nResponse:\n{}",
            errors.join("\n"),
            serde_json::to_string_pretty(json).unwrap()
        );
    }
}

/// Every feature is named by a station of the collection's `/locations`
/// list, and its `edrqueryendpoint` is that station's location resource.
fn assert_named_by_locations(json: &Value, locations: &Value, collection: &str) {
    let listed: HashMap<&str, &Value> = locations["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| (f["id"].as_str().unwrap(), f))
        .collect();
    let features = json["features"].as_array().unwrap();
    assert!(!features.is_empty());
    for f in features {
        let id = f["id"]
            .as_str()
            .unwrap_or_else(|| panic!("an unnamed feature: {f}"));
        let station = listed[id];
        assert_eq!(f["properties"]["label"], station["properties"]["label"]);
        assert_eq!(f["geometry"], station["geometry"]);
        let endpoint = f["properties"]["edrqueryendpoint"].as_str().unwrap();
        assert!(
            endpoint.starts_with(&format!(
                "https://example.org/edr/collections/{collection}/locations/"
            )),
            "{endpoint}"
        );
        // Aligned arrays: one value per instant for every parameter.
        let times = f["properties"]["time"].as_array().unwrap().len();
        for name in f["properties"]["parameter-name"].as_array().unwrap() {
            let values = &f["properties"][name.as_str().unwrap()];
            assert_eq!(values.as_array().unwrap().len(), times, "{id} {name}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn csv_station_series_as_geojson() {
    let app = app();
    let locations = geojson(&app, "/collections/weather/locations").await;

    let radius = geojson(
        &app,
        "/collections/weather/radius?coords=POINT(24.94%2060.17)&within=50&within-units=km&f=GeoJSON",
    )
    .await;
    assert_valid("/collections/{collectionId}/radius", &radius);
    assert_named_by_locations(&radius, &locations, "weather");

    // A station id with a space and non-ASCII letters.
    let one = geojson(
        &app,
        "/collections/weather/locations/Alaj%C3%A4rvi%20M%C3%B6ksy?f=GeoJSON",
    )
    .await;
    assert_valid("/collections/{collectionId}/locations/{locationId}", &one);
    assert_named_by_locations(&one, &locations, "weather");
    assert_eq!(one["features"][0]["id"], "Alajärvi Möksy");
    assert_eq!(
        one["features"][0]["properties"]["edrqueryendpoint"],
        "https://example.org/edr/collections/weather/locations/Alaj%C3%A4rvi%20M%C3%B6ksy"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn bufr_station_series_as_geojson() {
    let app = app();
    let locations = geojson(&app, "/collections/synop/locations").await;

    // Position: the nearest station, SMHI Östergarnsholm.
    let position = geojson(
        &app,
        "/collections/synop/position?coords=POINT(18.98%2057.44)&f=GeoJSON",
    )
    .await;
    assert_valid("/collections/{collectionId}/position", &position);
    assert_named_by_locations(&position, &locations, "synop");
    let f = &position["features"][0];
    assert_eq!(f["id"], "0-20000-0-02598");
    assert_eq!(f["properties"]["label"], "OSTERGARNSHOLM");
    assert_eq!(f["properties"]["air_temperature"][0], 16.97);

    // Radius: the stations within 1000 km, each one named.
    let radius = geojson(
        &app,
        "/collections/synop/radius?coords=POINT(18.98%2057.44)&within=1000&within-units=km&f=GeoJSON",
    )
    .await;
    assert_valid("/collections/{collectionId}/radius", &radius);
    assert_named_by_locations(&radius, &locations, "synop");
}
