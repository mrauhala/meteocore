//! CSV output for station series (#1066): `f=CSV`, `f=text/csv` and
//! `Accept: text/csv` on `locations/{locationId}` (one id and id lists),
//! `position`, `radius` and `area` of a station-series engine
//! (`EdrEngine::serves_station_series`); 400 naming the offered formats on
//! a gridded collection. The body is the long form, one row per location,
//! time and parameter, built from the same result as the CoverageJSON twin.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use api_edr::handlers::EdrState;
use ds_core::config::CollectionConfig;
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::feature::parse_point_coords;
use ds_core::model::*;

#[path = "support/edr_schema.rs"]
mod edr_schema;

const BASE: &str = "https://api.example.com";
const CONTENT_TYPE: &str = "text/csv; charset=utf-8; header=present";
const HEADER: [&str; 8] = [
    "location_id",
    "label",
    "longitude",
    "latitude",
    "datetime",
    "parameter",
    "value",
    "unit",
];
/// A label a spreadsheet would split at its comma and quote.
const PIER_LABEL: &str = r#"Pier 4, "North" end"#;
/// An id and a label a spreadsheet would run as formulas.
const FORMULA_ID: &str = "-west";
const FORMULA_LABEL: &str = r#"=HYPERLINK("http://example.org","x")"#;
/// A station whose CSV is over the 64 MiB cap while its CoverageJSON is
/// under 1 MB: its 4000-character label repeats on each of
/// [`LONG_STEPS`] rows.
const LONG_ID: &str = "long";
const LONG_LABEL_CHARS: usize = 4000;
const LONG_STEPS: usize = 20_000;

fn stations() -> Vec<Location> {
    let long_label = "L".repeat(LONG_LABEL_CHARS);
    [
        ("helsinki", "Helsinki Kaisaniemi", 24.9384, 60.1699),
        ("tampere", "Tampere Härmälä", 23.7610, 61.4978),
        ("oulu airport", "Oulu airport", 25.3546, 64.9301),
        ("pier", PIER_LABEL, 10.0, 55.0),
        (FORMULA_ID, FORMULA_LABEL, -5.25, 52.0),
        (LONG_ID, long_label.as_str(), -60.0, -30.0),
    ]
    .into_iter()
    .map(|(id, label, lon, lat)| Location {
        id: id.into(),
        label: label.into(),
        latitude: lat,
        longitude: lon,
    })
    .collect()
}

/// [`LONG_STEPS`] minutes of `temperature` at `(x, y)`.
fn long_series(x: f64, y: f64) -> QueryResult {
    let start: DateTime<Utc> = "2024-01-01T00:00:00Z".parse().unwrap();
    let t: Vec<DateTime<Utc>> = (0..LONG_STEPS)
        .map(|m| start + chrono::Duration::minutes(m as i64))
        .collect();
    let mut q = series(x, y);
    q.ranges.retain(|name, _| name == "temperature");
    q.parameters.retain(|name, _| name == "temperature");
    let range = q.ranges.get_mut("temperature").unwrap();
    range.shape = vec![LONG_STEPS];
    range.values = (0..LONG_STEPS).map(|i| Some(i as f64 / 10.0)).collect();
    q.domain = DomainDescription::PointSeries { x, y, t, z: None };
    q
}

/// A station series at `(x, y)`: three hours of `temperature` and
/// `humidity`, one humidity value missing.
fn series(x: f64, y: f64) -> QueryResult {
    let t: Vec<DateTime<Utc>> = (0..3)
        .map(|h| format!("2024-01-01T{h:02}:00:00Z").parse().unwrap())
        .collect();
    let columns: [(&str, &str, Vec<Option<f64>>); 2] = [
        (
            "temperature",
            "°C",
            vec![Some(-2.5), Some(-2.8), Some(0.1 + 0.2)],
        ),
        ("humidity", "%", vec![Some(80.0), None, Some(82.0)]),
    ];
    let mut parameters = HashMap::new();
    let mut ranges = HashMap::new();
    for (name, unit, values) in columns {
        parameters.insert(
            name.to_string(),
            ParameterDescription {
                label: name.to_string(),
                unit: unit.into(),
                observed_property: name.to_string(),
                standard_name: None,
            },
        );
        ranges.insert(
            name.to_string(),
            NdArray {
                shape: vec![values.len()],
                axis_names: vec!["t".into()],
                values,
            },
        );
    }
    QueryResult {
        domain: DomainDescription::PointSeries { x, y, t, z: None },
        parameters,
        ranges,
    }
}

/// A station engine (CSV/PostGIS/BUFR-like): series at its locations'
/// exact coordinates.
struct StationEngine;

impl EdrEngine for StationEngine {
    fn get_locations(&self) -> Result<Arc<LocationSnapshot>, DataServerError> {
        Ok(Arc::new(LocationSnapshot::new(stations())))
    }

    fn query_location(
        &self,
        location_id: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        let loc = stations()
            .into_iter()
            .find(|l| l.id == location_id)
            .ok_or_else(|| DataServerError::LocationNotFound(location_id.into()))?;
        if loc.id == LONG_ID {
            return Ok(CoverageResponse::Single(long_series(
                loc.longitude,
                loc.latitude,
            )));
        }
        Ok(CoverageResponse::Single(series(
            loc.longitude,
            loc.latitude,
        )))
    }

    fn query_position(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        // Nearest station within half a degree.
        let (lat, lon) = parse_point_coords(coords)?;
        let near = stations()
            .into_iter()
            .find(|l| (l.longitude - lon).abs() < 0.5 && (l.latitude - lat).abs() < 0.5)
            .ok_or_else(|| DataServerError::LocationNotFound("no station".into()))?;
        self.query_location(&near.id, datetime, parameters, z, reference_time)
    }

    fn query_area(
        &self,
        coords: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        let polygon = ds_core::feature::parse_area_coords(coords)?;
        Ok(CoverageResponse::Collection(
            stations()
                .into_iter()
                .filter(|l| polygon.contains(l.longitude, l.latitude))
                .map(|l| series(l.longitude, l.latitude))
                .collect(),
        ))
    }

    fn get_parameters(&self) -> Vec<String> {
        vec!["temperature".into(), "humidity".into()]
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        Some((
            "2024-01-01T00:00:00Z".parse().unwrap(),
            "2024-01-01T02:00:00Z".parse().unwrap(),
        ))
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([-5.25, 52.0, 25.3546, 64.9301])
    }

    fn supported_query_types(&self) -> Vec<String> {
        ["locations", "position", "area", "radius"]
            .map(String::from)
            .to_vec()
    }

    fn serves_station_series(&self) -> bool {
        true
    }
}

/// A gridded engine: the trait's default `serves_station_series` (false).
struct GriddedEngine;

impl EdrEngine for GriddedEngine {
    fn get_locations(&self) -> Result<Arc<LocationSnapshot>, DataServerError> {
        Ok(LocationSnapshot::empty())
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

    fn query_position(
        &self,
        coords: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        let (lat, lon) = parse_point_coords(coords)?;
        Ok(CoverageResponse::Single(series(lon, lat)))
    }

    fn query_area(
        &self,
        _coords: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Ok(CoverageResponse::Single(QueryResult {
            domain: DomainDescription::Grid {
                x: vec![24.0, 25.0],
                y: vec![60.0],
                t: None,
                z: None,
            },
            parameters: series(0.0, 0.0).parameters,
            ranges: HashMap::new(),
        }))
    }

    fn get_parameters(&self) -> Vec<String> {
        vec!["temperature".into(), "humidity".into()]
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        None
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([19.0, 59.0, 32.0, 71.0])
    }

    fn supported_query_types(&self) -> Vec<String> {
        ["position", "area", "radius"].map(String::from).to_vec()
    }
}

fn config(id: &str, title: &str) -> CollectionConfig {
    CollectionConfig {
        id: id.into(),
        title: title.into(),
        description: String::new(),
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
        station_grid: None,
        bufr: None,
        satellite: None,
        netcdf: None,
        preview: None,
        derive_wind: None,
    }
}

fn app() -> axum::Router {
    let entries: [(&str, &str, Arc<dyn EdrEngine>); 2] = [
        ("obs", "Station observations", Arc::new(StationEngine)),
        ("grid", "Gridded model", Arc::new(GriddedEngine)),
    ];
    let mut engines = HashMap::new();
    let mut collections = HashMap::new();
    for (id, title, engine) in entries {
        engines.insert(id.to_string(), engine);
        collections.insert(id.to_string(), config(id, title));
    }
    api_edr::router(Arc::new(ArcSwap::from_pointee(EdrState {
        engines,
        collections,
        styles: HashMap::new(),
        feature_engines: HashMap::new(),
        base_url: BASE.into(),
        trust_proxy_headers: false,
    })))
}

async fn request(uri: &str, accept: Option<&str>) -> (StatusCode, HeaderMap, String) {
    let mut req = Request::builder().uri(uri);
    if let Some(accept) = accept {
        req = req.header(header::ACCEPT, accept);
    }
    let resp = app()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8(body.to_vec()).unwrap())
}

async fn get_json(uri: &str) -> (StatusCode, Value) {
    let (status, _, body) = request(uri, None).await;
    let json =
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("{uri}: not JSON ({e}): {body}"));
    (status, json)
}

fn content_type(headers: &HeaderMap) -> &str {
    headers[header::CONTENT_TYPE].to_str().unwrap()
}

fn varies_on_accept(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::VARY)
        .iter()
        .any(|v| v.to_str().unwrap().eq_ignore_ascii_case("accept"))
}

/// An RFC 4180 reader: records end in CRLF, a quoted field may hold commas,
/// CR, LF and doubled quotes. Panics on anything else, so a malformed body
/// fails the test.
fn parse_csv(body: &str) -> Vec<Vec<String>> {
    let mut records = Vec::new();
    let mut record = Vec::new();
    let mut field = String::new();
    let mut chars = body.chars().peekable();
    let mut quoted = false;
    let mut at_field_start = true;
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                }
                '"' => {
                    quoted = false;
                    assert!(
                        matches!(chars.peek(), Some(',' | '\r')),
                        "text after a closing quote: {body}"
                    );
                }
                c => field.push(c),
            }
            continue;
        }
        match c {
            '"' => {
                assert!(at_field_start, "a quote inside an unquoted field: {body}");
                quoted = true;
                at_field_start = false;
            }
            ',' => {
                record.push(std::mem::take(&mut field));
                at_field_start = true;
            }
            '\r' => {
                assert_eq!(chars.next(), Some('\n'), "a bare CR: {body}");
                record.push(std::mem::take(&mut field));
                records.push(std::mem::take(&mut record));
                at_field_start = true;
            }
            '\n' => panic!("a bare LF ends a record: {body}"),
            c => {
                field.push(c);
                at_field_start = false;
            }
        }
    }
    assert!(!quoted, "an unterminated quote: {body}");
    assert!(
        field.is_empty() && record.is_empty(),
        "the last record does not end in CRLF: {body}"
    );
    records
}

/// The CSV's rows as maps of the header's columns, the header checked.
fn rows(body: &str) -> Vec<HashMap<&'static str, String>> {
    let mut records = parse_csv(body).into_iter();
    assert_eq!(records.next().unwrap(), HEADER);
    records
        .map(|record| {
            assert_eq!(record.len(), HEADER.len(), "{record:?}");
            HEADER.into_iter().zip(record).collect()
        })
        .collect()
}

fn column<'a>(rows: &'a [HashMap<&'static str, String>], name: &str) -> Vec<&'a str> {
    rows.iter().map(|r| r[name].as_str()).collect()
}

/// The CSV of a location series and its CoverageJSON carry the same
/// location, times and values: one row per step and parameter, a null an
/// empty field, each number the JSON's own text.
#[tokio::test]
async fn location_csv_carries_the_coveragejson_values() {
    let window = "datetime=2024-01-01T00:00:00Z/2024-01-01T02:00:00Z";
    let uri = format!("/collections/obs/locations/helsinki?{window}");
    let (status, headers, body) = request(&format!("{uri}&f=CSV"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(content_type(&headers), CONTENT_TYPE);
    assert!(!varies_on_accept(&headers), "an explicit f does not vary");
    // The caching of every data query: a strong ETag, and a settled window
    // keeps its long policy.
    assert!(headers.contains_key(header::ETAG));
    assert_eq!(headers[header::CACHE_CONTROL], "public, max-age=86400");
    let csv = rows(&body);

    let (status, cov) = get_json(&format!("{uri}&f=CoverageJSON")).await;
    assert_eq!(status, StatusCode::OK, "{cov}");
    let times = cov["domain"]["axes"]["t"]["values"].as_array().unwrap();
    let ranges = cov["ranges"].as_object().unwrap();
    assert_eq!(csv.len(), times.len() * ranges.len());
    let mut matched = 0;
    for row in &csv {
        assert_eq!(row["location_id"], "helsinki");
        assert_eq!(row["label"], "Helsinki Kaisaniemi");
        assert_eq!(
            row["longitude"],
            cov["domain"]["axes"]["x"]["values"][0].to_string()
        );
        assert_eq!(
            row["latitude"],
            cov["domain"]["axes"]["y"]["values"][0].to_string()
        );
        assert!(row["datetime"].ends_with('Z'), "{}", row["datetime"]);
        let at: DateTime<Utc> = row["datetime"].parse().unwrap();
        let step = times
            .iter()
            .position(|t| t.as_str().unwrap().parse::<DateTime<Utc>>().unwrap() == at)
            .unwrap_or_else(|| panic!("{at} is not on the CoverageJSON t axis"));
        let parameter = row["parameter"].as_str();
        let value = &ranges[parameter]["values"][step];
        match value {
            Value::Null => assert_eq!(row["value"], "", "{parameter} at {at}"),
            number => {
                assert_eq!(row["value"], number.to_string(), "{parameter} at {at}");
                assert_eq!(
                    row["value"].parse::<f64>().unwrap(),
                    number.as_f64().unwrap()
                );
            }
        }
        assert_eq!(
            row["unit"],
            cov["parameters"][parameter]["unit"]["label"]["en"]
                .as_str()
                .unwrap()
        );
        matched += 1;
    }
    assert_eq!(matched, 6);
    // Rows run by time, then parameter by name; the missing humidity is an
    // empty field, not 0, and 0.1 + 0.2 keeps every digit.
    assert_eq!(
        column(&csv, "parameter"),
        ["humidity", "temperature"].repeat(3)
    );
    assert_eq!(
        column(&csv, "value"),
        ["80.0", "-2.5", "", "-2.8", "82.0", "0.30000000000000004"]
    );
    assert_eq!(column(&csv, "datetime")[0], "2024-01-01T00:00:00Z");

    // Byte-identical across requests, so the ETag revalidates.
    let (_, again_headers, again) = request(&format!("{uri}&f=CSV"), None).await;
    assert_eq!(again, body);
    assert_eq!(again_headers[header::ETAG], headers[header::ETAG]);
}

/// A list of ids (#923) is one table, every id's rows in request order, each
/// row naming its own location; `limit` keeps the first ids' rows.
#[tokio::test]
async fn a_location_list_keeps_request_order() {
    let (status, _, body) = request(
        "/collections/obs/locations/tampere,oulu%20airport,helsinki?f=CSV",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let csv = rows(&body);
    let mut order: Vec<&str> = column(&csv, "location_id");
    assert_eq!(order.len(), 18);
    order.dedup();
    assert_eq!(order, ["tampere", "oulu airport", "helsinki"]);
    let tampere = &csv[0];
    assert_eq!(tampere["label"], "Tampere Härmälä");
    assert_eq!(
        (tampere["longitude"].as_str(), tampere["latitude"].as_str()),
        ("23.761", "61.4978")
    );

    let (status, _, body) = request(
        "/collections/obs/locations/helsinki,tampere?f=CSV&limit=1",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let csv = rows(&body);
    let mut order = column(&csv, "location_id");
    order.dedup();
    assert_eq!(order, ["helsinki"]);
}

/// RFC 4180 quoting, and the OWASP formula guard on text cells.
#[tokio::test]
async fn labels_are_quoted_and_formula_cells_prefixed() {
    let (status, _, body) = request("/collections/obs/locations/pier?f=CSV", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The comma and the quotes stay inside one quoted field.
    assert!(
        body.contains("\r\npier,\"Pier 4, \"\"North\"\" end\",10.0,55.0,"),
        "{body}"
    );
    let csv = rows(&body);
    assert_eq!(csv[0]["label"], PIER_LABEL);

    let (status, _, body) = request("/collections/obs/locations/-west?f=CSV", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains(
            "\r\n\"'-west\",\"'=HYPERLINK(\"\"http://example.org\"\",\"\"x\"\")\",-5.25,52.0,"
        ),
        "{body}"
    );
    let csv = rows(&body);
    for row in &csv {
        assert_eq!(row["location_id"], format!("'{FORMULA_ID}"));
        assert_eq!(row["label"], format!("'{FORMULA_LABEL}"));
        // A negative coordinate is a number, never prefixed.
        assert_eq!(row["longitude"], "-5.25");
    }
}

/// The CSV byte cap (64 MiB): a result inside the value budgets whose rows
/// would pass it is a 400 naming the ways to narrow it, while its
/// CoverageJSON is served.
#[tokio::test]
async fn csv_over_the_byte_cap_is_a_400() {
    let uri = format!("/collections/obs/locations/{LONG_ID}");
    let (status, json) = get_json(&format!("{uri}?f=CSV")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(json["code"], "BadRequest");
    let description = json["description"].as_str().unwrap();
    for part in [
        "The CSV response would exceed 67108864 bytes (64 MiB)",
        "datetime",
        "parameter-name",
        "limit",
        "f=CoverageJSON",
    ] {
        assert!(description.contains(part), "{part}: {description}");
    }
    let (status, headers, body) = request(&format!("{uri}?f=CoverageJSON"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), "application/vnd.cov+json");
    assert!(body.len() < 1 << 20, "{}", body.len());
}

/// Position, radius and area answer CSV on a station collection, by `f`
/// (the token or the media type) or by `Accept`.
#[tokio::test]
async fn position_radius_and_area_answer_csv() {
    let position = "/collections/obs/position?coords=POINT(24.94%2060.17)";
    let radius = "/collections/obs/radius?coords=POINT(24.94%2060.17)&within=200&within-units=km";
    let area = "/collections/obs/area?coords=POLYGON((20%2059,30%2059,30%2066,20%2066,20%2059))";
    for (uri, ids) in [
        (position, vec!["helsinki"]),
        (radius, vec!["helsinki", "tampere"]),
        (area, vec!["helsinki", "tampere", "oulu airport"]),
    ] {
        for f in ["CSV", "csv", "text/csv", "text/csv%3Bheader%3Dpresent"] {
            let (status, headers, body) = request(&format!("{uri}&f={f}"), None).await;
            assert_eq!(status, StatusCode::OK, "{uri} f={f}: {body}");
            assert_eq!(content_type(&headers), CONTENT_TYPE, "{uri} f={f}");
            let csv = rows(&body);
            let mut order = column(&csv, "location_id");
            order.dedup();
            assert_eq!(order, ids, "{uri} f={f}");
        }
        // Accept chooses it among the offered formats, and the response
        // varies on Accept; a tie with CoverageJSON keeps CoverageJSON.
        let (status, headers, _) = request(uri, Some("text/csv")).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(content_type(&headers), CONTENT_TYPE, "{uri}");
        assert!(varies_on_accept(&headers), "{uri}");
        let (_, headers, _) = request(uri, Some("text/csv, application/vnd.cov+json")).await;
        assert_eq!(content_type(&headers), "application/vnd.cov+json", "{uri}");
        let (_, headers, _) = request(uri, Some("text/html, text/csv")).await;
        assert_eq!(content_type(&headers), CONTENT_TYPE, "{uri}");
    }
}

/// A gridded collection has no CSV: `f=CSV` is a 400 naming the formats the
/// query offers, and `Accept: text/csv` gets the default.
#[tokio::test]
async fn gridded_collections_answer_400_for_csv() {
    for (uri, what) in [
        (
            "/collections/grid/position?coords=POINT(24.94%2060.17)",
            "position queries; available: CoverageJSON, PNG, HTML",
        ),
        (
            "/collections/grid/radius?coords=POINT(24.94%2060.17)&within=10&within-units=km",
            "radius queries; available: CoverageJSON, HTML",
        ),
        (
            "/collections/grid/area?coords=POLYGON((20%2059,30%2059,30%2066,20%2066,20%2059))",
            "area queries; available: CoverageJSON, HTML",
        ),
    ] {
        for f in ["CSV", "text/csv"] {
            let (status, json) = get_json(&format!("{uri}&f={f}")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri} f={f}");
            let description = json["description"].as_str().unwrap();
            assert!(
                description.contains(&format!("CSV output is not available for {what}")),
                "{description}"
            );
        }
        let (status, headers, _) = request(uri, Some("text/csv")).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(content_type(&headers), "application/vnd.cov+json", "{uri}");
    }
}

/// The other representations of a station query link their CSV twin.
#[tokio::test]
async fn geojson_links_its_csv_alternate() {
    let (status, json) =
        get_json("/collections/obs/position?coords=POINT(24.94%2060.17)&f=GeoJSON").await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let csv = json["links"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["rel"] == "alternate" && l["type"] == "text/csv;header=present")
        .unwrap_or_else(|| panic!("no CSV alternate: {}", json["links"]));
    assert_eq!(
        csv["href"],
        format!("{BASE}/edr/collections/obs/position?coords=POINT(24.94%2060.17)&f=CSV")
    );
}

/// `data_queries` and `/api` advertise CSV exactly where it is served, the
/// collection documents still validate against the EDR 1.1 and 1.2 bundles,
/// and `/api` is still valid OpenAPI 3.0.
#[tokio::test]
async fn metadata_and_api_advertise_csv_where_served() {
    let (_, obs) = get_json("/collections/obs").await;
    let (_, grid) = get_json("/collections/grid").await;
    for doc in [&obs, &grid] {
        edr_schema::assert_valid(
            "/collections/{collectionId}",
            edr_schema::JSON,
            doc,
            "collection",
        );
    }
    let formats = |doc: &Value, qt: &str| {
        doc["data_queries"][qt]["link"]["variables"]["output_formats"].clone()
    };
    for qt in ["locations", "position"] {
        assert_eq!(
            formats(&obs, qt),
            json!(["CoverageJSON", "GeoJSON", "PNG", "CSV", "HTML"]),
            "{qt}"
        );
    }
    assert_eq!(
        formats(&obs, "radius"),
        json!(["CoverageJSON", "GeoJSON", "CSV", "HTML"])
    );
    assert_eq!(
        formats(&obs, "area"),
        json!(["CoverageJSON", "CSV", "HTML"])
    );
    assert_eq!(
        obs["output_formats"],
        json!(["CoverageJSON", "GeoJSON", "PNG", "CSV", "HTML"])
    );
    assert_eq!(
        formats(&grid, "position"),
        json!(["CoverageJSON", "PNG", "HTML"])
    );
    for qt in ["area", "radius"] {
        assert_eq!(formats(&grid, qt), json!(["CoverageJSON", "HTML"]), "{qt}");
    }
    assert_eq!(
        grid["output_formats"],
        json!(["CoverageJSON", "PNG", "HTML"])
    );

    let (status, api) = get_json("/api").await;
    assert_eq!(status, StatusCode::OK);
    let meta: Value =
        serde_json::from_str(include_str!("../../../schemas/openapi-3.0.json")).unwrap();
    let validator = jsonschema::Validator::new(&meta).unwrap();
    let errors: Vec<String> = validator
        .iter_errors(&api)
        .map(|e| format!("{e} at {}", e.instance_path()))
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
    let op = |path: &str| api["paths"][path]["get"].clone();
    let f_enum = |op: &Value| {
        op["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "f")
            .map(|p| p["schema"]["enum"].clone())
    };
    let csv_content =
        |op: &Value| op["responses"]["200"]["content"]["text/csv;header=present"].clone();
    for (path, expected) in [
        (
            "/edr/collections/obs/position",
            json!(["CoverageJSON", "GeoJSON", "PNG", "CSV", "HTML"]),
        ),
        (
            "/edr/collections/obs/locations/{locationId}",
            json!(["CoverageJSON", "GeoJSON", "PNG", "CSV", "HTML"]),
        ),
        (
            "/edr/collections/obs/radius",
            json!(["CoverageJSON", "GeoJSON", "CSV", "HTML"]),
        ),
        (
            "/edr/collections/obs/area",
            json!(["CoverageJSON", "CSV", "HTML"]),
        ),
    ] {
        assert_eq!(f_enum(&op(path)), Some(expected), "{path}");
        assert_eq!(
            csv_content(&op(path)),
            json!({"schema": {"type": "string"}}),
            "{path}"
        );
    }
    for path in [
        "/edr/collections/grid/position",
        "/edr/collections/grid/area",
        "/edr/collections/grid/radius",
    ] {
        assert!(csv_content(&op(path)).is_null(), "{path}");
        let names = f_enum(&op(path)).unwrap();
        assert!(!names.as_array().unwrap().contains(&json!("CSV")), "{path}");
    }
}
