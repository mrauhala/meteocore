use super::*;
use crate::test_support::{message, TestSource};
use chrono::TimeZone;

/// Bytes of one fixture message, read once per field with the cache off.
const FIELD_BYTES: u64 = 183;

const LEVELS: [u32; 3] = [1000, 850, 500];
const STEPS: [u32; 2] = [0, 6];

fn run_time() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 4, 5, 0, 0, 0).unwrap()
}

fn at(step: u32) -> DateTime<Utc> {
    run_time() + chrono::Duration::hours(i64::from(step))
}

/// Distinct per field, so every output slot proves where it came from.
fn base(param: usize, level: usize, step: usize) -> f32 {
    (if param == 0 { 250.0 } else { 40.0 }) + (level * 10 + step) as f32
}

/// The 2×2 fixture grid over [0, 0, 1, 1] packs `[0, 2, 4, 6]` north row
/// first; the cube's y axis ascends, so its row-major cells are these.
const CELLS: [f64; 4] = [4.0, 6.0, 0.0, 2.0];

/// `P0` is temperature (K, served in °C), `P1` relative humidity (%), each
/// at three pressure levels and two steps, one file per field. `family`
/// `None` is the legacy single-collection layout.
fn fixture(family: Option<GribLevelType>) -> (TestSource, GribEngine) {
    let source = TestSource::new();
    for (param, name) in ["P0", "P1"].into_iter().enumerate() {
        for (level, value) in LEVELS.into_iter().enumerate() {
            for (step, hours) in STEPS.into_iter().enumerate() {
                let mut bytes =
                    message(0, base(param, level, step), [0, 2, 4, 6], 100, value * 100);
                if param == 1 {
                    bytes[118] = 1;
                    bytes[119] = 1;
                }
                source.write(
                    &format!("{name}-{value}-{hours}"),
                    &[
                        (name, &format!("{value} mb"), bytes),
                        // Explicit length for the selected field: no tail HEAD.
                        ("TAIL", "surface", message(0, 0.0, [0; 4], 1, 0)),
                    ],
                    hours,
                );
            }
        }
    }
    let mut config = source.config();
    config.grid_cache_mb = 0;
    config.level_types = family.map(|family| vec![family]);
    config.parameters = Some(vec!["P0".into(), "P1".into()]);
    let owner = GribEngine::new("cube", &config).unwrap();
    let engine = match family {
        Some(_) => owner.level_collections().into_iter().next().unwrap(),
        None => owner,
    };
    (source, engine)
}

fn grid(response: CoverageResponse) -> QueryResult {
    match response {
        CoverageResponse::Single(result) => result,
        CoverageResponse::Collection(_) => panic!("expected one grid"),
    }
}

fn bbox(west: f64, south: f64, east: f64, north: f64) -> Bbox {
    Bbox::new(west, south, east, north).unwrap()
}

fn expected(param: usize, level: usize, step: usize, cell: f64) -> f64 {
    f64::from(base(param, level, step)) + cell - if param == 0 { 273.15 } else { 0.0 }
}

fn assert_close(actual: Option<f64>, expected: Option<f64>) {
    match (actual, expected) {
        (Some(a), Some(e)) => assert!((a - e).abs() < 1e-9, "{a} != {e}"),
        (None, None) => {}
        _ => panic!("{actual:?} != {expected:?}"),
    }
}

#[test]
fn cube_is_steps_by_levels_by_the_native_bbox() {
    let (_source, engine) = fixture(Some(GribLevelType::Pressure));
    let probed = engine.storage_bytes_read(); // discovery's header probes
    let names = ["P1".to_string(), "P0".to_string()];
    let result = grid(
        engine
            .query_cube(
                &bbox(0.0, 0.0, 1.0, 1.0),
                Some((at(0), at(6))),
                Some(&names),
                None,
                CubeResolution::default(),
                None,
            )
            .unwrap(),
    );
    let DomainDescription::Grid {
        x,
        y,
        t: Some(t),
        z: Some(z),
    } = result.domain
    else {
        panic!("expected a Grid with t and z")
    };
    assert_eq!((x, y), (vec![0.0, 1.0], vec![0.0, 1.0]));
    assert_eq!(t, [at(0), at(6)]);
    // Every level, bottom first, as the collection advertises them.
    assert_eq!(z.values, [1000.0, 850.0, 500.0]);
    assert_eq!(z.kind, ds_core::vertical::VerticalKind::Pressure);
    for (param, name) in ["P0", "P1"].into_iter().enumerate() {
        let range = &result.ranges[name];
        assert_eq!(range.shape, [2, 3, 2, 2]);
        assert_eq!(range.axis_names, ["t", "z", "y", "x"]);
        for step in 0..2 {
            for level in 0..3 {
                for (cell, offset) in CELLS.into_iter().enumerate() {
                    assert_close(
                        range.values[(step * 3 + level) * 4 + cell],
                        Some(expected(param, level, step, offset)),
                    );
                }
            }
        }
    }
    assert_eq!(result.parameters["P0"].unit, "°C");
    assert_eq!(result.parameters["P1"].unit, "%");
    // Each field once: 2 parameters × 3 levels × 2 steps.
    assert_eq!(engine.storage_bytes_read() - probed, 12 * FIELD_BYTES);
}

#[test]
fn resolution_resamples_by_nearest_neighbour_and_reads_only_sampled_levels() {
    let (_source, engine) = fixture(Some(GribLevelType::Pressure));
    let probed = engine.storage_bytes_read(); // discovery's header probes
    let result = grid(
        engine
            .query_cube(
                // One cell past the grid either side of x.
                &bbox(-1.0, 0.0, 2.0, 1.0),
                Some((at(6), at(6))),
                Some(&["P0".to_string()]),
                None,
                CubeResolution {
                    x: Some(4),
                    y: Some(3),
                    z: Some(2),
                },
                None,
            )
            .unwrap(),
    );
    let DomainDescription::Grid {
        x,
        y,
        t: Some(t),
        z: Some(z),
    } = result.domain
    else {
        panic!("expected a Grid with t and z")
    };
    // The axes are the requested positions, both ends included.
    assert_eq!(x, [-1.0, 0.0, 1.0, 2.0]);
    assert_eq!(y, [0.0, 0.5, 1.0]);
    assert_eq!(z.values, [500.0, 1000.0]);
    assert_eq!(t, [at(6)]);
    let range = &result.ranges["P0"];
    assert_eq!(range.shape, [1, 2, 3, 4]);
    // x −1 and 2 are a whole cell off the grid: missing, not the edge value.
    // y 0.5 ties between the rows and takes the lower (southern) one.
    let rows = [[4.0, 6.0], [4.0, 6.0], [0.0, 2.0]];
    for (zi, level) in [2, 0].into_iter().enumerate() {
        for (yi, row) in rows.iter().enumerate() {
            for (xi, cell) in [None, Some(row[0]), Some(row[1]), None]
                .into_iter()
                .enumerate()
            {
                assert_close(
                    range.values[(zi * 3 + yi) * 4 + xi],
                    cell.map(|cell| expected(0, level, 1, cell)),
                );
            }
        }
    }
    // 850 hPa is between the sampled positions and is never read.
    assert_eq!(engine.storage_bytes_read() - probed, 2 * FIELD_BYTES);
}

#[test]
fn budget_antimeridian_and_invalid_selections_fail_before_reading() {
    let (_source, engine) = fixture(Some(GribLevelType::Pressure));
    let probed = engine.storage_bytes_read(); // discovery's header probes
    let cube = |bbox: Bbox, datetime, names: &[&str], z: Option<&[f64]>, resolution| {
        let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        engine.query_cube(&bbox, datetime, Some(&names), z, resolution, None)
    };
    let fine = CubeResolution {
        x: Some(1000),
        y: Some(1000),
        z: None,
    };
    assert!(matches!(
        cube(bbox(0.0, 0.0, 1.0, 1.0), None, &["P0"], None, fine),
        Err(DataServerError::QueryTooLarge(m)) if m.contains("3 levels")
    ));
    let seam = cube(
        bbox(170.0, 10.0, -170.0, 20.0),
        None,
        &["P0"],
        None,
        CubeResolution::default(),
    );
    assert!(
        matches!(&seam, Err(DataServerError::InvalidParameter(m)) if m.contains("antimeridian")),
        "{seam:?}"
    );
    let default = CubeResolution::default();
    for result in [
        cube(
            bbox(0.0, 0.0, 1.0, 1.0),
            None,
            &["P0"],
            Some(&[925.0]),
            default,
        ),
        cube(bbox(0.0, 0.0, 1.0, 1.0), None, &["NOPE"], None, default),
        cube(
            bbox(0.0, 0.0, 1.0, 1.0),
            Some((at(1), at(5))),
            &["P0"],
            None,
            default,
        ),
    ] {
        assert!(
            matches!(result, Err(DataServerError::InvalidParameter(_))),
            "{result:?}"
        );
    }
    assert!(matches!(
        engine.query_cube(
            &bbox(0.0, 0.0, 1.0, 1.0),
            None,
            None,
            None,
            default,
            Some(at(12)),
        ),
        Err(DataServerError::ReferenceTimeNotFound(_))
    ));
    assert_eq!(engine.storage_bytes_read(), probed);
    // Off the grid: known only once the first field gives the geometry.
    assert!(matches!(
        cube(bbox(10.0, 10.0, 20.0, 20.0), None, &["P0"], None, default),
        Err(DataServerError::LocationNotFound(_))
    ));
}

#[test]
fn datetime_selects_a_step_or_every_step_of_an_interval() {
    let (_source, engine) = fixture(Some(GribLevelType::Pressure));
    let times = |datetime, reference_time| {
        let result = grid(
            engine
                .query_cube(
                    &bbox(0.0, 0.0, 1.0, 1.0),
                    datetime,
                    Some(&["P1".to_string()]),
                    Some(&[850.0]),
                    CubeResolution::default(),
                    reference_time,
                )
                .unwrap(),
        );
        match result.domain {
            DomainDescription::Grid { t: Some(t), .. } => t,
            _ => panic!("expected a t axis"),
        }
    };
    // No datetime: the run's last step, as an area query answers.
    assert_eq!(times(None, None), [at(6)]);
    // An instant snaps to the nearest step.
    assert_eq!(times(Some((at(2), at(2))), None), [at(0)]);
    assert_eq!(times(Some((at(5), at(5))), None), [at(6)]);
    // An interval takes every step inside it, open ends included.
    assert_eq!(times(Some((at(0), at(6))), None), [at(0), at(6)]);
    assert_eq!(
        times(Some((DateTime::<Utc>::MIN_UTC, at(3))), None),
        [at(0)]
    );
    assert_eq!(
        times(Some((at(3), DateTime::<Utc>::MAX_UTC)), None),
        [at(6)]
    );
    // The same through the run's instance.
    let open_start = Some((DateTime::<Utc>::MIN_UTC, at(3)));
    assert_eq!(times(open_start, Some(run_time())), [at(0)]);
}

#[test]
fn cube_is_offered_only_by_vertical_views() {
    for family in [None, Some(GribLevelType::Single)] {
        let source = TestSource::new();
        source.write(
            "surface",
            &[
                ("T2", "2 m above ground", message(0, 280.0, [0; 4], 103, 2)),
                ("TAIL", "surface", message(0, 0.0, [0; 4], 1, 0)),
            ],
            0,
        );
        let mut config = source.config();
        config.level_types = family.map(|family| vec![family]);
        let owner = GribEngine::new("single", &config).unwrap();
        let engine = match family {
            Some(_) => owner.level_collections().into_iter().next().unwrap(),
            None => owner,
        };
        assert!(!engine.supported_query_types().contains(&"cube".into()));
        assert!(matches!(
            engine.query_cube(
                &bbox(0.0, 0.0, 1.0, 1.0),
                None,
                None,
                None,
                CubeResolution::default(),
                None
            ),
            Err(DataServerError::InvalidParameter(_))
        ));
    }
    let (_source, engine) = fixture(Some(GribLevelType::Pressure));
    assert!(engine.supported_query_types().contains(&"cube".into()));
}
