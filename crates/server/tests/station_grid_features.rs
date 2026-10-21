//! A station grid's leave-one-out residuals (#1206) through the Features
//! API, end to end: the real CSV fixture (`testdata/weather.csv`), the
//! station-grid engine over it, and the `/features` router. Values are the
//! engine's independent reference
//! (`crates/engine-station-grid/tests/reference/loo_reference.py`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use api_features::handlers::FeaturesState;
use ds_core::config::{CollectionConfig, StationGridConfig, StationGridGeometry};
use ds_core::edr_engine::EdrEngine;
use ds_core::feature_engine::FeatureEngine;
use engine_station_grid::StationGridEngine;

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn engine() -> Arc<StationGridEngine> {
    let csv = engine_csv::CsvEngine::new(
        engine_csv::CsvDataStore::load(repo().join("testdata/weather.csv").to_str().unwrap())
            .unwrap(),
    );
    let config = StationGridConfig {
        source: "weather".into(),
        parameters: vec!["temperature".into()],
        grid: StationGridGeometry {
            crs: "EPSG:4326".into(),
            bbox: [19.0, 59.0, 32.0, 71.0],
            resolution: 0.25,
        },
        method: "idw".into(),
        power: None,
        correlation_length_km: None,
        max_neighbours: 8,
        search_radius_km: 150.0,
        max_age: "PT1H".into(),
        cadence: Some("PT1H".into()),
        poll_interval_secs: 60,
        precompute: 1,
    };
    Arc::new(
        StationGridEngine::new(
            "t2m-loo",
            "weather",
            Arc::new(csv) as Arc<dyn EdrEngine>,
            &config,
        )
        .unwrap(),
    )
}

fn app(engine: Arc<StationGridEngine>) -> axum::Router {
    let collection: CollectionConfig = serde_json::from_value(serde_json::json!({
        "id": "t2m-loo", "title": "Temperature analysis", "description": "",
        "engine_type": "station-grid", "apis": ["features"],
    }))
    .unwrap();
    api_features::router(Arc::new(ArcSwap::from_pointee(FeaturesState {
        engines: HashMap::from([("t2m-loo".to_string(), engine as Arc<dyn FeatureEngine>)]),
        collections: HashMap::from([("t2m-loo".to_string(), collection)]),
        base_url: "https://example.org".into(),
        trust_proxy_headers: false,
        vector_tileset_ids: Default::default(),
        collection_pages: Default::default(),
    })))
}

/// `(status, Retry-After, body)` of `GET uri`.
async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Option<String>, Value) {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let retry = response
        .headers()
        .get(header::RETRY_AFTER)
        .map(|v| v.to_str().unwrap().to_string());
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&body)
        .unwrap_or_else(|e| panic!("{uri}: {status}: {e}: {}", String::from_utf8_lossy(&body)));
    (status, retry, body)
}

/// `GET uri` until its analyses are computed: a 503 while they are not.
async fn ready(app: &axum::Router, uri: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (status, retry, page) = get(app, uri).await;
        if status == StatusCode::OK {
            return page;
        }
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}: {page}");
        assert_eq!(retry.as_deref(), Some("1"));
        assert!(Instant::now() < deadline, "{uri}: never ready");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn link<'a>(doc: &'a Value, rel: &str) -> Option<&'a str> {
    doc["links"].as_array()?.iter().find(|l| l["rel"] == rel)?["href"].as_str()
}

const ITEMS: &str = "/collections/t2m-loo/items";

#[tokio::test]
async fn residuals_are_served_as_features() {
    let engine = engine();
    let app = app(engine.clone());
    // The poll keeps the latest analysis ready.
    tokio::task::spawn_blocking({
        let engine = engine.clone();
        move || engine.poll_once()
    })
    .await
    .unwrap();

    // No datetime: the latest analysis time, paged by 100.
    let (status, _, latest) = get(&app, ITEMS).await;
    assert_eq!(status, StatusCode::OK, "{latest}");
    let features = latest["features"].as_array().unwrap();
    assert!(!features.is_empty());
    assert!(features
        .iter()
        .all(|f| f["properties"]["time"] == "2026-01-31T23:00:00Z"));
    let properties = &features[0]["properties"];
    for name in ["parameter", "value", "estimate", "residual", "neighbours"] {
        assert!(properties.get(name).is_some(), "{name}: {properties}");
    }
    assert_eq!(features[0]["geometry"]["type"], "Point");

    // A cold analysis time is a 503 at once, served on a retry.
    let at = format!("{ITEMS}?datetime=2026-01-15T12:00:00Z");
    let (status, retry, _) = get(&app, &at).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(retry.as_deref(), Some("1"));
    let page = ready(&app, &at).await;
    assert_eq!(page["numberMatched"], 192);
    assert_eq!(page["numberReturned"], 100);
    assert!(link(&page, "next").unwrap().contains("offset=100"));

    // Paging: the last page has no next link.
    let (_, _, last) = get(&app, &format!("{at}&limit=50&offset=150")).await;
    assert_eq!(last["numberReturned"], 42);
    assert!(link(&last, "next").is_none());
    assert!(link(&last, "prev").unwrap().contains("offset=100"));

    // An interval: every analysis time in it, ends included.
    let three = ready(
        &app,
        &format!("{ITEMS}?datetime=2026-01-15T10:00:00Z/2026-01-15T12:00:00Z&limit=1000"),
    )
    .await;
    let mut times: Vec<&str> = three["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["properties"]["time"].as_str().unwrap())
        .collect();
    times.dedup();
    assert_eq!(
        times,
        [
            "2026-01-15T10:00:00Z",
            "2026-01-15T11:00:00Z",
            "2026-01-15T12:00:00Z"
        ]
    );

    // Over the cap: a 400 naming it, before anything is computed.
    let (status, _, error) = get(
        &app,
        &format!("{ITEMS}?datetime=2026-01-01T00:00:00Z/2026-01-31T23:00:00Z"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let description = error["description"].as_str().unwrap();
    assert!(description.contains("the limit is 96"), "{description}");

    // The largest residual: the reference's station, sorted first.
    let (_, _, worst) = get(&app, &format!("{at}&sortby=-residual&limit=1")).await;
    let worst = &worst["features"][0];
    assert_eq!(
        worst["geometry"]["coordinates"],
        serde_json::json!([20.8509, 69.0428])
    );
    let residual = worst["properties"]["residual"].as_f64().unwrap();
    assert!((residual - 4.099723451918249).abs() < 1e-9, "{residual}");
    assert_eq!(worst["properties"]["value"], -11.9);

    // Its own item, by the link it came with (the router is mounted at
    // `/features` in the server).
    let href = link(worst, "self").unwrap();
    let path = href.strip_prefix("https://example.org/features").unwrap();
    let (status, _, item) = get(&app, path).await;
    assert_eq!(status, StatusCode::OK, "{path}: {item}");
    assert_eq!(
        item["properties"]["residual"],
        worst["properties"]["residual"]
    );

    // Filters and sorting are checked against what the engine offers.
    let (status, _, _) = get(&app, &format!("{at}&bogus=1")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _, error) = get(&app, &format!("{at}&sortby=name")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error["description"].as_str().unwrap().contains("residual"));
    let (_, _, eight) = get(&app, &format!("{at}&neighbours=3&limit=1000")).await;
    assert!(eight["features"]
        .as_array()
        .unwrap()
        .iter()
        .all(|f| f["properties"]["neighbours"] == 3));

    // Collection metadata: a feature collection over the analysis times.
    let (status, _, collection) = get(&app, "/collections/t2m-loo").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(collection["itemType"], "feature");
    let end = collection["extent"]["temporal"]["interval"][0][1]
        .as_str()
        .unwrap();
    assert!(end.starts_with("2026-01-31T23:00:00"), "{end}");
}
