//! Integration tests for EDR `f=png` plot output.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use tower::ServiceExt;

use api_edr::handlers::EdrState;
use ds_core::config::CollectionConfig;
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::model::*;
use ds_core::vertical::VerticalKind;

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// Engine whose position query returns a VerticalProfile (radar-style).
struct ProfileEngine;
/// Engine whose position query returns a PointSeries (time series).
struct SeriesEngine;

fn profile_result() -> QueryResult {
    let mut parameters = HashMap::new();
    parameters.insert(
        "DBZH".into(),
        ParameterDescription {
            label: "Reflectivity".into(),
            unit: "dBZ".into(),
            observed_property: "DBZH".into(),
        },
    );
    let mut ranges = HashMap::new();
    ranges.insert(
        "DBZH".into(),
        NdArray {
            shape: vec![3],
            axis_names: vec!["z".into()],
            values: vec![Some(10.0), Some(20.0), None],
        },
    );
    QueryResult {
        domain: DomainDescription::VerticalProfile {
            x: 25.0,
            y: 60.0,
            t: Some("2026-05-15T00:00:00Z".parse().unwrap()),
            z: VerticalCoord {
                kind: VerticalKind::ElevationAngle,
                values: vec![0.5, 2.0, 5.0],
            },
        },
        parameters,
        ranges,
    }
}

fn series_result() -> QueryResult {
    let times: Vec<DateTime<Utc>> = (0..3)
        .map(|h| format!("2024-01-01T{h:02}:00:00Z").parse().unwrap())
        .collect();
    let mut parameters = HashMap::new();
    parameters.insert(
        "temperature".into(),
        ParameterDescription {
            label: "Temperature".into(),
            unit: "degC".into(),
            observed_property: "temperature".into(),
        },
    );
    let mut ranges = HashMap::new();
    ranges.insert(
        "temperature".into(),
        NdArray {
            shape: vec![3],
            axis_names: vec!["t".into()],
            values: vec![Some(-2.5), Some(-2.8), None],
        },
    );
    QueryResult {
        domain: DomainDescription::PointSeries {
            x: 24.9,
            y: 60.1,
            t: times,
            z: None,
        },
        parameters,
        ranges,
    }
}

macro_rules! impl_common {
    ($t:ty, $result:expr) => {
        impl EdrEngine for $t {
            fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
                Ok(vec![])
            }
            fn query_location(
                &self,
                _id: &str,
                _dt: Option<(DateTime<Utc>, DateTime<Utc>)>,
                _p: Option<&[String]>,
                _z: Option<&[f64]>,
                _rt: Option<DateTime<Utc>>,
            ) -> Result<CoverageResponse, DataServerError> {
                Ok(CoverageResponse::Single($result))
            }
            fn get_parameters(&self) -> Vec<String> {
                vec![]
            }
            fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
                None
            }
            fn get_spatial_extent(&self) -> Option<[f64; 4]> {
                None
            }
            fn supported_query_types(&self) -> Vec<String> {
                vec!["position".into(), "area".into()]
            }
            fn query_position(
                &self,
                _coords: &str,
                _dt: Option<(DateTime<Utc>, DateTime<Utc>)>,
                _p: Option<&[String]>,
                _z: Option<&[f64]>,
                _rt: Option<DateTime<Utc>>,
            ) -> Result<CoverageResponse, DataServerError> {
                Ok(CoverageResponse::Single($result))
            }
            fn query_area(
                &self,
                _coords: &str,
                _dt: Option<(DateTime<Utc>, DateTime<Utc>)>,
                _p: Option<&[String]>,
                _z: Option<&[f64]>,
                _rt: Option<DateTime<Utc>>,
            ) -> Result<CoverageResponse, DataServerError> {
                Ok(CoverageResponse::Single($result))
            }
        }
    };
}

impl_common!(ProfileEngine, profile_result());
impl_common!(SeriesEngine, series_result());

fn router_with(engine: Arc<dyn EdrEngine>) -> axum::Router {
    let mut engines = HashMap::new();
    let mut collections = HashMap::new();
    engines.insert("c".to_string(), engine);
    collections.insert(
        "c".to_string(),
        CollectionConfig {
            id: "c".to_string(),
            title: "Test".to_string(),
            description: "Test".to_string(),
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

async fn request(engine: Arc<dyn EdrEngine>, uri: &str) -> (StatusCode, String, Vec<u8>) {
    let app = router_with(engine);
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, ct, body)
}

#[tokio::test]
async fn position_profile_png_returns_valid_image() {
    let (status, ct, body) = request(
        Arc::new(ProfileEngine),
        "/collections/c/position?coords=POINT(25%2060)&f=png",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(ct.contains("image/png"), "content-type was {ct}");
    assert_eq!(&body[0..8], &PNG_SIGNATURE, "PNG signature");
    assert!(
        body.len() > 200,
        "non-trivial PNG, got {} bytes",
        body.len()
    );
}

#[tokio::test]
async fn position_series_png_returns_valid_image() {
    let (status, ct, body) = request(
        Arc::new(SeriesEngine),
        "/collections/c/position?coords=POINT(25%2060)&f=png",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(ct.contains("image/png"), "content-type was {ct}");
    assert_eq!(&body[0..8], &PNG_SIGNATURE);
}

#[tokio::test]
async fn format_token_is_case_insensitive() {
    let (status, ct, _) = request(
        Arc::new(ProfileEngine),
        "/collections/c/position?coords=POINT(25%2060)&f=PNG",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(ct.contains("image/png"));
}

#[tokio::test]
async fn default_format_is_still_coveragejson() {
    let (status, ct, _) = request(
        Arc::new(ProfileEngine),
        "/collections/c/position?coords=POINT(25%2060)",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(ct.contains("coverage+json"), "content-type was {ct}");
}

#[tokio::test]
async fn unknown_format_is_400() {
    let (status, _, _) = request(
        Arc::new(ProfileEngine),
        "/collections/c/position?coords=POINT(25%2060)&f=bogus",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn png_on_area_is_400() {
    let (status, _, _) = request(
        Arc::new(SeriesEngine),
        "/collections/c/area?coords=POLYGON((0%200,1%200,1%201,0%201,0%200))&f=png",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn custom_dimensions_are_honored() {
    let (status, _, body) = request(
        Arc::new(ProfileEngine),
        "/collections/c/position?coords=POINT(25%2060)&f=png&width=400&height=300",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // IHDR width/height live at bytes 16..24, big-endian.
    let w = u32::from_be_bytes([body[16], body[17], body[18], body[19]]);
    let h = u32::from_be_bytes([body[20], body[21], body[22], body[23]]);
    assert_eq!((w, h), (400, 300));
}
