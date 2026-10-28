//! One frame's map cache-key version (#1334): the identity of the message
//! the render draws, so a scan that adds steps or runs a frame does not draw
//! from keeps its cached tiles. The late files that do change a frame's
//! message are pinned in `vertical.rs`.

use super::*;
use crate::test_support::{message, parameter_message, TestSource};

fn at(hour: i64) -> DateTime<Utc> {
    "2026-04-05T00:00:00Z".parse::<DateTime<Utc>>().unwrap() + chrono::Duration::hours(hour)
}

const OLD: &str = "2026040500";
const NEW: &str = "2026040506";

/// One step file of `date`: `TMP` at 2 m and `TCDC`.
fn step(source: &TestSource, name: &str, date: &str, forecast: &str, kelvin: f32) {
    source.write_run(
        name,
        date,
        &[
            (
                "TMP",
                "2 m above ground",
                forecast,
                message(0, kelvin, [0; 4], 103, 2),
            ),
            (
                "TCDC",
                "entire atmosphere",
                forecast,
                parameter_message(6, 1, 0x30, kelvin - 270.0, [0; 4], 10, 0),
            ),
        ],
    );
}

/// The run, time and version of the frame a map API draws for `parameter`
/// at `time`: resolved, then versioned, as WMS, Maps and Tiles do.
fn frame(
    engine: &GribEngine,
    parameter: Option<&str>,
    time: DateTime<Utc>,
) -> (Option<DateTime<Utc>>, Option<DateTime<Utc>>, u64) {
    let run = engine.resolve_parameter_reference_time(parameter, Some(time), None);
    let time = engine.resolve_parameter_time(parameter, Some(time), run);
    let version = engine.content_version_for(parameter, time, run, None);
    (run, time, version)
}

#[test]
fn a_scan_adding_other_steps_and_runs_keeps_each_frames_version() {
    let source = TestSource::new();
    step(&source, "old-f000", OLD, "anl", 280.0);
    step(&source, "old-f003", OLD, "3 hour fcst", 283.0);
    let engine = GribEngine::new("frames", &source.config()).unwrap();
    let frames = [
        (Some("TMP"), at(0)),
        (Some("TMP"), at(3)),
        (Some("TCDC"), at(3)),
        // The default layer.
        (None, at(3)),
    ];
    let before: Vec<_> = frames
        .iter()
        .map(|&(parameter, time)| frame(&engine, parameter, time))
        .collect();
    for (&(parameter, _), &(run, time, version)) in frames.iter().zip(&before) {
        assert_eq!(run, Some(at(0)), "{parameter:?}");
        assert_ne!(version, 0, "explicit-TIME responses must revalidate");
        assert_ne!(version, engine.content_version(), "{parameter:?}");
        // The render reads the frame the version names.
        engine
            .get_raster_tile(
                [0.0, 0.0, 1.0, 1.0],
                1,
                1,
                time,
                &OutputCrs::Wgs84,
                parameter,
                None,
                run,
            )
            .unwrap();
    }
    // Each frame draws its own message.
    let versions: HashSet<u64> = before[..3].iter().map(|&(.., v)| v).collect();
    assert_eq!(versions.len(), 3);
    // The default layer draws TMP, the step's first near-surface product.
    assert_eq!(before[3], before[1]);

    // Another time of the same run, and a newer run that does not cover
    // the frames' valid times.
    let catalog = engine.content_version();
    step(&source, "old-f006", OLD, "6 hour fcst", 286.0);
    step(&source, "new-f000", NEW, "anl", 290.0);
    engine.scan_once().unwrap();
    assert_eq!(
        engine.raster_info_shared().reference_times,
        [at(0), at(6)],
        "both scanned"
    );
    assert_ne!(
        engine.content_version(),
        catalog,
        "the catalog's version moved: keyed on it, every frame missed"
    );
    for (&(parameter, time), expected) in frames.iter().zip(&before) {
        assert_eq!(frame(&engine, parameter, time), *expected, "{parameter:?}");
    }
    // The newer run's analysis is a frame of its own.
    let (run, _, version) = frame(&engine, Some("TMP"), at(6));
    assert_eq!(run, Some(at(6)));
    assert!(!versions.contains(&version));
}

/// A frame the render cannot draw keys nothing: its version is the
/// catalog's, still nonzero.
#[test]
fn a_frame_that_fails_to_render_has_the_catalogs_version() {
    let source = TestSource::new();
    step(&source, "old-f000", OLD, "anl", 280.0);
    let engine = GribEngine::new("frames", &source.config()).unwrap();
    let version = |parameter, z| engine.content_version_for(parameter, Some(at(0)), None, z);
    let catalog = engine.content_version();
    assert_ne!(catalog, 0);
    // A parameter no run has, and a level on a view without levels.
    assert_eq!(version(Some("RH"), None), catalog);
    assert_eq!(version(Some("TMP"), Some(850.0)), catalog);
    assert_ne!(version(Some("TMP"), None), catalog);
}
