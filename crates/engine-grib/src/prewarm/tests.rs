//! The pre-warm of a new run's steps (#1335): what it fetches, when it
//! fetches nothing, its bounds, and what the first query afterwards reads.

use super::*;
use crate::point_fetch_tests::{counted, gets, series};
use crate::runtime::run_fetches;
use crate::test_support::{message, parameter_message, store::TestStore, TestSource};
use ds_core::config::GribLevelType;
use ds_core::edr_engine::EdrEngine;
use ds_core::map_engine::{MapEngine, OutputCrs};
use ds_core::model::CoverageResponse;
use ds_storage::object_store::{path::Path as ObjectPath, ObjectStoreExt};
use ds_storage::{bytes::Bytes, DataStore};
use std::sync::atomic::Ordering;

/// A real CCSDS global field (0.55 MB): a position query reads it through
/// the point index.
const ECMWF: &[u8] = include_bytes!("../../../../testdata/grib-local/sample-message.grib2");
const RUN: &str = "2026040500";
const NEXT_RUN: &str = "2026040506";
const NORTH: (f64, f64) = (24.96, 60.32);
const SOUTH: (f64, f64) = (151.2, -33.9);

/// Step `step` of the run `date` in file `{prefix}f{step:03}`: `params` at
/// 150 hPa, in that order, each one [`ECMWF`], then a tail record.
fn write_step(source: &TestSource, prefix: &str, date: &str, step: u32, params: &[&str]) {
    let forecast = format!("{step} hour fcst");
    let mut records: Vec<_> = params
        .iter()
        .map(|param| (*param, "150 mb", forecast.as_str(), ECMWF.to_vec()))
        .collect();
    records.push((
        "TAIL",
        "surface",
        forecast.as_str(),
        message(0, 0.0, [0; 4], 1, 0),
    ));
    source.write_run(&format!("{prefix}f{step:03}"), date, &records);
}

/// Copy every file of `source` into `store`, the sidecars too, so the next
/// scan finds what was written since.
fn publish(source: &TestSource, store: &TestStore) {
    run_fetches(async {
        for file in std::fs::read_dir(&source.dir).unwrap() {
            let path = file.unwrap().path();
            let name = path.file_name().unwrap().to_str().unwrap().to_owned();
            let bytes = std::fs::read(&path).unwrap();
            store
                .inner
                .put(&ObjectPath::from(name), bytes.into())
                .await
                .unwrap();
        }
    });
}

/// The pressure view of `source`, its owner and the instrumented store
/// both read through, with a `message_mb` message cache, the point index
/// and no grid cache, each GET taking `delay`.
fn served(
    source: &TestSource,
    message_mb: u64,
    delay: Duration,
) -> (GribEngine, GribEngine, Arc<TestStore>) {
    let mut config = source.config();
    config.message_cache_mb = message_mb;
    config.point_index_cache_mb = Some(16);
    config.grid_cache_mb = 0;
    config.level_types = Some(vec![GribLevelType::Pressure]);
    let mut owner = GribEngine::new("prewarm", &config).unwrap();
    let store = Arc::new(TestStore {
        suffix: ".grib2",
        allow_heads: true,
        delay,
        ..Default::default()
    });
    publish(source, &store);
    Arc::get_mut(&mut owner.source).unwrap().store = DataStore::new(store.clone());
    let view = owner.level_collections().remove(0);
    (owner, view, store)
}

/// The whole-message GETs among `gets`, sorted: a header probe of a new
/// run's representative reads a few kilobytes.
fn whole(gets: Vec<(String, u64, u64)>) -> Vec<(String, u64, u64)> {
    let mut whole: Vec<_> = gets
        .into_iter()
        .filter(|(_, _, bytes)| *bytes == len())
        .collect();
    whole.sort();
    whole
}

fn len() -> u64 {
    ECMWF.len() as u64
}

/// The acceptance case: once a position query has read `q`, the poll that
/// catalogues three new steps fetches their `q` messages whole and indexes
/// them; the first query over those steps then reads nothing from storage
/// and builds no index, and answers bit for bit as an engine without
/// caches. `w`, which nobody read, is not fetched.
#[test]
fn the_first_position_query_after_new_steps_reads_nothing_from_storage() {
    let source = TestSource::new();
    for step in 0..2 {
        write_step(&source, "", RUN, step, &["q", "w"]);
    }
    let (owner, view, store) = served(&source, 20, Duration::ZERO);
    series(&view, "q", &[NORTH], RUN, None);
    gets(&store);
    for step in 2..5 {
        write_step(&source, "", RUN, step, &["q", "w"]);
    }
    publish(&source, &store);
    owner.scan_once().unwrap();
    let file = |step: u32| format!("f{step:03}.grib2");
    assert_eq!(
        whole(gets(&store)),
        [
            (file(2), 0, len()),
            (file(3), 0, len()),
            (file(4), 0, len())
        ],
        "the new steps' q messages, at offset 0; w follows them"
    );
    assert_eq!(owner.prewarm_counts(), (3, 3 * len(), 0));

    let indexes = owner.point_index_cache_metrics();
    let expected = series(
        &counted(&source, 0, 0, 0).0,
        "q",
        &[NORTH, SOUTH],
        RUN,
        Some((2, 4)),
    );
    assert_eq!(
        series(&view, "q", &[NORTH, SOUTH], RUN, Some((2, 4))),
        expected
    );
    assert_eq!(gets(&store), [], "the first query reads nothing");
    let after = owner.point_index_cache_metrics();
    assert_eq!(
        (after.hits - indexes.hits, after.misses - indexes.misses),
        (3, 0),
        "the pre-warm built the three indexes the query read"
    );
    assert_eq!(owner.prewarm_counts().2, 3, "each pre-warmed message read");
    series(&view, "q", &[SOUTH], RUN, Some((2, 4)));
    assert_eq!(
        owner.prewarm_counts().2,
        3,
        "counted at its first read only"
    );
}

/// A source nobody read in the last 24 hours makes no pre-warm request:
/// nothing read at all, maps and areas, which read no point, and a
/// position read a day ago.
#[test]
fn a_source_without_recent_position_reads_makes_no_prewarm_requests() {
    let source = TestSource::new();
    write_step(&source, "", RUN, 0, &["q"]);
    let (owner, view, store) = served(&source, 20, Duration::ZERO);
    let mut step = 1;
    let mut poll = || {
        write_step(&source, "", RUN, step, &["q"]);
        step += 1;
        publish(&source, &store);
        gets(&store);
        owner.scan_once().unwrap();
        (whole(gets(&store)), owner.prewarm_counts())
    };
    assert_eq!(poll(), (vec![], (0, 0, 0)), "nothing read");

    let time = Some("2026-04-05T00:00:00Z".parse().unwrap());
    view.get_raster_tile(
        [0.0, 0.0, 10.0, 10.0],
        4,
        4,
        time,
        &OutputCrs::Wgs84,
        Some("q"),
        Some(150.0),
        None,
    )
    .unwrap();
    view.query_area(
        "POLYGON((0 0,10 0,10 10,0 10,0 0))",
        None,
        Some(&["q".into()]),
        Some(&[150.0]),
        None,
    )
    .unwrap();
    assert_eq!(poll(), (vec![], (0, 0, 0)), "a map and an area");

    let reads = &owner.source.prewarm.as_ref().unwrap().reads;
    let key = ParameterKey {
        param: "q".into(),
        levtype: "pl".into(),
        level: Some(150),
    };
    reads.note_ago([&key], READ_WINDOW + Duration::from_secs(60));
    assert_eq!(poll(), (vec![], (0, 0, 0)), "a day ago");

    reads.note_ago([&key], READ_WINDOW - Duration::from_secs(60));
    let (fetched, _) = poll();
    assert_eq!(fetched.len(), 1, "read just within the window");
}

/// Two parameters read, six new steps of them: the pre-warm keeps exactly
/// two GETs in flight, fetches the earliest steps first and stops at a
/// quarter of the message cache, across polls of the same run; the next
/// run has a share of its own.
#[test]
fn the_prewarm_keeps_two_gets_in_flight_and_its_share_of_each_run() {
    let source = TestSource::new();
    write_step(&source, "", RUN, 0, &["q", "w"]);
    // 16 MiB: a share of 4 MiB is seven of these messages.
    let (owner, view, store) = served(&source, 16, Duration::from_millis(30));
    series(&view, "q", &[NORTH], RUN, None);
    series(&view, "w", &[NORTH], RUN, None);
    for step in 1..7 {
        write_step(&source, "", RUN, step, &["q", "w"]);
    }
    publish(&source, &store);
    gets(&store);
    store.peak.store(0, Ordering::SeqCst);
    owner.scan_once().unwrap();
    let share = 16 * ds_cache::MIB / 100 * SHARE_PERCENT;
    let fetched = share / len();
    assert_eq!(fetched, 7);
    let file = |step: u32| format!("f{step:03}.grib2");
    let mut expected: Vec<_> = (1..=4)
        .flat_map(|step| [(file(step), 0, len()), (file(step), len(), len())])
        .take(fetched as usize)
        .collect();
    expected.sort();
    assert_eq!(whole(gets(&store)), expected, "steps 1–4, q before w");
    assert_eq!(
        store.peak.load(Ordering::SeqCst),
        2,
        "two in flight, never three"
    );
    let (gets_made, bytes, _) = owner.prewarm_counts();
    assert_eq!((gets_made, bytes), (fetched, fetched * len()));
    assert!(bytes <= share);

    // The run's share is spent: a later poll of it fetches nothing.
    write_step(&source, "", RUN, 7, &["q", "w"]);
    publish(&source, &store);
    owner.scan_once().unwrap();
    assert_eq!(whole(gets(&store)), []);
    assert_eq!(owner.prewarm_counts().0, fetched);

    // A new run starts its own share.
    for step in 0..2 {
        write_step(&source, "next-", NEXT_RUN, step, &["q", "w"]);
    }
    publish(&source, &store);
    owner.scan_once().unwrap();
    let next = |step: u32| format!("next-f{step:03}.grib2");
    assert_eq!(
        whole(gets(&store)),
        [
            (next(0), 0, len()),
            (next(0), len(), len()),
            (next(1), 0, len()),
            (next(1), len(), len()),
        ]
    );
    assert_eq!(owner.prewarm_counts().0, fetched + 4);
}

/// The cache must hold each key's messages of the run, key by key: `q`'s
/// sixteen 0.55 MB messages exceed 8 MiB, so a query over them fetches
/// only parts of them and the pre-warm skips `q`, while small `w`, read
/// too, is still fetched for every new step.
#[test]
fn a_key_the_cache_cannot_hold_over_the_run_is_skipped_and_the_others_are_not() {
    let small = message(0, 250.0, [0, 2, 4, 6], 100, 15_000);
    let write = |source: &TestSource, step: u32| {
        let forecast = format!("{step} hour fcst");
        source.write_run(
            &format!("f{step:03}"),
            RUN,
            &[
                ("q", "150 mb", forecast.as_str(), ECMWF.to_vec()),
                ("w", "150 mb", forecast.as_str(), small.clone()),
                (
                    "TAIL",
                    "surface",
                    forecast.as_str(),
                    message(0, 0.0, [0; 4], 1, 0),
                ),
            ],
        );
    };
    let source = TestSource::new();
    write(&source, 0);
    // 8 MiB holds 14 of the large messages; the run will have 16.
    let (owner, view, store) = served(&source, 8, Duration::ZERO);
    series(&view, "q", &[NORTH], RUN, None);
    series(&view, "w", &[(0.5, 0.5)], RUN, None);
    for step in 1..16 {
        write(&source, step);
    }
    publish(&source, &store);
    gets(&store);
    owner.scan_once().unwrap();
    let mut fetched = gets(&store);
    fetched.sort();
    let expected: Vec<_> = (1..16)
        .map(|step| (format!("f{step:03}.grib2"), len(), small.len() as u64))
        .collect();
    assert_eq!(fetched, expected, "w at every new step, no q");
    assert_eq!(owner.prewarm_counts(), (15, 15 * small.len() as u64, 0));
}

/// #1340's gotcha for the pre-warm: in a message cache full of map reads,
/// a new entry goes to the cold ring, smaller here than one message, and
/// the next fill evicts it unread. Kept, every message the pre-warm
/// fetched is resident for the first query, which reads nothing.
#[test]
fn a_prewarm_into_a_full_cache_keeps_every_message_it_fetched() {
    let source = TestSource::new();
    write_step(&source, "", RUN, 0, &["q"]);
    // 8 MiB: a hot share of 0.97 leaves a cold ring of 0.24 MiB.
    let (owner, view, store) = served(&source, 8, Duration::ZERO);
    series(&view, "q", &[NORTH], RUN, None);
    // Fill the cache as maps do: unkept reads of another file's messages.
    let filler = ObjectPath::from("filler.grib2");
    run_fetches(async {
        let bytes = ECMWF.repeat(16);
        store.inner.put(&filler, bytes.into()).await.unwrap();
    });
    let cache = owner.source.message_cache.as_ref().unwrap();
    for i in 0..16 {
        let entry = MessageEntry {
            source_url: None,
            param: "x".into(),
            step_kind: crate::wgrib2_index::StepKind::Instant,
            levtype: "pl".into(),
            level: Some(150),
            offset: i * len(),
            length: Some(len()),
        };
        cache.read(&owner.source.store, &filler, &entry).unwrap();
    }
    // Its hot ring has no room for another: a new entry goes cold.
    let full = owner.message_cache_metrics();
    assert!(
        full.bytes + len() > full.capacity_bytes / 100 * 97,
        "{full:?}"
    );

    for step in 1..4 {
        write_step(&source, "", RUN, step, &["q"]);
    }
    publish(&source, &store);
    gets(&store);
    owner.scan_once().unwrap();
    assert_eq!(whole(gets(&store)).len(), 3);
    series(&view, "q", &[NORTH], RUN, Some((1, 3)));
    assert_eq!(gets(&store), [], "every pre-warmed message stayed");
    assert_eq!(owner.prewarm_counts().2, 3);
}

/// No pre-warm work on a request runtime: the fetches run as field jobs
/// on the runtime the poll runs on.
#[test]
fn the_prewarm_runs_on_the_poll_runtime() {
    let source = TestSource::new();
    write_step(&source, "", RUN, 0, &["q"]);
    let (owner, view, store) = served(&source, 20, Duration::ZERO);
    series(&view, "q", &[NORTH], RUN, None);
    for step in 1..5 {
        write_step(&source, "", RUN, step, &["q"]);
    }
    publish(&source, &store);
    gets(&store);
    store.reads.lock().unwrap().threads.clear();
    let poll = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("prewarm-poll")
        .enable_all()
        .build()
        .unwrap();
    let owner = Arc::new(owner);
    let polled = owner.clone();
    poll.block_on(async move { tokio::spawn(async move { polled.scan_once() }).await })
        .unwrap()
        .unwrap();
    assert_eq!(whole(gets(&store)).len(), 4);
    let threads = std::mem::take(&mut store.reads.lock().unwrap().threads);
    assert_eq!(threads.len(), 4);
    assert!(
        threads.iter().all(|thread| thread == "prewarm-poll"),
        "{threads:?}"
    );
}

/// A derived wind speed reads its u and v components, which the pre-warm
/// then fetches for a new step (simple packing: decoded in full before
/// admission, no point index), and the next derived query reads nothing.
#[test]
fn a_derived_wind_query_prewarms_its_components() {
    use ds_core::wind::{DerivedWind, OutcomeLog, PairOutcome};
    let u = |base| parameter_message(2, 2, 0x30, base, [0, 10, 5, 8], 103, 10);
    let v = |base| parameter_message(2, 3, 0x30, base, [0, 8, 4, 10], 103, 10);
    let source = TestSource::new();
    let write = |step: u32| {
        source.write(
            &format!("f{step:03}"),
            &[
                ("UGRD", "10 m above ground", u(-5.0 + step as f32)),
                ("VGRD", "10 m above ground", v(-4.0)),
                ("TMP", "2 m above ground", message(0, 280.0, [0; 4], 103, 2)),
            ],
            step,
        )
    };
    write(0);
    let mut config = source.config();
    config.message_cache_mb = 1;
    let engine = Arc::new(GribEngine::new("gfs", &config).unwrap());
    let log: OutcomeLog = Arc::new(|_: &str, _: &PairOutcome| {});
    let wind = DerivedWind::new("gfs", engine.clone(), log);
    let speed = |wind: &DerivedWind| {
        let CoverageResponse::Single(result) = wind
            .query_position("POINT(0.5 0.5)", None, Some(&["WIND".into()]), None, None)
            .unwrap()
        else {
            panic!("expected a series")
        };
        result.ranges["WIND"].values.clone()
    };
    speed(&wind);
    let read = engine.source.prewarm.as_ref().unwrap().reads.recent();
    let read: Vec<_> = read
        .iter()
        .map(|key| (key.param.as_str(), key.levtype.as_str(), key.level))
        .collect();
    assert_eq!(read, [("UGRD", "hag", Some(10)), ("VGRD", "hag", Some(10))]);

    write(1);
    engine.scan_once().unwrap();
    let (gets_made, bytes, _) = engine.prewarm_counts();
    assert_eq!(gets_made, 2);
    assert_eq!(bytes, (u(0.0).len() + v(0.0).len()) as u64);
    let before = engine.storage_bytes_read();
    assert_eq!(speed(&wind).len(), 2);
    assert_eq!(engine.storage_bytes_read(), before);
    assert_eq!(engine.prewarm_counts().2, 2);
}

/// The message cache's side: a message that does not validate is not
/// admitted, a fill counts neither hit nor miss, a resident message is not
/// fetched again, and a request's first read of a pre-warmed message
/// counts once.
#[test]
fn the_message_cache_fills_ahead_of_reads_without_counting_lookups() {
    let cache = MessageCache::new(4).unwrap();
    let entry = MessageEntry {
        source_url: None,
        param: "q".into(),
        step_kind: crate::wgrib2_index::StepKind::Instant,
        levtype: "pl".into(),
        level: Some(150),
        offset: 0,
        length: Some(len()),
    };
    let key = MessageKey::new("f000.grib2", &entry);
    let fetch = || Ok(Bytes::from_static(ECMWF));
    let invalid = |_: &[u8]| Err(DataServerError::Engine("not GRIB".into()));
    assert!(cache.prewarm(&key, fetch, invalid).is_err());
    assert!(!cache.is_resident(&key));
    assert_eq!(
        cache.prewarm(&key, fetch, |_| Ok(())).unwrap(),
        Some(ECMWF.len())
    );
    assert!(cache.is_resident(&key));
    let refetch = || -> Result<Bytes, DataServerError> { panic!("fetched a resident message") };
    assert_eq!(cache.prewarm(&key, refetch, |_| Ok(())).unwrap(), None);
    let metrics = cache.metrics();
    assert_eq!((metrics.hits, metrics.misses), (0, 0));

    assert_eq!(cache.prewarm_hits(), 0);
    assert!(cache.get_resident(&key).is_some());
    assert!(cache.get_resident(&key).is_some());
    assert_eq!(cache.prewarm_hits(), 1);
}

/// What position queries read is forgotten a day after the last read, and
/// a source remembers at most [`MAX_KEYS`] parameter levels, dropping the
/// one read longest ago.
#[test]
fn reads_are_kept_for_a_day_and_bounded() {
    let reads = RecentReads::default();
    let key = |param: &str| ParameterKey {
        param: param.into(),
        levtype: "pl".into(),
        level: Some(150),
    };
    reads.note([&key("q")]);
    reads.note_ago([&key("w")], READ_WINDOW + Duration::from_secs(60));
    assert_eq!(reads.recent(), [key("q")]);
    reads.note_ago([&key("w")], READ_WINDOW - Duration::from_secs(60));
    assert_eq!(reads.recent(), [key("q"), key("w")]);
    for i in 0..MAX_KEYS {
        reads.note_ago([&key(&format!("p{i:03}"))], Duration::from_secs(60));
    }
    let recent = reads.recent();
    assert_eq!(recent.len(), MAX_KEYS);
    assert!(recent.contains(&key("q")));
    assert!(!recent.contains(&key("w")), "the one read longest ago");
}
