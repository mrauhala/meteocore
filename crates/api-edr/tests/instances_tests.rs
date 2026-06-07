//! OGC API - EDR instances (forecast model runs; #337).
//!
//! Exercises the instances endpoints against a mock forecast engine exposing two
//! runs (00Z, 12Z). The mock encodes the selected run's hour into every value so
//! a query can prove which run it hit (None ⇒ latest run = 12Z).

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, TimeZone, Timelike, Utc};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::util::ServiceExt;

use api_edr::handlers::EdrState;
use ds_core::config::CollectionConfig;
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::instances::RunInfo;
use ds_core::model::*;

fn dt(h: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 7, h, 0, 0).unwrap()
}

const LATEST_RUN_HOUR: u32 = 12;

struct ForecastMock;

impl ForecastMock {
    fn run_times() -> Vec<DateTime<Utc>> {
        vec![dt(0), dt(LATEST_RUN_HOUR)] // 00Z, 12Z (latest)
    }
}

impl EdrEngine for ForecastMock {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        Ok(vec![])
    }

    fn get_instances(&self) -> Vec<RunInfo> {
        Self::run_times()
            .into_iter()
            .map(|rt| RunInfo {
                reference_time: rt,
                valid_times: (0..3).map(|h| rt + chrono::Duration::hours(h)).collect(),
            })
            .collect()
    }

    fn query_location(
        &self,
        _location_id: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Err(DataServerError::InvalidParameter("no locations".into()))
    }

    fn get_parameters(&self) -> Vec<String> {
        vec!["temperature".to_string()]
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        // Latest run's valid-time span.
        Some((
            dt(LATEST_RUN_HOUR),
            dt(LATEST_RUN_HOUR) + chrono::Duration::hours(2),
        ))
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([-180.0, -90.0, 180.0, 90.0])
    }

    fn supported_query_types(&self) -> Vec<String> {
        vec!["position".to_string()]
    }

    fn query_position(
        &self,
        _coords: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        // None ⇒ latest run; encode the selected run's hour into every value.
        let rt = reference_time.unwrap_or_else(|| dt(LATEST_RUN_HOUR));
        let marker = rt.hour() as f64;
        let times: Vec<DateTime<Utc>> = (0..3).map(|h| rt + chrono::Duration::hours(h)).collect();
        let mut parameters = HashMap::new();
        parameters.insert(
            "temperature".to_string(),
            ParameterDescription {
                label: "temperature".to_string(),
                unit: "degC".to_string(),
                observed_property: "temperature".to_string(),
            },
        );
        let mut ranges = HashMap::new();
        ranges.insert(
            "temperature".to_string(),
            NdArray {
                shape: vec![3],
                axis_names: vec!["t".to_string()],
                values: vec![Some(marker); 3],
            },
        );
        Ok(CoverageResponse::Single(QueryResult {
            domain: DomainDescription::PointSeries {
                x: 25.0,
                y: 60.0,
                t: times,
                z: None,
            },
            parameters,
            ranges,
        }))
    }
}

fn state() -> api_edr::handlers::AppState {
    let mut engines: HashMap<String, Arc<dyn EdrEngine>> = HashMap::new();
    let mut collections = HashMap::new();
    engines.insert("fc".to_string(), Arc::new(ForecastMock));
    collections.insert(
        "fc".to_string(),
        CollectionConfig {
            id: "fc".to_string(),
            title: "Forecast".to_string(),
            description: "Mock forecast".to_string(),
            data_path: None,
            apis: vec!["edr".to_string()],
            engine_type: "grib".to_string(),
            keywords: Vec::new(),
            license: None,
            geotiff: None,
            querydata: None,
            wms: None,
            grib: None,
            zarr: None,
            odim: None,
            postgis: None,
            preview: None,
        },
    );
    Arc::new(ArcSwap::from_pointee(EdrState {
        engines,
        collections,
        base_url: String::new(),
    }))
}

async fn get(uri: &str) -> (StatusCode, Value) {
    let app = api_edr::router(state());
    let resp = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    (status, json)
}

#[tokio::test]
async fn instances_list_has_both_runs() {
    let (status, body) = get("/collections/fc/instances").await;
    assert_eq!(status, StatusCode::OK);
    let collections = body["collections"].as_array().unwrap();
    assert_eq!(collections.len(), 2);
    let ids: Vec<&str> = collections
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"20260607T0000Z"), "ids: {ids:?}");
    assert!(ids.contains(&"20260607T1200Z"), "ids: {ids:?}");
}

#[tokio::test]
async fn instance_metadata_scopes_extent_and_links() {
    let (status, body) = get("/collections/fc/instances/20260607T0000Z").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], "20260607T0000Z");
    // Temporal extent is the 00Z run's valid times (00:00..02:00), NOT the
    // collection's latest-run (12Z) extent.
    let interval = &body["extent"]["temporal"]["interval"][0];
    assert_eq!(interval[0], "2026-06-07T00:00:00+00:00");
    assert_eq!(interval[1], "2026-06-07T02:00:00+00:00");
    // Data-query hrefs are instance-scoped.
    let pos = body["data_queries"]["position"]["link"]["href"]
        .as_str()
        .unwrap();
    assert!(
        pos.ends_with("/collections/fc/instances/20260607T0000Z/position"),
        "{pos}"
    );
}

#[tokio::test]
async fn instance_position_query_hits_the_pinned_run() {
    // Pinned 00Z run → marker 0.
    let (status, body) =
        get("/collections/fc/instances/20260607T0000Z/position?coords=POINT(25%2060)").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ranges"]["temperature"]["values"][0], 0.0);

    // No instance → latest run (12Z) → marker 12.
    let (status, body) = get("/collections/fc/position?coords=POINT(25%2060)").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ranges"]["temperature"]["values"][0], 12.0);
}

#[tokio::test]
async fn unknown_instance_is_404_bad_id_is_400() {
    // A well-formed but absent run → 404 (both metadata and query).
    assert_eq!(
        get("/collections/fc/instances/20260607T0600Z").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get("/collections/fc/instances/20260607T0600Z/position?coords=POINT(25%2060)")
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    // An unparseable instance id → 400.
    assert_eq!(
        get("/collections/fc/instances/not-a-time").await.0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn collection_advertises_instances_data_query() {
    let (status, body) = get("/collections/fc").await;
    assert_eq!(status, StatusCode::OK);
    let href = body["data_queries"]["instances"]["link"]["href"]
        .as_str()
        .expect("instances data_query present");
    assert!(href.ends_with("/collections/fc/instances"), "{href}");
}
