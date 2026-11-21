//! A map whose message is missing from the data is the data's fault, a
//! server-class error that the WMS error-tile alert counts (#1506); a map
//! request naming a parameter, level, time or run that does not exist stays
//! the client's error. A pressure view's step missing a level the run has:
//! `parameter_times_tests::a_level_the_step_lacks_is_a_server_error`.

use super::*;
use crate::test_support::{message, parameter_message, TestSource};
use ds_core::error::ErrorClass;

fn at(hour: i64) -> DateTime<Utc> {
    "2026-04-05T00:00:00Z".parse::<DateTime<Utc>>().unwrap() + chrono::Duration::hours(hour)
}

/// A 1×1 map of `parameter` at `time`, level `z` and run `reference_time`.
fn map(
    engine: &GribEngine,
    parameter: Option<&str>,
    time: Option<DateTime<Utc>>,
    z: Option<f64>,
    reference_time: Option<DateTime<Utc>>,
) -> Result<RasterTile, DataServerError> {
    engine.get_raster_tile(
        [0.0, 0.0, 1.0, 1.0],
        1,
        1,
        time,
        &OutputCrs::Wgs84,
        parameter,
        z,
        reference_time,
    )
}

/// The class of a map that fails.
fn failure(result: Result<RasterTile, DataServerError>) -> (ErrorClass, String) {
    match result {
        Err(e) => (e.class(), e.to_string()),
        Ok(_) => panic!("the map draws"),
    }
}

/// A surface view whose second step lost its canonical 2 m temperature,
/// keeping only the 2 hPa field: the default layer there has no message to
/// draw, an incomplete step file. It used to be a 404 (WMS `not_found`).
#[test]
fn a_step_missing_its_default_product_is_a_server_error() {
    let source = TestSource::new();
    source.write(
        "f000",
        &[
            ("TMP", "2 mb", message(0, 200.0, [0; 4], 100, 200)),
            ("TMP", "2 m above ground", message(0, 280.0, [0; 4], 103, 2)),
        ],
        0,
    );
    source.write(
        "f001",
        &[("TMP", "2 mb", message(0, 200.0, [0; 4], 100, 200))],
        1,
    );
    let engine = GribEngine::new("faults", &source.config()).unwrap();

    let (class, message) = failure(map(&engine, None, Some(at(1)), None, None));
    assert_eq!(class, ErrorClass::Internal, "{message}");
    assert!(message.contains("incomplete step file"), "{message}");
    assert!(message.contains("f001.grib2"), "{message}");
    // The step that has it draws, and a named parameter snaps to it (#1005).
    assert!(map(&engine, None, Some(at(0)), None, None).is_ok());
    assert!(map(&engine, Some("TMP"), Some(at(1)), None, None).is_ok());

    // What a client names that does not exist stays the client's error.
    let client = [
        (
            "a parameter never advertised",
            map(&engine, Some("NOPE"), Some(at(0)), None, None),
            ErrorClass::BadRequest,
        ),
        (
            "a time outside the data",
            map(&engine, Some("TMP"), Some(at(240)), None, None),
            ErrorClass::BadRequest,
        ),
        (
            "the default layer at a time outside the data",
            map(&engine, None, Some(at(240)), None, None),
            ErrorClass::BadRequest,
        ),
        (
            "a level on a view without levels",
            map(&engine, Some("TMP"), Some(at(0)), Some(850.0), None),
            ErrorClass::BadRequest,
        ),
        (
            "a run that does not exist",
            map(&engine, Some("TMP"), Some(at(0)), None, Some(at(-6))),
            ErrorClass::NotFound,
        ),
    ];
    for (what, result, expected) in client {
        let (class, message) = failure(result);
        assert_eq!(class, expected, "{what}: {message}");
    }
}

/// A parameter the collection advertises that the newest run lacks draws
/// from the older run that has it (#1026), never an error; only a run
/// selected for a parameter yet lacking it, which the selection rules out,
/// would be one, and then the data's fault: an `Engine` error, where it was
/// a 400 (#1506).
#[test]
fn an_advertised_parameter_missing_from_the_run_drawn_is_a_server_error() {
    let source = TestSource::new();
    let air = |kelvin: f32| message(0, kelvin, [0; 4], 103, 2);
    let cloud = |percent: f32| parameter_message(6, 1, 0x30, percent, [0; 4], 10, 0);
    source.write_run(
        "a000",
        "2026040500",
        &[
            ("TMP", "2 m above ground", "anl", air(280.0)),
            ("TCDC", "entire atmosphere", "anl", cloud(50.0)),
        ],
    );
    source.write_run(
        "b000",
        "2026040506",
        &[("TMP", "2 m above ground", "anl", air(281.0))],
    );
    let engine = GribEngine::new("faults", &source.config()).unwrap();
    let tile = map(&engine, Some("TCDC"), None, None, None).unwrap();
    assert_eq!(tile.values.value_at(0), Some(50.0));
    assert_eq!(
        engine.resolve_parameter_reference_time(Some("TCDC"), None, None),
        Some(at(0))
    );

    let catalog = engine.catalog();
    let newest = catalog.runs.values().next_back().unwrap();
    let step_file = newest.steps.values().next().unwrap();
    let missing = canonical_message(step_file, "TCDC", None, None).unwrap_err();
    assert!(matches!(missing, DataServerError::Engine(_)), "{missing:?}");
    assert_eq!(missing.class(), ErrorClass::Internal);
}
