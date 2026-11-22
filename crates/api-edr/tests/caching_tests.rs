//! Cache-Control + ETag / If-None-Match behavior (#499).
//!
//! Every 200 must carry an explicit `Cache-Control` (short for metadata and
//! "latest" queries, long for settled past windows) and a weak ETag that
//! answers `If-None-Match` with 304, compared weakly (#1500).

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use tower::ServiceExt;

use api_edr::handlers::EdrState;
use ds_core::config::CollectionConfig;
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::model::*;

/// `station_series`: serves GeoJSON and CSV as well. `runs`: a collection
/// with two model runs, [`OLD_RUN`] and [`NEWEST_RUN`], as a forecast
/// engine.
#[derive(Default)]
struct MockEngine {
    station_series: bool,
    runs: bool,
}

const OLD_RUN: &str = "2024-01-01T00:00:00Z";
const NEWEST_RUN: &str = "2024-01-01T06:00:00Z";

impl MockEngine {
    fn sample_query_result() -> QueryResult {
        let times: Vec<DateTime<Utc>> = (0..3)
            .map(|h| {
                format!("2024-01-01T{h:02}:00:00Z")
                    .parse::<DateTime<Utc>>()
                    .unwrap()
            })
            .collect();
        // Several parameters in fresh-per-query HashMaps: with serde_json's
        // workspace-enabled `preserve_order`, unsorted map serialization
        // would emit a different key order per request — the revalidation
        // tests below would then fail, guarding the sorted-serialization fix.
        let mut parameters = HashMap::new();
        let mut ranges = HashMap::new();
        for name in [
            "temperature",
            "humidity",
            "pressure",
            "wind_speed",
            "dewpoint",
            "visibility",
        ] {
            parameters.insert(
                name.to_string(),
                ParameterDescription {
                    label: name.into(),
                    unit: "degC".into(),
                    observed_property: name.into(),
                    standard_name: None,
                },
            );
            ranges.insert(
                name.to_string(),
                NdArray {
                    shape: vec![3],
                    axis_names: vec!["t".into()],
                    values: vec![Some(-2.5), Some(-2.8), None],
                    precision: ValuePrecision::F64,
                },
            );
        }
        QueryResult {
            domain: DomainDescription::PointSeries {
                x: 24.9384,
                y: 60.1699,
                t: times,
                z: None,
            },
            parameters,
            ranges,
        }
    }
}

impl EdrEngine for MockEngine {
    fn get_locations(&self) -> Result<Arc<LocationSnapshot>, DataServerError> {
        Ok(Arc::new(LocationSnapshot::new(vec![
            Location {
                id: "helsinki".into(),
                label: "Helsinki".into(),
                latitude: 60.1699,
                longitude: 24.9384,
            },
            Location {
                id: "tampere".into(),
                label: "Tampere".into(),
                latitude: 61.4978,
                longitude: 23.7610,
            },
        ])))
    }

    fn query_location(
        &self,
        _location_id: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Ok(CoverageResponse::Single(Self::sample_query_result()))
    }

    fn query_position(
        &self,
        _coords: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Ok(CoverageResponse::Single(Self::sample_query_result()))
    }

    fn query_area(
        &self,
        _coords: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Ok(CoverageResponse::Single(Self::sample_query_result()))
    }

    fn query_radius(
        &self,
        _coords: &str,
        _within_m: f64,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Ok(CoverageResponse::Single(Self::sample_query_result()))
    }

    fn query_cube(
        &self,
        _bbox: &ds_core::feature::Bbox,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _resolution: ds_core::cube::CubeResolution,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Ok(CoverageResponse::Single(Self::sample_query_result()))
    }

    fn query_trajectory(
        &self,
        _coords: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Ok(CoverageResponse::Single(Self::sample_query_result()))
    }

    fn get_parameters(&self) -> Vec<String> {
        // Six parameters, not in name order, so the /collections metadata
        // path (the default `get_parameter_descriptions`, a new map per
        // call) also exercises the sorted serialization guarded by the
        // revalidation tests.
        [
            "temperature",
            "humidity",
            "pressure",
            "wind_speed",
            "dewpoint",
            "visibility",
        ]
        .map(String::from)
        .to_vec()
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        Some((
            "2024-01-01T00:00:00Z".parse().unwrap(),
            "2024-01-01T23:00:00Z".parse().unwrap(),
        ))
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([23.7610, 60.1699, 24.9384, 61.4978])
    }

    fn supported_query_types(&self) -> Vec<String> {
        [
            "locations",
            "position",
            "area",
            "radius",
            "cube",
            "trajectory",
        ]
        .map(String::from)
        .to_vec()
    }

    fn serves_station_series(&self) -> bool {
        self.station_series
    }

    fn get_instances(&self) -> Vec<ds_core::instances::RunInfo> {
        if !self.runs {
            return Vec::new();
        }
        [OLD_RUN, NEWEST_RUN]
            .map(|run| ds_core::instances::RunInfo {
                reference_time: run.parse().unwrap(),
                valid_times: Self::sample_query_result_times(),
            })
            .to_vec()
    }

    fn has_instances(&self) -> bool {
        self.runs
    }
}

impl MockEngine {
    fn sample_query_result_times() -> Vec<DateTime<Utc>> {
        match Self::sample_query_result().domain {
            DomainDescription::PointSeries { t, .. } => t,
            _ => unreachable!(),
        }
    }
}

/// `weather` (observations), `stations` (station series, for GeoJSON and
/// CSV) and `nwp` (a forecast collection with runs).
fn build_state() -> api_edr::handlers::AppState {
    let mut engines: HashMap<String, Arc<dyn EdrEngine>> = HashMap::new();
    let mut collections = HashMap::new();
    for (id, engine) in [
        ("weather", MockEngine::default()),
        (
            "stations",
            MockEngine {
                station_series: true,
                ..MockEngine::default()
            },
        ),
        (
            "nwp",
            MockEngine {
                runs: true,
                ..MockEngine::default()
            },
        ),
    ] {
        engines.insert(id.to_string(), Arc::new(engine));
        collections.insert(id.to_string(), collection(id));
    }
    Arc::new(ArcSwap::from_pointee(EdrState {
        engines,
        feature_engines: HashMap::new(),
        collections,
        styles: HashMap::new(),
        base_url: String::new(),
        trust_proxy_headers: false,
        discovery: Default::default(),
    }))
}

fn build_router() -> axum::Router {
    api_edr::router(build_state())
}

fn collection(id: &str) -> CollectionConfig {
    CollectionConfig {
        id: id.to_string(),
        title: "Weather".to_string(),
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
        station_grid: None,
        period: None,
        bufr: None,
        satellite: None,
        netcdf: None,
        csv: None,
        preview: None,
        derive_wind: None,
        derive_geopotential_height: None,
        discovery: None,
        derive_humidity: None,
        expected_interval: None,
    }
}

async fn get_response(uri: &str, if_none_match: Option<&str>) -> axum::response::Response {
    let mut req = Request::builder().uri(uri);
    if let Some(inm) = if_none_match {
        req = req.header(header::IF_NONE_MATCH, inm);
    }
    build_router()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

fn header_str(resp: &axum::response::Response, name: header::HeaderName) -> Option<&str> {
    resp.headers().get(name).and_then(|v| v.to_str().ok())
}

/// A closed interval comfortably in the past relative to the test run.
const SETTLED_WINDOW: &str = "2024-01-01T00:00:00Z/2024-01-01T06:00:00Z";

#[tokio::test]
async fn metadata_endpoints_carry_short_cache_control_and_etag() {
    for uri in [
        "/",
        "/conformance",
        "/collections",
        "/collections/weather",
        "/collections/weather/locations",
        "/api",
    ] {
        let resp = get_response(uri, None).await;
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
        assert_eq!(
            header_str(&resp, header::CACHE_CONTROL),
            Some("public, max-age=60"),
            "{uri}"
        );
        let etag = header_str(&resp, header::ETAG)
            .expect("etag present")
            .to_owned();
        // Weak (#1500): the server's compression layer gzips the body
        // after the middleware tagged it, so one tag names both forms.
        assert!(
            etag.starts_with("W/\"") && etag.ends_with('"'),
            "{uri}: {etag}"
        );
    }
}

#[tokio::test]
async fn if_none_match_revalidates_to_304() {
    let first = get_response("/collections", None).await;
    let etag = header_str(&first, header::ETAG).unwrap().to_owned();

    let second = get_response("/collections", Some(&etag)).await;
    assert_eq!(second.status(), StatusCode::NOT_MODIFIED);
    // RFC 7232 §4.1: the 304 repeats ETag/Cache-Control (and Vary, which the
    // content-negotiated metadata endpoints set).
    assert_eq!(header_str(&second, header::ETAG), Some(etag.as_str()));
    assert_eq!(
        header_str(&second, header::CACHE_CONTROL),
        Some("public, max-age=60")
    );
    assert_eq!(header_str(&second, header::VARY), Some("accept"));
    let body = second.into_body().collect().await.unwrap().to_bytes();
    assert!(body.is_empty());

    // A non-matching tag still gets the full 200.
    let third = get_response("/collections", Some("\"deadbeefdeadbeef\"")).await;
    assert_eq!(third.status(), StatusCode::OK);
    let body = third.into_body().collect().await.unwrap().to_bytes();
    assert!(!body.is_empty());
}

#[tokio::test]
async fn coverage_query_revalidates_to_304() {
    let uri = format!(
        "/collections/weather/position?coords=POINT(24.94%2060.17)&datetime={SETTLED_WINDOW}"
    );
    let first = get_response(&uri, None).await;
    assert_eq!(first.status(), StatusCode::OK);
    let etag = header_str(&first, header::ETAG).unwrap().to_owned();

    let second = get_response(&uri, Some(&etag)).await;
    assert_eq!(second.status(), StatusCode::NOT_MODIFIED);
    let body = second.into_body().collect().await.unwrap().to_bytes();
    assert!(body.is_empty());
}

#[tokio::test]
async fn settled_past_window_gets_long_cache_control() {
    for uri in [
        format!(
            "/collections/weather/position?coords=POINT(24.94%2060.17)&datetime={SETTLED_WINDOW}"
        ),
        format!("/collections/weather/locations/helsinki?datetime={SETTLED_WINDOW}"),
        // An instant in the past is settled too.
        "/collections/weather/position?coords=POINT(24.94%2060.17)&datetime=2024-01-01T03:00:00Z"
            .to_string(),
    ] {
        let resp = get_response(&uri, None).await;
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
        assert_eq!(
            header_str(&resp, header::CACHE_CONTROL),
            Some("public, max-age=86400"),
            "{uri}"
        );
    }
}

#[tokio::test]
async fn open_latest_or_future_windows_get_short_cache_control() {
    let now_plus_day =
        (Utc::now() + chrono::Duration::days(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    for uri in [
        // No datetime — "latest".
        "/collections/weather/position?coords=POINT(24.94%2060.17)".to_string(),
        // Open start: retention keeps changing the answer.
        "/collections/weather/position?coords=POINT(24.94%2060.17)&datetime=../2024-01-01T06:00:00Z"
            .to_string(),
        // Open end.
        "/collections/weather/position?coords=POINT(24.94%2060.17)&datetime=2024-01-01T00:00:00Z/.."
            .to_string(),
        // Closed window that isn't settled yet.
        format!(
            "/collections/weather/position?coords=POINT(24.94%2060.17)&datetime=2024-01-01T00:00:00Z/{now_plus_day}"
        ),
    ] {
        let resp = get_response(&uri, None).await;
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
        assert_eq!(
            header_str(&resp, header::CACHE_CONTROL),
            Some("public, max-age=60"),
            "{uri}"
        );
    }
}

#[tokio::test]
async fn error_responses_are_left_alone() {
    let resp = get_response("/collections/nope", None).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(resp.headers().get(header::ETAG).is_none());
    assert!(resp.headers().get(header::CACHE_CONTROL).is_none());
}

/// `If-None-Match` compares weakly (RFC 9110 §13.1.2, #1500): the strong
/// form of a data query's tag, as a client held it before the tags turned
/// weak, and the weak tag in a list are both a 304.
#[tokio::test]
async fn if_none_match_compares_weakly() {
    let uri = format!(
        "/collections/weather/position?coords=POINT(24.94%2060.17)&datetime={SETTLED_WINDOW}"
    );
    let first = get_response(&uri, None).await;
    let etag = header_str(&first, header::ETAG).unwrap().to_owned();
    assert!(etag.starts_with("W/\""), "{etag}");
    let strong = etag.trim_start_matches("W/").to_owned();
    for sent in [strong, format!("\"aaa\", {etag}")] {
        let resp = get_response(&uri, Some(&sent)).await;
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED, "{sent}");
        assert_eq!(header_str(&resp, header::ETAG), Some(etag.as_str()));
    }
}

/// The data-query routes alone, without the router's `conditional_get`
/// middleware, so a tag on their 200s is one the handler set.
fn handlers_only() -> axum::Router {
    use api_edr::handlers::{
        area_query, cube_query, instance_position_query, location_query, position_query,
        radius_query, trajectory_query,
    };
    use axum::routing::get;
    axum::Router::new()
        .route("/collections/{id}/position", get(position_query))
        .route("/collections/{id}/area", get(area_query))
        .route("/collections/{id}/radius", get(radius_query))
        .route("/collections/{id}/cube", get(cube_query))
        .route("/collections/{id}/trajectory", get(trajectory_query))
        .route("/collections/{id}/locations/{loc_id}", get(location_query))
        .route(
            "/collections/{id}/instances/{instanceId}/position",
            get(instance_position_query),
        )
        .with_state(build_state())
}

/// Every data query's body is hashed by its handler, on the query executor
/// inside the query's slot, not by the middleware on the request runtime
/// after it (#1500): each query type's and each format's 200 carries the
/// weak tag of its own bytes before any middleware ran, and the full router
/// keeps that tag.
#[tokio::test]
async fn data_queries_tag_their_own_bodies() {
    let position = "/collections/weather/position?coords=POINT(24.94%2060.17)";
    let station = "/collections/stations/position?coords=POINT(24.94%2060.17)";
    for uri in [
        position.to_string(),
        format!("{position}&f=html"),
        format!("{position}&f=png"),
        format!("{station}&f=geojson"),
        format!("{station}&f=csv"),
        "/collections/weather/area?coords=POLYGON((24%2060,25%2060,25%2061,24%2061,24%2060))"
            .to_string(),
        "/collections/weather/radius?coords=POINT(24.94%2060.17)&within=10&within-units=km"
            .to_string(),
        "/collections/weather/cube?bbox=24,60,25,61".to_string(),
        "/collections/weather/trajectory?coords=LINESTRING(24%2060,25%2061)".to_string(),
        "/collections/weather/locations/helsinki".to_string(),
        // A list of locations, answered in one response.
        "/collections/stations/locations/helsinki,tampere?f=geojson".to_string(),
        format!("/collections/nwp/instances/{OLD_RUN}/position?coords=POINT(24.94%2060.17)"),
    ] {
        let resp = handlers_only()
            .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
        let etag = header_str(&resp, header::ETAG)
            .unwrap_or_else(|| panic!("{uri}: no ETag from the handler"))
            .to_owned();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(etag, ds_core::http_cache::weak_etag_of(&body), "{uri}");

        let routed = get_response(&uri, None).await;
        assert_eq!(
            header_str(&routed, header::ETAG),
            Some(etag.as_str()),
            "{uri}"
        );
        let revalidated = get_response(&uri, Some(&etag)).await;
        assert_eq!(revalidated.status(), StatusCode::NOT_MODIFIED, "{uri}");
    }
}

/// On a collection with runs, a run can still change an answer however
/// long ago its window ended (#1500): the newest run is published before
/// all its steps are in, and a later run covers valid times from its
/// reference time on. Those answers get the short policy; every other one
/// keeps its window's.
#[tokio::test]
async fn answers_a_run_can_still_change_are_never_settled() {
    const SHORT: &str = "public, max-age=60";
    const LONG: &str = "public, max-age=86400";
    let at = |window: &str| format!("coords=POINT(24.94%2060.17)&datetime={window}");
    for (uri, cache_control) in [
        // The newest run starts at 06Z: a window ending before it is
        // answered by older runs, and no later run reaches it.
        (
            format!(
                "/collections/nwp/position?{}",
                at("2024-01-01T00:00:00Z/2024-01-01T05:59:59Z")
            ),
            LONG,
        ),
        // A window reaching the newest run's reference time, or past it.
        (
            format!("/collections/nwp/position?{}", at(SETTLED_WINDOW)),
            SHORT,
        ),
        (
            format!(
                "/collections/nwp/position?{}",
                at("2024-01-01T05:00:00Z/2024-01-01T07:00:00Z")
            ),
            SHORT,
        ),
        // The newest run itself, even for a window before it.
        (
            format!(
                "/collections/nwp/instances/{NEWEST_RUN}/position?{}",
                at("2024-01-01T00:00:00Z/2024-01-01T02:00:00Z")
            ),
            SHORT,
        ),
        // An older run keeps its window's policy, whatever the window.
        (
            format!(
                "/collections/nwp/instances/{OLD_RUN}/position?{}",
                at(SETTLED_WINDOW)
            ),
            LONG,
        ),
        // A collection without runs keeps the window's policy.
        (
            format!("/collections/weather/position?{}", at(SETTLED_WINDOW)),
            LONG,
        ),
    ] {
        let resp = get_response(&uri, None).await;
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
        assert_eq!(
            header_str(&resp, header::CACHE_CONTROL),
            Some(cache_control),
            "{uri}"
        );
    }
}
