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
use ds_core::edr_engine::{EdrEngine, ItemRadius};
use ds_core::error::DataServerError;
use ds_core::feature::{Feature, FeaturePage, FeatureQuery, Geometry, PropertyValue};
use ds_core::feature_engine::FeatureEngine;
use ds_core::instances::RunInfo;
use ds_core::model::{CoverageResponse, Location};

#[path = "support/edr_schema.rs"]
mod edr_schema;

/// 25 stations on a line from (20, 60) eastwards, one per 0.5°, each
/// reporting at `T0 + i hours`. Implements both traits, like the station
/// engines EDR items serves (CSV, BUFR, PostGIS, nowcast cells).
struct Stations {
    timed: bool,
    runs: bool,
    /// The EDR query types: a collection without `locations` has items that
    /// are no locations, as the nowcast's cells.
    queries: &'static [&'static str],
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
    /// Every station is a location, labelled unlike its `name` property.
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        Ok((0..COUNT)
            .map(|i| Location {
                id: format!("s{i}"),
                label: format!("Station {i}"),
                latitude: 60.0,
                longitude: 20.0 + 0.5 * i as f64,
            })
            .collect())
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
        self.queries.iter().map(|q| q.to_string()).collect()
    }

    /// Every item is a 2.34 km circle at its own report time, as a nowcast
    /// sizes its cells; used only where `radius` is served.
    fn item_radius(&self, feature: &Feature) -> Option<ItemRadius> {
        let Some(PropertyValue::String(time)) = feature.properties.get("time") else {
            return None;
        };
        Some(ItemRadius {
            within_km: 2.34,
            datetime: Some(time.parse().unwrap()),
        })
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
/// feature engine; `cells`, `points` and `areas` serve items but no
/// locations, `cells` a radius query and `points` a position query.
fn router() -> axum::Router {
    let mut engines: HashMap<String, Arc<dyn EdrEngine>> = HashMap::new();
    let mut feature_engines: HashMap<String, Arc<dyn FeatureEngine>> = HashMap::new();
    let mut collections = HashMap::new();
    const LOCATED: &[&str] = &["locations", "area"];
    for (id, timed, runs, items, queries) in [
        ("stations", true, false, true, LOCATED),
        ("timeless", false, false, true, LOCATED),
        ("runs", true, true, true, LOCATED),
        ("grid", true, false, false, LOCATED),
        ("cells", true, false, true, &["area", "radius"][..]),
        ("points", true, false, true, &["position"][..]),
        ("areas", true, false, true, &["area"][..]),
    ] {
        let engine = Arc::new(Stations {
            timed,
            runs,
            queries,
        });
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

/// `/req/edr-geojson/content` A: every item is an EDR GeoJSON feature. Its
/// properties gain the `edrProperties` members, with the values the
/// station's `/locations` feature carries, and keep the engine's own.
#[tokio::test]
async fn items_are_edr_geojson_features_of_their_locations() {
    let (_, locations) = get("/collections/stations/locations").await;
    let (status, doc) = get("/collections/stations/items?limit=25").await;
    assert_eq!(status, StatusCode::OK);
    edr_schema::assert_valid(
        "/collections/{collectionId}/items",
        edr_schema::GEOJSON,
        &doc,
        "items",
    );
    let features = doc["features"].as_array().unwrap();
    assert_eq!(features.len(), COUNT);
    for (i, feature) in features.iter().enumerate() {
        let properties = &feature["properties"];
        let location = &locations["features"][i]["properties"];
        assert_eq!(properties["label"], format!("Station {i}"));
        assert_eq!(
            properties["edrqueryendpoint"],
            format!("https://example.test/edr/collections/stations/locations/s{i}")
        );
        for member in ["datetime", "parameter-name", "label", "edrqueryendpoint"] {
            assert_eq!(properties[member], location[member], "s{i} {member}");
        }
        assert_eq!(
            properties["parameter-name"],
            serde_json::json!(["temperature"])
        );
        assert_eq!(
            properties["datetime"],
            "2026-01-01T00:00:00+00:00/2026-01-02T00:00:00+00:00"
        );
        // The engine's own properties stay.
        assert_eq!(properties["name"], format!("S{i}"));
        assert!(properties["time"].is_string());
    }

    let (status, item) = get("/collections/stations/items/s3").await;
    assert_eq!(status, StatusCode::OK);
    edr_schema::assert_valid(edr_schema::ITEM, edr_schema::GEOJSON, &item, "item");
    assert_eq!(item["properties"], features[3]["properties"]);

    // The members follow the station, not the page or the filter.
    let (_, filtered) = get("/collections/stations/items?bbox=21,59,23,61&limit=1").await;
    assert_eq!(
        filtered["features"][0]["properties"],
        features[2]["properties"]
    );

    // The Features encoding without them fails the EDR schema: the check
    // above is not vacuous.
    let mut bare = item.clone();
    bare["properties"]
        .as_object_mut()
        .unwrap()
        .remove("edrqueryendpoint");
    for version in edr_schema::VERSIONS {
        assert!(
            !edr_schema::errors(version, edr_schema::ITEM, edr_schema::GEOJSON, &bare).is_empty(),
            "{version:?}"
        );
    }
}

/// An item that is no location still carries the members: its `name` as
/// `label`; the radius query the engine sizes for it, at its own time,
/// where the collection answers radius; else a position query at its point;
/// else the collection itself, with the collection's temporal extent.
#[tokio::test]
async fn items_that_are_no_locations_name_another_query() {
    let extent = "2026-01-01T00:00:00+00:00/2026-01-02T00:00:00+00:00";
    for (collection, endpoint, datetime) in [
        (
            "cells",
            "https://example.test/edr/collections/cells/radius?coords=POINT(21.5%2060)&within=2.4&within-units=km",
            "2026-01-01T03:00:00Z",
        ),
        (
            "points",
            "https://example.test/edr/collections/points/position?coords=POINT(21.5%2060)",
            extent,
        ),
        ("areas", "https://example.test/edr/collections/areas", extent),
    ] {
        let (status, item) = get(&format!("/collections/{collection}/items/s3")).await;
        assert_eq!(status, StatusCode::OK, "{item}");
        edr_schema::assert_valid(edr_schema::ITEM, edr_schema::GEOJSON, &item, collection);
        assert_eq!(item["properties"]["label"], "S3", "{collection}");
        assert_eq!(item["properties"]["edrqueryendpoint"], endpoint);
        assert_eq!(item["properties"]["datetime"], datetime, "{collection}");
        let (_, page) = get(&format!("/collections/{collection}/items?limit=25")).await;
        edr_schema::assert_valid(
            "/collections/{collectionId}/items",
            edr_schema::GEOJSON,
            &page,
            collection,
        );
        assert_eq!(page["features"][3]["properties"], item["properties"]);
    }
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
        "f=PNG",
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
    for query in ["limit=1", "f=PNG", "f=json&f=json"] {
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
    assert_eq!(
        variables["output_formats"],
        serde_json::json!(["GeoJSON", "HTML"])
    );
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

/// The collection documents carrying `data_queries.items` validate against
/// the bundled EDR 1.1 and 1.2 schemas, both of which define the `items`
/// link; 1.2 also requires its link variables.
#[tokio::test]
async fn collection_documents_with_items_validate_against_edr() {
    for (uri, path) in [
        ("/collections", "/collections"),
        ("/collections/stations", "/collections/{collectionId}"),
    ] {
        let (status, doc) = get(uri).await;
        assert_eq!(status, StatusCode::OK);
        edr_schema::assert_valid(path, edr_schema::JSON, &doc, uri);
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
