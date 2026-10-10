//! OGC API - EDR instances (forecast model runs; #337).
//!
//! Exercises the instances endpoints against a mock forecast engine exposing two
//! runs (00Z, 12Z). The mock encodes the selected run's hour into every value so
//! a query can prove which run it hit (None ⇒ latest run = 12Z).

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, TimeZone, Timelike, Utc};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::util::ServiceExt;

use api_edr::handlers::EdrState;
use ds_core::config::CollectionConfig;
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::instances::RunInfo;
use ds_core::model::*;

#[path = "support/edr_schema.rs"]
mod edr_schema;

fn dt(h: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 7, h, 0, 0).unwrap()
}

const LATEST_RUN_HOUR: u32 = 12;

struct ForecastMock;

impl ForecastMock {
    fn run_times() -> Vec<DateTime<Utc>> {
        vec![dt(0), dt(LATEST_RUN_HOUR)] // 00Z, 12Z (latest)
    }
}

impl EdrEngine for ForecastMock {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        Ok(vec![])
    }

    fn get_instances(&self) -> Vec<RunInfo> {
        Self::run_times()
            .into_iter()
            .map(|rt| RunInfo {
                reference_time: rt,
                valid_times: (0..3).map(|h| rt + chrono::Duration::hours(h)).collect(),
            })
            .collect()
    }

    fn query_location(
        &self,
        _location_id: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Err(DataServerError::InvalidParameter("no locations".into()))
    }

    fn get_parameters(&self) -> Vec<String> {
        vec!["temperature".to_string()]
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        // Latest run's valid-time span.
        Some((
            dt(LATEST_RUN_HOUR),
            dt(LATEST_RUN_HOUR) + chrono::Duration::hours(2),
        ))
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([-180.0, -90.0, 180.0, 90.0])
    }

    fn supported_query_types(&self) -> Vec<String> {
        vec!["position".to_string()]
    }

    fn query_position(
        &self,
        _coords: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        // None ⇒ latest run; a pinned run that doesn't exist → 404 (mirrors the
        // real engines, which return ReferenceTimeNotFound).
        let rt = reference_time.unwrap_or_else(|| dt(LATEST_RUN_HOUR));
        if !Self::run_times().contains(&rt) {
            return Err(DataServerError::ReferenceTimeNotFound(rt.to_rfc3339()));
        }
        let marker = rt.hour() as f64;
        let times: Vec<DateTime<Utc>> = (0..3).map(|h| rt + chrono::Duration::hours(h)).collect();
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
                shape: vec![3],
                axis_names: vec!["t".to_string()],
                values: vec![Some(marker); 3],
            },
        );
        Ok(CoverageResponse::Single(QueryResult {
            domain: DomainDescription::PointSeries {
                x: 25.0,
                y: 60.0,
                t: times,
                z: None,
            },
            parameters,
            ranges,
        }))
    }
}

/// [`ForecastMock`] as a station collection: its position series sit at the
/// one location it lists, so the point queries also offer EDR GeoJSON, whose
/// `self` link names the instance the request pinned.
struct StationForecastMock;

impl EdrEngine for StationForecastMock {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        Ok(vec![Location {
            id: "station".to_string(),
            label: "Station".to_string(),
            latitude: 60.0,
            longitude: 25.0,
        }])
    }
    fn serves_station_series(&self) -> bool {
        true
    }
    fn get_instances(&self) -> Vec<RunInfo> {
        ForecastMock.get_instances()
    }
    fn query_location(
        &self,
        location_id: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        ForecastMock.query_location(location_id, datetime, parameters, z, reference_time)
    }
    fn get_parameters(&self) -> Vec<String> {
        ForecastMock.get_parameters()
    }
    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        ForecastMock.get_temporal_extent()
    }
    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        ForecastMock.get_spatial_extent()
    }
    fn supported_query_types(&self) -> Vec<String> {
        ForecastMock.supported_query_types()
    }
    fn query_position(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        ForecastMock.query_position(coords, datetime, parameters, z, reference_time)
    }
}

/// A non-forecast engine: no instances, and `query_position` IGNORES
/// `reference_time` (returns latest data). Proves the API rejects instance
/// queries on such a collection rather than silently serving 200.
struct NonForecastMock;

impl EdrEngine for NonForecastMock {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        Ok(vec![])
    }
    fn query_location(
        &self,
        _: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Err(DataServerError::InvalidParameter("no locations".into()))
    }
    fn get_parameters(&self) -> Vec<String> {
        vec!["temperature".to_string()]
    }
    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        Some((dt(0), dt(2)))
    }
    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([-180.0, -90.0, 180.0, 90.0])
    }
    fn supported_query_types(&self) -> Vec<String> {
        vec!["position".to_string()]
    }
    fn query_position(
        &self,
        _coords: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>, // ignored — the engine has no runs
    ) -> Result<CoverageResponse, DataServerError> {
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
                shape: vec![1],
                axis_names: vec!["t".to_string()],
                values: vec![Some(1.0)],
            },
        );
        Ok(CoverageResponse::Single(QueryResult {
            domain: DomainDescription::PointSeries {
                x: 25.0,
                y: 60.0,
                t: vec![dt(0)],
                z: None,
            },
            parameters,
            ranges,
        }))
    }
}

/// An archive store exposing every historical run (#1006): six-hourly from
/// 2021-05-01 to 2026-10-09T18Z, less the two runs of 2022-01-01 00Z and 06Z,
/// 7950 runs. Every run has two days of hourly valid times; the collection's
/// own time axis is 30 days of hourly steps. Like the real forecast engines
/// it answers [`EdrEngine::instance_reference_times`] and
/// [`EdrEngine::find_instance`] from its run map; `get_instances` panics, so
/// no request may enumerate every run with its valid times.
struct ArchiveMock {
    /// Whether [`EdrEngine::instance_reference_times`] answers. The
    /// `archive-doc` collection's does not: it panics, so a collection
    /// document that copied the run axis would fail its request.
    run_axis: bool,
}

/// Valid times per archive run: two days, hourly.
const ARCHIVE_LEADS: i64 = 49;

/// The archive collection's time axis: 30 days, hourly, to its latest run's
/// last valid time.
const ARCHIVE_STEPS: i64 = 721;

impl ArchiveMock {
    fn first_run() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2021, 5, 1, 0, 0, 0).unwrap()
    }

    /// The two runs missing from the archive.
    fn gap() -> [DateTime<Utc>; 2] {
        let start = Utc.with_ymd_and_hms(2022, 1, 1, 0, 0, 0).unwrap();
        [start, start + chrono::Duration::hours(6)]
    }

    fn runs() -> Vec<DateTime<Utc>> {
        let last = Utc.with_ymd_and_hms(2026, 10, 9, 18, 0, 0).unwrap();
        let mut runs = Vec::new();
        let mut rt = Self::first_run();
        while rt <= last {
            if !Self::gap().contains(&rt) {
                runs.push(rt);
            }
            rt += chrono::Duration::hours(6);
        }
        runs
    }

    fn valid_times(rt: DateTime<Utc>) -> Vec<DateTime<Utc>> {
        (0..ARCHIVE_LEADS)
            .map(|h| rt + chrono::Duration::hours(h))
            .collect()
    }

    fn collection_times() -> Vec<DateTime<Utc>> {
        let end = *Self::valid_times(*Self::runs().last().unwrap())
            .last()
            .unwrap();
        (0..ARCHIVE_STEPS)
            .rev()
            .map(|h| end - chrono::Duration::hours(h))
            .collect()
    }
}

impl EdrEngine for ArchiveMock {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        Ok(vec![])
    }
    fn get_instances(&self) -> Vec<RunInfo> {
        panic!("a request enumerated every run of a long run axis with its valid times");
    }
    fn has_instances(&self) -> bool {
        true
    }
    fn instance_reference_times(&self) -> Vec<DateTime<Utc>> {
        assert!(
            self.run_axis,
            "collection metadata copied the run axis: O(runs) per request"
        );
        Self::runs()
    }
    fn find_instance(&self, reference_time: DateTime<Utc>) -> Option<RunInfo> {
        Self::runs()
            .binary_search(&reference_time)
            .ok()
            .map(|_| RunInfo {
                reference_time,
                valid_times: Self::valid_times(reference_time),
            })
    }
    fn query_location(
        &self,
        _: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Err(DataServerError::InvalidParameter("no locations".into()))
    }
    fn get_parameters(&self) -> Vec<String> {
        vec!["temperature".to_string()]
    }
    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        let times = Self::collection_times();
        Some((times[0], *times.last().unwrap()))
    }
    fn get_available_times(&self) -> Option<Vec<DateTime<Utc>>> {
        Some(Self::collection_times())
    }
    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([-180.0, -90.0, 180.0, 90.0])
    }
    fn supported_query_types(&self) -> Vec<String> {
        vec!["position".to_string()]
    }
}

fn config(id: &str, engine_type: &str) -> CollectionConfig {
    CollectionConfig {
        id: id.to_string(),
        title: id.to_string(),
        description: format!("The {id} collection"),
        data_path: None,
        apis: vec!["edr".to_string()],
        engine_type: engine_type.to_string(),
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

fn state() -> api_edr::handlers::AppState {
    let mut engines: HashMap<String, Arc<dyn EdrEngine>> = HashMap::new();
    let mut collections = HashMap::new();
    engines.insert("fc".to_string(), Arc::new(ForecastMock));
    collections.insert("fc".to_string(), config("fc", "grib"));
    engines.insert("obs".to_string(), Arc::new(NonForecastMock));
    collections.insert("obs".to_string(), config("obs", "geotiff"));
    engines.insert("st".to_string(), Arc::new(StationForecastMock));
    collections.insert("st".to_string(), config("st", "csv"));
    engines.insert(
        "archive".to_string(),
        Arc::new(ArchiveMock { run_axis: true }),
    );
    collections.insert("archive".to_string(), config("archive", "zarr"));
    engines.insert(
        "archive-doc".to_string(),
        Arc::new(ArchiveMock { run_axis: false }),
    );
    collections.insert("archive-doc".to_string(), config("archive-doc", "zarr"));
    Arc::new(ArcSwap::from_pointee(EdrState {
        engines,
        feature_engines: HashMap::new(),
        collections,
        styles: HashMap::new(),
        base_url: String::new(),
        trust_proxy_headers: false,
    }))
}

async fn get(uri: &str) -> (StatusCode, Value) {
    let app = api_edr::router(state());
    let resp = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    (status, json)
}

/// Raw response for the HTML representation (content type + body).
async fn get_raw(uri: &str, accept: Option<&str>) -> (StatusCode, String, String) {
    let app = api_edr::router(state());
    let mut req = Request::builder().uri(uri);
    if let Some(a) = accept {
        req = req.header("accept", a);
    }
    let resp = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let status = resp.status();
    let ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, ctype, String::from_utf8_lossy(&body).into_owned())
}

// EDR 1.1 `html` conformance class: the instance resources negotiate HTML
// like every other metadata page (review finding on #669).
#[tokio::test]
async fn instances_negotiate_html() {
    for uri in [
        "/collections/fc/instances?f=html",
        "/collections/fc/instances/2026-06-07T00:00:00Z?f=html",
    ] {
        let (status, ctype, body) = get_raw(uri, None).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert!(ctype.starts_with("text/html"), "{uri}: {ctype}");
        assert!(
            body.contains("2026-06-07T00:00:00Z"),
            "{uri} must name the run"
        );
        assert!(
            body.contains("?f=json"),
            "{uri} must link the JSON alternate"
        );
    }
    // Accept header alone selects HTML too, and JSON stays the default.
    let (status, ctype, _) = get_raw("/collections/fc/instances", Some("text/html")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(ctype.starts_with("text/html"), "{ctype}");
    let (_, ctype, _) = get_raw("/collections/fc/instances", None).await;
    assert!(ctype.starts_with("application/json"), "{ctype}");
    // An unknown format is a 400, not silently JSON.
    let (status, _, _) = get_raw("/collections/fc/instances?f=xml", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn instances_list_has_both_runs() {
    let (status, body) = get("/collections/fc/instances").await;
    assert_eq!(status, StatusCode::OK);
    // OGC EDR `instancesJSON` array field is `instances`, not `collections`.
    let instances = body["instances"].as_array().unwrap();
    assert_eq!(instances.len(), 2);
    let ids: Vec<&str> = instances
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    // Ascending by reference time, latest last (the build_instances contract).
    assert_eq!(
        ids,
        ["2026-06-07T00:00:00Z", "2026-06-07T12:00:00Z"],
        "order: {ids:?}"
    );
}

/// The `self` link of an instance document.
fn self_href(doc: &Value) -> &str {
    doc["links"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["rel"] == "self")
        .and_then(|l| l["href"].as_str())
        .unwrap_or_else(|| panic!("no self link: {doc}"))
}

/// MetOcean EDR profile `/req/nwp/collection_granularity` C: an instance id
/// is an RFC 3339 datestamp, and the instance's `self` link title and every
/// link use the same string with the colons unencoded (#947). The document
/// `title` is the collection's (EDR 1.2 `/req/instances/src-md-success` C).
#[tokio::test]
async fn instance_ids_are_rfc3339_in_ids_titles_and_links() {
    let (status, body) = get("/collections/fc/instances").await;
    assert_eq!(status, StatusCode::OK);
    for (instance, id) in body["instances"]
        .as_array()
        .unwrap()
        .iter()
        .zip(["2026-06-07T00:00:00Z", "2026-06-07T12:00:00Z"])
    {
        assert_eq!(instance["id"], id);
        assert_eq!(instance["title"], "fc");
        let self_link = instance["links"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["rel"] == "self")
            .unwrap();
        assert_eq!(self_link["title"], format!("fc — run {id}"));
        assert_eq!(
            self_href(instance),
            format!("/edr/collections/fc/instances/{id}")
        );
        assert_eq!(
            instance["data_queries"]["position"]["link"]["href"],
            format!("/edr/collections/fc/instances/{id}/position")
        );
    }

    // The HTML list links each run by the same id, colons unencoded.
    let (status, _, html) = get_raw("/collections/fc/instances?f=html", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("href=\"/edr/collections/fc/instances/2026-06-07T12:00:00Z?f=html\""),
        "{html}"
    );
    // Each card carries the instance's own title (`/req/html/content` A).
    assert!(html.contains("fc — run 2026-06-07T12:00:00Z"), "{html}");
    assert!(!html.contains("%3A"), "{html}");
}

/// The id a link carries, percent-encoded by a client, resolves to the same
/// run on the instance document and on its query routes: axum decodes the
/// path segment before it reaches the parser.
#[tokio::test]
async fn percent_encoded_instance_id_resolves() {
    for encoded in ["2026-06-07T00%3A00%3A00Z", "2026-06-07T00%3a00%3a00Z"] {
        let uri = format!("/collections/fc/instances/{encoded}");
        let (status, body) = get(&uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(body["id"], "2026-06-07T00:00:00Z", "{uri}");
        assert_eq!(
            self_href(&body),
            "/edr/collections/fc/instances/2026-06-07T00:00:00Z",
            "{uri}"
        );

        let uri = format!("/collections/fc/instances/{encoded}/position?coords=POINT(25%2060)");
        let (status, body) = get(&uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(body["ranges"]["temperature"]["values"][0], 0.0, "{uri}");
    }
}

/// The compact id served before #947 keeps resolving, as does any RFC 3339
/// offset naming the same instant; the document still answers with the
/// canonical id and links.
#[tokio::test]
async fn compact_and_offset_instance_ids_still_resolve() {
    for id in [
        "20260607T0000Z",
        "20260607T000000Z",
        "2026-06-07T03:00:00+03:00",
    ] {
        let uri = format!("/collections/fc/instances/{id}");
        let (status, body) = get(&uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(body["id"], "2026-06-07T00:00:00Z", "{uri}");
        assert_eq!(
            self_href(&body),
            "/edr/collections/fc/instances/2026-06-07T00:00:00Z",
            "{uri}"
        );

        let uri = format!("/collections/fc/instances/{id}/position?coords=POINT(25%2060)");
        let (status, body) = get(&uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(body["ranges"]["temperature"]["values"][0], 0.0, "{uri}");
    }
}

/// A data query's EDR GeoJSON links name the pinned run by its canonical id,
/// colons unencoded, whichever accepted form the request used.
#[tokio::test]
async fn geojson_links_name_the_canonical_instance_id() {
    for id in [
        "2026-06-07T00:00:00Z",
        "2026-06-07T00%3A00%3A00Z",
        "20260607T0000Z",
    ] {
        let uri =
            format!("/collections/st/instances/{id}/position?coords=POINT(25%2060)&f=GeoJSON");
        let (status, body) = get(&uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        assert_eq!(
            self_href(&body),
            "/edr/collections/st/instances/2026-06-07T00:00:00Z/position?coords=POINT(25%2060)&f=GeoJSON",
            "{uri}"
        );
    }
}

#[tokio::test]
async fn instance_metadata_scopes_extent_and_links() {
    let (status, body) = get("/collections/fc/instances/2026-06-07T00:00:00Z").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], "2026-06-07T00:00:00Z");
    // Temporal extent is the 00Z run's valid times (00:00..02:00), NOT the
    // collection's latest-run (12Z) extent.
    let interval = &body["extent"]["temporal"]["interval"][0];
    assert_eq!(interval[0], "2026-06-07T00:00:00+00:00");
    assert_eq!(interval[1], "2026-06-07T02:00:00+00:00");
    // Data-query hrefs are instance-scoped.
    let pos = body["data_queries"]["position"]["link"]["href"]
        .as_str()
        .unwrap();
    assert!(
        pos.ends_with("/collections/fc/instances/2026-06-07T00:00:00Z/position"),
        "{pos}"
    );
}

#[tokio::test]
async fn instance_position_query_hits_the_pinned_run() {
    // Pinned 00Z run → marker 0.
    let (status, body) =
        get("/collections/fc/instances/2026-06-07T00:00:00Z/position?coords=POINT(25%2060)").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ranges"]["temperature"]["values"][0], 0.0);

    // No instance → latest run (12Z) → marker 12.
    let (status, body) = get("/collections/fc/position?coords=POINT(25%2060)").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ranges"]["temperature"]["values"][0], 12.0);
}

#[tokio::test]
async fn unknown_instance_is_404_bad_id_is_400() {
    // A well-formed but absent run → 404 (both metadata and query), in the
    // RFC 3339 id form and the pre-#947 compact one.
    for id in ["2026-06-07T06:00:00Z", "20260607T0600Z"] {
        assert_eq!(
            get(&format!("/collections/fc/instances/{id}")).await.0,
            StatusCode::NOT_FOUND,
            "{id}"
        );
        assert_eq!(
            get(&format!(
                "/collections/fc/instances/{id}/position?coords=POINT(25%2060)"
            ))
            .await
            .0,
            StatusCode::NOT_FOUND,
            "{id}"
        );
    }
    // An unparseable instance id → 400.
    assert_eq!(
        get("/collections/fc/instances/not-a-time").await.0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn instance_query_on_non_forecast_collection_is_404() {
    // The `obs` collection has no instances; its engine ignores reference_time.
    // An instance path must 404, not silently serve 200 with latest data.
    assert_eq!(
        get("/collections/obs/instances").await.1["instances"]
            .as_array()
            .map(|a| a.len()),
        Some(0)
    );
    assert_eq!(
        get("/collections/obs/instances/20260607T0000Z").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get("/collections/obs/instances/20260607T0000Z/position?coords=POINT(25%2060)")
            .await
            .0,
        StatusCode::NOT_FOUND,
        "instance query on a non-forecast collection must be 404, not 200"
    );
    // Even an unparseable id is 404 here (no instance sub-resources at all) —
    // consistent between the metadata and query paths.
    assert_eq!(
        get("/collections/obs/instances/not-a-time").await.0,
        StatusCode::NOT_FOUND
    );
    // The plain (no-instance) position query still works.
    assert_eq!(
        get("/collections/obs/position?coords=POINT(25%2060)")
            .await
            .0,
        StatusCode::OK
    );
}

/// The forecast collection, its instances list and one instance validate
/// against EDR 1.1 and 1.2 (#919).
#[tokio::test]
async fn instance_documents_validate_against_edr_bundles() {
    for (uri, path) in [
        ("/collections/fc", "/collections/{collectionId}"),
        (
            "/collections/fc/instances",
            "/collections/{collectionId}/instances",
        ),
        (
            "/collections/fc/instances/2026-06-07T00:00:00Z",
            edr_schema::INSTANCE,
        ),
    ] {
        let (status, body) = get(uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        edr_schema::assert_valid(path, edr_schema::JSON, &body, uri);
    }
}

/// The `rel=data` links of a document's `links` (EDR 1.2
/// `/req/core/rc-md-query-links` A), as sorted `(href, type)` pairs, after checking that
/// every link has a `rel` and a `type` (B).
fn data_links(doc: &Value) -> Vec<(&str, &str)> {
    let links = doc["links"].as_array().unwrap();
    for link in links {
        assert!(
            link["rel"].is_string() && link["type"].is_string(),
            "{link}"
        );
    }
    links
        .iter()
        .filter(|l| l["rel"] == "data")
        .map(|l| (l["href"].as_str().unwrap(), l["type"].as_str().unwrap()))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// A forecast collection links its query end points and its instances list;
/// an instance document links the run's own end points and no instances.
#[tokio::test]
async fn collection_and_instance_links_name_their_query_end_points() {
    let (_, collection) = get("/collections/fc").await;
    assert_eq!(
        data_links(&collection),
        [
            ("/edr/collections/fc/instances", "application/json"),
            ("/edr/collections/fc/position", "application/vnd.cov+json"),
        ]
    );
    assert_eq!(
        collection["data_queries"]["instances"]["link"]["type"],
        "application/json"
    );
    // No radius query, so no collection-level `within_units`.
    assert!(collection.get("within_units").is_none());

    let (_, instance) = get("/collections/fc/instances/2026-06-07T00:00:00Z").await;
    assert_eq!(
        data_links(&instance),
        [(
            "/edr/collections/fc/instances/2026-06-07T00:00:00Z/position",
            "application/vnd.cov+json"
        )]
    );
    let (_, list) = get("/collections/fc/instances").await;
    for instance in list["instances"].as_array().unwrap() {
        assert_eq!(data_links(instance).len(), 1, "{instance}");
    }
}

/// The HTML pages list the data queries from `data_queries`; the `rel=data`
/// links are listed at their own href, never as `?f=html` pages a data
/// query is not.
#[tokio::test]
async fn html_pages_do_not_open_data_queries_as_pages() {
    for uri in [
        "/collections/fc?f=html",
        "/collections/fc/instances?f=html",
        "/collections/fc/instances/2026-06-07T00:00:00Z?f=html",
    ] {
        let (status, _, body) = get_raw(uri, None).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert!(body.contains("/position"), "{uri} lists the position query");
        assert!(!body.contains("/position?f=html"), "{uri}");
    }
}

#[tokio::test]
async fn collection_advertises_instances_data_query() {
    let (status, body) = get("/collections/fc").await;
    assert_eq!(status, StatusCode::OK);
    let href = body["data_queries"]["instances"]["link"]["href"]
        .as_str()
        .expect("instances data_query present");
    assert!(href.ends_with("/collections/fc/instances"), "{href}");
}

/// EDR 1.2 `/req/instances/src-md-success` C: an instance's `title` and
/// `description` are its collection's entry in `/collections`. The `id`
/// clause cannot hold (instance ids differ from the collection id and from
/// each other) and each run keeps its own `extent` (#982).
#[tokio::test]
async fn instance_title_and_description_are_the_collections() {
    let (_, collections) = get("/collections").await;
    let entry = collections["collections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "fc")
        .unwrap()
        .clone();
    assert_eq!(entry["description"], "The fc collection");
    let (_, list) = get("/collections/fc/instances").await;
    let (_, one) = get("/collections/fc/instances/2026-06-07T00:00:00Z").await;
    let listed = list["instances"].as_array().unwrap();
    for instance in listed.iter().chain([&one]) {
        assert_eq!(instance["title"], entry["title"], "{instance}");
        assert_eq!(instance["description"], entry["description"], "{instance}");
        assert_ne!(instance["id"], entry["id"]);
    }
    // The 00Z run's extent is its own, not the collection's (latest run).
    assert_ne!(
        one["extent"]["temporal"]["interval"],
        entry["extent"]["temporal"]["interval"]
    );
    // The HTML page still names the run in its heading.
    let (_, _, html) = get_raw(
        "/collections/fc/instances/2026-06-07T00:00:00Z?f=html",
        None,
    )
    .await;
    assert!(html.contains("fc — run 2026-06-07T00:00:00Z"), "{html}");
}

/// ATS `/conf/instances/rc-md-success` step 1 holds the instances list to EDR
/// 1.2 `/req/core/rc-collection-info-links`: `self`, an `alternate` for every
/// other media type (HTML), and a link to a query end point; every link has
/// `rel` and `type`. A collection without runs answers the same way.
#[tokio::test]
async fn instances_list_links_follow_collection_info_links() {
    for id in ["fc", "obs"] {
        let uri = format!("/collections/{id}/instances");
        let (status, list) = get(&uri).await;
        assert_eq!(status, StatusCode::OK);
        let links = list["links"].as_array().unwrap();
        let find = |rel: &str| {
            links
                .iter()
                .find(|l| l["rel"] == rel)
                .unwrap_or_else(|| panic!("{uri}: no {rel} link in {links:?}"))
        };
        let self_href = format!("/edr/collections/{id}/instances");
        assert_eq!(find("self")["href"], self_href);
        assert_eq!(find("self")["type"], "application/json");
        let alternate = find("alternate");
        assert_eq!(alternate["type"], "text/html");
        assert_eq!(alternate["href"], format!("{self_href}?f=html"));
        let (status, ctype, _) = get_raw(&format!("{uri}?f=html"), None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(ctype.starts_with("text/html"), "{ctype}");
        assert_eq!(find("collection")["href"], format!("/edr/collections/{id}"));

        // The query end points are the collection's, less the list itself.
        let (_, collection) = get(&format!("/collections/{id}")).await;
        let expected: Vec<_> = data_links(&collection)
            .into_iter()
            .filter(|(href, _)| *href != self_href)
            .collect();
        assert!(!expected.is_empty(), "{id}");
        assert_eq!(data_links(&list), expected, "{uri}");
    }
}

// ---------------------------------------------------------------------------
// A long run axis (#1006)
// ---------------------------------------------------------------------------

/// The href of a document's link with `rel`, if any.
fn link<'a>(doc: &'a Value, rel: &str) -> Option<&'a str> {
    doc["links"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["rel"] == rel)
        .and_then(|l| l["href"].as_str())
}

/// A long run axis is not described in collection metadata: the collection
/// links to its paged instances list, and its extent is the latest run's, as
/// for any forecast. The `archive-doc` engine panics if its run axis is read,
/// so neither the collection document, in JSON or HTML, nor the
/// `/collections` list copies thousands of run keys per request. Every run
/// still resolves by its instance id, and a missing one is a 404.
#[tokio::test]
async fn a_long_run_axis_stays_out_of_collection_metadata() {
    let (status, collection) = get("/collections/archive-doc").await;
    assert_eq!(status, StatusCode::OK);
    let keys: Vec<&str> = collection["extent"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["spatial", "temporal"]);
    assert_eq!(
        collection["data_queries"]["instances"]["link"]["href"],
        "/edr/collections/archive-doc/instances"
    );
    let (status, _, _) = get_raw("/collections/archive-doc?f=html", None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, list) = get("/collections").await;
    assert_eq!(status, StatusCode::OK);
    assert!(list["collections"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["id"] == "archive-doc"));

    let runs = ArchiveMock::runs();
    for rt in [runs[0], runs[979], runs[980], *runs.last().unwrap()] {
        let id = ds_core::instances::format_instance_id(rt);
        let (status, doc) = get(&format!("/collections/archive-doc/instances/{id}")).await;
        assert_eq!(status, StatusCode::OK, "{id}");
        assert_eq!(doc["id"], id);
    }
    for missing in ArchiveMock::gap() {
        let id = ds_core::instances::format_instance_id(missing);
        let (status, _) = get(&format!("/collections/archive-doc/instances/{id}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{id}");
    }
}

/// A long run axis pages its instances list, 100 runs a page without
/// `limit`, with `numberMatched`, `numberReturned` and `next`/`prev` links
/// that walk it in ascending order. The archive once answered 95 MB here;
/// a page is bounded, and `ArchiveMock::get_instances` panics, so only the
/// page's runs are built.
#[tokio::test]
async fn a_long_run_axis_pages_the_instances_list() {
    let runs = ArchiveMock::runs();
    let app = api_edr::router(state());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/collections/archive/instances")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    // About 3 KB a run, two days of hourly valid times included.
    assert!(bytes.len() < 400_000, "{} bytes", bytes.len());
    let first: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(first["numberMatched"], runs.len());
    assert_eq!(
        first["numberReturned"],
        api_edr::handlers::INSTANCES_PAGE_SIZE
    );
    let ids: Vec<&str> = first["instances"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 100);
    assert_eq!(ids[0], "2021-05-01T00:00:00Z");
    assert_eq!(ids[99], ds_core::instances::format_instance_id(runs[99]));
    let list = "/edr/collections/archive/instances";
    assert_eq!(link(&first, "self"), Some(&*format!("{list}?limit=100")));
    assert_eq!(
        link(&first, "alternate"),
        Some(&*format!("{list}?limit=100&f=html"))
    );
    assert_eq!(
        link(&first, "next"),
        Some(&*format!("{list}?limit=100&offset=100"))
    );
    assert_eq!(link(&first, "prev"), None);
    edr_schema::assert_valid(
        "/collections/{collectionId}/instances",
        edr_schema::JSON,
        &first,
        "archive instances page",
    );

    // The last page, reached by offset: the newest runs, a `prev` link and
    // no `next`.
    let offset = runs.len() - 30;
    let (status, last) = get(&format!(
        "/collections/archive/instances?limit=100&offset={offset}"
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(last["numberReturned"], 30);
    assert_eq!(
        last["instances"].as_array().unwrap().last().unwrap()["id"],
        "2026-10-09T18:00:00Z"
    );
    assert_eq!(link(&last, "next"), None);
    assert_eq!(
        link(&last, "prev"),
        Some(&*format!("{list}?limit=100&offset={}", offset - 100))
    );

    // A `limit` past the longest whole list is clamped to it.
    let (status, clamped) = get("/collections/archive/instances?limit=10000").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        clamped["numberReturned"],
        ds_core::time_axis::MAX_LISTED_VALUES
    );
    assert_eq!(link(&clamped, "self"), Some(&*format!("{list}?limit=500")));

    // The HTML page has a pager over the same pages.
    let (status, _, html) = get_raw("/collections/archive/instances?f=html", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("rel=\"next\" href=\"/edr/collections/archive/instances?limit=100&amp;offset=100&amp;f=html\""),
        "{html}"
    );
}

/// `limit` pages a short list too, and an invalid request is a 400 naming
/// the problem, never the whole list as if it worked.
#[tokio::test]
async fn instances_paging_parameters() {
    let (status, page) = get("/collections/fc/instances?limit=1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["numberMatched"], 2);
    assert_eq!(page["numberReturned"], 1);
    assert_eq!(page["instances"][0]["id"], "2026-06-07T00:00:00Z");
    assert_eq!(
        link(&page, "next"),
        Some("/edr/collections/fc/instances?limit=1&offset=1")
    );
    let (_, second) = get("/collections/fc/instances?limit=1&offset=1").await;
    assert_eq!(second["instances"][0]["id"], "2026-06-07T12:00:00Z");
    assert_eq!(link(&second, "next"), None);
    // Without `limit` a short list stays whole, without paging members.
    let (_, whole) = get("/collections/fc/instances").await;
    assert!(whole.get("numberMatched").is_none(), "{whole}");

    for (uri, needle) in [
        (
            "/collections/fc/instances?limt=1",
            "Unknown query parameter 'limt'",
        ),
        ("/collections/fc/instances?offset=1", "requires limit"),
        // The range this list pages in, not the data queries' 10000.
        (
            "/collections/fc/instances?limit=0",
            "Invalid limit '0': expected an integer from 1 to 500",
        ),
        ("/collections/fc/instances?limit=1&limit=2", "Duplicate"),
        (
            "/collections/fc/instances?limit=1&offset=-1",
            "Invalid offset",
        ),
    ] {
        let (status, body) = get(uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        let description = body["description"].as_str().unwrap();
        assert!(description.contains(needle), "{uri}: {description}");
    }
}

/// The archive's collection and instance documents and a page of an
/// instances list validate against both EDR bundles.
#[tokio::test]
async fn long_axis_documents_validate_against_edr_bundles() {
    for (uri, path) in [
        ("/collections/archive", "/collections/{collectionId}"),
        (
            "/collections/archive/instances/2026-10-09T18:00:00Z",
            edr_schema::INSTANCE,
        ),
        (
            "/collections/fc/instances?limit=1",
            "/collections/{collectionId}/instances",
        ),
    ] {
        let (status, body) = get(uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        edr_schema::assert_valid(path, edr_schema::JSON, &body, uri);
    }
}
