//! `bbox` and `datetime` on `GET /collections/{id}/locations` (#932).
//!
//! `bbox` keeps the locations inside the box, edges included, CRS84, with
//! `west > east` crossing the antimeridian. `datetime`, in the data queries'
//! grammar, keeps the locations the engine's `location_time_filter` says
//! have an observation in it: an interval with its open ends unbounded, or
//! each instant of a list matched exactly. Both filter before `limit` pages
//! the list, so `numberMatched` and the paging links describe the filtered
//! list, and the links carry both. A six-number box's heights are validated
//! and ignored. An engine without the hook answers `datetime` with a 400.
//! Malformed, reversed or repeated values are a 400 naming the parameter,
//! before the engine is read.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, TimeZone, Utc};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use api_edr::handlers::EdrState;
use ds_core::config::CollectionConfig;
use ds_core::edr_engine::{EdrEngine, LocationFilter};
use ds_core::error::DataServerError;
use ds_core::feature::DatetimeInterval;
use ds_core::model::*;

#[path = "support/edr_schema.rs"]
mod edr_schema;

const LOCATIONS_PATH: &str = "/collections/{collectionId}/locations";
const ROOT: &str = "https://example.test/edr/collections/obs/locations";

/// The inventory, in engine order: two Nordic stations and a ring of points
/// around the antimeridian seam box `170,10,-170,20`, each with the hours of
/// 2026-06-01 at which it has an observation.
const STATIONS: [(&str, f64, f64, &[u32]); 11] = [
    ("hel", 24.94, 60.17, &[0, 6]),
    ("w165", 165.0, 15.0, &[0]),
    ("w175", 175.0, 15.0, &[6]),
    ("corner", 170.0, 10.0, &[0, 12]),
    ("sto", 18.07, 59.33, &[12]),
    ("p180", 180.0, 12.0, &[6]),
    ("zero", 0.0, 15.0, &[]),
    ("m180", -180.0, 18.0, &[0, 6, 12]),
    ("e175", -175.0, 15.0, &[12]),
    ("e165", -165.0, 15.0, &[6]),
    ("n175", 175.0, 25.0, &[0]),
];

/// The stations inside the seam box, in engine order.
const SEAM: [&str; 5] = ["w175", "corner", "p180", "m180", "e175"];

fn hour(h: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 1, h, 0, 0).unwrap()
}

/// `timed`: the engine implements `location_time_filter` over the station
/// hours (the `obs` collection); otherwise it keeps the default (`plain`).
struct Stations {
    timed: bool,
    inventory_reads: Arc<AtomicUsize>,
}

impl EdrEngine for Stations {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        self.inventory_reads.fetch_add(1, Ordering::Relaxed);
        Ok(STATIONS
            .iter()
            .map(|&(id, longitude, latitude, _)| Location {
                id: id.into(),
                label: id.to_uppercase(),
                latitude,
                longitude,
            })
            .collect())
    }

    fn location_time_filter<'a>(
        &'a self,
        intervals: &'a [DatetimeInterval],
    ) -> Option<LocationFilter<'a>> {
        self.timed.then(|| -> LocationFilter<'a> {
            Box::new(move |location| {
                let (_, _, _, hours) = STATIONS
                    .iter()
                    .find(|(id, ..)| *id == location.id)
                    .expect("a listed station");
                hours.iter().map(|&h| hour(h)).any(|t| {
                    intervals.iter().any(|i| {
                        i.start.is_none_or(|start| t >= start) && i.end.is_none_or(|end| t <= end)
                    })
                })
            })
        })
    }

    fn query_location(
        &self,
        location_id: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Err(DataServerError::LocationNotFound(location_id.into()))
    }

    fn get_parameters(&self) -> Vec<String> {
        vec!["temperature".into()]
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        Some((hour(0), hour(12)))
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([-180.0, 10.0, 180.0, 61.0])
    }

    fn supported_query_types(&self) -> Vec<String> {
        vec!["locations".into()]
    }
}

struct App {
    router: axum::Router,
    inventory_reads: Arc<AtomicUsize>,
}

fn collection(id: &str) -> CollectionConfig {
    CollectionConfig {
        id: id.into(),
        title: "Observations".into(),
        description: "locations filter fixture".into(),
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
    }
}

fn app() -> App {
    let inventory_reads = Arc::new(AtomicUsize::new(0));
    let engine = |timed| -> Arc<dyn EdrEngine> {
        Arc::new(Stations {
            timed,
            inventory_reads: inventory_reads.clone(),
        })
    };
    let state = Arc::new(ArcSwap::from_pointee(EdrState {
        engines: HashMap::from([
            ("obs".to_string(), engine(true)),
            ("plain".to_string(), engine(false)),
        ]),
        collections: HashMap::from([
            ("obs".to_string(), collection("obs")),
            ("plain".to_string(), collection("plain")),
        ]),
        styles: HashMap::new(),
        feature_engines: HashMap::new(),
        base_url: "https://example.test".into(),
        trust_proxy_headers: false,
    }));
    App {
        router: api_edr::router(state),
        inventory_reads,
    }
}

impl App {
    /// `/collections/obs/locations?{query}`.
    async fn get(&self, query: &str) -> (StatusCode, Value) {
        self.get_uri(&format!("/collections/obs/locations?{query}"))
            .await
    }

    async fn get_uri(&self, uri: &str) -> (StatusCode, Value) {
        let response = self
            .router
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&body)
            .unwrap_or_else(|_| panic!("{uri}: non-JSON {status} body"));
        (status, json)
    }

    /// A 200 that validates against the EDR 1.1 and 1.2 bundles.
    async fn ok(&self, query: &str) -> Value {
        let (status, json) = self.get(query).await;
        assert_eq!(status, StatusCode::OK, "{query}: {json}");
        edr_schema::assert_valid(LOCATIONS_PATH, edr_schema::GEOJSON, &json, query);
        json
    }

    fn inventory_reads(&self) -> usize {
        self.inventory_reads.load(Ordering::Relaxed)
    }
}

fn ids(json: &Value) -> Vec<&str> {
    json["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_str().unwrap())
        .collect()
}

fn link<'a>(json: &'a Value, rel: &str) -> Option<&'a str> {
    json["links"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["rel"] == rel)
        .map(|l| l["href"].as_str().unwrap())
}

#[tokio::test]
async fn bbox_keeps_the_locations_inside_it() {
    let app = app();
    let json = app.ok("bbox=10,55,30,65").await;
    assert_eq!(ids(&json), ["hel", "sto"]);
    let json = app.ok("bbox=20,55,30,65").await;
    assert_eq!(ids(&json), ["hel"]);
    // The whole world is the whole inventory.
    let json = app.ok("bbox=-180,-90,180,90").await;
    assert_eq!(ids(&json).len(), STATIONS.len());
}

#[tokio::test]
async fn bbox_with_no_location_inside_is_an_empty_list() {
    let app = app();
    let json = app.ok("bbox=-10,-10,0,0").await;
    assert!(ids(&json).is_empty(), "{json}");
    assert_eq!(json["type"], "FeatureCollection");
}

/// `west > east` crosses the antimeridian: the box covers 170°E…180° and
/// −180°…170°W, both seams included, and nothing between 170°W and 170°E.
#[tokio::test]
async fn bbox_across_the_antimeridian_keeps_both_sides_of_the_seam() {
    let app = app();
    let json = app.ok("bbox=170,10,-170,20").await;
    assert_eq!(ids(&json), SEAM);
    // Edges are inside: the box's south-west corner station is listed.
    let json = app.ok("bbox=170,10,175,15").await;
    assert_eq!(ids(&json), ["w175", "corner"]);
}

/// Six numbers: the vertical pair is the 1.2 schema's optional third axis.
/// Locations have no height, so it is checked and otherwise ignored.
#[tokio::test]
async fn six_number_bbox_ignores_its_heights() {
    let app = app();
    let four = app.ok("bbox=170,10,-170,20").await;
    let six = app.ok("bbox=170,10,-500,-170,20,9000").await;
    assert_eq!(ids(&six), SEAM);
    assert_eq!(ids(&six), ids(&four));
    assert_eq!(
        link(&six, "self"),
        Some(&*format!("{ROOT}?bbox=170,10,-500,-170,20,9000"))
    );
}

/// An unpaged filtered list names its query in `self` and carries no counts;
/// an unfiltered one is the complete inventory's body as before.
#[tokio::test]
async fn unpaged_filtered_list_links_to_itself() {
    let app = app();
    let json = app.ok("bbox=10,55,30,65&f=GeoJSON").await;
    assert!(json.get("numberMatched").is_none());
    assert!(json.get("numberReturned").is_none());
    assert_eq!(
        json["links"],
        serde_json::json!([{
            "href": format!("{ROOT}?bbox=10,55,30,65&f=GeoJSON"),
            "rel": "self",
            "title": "Locations",
            "type": "application/geo+json"
        }])
    );
    let json = app.ok("f=GeoJSON").await;
    assert_eq!(link(&json, "self"), Some(ROOT));
    assert_eq!(json["links"].as_array().unwrap().len(), 1);
}

/// The box filters first and `limit` pages what is left: `numberMatched`
/// counts the locations inside the box and the links carry the box.
#[tokio::test]
async fn bbox_filters_before_paging() {
    let app = app();
    let first = app.ok("bbox=170,10,-170,20&limit=2").await;
    assert_eq!(ids(&first), ["w175", "corner"]);
    assert_eq!(first["numberMatched"], SEAM.len());
    assert_eq!(first["numberReturned"], 2);
    let base = format!("{ROOT}?bbox=170,10,-170,20&limit=2");
    assert_eq!(link(&first, "self"), Some(&*base));
    assert_eq!(link(&first, "prev"), None);
    assert_eq!(link(&first, "next"), Some(&*format!("{base}&offset=2")));

    let mut pages = vec![ids(&first).join(",")];
    let mut next = link(&first, "next").map(str::to_owned);
    let mut last = first.clone();
    while let Some(href) = next {
        let query = href.split_once('?').unwrap().1;
        last = app.ok(query).await;
        assert_eq!(last["numberMatched"], SEAM.len(), "{href}");
        pages.push(ids(&last).join(","));
        next = link(&last, "next").map(str::to_owned);
    }
    assert_eq!(pages, ["w175,corner", "p180,m180", "e175"]);
    assert_eq!(last["numberReturned"], 1);
    assert_eq!(link(&last, "prev"), Some(&*format!("{base}&offset=2")));

    // Past the filtered list's end: an empty page with nowhere to go.
    let empty = app.ok("bbox=170,10,-170,20&limit=2&offset=5").await;
    assert!(ids(&empty).is_empty());
    assert_eq!(empty["numberMatched"], SEAM.len());
    assert!(link(&empty, "next").is_none() && link(&empty, "prev").is_none());

    // Without the box the same page size walks the whole inventory.
    let all = app.ok("limit=2").await;
    assert_eq!(all["numberMatched"], STATIONS.len());
}

/// `datetime` keeps the stations with an observation in it, in every form
/// of the data queries' grammar: an instant matched exactly, an interval,
/// open ends, and a list whose instants each match exactly.
#[tokio::test]
async fn datetime_keeps_the_locations_with_an_observation_in_it() {
    let app = app();
    for (datetime, expected) in [
        (
            "2026-06-01T06:00:00Z",
            &["hel", "w175", "p180", "m180", "e165"][..],
        ),
        // Inside every station's span, but nobody observed at 03:00.
        ("2026-06-01T03:00:00Z", &[]),
        (
            "2026-06-01T05:00:00Z/2026-06-01T07:00:00Z",
            &["hel", "w175", "p180", "m180", "e165"],
        ),
        ("2026-06-01T01:00:00Z/2026-06-01T05:00:00Z", &[]),
        (
            "../2026-06-01T00:00:00Z",
            &["hel", "w165", "corner", "m180", "n175"],
        ),
        (
            "2026-06-01T12:00:00Z/..",
            &["corner", "sto", "m180", "e175"],
        ),
        (
            "../..",
            &[
                "hel", "w165", "w175", "corner", "sto", "p180", "m180", "e175", "e165", "n175",
            ],
        ),
        (
            "2026-06-01T12:00:00Z,2026-06-01T00:00:00Z",
            &["hel", "w165", "corner", "sto", "m180", "e175", "n175"],
        ),
        // A list matches each instant exactly, not the span between them.
        ("2026-06-01T03:00:00Z,2026-06-01T09:00:00Z", &[]),
    ] {
        let json = app.ok(&format!("datetime={datetime}")).await;
        assert_eq!(ids(&json), expected, "{datetime}");
        assert_eq!(
            link(&json, "self"),
            Some(&*format!("{ROOT}?datetime={datetime}")),
            "{datetime}"
        );
        assert!(json.get("numberMatched").is_none(), "{datetime}");
    }
}

/// `bbox` and `datetime` both filter before `limit` pages the list: the
/// counts describe the stations inside the box with an observation in the
/// window, and every link carries both filters.
#[tokio::test]
async fn bbox_and_datetime_filter_before_paging() {
    let app = app();
    let query = "bbox=170,10,-170,20&datetime=2026-06-01T06:00:00Z&limit=2";
    let first = app.ok(query).await;
    assert_eq!(ids(&first), ["w175", "p180"]);
    assert_eq!(first["numberMatched"], 3);
    assert_eq!(first["numberReturned"], 2);
    let base = format!("{ROOT}?{query}");
    assert_eq!(link(&first, "self"), Some(&*base));
    let next = link(&first, "next").unwrap();
    assert_eq!(next, format!("{base}&offset=2"));

    let last = app.ok(next.split_once('?').unwrap().1).await;
    assert_eq!(ids(&last), ["m180"]);
    assert_eq!(last["numberMatched"], 3);
    assert_eq!(last["numberReturned"], 1);
    assert_eq!(link(&last, "next"), None);
    assert_eq!(link(&last, "prev"), Some(&*base));

    // A list inside the box: each instant exactly, then paged.
    let json = app
        .ok("bbox=170,10,-170,20&datetime=2026-06-01T00:00:00Z,2026-06-01T12:00:00Z&limit=5")
        .await;
    assert_eq!(ids(&json), ["corner", "m180", "e175"]);
    assert_eq!(json["numberMatched"], 3);
    assert_eq!(link(&json, "next"), None);
}

/// An engine that cannot tell when a location has data answers `datetime`
/// with a 400 naming the collection, not with its unfiltered list.
#[tokio::test]
async fn datetime_on_an_engine_without_location_times_is_400() {
    let app = app();
    let (status, json) = app
        .get_uri("/collections/plain/locations?datetime=2026-06-01T06:00:00Z&limit=2")
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(json["code"], "BadRequest");
    let description = json["description"].as_str().unwrap();
    assert!(
        description.contains("'plain'") && description.contains("per-location time"),
        "{description}"
    );
    // The same collection lists, and filters by box, without it.
    let (status, json) = app
        .get_uri("/collections/plain/locations?bbox=10,55,30,65")
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&json), ["hel", "sto"]);
}

/// A reversed interval selects nothing and is a 400 on `/locations` and on
/// the data queries alike, as Features, Maps and Tiles answer it.
#[tokio::test]
async fn reversed_interval_is_400() {
    let app = app();
    let reversed = "2026-06-01T12:00:00Z/2026-06-01T00:00:00Z";
    for uri in [
        format!("/collections/obs/locations?datetime={reversed}"),
        format!("/collections/plain/locations?datetime={reversed}"),
        format!("/collections/obs/locations/hel?datetime={reversed}"),
    ] {
        let (status, json) = app.get_uri(&uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {json}");
        let description = json["description"].as_str().unwrap();
        assert!(
            description.contains("datetime") && description.contains("ends before it starts"),
            "{uri}: {description}"
        );
    }
    assert_eq!(app.inventory_reads(), 0);
}

#[tokio::test]
async fn malformed_or_repeated_filters_are_400_before_the_inventory_is_read() {
    let app = app();
    for (query, names) in [
        ("bbox=10,55,30", "bbox"),
        ("bbox=10,55,30,65,1", "bbox"),
        ("bbox=10,55,30,sixty", "bbox"),
        ("bbox=10,55,30,65,NaN,1", "bbox"),
        ("bbox=", "bbox"),
        ("bbox=10,65,30,55", "bbox"),
        ("bbox=10,55,190,65", "bbox"),
        ("bbox=10,-95,30,65", "bbox"),
        ("bbox=10,55,30,65&bbox=0,0,1,1", "'bbox'"),
        ("datetime=yesterday", "datetime"),
        ("datetime=", "datetime"),
        ("datetime=..", "datetime"),
        ("datetime=2026-06-01T00:00:00Z/tomorrow", "datetime"),
        (
            "datetime=2026-06-01T00:00:00Z,2026-06-01T01:00:00Z/..",
            "datetime",
        ),
        (
            "datetime=2026-06-01T00:00:00Z&datetime=2026-06-02T00:00:00Z",
            "'datetime'",
        ),
        ("bbox=10,55,30,65&limit=0", "limit"),
        // Unknown parameters stay 400s (#934), bbox-crs included.
        ("bbox-crs=EPSG:4326", "bbox-crs"),
    ] {
        let (status, json) = app.get(query).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {json}");
        assert_eq!(json["code"], "BadRequest", "{query}");
        let description = json["description"].as_str().unwrap();
        assert!(
            description.to_lowercase().contains(&names.to_lowercase()),
            "{query}: {description}"
        );
    }
    assert_eq!(app.inventory_reads(), 0);
}

/// `/api` lists `bbox` and `datetime` on `/locations` with the 1.2
/// OpenAPI's schema, `style` and `explode`, and stays valid OpenAPI 3.0.
#[tokio::test]
async fn openapi_declares_bbox_and_datetime_on_locations() {
    let app = app();
    let response = app
        .router
        .clone()
        .oneshot(Request::builder().uri("/api").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let api: Value = serde_json::from_slice(&body).unwrap();

    let meta: Value =
        serde_json::from_str(include_str!("../../../schemas/openapi-3.0.json")).unwrap();
    let validator = jsonschema::Validator::new(&meta).unwrap();
    let errors: Vec<String> = validator
        .iter_errors(&api)
        .map(|e| format!("{e} at {}", e.instance_path()))
        .collect();
    assert!(errors.is_empty(), "{errors:?}");

    let bundle: Value = serde_json::from_str(include_str!(
        "../../../schemas/ogcapi-edr-1.2-oas30-bundled.json"
    ))
    .unwrap();
    let parameters: Vec<Value> = api["paths"]["/edr/collections/obs/locations"]["get"]
        ["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| match p["$ref"].as_str() {
            Some(r) => api.pointer(r.strip_prefix('#').unwrap()).unwrap().clone(),
            None => p.clone(),
        })
        .collect();
    for name in ["bbox", "datetime"] {
        let ours = parameters
            .iter()
            .find(|p| p["name"] == name)
            .unwrap_or_else(|| panic!("/locations: no {name}"));
        let theirs = &bundle["components"]["parameters"][name];
        for key in ["in", "required", "schema", "style", "explode"] {
            assert_eq!(ours[key], theirs[key], "{name}.{key}");
        }
    }
}
