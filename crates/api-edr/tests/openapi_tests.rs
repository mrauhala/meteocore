//! `/api` describes every status code and response body the routes answer
//! (EDR 1.2 `/req/oas/completeness`, `/req/oas/exceptions-codes`, #965), and
//! only the capabilities they implement (`/req/oas/oas-impl`): real
//! requests check that each status a route answers is listed for its
//! operation, and a walk over the document checks every operation class.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use chrono::{DateTime, TimeZone, Utc};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::util::ServiceExt;

use api_edr::handlers::EdrState;
use ds_core::config::CollectionConfig;
use ds_core::cube::CubeResolution;
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::feature::Bbox;
use ds_core::instances::RunInfo;
use ds_core::model::*;

const ALL: &[&str] = &[
    "locations",
    "position",
    "area",
    "radius",
    "cube",
    "trajectory",
];
const RUN: &str = "2026-06-07T00:00:00Z";

fn run() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 7, 0, 0, 0).unwrap()
}

/// How the stub's data queries answer.
#[derive(Clone, Copy, PartialEq)]
enum Answer {
    Data,
    /// `ResourceExhausted`: the 503 of `map_query_error`.
    Busy,
    /// `DeadlineExceeded`: the 504.
    Late,
    /// An engine failure: the generic 500.
    Broken,
}

struct Stub {
    types: &'static [&'static str],
    answer: Answer,
}

impl Stub {
    fn answer(&self) -> Result<CoverageResponse, DataServerError> {
        match self.answer {
            Answer::Data => Ok(CoverageResponse::Single(QueryResult {
                domain: DomainDescription::PointSeries {
                    x: 1.0,
                    y: 1.0,
                    t: vec![run()],
                    z: None,
                },
                parameters: HashMap::from([(
                    "t".to_string(),
                    ParameterDescription {
                        label: "Temperature".into(),
                        unit: "K".into(),
                        observed_property: "t".into(),
                        standard_name: None,
                    },
                )]),
                ranges: HashMap::from([(
                    "t".to_string(),
                    NdArray {
                        shape: vec![1],
                        axis_names: vec!["t".into()],
                        values: vec![Some(273.0)],
                    },
                )]),
            })),
            Answer::Busy => Err(DataServerError::ResourceExhausted),
            Answer::Late => Err(DataServerError::DeadlineExceeded),
            Answer::Broken => Err(DataServerError::Engine("boom".into())),
        }
    }
}

impl EdrEngine for Stub {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        if self.answer == Answer::Broken {
            return Err(DataServerError::Engine("boom".into()));
        }
        Ok(vec![Location {
            id: "a".into(),
            label: "A".into(),
            latitude: 1.0,
            longitude: 1.0,
        }])
    }
    fn get_instances(&self) -> Vec<RunInfo> {
        vec![RunInfo {
            reference_time: run(),
            valid_times: vec![run()],
        }]
    }
    fn query_location(
        &self,
        id: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        if id != "a" {
            return Err(DataServerError::LocationNotFound(id.into()));
        }
        self.answer()
    }
    fn get_parameters(&self) -> Vec<String> {
        vec!["t".into()]
    }
    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        Some((run(), run()))
    }
    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([0.0, 0.0, 10.0, 10.0])
    }
    fn supported_query_types(&self) -> Vec<String> {
        self.types.iter().map(|t| t.to_string()).collect()
    }
    fn query_area(
        &self,
        _: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        self.answer()
    }
    fn query_position(
        &self,
        _: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        self.answer()
    }
    fn query_cube(
        &self,
        _: &Bbox,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: CubeResolution,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        self.answer()
    }
    fn query_trajectory(
        &self,
        _: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        self.answer()
    }
}

fn config(id: &str) -> CollectionConfig {
    CollectionConfig {
        id: id.to_string(),
        title: format!("{id} title"),
        description: String::new(),
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
        cap: None,
        postgis: None,
        nowcast: None,
        bufr: None,
        satellite: None,
        preview: None,
        derive_wind: None,
    }
}

/// `obs` answers every query type with data; `busy`, `late` and `broken`
/// fail every one; `grid` has no locations, as a gridded engine does not.
fn app() -> axum::Router {
    let stub = |types, answer| Stub { types, answer };
    let engines = vec![
        ("obs", stub(ALL, Answer::Data)),
        ("busy", stub(ALL, Answer::Busy)),
        ("late", stub(ALL, Answer::Late)),
        ("broken", stub(ALL, Answer::Broken)),
        ("grid", stub(&["position", "area", "radius"], Answer::Data)),
    ];
    let state = EdrState {
        collections: engines
            .iter()
            .map(|(id, _)| (id.to_string(), config(id)))
            .collect(),
        engines: engines
            .into_iter()
            .map(|(id, e)| (id.to_string(), Arc::new(e) as Arc<dyn EdrEngine>))
            .collect(),
        styles: HashMap::new(),
        feature_engines: HashMap::new(),
        base_url: String::new(),
        trust_proxy_headers: false,
    };
    api_edr::router(Arc::new(ArcSwap::from_pointee(state)))
}

struct Reply {
    status: StatusCode,
    content_type: String,
    etag: Option<String>,
    body: Value,
}

async fn send(app: &axum::Router, uri: &str, if_none_match: Option<&str>) -> Reply {
    let mut request = Request::builder().uri(uri);
    if let Some(etag) = if_none_match {
        request = request.header(header::IF_NONE_MATCH, etag);
    }
    let resp = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let value = |name: header::HeaderName| {
        resp.headers()
            .get(name)
            .map(|v| v.to_str().unwrap().to_string())
    };
    let (status, content_type, etag) = (
        resp.status(),
        value(header::CONTENT_TYPE).unwrap_or_default(),
        value(header::ETAG),
    );
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        content_type,
        etag,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

async fn api(app: &axum::Router) -> Value {
    let reply = send(app, "/api", None).await;
    assert_eq!(reply.status, StatusCode::OK);
    reply.body
}

/// Follow a local `$ref`, or return the value itself.
fn resolve<'a>(api: &'a Value, value: &'a Value) -> &'a Value {
    match value["$ref"].as_str() {
        Some(r) => api
            .pointer(r.strip_prefix('#').unwrap())
            .unwrap_or_else(|| panic!("dangling $ref {r}")),
        None => value,
    }
}

/// Every `(path, operation)` of the document.
fn operations(api: &Value) -> Vec<(&str, &Value)> {
    api["paths"]
        .as_object()
        .unwrap()
        .iter()
        .flat_map(|(path, item)| {
            item.as_object()
                .unwrap()
                .values()
                .map(move |op| (path.as_str(), op))
        })
        .collect()
}

fn parameter<'a>(api: &'a Value, op: &'a Value, name: &str) -> Option<&'a Value> {
    op["parameters"]
        .as_array()?
        .iter()
        .map(|p| resolve(api, p))
        .find(|p| p["name"] == name)
}

/// Requests and the status each answers, beside the `/api` path of its
/// operation.
fn cases() -> Vec<(String, String, StatusCode)> {
    use StatusCode as S;
    let mut cases: Vec<(String, String, StatusCode)> = [
        ("/?f=xml", "/edr/", S::BAD_REQUEST),
        ("/conformance?f=xml", "/edr/conformance", S::BAD_REQUEST),
        ("/collections?f=xml", "/edr/collections", S::BAD_REQUEST),
        (
            "/collections/obs?f=xml",
            "/edr/collections/obs",
            S::BAD_REQUEST,
        ),
        (
            "/collections/obs/instances?f=xml",
            "/edr/collections/obs/instances",
            S::BAD_REQUEST,
        ),
        (
            "/collections/obs/instances/2026-06-07T00:00:00Z?f=xml",
            "/edr/collections/obs/instances/{instanceId}",
            S::BAD_REQUEST,
        ),
        (
            "/collections/obs/instances/bogus",
            "/edr/collections/obs/instances/{instanceId}",
            S::BAD_REQUEST,
        ),
        (
            "/collections/obs/instances/1999-01-01T00:00:00Z",
            "/edr/collections/obs/instances/{instanceId}",
            S::NOT_FOUND,
        ),
        // `/locations`: an unsupported `f` and `crs` (not a /locations
        // parameter) are 400s, the inventory failing a 500.
        (
            "/collections/obs/locations?f=GeoJSON",
            "/edr/collections/obs/locations",
            S::OK,
        ),
        (
            "/collections/obs/locations?f=xml",
            "/edr/collections/obs/locations",
            S::BAD_REQUEST,
        ),
        (
            "/collections/obs/locations?crs=CRS84",
            "/edr/collections/obs/locations",
            S::BAD_REQUEST,
        ),
        (
            "/collections/broken/locations",
            "/edr/collections/broken/locations",
            S::INTERNAL_SERVER_ERROR,
        ),
        (
            "/collections/obs/locations/a,zz",
            "/edr/collections/obs/locations/{locationId}",
            S::NOT_FOUND,
        ),
    ]
    .into_iter()
    .map(|(uri, path, status)| (uri.to_string(), path.to_string(), status))
    .collect();

    let queries = [
        ("locations", "locations/a?"),
        ("position", "position?coords=POINT(1%201)&"),
        (
            "area",
            "area?coords=POLYGON((0%200,2%200,2%202,0%202,0%200))&",
        ),
        (
            "radius",
            "radius?coords=POINT(1%201)&within=1&within-units=km&",
        ),
        ("cube", "cube?bbox=0,0,2,2&"),
        ("trajectory", "trajectory?coords=LINESTRING(0%200,2%202)&"),
    ];
    for (query, request) in queries {
        let path = match query {
            "locations" => "locations/{locationId}",
            other => other,
        };
        // An instance route for the queries a model run serves.
        let mut prefixes = vec![(String::new(), String::new())];
        if matches!(query, "position" | "area" | "radius" | "cube") {
            prefixes.push((
                format!("/instances/{RUN}"),
                "/instances/{instanceId}".into(),
            ));
        }
        for (prefix, path_prefix) in prefixes {
            for (collection, status) in [
                ("obs", S::OK),
                ("busy", S::SERVICE_UNAVAILABLE),
                ("late", S::GATEWAY_TIMEOUT),
                ("broken", S::INTERNAL_SERVER_ERROR),
            ] {
                let uri = format!("/collections/{collection}{prefix}/{request}");
                let path = format!("/edr/collections/{collection}{path_prefix}/{path}");
                cases.push((uri.clone(), path.clone(), status));
                if collection == "obs" {
                    cases.push((format!("{uri}crs=CRS84"), path.clone(), S::OK));
                    cases.push((format!("{uri}crs=EPSG:3067"), path.clone(), S::BAD_REQUEST));
                    cases.push((format!("{uri}f=xml"), path, S::BAD_REQUEST));
                }
            }
        }
    }
    cases
}

/// Every status a route answers is listed for its operation, and every
/// error is the documented `exception` body.
#[tokio::test]
async fn every_status_a_route_answers_is_documented_for_it() {
    let app = app();
    let api = api(&app).await;
    for (uri, path, status) in cases() {
        let reply = send(&app, &uri, None).await;
        assert_eq!(reply.status, status, "{uri}: {}", reply.body);
        let responses = &api["paths"][&path]["get"]["responses"];
        assert!(
            responses.get(status.as_str()).is_some(),
            "{uri}: {status} is not documented for {path}: {responses}"
        );
        if status.is_client_error() || status.is_server_error() {
            assert_eq!(reply.content_type, "application/json", "{uri}");
            for member in ["code", "description"] {
                assert!(reply.body[member].is_string(), "{uri}: {}", reply.body);
            }
        }
    }
}

/// A repeated request naming the representation's ETag is a 304, on
/// metadata and data routes alike, and every operation documents it.
#[tokio::test]
async fn not_modified_is_documented() {
    let app = app();
    let api = api(&app).await;
    for (uri, path) in [
        ("/collections/obs", "/edr/collections/obs"),
        (
            "/collections/obs/position?coords=POINT(1%201)",
            "/edr/collections/obs/position",
        ),
    ] {
        let first = send(&app, uri, None).await;
        assert_eq!(first.status, StatusCode::OK, "{uri}");
        let etag = first.etag.expect("etag");
        let again = send(&app, uri, Some(&etag)).await;
        assert_eq!(again.status, StatusCode::NOT_MODIFIED, "{uri}");
        assert!(api["paths"][path]["get"]["responses"]["304"].is_object());
    }
}

/// The document's shape: served as the OpenAPI media type, valid OpenAPI
/// 3.0, every `$ref` resolving, and each operation class listing what its
/// routes answer.
#[tokio::test]
async fn every_operation_lists_its_status_classes_and_parameters() {
    let app = app();
    let reply = send(&app, "/api", None).await;
    assert_eq!(
        reply.content_type,
        "application/vnd.oai.openapi+json;version=3.0"
    );
    let api = reply.body;
    let meta: Value =
        serde_json::from_str(include_str!("../../../schemas/openapi-3.0.json")).unwrap();
    let validator = jsonschema::Validator::new(&meta).unwrap();
    let errors: Vec<String> = validator
        .iter_errors(&api)
        .map(|e| format!("{e} at {}", e.instance_path()))
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
    check_refs(&api, &api);

    let data_query = |path: &str| {
        [
            "position",
            "area",
            "radius",
            "cube",
            "trajectory",
            "locations",
        ]
        .iter()
        .any(|q| path.ends_with(&format!("/{q}")))
            || path.ends_with("/locations/{locationId}")
    };
    let mut queries = 0;
    for (path, op) in operations(&api) {
        let responses = &op["responses"];
        let has = |status: &str| responses.get(status).is_some();
        // The conditional-GET layer and an internal error, on every route.
        assert!(has("304") && has("500"), "{path}: {responses}");
        if parameter(&api, op, "f").is_some() {
            assert!(has("400"), "{path} negotiates f: {responses}");
        }
        for (status, response) in responses.as_object().unwrap() {
            if status.starts_with('4') || status.starts_with('5') {
                let response = resolve(&api, response);
                let schema = resolve(&api, &response["content"]["application/json"]["schema"]);
                assert_eq!(
                    schema["required"],
                    json!(["code", "description"]),
                    "{path} {status}"
                );
            }
        }
        if !data_query(path) {
            continue;
        }
        queries += 1;
        // Every data query documents its HTML page (#971) in `f` and in
        // its 200's content.
        let ok = resolve(&api, &responses["200"]);
        assert!(ok["content"]["text/html"].is_object(), "{path}: {ok}");
        let f = parameter(&api, op, "f").unwrap_or_else(|| panic!("{path} lacks f"));
        assert!(
            f["schema"]["enum"]
                .as_array()
                .is_some_and(|e| e.iter().any(|v| v == "HTML")),
            "{path}: {f}"
        );
        for status in ["400", "404", "503", "504"] {
            assert!(has(status), "{path} lacks {status}: {responses}");
        }
        assert!(parameter(&api, op, "f").is_some(), "{path} lacks f");
        // EDR 1.2 gives every data query but the /locations list a `crs`.
        let crs = parameter(&api, op, "crs");
        if path.ends_with("/locations") {
            assert!(crs.is_none(), "{path}");
            continue;
        }
        let crs = crs.unwrap_or_else(|| panic!("{path} lacks crs"));
        assert_eq!(crs["in"], "query", "{path}");
        assert_eq!(crs["required"], false, "{path}");
        assert_eq!(crs["schema"], json!({"type": "string"}), "{path}");
        assert_eq!(
            (&crs["style"], &crs["explode"]),
            (&json!("form"), &json!(false))
        );
    }
    // obs, busy, late and broken: seven query paths and four instance
    // routes each; grid: position, area and radius, on the collection and
    // on its run.
    assert_eq!(queries, 4 * 11 + 2 * 3);
    for path in [
        "/edr/collections/obs/instances",
        "/edr/collections/obs/instances/{instanceId}",
    ] {
        let op = &api["paths"][path]["get"];
        let f = parameter(&api, op, "f").unwrap_or_else(|| panic!("{path} lacks f"));
        assert_eq!(f["schema"]["enum"], json!(["json", "html"]));
    }
}

/// Every `$ref` in `value` names something in `api`.
fn check_refs(api: &Value, value: &Value) {
    match value {
        Value::Object(map) => {
            if let Some(r) = map.get("$ref").and_then(Value::as_str) {
                let pointer = r.strip_prefix('#').expect("local $ref");
                assert!(api.pointer(pointer).is_some(), "dangling $ref {r}");
            }
            map.values().for_each(|v| check_refs(api, v));
        }
        Value::Array(items) => items.iter().for_each(|v| check_refs(api, v)),
        _ => {}
    }
}

/// A collection whose engine has no locations has neither locations
/// resource: no path in `/api`, no `data_queries` entry, and 404 on both
/// routes, as every other unsupported query type (#668).
#[tokio::test]
async fn locations_are_gated_like_every_query_type() {
    let app = app();
    let api = api(&app).await;
    for path in [
        "/edr/collections/grid/locations",
        "/edr/collections/grid/locations/{locationId}",
    ] {
        assert!(api["paths"].get(path).is_none(), "{path}");
        let advertised = path.replace("/grid/", "/obs/");
        assert!(api["paths"][&advertised].is_object(), "{advertised}");
    }
    let doc = send(&app, "/collections/grid", None).await.body;
    assert!(doc["data_queries"].get("locations").is_none(), "{doc}");
    assert!(doc["data_queries"]["position"].is_object(), "{doc}");
    for uri in [
        "/collections/grid/locations",
        "/collections/grid/locations/a",
    ] {
        let reply = send(&app, uri, None).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(reply.body["code"], "NotFound", "{uri}");
        let description = reply.body["description"].as_str().unwrap();
        assert!(description.contains("location queries"), "{description}");
    }
}

/// `crs` accepts CRS84 in each of its spellings on every data query, and a
/// 400 names the CRS served for any other (`/req/edr/REQ_rc-crs-response`).
#[tokio::test]
async fn crs_names_crs84_or_is_a_400() {
    let app = app();
    for crs in [
        "CRS84",
        "OGC:CRS84",
        "http://www.opengis.net/def/crs/OGC/1.3/CRS84",
        "%5BOGC:CRS84%5D",
    ] {
        let uri = format!(
            "/collections/obs/area?coords=POLYGON((0%200,2%200,2%202,0%202,0%200))&crs={crs}"
        );
        assert_eq!(send(&app, &uri, None).await.status, StatusCode::OK, "{uri}");
    }
    for crs in ["EPSG:4326", "EPSG:3067", "bogus"] {
        let uri = format!("/collections/obs/position?coords=POINT(1%201)&crs={crs}");
        let reply = send(&app, &uri, None).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{uri}");
        let description = reply.body["description"].as_str().unwrap();
        assert!(description.contains("OGC/1.3/CRS84"), "{description}");
    }
}
