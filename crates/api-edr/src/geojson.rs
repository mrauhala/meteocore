//! EDR GeoJSON for station series (#929): the `application/geo+json`
//! representation of `locations`, `position` and `radius` results on
//! engines that serve time series at named locations
//! (`EdrEngine::serves_station_series`).
//!
//! One feature per coverage, that is per station, like the coverages of the
//! CoverageJSON twin. The geometry is the station's point; `properties`
//! carries the members EDR's `edrProperties` schema requires, followed by
//! the series: `time` (the RFC 3339 instants) and one array per parameter,
//! aligned with `time`, `null` where there is no value.
//!
//! ```json
//! { "type": "Feature", "id": "101004",
//!   "geometry": { "type": "Point", "coordinates": [24.94, 60.17] },
//!   "properties": {
//!     "datetime": "2026-09-29T00:00:00+00:00/2026-09-29T02:00:00+00:00",
//!     "label": "Helsinki Kaisaniemi",
//!     "parameter-name": ["air_temperature", "wind_speed"],
//!     "edrqueryendpoint": "https://…/edr/collections/obs/locations/101004",
//!     "time": ["2026-09-29T00:00:00+00:00", "…", "…"],
//!     "air_temperature": [13.4, 13.5, 13.4],
//!     "wind_speed": [3.5, null, 3.3] } }
//! ```
//!
//! The collection carries the parameters' metadata as the `parameters`
//! foreign member (EDR's parameter objects with their `id`), and the
//! `links` `/req/edr-geojson/content` B names: `self`, an `alternate` per
//! other format and the collection. `numberReturned` counts the features;
//! `numberMatched` the ones before `limit`, when the handler counted them.
//! Serialization streams into the writer with every map in sorted order, so
//! identical queries are byte-identical and the content-derived ETag
//! revalidates (#499).

use std::collections::{BTreeMap, HashMap};
use std::io::Write;

use ds_core::model::{CoverageResponse, DomainDescription, Location, QueryResult};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Serialize, Serializer};
use serde_json::Value;

use crate::response::build_parameter;

/// Property names a feature defines itself; a parameter of the same name
/// cannot be encoded next to them.
pub const RESERVED_PROPERTIES: [&str; 5] = [
    "datetime",
    "label",
    "parameter-name",
    "edrqueryendpoint",
    "time",
];

/// Why a result has no EDR GeoJSON representation.
#[derive(Debug)]
pub enum GeoJsonError {
    /// A coverage that is not a time series at one point without `z` — the
    /// engine broke its `serves_station_series` promise (a server fault).
    NotStationSeries(&'static str),
    /// A parameter named like one of the feature's own properties: a client
    /// can deselect it or ask for CoverageJSON.
    ReservedParameterName(String),
    Json(serde_json::Error),
}

impl From<serde_json::Error> for GeoJsonError {
    fn from(e: serde_json::Error) -> Self {
        GeoJsonError::Json(e)
    }
}

/// A link of the collection's `links` foreign member.
#[derive(Debug, Clone, Serialize)]
pub struct GeoJsonLink {
    pub href: String,
    pub rel: &'static str,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub title: String,
}

/// The locations a result's coverages may belong to, looked up by exact
/// coordinates: a station engine's coverage sits at its location's
/// coordinates (the `serves_station_series` contract). Coordinates two
/// locations share name neither.
pub struct LocationIndex {
    locations: Vec<Location>,
    by_point: HashMap<(u64, u64), Option<usize>>,
}

/// Hash key of a coordinate pair; `-0.0` and `0.0` are one point.
fn point_key(x: f64, y: f64) -> (u64, u64) {
    ((x + 0.0).to_bits(), (y + 0.0).to_bits())
}

impl LocationIndex {
    pub fn new(locations: Vec<Location>) -> Self {
        let mut by_point: HashMap<(u64, u64), Option<usize>> =
            HashMap::with_capacity(locations.len());
        for (i, loc) in locations.iter().enumerate() {
            if !(loc.longitude.is_finite() && loc.latitude.is_finite()) {
                continue;
            }
            by_point
                .entry(point_key(loc.longitude, loc.latitude))
                .and_modify(|slot| *slot = None)
                .or_insert(Some(i));
        }
        LocationIndex {
            locations,
            by_point,
        }
    }

    /// The one location at exactly `(x, y)`.
    pub fn at(&self, x: f64, y: f64) -> Option<&Location> {
        let i = (*self.by_point.get(&point_key(x, y))?)?;
        self.locations.get(i)
    }

    /// The location with this id.
    pub fn by_id(&self, id: &str) -> Option<&Location> {
        self.locations.iter().find(|l| l.id == id)
    }
}

/// The named location a feature describes.
#[derive(Debug, Clone, Copy)]
pub struct FeatureIdentity<'a> {
    pub id: &'a str,
    pub label: &'a str,
}

/// Percent-encode one URL path segment (RFC 3986 unreserved characters
/// pass): location ids are free text (CSV station names have spaces and
/// non-ASCII letters).
pub fn encode_path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// One station series, checked encodable.
struct Series<'a> {
    x: f64,
    y: f64,
    times: Vec<String>,
    result: &'a QueryResult,
    identity: Option<FeatureIdentity<'a>>,
}

/// Encode a station-series result as an EDR GeoJSON FeatureCollection.
///
/// `identify` names the location each coverage (by index) belongs to; a
/// coverage it cannot name is labelled by its coordinates and points at the
/// collection's location list. `collection_url` is
/// `{base}/edr/collections/{id}`; `links` become the collection's `links`.
/// `number_matched` is how many features matched before `limit` (`None`:
/// not counted, the member is left out); `numberReturned` is the features
/// written. Every coverage is checked before any byte is written.
pub fn write_station_series<'a, W: Write>(
    result: &'a CoverageResponse,
    identify: impl Fn(usize, &QueryResult) -> Option<FeatureIdentity<'a>>,
    collection_url: &str,
    links: &[GeoJsonLink],
    number_matched: Option<usize>,
    writer: W,
) -> Result<(), GeoJsonError> {
    let coverages: &[QueryResult] = match result {
        CoverageResponse::Single(q) => std::slice::from_ref(q),
        CoverageResponse::Collection(v) => v,
    };
    let mut series = Vec::with_capacity(coverages.len());
    // Parameters of every coverage, sorted: the collection's `parameters`.
    let mut parameters: BTreeMap<&str, Value> = BTreeMap::new();
    for (i, q) in coverages.iter().enumerate() {
        let (x, y, t) = match &q.domain {
            DomainDescription::PointSeries { x, y, t, z: None } => (*x, *y, t),
            DomainDescription::PointSeries { .. } => {
                return Err(GeoJsonError::NotStationSeries("PointSeries with z"))
            }
            DomainDescription::Point { .. } => return Err(GeoJsonError::NotStationSeries("Point")),
            DomainDescription::Grid { .. } => return Err(GeoJsonError::NotStationSeries("Grid")),
            DomainDescription::VerticalProfile { .. } => {
                return Err(GeoJsonError::NotStationSeries("VerticalProfile"))
            }
            DomainDescription::Section { .. } => {
                return Err(GeoJsonError::NotStationSeries("Section"))
            }
        };
        for name in q.ranges.keys().chain(q.parameters.keys()) {
            if RESERVED_PROPERTIES.contains(&name.as_str()) {
                return Err(GeoJsonError::ReservedParameterName(name.clone()));
            }
        }
        for (name, desc) in &q.parameters {
            parameters.entry(name.as_str()).or_insert_with(|| {
                let mut param = serde_json::Map::with_capacity(5);
                param.insert("id".into(), Value::String(name.clone()));
                if let Value::Object(fields) = build_parameter(desc) {
                    param.extend(fields);
                }
                Value::Object(param)
            });
        }
        series.push(Series {
            x,
            y,
            times: t.iter().map(|t| t.to_rfc3339()).collect(),
            result: q,
            identity: identify(i, q),
        });
    }
    let features = Features {
        series: &series,
        collection_url,
    };
    let collection = FeatureCollection {
        kind: "FeatureCollection",
        features,
        parameters: parameters.into_values().collect(),
        links,
        number_matched,
        number_returned: series.len(),
    };
    serde_json::to_writer(writer, &collection)?;
    Ok(())
}

#[derive(Serialize)]
struct FeatureCollection<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    features: Features<'a>,
    parameters: Vec<Value>,
    links: &'a [GeoJsonLink],
    #[serde(rename = "numberMatched", skip_serializing_if = "Option::is_none")]
    number_matched: Option<usize>,
    #[serde(rename = "numberReturned")]
    number_returned: usize,
}

struct Features<'a> {
    series: &'a [Series<'a>],
    collection_url: &'a str,
}

impl Serialize for Features<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.series.len()))?;
        for s in self.series {
            seq.serialize_element(&Feature {
                series: s,
                collection_url: self.collection_url,
            })?;
        }
        seq.end()
    }
}

struct Feature<'a> {
    series: &'a Series<'a>,
    collection_url: &'a str,
}

#[derive(Serialize)]
struct PointGeometry {
    #[serde(rename = "type")]
    kind: &'static str,
    coordinates: [f64; 2],
}

impl Serialize for Feature<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let s = self.series;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("type", "Feature")?;
        if let Some(identity) = s.identity {
            map.serialize_entry("id", identity.id)?;
        }
        map.serialize_entry(
            "geometry",
            &PointGeometry {
                kind: "Point",
                coordinates: [s.x, s.y],
            },
        )?;
        map.serialize_entry(
            "properties",
            &Properties {
                series: s,
                collection_url: self.collection_url,
            },
        )?;
        map.end()
    }
}

struct Properties<'a> {
    series: &'a Series<'a>,
    collection_url: &'a str,
}

impl Serialize for Properties<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let s = self.series;
        // `edrProperties.datetime`: the instant, or the period the series
        // spans.
        let datetime = match (s.times.first(), s.times.last()) {
            (Some(first), Some(last)) if first != last => format!("{first}/{last}"),
            (Some(only), _) => only.clone(),
            _ => String::new(),
        };
        let (label, endpoint) = match s.identity {
            Some(identity) => (
                identity.label.to_string(),
                format!(
                    "{}/locations/{}",
                    self.collection_url,
                    encode_path_segment(identity.id)
                ),
            ),
            None => (
                format!("POINT({} {})", s.x, s.y),
                format!("{}/locations", self.collection_url),
            ),
        };
        let mut names: Vec<&String> = s.result.ranges.keys().collect();
        names.sort();
        let mut map = serializer.serialize_map(Some(5 + names.len()))?;
        map.serialize_entry("datetime", &datetime)?;
        map.serialize_entry("label", &label)?;
        map.serialize_entry("parameter-name", &names)?;
        map.serialize_entry("edrqueryendpoint", &endpoint)?;
        map.serialize_entry("time", &s.times)?;
        for name in names {
            map.serialize_entry(name, &Values(&s.result.ranges[name].values))?;
        }
        map.end()
    }
}

/// A range's values: a number, or `null` for a missing or non-finite one
/// (as in CoverageJSON).
struct Values<'a>(&'a [Option<f64>]);

impl Serialize for Values<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for v in self.0 {
            seq.serialize_element(&v.filter(|v| v.is_finite()))?;
        }
        seq.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use ds_core::model::{NdArray, ParameterDescription};

    fn series(x: f64, y: f64, hours: &[u32], values: &[(&str, Vec<Option<f64>>)]) -> QueryResult {
        let t: Vec<DateTime<Utc>> = hours
            .iter()
            .map(|h| format!("2026-01-01T{h:02}:00:00Z").parse().unwrap())
            .collect();
        let mut parameters = HashMap::new();
        let mut ranges = HashMap::new();
        for (name, v) in values {
            parameters.insert(
                name.to_string(),
                ParameterDescription {
                    label: name.replace('_', " "),
                    unit: "°C".into(),
                    observed_property: name.to_string(),
                    standard_name: None,
                },
            );
            ranges.insert(
                name.to_string(),
                NdArray {
                    shape: vec![v.len()],
                    axis_names: vec!["t".into()],
                    values: v.clone(),
                },
            );
        }
        QueryResult {
            domain: DomainDescription::PointSeries { x, y, t, z: None },
            parameters,
            ranges,
        }
    }

    fn encode(result: &CoverageResponse, index: &LocationIndex) -> Result<Value, GeoJsonError> {
        let mut out = Vec::new();
        write_station_series(
            result,
            |_, q| match q.domain {
                DomainDescription::PointSeries { x, y, .. } => {
                    index.at(x, y).map(|l| FeatureIdentity {
                        id: &l.id,
                        label: &l.label,
                    })
                }
                _ => None,
            },
            "https://example.org/edr/collections/obs",
            &[],
            Some(result_len(result)),
            &mut out,
        )?;
        Ok(serde_json::from_slice(&out).unwrap())
    }

    fn result_len(result: &CoverageResponse) -> usize {
        match result {
            CoverageResponse::Single(_) => 1,
            CoverageResponse::Collection(v) => v.len(),
        }
    }

    fn loc(id: &str, label: &str, lon: f64, lat: f64) -> Location {
        Location {
            id: id.into(),
            label: label.into(),
            latitude: lat,
            longitude: lon,
        }
    }

    #[test]
    fn one_feature_per_station_with_aligned_arrays() {
        let index = LocationIndex::new(vec![loc("Alajärvi Möksy", "Möksy", 24.26, 63.09)]);
        let result = CoverageResponse::Single(series(
            24.26,
            63.09,
            &[0, 1, 2],
            &[
                ("wind_speed", vec![Some(3.5), None, Some(f64::NAN)]),
                (
                    "air_temperature",
                    vec![Some(-17.9), Some(-19.0), Some(-20.2)],
                ),
            ],
        ));
        let json = encode(&result, &index).unwrap();
        assert_eq!(json["type"], "FeatureCollection");
        assert_eq!(json["numberReturned"], 1);
        let f = &json["features"][0];
        assert_eq!(f["id"], "Alajärvi Möksy");
        assert_eq!(
            f["geometry"]["coordinates"],
            serde_json::json!([24.26, 63.09])
        );
        let p = &f["properties"];
        assert_eq!(
            p["datetime"],
            "2026-01-01T00:00:00+00:00/2026-01-01T02:00:00+00:00"
        );
        assert_eq!(p["label"], "Möksy");
        assert_eq!(
            p["edrqueryendpoint"],
            "https://example.org/edr/collections/obs/locations/Alaj%C3%A4rvi%20M%C3%B6ksy"
        );
        assert_eq!(
            p["parameter-name"],
            serde_json::json!(["air_temperature", "wind_speed"])
        );
        assert_eq!(p["time"].as_array().unwrap().len(), 3);
        assert_eq!(
            p["air_temperature"],
            serde_json::json!([-17.9, -19.0, -20.2])
        );
        // Missing and non-finite values are null, never 0.
        assert_eq!(p["wind_speed"], serde_json::json!([3.5, null, null]));
        let params = json["parameters"].as_array().unwrap();
        let ids: Vec<&str> = params.iter().map(|p| p["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["air_temperature", "wind_speed"]);
        assert_eq!(params[0]["type"], "Parameter");
        assert!(params[0]["observedProperty"]["label"]["en"].is_string());
    }

    #[test]
    fn an_unnamed_coverage_is_labelled_by_its_point() {
        // Two locations share the point: neither names the coverage.
        let index = LocationIndex::new(vec![loc("a", "A", 10.0, 60.0), loc("b", "B", 10.0, 60.0)]);
        let result =
            CoverageResponse::Collection(vec![series(10.0, 60.0, &[5], &[("t", vec![Some(1.0)])])]);
        let json = encode(&result, &index).unwrap();
        let f = &json["features"][0];
        assert!(f.get("id").is_none());
        assert_eq!(f["properties"]["label"], "POINT(10 60)");
        assert_eq!(f["properties"]["datetime"], "2026-01-01T05:00:00+00:00");
        assert_eq!(
            f["properties"]["edrqueryendpoint"],
            "https://example.org/edr/collections/obs/locations"
        );
    }

    #[test]
    fn empty_collection_is_an_empty_feature_collection() {
        let index = LocationIndex::new(Vec::new());
        let json = encode(&CoverageResponse::Collection(Vec::new()), &index).unwrap();
        assert_eq!(json["features"], serde_json::json!([]));
        assert_eq!(json["numberMatched"], 0);
        assert_eq!(json["parameters"], serde_json::json!([]));
    }

    #[test]
    fn non_series_and_reserved_names_are_rejected() {
        let index = LocationIndex::new(Vec::new());
        let grid = CoverageResponse::Single(QueryResult {
            domain: DomainDescription::Grid {
                x: vec![1.0],
                y: vec![2.0],
                t: None,
                z: None,
            },
            parameters: HashMap::new(),
            ranges: HashMap::new(),
        });
        assert!(matches!(
            encode(&grid, &index),
            Err(GeoJsonError::NotStationSeries("Grid"))
        ));
        let reserved =
            CoverageResponse::Single(series(1.0, 2.0, &[0], &[("time", vec![Some(1.0)])]));
        assert!(matches!(
            encode(&reserved, &index),
            Err(GeoJsonError::ReservedParameterName(ref n)) if n == "time"
        ));
    }

    #[test]
    fn path_segments_are_percent_encoded() {
        assert_eq!(encode_path_segment("0-20000-0-02598"), "0-20000-0-02598");
        assert_eq!(encode_path_segment("a b/c?d"), "a%20b%2Fc%3Fd");
        assert_eq!(encode_path_segment("ship:ABC"), "ship%3AABC");
    }
}
