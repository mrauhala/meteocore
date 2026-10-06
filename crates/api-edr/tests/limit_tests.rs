//! EDR 1.2 `limit` on data queries and paging of `/locations` (#922).
//!
//! `/req/edr/rc-limit-definition` + `/req/edr/REQ_rc-limit-response`: an
//! integer from 1 to 10000, a larger value clamped rather than rejected, and
//! at most `limit` top-level objects in the response. For CoverageJSON that
//! is the coverages of a CoverageCollection; a single Coverage is one object.
//! `/locations` pages with `limit` + `offset` and, without `limit`, stays the
//! complete inventory (#533).

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
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::instances::RunInfo;
use ds_core::model::*;

const LOCATIONS: usize = 5;
/// Coverages the mock's area query returns.
const AREA_COVERAGES: usize = 4;
const RUN: &str = "2026-06-07T06:00:00Z";

fn run_time() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 7, 6, 0, 0).unwrap()
}

/// A PointSeries coverage at `(x, y)`; `x` identifies it in assertions.
fn coverage(x: f64, y: f64) -> QueryResult {
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
            shape: vec![1],
            axis_names: vec!["t".into()],
            values: vec![Some(1.0)],
        },
    );
    QueryResult {
        domain: DomainDescription::PointSeries {
            x,
            y,
            t: vec![run_time()],
            z: None,
        },
        parameters,
        ranges,
    }
}

/// Answers every query type `limit` applies to, plus one model run so the
/// instance routes exist. `per_point` coverages come back for each position
/// point (1 = a single Coverage, more = a CoverageCollection, as a vertical
/// profile per step would).
struct LimitMock {
    per_point: usize,
    position_calls: Arc<AtomicUsize>,
}

impl LimitMock {
    fn check_run(reference_time: Option<DateTime<Utc>>) -> Result<(), DataServerError> {
        match reference_time {
            Some(rt) if rt != run_time() => {
                Err(DataServerError::ReferenceTimeNotFound(rt.to_rfc3339()))
            }
            _ => Ok(()),
        }
    }
}

impl EdrEngine for LimitMock {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        Ok((0..LOCATIONS)
            .map(|i| Location {
                id: format!("s{i}"),
                label: format!("Station {i}"),
                latitude: 60.0 + i as f64,
                longitude: 24.0,
            })
            .collect())
    }

    fn get_instances(&self) -> Vec<RunInfo> {
        vec![RunInfo {
            reference_time: run_time(),
            valid_times: vec![run_time()],
        }]
    }

    fn query_location(
        &self,
        location_id: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        match location_id {
            // A station with three coverages (one per level, say).
            "s0" => Ok(CoverageResponse::Collection(
                (0..3).map(|i| coverage(i as f64, 60.0)).collect(),
            )),
            "s1" => Ok(CoverageResponse::Single(coverage(0.0, 61.0))),
            other => Err(DataServerError::LocationNotFound(other.into())),
        }
    }

    fn get_parameters(&self) -> Vec<String> {
        vec!["temperature".into()]
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        Some((run_time(), run_time()))
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([20.0, 59.0, 30.0, 70.0])
    }

    fn supported_query_types(&self) -> Vec<String> {
        [
            "locations",
            "position",
            "area",
            "radius",
            "trajectory",
            "cube",
        ]
        .map(String::from)
        .to_vec()
    }

    fn query_position(
        &self,
        coords: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Self::check_run(reference_time)?;
        self.position_calls.fetch_add(1, Ordering::Relaxed);
        let (lat, lon) = ds_core::feature::parse_point_coords(coords)?;
        // Coverage x = the point's longitude + 0.1 × its index within the point.
        let mut coverages: Vec<_> = (0..self.per_point)
            .map(|i| coverage(lon + i as f64 / 10.0, lat))
            .collect();
        Ok(if self.per_point == 1 {
            CoverageResponse::Single(coverages.remove(0))
        } else {
            CoverageResponse::Collection(coverages)
        })
    }

    fn query_area(
        &self,
        _: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Self::check_run(reference_time)?;
        Ok(CoverageResponse::Collection(
            (0..AREA_COVERAGES)
                .map(|i| coverage(20.0 + i as f64, 60.0))
                .collect(),
        ))
    }

    /// One coverage per timestep, as an along-path trajectory over a
    /// `datetime` window answers: a CoverageCollection `limit` can cap.
    fn query_trajectory(
        &self,
        _: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Ok(CoverageResponse::Collection(
            (0..AREA_COVERAGES)
                .map(|i| coverage(20.0 + i as f64, 60.0))
                .collect(),
        ))
    }

    /// A cube is one coverage, which `limit` cannot page.
    fn query_cube(
        &self,
        _: &ds_core::feature::Bbox,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: ds_core::cube::CubeResolution,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Self::check_run(reference_time)?;
        Ok(CoverageResponse::Single(coverage(24.0, 60.0)))
    }
}

struct App {
    router: axum::Router,
    position_calls: Arc<AtomicUsize>,
}

fn app(per_point: usize) -> App {
    let position_calls = Arc::new(AtomicUsize::new(0));
    let engine: Arc<dyn EdrEngine> = Arc::new(LimitMock {
        per_point,
        position_calls: position_calls.clone(),
    });
    let config = CollectionConfig {
        id: "obs".into(),
        title: "Observations".into(),
        description: "limit fixture".into(),
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
        derive_wind: None,
    };
    let state = Arc::new(ArcSwap::from_pointee(EdrState {
        engines: HashMap::from([("obs".to_string(), engine)]),
        collections: HashMap::from([("obs".to_string(), config)]),
        styles: HashMap::new(),
        feature_engines: HashMap::new(),
        base_url: "https://example.test".into(),
        trust_proxy_headers: false,
    }));
    App {
        router: api_edr::router(state),
        position_calls,
    }
}

impl App {
    async fn get(&self, uri: &str) -> (StatusCode, Value) {
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

    fn calls(&self) -> usize {
        self.position_calls.load(Ordering::Relaxed)
    }
}

const POLYGON: &str = "POLYGON((20%2059,30%2059,30%2070,20%2070,20%2059))";
const CIRCLE: &str = "coords=POINT(24%2060)&within=10&within-units=km";

const LINE: &str = "LINESTRING(20%2060,21%2061)";
const CUBE: &str = "bbox=20,60,21,61";

/// Five points at longitudes 1, 2, 3, 4, 5.
const FIVE_POINTS: &str = "MULTIPOINT((1%2060),(2%2060),(3%2060),(4%2060),(5%2060))";

/// Every route `limit` applies to, with its other required parameters.
fn limited_routes() -> Vec<String> {
    vec![
        format!("/collections/obs/position?coords={FIVE_POINTS}"),
        format!("/collections/obs/area?coords={POLYGON}"),
        format!("/collections/obs/radius?{CIRCLE}"),
        "/collections/obs/locations/s0?".to_string(),
        "/collections/obs/locations?".to_string(),
        format!("/collections/obs/instances/{RUN}/position?coords={FIVE_POINTS}"),
        format!("/collections/obs/instances/{RUN}/area?coords={POLYGON}"),
        format!("/collections/obs/instances/{RUN}/radius?{CIRCLE}"),
        format!("/collections/obs/trajectory?coords={LINE}"),
        format!("/collections/obs/cube?{CUBE}"),
        format!("/collections/obs/instances/{RUN}/cube?{CUBE}"),
    ]
}

fn join(route: &str, query: &str) -> String {
    if route.ends_with('?') {
        format!("{route}{query}")
    } else {
        format!("{route}&{query}")
    }
}

fn xs(json: &Value) -> Vec<f64> {
    json["coverages"]
        .as_array()
        .unwrap_or_else(|| panic!("not a CoverageCollection: {json}"))
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

#[tokio::test]
async fn invalid_limit_is_400_on_every_route_before_any_query() {
    let app = app(1);
    for route in limited_routes() {
        for bad in ["0", "-1", "1.5", "1e2", "ten", "%2B3"] {
            let uri = join(&route, &format!("limit={bad}"));
            let (status, json) = app.get(&uri).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
            assert_eq!(json["code"], "BadRequest", "{uri}");
            let description = json["description"].as_str().unwrap();
            assert!(description.contains("1 to 10000"), "{uri}: {description}");
        }
    }
    assert_eq!(app.calls(), 0, "a bad limit must fail before the engine");
}

#[tokio::test]
async fn limit_above_the_maximum_is_clamped_not_an_error() {
    let app = app(1);
    for route in limited_routes() {
        for big in ["10001", "99999999999999999999999"] {
            let uri = join(&route, &format!("limit={big}"));
            let (status, _) = app.get(&uri).await;
            assert_eq!(status, StatusCode::OK, "{uri}");
        }
    }
    let (_, json) = app
        .get(&format!(
            "/collections/obs/position?coords={FIVE_POINTS}&limit=50000"
        ))
        .await;
    assert_eq!(xs(&json).len(), 5);
    // The clamped value, not the requested one, is what links carry.
    let (_, json) = app.get("/collections/obs/locations?limit=50000").await;
    assert_eq!(json["numberReturned"], LOCATIONS);
    assert_eq!(
        json["links"][0]["href"],
        "https://example.test/edr/collections/obs/locations?limit=10000"
    );
}

#[tokio::test]
async fn multipoint_position_keeps_the_first_coverages_and_skips_later_points() {
    let app = app(1);
    let (status, json) = app
        .get(&format!(
            "/collections/obs/position?coords={FIVE_POINTS}&limit=2"
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["type"], "CoverageCollection");
    assert_eq!(xs(&json), [1.0, 2.0]);
    assert_eq!(app.calls(), 2, "points past the limit are never queried");
    validate("coveragejson.json", &json);

    // A MULTIPOINT stays a collection even when one coverage remains.
    let (_, json) = app
        .get(&format!(
            "/collections/obs/position?coords={FIVE_POINTS}&limit=1"
        ))
        .await;
    assert_eq!(json["type"], "CoverageCollection");
    assert_eq!(xs(&json), [1.0]);

    // Without limit: every point, as before.
    let (_, json) = app
        .get(&format!("/collections/obs/position?coords={FIVE_POINTS}"))
        .await;
    assert_eq!(xs(&json), [1.0, 2.0, 3.0, 4.0, 5.0]);
}

#[tokio::test]
async fn limit_counts_flattened_coverages_not_points() {
    // Three coverages per point (a vertical profile per step, say): limit
    // counts the top-level coverages of the one flattened collection.
    let app = app(3);
    let two_points = "MULTIPOINT((1%2060),(2%2060))";
    let (_, json) = app
        .get(&format!("/collections/obs/position?coords={two_points}"))
        .await;
    assert_eq!(xs(&json), [1.0, 1.1, 1.2, 2.0, 2.1, 2.2]);
    let (_, json) = app
        .get(&format!(
            "/collections/obs/position?coords={two_points}&limit=4"
        ))
        .await;
    assert_eq!(xs(&json), [1.0, 1.1, 1.2, 2.0]);
    validate("coveragejson.json", &json);
    // One point's coverages are capped too.
    let (_, json) = app
        .get("/collections/obs/position?coords=POINT(1%2060)&limit=2")
        .await;
    assert_eq!(json["type"], "CoverageCollection");
    assert_eq!(xs(&json), [1.0, 1.1]);
}

#[tokio::test]
async fn a_single_coverage_is_one_object_and_unchanged() {
    let app = app(1);
    let (status, json) = app
        .get("/collections/obs/position?coords=POINT(1%2060)&limit=1")
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["type"], "Coverage");
    let (_, json) = app.get("/collections/obs/locations/s1?limit=1").await;
    assert_eq!(json["type"], "Coverage");
}

#[tokio::test]
async fn location_area_and_radius_collections_are_capped() {
    let app = app(1);
    let (_, json) = app.get("/collections/obs/locations/s0").await;
    assert_eq!(xs(&json).len(), 3);
    let (_, json) = app.get("/collections/obs/locations/s0?limit=2").await;
    assert_eq!(xs(&json), [0.0, 1.0]);
    validate("coveragejson.json", &json);

    let area = format!("/collections/obs/area?coords={POLYGON}");
    let (_, json) = app.get(&area).await;
    assert_eq!(xs(&json).len(), AREA_COVERAGES);
    let (_, json) = app.get(&format!("{area}&limit=2")).await;
    assert_eq!(xs(&json), [20.0, 21.0]);
    validate("coveragejson.json", &json);

    let (_, json) = app
        .get(&format!("/collections/obs/radius?{CIRCLE}&limit=3"))
        .await;
    assert_eq!(xs(&json), [20.0, 21.0, 22.0]);
}

#[tokio::test]
async fn instance_routes_apply_limit() {
    let app = app(1);
    let base = format!("/collections/obs/instances/{RUN}");
    let (status, json) = app
        .get(&format!("{base}/position?coords={FIVE_POINTS}&limit=3"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(xs(&json), [1.0, 2.0, 3.0]);
    let (_, json) = app
        .get(&format!("{base}/area?coords={POLYGON}&limit=1"))
        .await;
    assert_eq!(xs(&json), [20.0]);
    let (_, json) = app.get(&format!("{base}/radius?{CIRCLE}&limit=2")).await;
    assert_eq!(xs(&json), [20.0, 21.0]);
    // An absent run is still a 404, limit or not.
    let (status, _) = app
        .get(&format!(
            "/collections/obs/instances/20200101T0000Z/area?coords={POLYGON}&limit=1"
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn locations_without_limit_is_the_complete_unpaged_inventory() {
    let app = app(1);
    let (status, json) = app.get("/collections/obs/locations").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["features"].as_array().unwrap().len(), LOCATIONS);
    assert!(json.get("numberMatched").is_none());
    assert!(json.get("numberReturned").is_none());
    assert_eq!(
        json["links"],
        serde_json::json!([{
            "href": "https://example.test/edr/collections/obs/locations",
            "rel": "self",
            "title": "Locations",
            "type": "application/geo+json"
        }])
    );
    // A blank limit is no limit.
    let (_, blank) = app.get("/collections/obs/locations?limit=").await;
    assert_eq!(blank, json);
}

fn link<'a>(json: &'a Value, rel: &str) -> Option<&'a str> {
    json["links"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["rel"] == rel)
        .map(|l| l["href"].as_str().unwrap())
}

fn ids(json: &Value) -> Vec<&str> {
    json["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn locations_pages_follow_next_links_through_the_inventory() {
    let app = app(1);
    let root = "https://example.test/edr/collections/obs/locations";
    let (status, first) = app.get("/collections/obs/locations?limit=2").await;
    assert_eq!(status, StatusCode::OK);
    validate("edr-locations-geojson.json", &first);
    assert_eq!(ids(&first), ["s0", "s1"]);
    assert_eq!(first["numberMatched"], LOCATIONS);
    assert_eq!(first["numberReturned"], 2);
    assert_eq!(link(&first, "self"), Some(&*format!("{root}?limit=2")));
    assert_eq!(link(&first, "prev"), None);

    let next = link(&first, "next").unwrap();
    assert_eq!(next, format!("{root}?limit=2&offset=2"));
    let (_, second) = app
        .get(next.strip_prefix("https://example.test/edr").unwrap())
        .await;
    assert_eq!(ids(&second), ["s2", "s3"]);
    assert_eq!(link(&second, "prev"), Some(&*format!("{root}?limit=2")));

    let next = link(&second, "next").unwrap();
    let (_, last) = app
        .get(next.strip_prefix("https://example.test/edr").unwrap())
        .await;
    assert_eq!(ids(&last), ["s4"]);
    assert_eq!(
        (
            last["numberMatched"].as_u64(),
            last["numberReturned"].as_u64()
        ),
        (Some(5), Some(1))
    );
    assert_eq!(link(&last, "next"), None);
    assert_eq!(
        link(&last, "prev"),
        Some(&*format!("{root}?limit=2&offset=2"))
    );

    // Past the end: an empty page with nowhere to go.
    let (status, empty) = app.get("/collections/obs/locations?limit=2&offset=5").await;
    assert_eq!(status, StatusCode::OK);
    assert!(ids(&empty).is_empty());
    assert_eq!(empty["numberReturned"], 0);
    assert_eq!(empty["numberMatched"], LOCATIONS);
    assert!(link(&empty, "next").is_none() && link(&empty, "prev").is_none());
}

#[tokio::test]
async fn locations_links_repeat_the_other_query_parameters() {
    let app = app(1);
    let (_, json) = app
        .get("/collections/obs/locations?f=GeoJSON&limit=1&offset=1")
        .await;
    let root = "https://example.test/edr/collections/obs/locations";
    assert_eq!(
        link(&json, "self"),
        Some(&*format!("{root}?f=GeoJSON&limit=1&offset=1"))
    );
    assert_eq!(
        link(&json, "next"),
        Some(&*format!("{root}?f=GeoJSON&limit=1&offset=2"))
    );
    assert_eq!(
        link(&json, "prev"),
        Some(&*format!("{root}?f=GeoJSON&limit=1"))
    );
}

#[tokio::test]
async fn locations_paging_rejects_bad_offsets_and_repeats() {
    let app = app(1);
    for query in [
        "offset=2",
        "limit=2&offset=-1",
        "limit=2&offset=x",
        "limit=2&limit=3",
        "limit=2&offset=1&offset=2",
    ] {
        let (status, json) = app
            .get(&format!("/collections/obs/locations?{query}"))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(json["code"], "BadRequest", "{query}");
    }
}

/// The OpenAPI document declares `limit` on every limited operation the way
/// `/req/edr/rc-limit-definition` does (integer, 1…10000, `style: form`,
/// `explode: false`), `offset` on `/locations`, and stays valid OpenAPI 3.0.
#[tokio::test]
async fn openapi_declares_limit_and_offset() {
    let app = app(1);
    let (_, api) = app.get("/api").await;
    let schema: Value =
        serde_json::from_str(include_str!("../../../schemas/openapi-3.0.json")).unwrap();
    let validator = jsonschema::Validator::new(&schema).unwrap();
    let errors: Vec<String> = validator
        .iter_errors(&api)
        .map(|e| format!("{e} at {}", e.instance_path()))
        .collect();
    assert!(errors.is_empty(), "{errors:?}");

    let parameter = |path: &str, name: &str| -> Option<Value> {
        api["paths"][path]["get"]["parameters"]
            .as_array()
            .unwrap_or_else(|| panic!("{path}: no parameters"))
            .iter()
            .map(|p| match p["$ref"].as_str() {
                Some(r) => api.pointer(r.strip_prefix('#').unwrap()).unwrap().clone(),
                None => p.clone(),
            })
            .find(|p| p["name"] == name)
    };
    for path in [
        "/edr/collections/obs/position",
        "/edr/collections/obs/area",
        "/edr/collections/obs/radius",
        "/edr/collections/obs/locations",
        "/edr/collections/obs/locations/{locationId}",
        "/edr/collections/obs/instances/{instanceId}/position",
        "/edr/collections/obs/instances/{instanceId}/area",
        "/edr/collections/obs/instances/{instanceId}/radius",
        "/edr/collections/obs/trajectory",
        "/edr/collections/obs/cube",
        "/edr/collections/obs/instances/{instanceId}/cube",
    ] {
        let limit = parameter(path, "limit").unwrap_or_else(|| panic!("{path}: no limit"));
        assert_eq!(limit["in"], "query", "{path}");
        assert_eq!(limit["required"], false, "{path}");
        assert_eq!(limit["style"], "form", "{path}");
        assert_eq!(limit["explode"], false, "{path}");
        assert_eq!(
            limit["schema"],
            serde_json::json!({"type": "integer", "minimum": 1, "maximum": 10000}),
            "{path}"
        );
        assert_eq!(
            parameter(path, "offset").is_some(),
            path.ends_with("/locations"),
            "{path}: offset only pages the location list"
        );
    }
}

/// EDR 1.2 `/req/edr/rc-core-query-parameters` L: `limit` is allowed on
/// every data query. Trajectory and cube used to 400 on it; now a
/// trajectory's CoverageCollection is capped like any other, and a cube's
/// single coverage, which cannot page, is returned unchanged.
#[tokio::test]
async fn limit_on_trajectory_and_cube_is_accepted() {
    let app = app(1);
    let (status, json) = app
        .get(&format!(
            "/collections/obs/trajectory?coords={LINE}&limit=2"
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(xs(&json), vec![20.0, 21.0]);
    validate("coveragejson.json", &json);
    let (status, json) = app
        .get(&format!(
            "/collections/obs/trajectory?coords={LINE}&limit=10000"
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(xs(&json).len(), AREA_COVERAGES);

    for route in [
        format!("/collections/obs/cube?{CUBE}"),
        format!("/collections/obs/instances/{RUN}/cube?{CUBE}"),
    ] {
        let (_, unlimited) = app.get(&route).await;
        let (status, json) = app.get(&format!("{route}&limit=1")).await;
        assert_eq!(status, StatusCode::OK, "{route}: {json}");
        assert_eq!(json["type"], "Coverage", "{route}");
        assert_eq!(
            json, unlimited,
            "{route}: limit leaves one coverage unchanged"
        );
    }
    // Still one cube parameter: a repeated limit is a 400 like any other.
    let (status, _) = app
        .get(&format!("/collections/obs/cube?{CUBE}&limit=1&limit=2"))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
