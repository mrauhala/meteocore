//! CoverageJSON media type (#920).
//!
//! OGC API - EDR 1.2 serves CoverageJSON as `application/vnd.cov+json`
//! (`/req/covjson/definition`); 1.1 used `application/prs.coverage+json`.
//! Every data route must send the 1.2 type, `/api` and the `/locations`
//! data links must document it, and a client still asking for the 1.1 type
//! — in `f` or in `Accept` — must keep getting CoverageJSON.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use chrono::{DateTime, TimeZone, Utc};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::util::ServiceExt;

use api_edr::handlers::EdrState;
use ds_core::config::CollectionConfig;
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::instances::RunInfo;
use ds_core::model::*;

const COVJSON: &str = "application/vnd.cov+json";
const LEGACY_COVJSON: &str = "application/prs.coverage+json";

fn run_time() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 7, 0, 0, 0).unwrap()
}

/// Answers every query type (and one model run) with the same PointSeries:
/// the media type is the handlers' business, not the domain type's.
struct EveryQueryEngine;

fn point_series() -> CoverageResponse {
    let t: Vec<DateTime<Utc>> = (0..2)
        .map(|h| run_time() + chrono::Duration::hours(h))
        .collect();
    let mut parameters = HashMap::new();
    parameters.insert(
        "temperature".to_string(),
        ParameterDescription {
            label: "temperature".to_string(),
            unit: "degC".to_string(),
            observed_property: "temperature".to_string(),
            standard_name: None,
        },
    );
    let mut ranges = HashMap::new();
    ranges.insert(
        "temperature".to_string(),
        NdArray {
            shape: vec![2],
            axis_names: vec!["t".to_string()],
            values: vec![Some(1.5), Some(2.5)],
        },
    );
    CoverageResponse::Single(QueryResult {
        domain: DomainDescription::PointSeries {
            x: 25.0,
            y: 60.0,
            t,
            z: None,
        },
        parameters,
        ranges,
    })
}

impl EdrEngine for EveryQueryEngine {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        Ok(vec![Location {
            id: "here".to_string(),
            label: "Here".to_string(),
            latitude: 60.0,
            longitude: 25.0,
        }])
    }

    fn get_instances(&self) -> Vec<RunInfo> {
        vec![RunInfo {
            reference_time: run_time(),
            valid_times: vec![run_time(), run_time() + chrono::Duration::hours(1)],
        }]
    }

    fn query_location(
        &self,
        _: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Ok(point_series())
    }

    fn query_position(
        &self,
        _: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Ok(point_series())
    }

    fn query_area(
        &self,
        _: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Ok(point_series())
    }

    fn query_trajectory(
        &self,
        _: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Ok(point_series())
    }

    fn get_parameters(&self) -> Vec<String> {
        vec!["temperature".to_string()]
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        Some((run_time(), run_time() + chrono::Duration::hours(1)))
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([20.0, 58.0, 30.0, 62.0])
    }

    fn supported_query_types(&self) -> Vec<String> {
        ["locations", "position", "area", "radius", "trajectory"]
            .map(String::from)
            .to_vec()
    }
}

fn router() -> axum::Router {
    let mut engines: HashMap<String, Arc<dyn EdrEngine>> = HashMap::new();
    engines.insert("c".to_string(), Arc::new(EveryQueryEngine));
    let mut collections = HashMap::new();
    collections.insert(
        "c".to_string(),
        CollectionConfig {
            id: "c".to_string(),
            title: "Every query".to_string(),
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
            bufr: None,
            satellite: None,
            preview: None,
        },
    );
    api_edr::router(Arc::new(ArcSwap::from_pointee(EdrState {
        engines,
        collections,
        styles: HashMap::new(),
        feature_engines: HashMap::new(),
        base_url: String::new(),
        trust_proxy_headers: false,
    })))
}

/// `(status, Content-Type, body)` for a GET with an optional `Accept`.
async fn get(uri: &str, accept: Option<&str>) -> (StatusCode, String, Value) {
    let mut req = Request::builder().uri(uri);
    if let Some(accept) = accept {
        req = req.header(header::ACCEPT, accept);
    }
    let resp = router()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, content_type, body)
}

const POINT: &str = "POINT(25%2060)";
const POLYGON: &str = "POLYGON((24%2059,26%2059,26%2061,24%2061,24%2059))";
const INSTANCE: &str = "20260607T0000Z";

/// Every route that produces CoverageJSON, instance-scoped ones included.
fn coverage_routes() -> Vec<String> {
    vec![
        "/collections/c/locations/here".to_string(),
        format!("/collections/c/position?coords={POINT}"),
        format!("/collections/c/area?coords={POLYGON}"),
        format!("/collections/c/radius?coords={POINT}&within=10&within-units=km"),
        "/collections/c/trajectory?coords=LINESTRING(24%2060,25%2061)".to_string(),
        format!("/collections/c/instances/{INSTANCE}/position?coords={POINT}"),
        format!("/collections/c/instances/{INSTANCE}/area?coords={POLYGON}"),
        format!(
            "/collections/c/instances/{INSTANCE}/radius?coords={POINT}&within=10&within-units=km"
        ),
    ]
}

#[tokio::test]
async fn every_coverage_json_route_sends_the_edr_1_2_media_type() {
    for uri in coverage_routes() {
        let (status, content_type, body) = get(&uri, None).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        assert_eq!(content_type, COVJSON, "{uri}");
        assert_eq!(body["type"], "Coverage", "{uri}");
    }
}

#[tokio::test]
async fn the_edr_1_1_media_type_is_still_accepted_as_input() {
    // In `f`: encoded, with `+` read as a space, and next to the 1.2 type
    // and the format token. The response is the same CoverageJSON, sent
    // under the 1.2 type.
    for f in [
        "application%2Fprs.coverage%2Bjson",
        "application/prs.coverage+json",
        "APPLICATION/PRS.COVERAGE%2BJSON",
        "application%2Fvnd.cov%2Bjson",
        "CoverageJSON",
    ] {
        for uri in coverage_routes() {
            let sep = if uri.contains('?') { '&' } else { '?' };
            let uri = format!("{uri}{sep}f={f}");
            let (status, content_type, body) = get(&uri, None).await;
            assert_eq!(status, StatusCode::OK, "{uri}: {body}");
            assert_eq!(content_type, COVJSON, "{uri}");
            assert_eq!(body["type"], "Coverage", "{uri}");
        }
    }
    // In `Accept`: data queries do not negotiate on it, so asking for either
    // CoverageJSON type (or anything else) still gets the CoverageJSON body.
    for accept in [LEGACY_COVJSON, COVJSON, "application/json", "*/*"] {
        for uri in coverage_routes() {
            let (status, content_type, body) = get(&uri, Some(accept)).await;
            assert_eq!(status, StatusCode::OK, "{uri} Accept: {accept}: {body}");
            assert_eq!(content_type, COVJSON, "{uri} Accept: {accept}");
            assert_eq!(body["type"], "Coverage", "{uri} Accept: {accept}");
        }
    }
}

#[tokio::test]
async fn openapi_documents_coverage_json_as_the_edr_1_2_media_type() {
    let (status, _, api) = get("/api", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !api.to_string().contains(LEGACY_COVJSON),
        "/api still documents {LEGACY_COVJSON}"
    );
    // Every data path's 200 lists CoverageJSON under the 1.2 type.
    let paths = api["paths"].as_object().expect("paths");
    let data_paths: Vec<&String> = paths
        .keys()
        .filter(|p| {
            [
                "/position",
                "/area",
                "/radius",
                "/trajectory",
                "/locations/{",
            ]
            .iter()
            .any(|suffix| p.contains(suffix))
        })
        .collect();
    assert_eq!(data_paths.len(), 8, "{data_paths:?}");
    for path in data_paths {
        let content = &paths[path]["get"]["responses"]["200"]["content"];
        assert_eq!(
            content[COVJSON]["schema"]["$ref"], "#/components/schemas/coverageJSON",
            "{path}: {content}"
        );
    }
}

#[tokio::test]
async fn locations_data_links_carry_the_edr_1_2_media_type() {
    let (status, _, body) = get("/collections/c/locations", None).await;
    assert_eq!(status, StatusCode::OK);
    let link = &body["features"][0]["links"][0];
    assert_eq!(link["rel"], "data");
    assert_eq!(link["type"], COVJSON);
}
