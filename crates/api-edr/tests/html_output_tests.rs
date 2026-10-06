//! HTML for every EDR data query (#971).
//!
//! OGC API - EDR 1.2 `/req/html/definition` A: every 200 response of every
//! operation supports `text/html`. `/req/html/content` A: the page is an
//! HTML 5 document with all the response's information and every link in an
//! `<a>`. Every data query, the `/locations` list and `items` answer
//! `f=html` and a browser's `Accept`; the page escapes what it shows and
//! lists every value of the CoverageJSON.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use chrono::{DateTime, Duration, TimeZone, Utc};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::util::ServiceExt;

use api_edr::handlers::EdrState;
use ds_core::config::CollectionConfig;
use ds_core::cube::CubeResolution;
use ds_core::edr_engine::{EdrEngine, TrajectoryShape};
use ds_core::error::DataServerError;
use ds_core::feature::{Bbox, Feature, FeaturePage, FeatureQuery, Geometry, PropertyValue};
use ds_core::feature_engine::FeatureEngine;
use ds_core::instances::RunInfo;
use ds_core::model::*;
use ds_core::vertical::VerticalKind;

const HTML: &str = "text/html; charset=utf-8";
const BROWSER: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 7, 0, 0, 0).unwrap()
}

fn parameters() -> HashMap<String, ParameterDescription> {
    HashMap::from([(
        "temperature".to_string(),
        ParameterDescription {
            label: "<script>alert(1)</script>".to_string(),
            unit: "degC".to_string(),
            observed_property: "temperature".to_string(),
            standard_name: Some("air_temperature".to_string()),
        },
    )])
}

fn coverage(
    domain: DomainDescription,
    axes: &[&str],
    shape: &[usize],
    values: &[f64],
) -> QueryResult {
    QueryResult {
        domain,
        parameters: parameters(),
        ranges: HashMap::from([(
            "temperature".to_string(),
            NdArray {
                shape: shape.to_vec(),
                axis_names: axes.iter().map(|a| a.to_string()).collect(),
                values: values.iter().map(|&v| Some(v)).collect(),
            },
        )]),
    }
}

fn point_series() -> CoverageResponse {
    CoverageResponse::Single(coverage(
        DomainDescription::PointSeries {
            x: 25.0,
            y: 60.0,
            t: vec![t0(), t0() + Duration::hours(1)],
            z: None,
        },
        &["t"],
        &[2],
        &[1.5, 2.5],
    ))
}

/// A 2 × 2 × 3 `[t, y, x]` grid holding 101…112.
fn grid() -> CoverageResponse {
    let values: Vec<f64> = (101..=112).map(f64::from).collect();
    CoverageResponse::Single(coverage(
        DomainDescription::Grid {
            x: vec![24.0, 24.5, 25.0],
            y: vec![60.0, 60.5],
            t: Some(vec![t0(), t0() + Duration::hours(1)]),
            z: None,
        },
        &["t", "y", "x"],
        &[2, 2, 3],
        &values,
    ))
}

/// Every query type, one model run and the features `items` serves.
struct Every;

impl EdrEngine for Every {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        Ok(vec![
            Location {
                id: "here".into(),
                label: "<b>Here</b>".into(),
                latitude: 60.0,
                longitude: 25.0,
            },
            Location {
                id: "a b".into(),
                label: "Second".into(),
                latitude: 61.0,
                longitude: 26.0,
            },
        ])
    }

    fn get_instances(&self) -> Vec<RunInfo> {
        vec![RunInfo {
            reference_time: t0(),
            valid_times: vec![t0(), t0() + Duration::hours(1)],
        }]
    }

    fn query_location(
        &self,
        _: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Ok(point_series())
    }

    fn query_position(
        &self,
        _: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Ok(point_series())
    }

    fn query_area(
        &self,
        _: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Ok(grid())
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
        Ok(grid())
    }

    fn query_trajectory(
        &self,
        _: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Ok(point_series())
    }

    fn get_parameters(&self) -> Vec<String> {
        vec!["temperature".to_string()]
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        Some((t0(), t0() + Duration::hours(1)))
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([20.0, 58.0, 30.0, 62.0])
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
}

impl FeatureEngine for Every {
    fn get_features(&self, query: &FeatureQuery) -> Result<FeaturePage, DataServerError> {
        let all: Vec<Feature> = (0..3).map(station).collect();
        let features: Vec<Feature> = all
            .into_iter()
            .skip(query.offset)
            .take(query.limit)
            .collect();
        Ok(FeaturePage {
            number_returned: features.len(),
            next_offset: (query.offset + features.len() < 3)
                .then_some(query.offset + features.len()),
            features,
            number_matched: 3,
        })
    }

    fn get_feature(&self, id: &str) -> Result<Feature, DataServerError> {
        (0..3)
            .map(station)
            .find(|f| f.id == id)
            .ok_or_else(|| DataServerError::FeatureNotFound(id.into()))
    }

    fn has_time_dimension(&self) -> bool {
        false
    }
}

fn station(i: usize) -> Feature {
    Feature {
        id: format!("s{i}"),
        geometry: Arc::new(Geometry::Point {
            x: 20.0 + i as f64,
            y: 60.0,
        }),
        properties: Arc::new(HashMap::from([(
            "name".to_string(),
            PropertyValue::String(format!("<i>S{i}</i>")),
        )])),
    }
}

/// A radar site: its trajectory is a cross-section.
struct Section;

impl EdrEngine for Section {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        Ok(Vec::new())
    }

    fn query_location(
        &self,
        id: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Err(DataServerError::LocationNotFound(id.into()))
    }

    fn query_position(
        &self,
        _: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Err(DataServerError::InvalidParameter("no".into()))
    }

    fn query_trajectory(
        &self,
        _: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Ok(CoverageResponse::Single(coverage(
            DomainDescription::Section {
                nodes: vec![(t0(), 24.5, 60.25), (t0(), 25.5, 60.75)],
                z: VerticalCoord {
                    kind: VerticalKind::Height,
                    values: vec![500.0, 1000.0],
                },
                coverage_floor: Some(vec![120.5, 340.25]),
            },
            &["composite", "z"],
            &[2, 2],
            &[11.0, 12.0, 13.0, 14.0],
        )))
    }

    fn get_parameters(&self) -> Vec<String> {
        vec!["temperature".to_string()]
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        Some((t0(), t0()))
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([20.0, 58.0, 30.0, 62.0])
    }

    fn supported_query_types(&self) -> Vec<String> {
        vec!["trajectory".to_string()]
    }

    fn trajectory_shape(&self) -> TrajectoryShape {
        TrajectoryShape::CrossSection
    }
}

/// A station collection (`serves_station_series`): each position, radius
/// and location answer is a series at a listed location, so it is also
/// offered as EDR GeoJSON naming each station.
struct Stations;

impl Stations {
    fn at(x: f64, y: f64) -> QueryResult {
        coverage(
            DomainDescription::PointSeries {
                x,
                y,
                t: vec![t0(), t0() + Duration::hours(1)],
                z: None,
            },
            &["t"],
            &[2],
            &[1.5, 2.5],
        )
    }
}

impl EdrEngine for Stations {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        Every.get_locations()
    }

    fn serves_station_series(&self) -> bool {
        true
    }

    fn query_location(
        &self,
        id: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        match id {
            "here" => Ok(CoverageResponse::Single(Self::at(25.0, 60.0))),
            "a b" => Ok(CoverageResponse::Single(Self::at(26.0, 61.0))),
            _ => Err(DataServerError::LocationNotFound(id.into())),
        }
    }

    fn query_position(
        &self,
        coords: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        let (lat, lon) = ds_core::feature::parse_point_coords(coords)?;
        Ok(CoverageResponse::Single(Self::at(lon, lat)))
    }

    /// Both stations, whatever the area: the radius query's answer.
    fn query_area(
        &self,
        _: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Ok(CoverageResponse::Collection(vec![
            Self::at(25.0, 60.0),
            Self::at(26.0, 61.0),
        ]))
    }

    fn get_parameters(&self) -> Vec<String> {
        vec!["temperature".to_string()]
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        Some((t0(), t0() + Duration::hours(1)))
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([20.0, 58.0, 30.0, 62.0])
    }

    fn supported_query_types(&self) -> Vec<String> {
        ["locations", "position", "area", "radius"]
            .map(String::from)
            .to_vec()
    }
}

fn config(id: &str, title: &str) -> CollectionConfig {
    CollectionConfig {
        id: id.to_string(),
        title: title.to_string(),
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

fn router() -> axum::Router {
    let every = Arc::new(Every);
    let mut engines: HashMap<String, Arc<dyn EdrEngine>> = HashMap::new();
    engines.insert("c".to_string(), every.clone());
    engines.insert("pvol".to_string(), Arc::new(Section));
    engines.insert("obs".to_string(), Arc::new(Stations));
    let mut feature_engines: HashMap<String, Arc<dyn FeatureEngine>> = HashMap::new();
    feature_engines.insert("c".to_string(), every);
    let collections = HashMap::from([
        ("c".to_string(), config("c", "Every <query> & \"more\"")),
        ("pvol".to_string(), config("pvol", "Radar site")),
        ("obs".to_string(), config("obs", "Stations")),
    ]);
    api_edr::router(Arc::new(ArcSwap::from_pointee(EdrState {
        engines,
        collections,
        styles: HashMap::new(),
        feature_engines,
        base_url: "https://example.org".to_string(),
        trust_proxy_headers: false,
    })))
}

/// `(status, headers, body)` for a GET with an optional `Accept`.
async fn get(uri: &str, accept: Option<&str>) -> (StatusCode, HeaderMap, String) {
    let mut req = Request::builder().uri(uri);
    if let Some(accept) = accept {
        req = req.header(header::ACCEPT, accept);
    }
    let resp = router()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let (status, headers) = (resp.status(), resp.headers().clone());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
}

const POINT: &str = "POINT(25%2060)";
const POLYGON: &str = "POLYGON((24%2059,26%2059,26%2061,24%2061,24%2059))";
const INSTANCE: &str = "2026-06-07T00:00:00Z";

/// Every route answering 200, instance-scoped ones included.
fn routes() -> Vec<String> {
    let radius = format!("radius?coords={POINT}&within=10&within-units=km");
    vec![
        "/collections/c/locations".to_string(),
        "/collections/c/locations/here".to_string(),
        "/collections/c/locations/here,a%20b".to_string(),
        format!("/collections/c/position?coords={POINT}"),
        format!("/collections/c/area?coords={POLYGON}"),
        format!("/collections/c/{radius}"),
        "/collections/c/trajectory?coords=LINESTRING(24%2060,25%2061)".to_string(),
        "/collections/c/cube?bbox=24,59,26,61".to_string(),
        "/collections/pvol/trajectory?coords=LINESTRING(24%2060,25%2061)".to_string(),
        format!("/collections/c/instances/{INSTANCE}/position?coords={POINT}"),
        format!("/collections/c/instances/{INSTANCE}/area?coords={POLYGON}"),
        format!("/collections/c/instances/{INSTANCE}/{radius}"),
        format!("/collections/c/instances/{INSTANCE}/cube?bbox=24,59,26,61"),
        "/collections/c/items".to_string(),
        "/collections/c/items/s1".to_string(),
    ]
}

fn with_f(uri: &str, f: &str) -> String {
    let sep = if uri.contains('?') { '&' } else { '?' };
    format!("{uri}{sep}f={f}")
}

fn assert_html_page(uri: &str, headers: &HeaderMap, body: &str) {
    assert_eq!(headers[header::CONTENT_TYPE], HTML, "{uri}");
    assert!(body.starts_with("<!DOCTYPE html>"), "{uri}");
    assert!(body.ends_with("</html>"), "{uri}");
}

#[tokio::test]
async fn every_query_answers_f_html() {
    for uri in routes() {
        for f in ["html", "HTML", "text%2Fhtml"] {
            let uri = with_f(&uri, f);
            let (status, headers, body) = get(&uri, None).await;
            assert_eq!(status, StatusCode::OK, "{uri}: {body}");
            assert_html_page(&uri, &headers, &body);
        }
    }
}

/// A browser names `text/html` explicitly and gets the page, with `Vary`.
/// An explicit data type at the same q, a wildcard or an explicit `f` keep
/// the data.
#[tokio::test]
async fn accept_negotiates_html_below_the_data_formats() {
    for uri in routes() {
        let (status, headers, body) = get(&uri, Some(BROWSER)).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        assert_html_page(&uri, &headers, &body);
        assert_eq!(headers[header::VARY], "accept", "{uri}");
        // A data type the route offers, named beside HTML at the same q.
        for accept in [
            "*/*",
            "text/html, application/vnd.cov+json, application/geo+json",
        ] {
            let (status, headers, _) = get(&uri, Some(accept)).await;
            assert_eq!(status, StatusCode::OK, "{uri}");
            assert_ne!(
                headers[header::CONTENT_TYPE],
                HTML,
                "{uri} Accept: {accept}"
            );
        }
    }
    let (_, headers, _) = get(
        &format!("/collections/c/area?coords={POLYGON}&f=CoverageJSON"),
        Some(BROWSER),
    )
    .await;
    assert_eq!(headers[header::CONTENT_TYPE], "application/vnd.cov+json");
}

/// A script or `fetch()` (`Accept: */*`) and a client asking for
/// `application/json` get what they got before HTML existed: the same
/// data format and body as without `Accept`.
#[tokio::test]
async fn wildcard_and_json_accept_keep_the_data_formats() {
    // `items` stamps its generation time; compare the rest.
    let comparable = |body: &str| {
        let mut doc: Value = serde_json::from_str(body).expect("a JSON body");
        doc.as_object_mut().map(|o| o.remove("timeStamp"));
        doc
    };
    for uri in routes() {
        let (status, plain, body) = get(&uri, None).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        let media = plain[header::CONTENT_TYPE].to_str().unwrap().to_owned();
        assert!(
            ["application/vnd.cov+json", "application/geo+json"].contains(&media.as_str()),
            "{uri}: {media}"
        );
        for accept in ["*/*", "application/json"] {
            let (status, headers, other) = get(&uri, Some(accept)).await;
            assert_eq!(status, StatusCode::OK, "{uri} Accept: {accept}");
            assert_eq!(
                headers[header::CONTENT_TYPE],
                media.as_str(),
                "{uri} Accept: {accept}"
            );
            assert_eq!(
                comparable(&other),
                comparable(&body),
                "{uri} Accept: {accept}"
            );
        }
    }
}

/// Every value of the grid is a cell, and the parameter, the request and
/// the collection title are escaped.
#[tokio::test]
async fn the_page_lists_every_value_escaped() {
    let uri = format!("/collections/c/area?coords={POLYGON}&parameter-name=%3Cimg%3E&f=html");
    let (status, _, body) = get(&uri, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for v in 101..=112 {
        assert!(body.contains(&format!("<td>{v}.0</td>")), "{v}");
    }
    // One y × x matrix per timestep.
    assert_eq!(body.matches("<caption>").count(), 2);
    for raw in ["<script>alert", "<img>", "<query>"] {
        assert!(!body.contains(raw), "{raw} unescaped");
    }
    assert!(body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(body.contains("&lt;img&gt;"));
    assert!(body.contains("Every &lt;query&gt; &amp; &quot;more&quot;"));
    // The cross-section's composite axis and foreign member are there too.
    let (_, _, body) = get(
        "/collections/pvol/trajectory?coords=LINESTRING(24%2060,25%2061)&f=html",
        None,
    )
    .await;
    assert!(
        body.contains("<td>25.5</td><td>60.75</td><td>1000.0</td><td>14.0</td>"),
        "{body}"
    );
    assert!(body.contains("meteocore:beamCoverage") && body.contains("340.25"));
}

/// Every link is an `<a>`: the page's own (`self`, an `alternate` per data
/// format, the collection), the observed property, the locations' data
/// links and the items' links.
#[tokio::test]
async fn links_are_anchors() {
    let anchored = |body: &str, href: &str| body.contains(&format!("href=\"{href}\""));
    let base = "https://example.org/edr/collections/c";
    let (_, _, body) = get(
        &format!("/collections/c/position?coords={POINT}&f=html"),
        None,
    )
    .await;
    for href in [
        format!("{base}/position?coords={POINT}&amp;f=HTML"),
        format!("{base}/position?coords={POINT}&amp;f=CoverageJSON"),
        format!("{base}/position?coords={POINT}&amp;f=PNG"),
        base.to_string(),
        "https://vocab.nerc.ac.uk/standard_name/air_temperature".to_string(),
    ] {
        assert!(anchored(&body, &href), "{href}: {body}");
    }

    let (_, _, body) = get("/collections/c/locations?f=html", None).await;
    for href in [
        format!("{base}/locations/here"),
        format!("{base}/locations/a%20b"),
        format!("{base}/locations/here?f=html"),
        format!("{base}/locations?f=html"),
        format!("{base}/locations?f=GeoJSON"),
    ] {
        assert!(anchored(&body, &href), "{href}: {body}");
    }
    assert!(body.contains("&lt;b&gt;Here&lt;/b&gt;") && !body.contains("<b>Here"));

    // Every per-feature link of the GeoJSON, and the next page as HTML.
    let (_, _, json) = get("/collections/c/items?limit=2", None).await;
    let json: Value = serde_json::from_str(&json).unwrap();
    let (_, _, body) = get("/collections/c/items?limit=2&f=html", None).await;
    for feature in json["features"].as_array().unwrap() {
        for link in feature["links"].as_array().unwrap() {
            let href = link["href"].as_str().unwrap();
            assert!(anchored(&body, href), "{href}: {body}");
        }
    }
    let next = json["links"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["rel"] == "next")
        .and_then(|l| l["href"].as_str())
        .unwrap();
    assert!(body.contains("rel") && body.contains("next"));
    assert!(body.contains(&next.replace('&', "&amp;")), "{next}");
    assert!(body.contains("&lt;i&gt;S0&lt;/i&gt;") && !body.contains("<i>S0"));
    // The EDR members (#970) are columns, the query endpoint an anchor.
    for member in ["datetime", "parameter-name", "label", "edrqueryendpoint"] {
        assert!(
            body.contains(&format!("<th scope=\"col\">{member}</th>")),
            "{member}"
        );
    }
    for feature in json["features"].as_array().unwrap() {
        let endpoint = feature["properties"]["edrqueryendpoint"].as_str().unwrap();
        assert!(
            anchored(&body, &endpoint.replace('&', "&amp;")),
            "{endpoint}"
        );
    }
}

/// The collection and instance pages anchor every `data_queries` end point
/// (`/req/html/content`), though their link lists leave `rel=data` out
/// (#980): the page lists those end points from `data_queries`.
#[tokio::test]
async fn collection_pages_anchor_their_data_queries() {
    let base = "https://example.org/edr/collections/c";
    for (uri, queries) in [
        (
            "/collections/c?f=html",
            &["position", "area", "radius", "trajectory", "cube"][..],
        ),
        (
            &*format!("/collections/c/instances/{INSTANCE}?f=html"),
            &["position", "area", "radius", "cube"][..],
        ),
    ] {
        let (status, _, body) = get(uri, None).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        for q in queries {
            let href = if uri.contains("/instances/") {
                format!("{base}/instances/{INSTANCE}/{q}")
            } else {
                format!("{base}/{q}")
            };
            assert!(body.contains(&format!("href=\"{href}\"")), "{uri}: {href}");
        }
    }
    let (_, _, body) = get("/collections/c?f=html", None).await;
    for q in ["locations", "items", "instances"] {
        assert!(body.contains(&format!("href=\"{base}/{q}?f=html\"")), "{q}");
    }
}

/// The string leaves of a JSON document, with the member each sits under.
fn string_leaves<'a>(value: &'a Value, key: &'a str, out: &mut Vec<(&'a str, &'a str)>) {
    match value {
        Value::String(s) => out.push((key, s)),
        Value::Array(values) => values.iter().for_each(|v| string_leaves(v, key, out)),
        Value::Object(members) => members.iter().for_each(|(k, v)| string_leaves(v, k, out)),
        _ => {}
    }
}

/// `/req/html/content` A, first bullet: the HTML of every metadata and
/// list response holds all the information of its JSON. Every string the
/// JSON carries is in the page body, as text or inside a JSON block —
/// the list pages' collections and instances in full (#984), and every
/// link's rel, type and title, not only its href.
#[tokio::test]
async fn html_pages_hold_every_json_string() {
    use ds_core::html::escape;
    for uri in [
        "/",
        "/collections",
        "/collections/c",
        "/collections/c/instances",
        &*format!("/collections/c/instances/{INSTANCE}"),
        "/collections/c/locations",
        "/collections/c/items?limit=2",
        "/collections/c/items/s1",
    ] {
        let (status, _, json) = get(uri, Some("application/json")).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        let mut json: Value = serde_json::from_str(&json).unwrap();
        // The document's own `self` and `alternate` describe the JSON
        // representation; the page lists its own (the HTML as `self`).
        if let Some(links) = json["links"].as_array_mut() {
            links.retain(|l| l["rel"] != "self" && l["rel"] != "alternate");
        }
        let (status, _, html) = get(uri, Some(BROWSER)).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        let body = html.split_once("<body").map_or(&*html, |(_, b)| b);
        let mut leaves = Vec::new();
        string_leaves(&json, "", &mut leaves);
        for (key, leaf) in leaves {
            if key == "timeStamp" {
                continue;
            }
            // A string inside a JSON block is JSON-escaped first.
            let quoted = serde_json::to_string(leaf).unwrap();
            let in_json = escape(&quoted[1..quoted.len() - 1]);
            assert!(
                body.contains(&escape(leaf)) || body.contains(&in_json),
                "{uri}: {key} = {leaf}"
            );
        }
    }
}

/// Links are listed with their relation, type and title, at their own
/// href: a `rel=data` query end point is never opened as a bare `?f=html`
/// page (#980, #984).
#[tokio::test]
async fn link_tables_keep_rel_type_and_title() {
    let (_, _, body) = get("/?f=html", None).await;
    for text in [
        "http://www.opengis.net/def/rel/ogc/1.0/conformance",
        "application/vnd.oai.openapi+json;version=3.0",
    ] {
        assert!(
            body.contains(&format!("<td>{text}</td>"))
                || body.contains(&format!("<code>{text}</code>")),
            "{text}"
        );
    }
    assert!(body.contains("<td>Conformance classes</td>"));
    let base = "https://example.org/edr/collections/c";
    let (_, _, body) = get("/collections/c?f=html", None).await;
    assert!(body.contains(&format!(
        "<tr><td><code>data</code></td><td>Instances (forecast model runs)</td><td>application/json</td><td><a class=\"table-link\" href=\"{base}/instances\">"
    )));
    assert!(body.contains(&format!("href=\"{base}/position\"")));
    assert!(!body.contains("/position?f=html"));

    // A location's link and geometry type, an item's link type.
    let (_, _, body) = get("/collections/c/locations?f=html", None).await;
    assert!(body.contains("<td>Point</td>"), "{body}");
    assert!(body.contains(&format!(
        "<code>data</code> <a class=\"table-link\" href=\"{base}/locations/here\">Data for &lt;b&gt;Here&lt;/b&gt;</a> <code>application/vnd.cov+json</code>"
    )), "{body}");
    let (_, _, body) = get("/collections/c/items?limit=2&f=html", None).await;
    assert!(body.contains("<code>application/geo+json</code>"), "{body}");
}

/// An items page's `timeStamp` changes per request; its ETag does not.
#[tokio::test]
async fn items_html_etag_ignores_the_generation_time() {
    let (_, first, body) = get("/collections/c/items?f=html", None).await;
    assert!(body.contains("<time data-generated>20"), "{body}");
    let (_, second, _) = get("/collections/c/items?f=html", None).await;
    assert_eq!(first[header::ETAG], second[header::ETAG]);
}

/// `/locations` validates `f` (#605): GeoJSON (or `json`) and HTML only.
#[tokio::test]
async fn the_location_list_rejects_formats_it_does_not_offer() {
    for f in ["foo", "CoverageJSON", "PNG"] {
        let (status, _, body) = get(&format!("/collections/c/locations?f={f}"), None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{f}");
        assert!(body.contains("available: GeoJSON, HTML"), "{body}");
    }
    for f in ["GeoJSON", "json"] {
        let (status, headers, _) = get(&format!("/collections/c/locations?f={f}"), None).await;
        assert_eq!(status, StatusCode::OK, "{f}");
        assert_eq!(headers[header::CONTENT_TYPE], "application/geo+json");
    }
}

/// Every data query advertises HTML in `output_formats`.
#[tokio::test]
async fn output_formats_list_html() {
    let (_, _, body) = get("/collections/c", None).await;
    let doc: Value = serde_json::from_str(&body).unwrap();
    let queries = doc["data_queries"].as_object().unwrap();
    assert!(queries.contains_key("items") && queries.contains_key("cube"));
    for (name, query) in queries {
        if name == "instances" {
            continue;
        }
        let formats = &query["link"]["variables"]["output_formats"];
        assert!(
            formats.as_array().unwrap().iter().any(|f| f == "HTML"),
            "{name}: {formats}"
        );
    }
}

/// `/req/html/content` A over the EDR GeoJSON schema of the same response
/// (#988): a station collection's position, radius and location pages say
/// which location each coverage is, with every identity member its GeoJSON
/// features carry: id, label, `edrqueryendpoint` as an `<a>`, `datetime`,
/// and `numberMatched` / `numberReturned`.
#[tokio::test]
async fn station_pages_name_each_coverage_as_the_geojson_does() {
    use ds_core::html::escape;
    for uri in [
        format!("/collections/obs/position?coords={POINT}"),
        "/collections/obs/position?coords=MULTIPOINT((25%2060),(26%2061))".to_string(),
        format!("/collections/obs/radius?coords={POINT}&within=500&within-units=km"),
        "/collections/obs/locations/here".to_string(),
        "/collections/obs/locations/here,a%20b".to_string(),
    ] {
        let (status, _, geojson) = get(&with_f(&uri, "GeoJSON"), None).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {geojson}");
        let geojson: Value = serde_json::from_str(&geojson).unwrap();
        let (status, _, html) = get(&with_f(&uri, "html"), None).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        let body = html.split_once("<body").map_or(&*html, |(_, b)| b);
        assert!(body.contains("<h2>Locations</h2>"), "{uri}");
        let features = geojson["features"].as_array().unwrap();
        assert!(!features.is_empty(), "{uri}");
        for (n, key) in ["numberMatched", "numberReturned"].iter().enumerate() {
            let value = geojson[*key].as_u64().unwrap();
            assert!(
                body.contains(&format!("<dt><code>{key}</code></dt><dd>{value}</dd>")),
                "{uri}: {key} ({n})"
            );
        }
        for (i, f) in features.iter().enumerate() {
            let props = &f["properties"];
            let id = f["id"].as_str().unwrap();
            let label = props["label"].as_str().unwrap();
            let endpoint = props["edrqueryendpoint"].as_str().unwrap();
            let datetime = props["datetime"].as_str().unwrap();
            assert!(
                body.contains(&format!("<code>{}</code>", escape(id))),
                "{uri}: {id}"
            );
            assert!(body.contains(&escape(datetime)), "{uri}: {datetime}");
            assert!(
                body.contains(&format!("href=\"{}\"", escape(endpoint))),
                "{uri}: {endpoint}"
            );
            // Each coverage's heading names its station.
            let heading = if features.len() == 1 {
                format!("Coverage · {}", escape(label))
            } else {
                format!(
                    "Coverage {} of {} · {}",
                    i + 1,
                    features.len(),
                    escape(label)
                )
            };
            assert!(body.contains(&heading), "{uri}: {heading}");
        }
        // Labels are escaped, never markup.
        assert!(!body.contains("<b>Here</b>"), "{uri}");
    }
    // A gridded collection has no GeoJSON form and no location table.
    let (_, _, html) = get(
        &format!("/collections/c/area?coords={POLYGON}&f=html"),
        None,
    )
    .await;
    assert!(!html.contains("<h2>Locations</h2>"));
}

/// EDR 1.2 `/req/edr/rc-core-query-parameters` L: an HTML page cannot be
/// paged, so `limit` is ignored there and every coverage is shown.
#[tokio::test]
async fn html_ignores_limit() {
    for uri in [
        "/collections/obs/position?coords=MULTIPOINT((25%2060),(26%2061))".to_string(),
        format!("/collections/obs/radius?coords={POINT}&within=500&within-units=km"),
        "/collections/obs/locations/here,a%20b".to_string(),
    ] {
        let (_, _, all) = get(&with_f(&uri, "html"), None).await;
        let (status, _, limited) = get(&with_f(&uri, "html&limit=1"), None).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert!(all.contains("Coverage 2 of 2"), "{uri}");
        assert!(
            limited.contains("Coverage 2 of 2"),
            "{uri}: limit truncated the page"
        );
        assert!(
            limited.contains("<dt><code>numberReturned</code></dt><dd>2</dd>"),
            "{uri}"
        );
        // The JSON forms still page.
        let (_, _, json) = get(&with_f(&uri, "GeoJSON&limit=1"), None).await;
        let json: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(json["numberReturned"], 1, "{uri}");
    }
}
