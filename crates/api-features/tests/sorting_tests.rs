//! `sortby` on `/items` — OGC API - Features Part 8: Sorting (draft 24-030).
//!
//! The behaviour these tests exist to lock down is not "sorting works" but
//! "sorting cannot silently not work": before this, `?sortby=` returned 200
//! with untouched order, which is indistinguishable from success at the call
//! site.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use api_features::handlers::FeaturesState;
use api_features::params::parse_sortby;
use ds_core::config::CollectionConfig;
use ds_core::error::DataServerError;
use ds_core::feature::*;
use ds_core::feature_engine::FeatureEngine;

// ---------------------------------------------------------------------------
// Engines: one that advertises sortables, one that does not.
// ---------------------------------------------------------------------------

fn feature(id: &str, score: Option<f64>, size: i64) -> Feature {
    let mut m = HashMap::new();
    m.insert(
        "score".to_string(),
        match score {
            Some(v) => PropertyValue::Float(v),
            None => PropertyValue::Null,
        },
    );
    m.insert("size".to_string(), PropertyValue::Integer(size));
    Feature {
        id: id.into(),
        geometry: Arc::new(Geometry::Point { x: 24.0, y: 60.0 }),
        properties: Arc::new(m),
    }
}

struct SortableEngine {
    features: Vec<Feature>,
}

impl SortableEngine {
    fn new() -> Self {
        Self {
            features: vec![
                feature("mid", Some(0.5), 30),
                feature("top", Some(0.9), 10),
                feature("nul", None, 20),
                feature("low", Some(0.1), 40),
            ],
        }
    }
}

impl FeatureEngine for SortableEngine {
    fn sortables(&self) -> &[&'static str] {
        &["score", "size"]
    }

    fn get_features(&self, query: &FeatureQuery) -> Result<FeaturePage, DataServerError> {
        let mut all: Vec<Feature> = self.features.clone();
        // Before paging — the contract sortables() opts into.
        sort_features(&mut all, &query.sortby);
        let number_matched = all.len();
        let offset = query.offset.min(number_matched);
        let end = offset.saturating_add(query.limit).min(number_matched);
        let page = all[offset..end].to_vec();
        let number_returned = page.len();
        Ok(FeaturePage {
            features: page,
            number_matched,
            number_returned,
            next_offset: (end < number_matched).then_some(end),
        })
    }

    fn get_feature(&self, id: &str) -> Result<Feature, DataServerError> {
        self.features
            .iter()
            .find(|f| f.id == id)
            .cloned()
            .ok_or_else(|| DataServerError::FeatureNotFound(id.into()))
    }
}

/// Advertises no sortables — the default for every engine that has not opted
/// in. Must reject `sortby` rather than ignore it.
struct PlainEngine;

impl FeatureEngine for PlainEngine {
    fn get_features(&self, _q: &FeatureQuery) -> Result<FeaturePage, DataServerError> {
        Ok(FeaturePage {
            features: vec![],
            number_matched: 0,
            number_returned: 0,
            next_offset: None,
        })
    }
    fn get_feature(&self, id: &str) -> Result<Feature, DataServerError> {
        Err(DataServerError::FeatureNotFound(id.into()))
    }
}

fn collection(id: &str) -> CollectionConfig {
    CollectionConfig {
        id: id.to_string(),
        title: id.to_string(),
        description: String::new(),
        data_path: None,
        apis: vec!["features".to_string()],
        engine_type: "geojson".to_string(),
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
    }
}

fn build_router() -> axum::Router {
    let mut engines: HashMap<String, Arc<dyn FeatureEngine>> = HashMap::new();
    engines.insert("sortable".into(), Arc::new(SortableEngine::new()));
    engines.insert("plain".into(), Arc::new(PlainEngine));
    let mut collections = HashMap::new();
    collections.insert("sortable".to_string(), collection("sortable"));
    collections.insert("plain".to_string(), collection("plain"));
    api_features::router(Arc::new(ArcSwap::from_pointee(FeaturesState {
        engines,
        collections,
        base_url: "http://test".into(),
        trust_proxy_headers: false,
    })))
}

async fn get(uri: &str) -> (StatusCode, Value) {
    let res = build_router()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

fn ids(v: &Value) -> Vec<String> {
    v["features"]
        .as_array()
        .expect("features array")
        .iter()
        .map(|f| f["id"].as_str().unwrap_or_default().to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

const SORTABLES: &[&str] = &["score", "size"];

#[test]
fn parses_directions_including_plus_decoded_as_space() {
    assert_eq!(
        parse_sortby("score", SORTABLES).unwrap(),
        vec![SortKey::ascending("score")]
    );
    assert_eq!(
        parse_sortby("-score", SORTABLES).unwrap(),
        vec![SortKey::descending("score")]
    );
    assert_eq!(
        parse_sortby("+score", SORTABLES).unwrap(),
        vec![SortKey::ascending("score")]
    );
    // A literal '+' in a query string decodes to a space before it reaches
    // us. Rejecting this would break the spec's own `%2B`-free example.
    assert_eq!(
        parse_sortby(" score", SORTABLES).unwrap(),
        vec![SortKey::ascending("score")]
    );
}

#[test]
fn parses_multiple_terms_in_precedence_order() {
    assert_eq!(
        parse_sortby("-score,size", SORTABLES).unwrap(),
        vec![SortKey::descending("score"), SortKey::ascending("size")]
    );
}

#[test]
fn rejects_unknown_empty_and_duplicate_terms() {
    let err = parse_sortby("nope", SORTABLES).unwrap_err().to_string();
    assert!(err.contains("nope"), "must name the bad key: {err}");
    assert!(err.contains("score"), "must list valid keys: {err}");

    assert!(parse_sortby("", SORTABLES).is_err());
    assert!(parse_sortby("score,", SORTABLES).is_err());
    assert!(parse_sortby("score,score", SORTABLES).is_err());
    // Part 8 pattern: a property starts with a letter or underscore.
    assert!(parse_sortby("1score", SORTABLES).is_err());
}

#[test]
fn a_collection_with_no_sortables_says_so() {
    let err = parse_sortby("score", &[]).unwrap_err().to_string();
    assert!(
        err.contains("does not support sorting"),
        "message should explain the collection can't sort, not blame the key: {err}"
    );
}

// ---------------------------------------------------------------------------
// End to end
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sorts_ascending_and_descending_with_nulls_last() {
    let (status, body) = get("/collections/sortable/items?sortby=-score").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        ids(&body),
        ["top", "mid", "low", "nul"],
        "descending must not hoist the null score to the top"
    );

    let (_, body) = get("/collections/sortable/items?sortby=score").await;
    assert_eq!(ids(&body), ["low", "mid", "top", "nul"]);
}

#[tokio::test]
async fn sorting_is_applied_before_pagination() {
    // The bug this parameter exists to prevent: a page sorted after slicing
    // returns the wrong rows entirely.
    let (_, body) = get("/collections/sortable/items?sortby=-score&limit=2").await;
    assert_eq!(
        ids(&body),
        ["top", "mid"],
        "limit must cut the SORTED set, not the natural order"
    );
    assert_eq!(body["numberMatched"], 4, "numberMatched counts all matches");

    // And the second page continues the same ordering.
    let (_, body) = get("/collections/sortable/items?sortby=-score&limit=2&offset=2").await;
    assert_eq!(ids(&body), ["low", "nul"]);
}

#[tokio::test]
async fn multi_key_sort_end_to_end() {
    let (_, body) = get("/collections/sortable/items?sortby=-score,size").await;
    assert_eq!(ids(&body), ["top", "mid", "low", "nul"]);
}

#[tokio::test]
async fn unknown_sort_property_is_a_400_not_a_silent_pass() {
    let (status, body) = get("/collections/sortable/items?sortby=bogus").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let desc = body["description"].as_str().unwrap_or_default();
    assert!(desc.contains("bogus"), "{desc}");
    assert!(desc.contains("score"), "should list valid keys: {desc}");
}

#[tokio::test]
async fn sortby_on_a_non_sortable_collection_is_a_400() {
    let (status, body) = get("/collections/plain/items?sortby=score").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["description"]
        .as_str()
        .unwrap_or_default()
        .contains("does not support sorting"));
}

#[tokio::test]
async fn no_sortby_preserves_the_engines_natural_order() {
    let (status, body) = get("/collections/sortable/items").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        ids(&body),
        ["mid", "top", "nul", "low"],
        "absent sortby must change nothing"
    );
}

// ---------------------------------------------------------------------------
// OpenAPI conformance
// ---------------------------------------------------------------------------

/// The Features API declares `.../ogcapi-features-1/1.0/conf/oas30`, which is
/// a claim that `/api` is a valid OpenAPI 3.0 document — but that document is
/// hand-built with `serde_json::json!` and nothing checked it until now. A
/// typo in any future hand-written addition fails here instead of shipping.
#[tokio::test]
async fn api_definition_validates_against_the_openapi_30_metaschema() {
    let schema: Value = serde_json::from_str(include_str!("../../../schemas/openapi-3.0.json"))
        .expect("bundled OpenAPI 3.0 meta-schema parses");
    let validator = jsonschema::Validator::new(&schema).expect("meta-schema compiles");

    let (status, doc) = get("/api").await;
    assert_eq!(status, StatusCode::OK);

    if let Err(errors) = validator.validate(&doc) {
        let detail: Vec<String> = std::iter::once(errors)
            .map(|e| format!("{} at {}", e, e.instance_path))
            .collect();
        panic!("/api is not valid OpenAPI 3.0:\n{}", detail.join("\n"));
    }
}

/// The `sortby` parameter must be declared exactly as OGC 24-030 specifies.
/// A plain `type: string` would validate as OpenAPI but lose the normative
/// array/form/explode semantics that make the comma-separated form correct.
#[tokio::test]
async fn sortby_is_declared_with_the_schema_the_standard_requires() {
    let (_, doc) = get("/api").await;
    let p = &doc["components"]["parameters"]["sortby"];
    assert_eq!(p["name"], "sortby");
    assert_eq!(p["in"], "query");
    assert_eq!(p["schema"]["type"], "array");
    assert_eq!(p["schema"]["minItems"], 1);
    assert_eq!(p["schema"]["items"]["type"], "string");
    assert_eq!(p["schema"]["items"]["pattern"], "[+|-]?[A-Za-z_].*");
    assert_eq!(p["style"], "form");
    assert_eq!(p["explode"], false);

    // Paths are generated per collection, so assert on EVERY items path —
    // a future path builder that adds a collection and forgets the parameter
    // fails here rather than shipping an inconsistent definition.
    let paths = doc["paths"].as_object().expect("paths object");
    let items_paths: Vec<&String> = paths.keys().filter(|k| k.ends_with("/items")).collect();
    assert!(!items_paths.is_empty(), "expected at least one items path");
    for path in items_paths {
        let params = doc["paths"][path]["get"]["parameters"]
            .as_array()
            .unwrap_or_else(|| panic!("{path} has no parameters array"));
        assert!(
            params
                .iter()
                .any(|r| r["$ref"] == "#/components/parameters/sortby"),
            "{path} must reference the sortby parameter"
        );
    }
}
