//! EDR `items` query (#928): a collection's `FeatureEngine` served as a
//! GeoJSON FeatureCollection with bbox/datetime filters, EDR `limit` paging
//! and `/items/{itemId}`; advertised in `data_queries` and `/api` only for
//! collections that have a feature engine.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use chrono::{DateTime, Duration, Utc};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use api_edr::handlers::EdrState;
use ds_core::config::CollectionConfig;
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::feature::{Feature, FeaturePage, FeatureQuery, Geometry, PropertyValue};
use ds_core::feature_engine::FeatureEngine;
use ds_core::instances::RunInfo;
use ds_core::model::{CoverageResponse, Location};

/// 25 stations on a line from (20, 60) eastwards, one per 0.5°, each
/// reporting at `T0 + i hours`. Implements both traits, like the station
/// engines EDR items serves (CSV, BUFR, PostGIS, nowcast cells).
struct Stations {
    timed: bool,
    runs: bool,
}

const COUNT: usize = 25;

fn t0() -> DateTime<Utc> {
    "2026-01-01T00:00:00Z".parse().unwrap()
}

impl Stations {
    fn feature(i: usize) -> Feature {
        let mut properties = HashMap::new();
        properties.insert("name".into(), PropertyValue::String(format!("S{i}")));
        properties.insert(
            "time".into(),
            PropertyValue::String((t0() + Duration::hours(i as i64)).to_rfc3339()),
        );
        Feature {
            id: format!("s{i}"),
            geometry: Arc::new(Geometry::Point {
                x: 20.0 + 0.5 * i as f64,
                y: 60.0,
            }),
            properties: Arc::new(properties),
        }
    }
}

impl FeatureEngine for Stations {
    fn get_features(&self, query: &FeatureQuery) -> Result<FeaturePage, DataServerError> {
        let matched: Vec<Feature> = (0..COUNT)
            .filter(|&i| {
                let Geometry::Point { x, y } = *Self::feature(i).geometry else {
                    unreachable!()
                };
                let t = t0() + Duration::hours(i as i64);
                query.bbox.is_none_or(|b| b.contains(x, y))
                    && query.datetime.as_ref().is_none_or(|d| {
                        d.start.is_none_or(|s| t >= s) && d.end.is_none_or(|e| t <= e)
                    })
            })
            .map(Self::feature)
            .collect();
        let number_matched = matched.len();
        let offset = query.offset.min(number_matched);
        let features: Vec<Feature> = matched.into_iter().skip(offset).take(query.limit).collect();
        let number_returned = features.len();
        Ok(FeaturePage {
            features,
            number_matched,
            number_returned,
            next_offset: (offset + number_returned < number_matched)
                .then_some(offset + number_returned),
        })
    }

    fn get_feature(&self, feature_id: &str) -> Result<Feature, DataServerError> {
        (0..COUNT)
            .map(Self::feature)
            .find(|f| f.id == feature_id)
            .ok_or_else(|| DataServerError::FeatureNotFound(feature_id.into()))
    }

    fn has_time_dimension(&self) -> bool {
        self.timed
    }
}

impl EdrEngine for Stations {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        Ok(Vec::new())
    }

    fn query_location(
        &self,
        location_id: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Err(DataServerError::LocationNotFound(location_id.into()))
    }

    fn get_parameters(&self) -> Vec<String> {
        vec!["temperature".into()]
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        Some((t0(), t0() + Duration::hours(COUNT as i64 - 1)))
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([20.0, 60.0, 32.0, 60.0])
    }

    fn supported_query_types(&self) -> Vec<String> {
        vec!["locations".into(), "area".into()]
    }

    fn get_instances(&self) -> Vec<RunInfo> {
        if self.runs {
            vec![RunInfo {
                reference_time: t0(),
                valid_times: vec![t0()],
            }]
        } else {
            Vec::new()
        }
    }
}

fn config(id: &str) -> CollectionConfig {
    serde_json::from_value(serde_json::json!({
        "id": id, "title": format!("{id} title"), "description": "test",
        "apis": ["edr"], "engine_type": "csv"
    }))
    .unwrap()
}

/// `stations` serves items; `timeless` serves items without a time
/// dimension; `runs` has model runs and items; `grid` is EDR without a
/// feature engine.
fn router() -> axum::Router {
    let mut engines: HashMap<String, Arc<dyn EdrEngine>> = HashMap::new();
    let mut feature_engines: HashMap<String, Arc<dyn FeatureEngine>> = HashMap::new();
    let mut collections = HashMap::new();
    for (id, timed, runs, items) in [
        ("stations", true, false, true),
        ("timeless", false, false, true),
        ("runs", true, true, true),
        ("grid", true, false, false),
    ] {
        let engine = Arc::new(Stations { timed, runs });
        engines.insert(id.into(), engine.clone());
        if items {
            feature_engines.insert(id.into(), engine);
        }
        collections.insert(id.to_string(), config(id));
    }
    api_edr::router(Arc::new(ArcSwap::from_pointee(EdrState {
        engines,
        feature_engines,
        collections,
        styles: HashMap::new(),
        base_url: "https://example.test".into(),
        trust_proxy_headers: false,
    })))
}

async fn send(uri: &str, headers: &[(&str, &str)]) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut req = Request::builder().uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = router()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, headers, body)
}

async fn get(uri: &str) -> (StatusCode, Value) {
    let (status, _, body) = send(uri, &[]).await;
    (status, serde_json::from_slice(&body).unwrap())
}

fn link<'a>(doc: &'a Value, rel: &str) -> Option<&'a str> {
    doc["links"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["rel"] == rel)
        .map(|l| l["href"].as_str().unwrap())
}

fn ids(doc: &Value) -> Vec<&str> {
    doc["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn items_is_a_geojson_page_of_the_default_limit_with_paging_links() {
    let (status, headers, body) = send("/collections/stations/items", &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "application/geo+json");
    let doc: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(doc["type"], "FeatureCollection");
    assert_eq!(doc["numberMatched"], COUNT);
    assert_eq!(doc["numberReturned"], 10, "EDR's default limit is 10");
    assert_eq!(ids(&doc)[0], "s0");
    assert!(doc["timeStamp"].as_str().unwrap().ends_with('Z'));
    assert_eq!(
        link(&doc, "self").unwrap(),
        "https://example.test/edr/collections/stations/items?offset=0&limit=10"
    );
    let next = link(&doc, "next").unwrap();
    assert_eq!(
        next,
        "https://example.test/edr/collections/stations/items?offset=10&limit=10"
    );
    assert!(link(&doc, "prev").is_none());
    let feature = &doc["features"][0];
    assert_eq!(feature["type"], "Feature");
    assert_eq!(
        feature["geometry"]["coordinates"],
        serde_json::json!([20.0, 60.0])
    );
    assert_eq!(feature["properties"]["name"], "S0");
    assert_eq!(
        link(feature, "self").unwrap(),
        "https://example.test/edr/collections/stations/items/s0"
    );
    assert_eq!(
        link(feature, "collection").unwrap(),
        "https://example.test/edr/collections/stations"
    );

    // Following `next` to the end: the last page has `prev` and no `next`.
    let (status, page) = get(next.strip_prefix("https://example.test/edr").unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&page)[0], "s10");
    let (_, last) = get("/collections/stations/items?offset=20&limit=10").await;
    assert_eq!(last["numberReturned"], 5);
    assert!(link(&last, "next").is_none());
    assert!(link(&last, "prev").unwrap().contains("offset=10&limit=10"));
}

#[tokio::test]
async fn bbox_and_datetime_filter_and_ride_along_on_the_links() {
    // Stations 2..=6 lie in 21..23 °E; of those, 4..=6 report at 04:00 or later.
    let (status, doc) = get(
        "/collections/stations/items?bbox=21,59,23,61&datetime=2026-01-01T04:00:00Z/..&limit=2",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{doc}");
    assert_eq!(doc["numberMatched"], 3);
    assert_eq!(ids(&doc), ["s4", "s5"]);
    let next = link(&doc, "next").unwrap();
    assert!(
        next.ends_with("offset=2&limit=2&bbox=21,59,23,61&datetime=2026-01-01T04:00:00Z/.."),
        "{next}"
    );
    let (_, page) = get(next.strip_prefix("https://example.test/edr").unwrap()).await;
    assert_eq!(ids(&page), ["s6"]);

    // An antimeridian-crossing box is a box, not an error; it holds none here.
    let (status, doc) = get("/collections/stations/items?bbox=170,-10,-170,10").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(doc["numberMatched"], 0);
}

#[tokio::test]
async fn limit_follows_the_edr_definition() {
    let (_, one) = get("/collections/stations/items?limit=1").await;
    assert_eq!(one["numberReturned"], 1);
    // Above the maximum is the maximum (10 000), not an error.
    for limit in ["10001", "99999999999999999999999"] {
        let (status, all) = get(&format!("/collections/stations/items?limit={limit}")).await;
        assert_eq!(status, StatusCode::OK, "{limit}");
        assert_eq!(all["numberReturned"], COUNT);
        assert!(link(&all, "self").unwrap().contains("limit=10000"));
    }
    for limit in ["0", "-1", "1.5", "ten", ""] {
        let (status, err) = get(&format!("/collections/stations/items?limit={limit}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "limit={limit}");
        assert_eq!(err["code"], "BadRequest");
    }
}

#[tokio::test]
async fn unknown_repeated_and_malformed_parameters_are_400() {
    for query in [
        "sortby=name",
        "name=S1",
        "crs=http://www.opengis.net/def/crs/OGC/1.3/CRS84",
        "bbox-crs=http://www.opengis.net/def/crs/OGC/1.3/CRS84",
        "parameter-name=temperature",
        "limit=1&limit=2",
        "offset=-1",
        "bbox=1,2,3",
        "bbox=21,61,23,59",
        "datetime=yesterday",
        "datetime=2026-01-02T00:00:00Z/2026-01-01T00:00:00Z",
        "f=html",
        "f=CoverageJSON",
    ] {
        let (status, err) = get(&format!("/collections/stations/items?{query}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {err}");
        assert_eq!(err["code"], "BadRequest", "{query}");
    }
    let (_, err) = get("/collections/stations/items?sortby=name").await;
    let description = err["description"].as_str().unwrap();
    assert!(
        description.contains("'sortby'")
            && description.contains("bbox, datetime, limit, offset, f"),
        "{description}"
    );
    for f in ["GeoJSON", "geojson", "application/geo%2Bjson", "json"] {
        let (status, _) = get(&format!("/collections/stations/items?f={f}")).await;
        assert_eq!(status, StatusCode::OK, "f={f}");
    }
}

#[tokio::test]
async fn datetime_on_a_collection_without_time_is_400() {
    let (status, err) = get("/collections/timeless/items?datetime=2026-01-01T00:00:00Z").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(err["description"]
        .as_str()
        .unwrap()
        .contains("no time dimension"));
    let (status, _) = get("/collections/timeless/items?bbox=19,59,33,61").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_single_item_is_a_feature_and_an_unknown_one_404() {
    let (status, headers, body) = send("/collections/stations/items/s3", &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "application/geo+json");
    let feature: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(feature["type"], "Feature");
    assert_eq!(feature["id"], "s3");
    assert_eq!(
        feature["geometry"]["coordinates"],
        serde_json::json!([21.5, 60.0])
    );

    let (status, err) = get("/collections/stations/items/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(err["code"], "NotFound");
    for query in ["limit=1", "f=html", "f=json&f=json"] {
        let (status, _) = get(&format!("/collections/stations/items/s3?{query}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
    }
    let (status, _) = get("/collections/stations/items/s3?f=GeoJSON").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn items_exist_only_where_the_engine_serves_features() {
    for uri in [
        "/collections/grid/items",
        "/collections/grid/items/s1",
        "/collections/missing/items",
        "/collections/missing/items/s1",
    ] {
        let (status, err) = get(uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(err["code"], "NotFound", "{uri}");
    }
    let (_, grid) = get("/collections/grid").await;
    assert!(grid["data_queries"].get("items").is_none(), "{grid}");
    let (_, api) = get("/api").await;
    assert!(api["paths"]["/edr/collections/grid/items"].is_null());
    assert!(api["paths"]["/edr/collections/grid/items/{itemId}"].is_null());
}

#[tokio::test]
async fn items_is_advertised_in_data_queries_with_the_edr_1_2_link_variables() {
    let (status, doc) = get("/collections/stations").await;
    assert_eq!(status, StatusCode::OK);
    let items = &doc["data_queries"]["items"]["link"];
    assert_eq!(
        items["href"],
        "https://example.test/edr/collections/stations/items"
    );
    assert_eq!(items["rel"], "data");
    let variables = &items["variables"];
    assert_eq!(variables["query_type"], "items");
    assert_eq!(variables["title"], "Items query");
    assert!(variables["description"]
        .as_str()
        .is_some_and(|d| !d.is_empty()));
    assert_eq!(variables["output_formats"], serde_json::json!(["GeoJSON"]));
    assert_eq!(variables["default_output_format"], "GeoJSON");
    let crs = &variables["crs_details"][0];
    assert_eq!(crs["crs"], "CRS84");
    let wkt = crs["wkt"].as_str().unwrap();
    assert!(wkt.ends_with(r#"ID["OGC","CRS84"]]"#), "{wkt}");
    assert!(
        wkt.find("longitude").unwrap() < wkt.find("latitude").unwrap(),
        "CRS84 is longitude first: {wkt}"
    );
    // Not a Features resource: no `itemType`, no `rel=items` link.
    assert!(doc.get("itemType").is_none());
    assert!(link(&doc, "items").is_none());

    // The collection list agrees; a model run's document does not offer it.
    let (_, list) = get("/collections").await;
    let listed = list["collections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "stations")
        .unwrap();
    assert_eq!(
        listed["data_queries"]["items"],
        doc["data_queries"]["items"]
    );
    let (_, runs) = get("/collections/runs").await;
    assert!(runs["data_queries"]["items"].is_object());
    let (status, instance) = get("/collections/runs/instances/20260101T0000Z").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        instance["data_queries"].get("items").is_none(),
        "{instance}"
    );
}

/// The collection documents carrying `data_queries.items` still validate
/// against the bundled EDR 1.1 schema, whose `items` link it defines.
#[tokio::test]
async fn collection_documents_with_items_validate_against_edr() {
    let bundle: Value =
        serde_json::from_str(include_str!("../../../schemas/ogcapi-edr-1.1-bundled.json")).unwrap();
    for (uri, path) in [
        ("/collections", "/collections"),
        ("/collections/stations", "/collections/{collectionId}"),
    ] {
        let (status, doc) = get(uri).await;
        assert_eq!(status, StatusCode::OK);
        let schema = &bundle["paths"][path]["get"]["responses"]["200"]["content"]
            ["application/json"]["schema"];
        let validator = jsonschema::Validator::new(schema).unwrap();
        let errors: Vec<String> = validator
            .iter_errors(&doc)
            .map(|e| format!("{e} at {}", e.instance_path()))
            .collect();
        assert!(errors.is_empty(), "{uri}: {}", errors.join("\n"));
    }
}

#[tokio::test]
async fn openapi_documents_the_items_paths_with_the_standard_parameters() {
    let (_, api) = get("/api").await;
    let list = &api["paths"]["/edr/collections/stations/items"]["get"];
    assert_eq!(list["operationId"], "getItems_stations");
    assert_eq!(list["tags"], serde_json::json!(["stations"]));
    let single = &api["paths"]["/edr/collections/stations/items/{itemId}"]["get"];
    assert_eq!(single["operationId"], "getItem_stations");
    assert_eq!(single["parameters"][0]["name"], "itemId");

    let parameters = &api["components"]["parameters"];
    let resolve = |p: &Value| -> Value {
        match p["$ref"].as_str() {
            Some(r) => api
                .pointer(r.strip_prefix('#').unwrap())
                .unwrap_or_else(|| panic!("dangling {r}"))
                .clone(),
            None => p.clone(),
        }
    };
    let names: Vec<Value> = list["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| resolve(p)["name"].clone())
        .collect();
    assert_eq!(
        names,
        serde_json::json!(["bbox", "datetime", "limit", "offset", "f"])
            .as_array()
            .unwrap()
            .clone()
    );
    // /req/edr/rc-limit-definition, verbatim.
    let limit = &parameters["items-limit"];
    assert_eq!(
        limit["schema"],
        serde_json::json!({"type": "integer", "minimum": 1, "maximum": 10000, "default": 10})
    );
    for p in ["items-limit", "items-bbox", "items-datetime"] {
        assert_eq!(parameters[p]["style"], "form", "{p}");
        assert_eq!(parameters[p]["explode"], false, "{p}");
        assert_eq!(parameters[p]["required"], false, "{p}");
    }
    assert_eq!(
        parameters["items-bbox"]["schema"]["oneOf"][1]["minItems"],
        6
    );

    // The whole document is valid OpenAPI 3.0 and every reference resolves.
    let schema: Value =
        serde_json::from_str(include_str!("../../../schemas/openapi-3.0.json")).unwrap();
    let validator = jsonschema::Validator::new(&schema).unwrap();
    let errors: Vec<String> = validator
        .iter_errors(&api)
        .map(|e| format!("{e} at {}", e.instance_path()))
        .collect();
    assert!(errors.is_empty(), "{}", errors.join("\n"));
    let text = api.to_string();
    for reference in text.split("\"$ref\":\"#").skip(1) {
        let pointer = reference.split('"').next().unwrap();
        assert!(api.pointer(pointer).is_some(), "dangling $ref #{pointer}");
    }
}

#[tokio::test]
async fn the_etag_ignores_the_generation_timestamp_and_revalidates() {
    let uri = "/collections/stations/items?limit=3";
    let (_, first, _) = send(uri, &[]).await;
    let etag = first[header::ETAG].to_str().unwrap().to_owned();
    let (_, second, _) = send(uri, &[]).await;
    assert_eq!(second[header::ETAG], etag.as_str());
    let (status, _, body) = send(uri, &[("if-none-match", &etag)]).await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert!(body.is_empty());
    // A settled window gets the long policy; an open one the short.
    let (_, settled, _) = send(
        "/collections/stations/items?datetime=2026-01-01T00:00:00Z/2026-01-01T06:00:00Z",
        &[],
    )
    .await;
    assert!(settled[header::CACHE_CONTROL]
        .to_str()
        .unwrap()
        .contains("86400"));
    assert!(first[header::CACHE_CONTROL]
        .to_str()
        .unwrap()
        .contains("max-age=60"));
}
