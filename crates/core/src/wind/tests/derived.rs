//! The [`DerivedWind`] wrapper over a mock engine whose component fields are
//! known exactly.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use chrono::{DateTime, Utc};

use crate::edr_engine::{select_parameters, EdrEngine, TrajectoryShape};
use crate::error::DataServerError;
use crate::map_engine::{MapEngine, OutputCrs, ParameterInfo, RasterInfo, RasterTile};
use crate::model::{
    CoverageResponse, DomainDescription, Location, NdArray, ParameterDescription, QueryResult,
};
use crate::wind::*;

fn hour(h: u32) -> DateTime<Utc> {
    format!("2026-10-01T{h:02}:00:00Z").parse().unwrap()
}

const PARAMETERS: [&str; 3] = ["10u", "10v", "2t"];

/// Four pixels / samples per field. 10u, 10v: a northerly, an easterly,
/// calm, and a pixel without u.
fn field(parameter: &str) -> [Option<f64>; 4] {
    match parameter {
        "10u" => [Some(0.0), Some(-3.0), Some(0.0), None],
        "10v" => [Some(-4.0), Some(0.0), Some(0.0), Some(2.0)],
        "2t" => [Some(10.0); 4],
        other => panic!("unknown parameter {other}"),
    }
}

type Call = (
    String,
    Option<DateTime<Utc>>,
    Option<f64>,
    Option<DateTime<Utc>>,
);

struct Mock {
    facts: RwLock<Arc<WindFacts>>,
    info: Arc<RasterInfo>,
    /// Per-parameter time axes, like a satellite collection's.
    axes: HashMap<&'static str, Arc<[DateTime<Utc>]>>,
    tiles: Mutex<Vec<Call>>,
    queries: Mutex<Vec<Option<Vec<String>>>>,
    /// Values per parameter in a position series: [`field`] repeated.
    samples: usize,
}

impl Mock {
    fn new(frame: VectorFrame) -> Arc<Self> {
        Arc::new(Self {
            facts: RwLock::new(Arc::new(wind_facts(frame))),
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
                        unit: if p == "2t" { "°C" } else { "m s-1" }.into(),
                    })
                    .collect(),
                vertical: None,
                grid_size: Some([2, 2]),
                layer_subtitle: None,
                reference_times: vec![hour(0)],
            }),
            axes: HashMap::new(),
            tiles: Mutex::new(Vec::new()),
            queries: Mutex::new(Vec::new()),
            samples: 4,
        })
    }
}

fn wind_facts(frame: VectorFrame) -> WindFacts {
    WindFacts {
        grid: GridAxes::NorthAligned,
        parameters: PARAMETERS
            .iter()
            .map(|&p| ParameterFacts {
                grib: Some(match p {
                    "10u" => (0, 2, 2),
                    "10v" => (0, 2, 3),
                    _ => (0, 0, 0),
                }),
                frame,
                level: Some("sfc".into()),
                unit: if p == "2t" { "°C" } else { "m s-1" }.into(),
                ..ParameterFacts::new(p)
            })
            .collect(),
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
        assert_eq!((width, height), (2, 2));
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

    fn resolve_reference_time(
        &self,
        _time: Option<DateTime<Utc>>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        Some(hour(0))
    }

    fn content_version(&self) -> u64 {
        42
    }

    fn default_time(&self) -> Option<DateTime<Utc>> {
        Some(hour(5))
    }

    fn parameter_times(&self, parameter: &str) -> Option<Arc<[DateTime<Utc>]>> {
        self.axes.get(parameter).cloned()
    }

    /// Latest-not-after on a parameter's own axis.
    fn resolve_parameter_time(
        &self,
        parameter: Option<&str>,
        time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        match parameter.and_then(|p| self.axes.get(p)) {
            Some(axis) => {
                let time = time.unwrap_or(DateTime::<Utc>::MAX_UTC);
                axis.iter().rev().find(|t| **t <= time).copied()
            }
            None => self.resolve_time(time, reference_time),
        }
    }
}

impl EdrEngine for Mock {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        Ok(Vec::new())
    }

    fn location_time_filter<'a>(
        &'a self,
        _: &'a [crate::feature::DatetimeInterval],
    ) -> Option<crate::edr_engine::LocationFilter<'a>> {
        Some(Box::new(|location: &Location| location.id == "kept"))
    }

    fn has_instances(&self) -> bool {
        true
    }

    fn instance_reference_times(&self) -> Vec<DateTime<Utc>> {
        vec![hour(0), hour(6)]
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
                        unit: "m s-1".into(),
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

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([0.0, 0.0, 1.0, 1.0])
    }

    fn supported_query_types(&self) -> Vec<String> {
        vec!["position".into(), "cube".into()]
    }

    fn trajectory_shape(&self) -> TrajectoryShape {
        TrajectoryShape::CrossSection
    }

    /// A four-step series per parameter, through the shared selection rule.
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
        self.facts.read().unwrap().clone()
    }
}

type Logged = Arc<Mutex<Vec<String>>>;

fn wrap(mock: &Arc<Mock>) -> (DerivedWind, Logged) {
    let logged: Logged = Arc::default();
    let sink = logged.clone();
    let log: OutcomeLog = Arc::new(move |id: &str, outcome: &PairOutcome| {
        sink.lock().unwrap().push(format!("{id}: {outcome}"));
    });
    (DerivedWind::new("nwp", mock.clone(), log), logged)
}

fn tile(wind: &DerivedWind, parameter: &str) -> Result<RasterTile, DataServerError> {
    wind.get_raster_tile(
        [0.0, 0.0, 1.0, 1.0],
        2,
        2,
        Some(hour(1)),
        &OutputCrs::WebMercator,
        Some(parameter),
        Some(850.0),
        Some(hour(0)),
    )
}

#[test]
fn the_map_apis_get_the_speed_only() {
    let mock = Mock::new(VectorFrame::Earth);
    let (wind, _) = wrap(&mock);
    let info = wind.raster_info_shared();
    let names: Vec<&str> = info.parameters.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["10u", "10v", "2t", "10si"]);
    let speed = &info.parameters[3];
    assert_eq!(
        (speed.title.as_str(), speed.unit.as_str()),
        ("Wind speed", "m s-1")
    );
    // O(1): one snapshot until the engine's or the plan's changes.
    assert!(Arc::ptr_eq(&info, &wind.raster_info_shared()));
    assert!(matches!(
        tile(&wind, "10wdir"),
        Err(DataServerError::InvalidParameter(_))
    ));
}

/// The speed tile is the hypotenuse of the two component tiles of the same
/// request, and both components are read with identical arguments.
#[test]
fn a_speed_tile_combines_the_component_tiles_of_one_request() {
    let mock = Mock::new(VectorFrame::Earth);
    let (wind, _) = wrap(&mock);
    let speed = tile(&wind, "10si").unwrap();
    let values: Vec<Option<f64>> = speed.values.iter_values().collect();
    assert_eq!(values, [Some(4.0), Some(3.0), Some(0.0), None]);
    let calls = std::mem::take(&mut *mock.tiles.lock().unwrap());
    let call = |p: &str| (p.to_string(), Some(hour(1)), Some(850.0), Some(hour(0)));
    assert_eq!(calls, [call("10u"), call("10v")]);
    // Other parameters pass straight through.
    tile(&wind, "2t").unwrap();
    assert_eq!(*mock.tiles.lock().unwrap(), [call("2t")]);
}

#[test]
fn edr_serves_speed_and_direction_from_the_queried_components() {
    let mock = Mock::new(VectorFrame::Earth);
    let (wind, _) = wrap(&mock);
    let names = wind.get_parameters();
    assert_eq!(names, ["10u", "10v", "2t", "10si", "10wdir"]);
    let descriptions = wind.get_parameter_descriptions();
    assert_eq!(
        descriptions["10wdir"].standard_name.as_deref(),
        Some("wind_from_direction")
    );
    assert_eq!(descriptions["10wdir"].unit, "°");
    assert_eq!(
        descriptions["10si"].standard_name.as_deref(),
        Some("wind_speed")
    );

    let query = |names: Option<&[&str]>| {
        let names: Option<Vec<String>> = names.map(|n| n.iter().map(|s| s.to_string()).collect());
        let CoverageResponse::Single(result) = wind
            .query_position("POINT(0.5 0.5)", None, names.as_deref(), None, None)
            .unwrap()
        else {
            panic!()
        };
        result
    };
    // Only the derived names: the components are queried and dropped again.
    let result = query(Some(&["10SI", "10wdir"]));
    assert_eq!(
        mock.queries.lock().unwrap().pop().unwrap(),
        Some(vec!["10u".to_string(), "10v".to_string()])
    );
    let mut keys: Vec<&String> = result.ranges.keys().collect();
    keys.sort();
    assert_eq!(keys, ["10si", "10wdir"]);
    assert_eq!(
        result.ranges["10si"].values,
        [Some(4.0), Some(3.0), Some(0.0), None]
    );
    // Northerly, easterly, calm, missing.
    assert_eq!(
        result.ranges["10wdir"].values,
        [Some(0.0), Some(90.0), None, None]
    );
    assert_eq!(result.ranges["10wdir"].axis_names, ["t"]);
    assert_eq!(result.parameters["10si"].label, "Wind speed");
    // A component the client asked for stays; the other does not.
    let result = query(Some(&["10si", "10U"]));
    let mut keys: Vec<&String> = result.ranges.keys().collect();
    keys.sort();
    assert_eq!(keys, ["10si", "10u"]);
    // Every parameter: the derived ones join the components.
    let result = query(None);
    assert_eq!(mock.queries.lock().unwrap().pop().unwrap(), None);
    assert_eq!(result.ranges.len(), 5);
    // Plain parameters pass straight through.
    query(Some(&["2t"]));
    assert_eq!(
        mock.queries.lock().unwrap().pop().unwrap(),
        Some(vec!["2t".to_string()])
    );
}

#[test]
fn an_unknown_frame_gives_no_direction_until_the_source_states_it() {
    let mock = Mock::new(VectorFrame::Unknown);
    let (wind, logged) = wrap(&mock);
    assert_eq!(wind.get_parameters(), ["10u", "10v", "2t", "10si"]);
    let first = wind.raster_info_shared();
    assert_eq!(logged.lock().unwrap().len(), 1);
    assert!(logged.lock().unwrap()[0].starts_with(
        "nwp: 10u/10v: derived speed '10si' only; no direction: whether u/v are earth- or grid-relative is unknown"
    ));
    // Unchanged facts: no new plan, no new log line.
    wind.plan();
    wind.get_parameters();
    assert_eq!(logged.lock().unwrap().len(), 1);
    // A metadata probe states the frame: the direction appears, logged once.
    *mock.facts.write().unwrap() = Arc::new(wind_facts(VectorFrame::Earth));
    assert_eq!(
        wind.get_parameters(),
        ["10u", "10v", "2t", "10si", "10wdir"]
    );
    wind.plan();
    let logged = logged.lock().unwrap().clone();
    assert_eq!(
        logged[1..],
        ["nwp: 10u/10v: derived speed '10si' and direction '10wdir'"]
    );
    // The map side is unchanged in content but rebuilt for the new plan.
    let second = wind.raster_info_shared();
    assert!(!Arc::ptr_eq(&first, &second));
    assert_eq!(first.parameters, second.parameters);
}

/// Components with time axes of their own render, and key the caches, at a
/// timestep both have (#507).
#[test]
fn a_derived_speed_resolves_time_on_the_shared_component_axis() {
    let mut mock = Mock::new(VectorFrame::Earth);
    let axes = &mut Arc::get_mut(&mut mock).unwrap().axes;
    axes.insert("10u", vec![hour(0), hour(1), hour(2)].into());
    axes.insert("10v", vec![hour(0), hour(2)].into());
    let (wind, _) = wrap(&mock);
    let axis = wind.parameter_times("10si").unwrap();
    assert_eq!(*axis, [hour(0), hour(2)]);
    assert!(Arc::ptr_eq(&axis, &wind.parameter_times("10si").unwrap()));
    assert_eq!(
        wind.resolve_parameter_time(Some("10si"), Some(hour(1)), None),
        Some(hour(0))
    );
    assert_eq!(
        wind.resolve_parameter_time(Some("10u"), Some(hour(1)), None),
        Some(hour(1))
    );
    assert_eq!(
        wind.resolve_parameters_time(&["10si", "2t"], None, None),
        Some(hour(2))
    );
    // The render reads both components at that shared timestep.
    tile(&wind, "10si").unwrap();
    let times: Vec<_> = mock.tiles.lock().unwrap().iter().map(|c| c.1).collect();
    assert_eq!(times, [Some(hour(0)), Some(hour(0))]);
    assert_eq!(
        wind.get_parameter_available_times("10si"),
        None,
        "the mock's EDR side has no per-parameter axes"
    );
}

/// Everything that is not a derived parameter is the wrapped engine's.
#[test]
fn every_other_method_delegates() {
    let mock = Mock::new(VectorFrame::Earth);
    let (wind, _) = wrap(&mock);
    assert_eq!(wind.resolve_time(None, None), Some(hour(7)));
    assert_eq!(
        wind.resolve_parameter_time(Some("10si"), None, None),
        Some(hour(7))
    );
    assert_eq!(wind.resolve_reference_time(None, None), Some(hour(0)));
    assert_eq!(wind.content_version(), 42);
    assert_eq!(wind.default_time(), Some(hour(5)));
    assert!(wind.composites().is_empty());
    assert!(wind.has_instances());
    assert_eq!(wind.instance_reference_times(), [hour(0), hour(6)]);
    assert_eq!(wind.supported_query_types(), ["position", "cube"]);
    assert_eq!(wind.trajectory_shape(), TrajectoryShape::CrossSection);
    assert_eq!(wind.get_temporal_extent(), Some((hour(0), hour(2))));
    assert_eq!(wind.get_spatial_extent(), Some([0.0, 0.0, 1.0, 1.0]));
    assert_eq!(wind.collection_id(), "nwp");
    let location = |id: &str| Location {
        id: id.into(),
        label: id.into(),
        latitude: 0.0,
        longitude: 0.0,
    };
    let filter = wind
        .location_time_filter(&[])
        .expect("the inner engine's location filter");
    assert!(filter(&location("kept")));
    assert!(!filter(&location("dropped")));
}

/// The derived values count against the shared response budget before any
/// is computed: a request the engine answered within it may not grow past
/// it with its derived parameters.
#[test]
fn derived_values_count_against_the_response_budget() {
    use crate::feature::MAX_AREA_VALUES;
    let mut mock = Mock::new(VectorFrame::Earth);
    // 10u, 10v and 2t together fit; with 10si and 10wdir on top they do not.
    Arc::get_mut(&mut mock).unwrap().samples = MAX_AREA_VALUES / 3;
    let (wind, _) = wrap(&mock);
    let query = |names: Option<&[&str]>| {
        let names: Option<Vec<String>> = names.map(|n| n.iter().map(|s| s.to_string()).collect());
        wind.query_position("POINT(0.5 0.5)", None, names.as_deref(), None, None)
    };
    let n = MAX_AREA_VALUES / 3;
    let too_large = |names: Option<&[&str]>, served: usize, derived: usize| {
        let Err(DataServerError::QueryTooLarge(message)) = query(names) else {
            panic!("{names:?}: expected QueryTooLarge");
        };
        assert_eq!(
            message,
            format!(
                "Query would return {} values ({served} queried + {derived} derived wind \
                 values); the limit is {MAX_AREA_VALUES} — narrow the datetime window, the \
                 area, z or the parameters",
                served + derived
            )
        );
    };
    // Every parameter: the engine's three plus two derived.
    too_large(None, 3 * n, 2 * n);
    // Both derived and both components.
    too_large(Some(&["10si", "10wdir", "10u", "10v"]), 2 * n, 2 * n);
    // Within the budget: the components only the derivation asked for do
    // not count, as they are not returned.
    assert!(query(Some(&["10si", "10wdir"])).is_ok());
    assert!(query(Some(&["10si", "10u", "2t"])).is_ok());
    // Plain parameters are the engine's business alone.
    assert!(query(Some(&["10u", "10v", "2t"])).is_ok());
}
