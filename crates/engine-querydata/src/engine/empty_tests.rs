//! A collection with no data (#1499). The engine refuses to start without a
//! loadable `.sqd` file and a poll keeps its last run set, so this is a
//! guard: no run, or a run without a timestep, answers every EDR query type
//! and map render with [`DataServerError::no_data_yet`], a 404 a `datetime`
//! list skips, never the `Engine` 500 the API layers log at ERROR.

use super::*;
use ds_core::error::ErrorClass;

fn engine_over(set: RunSet) -> QueryDataEngine {
    QueryDataEngine {
        runs: ArcSwap::from_pointee(set),
        data_dir: PathBuf::new(),
        wms_parameter: None,
        collection_id: "empty".into(),
        poll_interval: Duration::from_secs(30),
        max_runs: 1,
        shutdown: Shutdown::new(),
        polled_at: Mutex::new(None),
    }
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

fn map(engine: &QueryDataEngine, time: Option<DateTime<Utc>>) -> Result<(), DataServerError> {
    engine
        .get_raster_tile(
            [36.0, -2.0, 38.0, 0.0],
            4,
            4,
            time,
            &OutputCrs::Wgs84,
            None,
            None,
            None,
        )
        .map(|_| ())
}

#[test]
fn no_run_answers_every_query_with_no_data_yet() {
    let engine = engine_over(RunSet::default());
    let instant = at("2026-04-04T06:00:00Z");
    for (variant, datetime) in [("plain", None), ("instant", Some((instant, instant)))] {
        for (kind, result) in [
            (
                "position",
                engine.query_position("POINT(37 -1)", datetime, None, None, None),
            ),
            (
                "area",
                engine.query_area(
                    "POLYGON((36 -2,38 -2,38 0,36 0,36 -2))",
                    datetime,
                    None,
                    None,
                    None,
                ),
            ),
            (
                "radius",
                engine.query_radius("POINT(37 -1)", 10_000.0, datetime, None, None, None),
            ),
            (
                "trajectory",
                engine.query_trajectory("LINESTRING(36 -1, 38 -1)", datetime, None, None, None),
            ),
        ] {
            assert_no_data_yet(&format!("{kind} {variant}"), result);
        }
    }
    // A pinned run of an empty set: the collection has none at all.
    assert_no_data_yet(
        "position pinned",
        engine.query_position("POINT(37 -1)", None, None, None, Some(instant)),
    );
    for time in [None, Some(instant)] {
        assert_no_data_yet(&format!("map at {time:?}"), map(&engine, time));
    }
}

/// A run without a timestep has nothing to draw: the map answers as a
/// collection without runs does, as its EDR queries already did.
#[test]
fn a_run_without_timesteps_draws_no_data_yet() {
    let path = list_sqd_files(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/ecmwf-kenya"),
    )
    .into_iter()
    .next()
    .expect("ecmwf-kenya fixture");
    let mut data = QueryData::open(&path).unwrap();
    data.times.clear();
    let runs = BTreeMap::from([(data.origin_time, RunEntry::new(Arc::new(data), path))]);
    let engine = engine_over(RunSet::new(runs, None));
    for time in [None, Some(at("2026-04-04T06:00:00Z"))] {
        assert_no_data_yet(&format!("map at {time:?}"), map(&engine, time));
    }
}
