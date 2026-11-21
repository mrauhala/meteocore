//! A `datetime` list on a remote GeoTIFF collection through the EDR API
//! (#1522). Each instant reads one frame, a round trip on a remote source,
//! so a list answering its instants in turn paid one round trip per instant
//! against the query's deadline. Against a store whose every read takes a
//! set time, a list of eight instants now keeps
//! `api_edr::INSTANT_CONCURRENCY` reads in flight (fewer on a host whose
//! EDR admits fewer queries: each instant past the first takes a free query
//! slot) and answers in about eight ÷ that many delays, with the body and
//! ETag a query of the same frames as one window gives.
//!
//! In a test binary of its own: every EDR query slot of the process is
//! free, so the list gets all it wants.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use tower::ServiceExt;

use ds_core::config::{CollectionConfig, GeoTiffConfig};
use ds_core::edr_engine::EdrEngine;
use ds_storage::test_util::SlowReads;
use ds_storage::DataStore;
use engine_geotiff::GeoTiffEngine;

const ID: &str = "radar";
/// Instants of the list, one frame each.
const FRAMES: usize = 8;
/// A point of the TM35FIN fixture's middle tile holding data (95), from
/// `gdallocationinfo -wgs84`.
const POINT: &str = "POINT(25.4%2064.64)";

/// The `i`th frame's time, five minutes apart from 06:40.
fn time_of(i: usize) -> DateTime<Utc> {
    "2026-04-06T06:40:00Z".parse::<DateTime<Utc>>().unwrap()
        + chrono::Duration::minutes(5 * i as i64)
}

/// An EDR serving a GeoTIFF collection of [`FRAMES`] frames of the TM35FIN
/// fixture (256 × 256 byte tiles: one small range read per frame) in
/// `store`, catalogued once while the store answers at once.
fn router(store: &Arc<SlowReads>) -> axum::Router {
    let bytes = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/radar-tm35fin/radar_tm35_20260406T0640Z.tif"),
    )
    .unwrap();
    for i in 0..FRAMES {
        let key = format!("d/radar_tm35_{}.tif", time_of(i).format("%Y%m%dT%H%MZ"));
        store.put(&key, bytes.clone());
    }
    let geotiff: GeoTiffConfig = serde_json::from_value(serde_json::json!({
        "filename_template": "radar_tm35_%Y%m%dT%H%MZ.tif",
        "parameter": "reflectivity",
        "unit": "dBZ",
    }))
    .unwrap();
    let engine =
        GeoTiffEngine::remote_for_test(ID, &geotiff, DataStore::new(store.clone()), "d").unwrap();
    let config: CollectionConfig = serde_json::from_value(serde_json::json!({
        "id": ID, "title": "Radar", "description": "Remote GeoTIFF frames",
        "engine_type": "geotiff",
    }))
    .unwrap();
    let state = Arc::new(ArcSwap::from_pointee(api_edr::handlers::EdrState {
        engines: HashMap::from([(ID.to_string(), Arc::new(engine) as Arc<dyn EdrEngine>)]),
        feature_engines: HashMap::new(),
        collections: HashMap::from([(ID.to_string(), config)]),
        discovery: Default::default(),
        styles: HashMap::new(),
        base_url: String::new(),
        trust_proxy_headers: false,
    }));
    api_edr::router(state)
}

/// Status, ETag and body.
async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Option<String>, Bytes) {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let etag = resp
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    (
        status,
        etag,
        resp.into_body().collect().await.unwrap().to_bytes(),
    )
}

/// The acceptance case of #1522: eight instants, every read taking
/// 250 ms, read four at a time (or as many as EDR admits beside the
/// list's own query, at least two) and answered in about two delays (four
/// at two at a time), well under the eight of one instant after another;
/// the answer is the window query's, byte for byte.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_list_on_a_remote_geotiff_reads_its_instants_side_by_side() {
    let store = Arc::new(SlowReads::default());
    let app = {
        let store = store.clone();
        tokio::task::spawn_blocking(move || router(&store))
            .await
            .unwrap()
    };
    let delay = Duration::from_millis(250);
    store.reset();
    store.set_delay(delay);

    let list = (0..FRAMES)
        .map(|i| time_of(i).format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .collect::<Vec<_>>()
        .join(",");
    let started = Instant::now();
    let (status, etag, body) = get(
        &app,
        &format!("/collections/{ID}/position?coords={POINT}&datetime={list}"),
    )
    .await;
    let elapsed = started.elapsed();
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));

    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let times = json["domain"]["axes"]["t"]["values"].as_array().unwrap();
    assert_eq!(times.len(), FRAMES, "{json}");
    let values = json["ranges"]["reflectivity"]["values"].as_array().unwrap();
    assert!(
        values.iter().all(|v| v == &values[0] && !v.is_null()),
        "{values:?}"
    );

    let bound = api_edr::INSTANT_CONCURRENCY.min(ds_executor::query_workers());
    assert!(bound >= 2);
    assert_eq!(store.reads(), FRAMES, "one tile read per instant");
    assert_eq!(store.peak(), bound, "instants in flight");
    let rounds = FRAMES.div_ceil(bound) as u32;
    assert!(
        elapsed >= delay * rounds && elapsed < delay * FRAMES as u32 * 3 / 4,
        "{elapsed:?} for {FRAMES} instants of {delay:?}, {bound} at a time"
    );

    // The window over the same frames answers the same series: the same
    // body and ETag.
    let window = format!(
        "{}/{}",
        time_of(0).format("%Y-%m-%dT%H:%M:%SZ"),
        time_of(FRAMES - 1).format("%Y-%m-%dT%H:%M:%SZ")
    );
    let (status, window_etag, window_body) = get(
        &app,
        &format!("/collections/{ID}/position?coords={POINT}&datetime={window}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(window_body, body, "the list's body is the window's");
    assert_eq!(window_etag, etag);
    assert!(etag.is_some());
}
