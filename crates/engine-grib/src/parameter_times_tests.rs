//! Per-parameter time axes and parameter-aware map selection (#1005): an
//! hour-window aggregate exists only at some steps of a run, so its map
//! layer advertises those steps and renders, and keys, a step that has it.

use super::*;
use crate::test_support::{message, parameter_message, TestSource};
use ds_core::map_engine::default_request_time;

fn at(hour: i64) -> DateTime<Utc> {
    "2026-04-05T00:00:00Z".parse::<DateTime<Utc>>().unwrap() + chrono::Duration::hours(hour)
}

/// Total cloud cover (WMO 0/6/1, %), the same value at every node.
fn cloud(value: f32) -> Vec<u8> {
    parameter_message(6, 1, 0x30, value, [0; 4], 10, 0)
}

/// Two runs, A (00Z) and B (06Z), each with f000, f003 and f006. `TMP` and
/// the instantaneous `TCDC` are at every step. The GFS-style averages
/// restart every six hours: `TCDC_avg_3h` at f003, `TCDC_avg_6h` at f006,
/// neither at the analysis. Only B has a 2 m maximum, at f003 and f006.
/// Valid times 00, 03, 06 (A f006 and B f000), 09 and 12.
fn two_runs() -> (TestSource, GribEngine) {
    let source = TestSource::new();
    let air = |kelvin: f32| message(0, kelvin, [0; 4], 103, 2);
    let step =
        |name: &str, date: &str, forecast: &str, offset: f32, extra: &[(&str, &str, f32)]| {
            let mut records = vec![
                ("TMP", "2 m above ground", forecast, air(280.0 + offset)),
                ("TCDC", "entire atmosphere", forecast, cloud(offset)),
            ];
            for &(param, window, value) in extra {
                let field = match param {
                    "TMAX" => air(value),
                    _ => cloud(value),
                };
                let level = match param {
                    "TMAX" => "2 m above ground",
                    _ => "entire atmosphere",
                };
                records.push((param, level, window, field));
            }
            source.write_run(name, date, &records);
        };
    let (a, b) = ("2026040500", "2026040506");
    step("a0", a, "anl", 0.0, &[]);
    step(
        "a3",
        a,
        "3 hour fcst",
        3.0,
        &[("TCDC", "0-3 hour ave fcst", 30.0)],
    );
    step(
        "a6",
        a,
        "6 hour fcst",
        6.0,
        &[("TCDC", "0-6 hour ave fcst", 60.0)],
    );
    step("b0", b, "anl", 10.0, &[]);
    step(
        "b3",
        b,
        "3 hour fcst",
        13.0,
        &[
            ("TCDC", "0-3 hour ave fcst", 130.0),
            ("TMAX", "0-3 hour max fcst", 300.0),
        ],
    );
    step(
        "b6",
        b,
        "6 hour fcst",
        16.0,
        &[
            ("TCDC", "0-6 hour ave fcst", 160.0),
            ("TMAX", "0-6 hour max fcst", 301.0),
        ],
    );
    let engine = GribEngine::new("windows", &source.config()).unwrap();
    (source, engine)
}

/// The one value of a map tile `engine` renders.
fn value(
    engine: &GribEngine,
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

#[test]
fn aggregates_advertise_only_the_steps_that_carry_them() {
    let (_source, engine) = two_runs();
    let info = engine.raster_info_shared();
    assert_eq!(info.times, [at(0), at(3), at(6), at(9), at(12)]);
    assert_eq!(info.reference_times, [at(0), at(6)]);
    let times = |name: &str| engine.parameter_times(name).map(|t| t.to_vec());
    assert_eq!(times("TCDC_avg_3h"), Some(vec![at(3), at(9)]));
    assert_eq!(times("TCDC_avg_6h"), Some(vec![at(6), at(12)]));
    assert_eq!(times("TMAX"), Some(vec![at(9), at(12)]));
    // At every step: the collection's axis.
    assert_eq!(times("TMP"), None);
    assert_eq!(times("TCDC"), None);
    assert_eq!(times("unknown"), None);
    // One snapshot: an `Arc` clone per call, never rebuilt.
    assert!(Arc::ptr_eq(
        &engine.parameter_times("TCDC_avg_3h").unwrap(),
        &engine.parameter_times("TCDC_avg_3h").unwrap()
    ));
    assert_eq!(
        engine.get_parameter_available_times("TCDC_avg_3h"),
        Some(vec![at(3), at(9)])
    );
    assert_eq!(engine.get_parameter_available_times("TMP"), None);
    // What WMS, Maps and Tiles render with TIME omitted.
    assert_eq!(
        default_request_time(&engine, &info, Some("TCDC_avg_3h")),
        Some(at(9))
    );
    assert_eq!(
        default_request_time(&engine, &info, Some("TMP")),
        Some(at(12))
    );
}

#[test]
fn a_map_renders_the_nearest_step_that_carries_the_parameter() {
    let (_source, engine) = two_runs();
    let check = |parameter, time: DateTime<Utc>, run, step: i64, expected: f32| {
        let resolved = engine.resolve_parameter_reference_time(parameter, Some(time), None);
        assert_eq!(resolved, Some(run), "{parameter:?} at {time}: run");
        let valid = engine.resolve_parameter_time(parameter, Some(time), resolved);
        assert_eq!(valid, Some(at(step)), "{parameter:?} at {time}: step");
        // The key names what renders: the request as given, and the
        // resolved instant pinned to the resolved run.
        assert_eq!(
            value(&engine, parameter, Some(time), None).unwrap(),
            expected
        );
        assert_eq!(
            value(&engine, parameter, valid, resolved).unwrap(),
            expected
        );
    };
    // 06Z: the newest run (B f000) for the default layer and `TMP`, but B
    // has no 6 h average then: A's f006 has it.
    check(None, at(6), at(6), 6, 16.85);
    check(Some("TMP"), at(6), at(6), 6, 16.85);
    check(Some("TCDC_avg_6h"), at(6), at(0), 6, 60.0);
    // A time some steps lack snaps to the nearest that carries it, across
    // runs: never the error tile, never the analysis that lacks it.
    check(Some("TCDC_avg_6h"), at(0), at(0), 6, 60.0);
    check(Some("TCDC_avg_6h"), at(10), at(6), 12, 160.0);
    check(Some("TCDC_avg_3h"), at(3), at(0), 3, 30.0);
    check(Some("TCDC_avg_3h"), at(4), at(0), 3, 30.0);
    check(Some("TCDC_avg_3h"), at(8), at(6), 9, 130.0);
    check(Some("TMAX"), at(0), at(6), 9, 26.85);
    check(Some("TMAX"), at(12), at(6), 12, 27.85);
    // Equally near a step of each run: the newer run.
    check(Some("TCDC_avg_3h"), at(6), at(6), 9, 130.0);
    check(Some("TCDC_avg_6h"), at(9), at(6), 12, 160.0);
    // The default layer is unchanged: B's nearest step.
    check(None, at(8), at(6), 9, 19.85);

    // TIME omitted at the engine: the newest run carrying it, its last step.
    assert_eq!(
        value(&engine, Some("TCDC_avg_3h"), None, None).unwrap(),
        130.0
    );
    assert_eq!(
        engine.resolve_parameter_time(Some("TCDC_avg_3h"), None, None),
        Some(at(9))
    );
    assert_eq!(
        engine.resolve_parameter_reference_time(Some("TCDC_avg_3h"), None, None),
        Some(at(6))
    );

    // A pinned run stays pinned: its own nearest carrying step.
    assert_eq!(
        engine.resolve_parameter_time(Some("TCDC_avg_3h"), Some(at(6)), Some(at(0))),
        Some(at(3))
    );
    assert_eq!(
        value(&engine, Some("TCDC_avg_3h"), Some(at(6)), Some(at(0))).unwrap(),
        30.0
    );
    // A run without the parameter is missing data, a 404, not a bad request.
    assert!(matches!(
        value(&engine, Some("TMAX"), Some(at(3)), Some(at(0))),
        Err(DataServerError::LocationNotFound(_))
    ));
    assert_eq!(
        engine.resolve_parameter_reference_time(Some("TMAX"), Some(at(3)), Some(at(0))),
        Some(at(0)),
        "a failed selection echoes the pinned run"
    );
    // A parameter no run has is a bad request.
    assert!(matches!(
        value(&engine, Some("nope"), Some(at(3)), None),
        Err(DataServerError::InvalidParameter(_))
    ));
    // Outside the data: the default layer's error, and the request echoed.
    let tomorrow = at(24);
    assert!(matches!(
        value(&engine, Some("TCDC_avg_3h"), Some(tomorrow), None),
        Err(DataServerError::InvalidParameter(_))
    ));
    assert_eq!(
        engine.resolve_parameter_time(Some("TCDC_avg_3h"), Some(tomorrow), None),
        Some(tomorrow)
    );
}

/// The #507/#521 invariant at every requested time: the pixels rendered
/// for a request are the pixels rendered at the resolved (time, run), the
/// key the API layers cache them under.
#[test]
fn resolved_keys_render_the_same_pixels_as_the_request() {
    let (_source, engine) = two_runs();
    let parameters = [
        None,
        Some("TMP"),
        Some("TCDC_avg_3h"),
        Some("TCDC_avg_6h"),
        Some("TMAX"),
    ];
    for parameter in parameters {
        for minutes in (-60..=780).step_by(30) {
            let time = at(0) + chrono::Duration::minutes(minutes);
            let run = engine.resolve_parameter_reference_time(parameter, Some(time), None);
            let valid = engine.resolve_parameter_time(parameter, Some(time), run);
            let direct = value(&engine, parameter, Some(time), None);
            let keyed = value(&engine, parameter, valid, run);
            match (direct, keyed) {
                (Ok(direct), Ok(keyed)) => {
                    assert_eq!(direct, keyed, "{parameter:?} at {time}")
                }
                (Err(_), Err(_)) => {}
                (direct, keyed) => panic!("{parameter:?} at {time}: {direct:?} vs {keyed:?}"),
            }
        }
    }
}

/// Bands rendered together come from one step that carries them all.
#[test]
fn several_parameters_share_one_carrying_step() {
    let (_source, engine) = two_runs();
    let tiles = |parameters: &[&str], time| {
        engine.get_raster_tiles(
            [0.0, 0.0, 1.0, 1.0],
            1,
            1,
            time,
            &OutputCrs::Wgs84,
            parameters,
            None,
            None,
        )
    };
    let values = |parameters: &[&str], time| -> Vec<f32> {
        tiles(parameters, time)
            .unwrap()
            .iter()
            .map(|t| t.values.value_at(0).unwrap() as f32)
            .collect()
    };
    // TMP alone at 09Z is B f003; with the 6 h average, only f006 steps
    // have both, and B's is the newer of the two equally near.
    assert_eq!(
        engine.resolve_parameters_time(&["TMP", "TCDC_avg_6h"], Some(at(9)), None),
        Some(at(12))
    );
    assert_eq!(values(&["TMP", "TCDC_avg_6h"], Some(at(9))), [22.85, 160.0]);
    assert_eq!(values(&["TCDC_avg_6h", "TMP"], Some(at(7))), [60.0, 12.85]);
    assert_eq!(values(&["TCDC_avg_6h", "TMP"], None), [160.0, 22.85]);
    assert!(tiles(&[], Some(at(9))).unwrap().is_empty());
    // No step has both windows.
    assert!(matches!(
        tiles(&["TCDC_avg_3h", "TCDC_avg_6h"], Some(at(6))),
        Err(DataServerError::LocationNotFound(_))
    ));
}

/// A pressure view carries a parameter at a step that has it at any
/// level; a level that step lacks is missing data, a 404.
#[test]
fn a_level_the_step_lacks_is_not_found() {
    let source = TestSource::new();
    let level = |hpa: u32, kelvin: f32| message(0, kelvin, [0; 4], 100, hpa * 100);
    source.write(
        "f000",
        &[
            ("TMP", "850 mb", level(850, 270.0)),
            ("TMP", "500 mb", level(500, 250.0)),
        ],
        0,
    );
    source.write("f003", &[("TMP", "850 mb", level(850, 271.0))], 3);
    let mut config = source.config();
    config.level_types = Some(vec![GribLevelType::Pressure]);
    let owner = GribEngine::new("levels", &config).unwrap();
    let views = owner.level_collections();
    let pressure = &views[0];
    assert_eq!(pressure.parameter_times("TMP"), None);
    let render = |z| {
        pressure.get_raster_tile(
            [0.0, 0.0, 1.0, 1.0],
            1,
            1,
            Some(at(3)),
            &OutputCrs::Wgs84,
            Some("TMP"),
            Some(z),
            None,
        )
    };
    assert_eq!(
        render(850.0).unwrap().values.value_at(0).unwrap() as f32,
        -2.15
    );
    assert!(matches!(
        render(500.0),
        Err(DataServerError::LocationNotFound(_))
    ));
}

/// The real NOAA GFS sidecars of one run (`testdata/gfs`, 2026-04-08 00Z
/// f000, f003 and f006): averages and maxima are absent from the analysis,
/// and each window length exists only where the step ends one. Indexes
/// only: the data files are not committed and nothing is read.
#[test]
fn real_gfs_windows_have_their_own_axes() {
    let source = TestSource::new();
    for step in ["f000", "f003", "f006"] {
        let name = format!("gfs.t00z.pgrb2.0p25.{step}.idx");
        std::fs::copy(
            format!("{}/../../testdata/gfs/{name}", env!("CARGO_MANIFEST_DIR")),
            source.dir.join(&name),
        )
        .unwrap();
    }
    let config: GribConfig = serde_json::from_value(serde_json::json!({
        "data_path": source.dir.to_str().unwrap(),
        "index_format": "wgrib2", "index_suffix": ".idx", "data_suffix": "",
        "parameters": ["TMP", "TMAX", "PRATE", "TCDC"]
    }))
    .unwrap();
    let engine = GribEngine::new("gfs", &config).unwrap();
    let run: DateTime<Utc> = "2026-04-08T00:00:00Z".parse().unwrap();
    let step = |hours: i64| run + chrono::Duration::hours(hours);
    let info = engine.raster_info_shared();
    assert_eq!(info.times, [step(0), step(3), step(6)]);
    let times = |name: &str| engine.parameter_times(name).map(|t| t.to_vec());
    for name in ["TMP", "PRATE", "TCDC"] {
        assert_eq!(times(name), None, "{name} is at every step");
    }
    assert_eq!(times("TMAX"), Some(vec![step(3), step(6)]));
    for name in ["PRATE_avg_3h", "TCDC_avg_3h"] {
        assert_eq!(times(name), Some(vec![step(3)]), "{name}");
    }
    for name in ["PRATE_avg_6h", "TCDC_avg_6h"] {
        assert_eq!(times(name), Some(vec![step(6)]), "{name}");
    }
    // The analysis has no maximum or average: a map of one at f000 keys
    // the nearest step that has it.
    for (name, hours) in [("TMAX", 3), ("PRATE_avg_3h", 3), ("PRATE_avg_6h", 6)] {
        assert_eq!(
            engine.resolve_parameter_time(Some(name), Some(step(0)), None),
            Some(step(hours)),
            "{name}"
        );
        assert_eq!(
            engine.resolve_parameter_reference_time(Some(name), Some(step(0)), None),
            Some(run)
        );
        assert_eq!(
            default_request_time(&engine, &info, Some(name)),
            times(name).unwrap().last().copied()
        );
    }
    assert_eq!(
        engine.resolve_parameter_time(Some("PRATE_avg_3h"), Some(step(6)), None),
        Some(step(3))
    );
    assert_eq!(engine.resolve_time(Some(step(0)), None), Some(step(0)));
}
