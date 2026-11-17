//! A GRIB source with no data yet (#1499): a fresh directory, a store
//! unreachable since the start, a mistyped prefix. Every EDR query type and
//! map render answers [`DataServerError::no_data_yet`], a 404 a `datetime`
//! list skips, never the `Engine` 500 the API layers log at ERROR.

use super::*;
use crate::test_support::TestSource;
use ds_core::cube::CubeResolution;
use ds_core::error::ErrorClass;
use ds_core::feature::Bbox;
use ds_core::map_engine::OutputCrs;

fn at(rfc3339: &str) -> DateTime<Utc> {
    rfc3339.parse().unwrap()
}

/// Every query type of `engine`, each with no `datetime`, an instant (one
/// entry of a `datetime` list), a pinned run and a `parameter-name`.
fn every_query(engine: &GribEngine) -> Vec<(String, Result<CoverageResponse, DataServerError>)> {
    let instant = at("2026-04-05T06:00:00Z");
    let names = ["TMP".to_string()];
    let mut answers = Vec::new();
    for (variant, datetime, parameters, reference_time) in [
        ("plain", None, None, None),
        ("instant", Some((instant, instant)), None, None),
        ("named", None, Some(&names[..]), None),
        ("pinned", None, None, Some(at("2026-04-05T00:00:00Z"))),
    ] {
        let mut answer = |kind: &str, result| answers.push((format!("{kind} {variant}"), result));
        answer(
            "position",
            engine.query_position("POINT(0.5 0.5)", datetime, parameters, None, reference_time),
        );
        answer(
            "area",
            engine.query_area(
                "POLYGON((0 0,1 0,1 1,0 1,0 0))",
                datetime,
                parameters,
                None,
                reference_time,
            ),
        );
        answer(
            "radius",
            engine.query_radius(
                "POINT(0.5 0.5)",
                10_000.0,
                datetime,
                parameters,
                None,
                reference_time,
            ),
        );
        answer(
            "trajectory",
            engine.query_trajectory(
                "LINESTRING(0 0.5, 1 0.5)",
                datetime,
                parameters,
                None,
                reference_time,
            ),
        );
        // An M path selects its run by its first time, as a map does.
        answer(
            "trajectory M",
            engine.query_trajectory(
                "LINESTRING M(0 0.5 1775347200, 1 0.5 1775368800)",
                None,
                parameters,
                None,
                reference_time,
            ),
        );
        if engine.vertical_kind().is_some() {
            answer(
                "cube",
                engine.query_cube(
                    &Bbox::new(0.0, 0.0, 1.0, 1.0).unwrap(),
                    datetime,
                    parameters,
                    None,
                    CubeResolution::default(),
                    reference_time,
                ),
            );
        }
    }
    answers
}

fn assert_no_data_yet(what: &str, result: Result<impl std::fmt::Debug, DataServerError>) {
    match result {
        Err(DataServerError::LocationNotFound(message)) => {
            assert_eq!(message, DataServerError::NO_DATA_YET, "{what}")
        }
        other => panic!("{what}: expected no_data_yet, got {other:?}"),
    }
}

/// An empty directory builds an engine with no runs; every query on it and
/// on a pressure view over it is the not-found, as is every map render, and
/// nothing on the way logs at ERROR.
#[test]
fn an_empty_source_answers_every_query_with_no_data_yet() {
    let source = TestSource::new();
    let engine = GribEngine::new("empty", &source.config()).expect("an empty directory loads");
    assert!(engine.catalog().runs.is_empty());
    // A pressure view whose family lost every run (views are built only
    // for a family with one): cube answers too.
    let view = GribEngine {
        collection_id: "empty-pressure".into(),
        family: Some(GribLevelType::Pressure),
        source: engine.source.clone(),
    };
    assert_eq!(
        view.supported_query_types(),
        ["position", "area", "radius", "trajectory", "cube"]
    );

    let errors = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let subscriber = ErrorCounter(errors.clone());
    tracing::subscriber::with_default(subscriber, || {
        for engine in [&engine, &view] {
            let answers = every_query(engine);
            assert!(answers.len() >= 20, "{}", answers.len());
            for (what, result) in answers {
                let what = format!("{}: {what}", engine.collection_id());
                if let Err(e) = &result {
                    assert_eq!(e.class(), ErrorClass::NotFound, "{what}");
                }
                assert_no_data_yet(&what, result);
            }
            for (time, reference_time) in [
                (None, None),
                (Some(at("2026-04-05T06:00:00Z")), None),
                (None, Some(at("2026-04-05T00:00:00Z"))),
            ] {
                for parameter in [None, Some("TMP")] {
                    let tile = engine.get_raster_tile(
                        [0.0, 0.0, 1.0, 1.0],
                        4,
                        4,
                        time,
                        &OutputCrs::Wgs84,
                        parameter,
                        None,
                        reference_time,
                    );
                    assert_no_data_yet(
                        &format!("map {parameter:?} at {time:?} of {reference_time:?}"),
                        tile.map(|_| ()),
                    );
                }
            }
        }
    });
    assert_eq!(
        errors.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "nothing is logged at ERROR"
    );
}

/// Counts the ERROR events of the queries it observes.
struct ErrorCounter(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl tracing::Subscriber for ErrorCounter {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        if *event.metadata().level() == tracing::Level::ERROR {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}
