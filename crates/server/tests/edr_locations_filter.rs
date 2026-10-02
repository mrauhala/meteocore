//! `bbox` and `datetime` on the EDR `/locations` list (#932), end to end on
//! the real station engines: CSV (`testdata/weather.csv`, 193 stations,
//! hourly through January 2026, a few starting late or with gaps) and BUFR
//! (`testdata/bufr-synop`, eight SYNOP stations with one report each, seven
//! at 08:00 and Rwanda's at 08:20 on 2026-09-12).
//!
//! `datetime` keeps a station with at least one observation in the
//! interval, each instant of a list matched exactly: the rule their Features
//! `/items` filter applies (#682), from the same engine code. Both filters
//! come before `limit` pages the list. Every 200 validates against the EDR
//! 1.1 and 1.2 bundles.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use ds_core::config::{BufrConfig, CollectionConfig};
use ds_core::edr_engine::EdrEngine;

#[path = "../../api-edr/tests/support/edr_schema.rs"]
mod edr_schema;

/// Stations in `weather.csv`.
const CSV_STATIONS: usize = 193;
/// Its first row is at 12:00 on 1 January.
const KASKINEN: &str = "Kaskinen Sälgrund";
/// Its first row is at 01:00 on 1 January.
const KEMI: &str = "Kemi I majakka";
/// No row at 09:00 on 1 January.
const LEMLAND: &str = "Lemland Nyhamn";
/// The one SYNOP station that reported at 08:20.
const RWANDA: &str = "0-20000-0-64384";

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

/// `GET /collections/{collection}/locations?{query}`: a 200 that validates
/// against both EDR bundles.
async fn locations(app: &axum::Router, collection: &str, query: &str) -> Value {
    let uri = format!("/collections/{collection}/locations?{query}");
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        status,
        StatusCode::OK,
        "{uri}: {}",
        String::from_utf8_lossy(&body)
    );
    let json: Value = serde_json::from_slice(&body).unwrap();
    edr_schema::assert_valid(
        "/collections/{collectionId}/locations",
        edr_schema::GEOJSON,
        &json,
        &uri,
    );
    json
}

fn ids(json: &Value) -> Vec<String> {
    json["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn csv_locations_by_datetime() {
    let app = app();
    let all = ids(&locations(&app, "weather", "").await);
    assert_eq!(all.len(), CSV_STATIONS);

    let count = |json: &Value| ids(json).len();
    let without = |json: &Value, gone: &[&str]| {
        let listed = ids(json);
        gone.iter().all(|id| !listed.iter().any(|l| l == id))
    };

    // An instant matches the stations with a row at exactly that time.
    let json = locations(&app, "weather", "datetime=2026-01-01T00:00:00Z").await;
    assert_eq!(count(&json), CSV_STATIONS - 2);
    assert!(without(&json, &[KASKINEN, KEMI]));
    let json = locations(&app, "weather", "datetime=2026-01-01T09:00:00Z").await;
    assert_eq!(count(&json), CSV_STATIONS - 3);
    assert!(without(&json, &[KASKINEN, KEMI, LEMLAND]));
    // Between two hourly rows: nobody.
    let json = locations(&app, "weather", "datetime=2026-01-01T00:30:00Z").await;
    assert_eq!(count(&json), 0);

    // An interval: any row inside it, both ends included.
    let json = locations(
        &app,
        "weather",
        "datetime=2026-01-01T00:00:00Z/2026-01-01T11:00:00Z",
    )
    .await;
    assert_eq!(count(&json), CSV_STATIONS - 1);
    assert!(without(&json, &[KASKINEN]));
    let json = locations(
        &app,
        "weather",
        "datetime=2026-01-01T00:00:00Z/2026-01-01T12:00:00Z",
    )
    .await;
    assert_eq!(count(&json), CSV_STATIONS);

    // Open ends.
    let json = locations(&app, "weather", "datetime=../2026-01-01T00:59:59Z").await;
    assert_eq!(count(&json), CSV_STATIONS - 2);
    let json = locations(&app, "weather", "datetime=../2026-01-01T01:00:00Z").await;
    assert_eq!(count(&json), CSV_STATIONS - 1);
    assert!(without(&json, &[KASKINEN]));
    let json = locations(&app, "weather", "datetime=2026-01-31T23:00:00Z/..").await;
    assert_eq!(count(&json), CSV_STATIONS);
    let json = locations(&app, "weather", "datetime=2026-02-01T00:00:00Z/..").await;
    assert_eq!(count(&json), 0);

    // A list: each instant exactly, so 00:30 adds nobody and 12:00 everyone.
    let json = locations(
        &app,
        "weather",
        "datetime=2026-01-01T00:30:00Z,2026-01-01T00:00:00Z",
    )
    .await;
    assert_eq!(count(&json), CSV_STATIONS - 2);
    let json = locations(
        &app,
        "weather",
        "datetime=2026-01-01T00:30:00Z,2026-01-01T12:00:00Z",
    )
    .await;
    assert_eq!(count(&json), CSV_STATIONS);
    // Filtering keeps the inventory's order.
    let all_at_noon = ids(&json);
    assert_eq!(all_at_noon, all);
}

/// Both filters, then the page: 37 stations in the box, 35 of them with a
/// row at 09:00 on 1 January (not Kaskinen or Lemland), paged by 20.
#[tokio::test(flavor = "multi_thread")]
async fn csv_bbox_and_datetime_filter_before_paging() {
    let app = app();
    let in_box = locations(&app, "weather", "bbox=19,59,23,64").await;
    assert_eq!(ids(&in_box).len(), 37);
    let filter = "bbox=19,59,23,64&datetime=2026-01-01T09:00:00Z";
    let first = locations(&app, "weather", &format!("{filter}&limit=20")).await;
    assert_eq!(first["numberMatched"], 35);
    assert_eq!(first["numberReturned"], 20);
    let root = "https://example.org/edr/collections/weather/locations";
    let next = first["links"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["rel"] == "next")
        .and_then(|l| l["href"].as_str())
        .unwrap();
    assert_eq!(next, format!("{root}?{filter}&limit=20&offset=20"));
    let second = locations(&app, "weather", next.split_once('?').unwrap().1).await;
    assert_eq!(second["numberMatched"], 35);
    assert_eq!(second["numberReturned"], 15);
    assert!(second["links"]
        .as_array()
        .unwrap()
        .iter()
        .all(|l| l["rel"] != "next"));
    let paged: Vec<String> = ids(&first).into_iter().chain(ids(&second)).collect();
    let expected: Vec<String> = ids(&in_box)
        .into_iter()
        .filter(|id| id != KASKINEN && id != LEMLAND)
        .collect();
    assert_eq!(paged, expected);
}

#[tokio::test(flavor = "multi_thread")]
async fn bufr_locations_by_datetime() {
    let app = app();
    let all = ids(&locations(&app, "synop", "").await);
    assert_eq!(all.len(), 8);
    let at_eight: Vec<String> = all.iter().filter(|id| *id != RWANDA).cloned().collect();

    for (datetime, expected) in [
        ("2026-09-12T08:00:00Z", at_eight.clone()),
        ("2026-09-12T08:20:00Z", vec![RWANDA.to_owned()]),
        // Inside the collection's extent, but nobody reported at 08:10.
        ("2026-09-12T08:10:00Z", Vec::new()),
        ("2026-09-12T08:00:00Z/2026-09-12T08:20:00Z", all.clone()),
        ("2026-09-12T08:01:00Z/2026-09-12T08:19:00Z", Vec::new()),
        ("2026-09-12T08:10:00Z/..", vec![RWANDA.to_owned()]),
        ("../2026-09-12T08:10:00Z", at_eight.clone()),
        ("../..", all.clone()),
        (
            "2026-09-12T08:10:00Z,2026-09-12T08:20:00Z",
            vec![RWANDA.to_owned()],
        ),
        ("2026-09-12T08:20:00Z,2026-09-12T08:00:00Z", all.clone()),
    ] {
        let json = locations(&app, "synop", &format!("datetime={datetime}")).await;
        assert_eq!(ids(&json), expected, "{datetime}");
    }

    // With paging: the counts are the filtered list's.
    let json = locations(&app, "synop", "datetime=../2026-09-12T08:10:00Z&limit=5").await;
    assert_eq!(json["numberMatched"], 7);
    assert_eq!(json["numberReturned"], 5);
    assert_eq!(ids(&json), at_eight[..5]);
}
