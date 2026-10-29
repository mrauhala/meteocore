//! What a browser sees when an EDR request fails (#1132).
//!
//! A request that asked for HTML, with `f=html` or a browser's `Accept`,
//! gets each error as a workbench page with the same status: the message,
//! the parameter at fault, and links back. An unknown `parameter-name` lists
//! the valid ones as links re-running the query, and a query with no data
//! says so for its query type with the advertised time range. A JSON request
//! gets the JSON body, unchanged.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use chrono::{DateTime, Duration, TimeZone, Utc};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::util::ServiceExt;

use api_edr::handlers::EdrState;
use ds_core::config::CollectionConfig;
use ds_core::edr_engine::{select_parameters, EdrEngine};
use ds_core::error::DataServerError;
use ds_core::model::{CoverageResponse, LocationSnapshot};

const BROWSER: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";
const POINT: &str = "POINT(25%2060)";
const POLYGON: &str = "POLYGON((24%2059,26%2059,26%2061,24%2061,24%2059))";
const PARAMETERS: [&str; 2] = ["temperature", "wind speed"];

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 7, 0, 0, 0).unwrap()
}

/// A gridded collection that checks `parameter-name` as engines do, then
/// has no data anywhere; a position at the origin is an internal error.
struct Empty;

impl Empty {
    fn answer(&self, parameters: Option<&[String]>) -> Result<CoverageResponse, DataServerError> {
        select_parameters(parameters, &PARAMETERS)?;
        Err(DataServerError::LocationNotFound("no data".into()))
    }
}

impl EdrEngine for Empty {
    fn get_locations(&self) -> Result<Arc<LocationSnapshot>, DataServerError> {
        Ok(LocationSnapshot::empty())
    }
    fn query_location(
        &self,
        id: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Err(DataServerError::LocationNotFound(id.into()))
    }
    fn query_position(
        &self,
        coords: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        if coords.contains("(0 0)") {
            return Err(DataServerError::Engine("disk /var/secret failed".into()));
        }
        self.answer(parameters)
    }
    fn query_area(
        &self,
        _: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        self.answer(parameters)
    }
    fn get_parameters(&self) -> Vec<String> {
        PARAMETERS.map(String::from).to_vec()
    }
    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        Some((t0(), t0() + Duration::hours(6)))
    }
    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([20.0, 58.0, 30.0, 62.0])
    }
    fn supported_query_types(&self) -> Vec<String> {
        vec!["position".to_string(), "area".to_string()]
    }
}

/// The EDR router mounted at `/edr`, as the server mounts it.
fn router() -> axum::Router {
    let mut engines: HashMap<String, Arc<dyn EdrEngine>> = HashMap::new();
    engines.insert("g".into(), Arc::new(Empty));
    let config: CollectionConfig =
        serde_json::from_value(json!({"id": "g", "title": "Gridded", "description": "No data"}))
            .unwrap();
    let state = EdrState {
        engines,
        collections: HashMap::from([("g".to_string(), config)]),
        discovery: Default::default(),
        styles: HashMap::new(),
        feature_engines: HashMap::new(),
        base_url: "https://example.org".to_string(),
        trust_proxy_headers: false,
    };
    axum::Router::new().nest(
        "/edr",
        api_edr::router(Arc::new(ArcSwap::from_pointee(state))),
    )
}

/// `(status, headers, body)` for a GET with an optional `Accept`.
async fn get(uri: &str, accept: Option<&str>) -> (StatusCode, HeaderMap, String) {
    let mut req = Request::builder().uri(uri);
    if let Some(accept) = accept {
        req = req.header(header::ACCEPT, accept);
    }
    let resp = router()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let (status, headers) = (resp.status(), resp.headers().clone());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
}

fn media(headers: &HeaderMap) -> &str {
    headers[header::CONTENT_TYPE].to_str().unwrap()
}

/// The HTML page of `uri` with `f=html`, and of `uri` with a browser's
/// `Accept`: both pages, with `status`.
async fn pages(uri: &str, status: StatusCode) -> String {
    let (got, headers, body) = get(&format!("{uri}&f=html"), None).await;
    assert_eq!(got, status, "{uri}: {body}");
    assert_eq!(media(&headers), "text/html; charset=utf-8", "{uri}");
    assert!(body.starts_with("<!DOCTYPE html>"), "{uri}");
    let (got, headers, browser) = get(uri, Some(BROWSER)).await;
    assert_eq!(got, status, "{uri}");
    assert_eq!(media(&headers), "text/html; charset=utf-8", "{uri}");
    assert!(browser.contains(&format!("HTTP {}", status.as_u16())));
    body
}

/// The JSON body of `uri`, which has `status`.
async fn json_error(uri: &str, status: StatusCode) -> Value {
    let (got, headers, body) = get(uri, None).await;
    assert_eq!(got, status, "{uri}: {body}");
    assert_eq!(media(&headers), "application/json", "{uri}");
    serde_json::from_str(&body).unwrap()
}

/// An unknown `parameter-name` is a page listing the collection's
/// parameters, each a link re-running the query with it; JSON keeps its
/// body.
#[tokio::test]
async fn an_unknown_parameter_lists_the_valid_ones_as_links() {
    let uri = format!("/edr/collections/g/area?coords={POLYGON}&parameter-name=nope");
    let html = pages(&uri, StatusCode::BAD_REQUEST).await;
    assert!(html.contains("<h1>Unknown parameter</h1>"), "{html}");
    assert!(html.contains("Unknown parameter &#39;nope&#39;; available: temperature, wind speed"));
    assert!(html.contains("Parameter <code>parameter-name</code>: <code>nope</code>"));
    let rerun = "https://example.org/edr/collections/g/area?coords=POLYGON%28%2824+59%2C26+59%2C26+61%2C24+61%2C24+59%29%29&amp;parameter-name=";
    for value in ["temperature", "wind+speed"] {
        assert!(
            html.contains(&format!("href=\"{rerun}{value}&amp;f=html\"")),
            "{value}: {html}"
        );
    }
    // Back to the collection and to the catalog.
    assert!(html.contains("href=\"https://example.org/edr/collections/g?f=html\">← Back to g</a>"));
    assert!(html.contains("href=\"https://example.org/edr/collections?f=html\""));

    let error = json_error(&uri, StatusCode::BAD_REQUEST).await;
    assert_eq!(
        error,
        json!({
            "code": "BadRequest",
            "description": "Invalid parameter: Unknown parameter 'nope'; available: temperature, wind speed"
        })
    );
    // A known parameter that finds no data is no "unknown parameter".
    let known = format!("/edr/collections/g/area?coords={POLYGON}&parameter-name=temperature");
    let html = pages(&known, StatusCode::NOT_FOUND).await;
    assert!(!html.contains("Valid values"));
    // Nor is a 400 about another parameter that the request fails on
    // first: its page is about that one.
    let datetime =
        format!("/edr/collections/g/area?coords={POLYGON}&datetime=yesterday&parameter-name=nope");
    let html = pages(&datetime, StatusCode::BAD_REQUEST).await;
    assert!(!html.contains("<h1>Unknown parameter</h1>"), "{html}");
    assert!(!html.contains("Valid values"), "{html}");
    assert!(!html.contains("Unknown parameter &#39;nope&#39;"), "{html}");
}

/// No data reads as what the query asked about, with the time range the
/// collection advertises, never "Location not found"; JSON keeps its body.
#[tokio::test]
async fn no_data_is_worded_for_the_query_type_with_the_time_range() {
    for (query, title) in [
        (format!("area?coords={POLYGON}"), "No data in this area"),
        (
            format!("position?coords={POINT}"),
            "No data at this position",
        ),
    ] {
        let uri = format!("/edr/collections/g/{query}&datetime=2020-01-01T00:00:00Z");
        let html = pages(&uri, StatusCode::NOT_FOUND).await;
        assert!(html.contains(&format!("<h1>{title}</h1>")), "{html}");
        assert!(html.contains("at datetime 2020-01-01T00:00:00Z"), "{html}");
        assert!(
            html.contains("data from 2026-06-07T00:00:00Z to 2026-06-07T06:00:00Z"),
            "{html}"
        );
        assert!(!html.contains("Location not found"), "{html}");
        let error = json_error(&uri, StatusCode::NOT_FOUND).await;
        assert_eq!(error["description"], "Location not found: no data");
    }
}

/// An unknown collection, a query string that does not parse and an
/// internal error are pages too; the last says nothing of its cause.
#[tokio::test]
async fn other_failures_are_pages_with_a_way_back() {
    let html = pages(
        &format!("/edr/collections/nope/area?coords={POLYGON}"),
        StatusCode::NOT_FOUND,
    )
    .await;
    assert!(html.contains("href=\"https://example.org/edr/collections?f=html\""));
    // No way "back" to a collection the API does not serve.
    assert!(!html.contains("Back to nope"), "{html}");

    // `JsonQuery`'s rejection names the field it failed on.
    let twice = format!("/edr/collections/g/position?coords={POINT}&coords={POINT}");
    let html = pages(&twice, StatusCode::BAD_REQUEST).await;
    assert!(html.contains("duplicate field `coords`"), "{html}");
    assert!(html.contains("Parameter <code>coords</code>"), "{html}");
    let error = json_error(&twice, StatusCode::BAD_REQUEST).await;
    assert!(error["description"]
        .as_str()
        .unwrap()
        .contains("duplicate field `coords`"));

    let html = pages(
        "/edr/collections/g/position?coords=POINT(0%200)",
        StatusCode::INTERNAL_SERVER_ERROR,
    )
    .await;
    assert!(html.contains("The server could not complete this request."));
    assert!(!html.contains("secret"));
}
