//! The map read-ahead (#1335): what a map loop reads from storage with it,
//! what it fetches and when it fetches nothing, its bounds, where it runs,
//! and how a request meets a fill in flight.

use super::*;
use crate::point_fetch_tests::series;
use crate::runtime::run_fetches;
use crate::test_support::{message, store::TestStore, TestSource};
use ds_core::config::GribLevelType;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterValues};
use ds_storage::object_store::{path::Path as ObjectPath, GetRange, ObjectStoreExt};
use ds_storage::{bytes::Bytes, DataStore};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A real CCSDS global field (0.55 MB).
const ECMWF: &[u8] = include_bytes!("../../../../testdata/grib-local/sample-message.grib2");
const RUN: &str = "2026040500";
const NEXT_RUN: &str = "2026040506";

fn len() -> u64 {
    ECMWF.len() as u64
}

/// A small field at 150 hPa, about 200 bytes.
fn small() -> Vec<u8> {
    message(0, 250.0, [0, 2, 4, 6], 100, 15_000)
}

/// Step `step` of the run `date` in file `{prefix}f{step:03}`: `fields` at
/// 150 hPa, in that order, then a tail record.
fn write_step(source: &TestSource, prefix: &str, date: &str, step: u32, fields: &[(&str, &[u8])]) {
    let forecast = format!("{step} hour fcst");
    let mut records: Vec<_> = fields
        .iter()
        .map(|(param, bytes)| (*param, "150 mb", forecast.as_str(), bytes.to_vec()))
        .collect();
    records.push((
        "TAIL",
        "surface",
        forecast.as_str(),
        message(0, 0.0, [0; 4], 1, 0),
    ));
    source.write_run(&format!("{prefix}f{step:03}"), date, &records);
}

/// Steps `steps` of the run `date`, `q` alone, the real field.
fn write_q(source: &TestSource, prefix: &str, date: &str, steps: std::ops::Range<u32>) {
    for step in steps {
        write_step(source, prefix, date, step, &[("q", ECMWF)]);
    }
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
/// both read through `store`: a `message_mb` message cache, a `grid_mb`
/// grid cache and a read-ahead of `depth` steps.
fn served(
    source: &TestSource,
    message_mb: u64,
    grid_mb: u64,
    depth: usize,
    store: TestStore,
) -> (GribEngine, GribEngine, Arc<TestStore>) {
    let mut config = source.config();
    config.message_cache_mb = message_mb;
    config.grid_cache_mb = grid_mb;
    config.point_index_cache_mb = Some(16);
    config.read_ahead = depth;
    config.level_types = Some(vec![GribLevelType::Pressure]);
    let mut owner = GribEngine::new("readahead", &config).unwrap();
    let store = Arc::new(TestStore {
        suffix: ".grib2",
        allow_heads: true,
        ..store
    });
    publish(source, &store);
    Arc::get_mut(&mut owner.source).unwrap().store = DataStore::new(store.clone());
    let view = owner.level_collections().remove(0);
    (owner, view, store)
}

fn at(run: &str, step: u32) -> DateTime<Utc> {
    let run = chrono::NaiveDateTime::parse_from_str(&format!("{run}0000"), "%Y%m%d%H%M%S")
        .unwrap()
        .and_utc();
    run + chrono::Duration::hours(step.into())
}

/// One tile of `parameter`'s frame at `step` of the run `run`, as bits.
fn frame(view: &GribEngine, run: &str, step: u32, parameter: &str, tile: usize) -> Vec<u32> {
    let west = tile as f64;
    let tile = view
        .get_raster_tile(
            [west, 0.0, west + 1.0, 1.0],
            4,
            4,
            Some(at(run, step)),
            &OutputCrs::Wgs84,
            Some(parameter),
            Some(150.0),
            None,
        )
        .unwrap();
    let RasterValues::F32 { data, .. } = tile.values else {
        panic!("a GRIB map tile is f32");
    };
    data.iter().map(|v| v.to_bits()).collect()
}

/// A storage read: the file, the offset and length read, and whether the
/// read-ahead made it (on the GRIB I/O runtime) rather than a request.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Read {
    file: String,
    offset: u64,
    length: u64,
    background: bool,
}

/// The GETs since the last call, sorted.
fn reads(store: &TestStore) -> Vec<Read> {
    let mut reads = store.reads.lock().unwrap();
    let threads = std::mem::take(&mut reads.threads);
    let mut made: Vec<Read> = std::mem::take(&mut reads.ranges)
        .into_iter()
        .zip(threads)
        .map(|((file, range), thread)| {
            let Some(GetRange::Bounded(range)) = range else {
                panic!("unbounded GET of {file}");
            };
            Read {
                file,
                offset: range.start,
                length: range.end - range.start,
                background: thread == "grib-io",
            }
        })
        .collect();
    made.sort();
    made
}

fn file(prefix: &str, step: u32) -> String {
    format!("{prefix}f{step:03}.grib2")
}

/// A whole `q` message of step `step`, read by a request or in the
/// background.
fn q(prefix: &str, step: u32, background: bool) -> Read {
    Read {
        file: file(prefix, step),
        offset: 0,
        length: len(),
        background,
    }
}

/// The acceptance case: a client stepping through a cold run, one frame
/// after another and each of three tiles, waits on storage for its first
/// frame only. The read-ahead reads every later step's message once, on
/// the GRIB I/O runtime, planned once per frame, and every one is read; the
/// frames equal those of an engine that reads each itself.
#[test]
fn a_sequential_map_loop_over_a_cold_run_reads_storage_only_for_its_first_frame() {
    let source = TestSource::new();
    write_q(&source, "", RUN, 0..8);
    let (owner, view, store) = served(&source, 32, 0, 3, TestStore::default());
    let (_, plain, _) = served(&source, 32, 0, 0, TestStore::default());
    reads(&store);
    let mut frames = Vec::new();
    for step in 0..8 {
        let tiles: Vec<_> = (0..3)
            .map(|tile| frame(&view, RUN, step, "q", tile))
            .collect();
        frames.push(tiles);
        // A client slower than the store: the next step is fetched by
        // the time it asks.
        owner.wait_for_read_ahead();
    }
    let mut expected = vec![q("", 0, false)];
    expected.extend((1..8).map(|step| q("", step, true)));
    expected.sort();
    assert_eq!(reads(&store), expected, "the requests read step 0 only");
    assert_eq!(owner.read_ahead_counts(), (7, 7 * len(), 7));
    let ahead = owner.source.read_ahead.as_ref().unwrap();
    assert_eq!(ahead.triggered.lock().unwrap().len(), 8, "once per frame");
    for (step, tiles) in frames.iter().enumerate() {
        for (tile, values) in tiles.iter().enumerate() {
            assert_eq!(*values, frame(&plain, RUN, step as u32, "q", tile));
        }
    }
}

/// The render that plans the read-ahead returns before its GETs do, here
/// two seconds each: it waits on none of them.
#[test]
fn the_triggering_render_does_not_wait_for_the_read_ahead() {
    let source = TestSource::new();
    write_q(&source, "", RUN, 0..3);
    let slow = Duration::from_secs(2);
    let store = TestStore {
        delays: [(file("", 1), slow), (file("", 2), slow)].into(),
        ..Default::default()
    };
    let (owner, view, store) = served(&source, 32, 0, 2, store);
    reads(&store);
    let started = Instant::now();
    frame(&view, RUN, 0, "q", 0);
    let rendered = started.elapsed();
    let completed = store.reads.lock().unwrap().completed.clone();
    assert!(
        !completed.contains(&file("", 1)) && !completed.contains(&file("", 2)),
        "the render returned after {rendered:?}, past a read-ahead GET: {completed:?}"
    );
    owner.wait_for_read_ahead();
    assert!(started.elapsed() >= slow);
    assert_eq!(
        reads(&store),
        [q("", 0, false), q("", 1, true), q("", 2, true)]
    );
}

/// A client asking for the next frame while its read-ahead GET is in
/// flight joins that GET: the message is read once, and the read counts.
#[test]
fn a_request_joins_the_read_ahead_fill_in_flight() {
    let source = TestSource::new();
    write_q(&source, "", RUN, 0..2);
    let store = TestStore {
        delays: [(file("", 1), Duration::from_millis(500))].into(),
        ..Default::default()
    };
    let (owner, view, store) = served(&source, 32, 0, 1, store);
    reads(&store);
    frame(&view, RUN, 0, "q", 0);
    let waiting = Instant::now();
    while store.active.load(Ordering::SeqCst) == 0 {
        assert!(waiting.elapsed() < Duration::from_secs(10), "no read-ahead");
        std::thread::yield_now();
    }
    let expected = frame(&view, RUN, 1, "q", 0);
    owner.wait_for_read_ahead();
    assert_eq!(reads(&store), [q("", 0, false), q("", 1, true)]);
    assert_eq!(owner.read_ahead_counts(), (1, len(), 1));
    let (_, plain, _) = served(&source, 0, 0, 0, TestStore::default());
    assert_eq!(expected, frame(&plain, RUN, 1, "q", 0));
}

/// The message cache's side of a fill in flight: a request with a deadline
/// waits on it for half its remaining time, then reads the message itself
/// around the cache, which the fill then fills; one without a deadline
/// waits for the fill and reads nothing. A fill counts no hit or miss, the
/// joined read a hit and the read around neither.
#[test]
fn a_request_waits_on_a_read_ahead_fill_at_most_half_its_time() {
    let store = Arc::new(TestStore {
        suffix: ".grib2",
        ..Default::default()
    });
    let path = ObjectPath::from("f000.grib2");
    run_fetches(async {
        let bytes = ECMWF.repeat(2);
        store.inner.put(&path, bytes.into()).await.unwrap();
    });
    let data = DataStore::new(store.clone());
    let cache = Arc::new(MessageCache::new(8).unwrap());
    let entry = |offset: u64| MessageEntry {
        source_url: None,
        param: "q".into(),
        step_kind: crate::wgrib2_index::StepKind::Instant,
        levtype: "pl".into(),
        level: Some(150),
        offset,
        length: Some(len()),
    };
    // A fill that holds its claim until released, or for three seconds:
    // a request waiting on it for longer fails the timing below rather
    // than hang.
    let hold = |offset: u64| {
        let (claimed, started) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let timeout = release.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(3));
            let _ = timeout.send(());
        });
        let cache = cache.clone();
        let key = MessageKey::new(path.as_ref(), &entry(offset));
        let fill = std::thread::spawn(move || {
            let fetch = move || {
                claimed.send(()).unwrap();
                released.recv().unwrap();
                Ok(Bytes::from_static(ECMWF))
            };
            cache.read_ahead(&key, fetch).unwrap()
        });
        started.recv().unwrap();
        (release, fill)
    };
    let accept = |_: &[u8]| Ok::<_, DataServerError>(());

    let (release, fill) = hold(0);
    let started = Instant::now();
    {
        let _deadline = ds_core::deadline::enter(Some(started + Duration::from_secs(1)));
        let (bytes, validated) = cache
            .get_with(&data, &path, &entry(0), false, accept)
            .unwrap();
        assert_eq!(&bytes[..], ECMWF);
        assert!(validated.is_some(), "read and validated around the fill");
    }
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(500) && waited < Duration::from_secs(1),
        "{waited:?}"
    );
    assert_eq!(store.reads.lock().unwrap().attempts[path.as_ref()], 1);
    let key = MessageKey::new(path.as_ref(), &entry(0));
    assert!(!cache.is_resident(&key), "the fill still holds it");
    let _ = release.send(());
    assert_eq!(fill.join().unwrap(), Some(ECMWF.len()));
    assert!(cache.is_resident(&key));
    let metrics = cache.metrics();
    assert_eq!((metrics.hits, metrics.misses), (0, 0));
    assert_eq!(metrics.churn.bypasses, 1);

    let (release, fill) = hold(len());
    let joined = std::thread::spawn({
        let (cache, data, path) = (cache.clone(), data.clone(), path.clone());
        move || {
            cache
                .get_with(&data, &path, &entry(len()), false, accept)
                .unwrap()
        }
    });
    std::thread::sleep(Duration::from_millis(100));
    let _ = release.send(());
    assert_eq!(fill.join().unwrap(), Some(ECMWF.len()));
    let (bytes, validated) = joined.join().unwrap();
    assert_eq!(&bytes[..], ECMWF);
    assert!(validated.is_none(), "the fill's bytes");
    assert_eq!(store.reads.lock().unwrap().attempts[path.as_ref()], 1);
    assert_eq!(cache.metrics().hits, 1);
    assert_eq!(cache.read_ahead_hits(), 1);
}

/// The bounds over a `TestStore` replay. A map of `q` reads ahead while a
/// poll pre-warms `w` for position clients: two GETs in flight between
/// them, never more. Then a loop over a cold run reads ahead within a
/// quarter of the message cache, the earliest steps first, and the maps
/// read the rest; the next run has a share of its own.
#[test]
fn the_read_ahead_keeps_two_gets_in_flight_with_the_prewarm_and_its_share_of_each_run() {
    let source = TestSource::new();
    for step in 0..2 {
        write_step(&source, "", RUN, step, &[("q", ECMWF), ("w", ECMWF)]);
    }
    let store = TestStore {
        delay: Duration::from_millis(100),
        ..Default::default()
    };
    let (owner, view, store) = served(&source, 32, 0, 3, store);
    series(&view, "w", &[(24.96, 60.32)], RUN, None);
    // Frame 1 is the run's last step: nothing to read ahead yet.
    frame(&view, RUN, 1, "q", 0);
    owner.wait_for_read_ahead();
    for step in 2..10 {
        write_step(&source, "", RUN, step, &[("q", ECMWF), ("w", ECMWF)]);
    }
    publish(&source, &store);
    reads(&store);
    store.peak.store(0, Ordering::SeqCst);
    let owner = Arc::new(owner);
    let poll = std::thread::spawn({
        let owner = owner.clone();
        move || owner.scan_once().unwrap()
    });
    let waiting = Instant::now();
    while owner.prewarm_counts().0 == 0 {
        assert!(waiting.elapsed() < Duration::from_secs(10), "no pre-warm");
        std::thread::yield_now();
    }
    // The catalog is published: frame 1 plans again, from the cache.
    frame(&view, RUN, 1, "q", 0);
    poll.join().unwrap();
    owner.wait_for_read_ahead();
    assert_eq!(owner.prewarm_counts().0, 8, "w at steps 2–9");
    assert_eq!(owner.read_ahead_counts().0, 3, "q at steps 2–4");
    assert_eq!(reads(&store).len(), 11, "and nothing else");
    assert_eq!(
        store.peak.load(Ordering::SeqCst),
        2,
        "two in flight between them, never more"
    );

    let source = TestSource::new();
    write_q(&source, "", RUN, 0..12);
    // 16 MiB: a share of 4 MiB is seven of these messages.
    let (owner, view, store) = served(&source, 16, 0, 3, TestStore::default());
    let share = 16 * ds_cache::MIB / 100 * SHARE_PERCENT;
    assert_eq!(share / len(), 7);
    reads(&store);
    for step in 0..12 {
        frame(&view, RUN, step, "q", 0);
        owner.wait_for_read_ahead();
    }
    let mut expected: Vec<_> = (0..12)
        .map(|step| q("", step, (1..=7).contains(&step)))
        .collect();
    expected.sort();
    assert_eq!(
        reads(&store),
        expected,
        "steps 1–7 read ahead, every message once"
    );
    let (gets, bytes, hits) = owner.read_ahead_counts();
    assert_eq!((gets, bytes, hits), (7, 7 * len(), 7));
    assert!(bytes <= share);

    write_q(&source, "next-", NEXT_RUN, 0..4);
    publish(&source, &store);
    owner.scan_once().unwrap();
    reads(&store);
    frame(&view, NEXT_RUN, 0, "q", 0);
    owner.wait_for_read_ahead();
    assert_eq!(
        reads(&store),
        [
            q("next-", 0, false),
            q("next-", 1, true),
            q("next-", 2, true),
            q("next-", 3, true),
        ],
        "a new run reads ahead within a share of its own"
    );
}

/// No read-ahead when the config turns it off, without a message cache, or
/// for a parameter whose next steps the cache cannot hold together: at a
/// depth of 12, `q`'s 0.55 MB messages need 6.6 MB of a 4 MiB cache, whose
/// share would still take one of them, while small `w` is read ahead.
#[test]
fn no_read_ahead_when_off_or_when_the_cache_cannot_hold_the_steps_ahead() {
    let source = TestSource::new();
    let w = small();
    for step in 0..13 {
        write_step(&source, "", RUN, step, &[("q", ECMWF), ("w", &w)]);
    }
    for (message_mb, depth) in [(32, 0), (0, 3)] {
        let (owner, view, store) = served(&source, message_mb, 0, depth, TestStore::default());
        assert!(owner.source.read_ahead.is_none());
        reads(&store);
        for step in 0..3 {
            frame(&view, RUN, step, "q", 0);
        }
        let read = reads(&store);
        assert_eq!(read.len(), 3, "{message_mb} MiB, depth {depth}: {read:?}");
        assert!(read.iter().all(|read| !read.background));
        assert_eq!(owner.read_ahead_counts(), (0, 0, 0));
    }

    let (owner, view, store) = served(&source, 4, 0, 12, TestStore::default());
    reads(&store);
    frame(&view, RUN, 0, "q", 0);
    frame(&view, RUN, 0, "w", 0);
    owner.wait_for_read_ahead();
    let mut expected = vec![
        q("", 0, false),
        Read {
            file: file("", 0),
            offset: len(),
            length: w.len() as u64,
            background: false,
        },
    ];
    expected.extend((1..13).map(|step| Read {
        file: file("", step),
        offset: len(),
        length: w.len() as u64,
        background: true,
    }));
    expected.sort();
    assert_eq!(reads(&store), expected, "w at every later step, no q");
}

/// Only forward, and only what neither cache holds: a frame reads the
/// steps after it, the run's last frame nothing, and an earlier frame no
/// step whose decoded grid the grid cache holds.
#[test]
fn a_frame_reads_ahead_only_later_steps_that_neither_cache_holds() {
    let source = TestSource::new();
    write_q(&source, "", RUN, 0..8);
    let (owner, view, store) = served(&source, 32, 64, 3, TestStore::default());
    reads(&store);
    frame(&view, RUN, 4, "q", 0);
    owner.wait_for_read_ahead();
    assert_eq!(
        reads(&store),
        [
            q("", 4, false),
            q("", 5, true),
            q("", 6, true),
            q("", 7, true)
        ]
    );
    frame(&view, RUN, 7, "q", 0);
    owner.wait_for_read_ahead();
    assert_eq!(reads(&store), [], "the last step: nothing after it");

    // Steps 1 and 2 decoded into the grid cache, their messages not kept.
    let catalog = view.catalog();
    let run = catalog.latest_run().unwrap();
    let grids = owner.source.grid_cache.as_ref().unwrap();
    for step in [1, 2] {
        let drawn = view
            .map_message(&catalog, run, &run.steps[&step], Some("q"), Some(150.0))
            .unwrap();
        let path = ObjectPath::from(drawn.url);
        let grid = crate::reader::read_message(&owner.source.store, &path, drawn.entry).unwrap();
        grids.insert(drawn.url, drawn.entry.offset, Arc::new(grid));
    }
    reads(&store);
    frame(&view, RUN, 0, "q", 0);
    owner.wait_for_read_ahead();
    assert_eq!(
        reads(&store),
        [q("", 0, false), q("", 3, true)],
        "step 3 only: the grid cache holds 1 and 2"
    );
}

/// A parameter some steps lack (#1005), here `w` at the even steps only,
/// reads ahead the next steps carrying it, as the maps then select them,
/// never the steps between.
#[test]
fn a_parameter_some_steps_lack_reads_ahead_the_steps_that_carry_it() {
    let source = TestSource::new();
    for step in 0..8 {
        let fields: &[(&str, &[u8])] = if step % 2 == 0 {
            &[("q", ECMWF), ("w", ECMWF)]
        } else {
            &[("q", ECMWF)]
        };
        write_step(&source, "", RUN, step, fields);
    }
    let (owner, view, store) = served(&source, 32, 0, 3, TestStore::default());
    reads(&store);
    frame(&view, RUN, 0, "w", 0);
    owner.wait_for_read_ahead();
    let w = |step: u32, background: bool| Read {
        offset: len(),
        ..q("", step, background)
    };
    assert_eq!(
        reads(&store),
        [w(0, false), w(2, true), w(4, true), w(6, true)]
    );
}

/// The read-ahead admits a message without decoding it. One that does not
/// decode fails its frame's render, as it does read by the render itself,
/// and leaves the cache, so the next render fetches it again and draws it
/// once the store serves it whole.
#[test]
fn a_read_ahead_message_that_does_not_decode_is_fetched_again() {
    let source = TestSource::new();
    write_q(&source, "", RUN, 0..2);
    let (owner, view, store) = served(&source, 32, 0, 1, TestStore::default());
    let path = ObjectPath::from(file("", 1));
    let whole = std::fs::read(source.dir.join(file("", 1))).unwrap();
    let mut corrupt = whole.clone();
    corrupt[..4].copy_from_slice(b"XXXX");
    let put = |bytes: Vec<u8>| run_fetches(async { store.inner.put(&path, bytes.into()).await });
    put(corrupt).unwrap();
    reads(&store);
    frame(&view, RUN, 0, "q", 0);
    owner.wait_for_read_ahead();
    assert_eq!(reads(&store), [q("", 0, false), q("", 1, true)]);
    let render = view.get_raster_tile(
        [0.0, 0.0, 1.0, 1.0],
        4,
        4,
        Some(at(RUN, 1)),
        &OutputCrs::Wgs84,
        Some("q"),
        Some(150.0),
        None,
    );
    assert!(render.is_err(), "the corrupt message does not draw");
    assert_eq!(reads(&store), [], "read from the cache");
    put(whole).unwrap();
    let drawn = frame(&view, RUN, 1, "q", 0);
    assert_eq!(reads(&store), [q("", 1, false)], "fetched again");
    let (_, plain, _) = served(&source, 0, 0, 0, TestStore::default());
    assert_eq!(drawn, frame(&plain, RUN, 1, "q", 0));
}
