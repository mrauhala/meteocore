//! Station-grid corrections (#1206) over synthetic collections: a station
//! source with its inventory, a terrain and a land/sea mask, each an
//! in-memory engine the way the server hands them to the engine.
//!
//! The valley's values are pinned to the independent stdlib script
//! `tests/reference/lapse_rate_reference.py`.

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, TimeZone, Utc};
use ds_core::config::{
    StationExclusionRule, StationExclusions, StationGridConfig, StationGridGeometry,
    StationGridParameter, StationGridParameterOptions, StationPropertyValue,
};
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::feature::{
    parse_area_coords, Feature, FeaturePage, FeatureQuery, Geometry, PropertyValue,
};
use ds_core::feature_engine::FeatureEngine;
use ds_core::map_engine::{
    MapEngine, OutputCrs, ParameterInfo, RasterInfo, RasterTile, RasterValues,
};
use ds_core::model::{
    CoverageResponse, DomainDescription, LocationSnapshot, NdArray, ParameterDescription,
    QueryResult,
};
use engine_station_grid::{Auxiliary, StationGridEngine};

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 3, 1, 12, 0, 0).unwrap()
}

// ---------------------------------------------------------------------------
// A station source with an inventory
// ---------------------------------------------------------------------------

struct Station {
    lon: f64,
    lat: f64,
    /// Values at `t0()`, per parameter.
    values: Vec<(&'static str, f64)>,
    properties: HashMap<String, PropertyValue>,
}

fn station(lon: f64, lat: f64, values: &[(&'static str, f64)]) -> Station {
    Station {
        lon,
        lat,
        values: values.to_vec(),
        properties: HashMap::new(),
    }
}

impl Station {
    fn with(mut self, property: &str, value: PropertyValue) -> Self {
        self.properties.insert(property.to_string(), value);
        self
    }
}

/// What a BUFR station collection is to the engine: an `EdrEngine` whose
/// area query answers each station's series, and a `FeatureEngine` whose
/// features carry the station's properties.
struct Source {
    stations: Mutex<Vec<Station>>,
    version: AtomicU64,
    /// Every station reports its values at each of these, ascending.
    times: Vec<DateTime<Utc>>,
}

impl Source {
    fn new(stations: Vec<Station>) -> Arc<Self> {
        Self::at(stations, vec![t0()])
    }

    fn at(stations: Vec<Station>, times: Vec<DateTime<Utc>>) -> Arc<Self> {
        Arc::new(Self {
            stations: Mutex::new(stations),
            version: AtomicU64::new(1),
            times,
        })
    }

    fn set_property(&self, index: usize, property: &str, value: PropertyValue) {
        self.stations.lock().unwrap()[index]
            .properties
            .insert(property.to_string(), value);
        self.version.fetch_add(1, Ordering::SeqCst);
    }
}

fn describe(label: &str, unit: &str, standard_name: &str) -> ParameterDescription {
    ParameterDescription {
        label: label.into(),
        unit: unit.into(),
        observed_property: standard_name.into(),
        standard_name: Some(standard_name.into()),
    }
}

impl EdrEngine for Source {
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

    fn get_parameters(&self) -> Vec<String> {
        ["t2m", "td", "rh"].map(String::from).to_vec()
    }

    /// BUFR's built-ins: CF standard names stated by the source.
    fn get_parameter_descriptions(&self) -> HashMap<String, ParameterDescription> {
        HashMap::from([
            (
                "t2m".to_string(),
                describe("Air temperature", "°C", "air_temperature"),
            ),
            (
                "td".to_string(),
                describe("Dew point", "°C", "dew_point_temperature"),
            ),
            (
                "rh".to_string(),
                describe("Relative humidity", "%", "relative_humidity"),
            ),
        ])
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        Some((*self.times.first()?, *self.times.last()?))
    }

    /// As CSV and BUFR list theirs.
    fn observation_instants(&self) -> Option<Arc<[DateTime<Utc>]>> {
        Some(self.times.clone().into())
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        None
    }

    fn supported_query_types(&self) -> Vec<String> {
        vec!["area".into()]
    }

    fn serves_station_series(&self) -> bool {
        true
    }

    fn query_area(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        let polygon = parse_area_coords(coords)?;
        let (start, end) = datetime.expect("a window");
        let wanted = parameters.expect("named parameters");
        let times: Vec<DateTime<Utc>> = self
            .times
            .iter()
            .copied()
            .filter(|t| start <= *t && *t <= end)
            .collect();
        let stations = self.stations.lock().unwrap();
        Ok(CoverageResponse::Collection(
            stations
                .iter()
                .filter(|s| !times.is_empty() && polygon.contains(s.lon, s.lat))
                .map(|s| QueryResult {
                    domain: DomainDescription::PointSeries {
                        x: s.lon,
                        y: s.lat,
                        t: times.clone(),
                        z: None,
                    },
                    parameters: HashMap::new(),
                    ranges: s
                        .values
                        .iter()
                        .filter(|(p, _)| wanted.iter().any(|w| w == p))
                        .map(|(p, v)| {
                            (
                                p.to_string(),
                                NdArray {
                                    shape: vec![times.len()],
                                    axis_names: vec!["t".into()],
                                    values: vec![Some(*v); times.len()],
                                },
                            )
                        })
                        .collect(),
                })
                .collect(),
        ))
    }
}

impl FeatureEngine for Source {
    fn get_features(&self, query: &FeatureQuery) -> Result<FeaturePage, DataServerError> {
        let stations = self.stations.lock().unwrap();
        let features: Vec<Feature> = stations
            .iter()
            .enumerate()
            .skip(query.offset)
            .take(query.limit)
            .map(|(i, s)| Feature {
                id: format!("s{i}"),
                geometry: Arc::new(Geometry::Point { x: s.lon, y: s.lat }),
                properties: Arc::new(s.properties.clone()),
                related: Vec::new(),
            })
            .collect();
        let end = query.offset + features.len();
        Ok(FeaturePage {
            number_matched: stations.len(),
            number_returned: features.len(),
            next_offset: (end < stations.len()).then_some(end),
            features,
        })
    }

    fn get_feature(&self, id: &str) -> Result<Feature, DataServerError> {
        Err(DataServerError::FeatureNotFound(id.into()))
    }

    fn data_version(&self) -> u64 {
        self.version.load(Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------------------
// A raster collection: terrain or mask as a formula
// ---------------------------------------------------------------------------

type Field = Box<dyn Fn(f64, f64) -> Option<f64> + Send + Sync>;

/// One request a raster collection answered: (bbox, width, height,
/// parameter).
type Request = ([f64; 4], u32, u32, Option<String>);

struct Raster {
    /// (parameter, unit); one entry is a single-parameter engine.
    parameters: Vec<(&'static str, &'static str)>,
    field: Field,
    /// Off: a remote collection still polling — no metadata, no data.
    ready: AtomicBool,
    /// Every request: (bbox, width, height, parameter).
    requests: Mutex<Vec<Request>>,
}

impl Raster {
    fn new(
        parameters: &[(&'static str, &'static str)],
        field: impl Fn(f64, f64) -> Option<f64> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            parameters: parameters.to_vec(),
            field: Box::new(field),
            ready: AtomicBool::new(true),
            requests: Mutex::new(Vec::new()),
        })
    }
}

impl MapEngine for Raster {
    /// Pixel `(i, j)` at the box's `((i + ½)/width, (j + ½)/height)`, as
    /// every engine samples through `OutputCrs::project_node`.
    fn get_raster_tile(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        _: Option<DateTime<Utc>>,
        crs: &OutputCrs,
        parameter: Option<&str>,
        _: Option<f64>,
        _: Option<DateTime<Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        if !self.ready.load(Ordering::SeqCst) {
            return Err(DataServerError::LocationNotFound(
                "No data available for this collection".into(),
            ));
        }
        self.requests
            .lock()
            .unwrap()
            .push((bbox, width, height, parameter.map(String::from)));
        let mut values = Vec::with_capacity(width as usize * height as usize);
        for j in 0..height {
            for i in 0..width {
                let (lon, lat) = crs.project_node(
                    bbox,
                    (i as f64 + 0.5) / width as f64,
                    (j as f64 + 0.5) / height as f64,
                );
                values.push((self.field)(lon, lat));
            }
        }
        Ok(RasterTile {
            width,
            height,
            values: RasterValues::F64(values),
        })
    }

    fn raster_info(&self) -> RasterInfo {
        let ready = self.ready.load(Ordering::SeqCst);
        let (parameter, unit) = match (ready, self.parameters.as_slice()) {
            (true, [(name, unit)]) => (name.to_string(), unit.to_string()),
            _ => (String::new(), String::new()),
        };
        RasterInfo {
            native_crs: "EPSG:4326".into(),
            spatial_extent: None,
            times: if ready { vec![t0()] } else { Vec::new() },
            parameter,
            unit,
            parameters: if ready && self.parameters.len() > 1 {
                self.parameters
                    .iter()
                    .map(|(name, unit)| ParameterInfo {
                        name: name.to_string(),
                        title: name.to_string(),
                        unit: unit.to_string(),
                    })
                    .collect()
            } else {
                Vec::new()
            },
            vertical: None,
            grid_size: None,
            layer_subtitle: None,
            reference_times: Vec::new(),
            native_grid: None,
        }
    }
}

// ---------------------------------------------------------------------------
// The valley
// ---------------------------------------------------------------------------

/// Terrain: ridges at 24.0 and 25.0 E, the floor at 24.5 E.
fn valley_terrain(lon: f64) -> f64 {
    50.0 + 950.0 * (lon - 24.5).abs() / 0.5
}

/// `(lon, lat, corrected, uncorrected)` printed by
/// `python3 crates/engine-station-grid/tests/reference/lapse_rate_reference.py`.
const VALLEY: [(f64, f64, f64, f64); 6] = [
    (24.05, 60.25, -4.032499999999991, -4.0),
    (24.45, 60.15, 1.5, 1.5),
    (24.55, 60.25, 1.4001938817011705, 0.5735297035551843),
    (24.95, 60.35, -3.6338597153994976, -0.29954166193641046),
    (24.25, 60.05, -1.1391392851499629, -0.0700693136284065),
    (24.75, 60.15, -1.0561649556775965, 0.7001565295749843),
];

const VALLEY_BBOX: [f64; 4] = [24.0, 60.0, 25.0, 60.4];

fn config(parameters: Vec<StationGridParameter>) -> StationGridConfig {
    StationGridConfig {
        source: "obs".into(),
        parameters,
        elevation_source: None,
        elevation_parameter: None,
        land_mask_source: None,
        land_mask_parameter: None,
        exclude_where: StationExclusions::default(),
        grid: StationGridGeometry {
            crs: "EPSG:4326".into(),
            bbox: VALLEY_BBOX,
            resolution: 0.1,
        },
        method: "idw".into(),
        power: None,
        correlation_length_km: None,
        max_neighbours: 8,
        search_radius_km: 100.0,
        max_age: "PT1H".into(),
        cadence: Some("PT1H".into()),
        poll_interval_secs: 60,
        precompute: 1,
    }
}

fn options(name: &str, lapse_rate: Option<f64>, land_mask: Option<bool>) -> StationGridParameter {
    StationGridParameter::Options(StationGridParameterOptions {
        name: name.into(),
        lapse_rate,
        land_mask,
    })
}

/// The valley's stations: the ridge one states its height, the valley one
/// does not.
fn valley_stations() -> Vec<Station> {
    vec![
        station(24.05, 60.25, &[("t2m", -4.0), ("td", -9.0), ("rh", 80.0)])
            .with("elevation", PropertyValue::Float(900.0)),
        station(24.45, 60.15, &[("t2m", 1.5), ("td", -2.0), ("rh", 60.0)]),
    ]
}

fn terrain_engine() -> Arc<Raster> {
    Raster::new(&[("height", "m")], |lon, _| Some(valley_terrain(lon)))
}

fn build(
    id: &str,
    source: &Arc<Source>,
    config: &StationGridConfig,
    elevation: Option<Arc<Raster>>,
    land_mask: Option<Arc<Raster>>,
) -> Result<StationGridEngine, String> {
    StationGridEngine::new_with(
        id,
        "obs",
        source.clone(),
        config,
        Auxiliary {
            elevation: elevation.map(|e| e as Arc<dyn MapEngine>),
            land_mask: land_mask.map(|e| e as Arc<dyn MapEngine>),
            stations: Some(source.clone() as Arc<dyn FeatureEngine>),
        },
    )
    .map_err(|e| e.to_string())
}

fn single(response: CoverageResponse) -> QueryResult {
    match response {
        CoverageResponse::Single(result) => result,
        CoverageResponse::Collection(_) => panic!("expected one coverage"),
    }
}

fn at(engine: &StationGridEngine, parameter: &str, lon: f64, lat: f64) -> Option<f64> {
    let names = [parameter.to_string()];
    let result = single(
        engine
            .query_position(
                &format!("POINT({lon} {lat})"),
                Some((t0(), t0())),
                Some(&names),
                None,
                None,
            )
            .unwrap(),
    );
    result.ranges[parameter].values[0]
}

fn tile(engine: &StationGridEngine, parameter: &str, bbox: [f64; 4], w: u32, h: u32) -> Vec<f32> {
    let tile = engine
        .get_raster_tile(
            bbox,
            w,
            h,
            Some(t0()),
            &OutputCrs::Wgs84,
            Some(parameter),
            None,
            None,
        )
        .unwrap();
    match tile.values {
        RasterValues::F32 { data, .. } => data,
        _ => panic!("station grids render as F32"),
    }
}

/// The ridge station on its stated height and the valley station on the
/// terrain under it give an analysis that follows the terrain: warmer on
/// the valley floor, colder on the ridges than the uncorrected field.
#[test]
fn valley_analysis_follows_the_terrain() {
    let source = Source::new(valley_stations());
    let mut corrected = config(vec!["t2m".into()]);
    corrected.elevation_source = Some("dem".into());
    let terrain = terrain_engine();
    let engine = build("valley", &source, &corrected, Some(terrain.clone()), None).unwrap();
    // Nothing until the poll has sampled the terrain.
    assert!(!engine.has_data());
    assert!(engine.waiting_for().is_some());
    engine.poll_once();
    assert!(engine.has_data(), "unexpected value");
    assert_eq!(engine.station_counts(), vec![("t2m".to_string(), Some(2))]);
    assert!(
        engine.provenance().contains("lapse rate (t2m -6.5 K/km)"),
        "unexpected value"
    );

    let plain = build(
        "valley-plain",
        &source,
        &config(vec!["t2m".into()]),
        None,
        None,
    )
    .unwrap();
    for (lon, lat, want, want_plain) in VALLEY {
        let got = at(&engine, "t2m", lon, lat).unwrap();
        assert!(
            (got - want).abs() < 1e-4,
            "the corrected analysis is the reference's"
        );
        let got_plain = at(&plain, "t2m", lon, lat).unwrap();
        assert!(
            (got_plain - want_plain).abs() < 1e-4,
            "the uncorrected analysis is the reference's"
        );
    }
    // The east ridge is colder, the valley floor warmer, than without.
    assert!(
        at(&engine, "t2m", 24.95, 60.35).unwrap() < at(&plain, "t2m", 24.95, 60.35).unwrap() - 3.0
    );
    assert!(
        at(&engine, "t2m", 24.55, 60.25).unwrap() > at(&plain, "t2m", 24.55, 60.25).unwrap() + 0.5
    );

    // The map draws the values EDR answers: one pixel per cell.
    let pixels = tile(&engine, "t2m", VALLEY_BBOX, 10, 4);
    for (lon, lat, want, _) in VALLEY {
        let (col, row) = (((lon - 24.0) / 0.1) as usize, ((60.4 - lat) / 0.1) as usize);
        assert!((f64::from(pixels[row * 10 + col]) - want).abs() < 1e-4);
    }

    // The terrain was asked once, for the grid widened by whole cells to the
    // search radius: cell centres on the analysis grid's.
    engine.poll_once();
    let requests = terrain.requests.lock().unwrap();
    assert_eq!(requests.len(), 1, "sampled once per engine");
    let ([w, s, e, n], width, height, parameter) = requests[0].clone();
    assert_eq!((width, height), (48, 22));
    assert!(
        (w - 22.1).abs() < 1e-9 && (e - 26.9).abs() < 1e-9,
        "unexpected value"
    );
    assert!(
        (s - 59.1).abs() < 1e-9 && (n - 61.3).abs() < 1e-9,
        "unexpected value"
    );
    assert_eq!(parameter, None, "a single-parameter collection");
}

/// The source's stated height wins over the terrain, and a difference of
/// more than 100 m is logged once. A station with neither is left out of
/// the corrected parameter only; a cell over terrain nodata is nodata in
/// the corrected parameter only.
#[test]
fn stated_heights_terrain_fallback_and_nodata() {
    let mut stations = valley_stations();
    stations[0]
        .properties
        .insert("elevation".into(), PropertyValue::Integer(2000));
    // Over the terrain's nodata strip east of 24.9 E, with no stated height.
    stations.push(station(24.95, 60.05, &[("t2m", 0.0), ("td", -5.0)]));
    let source = Source::new(stations);
    let terrain = Raster::new(&[("height", "km")], |lon, _| {
        (lon < 24.9).then(|| valley_terrain(lon) / 1000.0)
    });
    let mut cfg = config(vec!["t2m".into(), "td".into()]);
    cfg.elevation_source = Some("dem".into());
    let engine = build("heights", &source, &cfg, Some(terrain), None).unwrap();

    let warnings = warnings_of("heights", || engine.poll_once());
    let disagreements: Vec<&String> = warnings
        .iter()
        .filter(|w| w.contains("states a height of 2000 m"))
        .collect();
    assert_eq!(disagreements.len(), 1, "unexpected value");
    assert!(disagreements[0].contains("1095 m from the terrain's 905 m"));
    assert_eq!(
        warnings
            .iter()
            .filter(|w| w.contains("(24.95, 60.05) has no stated height"))
            .count(),
        1,
        "unexpected value"
    );

    // The ridge station on its stated 2000 m: its own cell is its value
    // reduced from 2000 m and brought back to the cell's 905 m.
    let ridge = at(&engine, "t2m", 24.05, 60.25).unwrap();
    assert!(
        (ridge - (-4.0 + 0.0065 * (2000.0 - 905.0))).abs() < 1e-4,
        "unexpected value"
    );
    // The valley station on the terrain under it.
    assert!((at(&engine, "t2m", 24.45, 60.15).unwrap() - 1.5).abs() < 1e-5);
    // No height: left out of t2m (and its cell, over nodata, is nodata);
    // dew point is not corrected and keeps every station and cell.
    assert_eq!(
        engine.station_counts(),
        vec![("t2m".to_string(), Some(2)), ("td".to_string(), Some(3))]
    );
    assert_eq!(at(&engine, "t2m", 24.95, 60.05), None);
    assert_eq!(at(&engine, "td", 24.95, 60.05), Some(-5.0));

    // A new inventory version re-reads the stations but logs nothing again.
    source.set_property(1, "name", PropertyValue::String("Valley".into()));
    let again = warnings_of("heights", || engine.poll_once());
    assert!(
        !again.iter().any(|w| w.contains("states a height")),
        "unexpected value"
    );
}

/// Cells outside the mask are nodata in maps and in EDR position and area,
/// for every parameter that does not opt out. A percentage mask is inside
/// from 50 %, and its nodata is outside.
#[test]
fn land_mask_is_nodata_in_maps_and_edr() {
    let source = Source::new(valley_stations());
    // Land south of 60.2 N; 50 % exactly on the 60.15 row; nodata east of
    // 24.8 E.
    let mask = Raster::new(&[("land", "%")], |lon, lat| {
        if lon > 24.8 {
            None
        } else if (lat - 60.15).abs() < 0.01 {
            Some(50.0)
        } else if lat < 60.2 {
            Some(100.0)
        } else {
            Some(49.9)
        }
    });
    let mut cfg = config(vec!["t2m".into(), options("rh", None, Some(false))]);
    cfg.land_mask_source = Some("land".into());
    let engine = build("masked", &source, &cfg, None, Some(mask)).unwrap();
    engine.poll_once();
    assert!(engine.has_data());

    let inside = [(24.05, 60.15), (24.45, 60.05), (24.75, 60.15)];
    let outside = [
        (24.05, 60.25),
        (24.45, 60.35),
        (24.85, 60.05),
        (24.95, 60.15),
    ];
    for (lon, lat) in inside {
        assert!(at(&engine, "t2m", lon, lat).is_some(), "unexpected value");
    }
    for (lon, lat) in outside {
        assert_eq!(at(&engine, "t2m", lon, lat), None, "unexpected value");
        // `rh` opts out: today's estimate from every station.
        assert!(at(&engine, "rh", lon, lat).is_some(), "unexpected value");
    }
    // The map: one pixel per cell, NaN exactly where EDR answers null.
    let pixels = tile(&engine, "t2m", VALLEY_BBOX, 10, 4);
    for row in 0..4 {
        for col in 0..10 {
            let (lon, lat) = (24.05 + 0.1 * col as f64, 60.35 - 0.1 * row as f64);
            assert_eq!(
                pixels[row * 10 + col].is_nan(),
                at(&engine, "t2m", lon, lat).is_none(),
                "unexpected value"
            );
        }
    }
    let land_cells = pixels.iter().filter(|v| v.is_finite()).count();
    assert_eq!(land_cells, 2 * 8, "two rows of land west of 24.8 E");
    // EDR area: the same cells null.
    let area = single(
        engine
            .query_area(
                "24.0,60.0,25.0,60.4",
                Some((t0(), t0())),
                Some(&["t2m".to_string()]),
                None,
                None,
            )
            .unwrap(),
    );
    let values = &area.ranges["t2m"].values;
    assert_eq!(values.iter().filter(|v| v.is_some()).count(), land_cells);
    assert!(engine.provenance().contains("land mask of 'land'"));
}

/// Per parameter: the default lapse rate only where the source identifies
/// air temperature (not dew point), `lapse_rate = 0` turns it off, an
/// explicit rate turns it on.
#[test]
fn per_parameter_lapse_rates() {
    let source = Source::new(valley_stations());
    let plain = build(
        "rates-plain",
        &source,
        &config(vec!["t2m".into(), "td".into()]),
        None,
        None,
    )
    .unwrap();
    let mut cfg = config(vec![options("t2m", Some(0.0), None), "td".into()]);
    cfg.elevation_source = Some("dem".into());
    // Nothing is corrected: t2m opts out and dew point has no default.
    let err = build("rates-none", &source, &cfg, Some(terrain_engine()), None)
        .err()
        .expect("must fail");
    assert!(
        err.contains("no parameter is corrected"),
        "unexpected value"
    );

    cfg.parameters = vec![
        options("t2m", Some(0.0), None),
        options("td", Some(-2.0), None),
    ];
    let engine = build("rates", &source, &cfg, Some(terrain_engine()), None).unwrap();
    engine.poll_once();
    let (lon, lat) = (24.95, 60.35);
    assert_eq!(at(&engine, "t2m", lon, lat), at(&plain, "t2m", lon, lat));
    let td = at(&engine, "td", lon, lat).unwrap();
    let td_plain = at(&plain, "td", lon, lat).unwrap();
    assert!(
        td < td_plain - 0.5,
        "the corrected dew point is colder on the ridge"
    );

    cfg.parameters = vec!["t2m".into(), "td".into()];
    let engine = build("rates-default", &source, &cfg, Some(terrain_engine()), None).unwrap();
    engine.poll_once();
    assert_eq!(at(&engine, "td", lon, lat), at(&plain, "td", lon, lat));
    assert!(at(&engine, "t2m", lon, lat).unwrap() < at(&plain, "t2m", lon, lat).unwrap() - 3.0);
}

/// `exclude_where` leaves a station out; when the station's properties
/// change, the poll finds it: the fingerprint and the content version
/// move and the analysis is computed again.
#[test]
fn exclusions_change_the_analysis_and_the_version() {
    let mut stations = valley_stations();
    stations.push(
        station(24.75, 60.05, &[("t2m", 8.0)])
            .with("exclude_from_gridding", PropertyValue::Bool(true)),
    );
    let source = Source::new(stations);
    let mut cfg = config(vec!["t2m".into()]);
    cfg.exclude_where = StationExclusions(vec![StationExclusionRule::from([(
        "exclude_from_gridding".to_string(),
        vec![StationPropertyValue::Bool(true)],
    )])]);
    let engine = build("excluded", &source, &cfg, None, None).unwrap();
    assert!(!engine.has_data(), "the inventory is read by the poll");
    engine.poll_once();
    assert_eq!(engine.station_counts(), vec![("t2m".to_string(), Some(2))]);
    let excluded = at(&engine, "t2m", 24.75, 60.05).unwrap();
    assert!(excluded < 2.0, "the station's 8 °C is left out");
    let version = engine.content_version();
    assert!(engine.provenance().contains("exclude_where"));

    source.set_property(2, "exclude_from_gridding", PropertyValue::Bool(false));
    engine.poll_once();
    assert!(engine.content_version() > version, "a revision");
    assert_eq!(engine.station_counts(), vec![("t2m".to_string(), Some(3))]);
    assert_eq!(at(&engine, "t2m", 24.75, 60.05), Some(8.0));

    // Without the source's inventory there is nothing to read them from.
    let err = StationGridEngine::new_with(
        "no-inventory",
        "obs",
        source.clone(),
        &cfg,
        Auxiliary::default(),
    )
    .err()
    .unwrap()
    .to_string();
    assert!(err.contains("no station inventory"), "unexpected value");
}

/// A terrain still polling (remote, no data yet): no analysis time, an
/// empty tile, `resolve_time` None and an EDR 404, never an uncorrected
/// grid; the poll retries until it is sampled. Without a `cadence` the axis
/// is the source's observation instants, behind the same gate.
#[test]
fn nothing_is_served_before_the_terrain_is_sampled() {
    let source = Source::new(valley_stations());
    let terrain = terrain_engine();
    terrain.ready.store(false, Ordering::SeqCst);
    let mut cfg = config(vec!["t2m".into()]);
    cfg.cadence = None;
    cfg.elevation_source = Some("dem".into());
    let engine = build("pending", &source, &cfg, Some(terrain.clone()), None).unwrap();
    engine.poll_once();
    assert!(!engine.has_data());
    assert!(
        engine
            .waiting_for()
            .unwrap()
            .contains("'dem' has no data yet"),
        "unexpected value"
    );
    assert_eq!(engine.resolve_time(Some(t0()), None), None);
    assert!(tile(&engine, "t2m", VALLEY_BBOX, 10, 4)
        .iter()
        .all(|v| v.is_nan()));
    assert!(matches!(
        engine.query_position("POINT(24.5 60.2)", None, None, None, None),
        Err(DataServerError::LocationNotFound(_))
    ));
    assert_eq!(engine.metrics().0, 0, "no analysis computed");

    terrain.ready.store(true, Ordering::SeqCst);
    engine.poll_once();
    assert!(engine.has_data());
    assert_eq!(engine.waiting_for(), None);
    assert_eq!(engine.resolve_time(None, None), Some(t0()));
    let (lon, lat, want, _) = VALLEY[3];
    assert!((at(&engine, "t2m", lon, lat).unwrap() - want).abs() < 1e-4);
}

/// A raster once sampled is kept: while the mask is still missing, each
/// retry reads the mask alone, never the terrain again.
#[test]
fn a_sampled_terrain_is_not_read_again_while_the_mask_is_missing() {
    let source = Source::new(valley_stations());
    let terrain = terrain_engine();
    let mask = Raster::new(&[("land", "1")], |_, _| Some(1.0));
    mask.ready.store(false, Ordering::SeqCst);
    let mut cfg = config(vec!["t2m".into()]);
    cfg.elevation_source = Some("dem".into());
    cfg.land_mask_source = Some("land".into());
    let engine = build(
        "retries",
        &source,
        &cfg,
        Some(terrain.clone()),
        Some(mask.clone()),
    )
    .unwrap();
    for _ in 0..3 {
        engine.poll_once();
    }
    assert!(!engine.has_data());
    assert!(engine
        .waiting_for()
        .unwrap_or_default()
        .contains("land_mask_source 'land' has no data yet"));
    assert_eq!(
        terrain.requests.lock().unwrap().len(),
        1,
        "terrain read once"
    );

    mask.ready.store(true, Ordering::SeqCst);
    engine.poll_once();
    assert!(engine.has_data());
    assert_eq!(
        terrain.requests.lock().unwrap().len(),
        1,
        "terrain read once"
    );
    assert_eq!(mask.requests.lock().unwrap().len(), 1, "mask read once");
}

/// The canonical seam box `170,10,-170,20`: the terrain and the mask are
/// asked for the grid unwrapped past 180°, cells either side of it take
/// their own heights, and stations either side get theirs from the
/// terrain across the seam.
#[test]
fn terrain_and_mask_across_the_antimeridian() {
    // Continuous across 180°: 1000 m there, 50 m per degree eastwards.
    let height = |lon: f64| 1000.0 + 50.0 * (lon.rem_euclid(360.0) - 180.0);
    let terrain = Raster::new(&[("height", "m")], move |lon, _| Some(height(lon)));
    // Land within 5° of 180°.
    let mask = Raster::new(&[("land", "1")], |lon, _| {
        Some(if (lon.rem_euclid(360.0) - 180.0).abs() < 5.0 {
            1.0
        } else {
            0.0
        })
    });
    // Equal sea-level values, so the analysis is the terrain's lapse.
    let lapse = 0.0065;
    let sea_level = 16.0;
    let source = Source::new(vec![
        station(
            179.25,
            15.25,
            &[("t2m", sea_level - lapse * height(179.25))],
        ),
        station(
            -179.25,
            15.25,
            &[("t2m", sea_level - lapse * height(180.75))],
        ),
    ]);
    let mut cfg = config(vec!["t2m".into()]);
    cfg.grid.bbox = [170.0, 10.0, -170.0, 20.0];
    cfg.grid.resolution = 0.5;
    cfg.search_radius_km = 400.0;
    cfg.elevation_source = Some("dem".into());
    cfg.land_mask_source = Some("land".into());
    let engine = build(
        "seam",
        &source,
        &cfg,
        Some(terrain.clone()),
        Some(mask.clone()),
    )
    .unwrap();
    engine.poll_once();
    assert!(engine.has_data(), "unexpected value");

    let (mask_bbox, w, h, _) = mask.requests.lock().unwrap()[0].clone();
    assert_eq!(mask_bbox, [170.0, 10.0, 190.0, 20.0], "unwrapped past 180°");
    assert_eq!((w, h), (40, 20));
    let (dem_bbox, ..) = terrain.requests.lock().unwrap()[0].clone();
    assert!(
        dem_bbox[0] < 170.0 && dem_bbox[2] > 190.0,
        "unexpected value"
    );

    let pixels = tile(&engine, "t2m", [170.0, 10.0, -170.0, 20.0], 40, 20);
    // Row of 15.25 N; columns of 179.75 and -179.75.
    let (west, east) = (pixels[9 * 40 + 19], pixels[9 * 40 + 20]);
    let want_west = sea_level - lapse * height(179.75);
    let want_east = sea_level - lapse * height(180.25);
    assert!(
        (f64::from(west) - want_west).abs() < 1e-4,
        "west of 180° follows the terrain"
    );
    assert!(
        (f64::from(east) - want_east).abs() < 1e-4,
        "east of 180° follows the terrain"
    );
    assert_eq!(
        at(&engine, "t2m", -179.75, 15.25).map(|v| v as f32),
        Some(east)
    );
    // 171.25 E is outside the mask.
    assert!(pixels[9 * 40 + 2].is_nan());
    assert_eq!(at(&engine, "t2m", 171.25, 15.25), None);
}

/// `(lon, lat, value, estimate, residual)` of each station of the
/// four-station valley, printed by
/// `python3 crates/engine-station-grid/tests/reference/lapse_rate_reference.py`
/// (its second block).
const VALLEY_RESIDUALS: [(f64, f64, f64, f64, f64); 4] = [
    (24.05, 60.25, -4.0, -3.2092639284481863, 0.7907360715518137),
    (24.45, 60.15, 1.5, 1.7081178561559645, 0.20811785615596445),
    (24.95, 60.35, -3.0, -3.2880244289811773, -0.2880244289811773),
    (24.75, 60.05, 0.5, -0.14191717804495285, -0.6419171780449529),
];

/// Leave-one-out residuals of a corrected parameter: each estimate from the
/// other stations' sea-level values, put back at the station's own height,
/// minus its observed value. An excluded station and one without a height
/// have none; the mask removes no residual.
#[test]
fn residuals_with_corrections() {
    let mut stations = valley_stations();
    stations.extend([
        station(24.95, 60.35, &[("t2m", -3.0)]),
        station(24.75, 60.05, &[("t2m", 0.5)]).with("elevation", PropertyValue::Float(400.0)),
        // Excluded.
        station(24.25, 60.35, &[("t2m", 9.0)])
            .with("exclude_from_gridding", PropertyValue::Bool(true)),
        // Over the terrain's nodata east of 25 E, with no stated height.
        station(25.05, 60.2, &[("t2m", 7.0)]),
    ]);
    let source = Source::new(stations);
    let terrain = Raster::new(&[("height", "m")], |lon, _| {
        (lon <= 25.0).then(|| valley_terrain(lon))
    });
    // The 60.35 N row, the east-ridge station's, is outside the mask.
    let mask = Raster::new(&[("land", "1")], |_, lat| {
        Some(if lat > 60.3 { 0.0 } else { 1.0 })
    });
    let mut cfg = config(vec!["t2m".into()]);
    cfg.elevation_source = Some("dem".into());
    cfg.land_mask_source = Some("land".into());
    cfg.exclude_where = StationExclusions(vec![StationExclusionRule::from([(
        "exclude_from_gridding".to_string(),
        vec![StationPropertyValue::Bool(true)],
    )])]);
    let engine = build("residuals", &source, &cfg, Some(terrain), Some(mask)).unwrap();
    engine.poll_once();
    assert!(engine.has_data(), "unexpected value");

    let page = engine
        .get_features(&FeatureQuery {
            limit: 100,
            property_filters: vec![("parameter".into(), "t2m".into())],
            ..FeatureQuery::default()
        })
        .unwrap();
    let float = |f: &Feature, name: &str| match f.properties.get(name) {
        Some(PropertyValue::Float(v)) => *v,
        _ => panic!("a float property"),
    };
    let found: Vec<(f64, f64, f64, f64, f64)> = page
        .features
        .iter()
        .map(|f| {
            let Geometry::Point { x, y } = *f.geometry else {
                panic!("a Point");
            };
            (
                x,
                y,
                float(f, "value"),
                float(f, "estimate"),
                float(f, "residual"),
            )
        })
        .collect();
    assert_eq!(found.len(), 4, "neither the excluded nor the height-less");
    for (lon, lat, value, estimate, residual) in VALLEY_RESIDUALS {
        let got = found
            .iter()
            .find(|f| (f.0 - lon).abs() < 1e-9 && (f.1 - lat).abs() < 1e-9)
            .unwrap_or_else(|| panic!("a residual at every reference station"));
        assert_eq!(got.2, value, "the observed value");
        assert!(
            (got.3 - estimate).abs() < 1e-9,
            "the estimate is the reference's"
        );
        assert!(
            (got.4 - residual).abs() < 1e-9,
            "the residual is the reference's"
        );
    }
    // The east-ridge station's cell is masked; its residual is kept.
    assert_eq!(at(&engine, "t2m", 24.95, 60.35), None);
    let rms = (VALLEY_RESIDUALS.iter().map(|r| r.4 * r.4).sum::<f64>() / 4.0).sqrt();
    let summary = engine.residual_summaries()[0].1.unwrap();
    assert_eq!(summary.count, 4);
    assert!((summary.rmse.unwrap() - rms).abs() < 1e-9);
}

/// An older analysis time the poll re-checks (not one of the latest
/// `precompute`) reads its slice through the corrections too: a station
/// newly excluded revises its cached grid.
#[test]
fn rechecks_of_older_times_see_exclusions() {
    let times = vec![
        t0() - chrono::Duration::hours(2),
        t0() - chrono::Duration::hours(1),
        t0(),
    ];
    let mut stations = valley_stations();
    stations.push(station(24.75, 60.05, &[("t2m", 8.0)]));
    let source = Source::at(stations, times.clone());
    let mut cfg = config(vec!["t2m".into()]);
    cfg.exclude_where = StationExclusions(vec![StationExclusionRule::from([(
        "exclude_from_gridding".to_string(),
        vec![StationPropertyValue::Bool(true)],
    )])]);
    let engine = build("rechecked", &source, &cfg, None, None).unwrap();
    engine.poll_once();
    let older = times[1];
    let names = ["t2m".to_string()];
    let at_older = || {
        single(
            engine
                .query_position(
                    "POINT(24.75 60.05)",
                    Some((older, older)),
                    Some(&names),
                    None,
                    None,
                )
                .unwrap(),
        )
        .ranges["t2m"]
            .values[0]
    };
    // A request computes and caches the older time's grid.
    assert_eq!(at_older(), Some(8.0));
    let version = engine.content_version();

    source.set_property(2, "exclude_from_gridding", PropertyValue::Bool(true));
    engine.poll_once();
    assert!(engine.content_version() > version);
    let revised = at_older().unwrap();
    assert!(revised < 2.0, "the excluded 8 °C is gone");
}

/// Every new key's errors.
#[test]
fn rejects_bad_corrections() {
    let source = Source::new(valley_stations());
    let fail = |cfg: &StationGridConfig, dem: Option<Arc<Raster>>, mask: Option<Arc<Raster>>| {
        build("bad", &source, cfg, dem, mask)
            .err()
            .expect("must fail")
    };
    let with_dem = |parameters: Vec<StationGridParameter>| {
        let mut c = config(parameters);
        c.elevation_source = Some("dem".into());
        c
    };

    // lapse_rate / land_mask without their source.
    let c = config(vec![options("t2m", Some(-6.5), None)]);
    assert!(fail(&c, None, None).contains("sets lapse_rate, which needs elevation_source"));
    let c = config(vec![options("t2m", None, Some(true))]);
    assert!(fail(&c, None, None).contains("sets land_mask, which needs land_mask_source"));
    // A corrected parameter must be a temperature.
    let c = with_dem(vec![options("rh", Some(-6.5), None)]);
    let err = fail(&c, Some(terrain_engine()), None);
    assert!(
        err.contains("unit '%' is not a temperature"),
        "unexpected value"
    );
    // The rate's range.
    for rate in [-500.0, f64::NAN] {
        let c = with_dem(vec![options("t2m", Some(rate), None)]);
        let err = fail(&c, Some(terrain_engine()), None);
        assert!(
            err.contains("lapse_rate of 't2m' must be a number of K/km"),
            "unexpected value"
        );
    }
    // No corrected parameter: dew point has no default.
    let err = fail(&with_dem(vec!["td".into()]), Some(terrain_engine()), None);
    assert!(err.contains("elevation_source 'dem' is set but no parameter is corrected"));
    // Named but not wired (built without its engine).
    let err = fail(&with_dem(vec!["t2m".into()]), None, None);
    assert!(
        err.contains("elevation_source 'dem' is not wired"),
        "unexpected value"
    );
    // Heights in a unit that is not a length.
    let bad_unit = Raster::new(&[("height", "dBZ")], |_, _| Some(1.0));
    let err = fail(&with_dem(vec!["t2m".into()]), Some(bad_unit), None);
    assert!(
        err.contains("in 'dBZ'; they must be in m, km, ft or gpm"),
        "unexpected value"
    );
    // Several parameters: name one, and only one it has.
    let several = || Raster::new(&[("orog", "m"), ("lsm", "1")], |_, _| Some(1.0));
    let err = fail(&with_dem(vec!["t2m".into()]), Some(several()), None);
    assert!(
        err.contains("has 2 parameters (orog, lsm); name the one to read with elevation_parameter"),
        "unexpected value"
    );
    let mut c = with_dem(vec!["t2m".into()]);
    c.elevation_parameter = Some("height".into());
    let err = fail(&c, Some(several()), None);
    assert!(
        err.contains("elevation_parameter 'height' is not a parameter"),
        "unexpected value"
    );
    c.elevation_parameter = Some("orog".into());
    let several_dem = several();
    let engine = build("several", &source, &c, Some(several_dem.clone()), None).unwrap();
    engine.poll_once();
    assert!(engine.has_data());
    assert_eq!(
        several_dem.requests.lock().unwrap()[0].3.as_deref(),
        Some("orog")
    );
    let mut c = config(vec!["t2m".into()]);
    c.elevation_parameter = Some("orog".into());
    assert!(
        fail(&c, None, None).contains("elevation_parameter is set without its elevation_source")
    );
    // A mask in a unit that is not a fraction.
    let mut c = config(vec!["t2m".into()]);
    c.land_mask_source = Some("land".into());
    let metres = Raster::new(&[("land", "m")], |_, _| Some(1.0));
    let err = fail(&c, None, Some(metres));
    assert!(
        err.contains("land_mask_source 'land' states its mask ('land') in 'm'"),
        "unexpected value"
    );
    // Every parameter opting out of the mask.
    c.parameters = vec![options("t2m", None, Some(false))];
    let fraction = Raster::new(&[("land", "1")], |_, _| Some(1.0));
    let err = fail(&c, None, Some(fraction));
    assert!(
        err.contains("every parameter sets land_mask = false"),
        "unexpected value"
    );
    // Exclusion rules naming nothing.
    let mut c = config(vec!["t2m".into()]);
    c.exclude_where = StationExclusions(vec![StationExclusionRule::new()]);
    assert!(fail(&c, None, None).contains("exclude_where has a rule naming no property"));
    c.exclude_where = StationExclusions(vec![StationExclusionRule::from([(
        "network".to_string(),
        Vec::new(),
    )])]);
    assert!(fail(&c, None, None).contains("exclude_where property 'network' lists no values"));
}

// ---------------------------------------------------------------------------
// Log capture
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Every WARN line of this test binary. One global subscriber, not a
/// scoped one per test: a scoped subscriber misses a callsite another test's
/// thread registered first, as tests run in parallel.
fn log() -> &'static LogBuffer {
    static LOG: std::sync::OnceLock<LogBuffer> = std::sync::OnceLock::new();
    LOG.get_or_init(|| {
        let log = LogBuffer::default();
        let writer = log.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("one global subscriber");
        log
    })
}

/// The WARN lines collection `id` logs while `f` runs: other tests log
/// meanwhile, under their own ids.
fn warnings_of(id: &str, f: impl FnOnce()) -> Vec<String> {
    let log = log();
    let start = log.0.lock().unwrap().len();
    f();
    let bytes = log.0.lock().unwrap();
    let tag = format!("[{id}]");
    String::from_utf8_lossy(&bytes[start..])
        .lines()
        .filter(|line| line.contains(&tag))
        .map(String::from)
        .collect()
}
