//! Parameters over every retained run (#1026): while a new run has only
//! its analysis, its hour-window aggregates, maxima and anything else only
//! an older run still has stay advertised, described, rendered and queried
//! from the older run, until it is evicted.

use super::*;
use crate::test_support::{message, parameter_message, TestSource};
use ds_core::map_engine::default_request_time;
use ds_core::wind::{DerivedWind, PairOutcome, WindSource};

fn at(hour: i64) -> DateTime<Utc> {
    "2026-04-05T00:00:00Z".parse::<DateTime<Utc>>().unwrap() + chrono::Duration::hours(hour)
}

const OLD: &str = "2026040500";
const NEW: &str = "2026040506";

fn air(kelvin: f32) -> Vec<u8> {
    message(0, kelvin, [0; 4], 103, 2)
}

/// Total cloud cover (WMO 0/6/1, %), the same value at every node.
fn cloud(value: f32) -> Vec<u8> {
    parameter_message(6, 1, 0x30, value, [0; 4], 10, 0)
}

/// Relative humidity (WMO 0/1/1, %) at 2 m.
fn humidity(value: f32) -> Vec<u8> {
    parameter_message(1, 1, 0x30, value, [0; 4], 103, 2)
}

/// One step file: `TMP` and `TCDC`, `RH` unless `None`, then `extra`.
fn step(
    source: &TestSource,
    name: &str,
    date: &str,
    forecast: &str,
    offset: f32,
    rh: Option<f32>,
    extra: &[(&str, &str, &str, Vec<u8>)],
) {
    let mut records = vec![
        ("TMP", "2 m above ground", forecast, air(280.0 + offset)),
        ("TCDC", "entire atmosphere", forecast, cloud(offset)),
    ];
    if let Some(value) = rh {
        records.push(("RH", "2 m above ground", forecast, humidity(value)));
    }
    records.extend(extra.iter().cloned());
    source.write_run(name, date, &records);
}

/// A run's f003 and f006: `TMP`, `TCDC`, `RH`, a window average and `TMAX`.
fn complete(source: &TestSource, date: &str, prefix: &str, offset: f32) {
    step(
        source,
        &format!("{prefix}3"),
        date,
        "3 hour fcst",
        offset + 3.0,
        Some(offset + 53.0),
        &[
            (
                "TCDC",
                "entire atmosphere",
                "0-3 hour ave fcst",
                cloud(offset + 30.0),
            ),
            (
                "TMAX",
                "2 m above ground",
                "0-3 hour max fcst",
                air(290.0 + offset),
            ),
        ],
    );
    step(
        source,
        &format!("{prefix}6"),
        date,
        "6 hour fcst",
        offset + 6.0,
        Some(offset + 56.0),
        &[
            (
                "TCDC",
                "entire atmosphere",
                "0-6 hour ave fcst",
                cloud(offset + 60.0),
            ),
            (
                "TMAX",
                "2 m above ground",
                "0-6 hour max fcst",
                air(291.0 + offset),
            ),
        ],
    );
}

/// The 00Z run complete (f000, f003, f006): `TMP`, `TCDC` and `RH` at
/// every step, `TCDC_avg_3h` and `TMAX` at f003, `TCDC_avg_6h` and `TMAX`
/// at f006. The 06Z run with only its analysis so far: `TMP` and `TCDC`.
/// Valid times 00, 03 and 06 (00Z f006 and 06Z f000).
fn new_run_with_only_its_analysis(max_runs: Option<usize>) -> (TestSource, GribEngine) {
    let source = TestSource::new();
    step(&source, "a0", OLD, "anl", 0.0, Some(50.0), &[]);
    complete(&source, OLD, "a", 0.0);
    step(&source, "b0", NEW, "anl", 10.0, None, &[]);
    let mut config = source.config();
    config.max_runs = max_runs;
    let engine = GribEngine::new("boundary", &config).unwrap();
    (source, engine)
}

const ADVERTISED: [&str; 6] = ["TMP", "TCDC", "RH", "TCDC_avg_3h", "TMAX", "TCDC_avg_6h"];

fn names(info: &RasterInfo) -> Vec<&str> {
    info.parameters.iter().map(|p| p.name.as_str()).collect()
}

/// The one value of a map tile `engine` renders.
fn value(
    engine: &dyn MapEngine,
    parameter: Option<&str>,
    time: Option<DateTime<Utc>>,
    reference_time: Option<DateTime<Utc>>,
) -> Result<f32, DataServerError> {
    let tile = engine.get_raster_tile(
        [0.0, 0.0, 1.0, 1.0],
        1,
        1,
        time,
        &OutputCrs::Wgs84,
        parameter,
        None,
        reference_time,
    )?;
    Ok(tile.values.value_at(0).unwrap() as f32)
}

/// What the API layers render for `parameter` at `time`: the run resolved
/// with the parameter, the time within it, then the render pinned to both.
fn as_the_api_renders(
    engine: &dyn MapEngine,
    parameter: Option<&str>,
    time: Option<DateTime<Utc>>,
) -> (Option<DateTime<Utc>>, Option<DateTime<Utc>>, f32) {
    let run = engine.resolve_parameter_reference_time(parameter, time, None);
    let valid = engine.resolve_parameter_time(parameter, time, run);
    (run, valid, value(engine, parameter, valid, run).unwrap())
}

/// A position series: its times and each parameter's values.
type Series = (Vec<DateTime<Utc>>, BTreeMap<String, Vec<Option<f64>>>);

/// The position series at the middle of the grid.
fn series(
    engine: &dyn EdrEngine,
    datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
    parameters: Option<&[&str]>,
    reference_time: Option<DateTime<Utc>>,
) -> Series {
    let parameters: Option<Vec<String>> =
        parameters.map(|p| p.iter().map(|n| n.to_string()).collect());
    let CoverageResponse::Single(result) = engine
        .query_position(
            "POINT(0.5 0.5)",
            datetime,
            parameters.as_deref(),
            None,
            reference_time,
        )
        .unwrap()
    else {
        panic!("expected one series")
    };
    let DomainDescription::PointSeries { t, .. } = result.domain else {
        panic!("expected a point series")
    };
    let ranges = result
        .ranges
        .into_iter()
        .map(|(name, range)| (name, range.values))
        .collect();
    (t, ranges)
}

fn rounded(values: &[Option<f64>]) -> Vec<Option<f64>> {
    values
        .iter()
        .map(|v| v.map(|v| (v * 100.0).round() / 100.0))
        .collect()
}

#[test]
fn an_analysis_only_run_keeps_older_runs_parameters_advertised_and_described() {
    let (source, engine) = new_run_with_only_its_analysis(None);
    let info = engine.raster_info_shared();
    assert_eq!(info.reference_times, [at(0), at(6)]);
    assert_eq!(info.times, [at(0), at(3), at(6)]);
    // The newest run's parameters first, then the older run's.
    assert_eq!(names(&info), ADVERTISED);
    assert_eq!(engine.get_parameters(), ADVERTISED);
    // Labels and units from the older run's messages, probed at load.
    let descriptions = engine.get_parameter_descriptions();
    for (name, unit) in [
        ("RH", "%"),
        ("TCDC_avg_3h", "%"),
        ("TCDC_avg_6h", "%"),
        ("TMAX", "°C"),
    ] {
        let parameter = info.parameters.iter().find(|p| p.name == name).unwrap();
        assert_eq!(parameter.unit, unit, "{name}");
        assert_ne!(parameter.title, name, "{name} has its WMO label");
        assert_eq!(descriptions[name].unit, unit, "{name}");
        assert_eq!(descriptions[name].label, parameter.title, "{name}");
    }
    assert!(info.parameters[3].title.contains("Total cloud cover"));
    // Their own axes, from the older run.
    let times = |name: &str| engine.parameter_times(name).map(|t| t.to_vec());
    assert_eq!(times("TCDC_avg_3h"), Some(vec![at(3)]));
    assert_eq!(times("TCDC_avg_6h"), Some(vec![at(6)]));
    assert_eq!(times("TMAX"), Some(vec![at(3), at(6)]));
    // Every valid time has an `RH`: the older run's.
    assert_eq!(times("RH"), None);

    // The order holds once the new run has them all.
    complete(&source, NEW, "b", 10.0);
    engine.scan_once().unwrap();
    let info = engine.raster_info_shared();
    assert_eq!(info.times, [at(0), at(3), at(6), at(9), at(12)]);
    assert_eq!(names(&info), ADVERTISED);
}

/// The default layer and a parameter the new run has keep the new run; a
/// parameter only the older run has renders, and keys, the older run.
#[test]
fn a_map_of_an_older_runs_parameter_renders_that_run() {
    let (_source, engine) = new_run_with_only_its_analysis(None);
    let info = engine.raster_info_shared();
    // The new run's analysis: 290 K.
    assert_eq!(engine.resolve_reference_time(None, None), Some(at(6)));
    assert_eq!(
        as_the_api_renders(&engine, None, None),
        (Some(at(6)), Some(at(6)), 16.85)
    );
    assert_eq!(
        as_the_api_renders(&engine, Some("TMP"), None),
        (Some(at(6)), Some(at(6)), 16.85)
    );
    // TIME omitted: the older run's last step carrying it.
    for (name, valid, expected) in [
        ("TCDC_avg_6h", 6, 60.0),
        ("TCDC_avg_3h", 3, 30.0),
        ("TMAX", 6, 17.85),
        ("RH", 6, 56.0),
    ] {
        assert_eq!(
            as_the_api_renders(&engine, Some(name), None),
            (Some(at(0)), Some(at(valid)), expected),
            "{name}"
        );
        assert_eq!(value(&engine, Some(name), None, None).unwrap(), expected);
        let default = default_request_time(&engine, &info, Some(name));
        assert_eq!(default, Some(at(valid)), "{name}");
        assert_eq!(
            as_the_api_renders(&engine, Some(name), default),
            (Some(at(0)), Some(at(valid)), expected),
            "{name}"
        );
    }
    // At the new run's analysis time: the older run's step valid then.
    assert_eq!(
        as_the_api_renders(&engine, Some("RH"), Some(at(6))),
        (Some(at(0)), Some(at(6)), 56.0)
    );

    // The #507/#521 invariant: the resolved (time, run) renders the
    // request's pixels, at every requested time.
    let parameters = [None, Some("TMP"), Some("RH"), Some("TCDC_avg_3h")];
    for parameter in parameters
        .into_iter()
        .chain(["TCDC_avg_6h", "TMAX"].map(Some))
    {
        for minutes in (-60..=480).step_by(30) {
            let time = at(0) + chrono::Duration::minutes(minutes);
            let run = engine.resolve_parameter_reference_time(parameter, Some(time), None);
            let valid = engine.resolve_parameter_time(parameter, Some(time), run);
            match (
                value(&engine, parameter, Some(time), None),
                value(&engine, parameter, valid, run),
            ) {
                (Ok(direct), Ok(keyed)) => assert_eq!(direct, keyed, "{parameter:?} at {time}"),
                (Err(_), Err(_)) => {}
                (direct, keyed) => panic!("{parameter:?} at {time}: {direct:?} vs {keyed:?}"),
            }
        }
    }
}

/// EDR queries naming a parameter the new run lacks read a run that has
/// it; without one, or naming only what the new run has, the latest run.
#[test]
fn edr_queries_naming_an_older_runs_parameter_read_that_run() {
    let (_source, engine) = new_run_with_only_its_analysis(None);
    // No parameter, or only what the new run has: its analysis.
    let (t, ranges) = series(&engine, None, None, None);
    assert_eq!(t, [at(6)]);
    assert_eq!(rounded(&ranges["TMP"]), [Some(16.85)]);
    assert_eq!(series(&engine, None, Some(&["TMP"]), None).0, [at(6)]);
    // A window aggregate: the older run, null where a step lacks it.
    let (t, ranges) = series(&engine, None, Some(&["TCDC_avg_6h"]), None);
    assert_eq!(t, [at(0), at(3), at(6)]);
    assert_eq!(rounded(&ranges["TCDC_avg_6h"]), [None, None, Some(60.0)]);
    let (t, ranges) = series(&engine, None, Some(&["RH"]), None);
    assert_eq!(t, [at(0), at(3), at(6)]);
    assert_eq!(rounded(&ranges["RH"]), [Some(50.0), Some(53.0), Some(56.0)]);
    // With a parameter both runs have: the run that has them all.
    let (t, ranges) = series(&engine, None, Some(&["TMP", "TMAX"]), None);
    assert_eq!(t, [at(0), at(3), at(6)]);
    assert_eq!(
        rounded(&ranges["TMP"]),
        [Some(6.85), Some(9.85), Some(12.85)]
    );
    assert_eq!(rounded(&ranges["TMAX"]), [None, Some(16.85), Some(17.85)]);
    // An instant both runs have a step at: the newest run that has it.
    let instant = Some((at(6), at(6)));
    let (_, ranges) = series(&engine, instant, Some(&["TCDC_avg_6h"]), None);
    assert_eq!(rounded(&ranges["TCDC_avg_6h"]), [Some(60.0)]);
    let (_, ranges) = series(&engine, instant, Some(&["TMP"]), None);
    assert_eq!(rounded(&ranges["TMP"]), [Some(16.85)]);
    // A pinned run never falls back: the new run has no average.
    let (t, ranges) = series(&engine, None, Some(&["TCDC_avg_6h"]), Some(at(6)));
    assert_eq!(
        (t, ranges["TCDC_avg_6h"].clone()),
        (vec![at(6)], vec![None])
    );

    // An area without a datetime: the last step of the run that has it,
    // where the new run's analysis used to be the 400 of a missing field.
    let area = engine
        .query_area(
            "POLYGON((0 0,1 0,1 1,0 1,0 0))",
            None,
            Some(&["TCDC_avg_6h".to_string()]),
            None,
            None,
        )
        .unwrap();
    let CoverageResponse::Single(area) = area else {
        panic!("expected one grid")
    };
    let values = &area.ranges["TCDC_avg_6h"].values;
    assert!(!values.is_empty());
    assert!(values.iter().all(|v| *v == Some(60.0)), "{values:?}");
    // One the run's last step lacks: its last step carrying it, as a map
    // without TIME renders, not the 400 of a field the last step lacks.
    // TMP alone stays the new run's analysis.
    for expected in [
        &[("TCDC_avg_3h", 30.0)][..],
        &[("TMP", 9.85), ("TCDC_avg_3h", 30.0)][..],
        &[("TMP", 16.85)][..],
    ] {
        let names: Vec<String> = expected.iter().map(|(n, _)| n.to_string()).collect();
        let CoverageResponse::Single(area) = engine
            .query_area(
                "POLYGON((0 0,1 0,1 1,0 1,0 0))",
                None,
                Some(&names),
                None,
                None,
            )
            .unwrap()
        else {
            panic!("expected one grid")
        };
        for &(name, value) in expected {
            let values = &area.ranges[name].values;
            assert!(!values.is_empty());
            assert!(
                rounded(values).iter().all(|v| *v == Some(value)),
                "{names:?}: {name} {values:?}"
            );
        }
    }
}

/// An M trajectory whose first time is the new run's analysis, naming a
/// parameter only the older run has: the older run's step valid then.
#[test]
fn an_m_trajectory_naming_an_older_runs_parameter_reads_that_run() {
    let (_source, engine) = new_run_with_only_its_analysis(None);
    let six = at(6).timestamp();
    let coords = format!("LINESTRING M(0 0.5 {six}, 1 0.5 {six})");
    let query = |names: &[&str]| {
        let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        let CoverageResponse::Single(coverage) = engine
            .query_trajectory(&coords, None, Some(&names), None, None)
            .unwrap()
        else {
            panic!("an M path is one coverage")
        };
        coverage
    };
    let coverage = query(&["RH"]);
    let values = rounded(&coverage.ranges["RH"].values);
    assert!(!values.is_empty());
    assert!(values.iter().all(|v| *v == Some(56.0)), "{values:?}");
    // A parameter the new run has: still its analysis.
    let values = rounded(&query(&["TMP"]).ranges["TMP"].values);
    assert!(values.iter().all(|v| *v == Some(16.85)), "{values:?}");
}

/// A parameter dropped from the newer runs stays advertised until the last
/// run that has it is evicted.
#[test]
fn an_evicted_runs_parameters_are_no_longer_advertised() {
    let (_source, engine) = new_run_with_only_its_analysis(Some(1));
    let info = engine.raster_info_shared();
    assert_eq!(info.reference_times, [at(6)]);
    assert_eq!(names(&info), ["TMP", "TCDC"]);
    assert!(matches!(
        value(&engine, Some("RH"), None, None),
        Err(DataServerError::InvalidParameter(_))
    ));
}

/// A pressure-level view builds its own catalog from the same runs: its
/// parameters are their union too.
#[test]
fn a_pressure_view_keeps_an_older_runs_parameter() {
    let source = TestSource::new();
    let tmp = |kelvin: f32| message(0, kelvin, [0; 4], 100, 85000);
    let rh = |value: f32| parameter_message(1, 1, 0x30, value, [0; 4], 100, 85000);
    for (name, forecast, offset) in [("a0", "anl", 0.0), ("a3", "3 hour fcst", 3.0)] {
        source.write_run(
            name,
            OLD,
            &[
                ("TMP", "850 mb", forecast, tmp(270.0 + offset)),
                ("RH", "850 mb", forecast, rh(70.0 + offset)),
            ],
        );
    }
    source.write_run("b0", NEW, &[("TMP", "850 mb", "anl", tmp(280.0))]);
    let mut config = source.config();
    config.level_types = Some(vec![GribLevelType::Pressure]);
    let owner = GribEngine::new("levels", &config).unwrap();
    let views = owner.level_collections();
    let pressure = &views[0];
    let info = pressure.raster_info_shared();
    assert_eq!(names(&info), ["TMP", "RH"]);
    assert_eq!(info.parameters[1].unit, "%");
    let z = [850.0];
    let CoverageResponse::Single(result) = pressure
        .query_position("POINT(0.5 0.5)", None, Some(&["RH".into()]), Some(&z), None)
        .unwrap()
    else {
        panic!("expected one series")
    };
    assert_eq!(
        rounded(&result.ranges["RH"].values),
        [Some(70.0), Some(73.0)]
    );
    let tile = pressure
        .get_raster_tile(
            [0.0, 0.0, 1.0, 1.0],
            1,
            1,
            None,
            &OutputCrs::Wgs84,
            Some("RH"),
            Some(850.0),
            None,
        )
        .unwrap();
    assert_eq!(tile.values.value_at(0), Some(73.0));
    assert_eq!(
        pressure.resolve_parameter_reference_time(Some("RH"), None, None),
        Some(at(0))
    );
    // A cube without a datetime: the older run's last step.
    let CoverageResponse::Single(cube) = pressure
        .query_cube(
            &ds_core::feature::Bbox::new(0.0, 0.0, 1.0, 1.0).unwrap(),
            None,
            Some(&["RH".into()]),
            Some(&z),
            ds_core::cube::CubeResolution::default(),
            None,
        )
        .unwrap()
    else {
        panic!("expected one grid")
    };
    let values = rounded(&cube.ranges["RH"].values);
    assert!(!values.is_empty());
    assert!(values.iter().all(|v| *v == Some(73.0)), "{values:?}");
}

/// u and v at every step of the 00Z run, f006 included; the 06Z run's
/// analysis has only its u. The v component, from the older run, still
/// pairs with u at the same level, and the derived speed renders both from
/// the run that has both, never the new run's u with the old run's v: also
/// unpinned, at 06Z, where each component alone would pick another run.
#[test]
fn a_component_only_an_older_run_has_still_pairs() {
    let source = TestSource::new();
    let u = |base: f32| parameter_message(2, 2, 0x30, base, [0, 10, 5, 8], 103, 10);
    let v = |base: f32| parameter_message(2, 3, 0x30, base, [0, 8, 4, 10], 103, 10);
    for (name, forecast, shift) in [
        ("a0", "anl", 0.0),
        ("a3", "3 hour fcst", 1.0),
        ("a6", "6 hour fcst", 2.0),
    ] {
        source.write_run(
            name,
            OLD,
            &[
                ("UGRD", "10 m above ground", forecast, u(-5.0 + shift)),
                ("VGRD", "10 m above ground", forecast, v(-4.0 + shift)),
            ],
        );
    }
    source.write_run("b0", NEW, &[("UGRD", "10 m above ground", "anl", u(20.0))]);
    let engine = Arc::new(GribEngine::new("gfs", &source.config()).unwrap());
    let facts = engine.wind_facts();
    let component = |name: &str| facts.parameters.iter().find(|p| p.name == name).unwrap();
    assert_eq!(component("VGRD").level.as_deref(), Some("hag:10"));
    assert_eq!(component("VGRD").unit, component("UGRD").unit);
    assert_eq!(component("VGRD").grib, Some((0, 2, 3)));

    let logged = Arc::new(Mutex::new(Vec::new()));
    let sink = logged.clone();
    let wind = DerivedWind::new(
        "gfs",
        engine.clone(),
        Arc::new(move |id: &str, outcome: &PairOutcome| {
            sink.lock().unwrap().push(format!("{id}: {outcome}"));
        }),
    );
    assert_eq!(
        *logged.lock().unwrap(),
        ["gfs: UGRD/VGRD: derived speed 'WIND' and direction 'WDIR'"]
    );
    assert!(names(&wind.raster_info_shared()).contains(&"WIND"));

    // The run both components share, and their last shared step in it.
    let run = wind.resolve_parameter_reference_time(Some("WIND"), None, None);
    assert_eq!(run, Some(at(0)));
    let valid = wind.resolve_parameter_time(Some("WIND"), None, run);
    assert_eq!(valid, Some(at(6)));
    let speed = value(&wind, Some("WIND"), valid, run).unwrap();
    let component = |name| value(engine.as_ref(), Some(name), valid, run).unwrap();
    let expected = f64::from(component("UGRD")).hypot(f64::from(component("VGRD")));
    assert!(
        (f64::from(speed) - expected).abs() < 1e-5,
        "{speed} vs {expected}"
    );
    // Not the new run's u: it is another field.
    assert_ne!(
        value(engine.as_ref(), Some("UGRD"), None, None).unwrap(),
        component("UGRD")
    );

    // An unpinned render is the keyed one, at every requested time: at
    // 06Z each component alone picks another run (#507/#521).
    assert_eq!(value(&wind, Some("WIND"), None, None).unwrap(), speed);
    for minutes in (-60..=480).step_by(30) {
        let time = at(0) + chrono::Duration::minutes(minutes);
        let run = wind.resolve_parameter_reference_time(Some("WIND"), Some(time), None);
        let valid = wind.resolve_parameter_time(Some("WIND"), Some(time), run);
        match (
            value(&wind, Some("WIND"), Some(time), None),
            value(&wind, Some("WIND"), valid, run),
        ) {
            (Ok(direct), Ok(keyed)) => assert_eq!(direct, keyed, "WIND at {time}"),
            (Err(_), Err(_)) => {}
            (direct, keyed) => panic!("WIND at {time}: {direct:?} vs {keyed:?}"),
        }
    }

    // EDR: the run that has both components.
    let (t, ranges) = series(&wind, None, Some(&["WIND"]), None);
    assert_eq!(t, [at(0), at(3), at(6)]);
    assert!(ranges["WIND"].iter().all(Option::is_some), "{ranges:?}");
}
