//! EDR GeoJSON (#929) end to end on the real station engines: CSV
//! (`testdata/weather.csv`) and BUFR (`testdata/bufr-synop`, eight SYNOP
//! reports). Every feature must be named by its station — the API layer
//! matches each series to the engine's `get_locations` by exact coordinates,
//! the `serves_station_series` contract — and every body validates against
//! the EDR 1.1 and 1.2 bundles' `application/geo+json` schema of its route.
//! The items query (#970) and `/locations` are EDR GeoJSON too: every
//! station item is its location, and a nowcast cell (over a synthetic
//! translating echo) names a radius query over the motion field; each
//! `edrqueryendpoint` answers.

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
use ds_core::feature_engine::FeatureEngine;

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
    let (csv, bufr) = (Arc::new(csv), Arc::new(bufr));
    let engines: HashMap<String, Arc<dyn EdrEngine>> = HashMap::from([
        ("weather".to_string(), csv.clone() as Arc<dyn EdrEngine>),
        ("synop".to_string(), bufr.clone() as Arc<dyn EdrEngine>),
    ]);
    // Both engines serve their stations as items too, as `admin.rs` wires
    // every EDR collection whose engine implements `FeatureEngine`.
    let feature_engines: HashMap<String, Arc<dyn FeatureEngine>> = HashMap::from([
        ("weather".to_string(), csv as Arc<dyn FeatureEngine>),
        ("synop".to_string(), bufr as Arc<dyn FeatureEngine>),
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
            feature_engines,
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

/// `/req/edr-geojson/content` A on `/items`, `/items/{itemId}` and
/// `/locations`: EDR GeoJSON whose every feature is a station of the
/// collection's locations, carrying the same members as its `/locations`
/// feature, with an `edrqueryendpoint` that is a valid URI and answers.
#[tokio::test(flavor = "multi_thread")]
async fn items_and_locations_are_edr_geojson_of_the_stations() {
    let app = app();
    for collection in ["weather", "synop"] {
        let locations = geojson(&app, &format!("/collections/{collection}/locations")).await;
        assert_valid("/collections/{collectionId}/locations", &locations);
        let listed: HashMap<&str, &Value> = locations["features"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| (f["id"].as_str().unwrap(), f))
            .collect();

        let items = geojson(
            &app,
            &format!("/collections/{collection}/items?limit=10000"),
        )
        .await;
        assert_valid("/collections/{collectionId}/items", &items);
        let features = items["features"].as_array().unwrap();
        assert_eq!(features.len(), listed.len(), "{collection}");
        for item in features {
            let id = item["id"].as_str().unwrap();
            let location = listed[id];
            assert_eq!(item["geometry"], location["geometry"], "{id}");
            for member in ["datetime", "parameter-name", "label", "edrqueryendpoint"] {
                assert_eq!(
                    item["properties"][member], location["properties"][member],
                    "{collection} {id} {member}"
                );
            }
            let endpoint = item["properties"]["edrqueryendpoint"].as_str().unwrap();
            assert!(
                endpoint.bytes().all(|b| b.is_ascii_graphic()),
                "not a URI: {endpoint}"
            );
            assert_eq!(location["links"][0]["href"], endpoint, "{id}");
        }

        // One item, the first station: a Feature with the same members, and
        // its endpoint is the station's working location query.
        let first = &features[0];
        let id = first["id"].as_str().unwrap();
        let encoded = first["links"][0]["href"]
            .as_str()
            .unwrap()
            .rsplit('/')
            .next()
            .unwrap()
            .to_owned();
        let item = geojson(&app, &format!("/collections/{collection}/items/{encoded}")).await;
        assert_valid(edr_schema::ITEM, &item);
        assert_eq!(item["id"], id);
        assert_eq!(item["properties"], first["properties"]);
        let endpoint = first["properties"]["edrqueryendpoint"].as_str().unwrap();
        let query = endpoint.strip_prefix("https://example.org/edr").unwrap();
        let series = geojson(&app, &format!("{query}?f=GeoJSON")).await;
        assert_eq!(series["features"][0]["id"], id, "{endpoint}");
    }

    // A CSV station id with a space and non-ASCII letters: percent-encoded
    // in its links, the text in its id and label.
    let item = geojson(
        &app,
        "/collections/weather/items/Alaj%C3%A4rvi%20M%C3%B6ksy",
    )
    .await;
    assert_eq!(item["properties"]["label"], "Alajärvi Möksy");
    assert_eq!(
        item["properties"]["edrqueryendpoint"],
        "https://example.org/edr/collections/weather/locations/Alaj%C3%A4rvi%20M%C3%B6ksy"
    );
}

/// Synthetic radar source for the nowcast: a 15 px disc of ~40 dBZ on a
/// 200 × 200 grid over 0–10°E, 50–60°N, moving 2 px east per 5 minutes.
struct DiscSource {
    times: Vec<chrono::DateTime<chrono::Utc>>,
}

impl ds_core::map_engine::MapEngine for DiscSource {
    fn get_raster_tile(
        &self,
        _bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<chrono::DateTime<chrono::Utc>>,
        _output_crs: &ds_core::map_engine::OutputCrs,
        _parameter: Option<&str>,
        _z: Option<f64>,
        _reference_time: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<ds_core::map_engine::RasterTile, ds_core::error::DataServerError> {
        let minutes = (time.unwrap() - self.times[0]).num_minutes() as f64;
        let (cx, cy) = (60.0 + 2.0 * minutes / 5.0, 100.0);
        let data = (0..width * height)
            .map(|i| {
                let x = f64::from(i % width) + 0.5;
                let y = f64::from(i / width) + 0.5;
                if (x - cx).powi(2) + (y - cy).powi(2) <= 225.0 {
                    175
                } else {
                    0
                }
            })
            .collect();
        Ok(ds_core::map_engine::RasterTile {
            width,
            height,
            values: ds_core::map_engine::RasterValues::U8 {
                data,
                nodata: Some(255),
                gain: 0.4,
                offset: -30.0,
            },
        })
    }

    fn raster_info(&self) -> ds_core::map_engine::RasterInfo {
        ds_core::map_engine::RasterInfo {
            native_crs: "CRS:84".into(),
            spatial_extent: Some([0.0, 50.0, 10.0, 60.0]),
            times: self.times.clone(),
            parameter: "reflectivity".into(),
            unit: "dBZ".into(),
            parameters: vec![],
            vertical: None,
            grid_size: Some([200, 200]),
            layer_subtitle: None,
            reference_times: Vec::new(),
        }
    }
}

/// A tracked nowcast cell as an EDR item (#970): no location, so its
/// `edrqueryendpoint` is the radius query over the motion field centred on
/// the cell, as wide as the cell, and its `datetime` the frame it was
/// observed in. Following the endpoint answers the motion vectors.
#[tokio::test(flavor = "multi_thread")]
async fn a_nowcast_cell_item_names_a_radius_query_that_answers() {
    let t0: chrono::DateTime<chrono::Utc> = "2026-07-20T12:00:00Z".parse().unwrap();
    let source = Arc::new(DiscSource {
        times: vec![t0, t0 + chrono::Duration::minutes(5)],
    });
    let config = ds_core::config::NowcastConfig {
        source: "radar".into(),
        horizon: "PT30M".into(),
        step: None,
        history_frames: 2,
        poll_interval_secs: 30,
        max_generations: 2,
        max_pixels: 4_000_000,
        min_echo: 10.0,
        growth_decay: false,
        lightning_source: None,
        significance: Default::default(),
        impact_source: None,
        impact_name_property: "name".into(),
        impact_weight_property: None,
        radar_source: None,
    };
    let nowcast =
        Arc::new(engine_nowcast::NowcastEngine::new("cells", "radar", source, &config).unwrap());
    nowcast.poll_once();
    let app = api_edr::router(Arc::new(ArcSwap::from_pointee(
        api_edr::handlers::EdrState {
            engines: HashMap::from([("cells".to_string(), nowcast.clone() as Arc<dyn EdrEngine>)]),
            feature_engines: HashMap::from([(
                "cells".to_string(),
                nowcast as Arc<dyn FeatureEngine>,
            )]),
            collections: HashMap::from([("cells".to_string(), collection("cells", "nowcast"))]),
            styles: HashMap::new(),
            base_url: "https://example.org".into(),
            trust_proxy_headers: false,
        },
    )));

    let items = geojson(&app, "/collections/cells/items").await;
    assert_valid("/collections/{collectionId}/items", &items);
    let cell = &items["features"][0];
    let id = cell["id"].as_str().unwrap();
    let properties = &cell["properties"];
    assert_eq!(properties["label"], id);
    assert_eq!(properties["datetime"], properties["observed"]);
    assert_eq!(
        properties["parameter-name"],
        serde_json::json!(["motion_u", "motion_v", "motion_quality"])
    );
    let endpoint = properties["edrqueryendpoint"].as_str().unwrap();
    let [x, y] = [0, 1].map(|i| cell["geometry"]["coordinates"][i].as_f64().unwrap());
    let within = ((properties["area_km2"].as_f64().unwrap() / std::f64::consts::PI).sqrt() * 10.0)
        .ceil()
        / 10.0;
    assert_eq!(
        endpoint,
        format!(
            "https://example.org/edr/collections/cells/radius?coords=POINT({x}%20{y})&within={within}&within-units=km"
        )
    );
    let item = geojson(&app, &format!("/collections/cells/items/{id}")).await;
    assert_valid(edr_schema::ITEM, &item);
    assert_eq!(item["properties"], *properties);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(endpoint.strip_prefix("https://example.org/edr").unwrap())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{endpoint}");
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let coverage: Value = serde_json::from_slice(&body).unwrap();
    let u = coverage["ranges"]["motion_u"]["values"].as_array().unwrap();
    assert!(u.iter().any(|v| v.is_number()), "{coverage}");
}
