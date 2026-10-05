//! EDR 1.2 `z` and `datetime` grammar through the real router (#921).
//!
//! A recording engine keeps the `z` levels and `datetime` windows each query
//! reached it with, so a test proves what the handler did with the request:
//! `z` against a collection without a vertical extent is ignored (EDR 1.2
//! `/req/edr/z-response` A) on every query route, open and recurring `z`
//! intervals resolve against a vertical collection's levels, and a
//! `datetime` list, or the repeating interval `Rn/date-time/duration` that
//! expands to one (#933), runs one query per instant and merges the answers.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, TimeZone, Timelike, Utc};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use api_edr::handlers::EdrState;
use ds_core::config::CollectionConfig;
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::instances::RunInfo;
use ds_core::model::*;
use ds_core::vertical::{VerticalDimension, VerticalKind};

type Window = (DateTime<Utc>, DateTime<Utc>);

fn at(hour: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2024, 1, 1, hour, 0, 0).unwrap()
}

/// The instant the recording engine has no data for.
const GAP_HOUR: u32 = 2;
const LEVELS: [f64; 5] = [1000.0, 850.0, 700.0, 500.0, 250.0];

#[derive(Debug, Clone, PartialEq)]
struct Call {
    datetime: Option<Window>,
    z: Option<Vec<f64>>,
}

struct Recorder {
    vertical: bool,
    calls: Mutex<Vec<Call>>,
}

impl Recorder {
    fn new(vertical: bool) -> Arc<Self> {
        Arc::new(Self {
            vertical,
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn clear(&self) {
        self.calls.lock().unwrap().clear();
    }

    /// Record the call and answer a one-step `PointSeries` at the window's
    /// start (or 00Z), valued with its hour; no data at [`GAP_HOUR`].
    fn answer(
        &self,
        datetime: Option<Window>,
        z: Option<&[f64]>,
    ) -> Result<CoverageResponse, DataServerError> {
        self.calls.lock().unwrap().push(Call {
            datetime,
            z: z.map(<[f64]>::to_vec),
        });
        let t = datetime.map_or(at(0), |(start, _)| start);
        if t == at(GAP_HOUR) {
            return Err(DataServerError::LocationNotFound("no data then".into()));
        }
        Ok(CoverageResponse::Single(QueryResult {
            domain: DomainDescription::PointSeries {
                x: 24.0,
                y: 60.0,
                t: vec![t],
                z: None,
            },
            parameters: HashMap::from([(
                "temperature".to_string(),
                ParameterDescription {
                    label: "temperature".into(),
                    unit: "K".into(),
                    observed_property: "temperature".into(),
                    standard_name: None,
                },
            )]),
            ranges: HashMap::from([(
                "temperature".to_string(),
                NdArray {
                    shape: vec![1],
                    axis_names: vec!["t".into()],
                    values: vec![Some(f64::from(t.hour()))],
                },
            )]),
        }))
    }
}

impl EdrEngine for Recorder {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        Ok(vec![Location {
            id: "site".into(),
            label: "Site".into(),
            latitude: 60.0,
            longitude: 24.0,
        }])
    }

    fn get_instances(&self) -> Vec<RunInfo> {
        vec![RunInfo {
            reference_time: at(0),
            valid_times: (0..6).map(at).collect(),
        }]
    }

    fn query_location(
        &self,
        _location_id: &str,
        datetime: Option<Window>,
        _parameters: Option<&[String]>,
        z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        self.answer(datetime, z)
    }

    fn get_parameters(&self) -> Vec<String> {
        vec!["temperature".into()]
    }

    fn get_temporal_extent(&self) -> Option<Window> {
        Some((at(0), at(5)))
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([20.0, 55.0, 30.0, 65.0])
    }

    fn get_vertical_extent(&self) -> Option<VerticalDimension> {
        self.vertical
            .then(|| VerticalDimension::new(VerticalKind::Pressure, LEVELS.to_vec()))
    }

    fn supported_query_types(&self) -> Vec<String> {
        ["locations", "position", "area", "radius", "trajectory"]
            .map(String::from)
            .to_vec()
    }

    fn query_area(
        &self,
        _coords: &str,
        datetime: Option<Window>,
        _parameters: Option<&[String]>,
        z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        self.answer(datetime, z)
    }

    fn query_position(
        &self,
        _coords: &str,
        datetime: Option<Window>,
        _parameters: Option<&[String]>,
        z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        self.answer(datetime, z)
    }

    fn query_trajectory(
        &self,
        _coords: &str,
        datetime: Option<Window>,
        _parameters: Option<&[String]>,
        z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        self.answer(datetime, z)
    }
}

fn config(id: &str) -> CollectionConfig {
    CollectionConfig {
        id: id.to_string(),
        title: id.to_string(),
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
        bufr: None,
        satellite: None,
        preview: None,
        derive_wind: None,
    }
}

/// A router serving `flat` (no vertical extent) and `levels` (pressure).
fn router(flat: &Arc<Recorder>, levels: &Arc<Recorder>) -> axum::Router {
    let flat_engine: Arc<dyn EdrEngine> = flat.clone();
    let levels_engine: Arc<dyn EdrEngine> = levels.clone();
    let engines = HashMap::from([
        ("flat".to_string(), flat_engine),
        ("levels".to_string(), levels_engine),
    ]);
    let collections = HashMap::from([
        ("flat".to_string(), config("flat")),
        ("levels".to_string(), config("levels")),
    ]);
    api_edr::router(Arc::new(ArcSwap::from_pointee(EdrState {
        engines,
        collections,
        styles: HashMap::new(),
        feature_engines: HashMap::new(),
        base_url: String::new(),
        trust_proxy_headers: false,
    })))
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Value, Option<String>) {
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let cache_control = resp
        .headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, json, cache_control)
}

/// Every EDR data-query route, with `{z}` for the `z` value.
const ROUTES: [&str; 8] = [
    "/collections/{c}/locations/site?z={z}",
    "/collections/{c}/position?coords=POINT(24%2060)&z={z}",
    "/collections/{c}/area?coords=POLYGON((23%2059,25%2059,25%2061,23%2061,23%2059))&z={z}",
    "/collections/{c}/radius?coords=POINT(24%2060)&within=10&within-units=km&z={z}",
    "/collections/{c}/trajectory?coords=LINESTRING(24%2060,25%2061)&z={z}",
    "/collections/{c}/instances/20240101T0000Z/position?coords=POINT(24%2060)&z={z}",
    "/collections/{c}/instances/20240101T0000Z/area?coords=POLYGON((23%2059,25%2059,25%2061,23%2061,23%2059))&z={z}",
    "/collections/{c}/instances/20240101T0000Z/radius?coords=POINT(24%2060)&within=10&within-units=km&z={z}",
];

fn route(template: &str, collection: &str, z: &str) -> String {
    template.replace("{c}", collection).replace("{z}", z)
}

#[tokio::test]
async fn z_is_ignored_without_a_vertical_extent_on_every_route() {
    let (flat, levels) = (Recorder::new(false), Recorder::new(true));
    let app = router(&flat, &levels);
    for template in ROUTES {
        for z in [
            "850",
            "850,700",
            "100/550",
            "../850",
            "500/..",
            "R20/100/50",
        ] {
            flat.clear();
            let uri = route(template, "flat", z);
            let (status, body, _) = get(&app, &uri).await;
            assert_eq!(status, StatusCode::OK, "{uri}: {body}");
            let calls = flat.calls();
            assert_eq!(calls.len(), 1, "{uri}");
            assert_eq!(calls[0].z, None, "{uri}: z must not reach the engine");
        }
    }
}

#[tokio::test]
async fn malformed_z_is_a_400_even_without_a_vertical_extent() {
    let (flat, levels) = (Recorder::new(false), Recorder::new(true));
    let app = router(&flat, &levels);
    for z in [
        "abc",
        "../..",
        "1/2/3",
        "R0/100/50",
        "R1001/0/1",
        "R3/100/0",
    ] {
        let uri = route(ROUTES[1], "flat", z);
        let (status, body, _) = get(&app, &uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body}");
    }
    assert!(flat.calls().is_empty(), "no engine call for a malformed z");
}

/// #940: a `z` list longer than the cap is a 400 naming it on every route,
/// with or without a vertical extent, before any engine call.
#[tokio::test]
async fn z_list_over_the_cap_is_a_400_before_the_engine() {
    use api_edr::params::MAX_Z_LEVELS;
    let (flat, levels) = (Recorder::new(false), Recorder::new(true));
    let app = router(&flat, &levels);
    let at_cap = vec!["850"; MAX_Z_LEVELS].join(",");
    let (status, body, _) = get(&app, &route(ROUTES[1], "levels", &at_cap)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a list at the cap is served: {body}"
    );
    levels.clear();
    let over = vec!["850"; MAX_Z_LEVELS + 1].join(",");
    for template in ROUTES {
        for collection in ["flat", "levels"] {
            let (status, body, _) = get(&app, &route(template, collection, &over)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{template}");
            let message = body["description"].as_str().unwrap();
            assert!(
                message.contains(&format!("maximum of {MAX_Z_LEVELS}")),
                "{template}: {message}"
            );
        }
    }
    assert!(flat.calls().is_empty() && levels.calls().is_empty());
}

#[tokio::test]
async fn z_is_honoured_with_a_vertical_extent() {
    let (flat, levels) = (Recorder::new(false), Recorder::new(true));
    let app = router(&flat, &levels);
    let cases: [(&str, Vec<f64>); 6] = [
        ("850", vec![850.0]),
        ("850,500", vec![850.0, 500.0]),
        ("500/850", vec![850.0, 700.0, 500.0]),
        // Open ends reach the lowest / highest advertised level.
        ("../700", vec![700.0, 500.0, 250.0]),
        ("700/..", vec![1000.0, 850.0, 700.0]),
        // A recurring interval becomes the list the engine snaps.
        ("R3/1000/-150", vec![1000.0, 850.0, 700.0]),
    ];
    for template in ROUTES {
        for (z, expected) in &cases {
            levels.clear();
            let uri = route(template, "levels", z);
            let (status, body, _) = get(&app, &uri).await;
            assert_eq!(status, StatusCode::OK, "{uri}: {body}");
            assert_eq!(levels.calls()[0].z.as_ref(), Some(expected), "{uri}");
        }
    }
    // An open interval past every level selects none: 400.
    let (status, body, _) = get(&app, &route(ROUTES[0], "levels", "../100")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["description"].as_str().unwrap().contains("../100"),
        "{body}"
    );
}

/// `T3,T1,T2`: each instant is queried alone, ascending; the instant with
/// no data drops out and the rest merge into one series.
#[tokio::test]
async fn datetime_list_queries_each_instant_and_merges() {
    let (flat, levels) = (Recorder::new(false), Recorder::new(true));
    let app = router(&flat, &levels);
    let list = "2024-01-01T03:00:00Z,2024-01-01T01:00:00Z,2024-01-01T02:00:00Z";
    let data_routes = [
        format!("/collections/flat/locations/site?datetime={list}"),
        format!("/collections/flat/position?coords=POINT(24%2060)&datetime={list}"),
        format!(
            "/collections/flat/area?coords=POLYGON((23%2059,25%2059,25%2061,23%2061,23%2059))&datetime={list}"
        ),
        format!(
            "/collections/flat/radius?coords=POINT(24%2060)&within=10&within-units=km&datetime={list}"
        ),
        format!("/collections/flat/trajectory?coords=LINESTRING(24%2060,25%2061)&datetime={list}"),
        format!(
            "/collections/flat/instances/20240101T0000Z/position?coords=POINT(24%2060)&datetime={list}"
        ),
    ];
    for uri in &data_routes {
        flat.clear();
        let (status, body, cache_control) = get(&app, uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        let windows: Vec<Option<Window>> = flat.calls().into_iter().map(|c| c.datetime).collect();
        assert_eq!(
            windows,
            vec![
                Some((at(1), at(1))),
                Some((at(2), at(2))),
                Some((at(3), at(3)))
            ],
            "{uri}"
        );
        assert_eq!(body["type"], "Coverage", "{uri}: {body}");
        assert_eq!(
            body["domain"]["axes"]["t"]["values"],
            serde_json::json!([at(1).to_rfc3339(), at(3).to_rfc3339()]),
            "{uri}"
        );
        assert_eq!(
            body["ranges"]["temperature"]["values"],
            serde_json::json!([1.0, 3.0]),
            "{uri}"
        );
        // Every listed instant is long past: the settled policy.
        assert!(
            cache_control.as_deref().unwrap_or("").contains("86400"),
            "{uri}: {cache_control:?}"
        );
    }
}

#[tokio::test]
async fn datetime_list_without_data_at_any_instant_is_404() {
    let (flat, levels) = (Recorder::new(false), Recorder::new(true));
    let app = router(&flat, &levels);
    let (status, body, _) = get(
        &app,
        "/collections/flat/locations/site?datetime=2024-01-01T02:00:00Z,2024-01-01T02:00:00%2B00:00",
    )
    .await;
    // Both elements name the same instant, so it is one query — and a 404.
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(flat.calls().len(), 1);
}

#[tokio::test]
async fn malformed_datetime_lists_are_400() {
    let (flat, levels) = (Recorder::new(false), Recorder::new(true));
    let app = router(&flat, &levels);
    let too_many = (0..65)
        .map(|m| format!("2024-01-01T00:{:02}:00Z", m % 60))
        .collect::<Vec<_>>()
        .join(",");
    for list in [
        "2024-01-01T00:00:00Z,2024-01-01T01:00:00Z/2024-01-01T02:00:00Z",
        "2024-01-01T00:00:00Z,,2024-01-01T01:00:00Z",
        "2024-01-01T00:00:00Z,not-a-date",
        too_many.as_str(),
    ] {
        let uri = format!("/collections/flat/locations/site?datetime={list}");
        let (status, body, _) = get(&app, &uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{list}: {body}");
    }
    assert!(flat.calls().is_empty());
}

/// A MULTIPOINT × datetime-list position request is capped on the product of
/// the two (`MAX_POSITION_LOOKUPS`): each instant re-queries every point.
#[tokio::test]
async fn multipoint_times_datetime_list_is_capped() {
    let (flat, levels) = (Recorder::new(false), Recorder::new(true));
    let app = router(&flat, &levels);
    let points = (0..20)
        .map(|i| format!("{}%20{}", 20 + i % 10, 60 + i / 10))
        .collect::<Vec<_>>()
        .join(",");
    let instants = (0..16)
        .map(|m| format!("2024-01-01T00:{m:02}:00Z"))
        .collect::<Vec<_>>()
        .join(",");
    let uri = format!("/collections/flat/position?coords=MULTIPOINT({points})&datetime={instants}");
    let (status, body, _) = get(&app, &uri).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("320 position lookups"), "{body}");
    assert!(flat.calls().is_empty(), "rejected before any engine call");
}

/// A repeating interval `R3/T/PT1H` (#933) is the list of its three
/// instants: on every data route it makes the same engine calls and gets
/// the same response, status, body and `Cache-Control`, as the list
/// `T,T+1h,T+2h` — including the instant without data dropping out.
#[tokio::test]
async fn repeating_datetime_answers_like_the_equivalent_list() {
    let (flat, levels) = (Recorder::new(false), Recorder::new(true));
    let app = router(&flat, &levels);
    let repeating = "R3/2024-01-01T01:00:00Z/PT1H";
    let list = "2024-01-01T01:00:00Z,2024-01-01T02:00:00Z,2024-01-01T03:00:00Z";
    let templates = [
        "/collections/flat/locations/site?datetime={dt}",
        "/collections/flat/position?coords=POINT(24%2060)&datetime={dt}",
        "/collections/flat/position?coords=MULTIPOINT((24%2060),(25%2061))&datetime={dt}",
        "/collections/flat/area?coords=POLYGON((23%2059,25%2059,25%2061,23%2061,23%2059))&datetime={dt}",
        "/collections/flat/radius?coords=POINT(24%2060)&within=10&within-units=km&datetime={dt}",
        "/collections/flat/trajectory?coords=LINESTRING(24%2060,25%2061)&datetime={dt}",
        "/collections/flat/instances/20240101T0000Z/position?coords=POINT(24%2060)&datetime={dt}",
    ];
    for template in templates {
        flat.clear();
        let uri = template.replace("{dt}", repeating);
        let got = get(&app, &uri).await;
        let got_calls = flat.calls();
        flat.clear();
        let want = get(&app, &template.replace("{dt}", list)).await;
        assert_eq!(got_calls, flat.calls(), "{uri}");
        assert_eq!(got, want, "{uri}");
        assert_eq!(got.0, StatusCode::OK, "{uri}: {}", got.1);
    }
    // The position answer itself: one series over the instants with data.
    let (_, body, _) = get(
        &app,
        &format!("/collections/flat/position?coords=POINT(24%2060)&datetime={repeating}"),
    )
    .await;
    assert_eq!(
        body["domain"]["axes"]["t"]["values"],
        serde_json::json!([at(1).to_rfc3339(), at(3).to_rfc3339()]),
        "{body}"
    );
    assert_eq!(
        body["ranges"]["temperature"]["values"],
        serde_json::json!([1.0, 3.0]),
        "{body}"
    );
}

#[tokio::test]
async fn malformed_repeating_datetimes_are_400() {
    let (flat, levels) = (Recorder::new(false), Recorder::new(true));
    let app = router(&flat, &levels);
    for (value, fragment) in [
        ("R0/2024-01-01T00:00:00Z/PT1H", "R0"),
        ("R/2024-01-01T00:00:00Z/PT1H", "unbounded"),
        ("R17/2024-01-01T00:00:00Z/PT1H", "the maximum is 16"),
        ("R3/2024-01-01T00:00:00Z/PT0S", "zero or negative"),
        ("R3/2024-01-01T00:00:00Z/P1M", "months"),
        ("R3/2024-01-01T00:00:00Z", "Rn/date-time/duration"),
        ("R3/PT1H/2024-01-01T00:00:00Z", "RFC 3339"),
    ] {
        let uri = format!("/collections/flat/position?coords=POINT(24%2060)&datetime={value}");
        let (status, body, _) = get(&app, &uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{value}: {body}");
        let description = body["description"].as_str().unwrap_or_default();
        assert!(description.contains(fragment), "{value}: {body}");
    }
    assert!(
        flat.calls().is_empty(),
        "no engine call for a malformed value"
    );

    // The expanded instants count toward MULTIPOINT points × instants, as a
    // list's do: 20 points × R16 is 320 lookups.
    let points = (0..20)
        .map(|i| format!("{}%20{}", 20 + i % 10, 60 + i / 10))
        .collect::<Vec<_>>()
        .join(",");
    let uri = format!(
        "/collections/flat/position?coords=MULTIPOINT({points})&datetime=R16/2024-01-01T00:00:00Z/PT1M"
    );
    let (status, body, _) = get(&app, &uri).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("320 position lookups"), "{body}");
    assert!(flat.calls().is_empty(), "rejected before any engine call");
}
