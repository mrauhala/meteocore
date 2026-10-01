//! EDR GeoJSON output for data queries (#929): `f=GeoJSON` and
//! `Accept: application/geo+json` on the point queries — locations,
//! position, radius — of a station-series engine
//! (`EdrEngine::serves_station_series`); 400 for `f=GeoJSON` everywhere
//! else. Each GeoJSON body is validated against the EDR 1.1 and 1.2
//! bundles' `application/geo+json` schema of its route.

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

fn stations() -> Vec<Location> {
    [
        ("helsinki", "Helsinki Kaisaniemi", 24.9384, 60.1699),
        ("tampere", "Tampere Härmälä", 23.7610, 61.4978),
        ("oulu airport", "Oulu airport", 25.3546, 64.9301),
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

/// A station series at `(x, y)`: three hours of `temperature` and
/// `humidity` (one missing), or of `extra` alone when given.
fn series(x: f64, y: f64, extra: Option<&str>) -> QueryResult {
    let t: Vec<DateTime<Utc>> = (0..3)
        .map(|h| format!("2024-01-01T{h:02}:00:00Z").parse().unwrap())
        .collect();
    let columns: Vec<(&str, &str, Vec<Option<f64>>)> = match extra {
        Some(name) => vec![(name, "1", vec![Some(1.0), Some(2.0), Some(3.0)])],
        None => vec![
            (
                "temperature",
                "°C",
                vec![Some(-2.5), Some(-2.8), Some(-3.1)],
            ),
            ("humidity", "%", vec![Some(80.0), None, Some(82.0)]),
        ],
    };
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

/// `series` cut to the steps inside `window`, like a station engine's
/// answer; `None` (a 404 from the engine) when no step is inside it.
fn windowed(
    x: f64,
    y: f64,
    extra: Option<&str>,
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> Option<QueryResult> {
    let mut q = series(x, y, extra);
    let Some((start, end)) = window else {
        return Some(q);
    };
    let DomainDescription::PointSeries { t, .. } = &mut q.domain else {
        unreachable!("series is a PointSeries")
    };
    let keep: Vec<bool> = t.iter().map(|t| *t >= start && *t <= end).collect();
    let mut kept = keep.iter();
    t.retain(|_| *kept.next().unwrap());
    if t.is_empty() {
        return None;
    }
    for range in q.ranges.values_mut() {
        let mut kept = keep.iter();
        range.values.retain(|_| *kept.next().unwrap());
        range.shape = vec![range.values.len()];
    }
    Some(q)
}

/// A station engine (CSV/PostGIS/BUFR-like): series at its locations'
/// exact coordinates, cut to the requested window. `extra` replaces the
/// parameters with one of that name.
struct StationEngine {
    extra: Option<&'static str>,
}

impl EdrEngine for StationEngine {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        Ok(stations())
    }

    fn query_location(
        &self,
        location_id: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        let loc = stations()
            .into_iter()
            .find(|l| l.id == location_id)
            .ok_or_else(|| DataServerError::LocationNotFound(location_id.into()))?;
        windowed(loc.longitude, loc.latitude, self.extra, datetime)
            .map(CoverageResponse::Single)
            .ok_or_else(|| DataServerError::LocationNotFound(format!("{location_id} (no data)")))
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
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        let polygon = ds_core::feature::parse_area_coords(coords)?;
        Ok(CoverageResponse::Collection(
            stations()
                .into_iter()
                .filter(|l| polygon.contains(l.longitude, l.latitude))
                .filter_map(|l| windowed(l.longitude, l.latitude, self.extra, datetime))
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
        Some([23.7610, 60.1699, 25.3546, 64.9301])
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

/// A gridded engine: the same answers, but it serves coverages, not
/// station series (the trait default).
struct GriddedEngine;

impl EdrEngine for GriddedEngine {
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

    fn query_position(
        &self,
        coords: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        let (lat, lon) = parse_point_coords(coords)?;
        Ok(CoverageResponse::Single(series(lon, lat, None)))
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
            parameters: series(0.0, 0.0, None).parameters,
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
        bufr: None,
        satellite: None,
        preview: None,
    }
}

fn app() -> axum::Router {
    let entries: [(&str, &str, Arc<dyn EdrEngine>); 3] = [
        (
            "obs",
            "Station observations",
            Arc::new(StationEngine { extra: None }),
        ),
        (
            "odd",
            "Stations with a reserved parameter name",
            Arc::new(StationEngine {
                extra: Some("time"),
            }),
        ),
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

async fn request(uri: &str, accept: Option<&str>) -> (StatusCode, HeaderMap, Vec<u8>) {
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
    let body = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, headers, body)
}

async fn get_json(uri: &str, accept: Option<&str>) -> (StatusCode, HeaderMap, Value) {
    let (status, headers, body) = request(uri, accept).await;
    let json = serde_json::from_slice(&body)
        .unwrap_or_else(|e| panic!("{uri}: not JSON ({e}): {}", String::from_utf8_lossy(&body)));
    (status, headers, json)
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

/// Validate a GeoJSON body against the `application/geo+json` 200 schema
/// of `path` in both the EDR 1.1 and 1.2 bundles (`edr_schema`). The
/// location data path names its parameter `{locationId}` in 1.2 and
/// `{locId}` in 1.1, so that one path is looked up per version.
fn assert_valid_edr_geojson(path: &str, json: &Value) {
    if !path.ends_with("/{locationId}") {
        return edr_schema::assert_valid(path, edr_schema::GEOJSON, json, "GeoJSON");
    }
    for version in edr_schema::VERSIONS {
        let path = match version {
            edr_schema::Edr::V1_1 => path.replace("{locationId}", "{locId}"),
            edr_schema::Edr::V1_2 => path.to_string(),
        };
        let errors = edr_schema::errors(version, &path, edr_schema::GEOJSON, json);
        assert!(
            errors.is_empty(),
            "EDR {version:?} {path}:\n{}\n\nResponse:\n{}",
            errors.join("\n"),
            serde_json::to_string_pretty(json).unwrap()
        );
    }
}

fn link<'a>(json: &'a Value, rel: &str, kind: &str) -> &'a str {
    json["links"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["rel"] == rel && l["type"] == kind)
        .unwrap_or_else(|| panic!("no {rel} link of type {kind}: {}", json["links"]))["href"]
        .as_str()
        .unwrap()
}

#[tokio::test]
async fn position_geojson_is_one_named_feature_per_station() {
    let uri = "/collections/obs/position?coords=POINT(24.94%2060.17)&f=GeoJSON";
    let (status, headers, json) = get_json(uri, None).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(content_type(&headers), "application/geo+json");
    assert!(!varies_on_accept(&headers), "an explicit f does not vary");
    assert_valid_edr_geojson("/collections/{collectionId}/position", &json);

    assert_eq!(json["type"], "FeatureCollection");
    assert_eq!(json["numberReturned"], 1);
    assert_eq!(json["numberMatched"], 1);
    let features = json["features"].as_array().unwrap();
    assert_eq!(features.len(), 1);
    let f = &features[0];
    assert_eq!(f["type"], "Feature");
    // Named by the location at the series' exact coordinates.
    assert_eq!(f["id"], "helsinki");
    assert_eq!(
        f["geometry"],
        json!({"type": "Point", "coordinates": [24.9384, 60.1699]})
    );
    let p = &f["properties"];
    assert_eq!(p["label"], "Helsinki Kaisaniemi");
    assert_eq!(
        p["edrqueryendpoint"],
        format!("{BASE}/edr/collections/obs/locations/helsinki")
    );
    assert_eq!(
        p["datetime"],
        "2024-01-01T00:00:00+00:00/2024-01-01T02:00:00+00:00"
    );
    assert_eq!(p["parameter-name"], json!(["humidity", "temperature"]));
    assert_eq!(
        p["time"],
        json!([
            "2024-01-01T00:00:00+00:00",
            "2024-01-01T01:00:00+00:00",
            "2024-01-01T02:00:00+00:00"
        ])
    );
    assert_eq!(p["temperature"], json!([-2.5, -2.8, -3.1]));
    assert_eq!(p["humidity"], json!([80.0, null, 82.0]));

    // Parameter metadata: EDR parameter objects with their id.
    let params = json["parameters"].as_array().unwrap();
    let ids: Vec<&str> = params.iter().map(|p| p["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["humidity", "temperature"]);
    for param in params {
        assert_eq!(param["type"], "Parameter");
        assert!(param["observedProperty"]["label"]["en"].is_string());
    }
    assert_eq!(params[1]["unit"]["label"]["en"], "°C");

    // Links: self, an alternate per other format, the collection.
    let query = "coords=POINT(24.94%2060.17)";
    assert_eq!(
        link(&json, "self", "application/geo+json"),
        format!("{BASE}/edr/collections/obs/position?{query}&f=GeoJSON")
    );
    assert_eq!(
        link(&json, "alternate", "application/vnd.cov+json"),
        format!("{BASE}/edr/collections/obs/position?{query}&f=CoverageJSON")
    );
    assert_eq!(
        link(&json, "alternate", "image/png"),
        format!("{BASE}/edr/collections/obs/position?{query}&f=PNG")
    );
    assert_eq!(
        link(&json, "collection", "application/json"),
        format!("{BASE}/edr/collections/obs")
    );
}

#[tokio::test]
async fn multipoint_position_is_a_feature_per_point() {
    let uri =
        "/collections/obs/position?coords=MULTIPOINT((24.94%2060.17),(23.76%2061.5))&f=geojson";
    let (status, _, json) = get_json(uri, None).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_valid_edr_geojson("/collections/{collectionId}/position", &json);
    let ids: Vec<&str> = json["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["helsinki", "tampere"]);
    assert_eq!(json["numberReturned"], 2);
}

#[tokio::test]
async fn location_geojson_names_the_requested_location() {
    let uri = "/collections/obs/locations/oulu%20airport?f=GeoJSON&datetime=2024-01-01T00:00:00Z/2024-01-01T02:00:00Z";
    let (status, headers, json) = get_json(uri, None).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(content_type(&headers), "application/geo+json");
    assert_valid_edr_geojson("/collections/{collectionId}/locations/{locationId}", &json);
    let f = &json["features"][0];
    assert_eq!(f["id"], "oulu airport");
    assert_eq!(f["properties"]["label"], "Oulu airport");
    // The id is percent-encoded into the endpoint and the links.
    assert_eq!(
        f["properties"]["edrqueryendpoint"],
        format!("{BASE}/edr/collections/obs/locations/oulu%20airport")
    );
    assert_eq!(
        link(&json, "self", "application/geo+json"),
        format!(
            "{BASE}/edr/collections/obs/locations/oulu%20airport\
             ?datetime=2024-01-01T00:00:00Z/2024-01-01T02:00:00Z&f=GeoJSON"
        )
    );
    // A settled window keeps its long cache policy in GeoJSON too.
    assert_eq!(headers[header::CACHE_CONTROL], "public, max-age=86400");
}

#[tokio::test]
async fn radius_geojson_lists_the_stations_in_the_circle() {
    let uri =
        "/collections/obs/radius?coords=POINT(24.94%2060.17)&within=200&within-units=km&f=GeoJSON";
    let (status, headers, json) = get_json(uri, None).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(content_type(&headers), "application/geo+json");
    assert_valid_edr_geojson("/collections/{collectionId}/radius", &json);
    let ids: Vec<&str> = json["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["helsinki", "tampere"]);
    // Radius offers no PNG, so the only alternate is CoverageJSON.
    let alternates: Vec<&str> = json["links"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|l| l["rel"] == "alternate")
        .map(|l| l["type"].as_str().unwrap())
        .collect();
    assert_eq!(alternates, ["application/vnd.cov+json"]);

    // An empty circle is an empty FeatureCollection, like CoverageJSON's
    // empty CoverageCollection.
    let (status, _, json) = get_json(
        "/collections/obs/radius?coords=POINT(10%2050)&within=10&within-units=km&f=GeoJSON",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["features"], json!([]));
    assert_valid_edr_geojson("/collections/{collectionId}/radius", &json);
}

/// `limit` (#922) caps GeoJSON features as it caps CoverageJSON coverages,
/// one per station; `numberMatched` counts them before the cap and is left
/// out when points past the cap were never queried.
#[tokio::test]
async fn limit_caps_features_and_counts_the_matches() {
    let radius =
        "/collections/obs/radius?coords=POINT(24.94%2060.17)&within=200&within-units=km&f=GeoJSON";
    let (status, _, json) = get_json(&format!("{radius}&limit=1"), None).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_valid_edr_geojson("/collections/{collectionId}/radius", &json);
    let ids: Vec<&str> = json["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["helsinki"]);
    assert_eq!(json["numberReturned"], 1);
    assert_eq!(json["numberMatched"], 2);
    // The links repeat the request, limit included.
    assert!(link(&json, "self", "application/geo+json").contains("&limit=1&f=GeoJSON"));

    // A limit above the match count changes nothing.
    let (_, _, json) = get_json(&format!("{radius}&limit=5"), None).await;
    assert_eq!(json["numberReturned"], 2);
    assert_eq!(json["numberMatched"], 2);

    // MULTIPOINT: the points past the limit are never queried, so the
    // matches are unknown and numberMatched is left out.
    let multipoint =
        "/collections/obs/position?coords=MULTIPOINT((24.94%2060.17),(23.76%2061.5))&f=GeoJSON";
    let (status, _, json) = get_json(&format!("{multipoint}&limit=1"), None).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_valid_edr_geojson("/collections/{collectionId}/position", &json);
    assert_eq!(json["features"].as_array().unwrap().len(), 1);
    assert_eq!(json["features"][0]["id"], "helsinki");
    assert_eq!(json["numberReturned"], 1);
    assert!(json.get("numberMatched").is_none(), "{json}");
    let (_, _, json) = get_json(&format!("{multipoint}&limit=2"), None).await;
    assert_eq!(json["numberReturned"], 2);
    assert_eq!(json["numberMatched"], 2);

    // One location is one feature, whatever the limit.
    let (status, _, json) =
        get_json("/collections/obs/locations/tampere?f=GeoJSON&limit=1", None).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["numberReturned"], 1);
    assert_eq!(json["numberMatched"], 1);

    // An invalid limit is the same 400 as for CoverageJSON.
    let (status, _, _) = request(&format!("{radius}&limit=0"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// A `datetime` list (#936) queries each instant and merges each station's
/// series along `t`: still one feature per station, its `time` the listed
/// instants, and `numberMatched` the merged features before `limit`.
#[tokio::test]
async fn datetime_lists_merge_into_one_feature_per_station() {
    let list = "datetime=2024-01-01T00:00:00Z,2024-01-01T02:00:00Z";
    let radius = format!(
        "/collections/obs/radius?coords=POINT(24.94%2060.17)&within=200&within-units=km&f=GeoJSON&{list}"
    );
    let (status, headers, json) = get_json(&radius, None).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(content_type(&headers), "application/geo+json");
    assert_valid_edr_geojson("/collections/{collectionId}/radius", &json);
    let ids: Vec<&str> = json["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["helsinki", "tampere"]);
    let p = &json["features"][0]["properties"];
    assert_eq!(
        p["time"],
        json!(["2024-01-01T00:00:00+00:00", "2024-01-01T02:00:00+00:00"])
    );
    assert_eq!(
        p["datetime"],
        "2024-01-01T00:00:00+00:00/2024-01-01T02:00:00+00:00"
    );
    assert_eq!(p["temperature"], json!([-2.5, -3.1]));
    assert_eq!(p["humidity"], json!([80.0, 82.0]));
    assert_eq!(json["numberMatched"], 2);

    // `limit` caps the merged features, not the per-instant answers.
    let (_, _, json) = get_json(&format!("{radius}&limit=1"), None).await;
    assert_eq!(json["numberReturned"], 1);
    assert_eq!(json["numberMatched"], 2);

    // Position: two points over two instants are still two features.
    let (status, _, json) = get_json(
        &format!(
            "/collections/obs/position?coords=MULTIPOINT((24.94%2060.17),(23.76%2061.5))&f=GeoJSON&{list}&limit=5"
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_valid_edr_geojson("/collections/{collectionId}/position", &json);
    assert_eq!(json["numberReturned"], 2);
    assert_eq!(json["numberMatched"], 2);
    assert_eq!(
        json["features"][1]["properties"]["time"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // A location over a list it has one instant of: the other is skipped.
    let (status, _, json) = get_json(
        "/collections/obs/locations/tampere?f=GeoJSON&datetime=2024-01-01T01:00:00Z,2024-01-01T05:00:00Z",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(
        json["features"][0]["properties"]["time"],
        json!(["2024-01-01T01:00:00+00:00"])
    );
    assert_eq!(json["features"][0]["properties"]["humidity"], json!([null]));
}

/// A list of location ids (EDR 1.2 multiple_locations, #923) is one
/// FeatureCollection on a station collection: every id's feature in request
/// order, each named by its own id; an unknown id is still the 404.
#[tokio::test]
async fn a_location_list_is_one_feature_collection_in_request_order() {
    let uri = "/collections/obs/locations/tampere,oulu%20airport?f=GeoJSON";
    let (status, headers, json) = get_json(uri, None).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(content_type(&headers), "application/geo+json");
    assert_valid_edr_geojson("/collections/{collectionId}/locations/{locationId}", &json);
    let features = json["features"].as_array().unwrap();
    let ids: Vec<&str> = features.iter().map(|f| f["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["tampere", "oulu airport"]);
    assert_eq!(features[0]["properties"]["label"], "Tampere Härmälä");
    assert_eq!(
        features[1]["properties"]["edrqueryendpoint"],
        format!("{BASE}/edr/collections/obs/locations/oulu%20airport")
    );
    assert_eq!(
        features[1]["geometry"]["coordinates"],
        json!([25.3546, 64.9301])
    );
    assert_eq!(json["numberMatched"], 2);
    assert_eq!(json["numberReturned"], 2);
    // The links repeat the list as it was sent; a list has no PNG twin.
    assert_eq!(
        link(&json, "self", "application/geo+json"),
        format!("{BASE}/edr/collections/obs/locations/tampere,oulu%20airport?f=GeoJSON")
    );
    let alternates: Vec<&str> = json["links"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|l| l["rel"] == "alternate")
        .map(|l| l["type"].as_str().unwrap())
        .collect();
    assert_eq!(alternates, ["application/vnd.cov+json"]);

    // Accept negotiates a list too, and never picks PNG for it.
    let (status, headers, _) = request(
        "/collections/obs/locations/helsinki,tampere",
        Some("application/geo+json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), "application/geo+json");
    assert!(varies_on_accept(&headers));
    let (status, headers, _) = request(
        "/collections/obs/locations/helsinki,tampere",
        Some("image/png"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), "application/vnd.cov+json");
    let (status, _, json) =
        get_json("/collections/obs/locations/helsinki,tampere?f=PNG", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        json["description"].as_str().unwrap().contains("PNG"),
        "{json}"
    );

    // `limit` keeps the first ids' features; the rest are never queried,
    // so their matches are uncounted.
    let (status, _, json) = get_json(
        "/collections/obs/locations/tampere,helsinki?f=GeoJSON&limit=1",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["features"][0]["id"], "tampere");
    assert_eq!(json["numberReturned"], 1);
    assert!(json.get("numberMatched").is_none(), "{json}");

    // One known and one unknown id: the 404 naming the unknown one.
    let (status, _, json) = get_json(
        "/collections/obs/locations/helsinki,nowhere?f=GeoJSON",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{json}");
    assert!(
        json["description"].as_str().unwrap().contains("nowhere"),
        "{json}"
    );

    // A gridded collection offers no GeoJSON for a list either.
    let (status, _, _) = request("/collections/grid/locations/a,b?f=GeoJSON", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn f_accepts_the_token_and_media_type_case_insensitively() {
    for f in [
        "GeoJSON",
        "GEOJSON",
        "application/geo%2Bjson",
        "Application/Geo%2BJSON",
        // An unencoded + arrives as a space.
        "application/geo+json",
    ] {
        let uri = format!("/collections/obs/position?coords=POINT(24.94%2060.17)&f={f}");
        let (status, headers, _) = request(&uri, None).await;
        assert_eq!(status, StatusCode::OK, "f={f}");
        assert_eq!(content_type(&headers), "application/geo+json", "f={f}");
    }
}

#[tokio::test]
async fn accept_header_negotiates_and_varies() {
    let uri = "/collections/obs/position?coords=POINT(24.94%2060.17)";
    let (status, headers, json) = get_json(uri, Some("application/geo+json")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), "application/geo+json");
    assert!(varies_on_accept(&headers));
    assert_eq!(json["features"][0]["id"], "helsinki");
    // The self link names the negotiated format.
    assert!(link(&json, "self", "application/geo+json").ends_with("&f=GeoJSON"));

    for (accept, expected) in [
        (
            "application/geo+json;q=0.5, application/prs.coverage+json",
            "application/vnd.cov+json",
        ),
        ("application/geo+json;q=0", "application/vnd.cov+json"),
        ("*/*", "application/vnd.cov+json"),
        (
            "image/png;q=0.4, application/geo+json;q=0.8",
            "application/geo+json",
        ),
        ("image/png", "image/png"),
    ] {
        let (status, headers, _) = request(uri, Some(accept)).await;
        assert_eq!(status, StatusCode::OK, "{accept}");
        assert_eq!(content_type(&headers), expected, "{accept}");
        assert!(varies_on_accept(&headers), "{accept}");
    }
    // No Accept: the default, still varying.
    let (_, headers, _) = request(uri, None).await;
    assert_eq!(content_type(&headers), "application/vnd.cov+json");
    assert!(varies_on_accept(&headers));

    // An explicit f wins over Accept.
    let (_, headers, _) = request(
        &format!("{uri}&f=CoverageJSON"),
        Some("application/geo+json"),
    )
    .await;
    assert_eq!(content_type(&headers), "application/vnd.cov+json");
    assert!(!varies_on_accept(&headers));

    // Radius and locations negotiate too.
    let (_, headers, _) = request(
        "/collections/obs/radius?coords=POINT(24.94%2060.17)&within=10&within-units=km",
        Some("application/geo+json"),
    )
    .await;
    assert_eq!(content_type(&headers), "application/geo+json");
    let (_, headers, _) = request(
        "/collections/obs/locations/tampere",
        Some("application/geo+json"),
    )
    .await;
    assert_eq!(content_type(&headers), "application/geo+json");
}

#[tokio::test]
async fn area_has_no_geojson_even_on_station_series() {
    let uri = "/collections/obs/area?coords=POLYGON((20%2059,30%2059,30%2066,20%2066,20%2059))";
    let (status, _, json) = get_json(&format!("{uri}&f=GeoJSON"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let description = json["description"].as_str().unwrap();
    assert!(
        description.contains("GeoJSON output is not available for area queries")
            && description.contains("available: CoverageJSON"),
        "{description}"
    );
    // One format: Accept has nothing to choose, and the response doesn't vary.
    let (status, headers, _) = request(uri, Some("application/geo+json")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), "application/vnd.cov+json");
    assert!(!varies_on_accept(&headers));
}

#[tokio::test]
async fn gridded_collections_answer_400_for_geojson() {
    for (uri, what) in [
        (
            "/collections/grid/position?coords=POINT(24.94%2060.17)&f=GeoJSON",
            "position queries; available: CoverageJSON, PNG",
        ),
        (
            "/collections/grid/radius?coords=POINT(24.94%2060.17)&within=10&within-units=km&f=GeoJSON",
            "radius queries; available: CoverageJSON",
        ),
    ] {
        let (status, _, json) = get_json(uri, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        let description = json["description"].as_str().unwrap();
        assert!(
            description.contains(&format!("GeoJSON output is not available for {what}")),
            "{description}"
        );
    }
    // Asking through Accept gets the default instead.
    let (status, headers, _) = request(
        "/collections/grid/position?coords=POINT(24.94%2060.17)",
        Some("application/geo+json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), "application/vnd.cov+json");
}

#[tokio::test]
async fn a_parameter_named_like_a_feature_property_is_400() {
    let (status, _, json) = get_json(
        "/collections/odd/position?coords=POINT(24.94%2060.17)&f=GeoJSON",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let description = json["description"].as_str().unwrap();
    assert!(
        description.contains("Parameter 'time'") && description.contains("f=CoverageJSON"),
        "{description}"
    );
    // CoverageJSON still serves it.
    let (status, _, _) = request(
        "/collections/odd/position?coords=POINT(24.94%2060.17)",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn geojson_bodies_are_byte_identical_across_requests() {
    let uri =
        "/collections/obs/radius?coords=POINT(24.94%2060.17)&within=200&within-units=km&f=GeoJSON";
    let (_, first_headers, first) = request(uri, None).await;
    for _ in 0..3 {
        let (_, headers, body) = request(uri, None).await;
        assert_eq!(body, first);
        assert_eq!(headers[header::ETAG], first_headers[header::ETAG]);
    }
}

#[tokio::test]
async fn metadata_advertises_geojson_where_it_is_served() {
    let (_, _, obs) = get_json("/collections/obs", None).await;
    let formats = |doc: &Value, qt: &str| {
        doc["data_queries"][qt]["link"]["variables"]["output_formats"].clone()
    };
    assert_eq!(
        formats(&obs, "locations"),
        json!(["CoverageJSON", "GeoJSON", "PNG"])
    );
    assert_eq!(
        formats(&obs, "position"),
        json!(["CoverageJSON", "GeoJSON", "PNG"])
    );
    assert_eq!(formats(&obs, "radius"), json!(["CoverageJSON", "GeoJSON"]));
    assert_eq!(formats(&obs, "area"), json!(["CoverageJSON"]));
    assert_eq!(
        obs["output_formats"],
        json!(["CoverageJSON", "GeoJSON", "PNG"])
    );

    let (_, _, grid) = get_json("/collections/grid", None).await;
    assert_eq!(formats(&grid, "position"), json!(["CoverageJSON", "PNG"]));
    assert_eq!(formats(&grid, "radius"), json!(["CoverageJSON"]));
    assert_eq!(grid["output_formats"], json!(["CoverageJSON", "PNG"]));
}

#[tokio::test]
async fn api_documents_geojson_where_it_is_served() {
    let (_, _, api) = get_json("/api", None).await;
    assert!(api["components"]["schemas"]["edrFeatureCollectionGeoJSON"].is_object());
    let op = |path: &str| api["paths"][path]["get"].clone();
    let f_enum = |op: &Value| {
        op["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "f")
            .map(|p| p["schema"]["enum"].clone())
    };
    let geojson_schema = |op: &Value| {
        op["responses"]["200"]["content"]["application/geo+json"]["schema"]["$ref"].clone()
    };
    let schema_ref = json!("#/components/schemas/edrFeatureCollectionGeoJSON");
    for path in [
        "/edr/collections/obs/position",
        "/edr/collections/obs/locations/{locationId}",
    ] {
        assert_eq!(geojson_schema(&op(path)), schema_ref, "{path}");
        assert_eq!(
            f_enum(&op(path)),
            Some(json!(["CoverageJSON", "GeoJSON", "PNG"])),
            "{path}"
        );
    }
    let radius = op("/edr/collections/obs/radius");
    assert_eq!(geojson_schema(&radius), schema_ref);
    assert_eq!(f_enum(&radius), Some(json!(["CoverageJSON", "GeoJSON"])));
    // Area, and every gridded route, stay CoverageJSON.
    for path in [
        "/edr/collections/obs/area",
        "/edr/collections/grid/position",
        "/edr/collections/grid/radius",
    ] {
        assert!(geojson_schema(&op(path)).is_null(), "{path}");
    }
    assert_eq!(
        f_enum(&op("/edr/collections/grid/radius")),
        Some(json!(["CoverageJSON"]))
    );
}

#[tokio::test]
async fn conformance_declares_the_geojson_classes() {
    let (_, _, json) = get_json("/conformance", None).await;
    let classes = json["conformsTo"].as_array().unwrap();
    for class in ["geojson", "edr-geojson"] {
        let uri = format!("http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/{class}");
        assert!(classes.iter().any(|c| c == &json!(uri)), "{uri}");
    }
}
