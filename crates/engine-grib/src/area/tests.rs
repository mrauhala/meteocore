use super::*;
use crate::runtime::FIELD_CONCURRENCY;
use crate::test_support::{message, store::TestStore, TestSource};
use ds_storage::object_store::{limit::LimitStore, path::Path, ObjectStoreExt};
use std::sync::atomic::Ordering;

fn filename(param: usize, level: usize) -> String {
    format!("f{param:02}-{level:02}")
}

fn parameter(param: usize) -> String {
    format!("P{param}")
}

fn level_value(kind: Option<GribLevelType>, level: usize) -> u32 {
    match kind {
        Some(GribLevelType::Pressure) => 500 + level as u32 * 10,
        _ => level as u32 + 1,
    }
}

fn base_value(param: usize, level: usize) -> f32 {
    (if param.is_multiple_of(2) { 280.0 } else { 40.0 }) + (param * 5 + level) as f32
}

fn fixture(
    kind: Option<GribLevelType>,
    params: usize,
    levels: usize,
    missing: &[(usize, usize)],
) -> (TestSource, GribEngine) {
    let source = TestSource::new();
    for param in 0..params {
        for level in 0..levels {
            if missing.contains(&(param, level)) {
                continue;
            }
            let value = level_value(kind, level);
            let (description, surface, encoded) = match kind {
                Some(GribLevelType::Pressure) => (format!("{value} mb"), 100, value * 100),
                Some(GribLevelType::Model) => (format!("{value} hybrid level"), 105, value),
                _ => ("2 m above ground".into(), 103, 2),
            };
            let mut bytes = message(0, base_value(param, level), [0, 2, 4, 6], surface, encoded);
            if !param.is_multiple_of(2) {
                bytes[118] = 1;
                bytes[119] = 1;
            } // RH, already in %
            source.write(
                &filename(param, level),
                &[
                    (&parameter(param), &description, bytes),
                    // Explicit length for the selected field: avoid tail HEADs.
                    ("TAIL", "surface", message(0, 0.0, [0; 4], 1, 0)),
                ],
                0,
            );
        }
    }
    let mut config = source.config();
    config.grid_cache_mb = 0;
    config.level_types = kind.map(|kind| vec![kind]);
    config.parameters = Some((0..params).map(parameter).collect());
    let engine = GribEngine::new("area", &config).unwrap();
    (source, engine)
}

fn instrument(
    source: &TestSource,
    engine: &mut GribEngine,
    delay: Duration,
    overrides: &[(&str, Duration)],
) -> Arc<TestStore> {
    let store = Arc::new(TestStore {
        suffix: ".grib2",
        delay,
        delays: overrides
            .iter()
            .map(|(name, delay)| ((*name).to_owned(), *delay))
            .collect(),
        ..Default::default()
    });
    run_fetches(async {
        for file in std::fs::read_dir(&source.dir).unwrap() {
            let file = file.unwrap();
            if file.path().extension().is_some_and(|ext| ext == "grib2") {
                store
                    .inner
                    .put(
                        &Path::from(file.file_name().to_str().unwrap()),
                        std::fs::read(file.path()).unwrap().into(),
                    )
                    .await
                    .unwrap();
            }
        }
    });
    Arc::get_mut(&mut engine.source).unwrap().store = ds_storage::DataStore::new(store.clone());
    store
}

fn view(owner: &GribEngine) -> GribEngine {
    owner
        .level_collections()
        .into_iter()
        .next()
        .unwrap_or_else(|| GribEngine {
            collection_id: owner.collection_id.clone(),
            family: owner.family,
            source: owner.source.clone(),
        })
}

fn single(response: CoverageResponse) -> QueryResult {
    match response {
        CoverageResponse::Single(result) => result,
        _ => panic!("expected one grid"),
    }
}

fn assert_value(actual: Option<f64>, expected: Option<f64>) {
    match (actual, expected) {
        (Some(actual), Some(expected)) => {
            assert!((actual - expected).abs() < 1e-9, "{actual} != {expected}")
        }
        (None, None) => {}
        _ => panic!("{actual:?} != {expected:?}"),
    }
}

#[test]
fn area_preserves_requested_level_order_missing_fields_units_and_polygon_holes() {
    for kind in [GribLevelType::Pressure, GribLevelType::Model] {
        let missing = [(0, 2), (1, 0)];
        let (source, mut owner) = fixture(Some(kind), 2, 3, &missing);
        let store = instrument(
            &source,
            &mut owner,
            Duration::from_millis(20),
            &[("f00-00.grib2", Duration::from_millis(100))],
        );
        let engine = view(&owner);
        let levels: Vec<_> = [2, 0, 1]
            .map(|level| f64::from(level_value(Some(kind), level)))
            .into();
        let polygon = "POLYGON((-0.5 -0.5,1.5 -0.5,1.5 1.5,-0.5 1.5,-0.5 -0.5),(0.75 0.75,1.25 0.75,1.25 1.25,0.75 1.25,0.75 0.75))";
        let result = single(
            engine
                .query_area(
                    polygon,
                    None,
                    Some(&[parameter(0), parameter(1)]),
                    Some(&levels),
                    None,
                )
                .unwrap(),
        );
        let DomainDescription::Grid {
            x, y, z: Some(z), ..
        } = result.domain
        else {
            panic!()
        };
        assert_eq!(x, vec![0.0, 1.0]);
        assert_eq!(y, vec![0.0, 1.0]);
        assert_eq!(z.values, levels);
        for param in 0..2 {
            let name = parameter(param);
            let range = &result.ranges[&name];
            assert_eq!(range.shape, vec![3, 2, 2]);
            assert_eq!(range.axis_names, ["z", "y", "x"]);
            assert_eq!(
                result.parameters[&name].unit,
                if param == 0 { "°C" } else { "%" }
            );
            for (slot, level) in [2, 0, 1].into_iter().enumerate() {
                for (pixel, offset) in [Some(4.0), Some(6.0), Some(0.0), None]
                    .into_iter()
                    .enumerate()
                {
                    let expected = if missing.contains(&(param, level)) {
                        None
                    } else {
                        offset.map(|offset| {
                            f64::from(base_value(param, level)) + offset
                                - if param == 0 { 273.15 } else { 0.0 }
                        })
                    };
                    assert_value(range.values[slot * 4 + pixel], expected);
                }
            }
        }
        // The reference grid is P1 at the first requested level, not P0.
        // It must be reused even though the decoded cache is disabled.
        assert_eq!(engine.storage_bytes_read(), 4 * 183);
        assert!(store
            .reads
            .lock()
            .unwrap()
            .attempts
            .values()
            .all(|&n| n == 1));
        assert_eq!(
            store.reads.lock().unwrap().completed.last().unwrap(),
            "f00-00.grib2"
        );
        assert!((2..=FIELD_CONCURRENCY).contains(&store.peak.load(Ordering::SeqCst)));
        assert_eq!(store.active.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn single_and_legacy_radius_queries_fetch_each_field_once() {
    for kind in [None, Some(GribLevelType::Single)] {
        let (source, mut owner) = fixture(kind, 9, 1, &[]);
        let store = instrument(&source, &mut owner, Duration::from_millis(20), &[]);
        let engine = view(&owner);
        let params: Vec<_> = (0..9).map(parameter).collect();
        let result = single(
            engine
                .query_radius("POINT(0 0)", 20_000.0, None, Some(&params), None, None)
                .unwrap(),
        );
        assert!(matches!(
            result.domain,
            DomainDescription::Grid { z: None, .. }
        ));
        for (param, name) in params.iter().enumerate() {
            let range = &result.ranges[name];
            assert_eq!(range.axis_names, ["y", "x"]);
            assert_eq!(range.shape, [2, 2]);
            assert_value(
                range.values[0],
                Some(
                    f64::from(base_value(param, 0)) + 4.0
                        - if param.is_multiple_of(2) { 273.15 } else { 0.0 },
                ),
            );
            assert_eq!(&range.values[1..], &[None; 3]);
        }
        assert_eq!(engine.storage_bytes_read(), 9 * 183);
        assert_eq!(store.reads.lock().unwrap().attempts.len(), 9);
        assert!((2..=FIELD_CONCURRENCY).contains(&store.peak.load(Ordering::SeqCst)));
    }
}

#[test]
fn invalid_requests_and_oversized_areas_stop_before_loading_other_fields() {
    let source = TestSource::new();
    let global = include_bytes!("../../../../testdata/grib-local/sample-message.grib2");
    for param in ["P0", "P1"] {
        source.write(
            param,
            &[
                (param, "surface", global.to_vec()),
                ("TAIL", "surface", message(0, 0.0, [0; 4], 1, 0)),
            ],
            0,
        );
    }
    let mut config = source.config();
    config.grid_cache_mb = 0;
    let mut engine = GribEngine::new("budget", &config).unwrap();
    let store = instrument(&source, &mut engine, Duration::ZERO, &[]);
    for (coords, params, z) in [
        ("invalid", vec!["P0".into()], None),
        ("0,0,1,1", vec!["UNKNOWN".into()], None),
        ("0,0,1,1", vec!["P0".into()], Some(vec![850.0])),
    ] {
        assert!(engine
            .query_area(coords, None, Some(&params), z.as_deref(), None)
            .is_err());
    }
    assert_eq!(engine.storage_bytes_read(), 0);
    assert!(matches!(
        engine.query_area(
            "-180,-90,180,90",
            None,
            Some(&["P0".into(), "P1".into()]),
            None,
            None
        ),
        Err(DataServerError::QueryTooLarge(_))
    ));
    assert_eq!(store.reads.lock().unwrap().attempts.len(), 1);
    assert_eq!(engine.storage_bytes_read(), global.len() as u64);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn area_errors_and_deadlines_drain_workers_and_stop_dispatch() {
    for deadline in [false, true] {
        let (source, mut owner) = fixture(None, 12, 1, &[]);
        let delay = if deadline {
            Duration::from_secs(30)
        } else {
            Duration::from_millis(50)
        };
        let store = instrument(
            &source,
            &mut owner,
            delay,
            &[
                ("f00-00.grib2", Duration::ZERO),
                (
                    "f01-00.grib2",
                    if deadline { delay } else { Duration::ZERO },
                ),
            ],
        );
        if !deadline {
            store
                .reads
                .lock()
                .unwrap()
                .fail_once
                .insert("f01-00.grib2".into());
        }
        let references = Arc::strong_count(&owner.source);
        let _deadline =
            ds_core::deadline::enter(deadline.then(|| Instant::now() + Duration::from_millis(100)));
        let params: Vec<_> = (0..12).map(parameter).collect();
        let result = owner.query_area("0,0,1,1", None, Some(&params), None, None);
        if deadline {
            assert!(matches!(result, Err(DataServerError::DeadlineExceeded)));
        } else {
            assert!(matches!(result, Err(DataServerError::Storage(_))));
        }
        assert!(store.reads.lock().unwrap().attempts.len() <= 1 + FIELD_CONCURRENCY);
        assert_eq!(store.active.load(Ordering::SeqCst), 0);
        assert_eq!(Arc::strong_count(&owner.source), references);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mismatched_grids_are_rejected_and_position_read_failures_still_become_null() {
    let (source, mut owner) = fixture(None, 6, 1, &[]);
    let store = instrument(&source, &mut owner, Duration::from_millis(20), &[]);
    let path = Path::from("f01-00.grib2");
    let mut bytes = std::fs::read(source.dir.join(path.as_ref())).unwrap();
    bytes[87..91].copy_from_slice(&1_000_000u32.to_be_bytes()); // first longitude = 1
    bytes[96..100].copy_from_slice(&2_000_000u32.to_be_bytes()); // last longitude = 2
    store.inner.put(&path, bytes.into()).await.unwrap();
    let params: Vec<_> = (0..6).map(parameter).collect();
    let result = owner.query_area("0,0,1,1", None, Some(&params), None, None);
    assert!(
        matches!(result, Err(DataServerError::InvalidParameter(message)) if message.contains("different grid"))
    );
    assert_eq!(store.active.load(Ordering::SeqCst), 0);
    store
        .reads
        .lock()
        .unwrap()
        .fail_once
        .insert("f00-00.grib2".into());
    let result = single(
        owner
            .query_position("POINT(0 0)", None, Some(&[parameter(0)]), None, None)
            .unwrap(),
    );
    assert_eq!(result.ranges["P0"].values, vec![None]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual area latency replay; simulated latency, no wall-clock CI assertions"]
async fn area_latency_replay() {
    let mut baseline = None;
    for concurrency in [1, FIELD_CONCURRENCY] {
        let (source, mut owner) = fixture(Some(GribLevelType::Pressure), 2, 16, &[]);
        let store = instrument(&source, &mut owner, Duration::from_millis(150), &[]);
        Arc::get_mut(&mut owner.source).unwrap().store =
            ds_storage::DataStore::new(Arc::new(LimitStore::new(store.clone(), concurrency)));
        let engine = view(&owner);
        let start = Instant::now();
        let result = single(
            engine
                .query_area(
                    "0,0,1,1",
                    None,
                    Some(&[parameter(0), parameter(1)]),
                    None,
                    None,
                )
                .unwrap(),
        );
        let elapsed = start.elapsed();
        let DomainDescription::Grid {
            x,
            y,
            t,
            z: Some(z),
        } = result.domain
        else {
            panic!("expected a vertical grid");
        };
        let fingerprint = (
            (x, y, t, z.kind, z.values),
            (0..2)
                .map(|param| {
                    let name = parameter(param);
                    let range = &result.ranges[&name];
                    let description = &result.parameters[&name];
                    (
                        range.shape.clone(),
                        range.axis_names.clone(),
                        range.values.clone(),
                        description.label.clone(),
                        description.unit.clone(),
                        description.observed_property.clone(),
                    )
                })
                .collect::<Vec<_>>(),
            engine.storage_bytes_read(),
        );
        if let Some(expected) = &baseline {
            assert_eq!(&fingerprint, expected);
        } else {
            baseline = Some(fingerprint);
        }
        assert_eq!(store.reads.lock().unwrap().attempts.len(), 32);
        assert_eq!(engine.storage_bytes_read(), 32 * 183);
        eprintln!(
            "fields=32 concurrency={concurrency} elapsed_ms={} bytes={}",
            elapsed.as_millis(),
            engine.storage_bytes_read()
        );
    }
}
