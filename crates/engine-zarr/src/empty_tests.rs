//! A store with no data yet (#1499): a time axis of length zero, as a
//! fresh store waiting for its first append is. Every EDR query type and map
//! render answers [`DataServerError::no_data_yet`], a 404 a `datetime` list
//! skips, never the `Engine` 500 the API layers log at ERROR.

use super::*;
use ds_core::error::ErrorClass;
use ds_core::map_engine::{MapEngine, OutputCrs};

/// A Zarr v2 store of `temp` on a 2 × 2 lat/lon grid over a `time` axis of
/// length zero: metadata only, no chunk written.
fn write_empty_store(path: &std::path::Path) {
    std::fs::write(path.join(".zgroup"), br#"{"zarr_format":2}"#).unwrap();
    let array = |name: &str, shape: &[u64], chunks: &[u64], attributes: serde_json::Value| {
        let dir = path.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let metadata = serde_json::json!({
            "zarr_format": 2, "shape": shape, "chunks": chunks,
            "dtype": if name == "temp" { "<f4" } else { "<f8" },
            "compressor": null, "fill_value": "NaN", "order": "C",
            "filters": null, "dimension_separator": "."
        });
        std::fs::write(dir.join(".zarray"), serde_json::to_vec(&metadata).unwrap()).unwrap();
        std::fs::write(
            dir.join(".zattrs"),
            serde_json::to_vec(&attributes).unwrap(),
        )
        .unwrap();
    };
    array(
        "time",
        &[0],
        &[1],
        serde_json::json!({"_ARRAY_DIMENSIONS": ["time"], "units": "hours since 2026-01-01"}),
    );
    for (name, values, units) in [
        ("lat", [59.0f64, 60.0], "degrees_north"),
        ("lon", [10.0, 11.0], "degrees_east"),
    ] {
        array(
            name,
            &[2],
            &[2],
            serde_json::json!({"_ARRAY_DIMENSIONS": [name], "units": units}),
        );
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        std::fs::write(path.join(name).join("0"), bytes).unwrap();
    }
    array(
        "temp",
        &[0, 2, 2],
        &[1, 2, 2],
        serde_json::json!({"_ARRAY_DIMENSIONS": ["time", "lat", "lon"], "units": "K"}),
    );
}

fn at(rfc3339: &str) -> DateTime<Utc> {
    rfc3339.parse().unwrap()
}

fn assert_no_data_yet(what: &str, result: Result<impl std::fmt::Debug, DataServerError>) {
    match result {
        Err(error @ DataServerError::LocationNotFound(_)) => {
            assert_eq!(error.class(), ErrorClass::NotFound, "{what}");
            assert_eq!(
                error.to_string(),
                DataServerError::no_data_yet().to_string(),
                "{what}"
            );
        }
        other => panic!("{what}: expected no_data_yet, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_time_axis_answers_every_query_with_no_data_yet() {
    let dir = tempfile::tempdir().unwrap();
    write_empty_store(dir.path());
    let config = ZarrConfig::auto_local(dir.path().to_string_lossy().into());
    let engine = ZarrEngine::new("empty", &config).expect("an empty time axis loads");
    assert!(engine.catalog.load().times.is_empty());
    assert_eq!(engine.get_parameters(), ["temp"]);

    let instant = at("2026-01-01T06:00:00Z");
    let names = ["temp".to_string()];
    for (variant, datetime, parameters) in [
        ("plain", None, None),
        // One entry of a `datetime` list.
        ("instant", Some((instant, instant)), None),
        ("named", None, Some(&names[..])),
    ] {
        for (kind, result) in [
            (
                "position",
                engine.query_position("POINT(10.5 59.5)", datetime, parameters, None, None),
            ),
            (
                "area",
                engine.query_area(
                    "POLYGON((10 59,11 59,11 60,10 60,10 59))",
                    datetime,
                    parameters,
                    None,
                    None,
                ),
            ),
            (
                "radius",
                engine.query_radius(
                    "POINT(10.5 59.5)",
                    10_000.0,
                    datetime,
                    parameters,
                    None,
                    None,
                ),
            ),
            (
                "trajectory",
                engine.query_trajectory(
                    "LINESTRING(10 59.5, 11 59.5)",
                    datetime,
                    parameters,
                    None,
                    None,
                ),
            ),
        ] {
            assert_no_data_yet(&format!("{kind} {variant}"), result);
        }
    }
    assert_no_data_yet(
        "trajectory M",
        engine.query_trajectory(
            "LINESTRING M(10 59.5 1767225600, 11 59.5 1767247200)",
            None,
            None,
            None,
            None,
        ),
    );
    for time in [None, Some(instant)] {
        for parameter in [None, Some("temp")] {
            let tile = engine.get_raster_tile(
                [10.0, 59.0, 11.0, 60.0],
                2,
                2,
                time,
                &OutputCrs::Wgs84,
                parameter,
                None,
                None,
            );
            assert_no_data_yet(&format!("map {parameter:?} at {time:?}"), tile.map(|_| ()));
        }
    }
}
