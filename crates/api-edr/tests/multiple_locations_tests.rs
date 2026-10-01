//! Several location ids in one locations query (EDR 1.2, #923).
//!
//! `/req/edr/REQ_rc-locationid-definition`: `locationId` is a comma-delimited
//! list (`style: simple`, `explode: false`). `/req/edr/REQ_rc-locationid-
//! response`: only the listed locations, and a 204 SHOULD answer a list none
//! of whose locations has data. `/req/edr/rc-locations-variables`: the
//! collection advertises it with `multiple_locations: true`.
//!
//! Decided for MeteoCore: an unknown id fails the whole list with a 404
//! naming it; known ids without data in the window contribute nothing, and
//! the 204 comes only when none of them has data. One id answers exactly as
//! before, its no-data 404 included.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use chrono::{DateTime, TimeZone, Utc};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use api_edr::handlers::EdrState;
use api_edr::params::{MAX_LOCATION_IDS, MAX_LOCATION_VALUES};
use ds_core::config::CollectionConfig;
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::model::*;

/// An id with a comma in it, addressable as `Helsinki%2C%20Kaisaniemi`.
const COMMA_ID: &str = "Helsinki, Kaisaniemi";
/// Values each `big*` station returns: two of them exceed the combined budget.
const BIG_VALUES: usize = MAX_LOCATION_VALUES / 2 + 1;

fn time(hour: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 7, hour, 0, 0).unwrap()
}

/// A two-step PointSeries coverage at `x`, which identifies it in assertions.
fn coverage(x: f64) -> QueryResult {
    coverage_with_values(x, vec![Some(1.0), Some(2.0)])
}

fn coverage_with_values(x: f64, values: Vec<Option<f64>>) -> QueryResult {
    let mut parameters = HashMap::new();
    parameters.insert(
        "temperature".to_string(),
        ParameterDescription {
            label: "temperature".into(),
            unit: "degC".into(),
            observed_property: "temperature".into(),
            standard_name: None,
        },
    );
    let mut ranges = HashMap::new();
    ranges.insert(
        "temperature".to_string(),
        NdArray {
            shape: vec![values.len()],
            axis_names: vec!["t".into()],
            values,
        },
    );
    QueryResult {
        domain: DomainDescription::PointSeries {
            x,
            y: 60.0,
            t: vec![time(0), time(1)],
            z: None,
        },
        parameters,
        ranges,
    }
}

/// A station network: `s0` has three coverages (one per level, say), `s1`
/// and the comma id one each, `s2`/`s3` none in any window, `big0`/`big1`
/// half the value budget each. Records every engine call.
#[derive(Default)]
struct Stations {
    queried: Mutex<Vec<String>>,
    inventory_reads: Mutex<usize>,
}

impl EdrEngine for Stations {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        *self.inventory_reads.lock().unwrap() += 1;
        Ok(["s0", "s1", "s2", "s3", "big0", "big1", COMMA_ID]
            .iter()
            .map(|id| Location {
                id: id.to_string(),
                label: id.to_string(),
                latitude: 60.0,
                longitude: 24.0,
            })
            .collect())
    }

    fn query_location(
        &self,
        location_id: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        self.queried.lock().unwrap().push(location_id.to_string());
        match location_id {
            "s0" => Ok(CoverageResponse::Collection(
                (0..3).map(|i| coverage(i as f64)).collect(),
            )),
            "s1" => Ok(CoverageResponse::Single(coverage(10.0))),
            COMMA_ID => Ok(CoverageResponse::Single(coverage(20.0))),
            // Half the combined budget plus one; the domain is not sized to
            // match, which nothing here validates.
            "big0" | "big1" => Ok(CoverageResponse::Single(coverage_with_values(
                30.0,
                vec![Some(0.0); BIG_VALUES],
            ))),
            // Known, but no data in the window: the engines' convention.
            "s2" | "s3" => Err(DataServerError::LocationNotFound(format!(
                "{location_id} (no data in time range)"
            ))),
            other => Err(DataServerError::LocationNotFound(other.into())),
        }
    }

    fn get_parameters(&self) -> Vec<String> {
        vec!["temperature".into()]
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        Some((time(0), time(1)))
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([20.0, 59.0, 30.0, 70.0])
    }
}

struct App {
    router: axum::Router,
    engine: Arc<Stations>,
}

fn app() -> App {
    let engine = Arc::new(Stations::default());
    let config = CollectionConfig {
        id: "obs".into(),
        title: "Observations".into(),
        description: "multiple-locations fixture".into(),
        data_path: None,
        apis: vec!["edr".into()],
        engine_type: "csv".into(),
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
    };
    let state = Arc::new(ArcSwap::from_pointee(EdrState {
        engines: HashMap::from([("obs".to_string(), engine.clone() as Arc<dyn EdrEngine>)]),
        collections: HashMap::from([("obs".to_string(), config)]),
        styles: HashMap::new(),
        feature_engines: HashMap::new(),
        base_url: "https://example.test".into(),
        trust_proxy_headers: false,
    }));
    App {
        router: api_edr::router(state),
        engine,
    }
}

impl App {
    async fn send(&self, uri: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
        let response = self
            .router
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, headers, body.to_vec())
    }

    async fn get(&self, uri: &str) -> (StatusCode, Value) {
        let (status, _, body) = self.send(uri).await;
        let json = serde_json::from_slice(&body)
            .unwrap_or_else(|_| panic!("{uri}: non-JSON {status} body"));
        (status, json)
    }

    /// The ids `query_location` was called with, in call order; clears them.
    fn queried(&self) -> Vec<String> {
        std::mem::take(&mut *self.engine.queried.lock().unwrap())
    }

    fn inventory_reads(&self) -> usize {
        *self.engine.inventory_reads.lock().unwrap()
    }
}

fn locations(ids: &str) -> String {
    format!("/collections/obs/locations/{ids}")
}

fn xs(json: &Value) -> Vec<f64> {
    assert_eq!(json["type"], "CoverageCollection", "{json}");
    json["coverages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["domain"]["axes"]["x"]["values"][0].as_f64().unwrap())
        .collect()
}

fn validate(schema_file: &str, json: &Value) {
    let path = format!("{}/../../schemas/{schema_file}", env!("CARGO_MANIFEST_DIR"));
    let schema: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let validator = jsonschema::Validator::new(&schema).unwrap();
    let errors: Vec<String> = validator
        .iter_errors(json)
        .map(|e| format!("{e} at {}", e.instance_path()))
        .collect();
    assert!(errors.is_empty(), "{schema_file}: {errors:?}\n{json}");
}

fn description(json: &Value) -> &str {
    json["description"].as_str().unwrap_or_default()
}

#[tokio::test]
async fn a_list_is_one_collection_in_request_order() {
    let app = app();
    let (status, headers, body) = app.send(&locations("s1,s0")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "application/vnd.cov+json");
    let json: Value = serde_json::from_slice(&body).unwrap();
    // s1's coverage, then s0's three in the engine's order.
    assert_eq!(xs(&json), [10.0, 0.0, 1.0, 2.0]);
    validate("coveragejson.json", &json);
    assert_eq!(app.queried(), ["s1", "s0"]);
    assert_eq!(
        app.inventory_reads(),
        0,
        "every id answered: no inventory read"
    );
}

#[tokio::test]
async fn one_id_answers_as_before() {
    let app = app();
    let (status, json) = app.get(&locations("s1")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["type"], "Coverage");
    let (_, json) = app.get(&locations("s0")).await;
    assert_eq!(xs(&json), [0.0, 1.0, 2.0]);
    // A known id without data is still the 404 it was, not a 204.
    let (status, json) = app.get(&locations("s2")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(description(&json).contains("s2"), "{json}");
    // A repeat-only list is one id.
    let (status, json) = app.get(&locations("s1,s1")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["type"], "Coverage");
    // PNG still plots one location.
    let (status, headers, _) = app.send(&format!("{}?f=PNG", locations("s1"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "image/png");
    assert_eq!(app.inventory_reads(), 0);
}

#[tokio::test]
async fn repeated_ids_are_answered_once() {
    let app = app();
    let (status, json) = app.get(&locations("s1,s0,s1,s0")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(xs(&json), [10.0, 0.0, 1.0, 2.0]);
    assert_eq!(app.queried(), ["s1", "s0"]);
}

#[tokio::test]
async fn an_unknown_id_fails_the_list_with_a_404_naming_it() {
    let app = app();
    for ids in ["s1,nope", "nope,s1", "s2,nope,s3"] {
        let (status, json) = app.get(&locations(ids)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{ids}: {json}");
        assert_eq!(json["code"], "NotFound", "{ids}");
        assert!(description(&json).contains("nope"), "{ids}: {json}");
    }
    // The list stops at the unknown id.
    app.queried();
    app.get(&locations("nope,s1")).await;
    assert_eq!(app.queried(), ["nope"]);
}

#[tokio::test]
async fn ids_without_data_contribute_nothing() {
    let app = app();
    let (status, json) = app.get(&locations("s2,s1,s3")).await;
    assert_eq!(status, StatusCode::OK);
    // Still a collection: the shape follows the request, as for MULTIPOINT.
    assert_eq!(xs(&json), [10.0]);
    validate("coveragejson.json", &json);
}

#[tokio::test]
async fn no_data_at_any_listed_id_is_204() {
    let app = app();
    let (status, headers, body) = app
        .send(&format!(
            "{}?datetime=2020-01-01T00:00:00Z/2020-01-02T00:00:00Z",
            locations("s2,s3")
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(body.is_empty());
    // A settled window with no data stays settled, like its 200 would.
    assert_eq!(
        headers[header::CACHE_CONTROL],
        ds_core::http_cache::CACHE_CONTROL_SETTLED
    );
    assert!(headers.get(header::ETAG).is_none());
}

#[tokio::test]
async fn the_list_is_capped_before_any_query() {
    let app = app();
    let at_cap: Vec<String> = (0..MAX_LOCATION_IDS).map(|_| "s1".to_string()).collect();
    let (status, _) = app.get(&locations(&at_cap.join(","))).await;
    assert_eq!(status, StatusCode::OK);
    app.queried();

    let over = vec!["s1"; MAX_LOCATION_IDS + 1].join(",");
    let (status, json) = app.get(&locations(&over)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert!(description(&json).contains("more than 64"), "{json}");
    assert!(app.queried().is_empty());
}

#[tokio::test]
async fn an_empty_element_is_400() {
    let app = app();
    for ids in ["s1,,s0", "s1,", ",s1"] {
        let (status, json) = app.get(&locations(ids)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{ids}: {json}");
        assert!(
            description(&json).contains("empty element"),
            "{ids}: {json}"
        );
    }
    assert!(app.queried().is_empty());
}

#[tokio::test]
async fn limit_counts_the_flattened_coverages() {
    let app = app();
    let (_, json) = app.get(&format!("{}?limit=2", locations("s0,s1"))).await;
    assert_eq!(xs(&json), [0.0, 1.0]);
    // Ids past the limit are never queried...
    assert_eq!(app.queried(), ["s0"]);
    let (_, json) = app.get(&format!("{}?limit=4", locations("s0,s1"))).await;
    assert_eq!(xs(&json), [0.0, 1.0, 2.0, 10.0]);
    validate("coveragejson.json", &json);
    app.queried();
    // ...but must still exist.
    let (status, json) = app.get(&format!("{}?limit=1", locations("s1,nope"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{json}");
    assert!(description(&json).contains("nope"), "{json}");
    assert_eq!(app.queried(), ["s1"]);
    let (status, json) = app.get(&format!("{}?limit=0", locations("s0,s1"))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
}

#[tokio::test]
async fn the_values_are_capped_combined() {
    let app = app();
    let (status, json) = app.get(&locations("big0,big1")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert!(description(&json).contains("values combined"), "{json}");
    // What `limit` drops does not count.
    let (status, _, _) = app
        .send(&format!("{}?limit=1", locations("big0,big1")))
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn png_is_one_location_only() {
    let app = app();
    let (status, json) = app.get(&format!("{}?f=PNG", locations("s0,s1"))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert!(description(&json).contains("PNG"), "{json}");
    assert!(app.queried().is_empty());
}

#[tokio::test]
async fn an_encoded_comma_belongs_to_the_id() {
    let app = app();
    let (status, json) = app.get(&locations("Helsinki%2C%20Kaisaniemi")).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["type"], "Coverage");
    let (_, json) = app.get(&locations("Helsinki%2C%20Kaisaniemi,s1")).await;
    assert_eq!(xs(&json), [20.0, 10.0]);
    assert_eq!(app.queried(), [COMMA_ID, COMMA_ID, "s1"]);
    // A literal comma separates: `Helsinki` is not a location.
    let (status, json) = app.get(&locations("Helsinki,%20Kaisaniemi")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{json}");
    assert!(description(&json).contains("Helsinki"), "{json}");
}

#[tokio::test]
async fn multiple_locations_is_advertised() {
    let app = app();
    let (_, collection) = app.get("/collections/obs").await;
    let (_, list) = app.get("/collections").await;
    for doc in [&collection, &list["collections"][0]] {
        let variables = &doc["data_queries"]["locations"]["link"]["variables"];
        assert_eq!(variables["multiple_locations"], true, "{variables}");
        assert_eq!(variables["query_type"], "locations");
    }
}

/// `locationId` is declared as `/req/edr/REQ_rc-locationid-definition` asks,
/// in the form the EDR 1.2 bundle writes it, and `/api` stays valid
/// OpenAPI 3.0.
#[tokio::test]
async fn openapi_declares_a_location_id_list() {
    let app = app();
    let (_, api) = app.get("/api").await;
    validate("openapi-3.0.json", &api);
    let operation = &api["paths"]["/edr/collections/obs/locations/{locationId}"]["get"];
    let location_id = operation["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "locationId")
        .expect("locationId parameter");
    assert_eq!(location_id["in"], "path");
    assert_eq!(location_id["required"], true);
    assert_eq!(location_id["style"], "simple");
    assert_eq!(location_id["explode"], false);
    assert_eq!(location_id["schema"], serde_json::json!({"type": "string"}));
    let text = location_id["description"].as_str().unwrap();
    assert!(text.starts_with("Comma-delimited list"), "{text}");
    assert!(text.contains("%2C"), "{text}");
    assert!(operation["responses"].get("204").is_some());
}
