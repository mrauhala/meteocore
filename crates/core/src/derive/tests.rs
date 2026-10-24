//! The [`Derived`] wrapper with every derivation over a mock engine whose
//! fields are known exactly: geopotential height next to wind (#948). The
//! wind-only behaviour is pinned by `crate::wind`'s tests, which build the
//! same wrapper through `DerivedWind::new`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};

use super::*;
use crate::edr_engine::{select_parameters, EdrEngine};
use crate::error::DataServerError;
use crate::geopotential::STANDARD_GRAVITY;
use crate::map_engine::{MapEngine, OutputCrs, ParameterInfo, RasterInfo, RasterTile};
use crate::model::{
    CoverageResponse, DomainDescription, LocationSnapshot, NdArray, ParameterDescription,
    QueryResult,
};
use crate::wind::{GridAxes, ParameterFacts, VectorFrame, WindFacts, WindSource};

fn hour(h: u32) -> DateTime<Utc> {
    format!("2026-10-01T{h:02}:00:00Z").parse().unwrap()
}

const PARAMETERS: [&str; 4] = ["10u", "10v", "z", "2t"];

/// Four pixels / samples per field: 500 hPa-like geopotential, one sample
/// missing.
fn field(parameter: &str) -> [Option<f64>; 4] {
    match parameter {
        "10u" => [Some(0.0), Some(-3.0), Some(0.0), None],
        "10v" => [Some(-4.0), Some(0.0), Some(0.0), Some(2.0)],
        "z" => [Some(57_600.0), Some(9_806.65), None, Some(0.0)],
        "2t" => [Some(10.0); 4],
        other => panic!("unknown parameter {other}"),
    }
}

fn unit(parameter: &str) -> &'static str {
    match parameter {
        "z" => "m2 s-2",
        "2t" => "°C",
        _ => "m s-1",
    }
}

type Call = (
    String,
    Option<DateTime<Utc>>,
    Option<f64>,
    Option<DateTime<Utc>>,
);

struct Mock {
    facts: Arc<WindFacts>,
    info: Arc<RasterInfo>,
    tiles: Mutex<Vec<Call>>,
    queries: Mutex<Vec<Option<Vec<String>>>>,
    samples: usize,
}

impl Mock {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            facts: Arc::new(WindFacts {
                grid: GridAxes::NorthAligned,
                parameters: PARAMETERS
                    .iter()
                    .map(|&p| ParameterFacts {
                        grib: Some(match p {
                            "10u" => (0, 2, 2),
                            "10v" => (0, 2, 3),
                            "z" => (0, 3, 4),
                            _ => (0, 0, 0),
                        }),
                        frame: VectorFrame::Earth,
                        level: Some("sfc".into()),
                        level_label: Some("surface".into()),
                        unit: unit(p).into(),
                        ..ParameterFacts::new(p)
                    })
                    .collect(),
            }),
            info: Arc::new(RasterInfo {
                native_crs: "CRS:84".into(),
                spatial_extent: Some([0.0, 0.0, 1.0, 1.0]),
                times: vec![hour(0), hour(1), hour(2)],
                parameter: "2t".into(),
                unit: "°C".into(),
                parameters: PARAMETERS
                    .iter()
                    .map(|&p| ParameterInfo {
                        name: p.into(),
                        title: p.into(),
                        unit: unit(p).into(),
                    })
                    .collect(),
                vertical: None,
                grid_size: Some([2, 2]),
                layer_subtitle: None,
                reference_times: vec![hour(0)],
                native_grid: None,
            }),
            tiles: Mutex::new(Vec::new()),
            queries: Mutex::new(Vec::new()),
            samples: 4,
        })
    }
}

impl MapEngine for Mock {
    fn get_raster_tile(
        &self,
        _bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        _output_crs: &OutputCrs,
        parameter: Option<&str>,
        z: Option<f64>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        let parameter = parameter.unwrap_or("2t");
        self.tiles
            .lock()
            .unwrap()
            .push((parameter.to_string(), time, z, reference_time));
        Ok(RasterTile {
            width,
            height,
            values: field(parameter).to_vec().into(),
        })
    }

    fn raster_info(&self) -> RasterInfo {
        (*self.info).clone()
    }

    fn raster_info_shared(&self) -> Arc<RasterInfo> {
        self.info.clone()
    }

    fn resolve_time(
        &self,
        _time: Option<DateTime<Utc>>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        Some(hour(7))
    }

    /// `z` has its own run and timestep; the plural forms say something
    /// else, so a height resolved through them would show.
    fn resolve_parameter_reference_time(
        &self,
        parameter: Option<&str>,
        _time: Option<DateTime<Utc>>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        Some(hour(if parameter == Some("z") { 3 } else { 0 }))
    }

    fn resolve_parameters_reference_time(
        &self,
        _parameters: &[&str],
        _time: Option<DateTime<Utc>>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        Some(hour(9))
    }

    fn parameter_times(&self, parameter: &str) -> Option<Arc<[DateTime<Utc>]>> {
        (parameter == "z").then(|| Arc::from([hour(4), hour(5)]))
    }

    fn resolve_parameter_time(
        &self,
        parameter: Option<&str>,
        time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        match parameter {
            Some("z") => Some(hour(5)),
            _ => self.resolve_time(time, reference_time),
        }
    }

    fn resolve_parameters_time(
        &self,
        _parameters: &[&str],
        _time: Option<DateTime<Utc>>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        Some(hour(9))
    }

    /// `2t` stands in for a layer that draws features (#1100).
    fn layer_picks_features(&self, parameter: &str) -> bool {
        parameter == "2t"
    }

    fn pick_features(
        &self,
        _pick: &crate::feature_pick::FeaturePick<'_>,
    ) -> Result<crate::feature_pick::PickedFeatures, DataServerError> {
        Ok(crate::feature_pick::PickedFeatures {
            features: Vec::new(),
            number_matched: 7,
        })
    }

    fn resolve_elevation(
        &self,
        parameter: Option<&str>,
        z: Option<f64>,
        _time: Option<DateTime<Utc>>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Option<f64> {
        z.or(match parameter {
            Some("z") => Some(500.0),
            _ => Some(1000.0),
        })
    }
}

impl EdrEngine for Mock {
    fn get_locations(&self) -> Result<Arc<LocationSnapshot>, DataServerError> {
        Ok(LocationSnapshot::empty())
    }

    fn query_location(
        &self,
        _: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        unreachable!()
    }

    fn get_parameters(&self) -> Vec<String> {
        PARAMETERS.iter().map(|p| p.to_string()).collect()
    }

    fn get_parameter_descriptions(&self) -> HashMap<String, ParameterDescription> {
        PARAMETERS
            .iter()
            .map(|&p| {
                (
                    p.to_string(),
                    ParameterDescription {
                        label: p.into(),
                        unit: unit(p).into(),
                        observed_property: p.into(),
                        standard_name: None,
                    },
                )
            })
            .collect()
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        Some((hour(0), hour(2)))
    }

    fn get_parameter_available_times(&self, parameter: &str) -> Option<Vec<DateTime<Utc>>> {
        (parameter == "z").then(|| vec![hour(4), hour(5)])
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([0.0, 0.0, 1.0, 1.0])
    }

    fn supported_query_types(&self) -> Vec<String> {
        vec!["position".into()]
    }

    fn query_position(
        &self,
        _: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        self.queries
            .lock()
            .unwrap()
            .push(parameters.map(<[String]>::to_vec));
        let selected = select_parameters(parameters, &PARAMETERS)?;
        let descriptions = self.get_parameter_descriptions();
        Ok(CoverageResponse::Single(QueryResult {
            domain: DomainDescription::PointSeries {
                x: 0.5,
                y: 0.5,
                t: (0..4).map(hour).collect(),
                z: None,
            },
            parameters: selected
                .iter()
                .map(|&p| (p.to_string(), descriptions[p].clone()))
                .collect(),
            ranges: selected
                .iter()
                .map(|&p| {
                    (
                        p.to_string(),
                        NdArray {
                            shape: vec![self.samples],
                            axis_names: vec!["t".into()],
                            values: field(p).into_iter().cycle().take(self.samples).collect(),
                        },
                    )
                })
                .collect(),
        }))
    }
}

impl WindSource for Mock {
    fn wind_facts(&self) -> Arc<WindFacts> {
        self.facts.clone()
    }
}

type Logged = Arc<Mutex<Vec<String>>>;

fn wrap(mock: &Arc<Mock>, derive: Derivations) -> (Derived, Logged) {
    let logged: Logged = Arc::default();
    let sink = logged.clone();
    let log: DerivationLog = Arc::new(move |id: &str, outcome: &Outcome| {
        sink.lock().unwrap().push(format!("{id}: {outcome}"));
    });
    (Derived::with("nwp", mock.clone(), derive, log), logged)
}

fn tile(derived: &Derived, parameter: &str) -> Vec<Option<f64>> {
    derived
        .get_raster_tile(
            [0.0, 0.0, 1.0, 1.0],
            2,
            2,
            Some(hour(1)),
            &OutputCrs::WebMercator,
            Some(parameter),
            Some(850.0),
            Some(hour(0)),
        )
        .unwrap()
        .values
        .iter_values()
        .collect()
}

fn position(derived: &Derived, names: Option<&[&str]>) -> QueryResult {
    let names: Option<Vec<String>> = names.map(|n| n.iter().map(|s| s.to_string()).collect());
    let CoverageResponse::Single(result) = derived
        .query_position("POINT(0.5 0.5)", None, names.as_deref(), None, None)
        .unwrap()
    else {
        panic!("expected one series")
    };
    result
}

fn heights() -> Vec<Option<f64>> {
    field("z")
        .iter()
        .map(|z| z.map(|z| z / STANDARD_GRAVITY))
        .collect()
}

/// Wind and height together: each logged once, in the server's line
/// shape, wind's exactly as before (#897). The height joins the map
/// parameters after the wind speed; EDR also gets the direction.
#[test]
fn every_derivation_is_planned_logged_and_listed() {
    let mock = Mock::new();
    let (derived, logged) = wrap(&mock, Derivations::ALL);
    assert_eq!(
        *logged.lock().unwrap(),
        [
            "nwp: wind 10u/10v: derived speed '10si' and direction '10wdir'",
            "nwp: geopotential height z: derived 'gh'",
        ]
    );
    let info = derived.raster_info_shared();
    let names: Vec<&str> = info.parameters.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["10u", "10v", "z", "2t", "10si", "gh"]);
    let gh = &info.parameters[5];
    assert_eq!(
        (gh.title.as_str(), gh.unit.as_str()),
        ("Geopotential height (surface)", "gpm")
    );
    assert!(Arc::ptr_eq(&info, &derived.raster_info_shared()), "O(1)");
    assert_eq!(
        derived.get_parameters(),
        ["10u", "10v", "z", "2t", "10si", "10wdir", "gh"]
    );
    let gh = &derived.get_parameter_descriptions()["gh"];
    assert_eq!(gh.unit, "gpm");
    assert_eq!(gh.standard_name.as_deref(), Some("geopotential_height"));
    // Unchanged facts: no new plan, no new log line.
    derived.plan();
    assert_eq!(logged.lock().unwrap().len(), 2);
}

/// Each derivation is its own opt-out: off, the parameters are the
/// engine's and nothing is logged for it.
#[test]
fn derivations_are_opted_out_one_by_one() {
    let mock = Mock::new();
    let (wind, logged) = wrap(&mock, Derivations::WIND);
    assert_eq!(
        wind.get_parameters(),
        ["10u", "10v", "z", "2t", "10si", "10wdir"]
    );
    assert_eq!(logged.lock().unwrap().len(), 1);
    let height = Derivations {
        wind: false,
        geopotential_height: true,
        humidity: false,
    };
    let (height, logged) = wrap(&mock, height);
    assert_eq!(height.get_parameters(), ["10u", "10v", "z", "2t", "gh"]);
    assert_eq!(
        *logged.lock().unwrap(),
        ["nwp: geopotential height z: derived 'gh'"]
    );
    let names: Vec<String> = height
        .raster_info_shared()
        .parameters
        .iter()
        .map(|p| p.name.clone())
        .collect();
    assert_eq!(names, ["10u", "10v", "z", "2t", "gh"]);
    let none = Derivations {
        wind: false,
        geopotential_height: false,
        humidity: false,
    };
    assert!(!none.any());
    let (plain, logged) = wrap(&mock, none);
    assert_eq!(plain.get_parameters(), PARAMETERS);
    assert!(Arc::ptr_eq(
        &plain.raster_info_shared(),
        &mock.raster_info_shared()
    ));
    assert!(logged.lock().unwrap().is_empty());
}

/// The height tile is the geopotential tile of the same request divided by
/// g₀ per pixel: z is read once, with the request's own time, level and
/// run, as the engine would read it.
#[test]
fn a_height_tile_is_the_geopotential_tile_over_g0() {
    let mock = Mock::new();
    let (derived, _) = wrap(&mock, Derivations::ALL);
    let got = tile(&derived, "gh");
    let want = heights();
    for (got, want) in got.iter().zip(&want) {
        match (got, want) {
            (Some(got), Some(want)) => assert!((got - want).abs() < 1e-3 * want.abs().max(1.0)),
            _ => assert_eq!(got.is_some(), want.is_some()),
        }
    }
    assert_eq!(
        *mock.tiles.lock().unwrap(),
        [("z".to_string(), Some(hour(1)), Some(850.0), Some(hour(0)))]
    );
    mock.tiles.lock().unwrap().clear();
    // Unpinned: still exactly what the engine is asked for z, no run is
    // picked on its behalf.
    derived
        .get_raster_tile(
            [0.0, 0.0, 1.0, 1.0],
            2,
            2,
            None,
            &OutputCrs::WebMercator,
            Some("gh"),
            None,
            None,
        )
        .unwrap();
    assert_eq!(
        *mock.tiles.lock().unwrap(),
        [("z".to_string(), None, None, None)]
    );
}

/// A height resolves time, run, level and axis exactly as its geopotential
/// does, through the engine's own single-parameter selection (#507/#521).
#[test]
fn a_height_resolves_as_its_geopotential() {
    let mock = Mock::new();
    let (derived, _) = wrap(&mock, Derivations::ALL);
    let rt = |p| derived.resolve_parameter_reference_time(Some(p), Some(hour(1)), None);
    assert_eq!(rt("gh"), rt("z"));
    assert_eq!(rt("gh"), Some(hour(3)));
    let t = |p| derived.resolve_parameter_time(Some(p), Some(hour(1)), None);
    assert_eq!(t("gh"), t("z"));
    assert_eq!(t("gh"), Some(hour(5)));
    assert_eq!(
        derived.parameter_times("gh").as_deref(),
        Some(&[hour(4), hour(5)][..])
    );
    assert_eq!(
        derived.get_parameter_available_times("gh"),
        Some(vec![hour(4), hour(5)])
    );
    assert_eq!(
        derived.resolve_elevation(Some("gh"), None, None, None),
        Some(500.0)
    );
    assert!(derived.layer_shows_values("gh"));
    // The wind speed keeps the plural selection over u and v together.
    assert_eq!(rt("10si"), Some(hour(9)));
    assert_eq!(t("10si"), Some(hour(9)));
}

/// #1100: the wrapper forwards the wrapped engine's feature picks, and a
/// derived parameter, which shows values, draws none.
#[test]
fn feature_picks_are_the_wrapped_engines() {
    let mock = Mock::new();
    let (derived, _) = wrap(&mock, Derivations::ALL);
    assert!(derived.layer_picks_features("2t"));
    assert!(!derived.layer_picks_features("gh"));
    assert!(!derived.layer_picks_features("10u"));
    let pick = |parameter| crate::feature_pick::FeaturePick {
        parameter: Some(parameter),
        map: crate::feature_pick::PickMap {
            output_crs: &OutputCrs::Wgs84,
            wgs84_bbox: [0.0, 0.0, 1.0, 1.0],
            width: 1,
            height: 1,
        },
        pixel: (0, 0),
        point: (0.5, 0.5),
        time: None,
        reference_time: None,
        z: None,
        limit: 1,
    };
    assert_eq!(
        derived.pick_features(&pick("2t")).unwrap().number_matched,
        7
    );
    assert!(matches!(
        derived.pick_features(&pick("gh")),
        Err(DataServerError::InvalidParameter(_))
    ));
}

/// EDR: the height is computed from the geopotential the engine returns,
/// value by value, with its own unit and CF standard name; a geopotential
/// the client did not ask for is dropped again.
#[test]
fn edr_serves_the_height_from_the_queried_geopotential() {
    let mock = Mock::new();
    let (derived, _) = wrap(&mock, Derivations::ALL);
    let result = position(&derived, Some(&["GH"]));
    assert_eq!(
        mock.queries.lock().unwrap().pop().unwrap(),
        Some(vec!["z".to_string()])
    );
    let keys: Vec<&String> = result.ranges.keys().collect();
    assert_eq!(keys, ["gh"]);
    assert_eq!(result.ranges["gh"].values, heights());
    assert_eq!(result.parameters["gh"].unit, "gpm");
    assert_eq!(
        result.parameters["gh"].standard_name.as_deref(),
        Some("geopotential_height")
    );
    // Asked for both: z stays, in m² s⁻².
    let result = position(&derived, Some(&["gh", "z"]));
    assert_eq!(result.ranges["z"].values, field("z"));
    assert_eq!(result.parameters["z"].unit, "m2 s-2");
    // Wind and height in one query: one inner query for all inputs.
    let result = position(&derived, Some(&["gh", "10si"]));
    assert_eq!(
        mock.queries.lock().unwrap().pop().unwrap(),
        Some(vec!["z".to_string(), "10u".to_string(), "10v".to_string()])
    );
    let mut keys: Vec<&String> = result.ranges.keys().collect();
    keys.sort();
    assert_eq!(keys, ["10si", "gh"]);
    // Every parameter: the derived ones join the inputs.
    let result = position(&derived, None);
    assert_eq!(result.ranges.len(), 7);
    assert_eq!(result.ranges["gh"].values, heights());
}

/// The budget message names the derivation when the derived parameters
/// are one derivation's, and none when they are several.
#[test]
fn derived_heights_count_against_the_response_budget() {
    use crate::feature::MAX_AREA_VALUES;
    let mut mock = Mock::new();
    Arc::get_mut(&mut mock).unwrap().samples = MAX_AREA_VALUES / 2;
    let (derived, _) = wrap(&mock, Derivations::ALL);
    let n = MAX_AREA_VALUES / 2;
    let message = |names: &[&str]| {
        let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        match derived.query_position("POINT(0.5 0.5)", None, Some(&names), None, None) {
            Err(DataServerError::QueryTooLarge(message)) => message,
            other => panic!("{names:?}: expected QueryTooLarge, got {other:?}"),
        }
    };
    assert!(message(&["gh", "z", "2t"]).starts_with(&format!(
        "Query would return {} values ({} queried + {n} derived geopotential height values)",
        3 * n,
        2 * n
    )));
    assert!(message(&["gh", "10si", "2t"]).starts_with(&format!(
        "Query would return {} values ({n} queried + {} derived values)",
        3 * n,
        2 * n
    )));
    let names: Vec<String> = ["gh", "z"].iter().map(|s| s.to_string()).collect();
    assert!(derived
        .query_position("POINT(0.5 0.5)", None, Some(&names), None, None)
        .is_ok());
}
