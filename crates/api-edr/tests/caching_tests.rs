//! Cache-Control + ETag / If-None-Match behavior (#499).
//!
//! Every 200 must carry an explicit `Cache-Control` (short for metadata and
//! "latest" queries, long for settled past windows) and a strong ETag that
//! answers `If-None-Match` with 304.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use tower::ServiceExt;

use api_edr::handlers::EdrState;
use ds_core::config::CollectionConfig;
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::model::*;

struct MockEngine;

impl MockEngine {
    fn sample_query_result() -> QueryResult {
        let times: Vec<DateTime<Utc>> = (0..3)
            .map(|h| {
                format!("2024-01-01T{h:02}:00:00Z")
                    .parse::<DateTime<Utc>>()
                    .unwrap()
            })
            .collect();
        // Several parameters in fresh-per-query HashMaps: with serde_json's
        // workspace-enabled `preserve_order`, unsorted map serialization
        // would emit a different key order per request — the revalidation
        // tests below would then fail, guarding the sorted-serialization fix.
        let mut parameters = HashMap::new();
        let mut ranges = HashMap::new();
        for name in [
            "temperature",
            "humidity",
            "pressure",
            "wind_speed",
            "dewpoint",
            "visibility",
        ] {
            parameters.insert(
                name.to_string(),
                ParameterDescription {
                    label: name.into(),
                    unit: "degC".into(),
                    observed_property: name.into(),
                },
            );
            ranges.insert(
                name.to_string(),
                NdArray {
                    shape: vec![3],
                    axis_names: vec!["t".into()],
                    values: vec![Some(-2.5), Some(-2.8), None],
                },
            );
        }
        QueryResult {
            domain: DomainDescription::PointSeries {
                x: 24.9384,
                y: 60.1699,
                t: times,
                z: None,
            },
            parameters,
            ranges,
        }
    }
}

impl EdrEngine for MockEngine {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        Ok(vec![Location {
            id: "helsinki".into(),
            label: "Helsinki".into(),
            latitude: 60.1699,
            longitude: 24.9384,
        }])
    }

    fn query_location(
        &self,
        _location_id: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Ok(CoverageResponse::Single(Self::sample_query_result()))
    }

    fn query_position(
        &self,
        _coords: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Ok(CoverageResponse::Single(Self::sample_query_result()))
    }

    fn get_parameters(&self) -> Vec<String> {
        // Six parameters so the /collections metadata path (default
        // `get_parameter_descriptions` → fresh HashMap) also exercises the
        // sorted serialization guarded by the revalidation tests.
        [
            "temperature",
            "humidity",
            "pressure",
            "wind_speed",
            "dewpoint",
            "visibility",
        ]
        .map(String::from)
        .to_vec()
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        Some((
            "2024-01-01T00:00:00Z".parse().unwrap(),
            "2024-01-01T23:00:00Z".parse().unwrap(),
        ))
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([23.7610, 60.1699, 24.9384, 61.4978])
    }

    fn supported_query_types(&self) -> Vec<String> {
        vec!["locations".to_string(), "position".to_string()]
    }
}

fn build_router() -> axum::Router {
    let engine: Arc<dyn EdrEngine> = Arc::new(MockEngine);
    let mut engines: HashMap<String, Arc<dyn EdrEngine>> = HashMap::new();
    let mut collections = HashMap::new();
    engines.insert("weather".to_string(), engine);
    collections.insert(
        "weather".to_string(),
        CollectionConfig {
            id: "weather".to_string(),
            title: "Weather".to_string(),
            description: "Test collection".to_string(),
            data_path: None,
            apis: vec!["edr".to_string()],
            engine_type: "csv".to_string(),
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
            preview: None,
        },
    );
    api_edr::router(Arc::new(ArcSwap::from_pointee(EdrState {
        engines,
        collections,
        base_url: String::new(),
        trust_proxy_headers: false,
    })))
}

async fn get_response(uri: &str, if_none_match: Option<&str>) -> axum::response::Response {
    let mut req = Request::builder().uri(uri);
    if let Some(inm) = if_none_match {
        req = req.header(header::IF_NONE_MATCH, inm);
    }
    build_router()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

fn header_str(resp: &axum::response::Response, name: header::HeaderName) -> Option<&str> {
    resp.headers().get(name).and_then(|v| v.to_str().ok())
}

/// A closed interval comfortably in the past relative to the test run.
const SETTLED_WINDOW: &str = "2024-01-01T00:00:00Z/2024-01-01T06:00:00Z";

#[tokio::test]
async fn metadata_endpoints_carry_short_cache_control_and_etag() {
    for uri in [
        "/",
        "/conformance",
        "/collections",
        "/collections/weather",
        "/collections/weather/locations",
        "/api",
    ] {
        let resp = get_response(uri, None).await;
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
        assert_eq!(
            header_str(&resp, header::CACHE_CONTROL),
            Some("public, max-age=60"),
            "{uri}"
        );
        let etag = header_str(&resp, header::ETAG)
            .expect("etag present")
            .to_owned();
        assert!(
            etag.starts_with('"') && etag.ends_with('"'),
            "{uri}: {etag}"
        );
    }
}

#[tokio::test]
async fn if_none_match_revalidates_to_304() {
    let first = get_response("/collections", None).await;
    let etag = header_str(&first, header::ETAG).unwrap().to_owned();

    let second = get_response("/collections", Some(&etag)).await;
    assert_eq!(second.status(), StatusCode::NOT_MODIFIED);
    // RFC 7232 §4.1: the 304 repeats ETag/Cache-Control (and Vary, which the
    // content-negotiated metadata endpoints set).
    assert_eq!(header_str(&second, header::ETAG), Some(etag.as_str()));
    assert_eq!(
        header_str(&second, header::CACHE_CONTROL),
        Some("public, max-age=60")
    );
    assert_eq!(header_str(&second, header::VARY), Some("accept"));
    let body = second.into_body().collect().await.unwrap().to_bytes();
    assert!(body.is_empty());

    // A non-matching tag still gets the full 200.
    let third = get_response("/collections", Some("\"deadbeefdeadbeef\"")).await;
    assert_eq!(third.status(), StatusCode::OK);
    let body = third.into_body().collect().await.unwrap().to_bytes();
    assert!(!body.is_empty());
}

#[tokio::test]
async fn coverage_query_revalidates_to_304() {
    let uri = format!(
        "/collections/weather/position?coords=POINT(24.94%2060.17)&datetime={SETTLED_WINDOW}"
    );
    let first = get_response(&uri, None).await;
    assert_eq!(first.status(), StatusCode::OK);
    let etag = header_str(&first, header::ETAG).unwrap().to_owned();

    let second = get_response(&uri, Some(&etag)).await;
    assert_eq!(second.status(), StatusCode::NOT_MODIFIED);
    let body = second.into_body().collect().await.unwrap().to_bytes();
    assert!(body.is_empty());
}

#[tokio::test]
async fn settled_past_window_gets_long_cache_control() {
    for uri in [
        format!(
            "/collections/weather/position?coords=POINT(24.94%2060.17)&datetime={SETTLED_WINDOW}"
        ),
        format!("/collections/weather/locations/helsinki?datetime={SETTLED_WINDOW}"),
        // An instant in the past is settled too.
        "/collections/weather/position?coords=POINT(24.94%2060.17)&datetime=2024-01-01T03:00:00Z"
            .to_string(),
    ] {
        let resp = get_response(&uri, None).await;
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
        assert_eq!(
            header_str(&resp, header::CACHE_CONTROL),
            Some("public, max-age=86400"),
            "{uri}"
        );
    }
}

#[tokio::test]
async fn open_latest_or_future_windows_get_short_cache_control() {
    let now_plus_day =
        (Utc::now() + chrono::Duration::days(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    for uri in [
        // No datetime — "latest".
        "/collections/weather/position?coords=POINT(24.94%2060.17)".to_string(),
        // Open start: retention keeps changing the answer.
        "/collections/weather/position?coords=POINT(24.94%2060.17)&datetime=../2024-01-01T06:00:00Z"
            .to_string(),
        // Open end.
        "/collections/weather/position?coords=POINT(24.94%2060.17)&datetime=2024-01-01T00:00:00Z/.."
            .to_string(),
        // Closed window that isn't settled yet.
        format!(
            "/collections/weather/position?coords=POINT(24.94%2060.17)&datetime=2024-01-01T00:00:00Z/{now_plus_day}"
        ),
    ] {
        let resp = get_response(&uri, None).await;
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
        assert_eq!(
            header_str(&resp, header::CACHE_CONTROL),
            Some("public, max-age=60"),
            "{uri}"
        );
    }
}

#[tokio::test]
async fn error_responses_are_left_alone() {
    let resp = get_response("/collections/nope", None).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(resp.headers().get(header::ETAG).is_none());
    assert!(resp.headers().get(header::CACHE_CONTROL).is_none());
}
