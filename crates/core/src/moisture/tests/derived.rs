//! Derived humidity through [`crate::derive::Derived`] over mock engines
//! whose fields are known exactly: values derived after sampling on the
//! map and in EDR, the level's pressure from the request and the coverage,
//! and the metadata every API serves.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};

use super::*;
use crate::derive::{DerivationLog, Derivations, Derived, Outcome};
use crate::edr_engine::{select_parameters, EdrEngine};
use crate::error::DataServerError;
use crate::map_engine::{MapEngine, OutputCrs, ParameterInfo, RasterInfo, RasterTile};
use crate::model::{
    CoverageResponse, DomainDescription, LocationSnapshot, NdArray, ParameterDescription,
    QueryResult, VerticalCoord,
};
use crate::vertical::VerticalDimension;
use crate::wind::GridAxes;

fn hour(h: u32) -> DateTime<Utc> {
    format!("2026-10-01T{h:02}:00:00Z").parse().unwrap()
}

/// One mock parameter: its facts, unit, and its four samples (pixels or
/// values), the same at every level.
struct Field {
    facts: ParameterFacts,
    samples: [Option<f64>; 4],
}

struct Mock {
    facts: Arc<WindFacts>,
    info: Arc<RasterInfo>,
    fields: Vec<Field>,
    /// `(parameter, z)` of every tile asked for.
    tiles: Mutex<Vec<(String, Option<f64>)>>,
}

impl Mock {
    fn new(fields: Vec<Field>, vertical: Option<VerticalDimension>) -> Arc<Self> {
        Arc::new(Self {
            facts: Arc::new(WindFacts {
                grid: GridAxes::NorthAligned,
                parameters: fields.iter().map(|f| f.facts.clone()).collect(),
            }),
            info: Arc::new(RasterInfo {
                native_crs: "CRS:84".into(),
                spatial_extent: Some([0.0, 0.0, 1.0, 1.0]),
                times: vec![hour(0)],
                parameter: fields[0].facts.name.clone(),
                unit: fields[0].facts.unit.clone(),
                parameters: fields
                    .iter()
                    .map(|f| ParameterInfo {
                        name: f.facts.name.clone(),
                        title: f.facts.name.clone(),
                        unit: f.facts.unit.clone(),
                    })
                    .collect(),
                vertical,
                grid_size: Some([2, 2]),
                layer_subtitle: None,
                reference_times: Vec::new(),
                native_grid: None,
            }),
            fields,
            tiles: Mutex::new(Vec::new()),
        })
    }

    fn field(&self, name: &str) -> &Field {
        self.fields
            .iter()
            .find(|f| f.facts.name == name)
            .unwrap_or_else(|| panic!("unknown parameter {name}"))
    }

    fn names(&self) -> Vec<String> {
        self.fields.iter().map(|f| f.facts.name.clone()).collect()
    }
}

impl MapEngine for Mock {
    fn get_raster_tile(
        &self,
        _bbox: [f64; 4],
        width: u32,
        height: u32,
        _time: Option<DateTime<Utc>>,
        _output_crs: &OutputCrs,
        parameter: Option<&str>,
        z: Option<f64>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        let parameter = parameter.unwrap();
        self.tiles.lock().unwrap().push((parameter.to_string(), z));
        Ok(RasterTile {
            width,
            height,
            values: self.field(parameter).samples.to_vec().into(),
        })
    }

    fn raster_info(&self) -> RasterInfo {
        (*self.info).clone()
    }

    fn raster_info_shared(&self) -> Arc<RasterInfo> {
        self.info.clone()
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
        self.names()
    }

    fn get_parameter_descriptions(&self) -> HashMap<String, ParameterDescription> {
        self.fields
            .iter()
            .map(|f| {
                (
                    f.facts.name.clone(),
                    ParameterDescription {
                        label: f.facts.name.clone(),
                        unit: f.facts.unit.clone(),
                        observed_property: f.facts.name.clone(),
                        standard_name: None,
                    },
                )
            })
            .collect()
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        Some((hour(0), hour(0)))
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([0.0, 0.0, 1.0, 1.0])
    }

    /// The domain `coords` names: `profile` (two levels on a `z` axis),
    /// `grid` (`t`, `z`, `y`, `x` with two levels and two columns),
    /// `level` (a series pinned to 500 hPa), `series` (no `z`). Each
    /// range holds the field's four samples.
    fn query_position(
        &self,
        coords: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        let names = self.names();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let selected = select_parameters(parameters, &names)?;
        let pressure = |values: Vec<f64>| VerticalCoord {
            kind: VerticalKind::Pressure,
            values,
        };
        let (domain, shape, axes): (_, Vec<usize>, Vec<&str>) = match coords {
            "profile" => (
                DomainDescription::VerticalProfile {
                    x: 0.5,
                    y: 0.5,
                    t: Some(hour(0)),
                    z: pressure(vec![850.0, 700.0, 500.0, 300.0]),
                },
                vec![4],
                vec!["z"],
            ),
            "grid" => (
                DomainDescription::Grid {
                    x: vec![0.25, 0.75],
                    y: vec![0.5],
                    t: Some(vec![hour(0)]),
                    z: Some(pressure(vec![850.0, 500.0])),
                },
                vec![1, 2, 1, 2],
                vec!["t", "z", "y", "x"],
            ),
            "level" => (
                DomainDescription::PointSeries {
                    x: 0.5,
                    y: 0.5,
                    t: (0..4).map(hour).collect(),
                    z: Some(pressure(vec![500.0])),
                },
                vec![4],
                vec!["t"],
            ),
            _ => (
                DomainDescription::PointSeries {
                    x: 0.5,
                    y: 0.5,
                    t: (0..4).map(hour).collect(),
                    z: None,
                },
                vec![4],
                vec!["t"],
            ),
        };
        let descriptions = self.get_parameter_descriptions();
        Ok(CoverageResponse::Single(QueryResult {
            domain,
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
                            shape: shape.clone(),
                            axis_names: axes.iter().map(|a| a.to_string()).collect(),
                            values: self.field(p).samples.to_vec(),
                        },
                    )
                })
                .collect(),
        }))
    }
}

impl crate::wind::WindSource for Mock {
    fn wind_facts(&self) -> Arc<WindFacts> {
        self.facts.clone()
    }
}

type Logged = Arc<Mutex<Vec<String>>>;

fn wrap(mock: &Arc<Mock>) -> (Derived, Logged) {
    let logged: Logged = Arc::default();
    let sink = logged.clone();
    let log: DerivationLog = Arc::new(move |id: &str, outcome: &Outcome| {
        sink.lock().unwrap().push(format!("{id}: {outcome}"));
    });
    (
        Derived::with("nwp", mock.clone(), Derivations::ALL, log),
        logged,
    )
}

fn tile(derived: &Derived, parameter: &str, z: Option<f64>) -> Vec<Option<f64>> {
    derived
        .get_raster_tile(
            [0.0, 0.0, 1.0, 1.0],
            2,
            2,
            None,
            &OutputCrs::WebMercator,
            Some(parameter),
            z,
            None,
        )
        .unwrap()
        .values
        .iter_values()
        .collect()
}

fn query(derived: &Derived, domain: &str, names: &[&str]) -> QueryResult {
    let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    match derived
        .query_position(domain, None, Some(&names), None, None)
        .unwrap()
    {
        CoverageResponse::Single(result) => result,
        CoverageResponse::Collection(_) => panic!("expected one coverage"),
    }
}

/// A map pixel is an f32.
fn as_f32(value: Option<f64>) -> Option<f64> {
    value.map(|v| f64::from(v as f32))
}

fn single_level() -> Arc<Mock> {
    let facts = |name: &str, triple, unit: &str| ParameterFacts {
        grib: Some(triple),
        unit: unit.into(),
        level: Some("hag:2".into()),
        level_label: Some("2 m above ground".into()),
        ..ParameterFacts::new(name)
    };
    Mock::new(
        vec![
            // 2t served in K, 2d in °C: each is converted from its own unit.
            Field {
                facts: facts("2t", (0, 0, 0), "K"),
                samples: [Some(293.15), Some(273.15), Some(253.15), Some(283.15)],
            },
            Field {
                facts: facts("2d", (0, 0, 6), "°C"),
                samples: [Some(10.0), Some(-2.0), None, Some(10.4)],
            },
        ],
        None,
    )
}

/// `2t` + `2d` → `2r` on every API: each pixel and value the formula over
/// the samples of the same request, never interpolated itself; logged once.
#[test]
fn rh_from_t_and_td_is_derived_after_sampling() {
    let mock = single_level();
    let (derived, logged) = wrap(&mock);
    assert_eq!(
        *logged.lock().unwrap(),
        [
            "nwp: humidity 2t: derived relative humidity '2r' from '2t' and '2d'; no dew \
          point: the collection already has '2d'"
        ]
    );
    let want: Vec<Option<f64>> = mock
        .field("2t")
        .samples
        .iter()
        .zip(mock.field("2d").samples)
        .map(|(t, td)| relative_humidity_from_dew_point((*t)?, td? + K))
        .collect();
    assert_eq!(want[0].map(|v| v.round()), Some(53.0));
    assert_eq!(want[2], None, "a missing dew point");
    assert_eq!(want[3], Some(100.0), "Td above T clamps");
    assert_eq!(
        tile(&derived, "2r", None),
        want.iter().copied().map(as_f32).collect::<Vec<_>>()
    );
    assert_eq!(
        *mock.tiles.lock().unwrap(),
        [("2t".to_string(), None), ("2d".to_string(), None)]
    );

    let info = derived.raster_info_shared();
    let rh = info.parameters.iter().find(|p| p.name == "2r").unwrap();
    assert_eq!(
        (rh.title.as_str(), rh.unit.as_str()),
        ("Relative humidity over water (2 m above ground)", "%")
    );
    assert!(derived.get_parameters().contains(&"2r".to_string()));
    let description = &derived.get_parameter_descriptions()["2r"];
    assert_eq!(description.unit, "%");
    assert_eq!(
        description.standard_name.as_deref(),
        Some("relative_humidity")
    );

    let result = query(&derived, "series", &["2R"]);
    assert_eq!(result.ranges["2r"].values, want);
    assert!(!result.ranges.contains_key("2t"), "inputs not asked for go");
    assert_eq!(result.parameters["2r"].unit, "%");
}

fn pressure_view() -> Arc<Mock> {
    let facts = |name: &str, triple, unit: &str| ParameterFacts {
        grib: Some(triple),
        unit: unit.into(),
        vertical: Some(VerticalKind::Pressure),
        ..ParameterFacts::new(name)
    };
    Mock::new(
        vec![
            Field {
                facts: facts("t", (0, 0, 0), "°C"),
                samples: [Some(10.0), Some(-5.0), Some(-30.0), Some(-50.0)],
            },
            // In g/kg, one negative.
            Field {
                facts: facts("q", (0, 1, 0), "g kg-1"),
                samples: [Some(6.0), Some(2.0), Some(0.3), Some(-0.01)],
            },
        ],
        Some(VerticalDimension::new(
            VerticalKind::Pressure,
            vec![850.0, 700.0, 500.0, 300.0],
        )),
    )
}

/// The formula over the pressure view's samples at `pressures` (Pa), one
/// per sample.
fn from_q(
    mock: &Mock,
    pressures: [f64; 4],
    f: fn(f64, f64, f64) -> Option<f64>,
    celsius: bool,
) -> Vec<Option<f64>> {
    let (t, q) = (mock.field("t").samples, mock.field("q").samples);
    (0..4)
        .map(|i| {
            let value = f(t[i]? + K, q[i]? / 1000.0, pressures[i])?;
            Some(if celsius { value - K } else { value })
        })
        .collect()
}

/// AIFS-like `t` + `q` on pressure levels → `r` and `dpt`, each from the
/// pressure of the level drawn: the request's `ELEVATION`, else the level
/// the engine draws without one.
#[test]
fn rh_and_td_from_q_take_the_drawn_level_pressure() {
    let mock = pressure_view();
    let (derived, logged) = wrap(&mock);
    assert_eq!(
        *logged.lock().unwrap(),
        [
            "nwp: humidity t: derived relative humidity 'r' from 't', 'q' and the level's \
          pressure; derived dew point 'dpt' from 't', 'q' and the level's pressure"
        ]
    );
    let names: Vec<String> = derived
        .raster_info_shared()
        .parameters
        .iter()
        .map(|p| p.name.clone())
        .collect();
    assert_eq!(names, ["t", "q", "r", "dpt"]);

    let at_500 = from_q(
        &mock,
        [50_000.0; 4],
        relative_humidity_from_specific_humidity,
        false,
    );
    assert_eq!(at_500[3], Some(0.0), "q ≤ 0 gives 0 %");
    assert_eq!(
        tile(&derived, "r", Some(500.0)),
        at_500.iter().copied().map(as_f32).collect::<Vec<_>>()
    );
    // No ELEVATION: the first advertised level, as the engine draws it.
    let at_850 = from_q(
        &mock,
        [85_000.0; 4],
        relative_humidity_from_specific_humidity,
        false,
    );
    assert_ne!(at_850, at_500);
    assert_eq!(
        tile(&derived, "r", None),
        at_850.iter().copied().map(as_f32).collect::<Vec<_>>()
    );
    let dew = from_q(&mock, [70_000.0; 4], dew_point_from_specific_humidity, true);
    assert_eq!(dew[3], None, "q ≤ 0 gives no dew point");
    assert_eq!(
        tile(&derived, "dpt", Some(700.0)),
        dew.iter().copied().map(as_f32).collect::<Vec<_>>()
    );
    assert_eq!(
        derived.get_parameter_descriptions()["dpt"]
            .standard_name
            .as_deref(),
        Some("dew_point_temperature")
    );
}

/// EDR takes each value's level from the coverage's `z`: a profile's `z`
/// axis, a grid's `z` axis between `t` and `y`, a series pinned to one
/// level; a coverage without an isobaric `z` gives no values.
#[test]
fn edr_takes_each_values_level_from_the_coverage() {
    let mock = pressure_view();
    let (derived, _) = wrap(&mock);
    let rh = relative_humidity_from_specific_humidity;
    let result = query(&derived, "profile", &["r", "t"]);
    assert_eq!(
        result.ranges["r"].values,
        from_q(&mock, [85_000.0, 70_000.0, 50_000.0, 30_000.0], rh, false)
    );
    assert!(result.ranges.contains_key("t"));
    assert!(!result.ranges.contains_key("q"));
    let result = query(&derived, "grid", &["dpt"]);
    assert_eq!(
        result.ranges["dpt"].values,
        from_q(
            &mock,
            [85_000.0, 85_000.0, 50_000.0, 50_000.0],
            dew_point_from_specific_humidity,
            true
        )
    );
    assert_eq!(result.parameters["dpt"].unit, "°C");
    let result = query(&derived, "level", &["r"]);
    assert_eq!(
        result.ranges["r"].values,
        from_q(&mock, [50_000.0; 4], rh, false)
    );
    let result = query(&derived, "series", &["r"]);
    assert_eq!(result.ranges["r"].values, [None; 4]);
}
