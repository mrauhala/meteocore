//! Grid-relative components on a rotated grid through [`DerivedWind`]: the
//! direction of every value is its components turned to true north by the
//! grid's convergence at that value's own position, in every EDR domain
//! shape, and the speed is untouched. The grid is TM35FIN, whose
//! convergence varies with both longitude and latitude, so a value turned
//! at another cell's position shows.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};

use crate::edr_engine::{select_parameters, EdrEngine};
use crate::error::DataServerError;
use crate::geo::{Convergence, Crs, Ellipsoid};
use crate::map_engine::{MapEngine, OutputCrs, RasterInfo, RasterTile};
use crate::model::{
    CoverageResponse, DomainDescription, LocationSnapshot, NdArray, ParameterDescription,
    QueryResult, ValuePrecision, VerticalCoord,
};
use crate::vertical::VerticalKind;
use crate::wind::*;

fn tm35fin() -> Crs {
    Crs::TransverseMercator {
        lat0: 0.0,
        lon0: 27f64.to_radians(),
        k0: 0.9996,
        false_e: 500_000.0,
        false_n: 0.0,
        figure: Ellipsoid::GRS80,
    }
}

fn hour(h: u32) -> DateTime<Utc> {
    format!("2026-10-01T{h:02}:00:00Z").parse().unwrap()
}

/// Value `i`'s grid-relative wind: from `θᵢ = 7i + 3` degrees on the grid,
/// at `2 + i mod 5` m/s.
fn grid_wind(i: usize) -> (f64, f64, f64, f64) {
    let theta = (7.0 * i as f64 + 3.0) % 360.0;
    let speed = 2.0 + (i % 5) as f64;
    let (sin, cos) = theta.to_radians().sin_cos();
    (-speed * sin, -speed * cos, theta, speed)
}

/// A rotated-grid engine whose every EDR query answers `x_wind`/`y_wind`
/// over a canned domain, value `i` being [`grid_wind`]`(i)`.
struct Rotated {
    facts: Arc<WindFacts>,
    info: Arc<RasterInfo>,
    asked: Mutex<Vec<Option<Vec<String>>>>,
}

const COMPONENTS: [&str; 2] = ["x_wind", "y_wind"];

impl Rotated {
    fn new() -> Arc<Self> {
        let component = |name: &str| ParameterFacts {
            standard_name: Some(name.into()),
            frame: VectorFrame::from_standard_name(name),
            unit: "m s-1".into(),
            ..ParameterFacts::new(name)
        };
        Arc::new(Self {
            facts: Arc::new(WindFacts {
                grid: GridAxes::of(&tm35fin()),
                parameters: COMPONENTS.iter().map(|&c| component(c)).collect(),
            }),
            info: Arc::new(RasterInfo {
                native_crs: "EPSG:3067".into(),
                spatial_extent: Some([10.0, 55.0, 40.0, 70.0]),
                times: vec![hour(0)],
                parameter: "x_wind".into(),
                unit: "m s-1".into(),
                parameters: Vec::new(),
                vertical: None,
                grid_size: Some([4, 3]),
                layer_subtitle: None,
                reference_times: Vec::new(),
                native_grid: None,
            }),
            asked: Mutex::new(Vec::new()),
        })
    }

    /// One coverage over `domain`: each component's range of `shape` along
    /// `axes`, value `i` from [`grid_wind`].
    fn coverage(
        &self,
        parameters: Option<&[String]>,
        domain: DomainDescription,
        shape: &[usize],
        axes: &[&str],
    ) -> Result<QueryResult, DataServerError> {
        self.asked
            .lock()
            .unwrap()
            .push(parameters.map(<[String]>::to_vec));
        let selected = select_parameters(parameters, &COMPONENTS)?;
        let n: usize = shape.iter().product();
        let range = |component: &str| NdArray {
            shape: shape.to_vec(),
            axis_names: axes.iter().map(|a| a.to_string()).collect(),
            values: (0..n)
                .map(|i| {
                    let (u, v, ..) = grid_wind(i);
                    Some(if component == "x_wind" { u } else { v })
                })
                .collect(),
            precision: ValuePrecision::F64,
        };
        Ok(QueryResult {
            domain,
            parameters: selected
                .iter()
                .map(|&p| (p.to_string(), self.get_parameter_descriptions()[p].clone()))
                .collect(),
            ranges: selected
                .iter()
                .map(|&p| (p.to_string(), range(p)))
                .collect(),
        })
    }
}

/// The grid of the area and cube answers: four longitudes by three
/// latitudes.
const XS: [f64; 4] = [10.0, 20.0, 30.0, 40.0];
const YS: [f64; 3] = [55.0, 60.0, 65.0];
/// The trajectory's nodes, and the vertical profiles' points.
const NODES: [(f64, f64); 5] = [
    (12.0, 58.0),
    (20.0, 61.0),
    (27.0, 64.0),
    (33.0, 66.0),
    (39.5, 69.0),
];

fn pressure(levels: &[f64]) -> VerticalCoord {
    VerticalCoord {
        kind: VerticalKind::Pressure,
        values: levels.to_vec(),
    }
}

impl MapEngine for Rotated {
    fn get_raster_tile(
        &self,
        _: [f64; 4],
        _: u32,
        _: u32,
        _: Option<DateTime<Utc>>,
        _: &OutputCrs,
        _: Option<&str>,
        _: Option<f64>,
        _: Option<DateTime<Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        unreachable!("EDR only")
    }

    fn raster_info(&self) -> RasterInfo {
        (*self.info).clone()
    }

    fn raster_info_shared(&self) -> Arc<RasterInfo> {
        self.info.clone()
    }
}

impl EdrEngine for Rotated {
    fn get_locations(&self) -> Result<Arc<LocationSnapshot>, DataServerError> {
        Ok(LocationSnapshot::empty())
    }

    /// Two vertical profiles, one per point, three levels each.
    fn query_location(
        &self,
        _: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        let profiles = NODES[..2]
            .iter()
            .map(|&(x, y)| {
                let domain = DomainDescription::VerticalProfile {
                    x,
                    y,
                    t: Some(hour(0)),
                    z: pressure(&[1000.0, 850.0, 500.0]),
                };
                self.coverage(parameters, domain, &[3], &["z"])
            })
            .collect::<Result<_, _>>()?;
        Ok(CoverageResponse::Collection(profiles))
    }

    fn get_parameters(&self) -> Vec<String> {
        COMPONENTS.iter().map(|c| c.to_string()).collect()
    }

    fn get_parameter_descriptions(&self) -> HashMap<String, ParameterDescription> {
        COMPONENTS
            .iter()
            .map(|&c| {
                (
                    c.to_string(),
                    ParameterDescription {
                        label: c.into(),
                        unit: "m s-1".into(),
                        observed_property: c.into(),
                        standard_name: Some(c.into()),
                    },
                )
            })
            .collect()
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        Some((hour(0), hour(2)))
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([10.0, 55.0, 40.0, 70.0])
    }

    /// A three-step series at 24°E 61°N.
    fn query_position(
        &self,
        _: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        let domain = DomainDescription::PointSeries {
            x: 24.0,
            y: 61.0,
            t: (0..3).map(hour).collect(),
            z: None,
        };
        Ok(CoverageResponse::Single(self.coverage(
            parameters,
            domain,
            &[3],
            &["t"],
        )?))
    }

    /// The grid over two steps and two levels, `[t, z, y, x]`; for the
    /// coords `"x left out"`, a range that does not say where its values
    /// lie: no `x` axis on a four-column grid.
    fn query_area(
        &self,
        coords: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        let domain = DomainDescription::Grid {
            x: XS.to_vec(),
            y: YS.to_vec(),
            t: Some(vec![hour(0), hour(1)]),
            z: Some(pressure(&[1000.0, 850.0])),
            coarsened: None,
        };
        let (shape, axes): (&[usize], &[&str]) = if coords == "x left out" {
            (&[2, 2, 12], &["t", "z", "y"])
        } else {
            (&[2, 2, 3, 4], &["t", "z", "y", "x"])
        };
        Ok(CoverageResponse::Single(
            self.coverage(parameters, domain, shape, axes)?,
        ))
    }

    /// The grid at one step, `[y, x]`, no `t` or `z` axis.
    fn query_cube(
        &self,
        _: &crate::feature::Bbox,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _: Option<&[f64]>,
        _: crate::cube::CubeResolution,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        let domain = DomainDescription::Grid {
            x: XS.to_vec(),
            y: YS.to_vec(),
            t: None,
            z: None,
            coarsened: None,
        };
        Ok(CoverageResponse::Single(self.coverage(
            parameters,
            domain,
            &[3, 4],
            &["y", "x"],
        )?))
    }

    /// Five nodes along a path, `[composite]`.
    fn query_trajectory(
        &self,
        _: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        let domain = DomainDescription::Trajectory {
            nodes: NODES.iter().map(|&(x, y)| (hour(0), x, y)).collect(),
            node_z: None,
            z: None,
        };
        Ok(CoverageResponse::Single(self.coverage(
            parameters,
            domain,
            &[NODES.len()],
            &["composite"],
        )?))
    }
}

impl WindSource for Rotated {
    fn wind_facts(&self) -> Arc<WindFacts> {
        self.facts.clone()
    }
}

fn wrap(engine: &Arc<Rotated>) -> (DerivedWind, Vec<String>) {
    let logged = Arc::new(Mutex::new(Vec::new()));
    let sink = logged.clone();
    let log: OutcomeLog = Arc::new(move |id: &str, outcome: &PairOutcome| {
        sink.lock().unwrap().push(format!("{id}: {outcome}"));
    });
    let wind = DerivedWind::new("tm", engine.clone(), log);
    let logged = logged.lock().unwrap().clone();
    (wind, logged)
}

fn derived() -> Option<Vec<String>> {
    Some(vec!["wind_direction".into(), "wind_speed".into()])
}

/// Each value's direction is its grid-relative direction plus the
/// convergence at `positions[i]`, and its speed the components' own.
fn assert_turned(result: &QueryResult, positions: &[(f64, f64)]) {
    let convergence = Convergence::of(&tm35fin()).unwrap();
    let directions = &result.ranges["wind_direction"].values;
    let speeds = &result.ranges["wind_speed"].values;
    assert_eq!(directions.len(), positions.len());
    for (i, &(lon, lat)) in positions.iter().enumerate() {
        let (u, v, theta, speed) = grid_wind(i);
        let gamma = convergence.degrees(lon, lat).unwrap();
        let want = (theta + gamma).rem_euclid(360.0);
        let got = directions[i].unwrap();
        assert!(
            (got - want).abs() < 1e-9,
            "a value is not turned by the convergence at its own position"
        );
        assert_eq!(speeds[i], Some(u.hypot(v)));
        assert!((speeds[i].unwrap() - speed).abs() < 1e-12);
    }
    assert!(!result.ranges.contains_key("x_wind"), "inputs dropped");
    assert_eq!(
        result.parameters["wind_direction"].standard_name.as_deref(),
        Some(WIND_FROM_DIRECTION)
    );
}

fn single(response: CoverageResponse) -> QueryResult {
    match response {
        CoverageResponse::Single(result) => result,
        CoverageResponse::Collection(_) => panic!("one coverage"),
    }
}

#[test]
fn every_value_is_turned_at_its_own_position() {
    let engine = Rotated::new();
    let (wind, logged) = wrap(&engine);
    assert_eq!(
        logged,
        ["tm: x_wind/y_wind: derived speed 'wind_speed' and direction 'wind_direction'"]
    );
    // Direction stays EDR only.
    assert!(wind.plan().map_parameter("wind_direction").is_none());
    assert!(wind
        .get_parameters()
        .contains(&"wind_direction".to_string()));

    // A position series: one point, every step there.
    let series = single(
        wind.query_position("POINT(24 61)", None, derived().as_deref(), None, None)
            .unwrap(),
    );
    assert_turned(&series, &[(24.0, 61.0); 3]);
    assert_eq!(
        engine.asked.lock().unwrap()[0],
        Some(vec!["x_wind".to_string(), "y_wind".to_string()]),
        "the components are read in place of the derived names"
    );

    // An area, [t, z, y, x]: each cell its own position, every step and
    // level of it alike.
    let area = single(
        wind.query_area("POLYGON(...)", None, derived().as_deref(), None, None)
            .unwrap(),
    );
    let mut cells = Vec::new();
    for _t in 0..2 {
        for _z in 0..2 {
            for &lat in &YS {
                for &lon in &XS {
                    cells.push((lon, lat));
                }
            }
        }
    }
    assert_turned(&area, &cells);
    // A cube's grid, [y, x].
    let bbox = crate::feature::Bbox {
        west: 10.0,
        south: 55.0,
        east: 40.0,
        north: 65.0,
    };
    let cube = single(
        wind.query_cube(
            &bbox,
            None,
            derived().as_deref(),
            None,
            crate::cube::CubeResolution::default(),
            None,
        )
        .unwrap(),
    );
    assert_turned(&cube, &cells[..12]);
    // A trajectory: each node its own position.
    let path = single(
        wind.query_trajectory("LINESTRING(...)", None, derived().as_deref(), None, None)
            .unwrap(),
    );
    assert_turned(&path, &NODES);
    // Vertical profiles: each at its own point, every level alike.
    let CoverageResponse::Collection(profiles) = wind
        .query_location("site", None, derived().as_deref(), None, None)
        .unwrap()
    else {
        panic!("a profile per point");
    };
    for (profile, &point) in profiles.iter().zip(&NODES) {
        assert_turned(profile, &[point; 3]);
    }

    // Every parameter, unasked: the components stay, the derived join them.
    let all = single(
        wind.query_position("POINT(24 61)", None, None, None, None)
            .unwrap(),
    );
    assert!(all.ranges.contains_key("x_wind") && all.ranges.contains_key("wind_direction"));
}

/// A range whose shape does not place its values on the domain has no
/// position to turn at: its directions are missing, never turned at a
/// guessed cell. The speed needs no position.
#[test]
fn a_value_without_a_position_gets_no_direction() {
    let engine = Rotated::new();
    let (wind, _) = wrap(&engine);
    let area = single(
        wind.query_area("x left out", None, derived().as_deref(), None, None)
            .unwrap(),
    );
    let directions = &area.ranges["wind_direction"].values;
    assert_eq!(directions.len(), 48);
    assert!(directions.iter().all(Option::is_none));
    assert!(area.ranges["wind_speed"].values.iter().all(Option::is_some));
}
