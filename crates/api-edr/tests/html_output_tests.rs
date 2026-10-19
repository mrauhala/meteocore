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
use ds_core::edr_engine::{select_parameters, EdrEngine, LocationFilter, TrajectoryShape};
use ds_core::error::DataServerError;
use ds_core::feature::{
    Bbox, DatetimeInterval, Feature, FeaturePage, FeatureQuery, Geometry, PropertyValue,
};
use ds_core::feature_engine::FeatureEngine;
use ds_core::instances::RunInfo;
use ds_core::model::*;
use ds_core::vertical::{VerticalDimension, VerticalKind};

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
    fn get_locations(&self) -> Result<Arc<LocationSnapshot>, DataServerError> {
        Ok(Arc::new(LocationSnapshot::new(vec![
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
        ])))
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
        related: Vec::new(),
    }
}

/// A radar site: its trajectory is a cross-section.
struct Section;

impl EdrEngine for Section {
    fn get_locations(&self) -> Result<Arc<LocationSnapshot>, DataServerError> {
        Ok(LocationSnapshot::empty())
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
struct Stations {
    /// Its location inventory cannot be read: the HTML page still answers.
    inventory_down: bool,
}

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
    fn get_locations(&self) -> Result<Arc<LocationSnapshot>, DataServerError> {
        if self.inventory_down {
            return Err(DataServerError::Engine("inventory unavailable".into()));
        }
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

/// A station network with more locations than an HTML page lists (#1138).
struct Network;

impl EdrEngine for Network {
    fn get_locations(&self) -> Result<Arc<LocationSnapshot>, DataServerError> {
        Ok(Arc::new(LocationSnapshot::new(
            (0..250)
                .map(|i| Location {
                    id: format!("st{i:03}"),
                    label: format!("Station {i}"),
                    latitude: 60.0 + f64::from(i) * 0.01,
                    longitude: 25.0,
                })
                .collect(),
        )))
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

    fn get_parameters(&self) -> Vec<String> {
        vec!["temperature".to_string()]
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        Some((t0(), t0() + Duration::hours(1)))
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([24.0, 59.0, 27.0, 63.0])
    }

    fn supported_query_types(&self) -> Vec<String> {
        vec!["locations".to_string()]
    }
}

/// One `Sparse` station: id, longitude, latitude and observation times.
type SparseStation = (&'static str, f64, f64, Vec<DateTime<Utc>>);

/// A station network whose stations stopped reporting at different times
/// (#1138): `quiet`, nearest the extent's centre, last reported three days
/// before the collection's newest observation; `busy` reports to the end.
/// As the CSV, BUFR and PostGIS engines do, a station answers a window
/// without its data with `LocationNotFound`, and `/locations?datetime=`
/// lists the stations with data in it.
struct Sparse;

impl Sparse {
    fn stations() -> [SparseStation; 2] {
        [
            ("quiet", 25.0, 60.0, vec![t0() - Duration::days(3)]),
            ("busy", 26.0, 61.5, vec![t0(), t0() + Duration::hours(1)]),
        ]
    }

    fn series(
        id: &str,
        window: Option<(DateTime<Utc>, DateTime<Utc>)>,
    ) -> Result<CoverageResponse, DataServerError> {
        let Some((_, x, y, times)) = Self::stations().into_iter().find(|s| s.0 == id) else {
            return Err(DataServerError::LocationNotFound(id.into()));
        };
        let t: Vec<DateTime<Utc>> = times
            .into_iter()
            .filter(|t| window.is_none_or(|(start, end)| start <= *t && *t <= end))
            .collect();
        if t.is_empty() {
            return Err(DataServerError::LocationNotFound(format!(
                "{id} (no data in time range)"
            )));
        }
        let values: Vec<f64> = (0..t.len()).map(|i| i as f64).collect();
        let n = t.len();
        Ok(CoverageResponse::Single(coverage(
            DomainDescription::PointSeries { x, y, t, z: None },
            &["t"],
            &[n],
            &values,
        )))
    }
}

impl EdrEngine for Sparse {
    fn get_locations(&self) -> Result<Arc<LocationSnapshot>, DataServerError> {
        Ok(Arc::new(LocationSnapshot::new(
            Self::stations()
                .into_iter()
                .map(|(id, x, y, _)| Location {
                    id: id.to_string(),
                    label: format!("Station {id}"),
                    latitude: y,
                    longitude: x,
                })
                .collect(),
        )))
    }

    fn serves_station_series(&self) -> bool {
        true
    }

    fn location_time_filter<'a>(
        &'a self,
        intervals: &'a [DatetimeInterval],
    ) -> Option<LocationFilter<'a>> {
        Some(Box::new(move |location| {
            Self::stations().iter().any(|(id, _, _, times)| {
                *id == location.id
                    && times.iter().any(|t| {
                        intervals.iter().any(|i| {
                            i.start.is_none_or(|s| s <= *t) && i.end.is_none_or(|e| *t <= e)
                        })
                    })
            })
        }))
    }

    fn query_location(
        &self,
        id: &str,
        window: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Self::series(id, window)
    }

    /// The nearest station's series.
    fn query_position(
        &self,
        coords: &str,
        window: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        let (lat, lon) = ds_core::feature::parse_point_coords(coords)?;
        let distance = |s: &SparseStation| (s.1 - lon).powi(2) + (s.2 - lat).powi(2);
        let nearest = Self::stations()
            .into_iter()
            .min_by(|a, b| distance(a).total_cmp(&distance(b)))
            .map(|s| s.0)
            .unwrap_or_default();
        Self::series(nearest, window)
    }

    fn get_parameters(&self) -> Vec<String> {
        vec!["temperature".to_string()]
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        Some((t0() - Duration::days(3), t0() + Duration::hours(1)))
    }

    fn get_available_times(&self) -> Option<Vec<DateTime<Utc>>> {
        let mut times: Vec<DateTime<Utc>> = Self::stations()
            .into_iter()
            .flat_map(|(_, _, _, times)| times)
            .collect();
        times.sort();
        Some(times)
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([24.0, 59.0, 27.0, 62.0])
    }

    fn supported_query_types(&self) -> Vec<String> {
        vec!["locations".to_string(), "position".to_string()]
    }
}

/// A collection on pressure levels with two parameters (#1138): its forms
/// carry a `z` select and a checkbox per parameter.
struct Levels;

impl EdrEngine for Levels {
    fn get_locations(&self) -> Result<Arc<LocationSnapshot>, DataServerError> {
        Ok(LocationSnapshot::empty())
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

    /// Parameter names checked as the real engines check them.
    fn query_position(
        &self,
        _: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        select_parameters(parameters, &["temperature", "humidity"])?;
        Ok(point_series())
    }

    fn query_area(
        &self,
        _: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        select_parameters(parameters, &["temperature", "humidity"])?;
        Ok(grid())
    }

    fn get_parameters(&self) -> Vec<String> {
        vec!["temperature".to_string(), "humidity".to_string()]
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        Some((t0(), t0() + Duration::hours(1)))
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([20.0, 58.0, 30.0, 62.0])
    }

    fn get_vertical_extent(&self) -> Option<VerticalDimension> {
        Some(VerticalDimension::new(
            VerticalKind::Pressure,
            vec![850.0, 500.0],
        ))
    }

    fn supported_query_types(&self) -> Vec<String> {
        vec!["position".to_string(), "area".to_string()]
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
        station_grid: None,
        bufr: None,
        satellite: None,
        netcdf: None,
        preview: None,
        derive_wind: None,
    }
}

fn router() -> axum::Router {
    let every = Arc::new(Every);
    let mut engines: HashMap<String, Arc<dyn EdrEngine>> = HashMap::new();
    engines.insert("c".to_string(), every.clone());
    engines.insert("pvol".to_string(), Arc::new(Section));
    engines.insert(
        "obs".to_string(),
        Arc::new(Stations {
            inventory_down: false,
        }),
    );
    engines.insert(
        "obsdown".to_string(),
        Arc::new(Stations {
            inventory_down: true,
        }),
    );
    engines.insert("net".to_string(), Arc::new(Network));
    engines.insert("sparse".to_string(), Arc::new(Sparse));
    engines.insert("lev".to_string(), Arc::new(Levels));
    let mut feature_engines: HashMap<String, Arc<dyn FeatureEngine>> = HashMap::new();
    feature_engines.insert("c".to_string(), every);
    let collections = HashMap::from([
        ("c".to_string(), config("c", "Every <query> & \"more\"")),
        ("pvol".to_string(), config("pvol", "Radar site")),
        ("obs".to_string(), config("obs", "Stations")),
        (
            "obsdown".to_string(),
            config("obsdown", "Stations, inventory down"),
        ),
        ("net".to_string(), config("net", "Network")),
        ("sparse".to_string(), config("sparse", "Sparse network")),
        ("lev".to_string(), config("lev", "Pressure levels")),
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
    // A PNG link's body is not text: lossy, for the link checks.
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
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
    // Displayed without a trailing `.0` (#1138): the same numbers.
    for v in 101..=112 {
        assert!(body.contains(&format!("<td>{v}</td>")), "{v}");
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
        body.contains("<td>25.5</td><td>60.75</td><td>1000</td><td>14</td>"),
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
        // A forecast's row opens the latest run's whole series (#1138).
        format!("{base}/locations/here?f=html"),
        // The HTML list is paged by default (#1138).
        format!("{base}/locations?limit=100&amp;f=html"),
        format!("{base}/locations?limit=100&amp;f=GeoJSON"),
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
/// href (#980, #984). The query rows open each query's HTML view, its form
/// (#1138).
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
    assert!(body.contains(&format!("href=\"{base}/position?f=html\"")));

    // A location's link and geometry type, an item's link type.
    let (_, _, body) = get("/collections/c/locations?f=html", None).await;
    assert!(body.contains("<td>Point</td>"), "{body}");
    assert!(body.contains(&format!(
        "<code>data</code> <a class=\"table-link\" href=\"{base}/locations/here\">Data for &lt;b&gt;Here&lt;/b&gt;</a> <code>application/vnd.cov+json</code>"
    )), "{body}");
    // A data table has its own layout, never the key/value table's row
    // header width (#1131).
    assert!(body.contains("<table class=\"data-table\""), "{body}");
    assert!(!body.contains("<table class=\"properties\">"), "{body}");
    let (_, _, body) = get("/collections/c/items?limit=2&f=html", None).await;
    assert!(body.contains("<code>application/geo+json</code>"), "{body}");
}

/// An items page's tab and heading name its item range and whether a
/// filter narrows it, as Features items pages do (#1131).
#[tokio::test]
async fn items_pages_name_their_range() {
    let (_, _, body) = get("/collections/c/items?limit=2&offset=1&f=html", None).await;
    assert!(
        body.contains(
            "<title>Items 2–3 · Every &lt;query&gt; &amp; &quot;more&quot; · MeteoCore</title>"
        ),
        "{body}"
    );
    assert!(body.contains("<h1>Items 2–3</h1>"), "{body}");
    let (_, _, body) = get("/collections/c/items?bbox=0,0,90,90&limit=1&f=html", None).await;
    assert!(body.contains("<title>Items 1–1 · filtered · "), "{body}");
    // A single item keeps its id.
    let (_, _, body) = get("/collections/c/items/s1?f=html", None).await;
    assert!(body.contains("<title>Item s1 · "), "{body}");
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

/// The location panel is extra: when the inventory cannot be read, a station
/// query's HTML page still answers with its CoverageJSON, without the panel.
#[tokio::test]
async fn station_pages_render_without_an_inventory() {
    for uri in [
        format!("/collections/obsdown/position?coords={POINT}&f=html"),
        format!("/collections/obsdown/radius?coords={POINT}&within=500&within-units=km&f=html"),
    ] {
        let (status, headers, body) = get(&uri, None).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert!(
            headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/html"),
            "{uri}"
        );
        assert!(!body.contains("<code>numberReturned</code>"), "{uri}");
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
                format!("Values · {}", escape(label))
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

/// Every `href="…"` of a page, `&amp;` decoded and any fragment dropped.
fn hrefs(body: &str) -> Vec<String> {
    body.split("href=\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .map(|href| href.replace("&amp;", "&"))
        .map(|href| href.split('#').next().unwrap_or_default().to_owned())
        .collect()
}

/// The URL a form's preview shows: the request it runs.
fn form_url(body: &str) -> String {
    body.split_once("data-query-url>")
        .and_then(|(_, rest)| rest.split_once("</code>"))
        .map(|(url, _)| url.replace("&amp;", "&"))
        .unwrap_or_else(|| panic!("no form URL: {body}"))
}

/// #1138: a data query's link from a page opens its HTML view; without
/// coordinates that is the query's form (200), whose example runs. A JSON
/// client still gets the 400 naming the missing parameter.
#[tokio::test]
async fn bare_queries_answer_html_with_their_form() {
    let run = format!("/collections/c/instances/{INSTANCE}");
    for (uri, field) in [
        ("/collections/c/position".to_string(), "coords"),
        ("/collections/c/area".to_string(), "coords"),
        ("/collections/c/radius".to_string(), "coords"),
        ("/collections/c/trajectory".to_string(), "coords"),
        ("/collections/c/cube".to_string(), "bbox"),
        (format!("{run}/position"), "coords"),
        (format!("{run}/area"), "coords"),
        (format!("{run}/radius"), "coords"),
        (format!("{run}/cube"), "bbox"),
        ("/collections/pvol/trajectory".to_string(), "coords"),
        ("/collections/obs/position".to_string(), "coords"),
        ("/collections/obs/radius".to_string(), "coords"),
    ] {
        for (request, accept) in [
            (with_f(&uri, "html"), None),
            (uri.clone(), Some(BROWSER)),
            (with_f(&format!("{uri}?{field}="), "HTML"), None),
        ] {
            let (status, headers, body) = get(&request, accept).await;
            assert_eq!(status, StatusCode::OK, "{request}: {body}");
            assert_html_page(&request, &headers, &body);
            assert!(body.contains("data-edr-query"), "{request}");
            assert!(body.contains(&format!("name=\"{field}\"")), "{request}");
            // The query crumb names the query.
            assert!(body.contains(" query</a></nav>"), "{request}: {body}");
            // The form's example is a request that works.
            let url = form_url(&body);
            let path = url.strip_prefix("https://example.org/edr").unwrap();
            assert!(path.ends_with("&f=HTML"), "{url}");
            let (status, _, page) = get(path, None).await;
            assert_eq!(status, StatusCode::OK, "{url}: {page}");
            assert!(page.contains("data-edr-query"), "{url}");
        }
        let (status, _, body) = get(&uri, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        assert!(
            body.contains(&format!("`{field}`")) || body.contains(&format!("a {field}")),
            "{body}"
        );
    }
    // A position on a station collection lists its stations' series: the
    // example is a station, the last day (all of this shorter series).
    let (_, _, body) = get("/collections/obs/position?f=html", None).await;
    assert!(
        form_url(&body)
            .contains("coords=POINT(25%2060)&datetime=2026-06-07T00:00:00Z/2026-06-07T01:00:00Z"),
        "{body}"
    );
    // An unknown collection or a query it does not offer has no form: the
    // handler's error page answers.
    for uri in [
        "/collections/nope/position?f=html",
        "/collections/net/area?f=html",
    ] {
        let (status, _, body) = get(uri, None).await;
        assert!(status.is_client_error(), "{uri}: {status}");
        assert!(!body.contains("data-edr-query"), "{uri}");
    }
}

/// #1138: no link on a catalog, collection, run, list or data page ends in
/// a 400 when a browser follows it.
#[tokio::test]
async fn no_link_on_a_page_ends_in_a_400() {
    let mut checked = 0;
    for page in [
        "/collections?f=html".to_string(),
        "/collections/c?f=html".to_string(),
        "/collections/pvol?f=html".to_string(),
        "/collections/obs?f=html".to_string(),
        "/collections/c/instances?f=html".to_string(),
        format!("/collections/c/instances/{INSTANCE}?f=html"),
        format!("/collections/c/position?coords={POINT}&f=html"),
        "/collections/c/position?f=html".to_string(),
        "/collections/obs/locations?f=html".to_string(),
        "/collections/sparse/locations?f=html".to_string(),
        "/collections/sparse/position?f=html".to_string(),
        format!("/collections/lev/position?coords={POINT}&f=html"),
    ] {
        let (status, _, body) = get(&page, None).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        for href in hrefs(&body) {
            // `obsdown`'s inventory is down by design: its list is a 500.
            let Some(path) = href
                .strip_prefix("https://example.org/edr")
                .filter(|path| !path.contains("/obsdown/"))
            else {
                continue;
            };
            let (status, _, answer) = get(path, Some(BROWSER)).await;
            assert!(
                status.is_success(),
                "{page} links {href}: {status} {answer}"
            );
            checked += 1;
        }
    }
    assert!(checked > 100, "{checked} links checked");
}

/// #1138: the HTML `/locations` list pages 100 locations at a time without
/// `limit`, with a pager; its rows open their data at a recent time. The
/// GeoJSON keeps the complete inventory.
#[tokio::test]
async fn the_html_location_list_pages_by_default() {
    let base = "https://example.org/edr/collections/net";
    for (uri, accept) in [
        ("/collections/net/locations?f=html", None),
        ("/collections/net/locations", Some(BROWSER)),
    ] {
        let (status, _, html) = get(uri, accept).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(html.matches("<tr data-location=").count(), 100, "{uri}");
        assert!(html.contains("1–100 of 250 locations"), "{uri}");
        assert!(html.contains("<nav class=\"pagination\""), "{uri}");
        assert!(
            html.contains(&format!("rel=\"next\" href=\"{base}/locations?")),
            "{uri}: {html}"
        );
        assert!(html.contains("limit=100&amp;offset=100"), "{uri}");
    }
    let (_, _, html) = get("/collections/net/locations?f=html", None).await;
    // The table comes first; the request, links and document are folded.
    let table = html.find("<tr data-location=").unwrap();
    assert!(
        table
            < html
                .find("<summary>Request, links and document</summary>")
                .unwrap()
    );
    assert!(table < html.find("<h2>Links</h2>").unwrap());
    // A row opens its data page at the latest advertised time.
    assert!(
        html.contains(&format!(
            "href=\"{base}/locations/st000?datetime=2026-06-07T01:00:00Z&amp;f=html\">Station 0</a>"
        )),
        "{html}"
    );
    // Coordinates are shown to 5 decimals, the exact value kept.
    assert!(html.contains("<td title=\"60.07\">60.07</td>") || html.contains("<td>60.07</td>"));
    assert!(
        html.contains("id=\"feature-map\""),
        "the page's points are mapped"
    );
    // Without WebGL the map says where the same points are (#1136).
    assert!(html.contains(
        "The map needs JavaScript and WebGL. The locations of this page are listed in the table."
    ));
    let (_, _, last) = get(
        "/collections/net/locations?f=html&limit=100&offset=200",
        None,
    )
    .await;
    assert_eq!(last.matches("<tr data-location=").count(), 50);
    assert!(last.contains("201–250 of 250 locations"));
    assert!(last.contains("rel=\"prev\""));
    // A station series' rows open the station's whole series: the
    // collection's last day may be none of its (see
    // `station_rows_open_data_the_station_has`).
    let (_, _, obs) = get("/collections/obs/locations?f=html", None).await;
    assert!(
        obs.contains("href=\"https://example.org/edr/collections/obs/locations/here?f=html\""),
        "{obs}"
    );
    // The GeoJSON is unchanged: the whole inventory, no paging members.
    let (_, _, json) = get("/collections/net/locations", None).await;
    let json: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(json["features"].as_array().unwrap().len(), 250);
    assert!(json.get("numberMatched").is_none());
}

/// #1138: a data page leads with the data: what and where it answers, a
/// summary, the plot, the values; then the request as a form, and the
/// links, parameters and document folded below.
#[tokio::test]
async fn data_pages_lead_with_the_data() {
    let collection = "Every &lt;query&gt; &amp; &quot;more&quot;";
    let (status, _, body) = get(
        &format!("/collections/c/position?coords={POINT}&f=html"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(&format!(
            "<title>Position at 60° N, 25° E · {collection} · MeteoCore</title>"
        )),
        "{body}"
    );
    assert!(body.contains(&format!("<h1>Position at 60° N, 25° E · {collection}</h1>")));
    let at = |needle: &str| {
        body.find(needle)
            .unwrap_or_else(|| panic!("{needle}: {body}"))
    };
    let summary = at("<dl class=\"edr-facts\">");
    let chart = at(&format!(
        "<img src=\"https://example.org/edr/collections/c/position?coords={POINT}&amp;f=PNG\""
    ));
    let values = at("<h2>Values</h2>");
    let form = at("data-edr-query");
    let folded = at("<details class=\"edr-more spaced\">");
    let links = at("<h2>Links</h2>");
    assert!(summary < chart && chart < values && values < form && form < folded && folded < links);
    // The run it read, the time span and each column's unit.
    assert!(
        body.contains(
            "<dt>Model run</dt><dd>The collection&#39;s default; choose one under Model runs</dd>"
        ),
        "{body}"
    );
    assert!(body.contains("2026-06-07 00:00 UTC → 2026-06-07 01:00 UTC"));
    assert!(body.contains("(°C) · temperature</th>"), "{body}");
    // The form holds the request.
    assert!(body.contains("value=\"POINT(25 60)\""));
    // An area has no plot; its heading names its bounds.
    let (_, _, area) = get(
        &format!("/collections/c/area?coords={POLYGON}&f=html"),
        None,
    )
    .await;
    assert!(!area.contains("<img src="));
    assert!(area.contains("<h1>Area 59° N to 61° N, 24° E to 26° E · "));
    // A run's page names it.
    let (_, _, run) = get(
        &format!("/collections/c/instances/{INSTANCE}/position?coords={POINT}&f=html"),
        None,
    )
    .await;
    assert!(
        run.contains("<dt>Model run</dt><dd>2026-06-07T00:00:00Z</dd>"),
        "{run}"
    );
    assert!(run.contains(&format!(">Run {INSTANCE}</a>")), "{run}");
}

/// #1138: a station's row opens data the station has. The collection's
/// last day is not a station's: one that stopped reporting answers it with
/// a 404, as the CSV, BUFR and PostGIS engines do. The row opens its whole
/// series, or the page's own `datetime` filter, in which every listed
/// station has data.
#[tokio::test]
async fn station_rows_open_data_the_station_has() {
    let base = "https://example.org/edr/collections/sparse/locations";
    let (status, _, html) = get("/collections/sparse/locations?f=html", None).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(
        html.contains(&format!("href=\"{base}/quiet?f=html\">Station quiet</a>")),
        "{html}"
    );
    // Only `quiet` reported in the filter's window.
    let filter = "2026-06-03T00:00:00Z/2026-06-05T00:00:00Z";
    let (status, _, filtered) = get(
        &format!("/collections/sparse/locations?datetime={filter}&f=html"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{filtered}");
    assert_eq!(filtered.matches("<tr data-location=").count(), 1);
    assert!(
        filtered.contains(&format!(
            "href=\"{base}/quiet?datetime={filter}&amp;f=html\">Station quiet</a>"
        )),
        "{filtered}"
    );
    // The end point column opens the same window.
    assert!(
        filtered.contains(&format!("href=\"{base}/quiet?datetime={filter}\"")),
        "{filtered}"
    );
    // Every location link on both pages answers a browser.
    for page in [&html, &filtered] {
        let links: Vec<String> = hrefs(page)
            .into_iter()
            .filter(|href| href.starts_with(&format!("{base}/")))
            .collect();
        assert!(links.len() >= 3, "{links:?}");
        for href in links {
            let path = href.strip_prefix("https://example.org/edr").unwrap();
            let (status, _, answer) = get(path, Some(BROWSER)).await;
            assert_eq!(status, StatusCode::OK, "{href}: {answer}");
        }
    }
    // What a row opened before: the collection's last day, none of `quiet`'s.
    let (status, _, _) = get(
        "/collections/sparse/locations/quiet?datetime=2026-06-06T01:00:00Z/2026-06-07T01:00:00Z&f=html",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// #1138: a station collection's position form opens the last day, not
/// the whole series an observation table may hold for years, at the
/// station nearest the centre that has data in it.
#[tokio::test]
async fn a_station_form_opens_the_last_day_at_a_reporting_station() {
    let (status, _, body) = get("/collections/sparse/position?f=html", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let url = form_url(&body);
    // `busy`, not `quiet`, which is nearer the centre but stopped reporting.
    assert!(
        url.contains("coords=POINT(26%2061.5)&datetime=2026-06-06T01:00:00Z/2026-06-07T01:00:00Z&"),
        "{url}"
    );
    assert!(
        body.contains(
            "<option value=\"2026-06-06T01:00:00Z/2026-06-07T01:00:00Z\" selected>Last 24 hours</option>"
        ),
        "{body}"
    );
    let path = url.strip_prefix("https://example.org/edr").unwrap();
    let (status, _, page) = get(path, None).await;
    assert_eq!(status, StatusCode::OK, "{url}: {page}");
}

/// `&lt;`, `&gt;`, `&quot;`, `&#39;` and `&amp;` decoded.
fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// The value of `name="…"` in a tag's text.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let start = tag.find(&format!(" {name}=\""))? + name.len() + 3;
    let end = tag[start..].find('"')? + start;
    Some(unescape(&tag[start..end]))
}

/// What a browser without JavaScript submits from a page's query form: its
/// `action`, and every named `<input>` (the `<noscript>` one included) with
/// its value and every `<select>` with its selected option, else its first,
/// in document order.
fn native_submission(body: &str) -> (String, Vec<(String, String)>) {
    let form = body
        .split_once("<form class=\"panel spaced edr-query\"")
        .and_then(|(_, rest)| rest.split_once("</form>"))
        .map(|(form, _)| form)
        .unwrap_or_else(|| panic!("no form: {body}"));
    let action = attribute(form.split_once('>').unwrap().0, "action").unwrap();
    let mut pairs = Vec::new();
    let mut rest = form;
    while let Some(i) = rest.find('<') {
        rest = &rest[i..];
        let tag = &rest[..rest.find('>').unwrap()];
        if tag.starts_with("<input") {
            if let Some(name) = attribute(tag, "name") {
                pairs.push((name, attribute(tag, "value").unwrap_or_default()));
            }
        } else if tag.starts_with("<select") {
            let end = rest.find("</select>").unwrap();
            let options: Vec<&str> = rest[..end]
                .split("<option")
                .skip(1)
                .map(|option| option.split_once('>').unwrap().0)
                .collect();
            let chosen = options
                .iter()
                .find(|option| option.contains(" selected"))
                .or(options.first())
                .unwrap();
            pairs.push((
                attribute(tag, "name").unwrap(),
                attribute(chosen, "value").unwrap(),
            ));
            rest = &rest[end..];
        }
        rest = &rest[1..];
    }
    (action, pairs)
}

/// #1138: a query form submitted by a browser without JavaScript runs. Its
/// empty fields, `z=` and `parameter-name=`, mean every level and every
/// parameter, as leaving them out does.
#[tokio::test]
async fn forms_submit_without_javascript() {
    let encode = |value: &str| {
        percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
    };
    for page in [
        format!("/collections/lev/position?coords={POINT}&f=html"),
        format!("/collections/lev/area?coords={POLYGON}&f=html"),
        "/collections/lev/position?f=html".to_string(),
        "/collections/lev/area?f=html".to_string(),
        format!("/collections/c/position?coords={POINT}&f=html"),
        "/collections/c/radius?f=html".to_string(),
        "/collections/c/cube?f=html".to_string(),
        format!("/collections/c/instances/{INSTANCE}/position?f=html"),
        "/collections/obs/position?f=html".to_string(),
        "/collections/obs/locations/here?f=html".to_string(),
        "/collections/sparse/position?f=html".to_string(),
    ] {
        let (status, _, body) = get(&page, None).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let (action, pairs) = native_submission(&body);
        let query: Vec<String> = pairs
            .iter()
            .map(|(name, value)| format!("{}={}", encode(name), encode(value)))
            .collect();
        let url = format!(
            "{}?{}",
            action.strip_prefix("https://example.org/edr").unwrap(),
            query.join("&")
        );
        let (status, headers, answer) = get(&url, None).await;
        assert_eq!(status, StatusCode::OK, "{page} submits {url}: {answer}");
        assert_html_page(&url, &headers, &answer);
    }
    // A data page that named neither sends both empty, and the query
    // answers them in every format.
    let (_, _, body) = get(
        &format!("/collections/lev/position?coords={POINT}&f=html"),
        None,
    )
    .await;
    let (_, pairs) = native_submission(&body);
    for name in ["parameter-name", "z"] {
        assert!(
            pairs.contains(&(name.to_owned(), String::new())),
            "{name}: {pairs:?}"
        );
    }
    let (status, _, json) = get(
        &format!("/collections/lev/position?coords={POINT}&z=&parameter-name="),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    // Only the blank value is every parameter: an empty name in a list is
    // still a name the collection does not advertise.
    let (status, _, json) = get(
        &format!("/collections/lev/position?coords={POINT}&parameter-name=temperature,,humidity"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert!(json.contains("Unknown parameter ''"), "{json}");
}

/// #1138: a data page's form follows its request and its route. Names in
/// any case check their parameter, as the query matches them; a location
/// list offers no PNG; a location or item whose id names a query is that
/// location or item, never a form.
#[tokio::test]
async fn data_page_forms_follow_their_request() {
    let (status, _, body) = get(
        &format!("/collections/lev/position?coords={POINT}&parameter-name=TEMPERATURE&f=html"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains("data-parameter value=\"temperature\" checked>"),
        "{body}"
    );
    assert!(
        body.contains("data-parameter value=\"humidity\">"),
        "{body}"
    );
    assert!(
        form_url(&body).contains("&parameter-name=temperature&"),
        "{body}"
    );
    let png = "<option value=\"PNG\"";
    let (_, _, one) = get("/collections/obs/locations/here?f=html", None).await;
    assert!(one.contains(png), "{one}");
    let (status, _, list) = get("/collections/obs/locations/here,a%20b?f=html", None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert!(list.contains("data-edr-query"), "{list}");
    assert!(!list.contains(png), "{list}");
    for uri in [
        "/collections/obs/locations/position?f=html",
        "/collections/c/items/area?f=html",
    ] {
        let (status, _, body) = get(uri, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}: {body}");
        assert!(!body.contains("data-edr-query"), "{uri}");
    }
}
