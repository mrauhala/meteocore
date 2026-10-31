//! The map read-ahead (#1335): what a map loop reads from storage with it,
//! what it fetches and when it fetches nothing, its bounds, where it runs,
//! how a request meets a fill in flight, and what its fills do to the
//! message cache.

use super::*;
use crate::point_fetch_tests::series;
use crate::runtime::run_fetches;
use crate::test_support::{message, store::TestStore, Ccsds, TestSource};
use ds_core::config::GribLevelType;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterTile, RasterValues};
use ds_storage::object_store::{path::Path as ObjectPath, GetRange, ObjectStoreExt};
use ds_storage::{bytes::Bytes, DataStore};
use std::sync::mpsc;
use std::sync::Arc;

/// A real CCSDS global field (0.55 MB).
const ECMWF: &[u8] = include_bytes!("../../../../testdata/grib-local/sample-message.grib2");
const RUN: &str = "2026040500";
const NORTH: (f64, f64) = (24.96, 60.32);
const MIB: u64 = ds_cache::MIB;

fn len() -> u64 {
    ECMWF.len() as u64
}

/// A small field at 150 hPa, about 200 bytes.
fn small() -> Vec<u8> {
    message(0, 250.0, [0, 2, 4, 6], 100, 15_000)
}

/// A CCSDS field of 1600 × 800 nodes at 0.1° from 0°E, 80°N, 16 noisy bits
/// a node: about 2.6 MB, which libaec cannot compress.
fn large() -> Vec<u8> {
    let field = Ccsds {
        ni: 1600,
        nj: 800,
        lat_first: 80_000_000,
        step: 100_000,
        ..Ccsds::global()
    };
    let mut state = 0x2545_f491_u32;
    let raw: Vec<u32> = (0..1600 * 800)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            state >> 16
        })
        .collect();
    field.message(&raw)
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

/// Put `bytes` at `path` of `store`.
fn put(store: &TestStore, path: &str, bytes: Vec<u8>) {
    run_fetches(async {
        let path = ObjectPath::from(path);
        store.inner.put(&path, bytes.into()).await.unwrap();
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

/// One tile of `parameter`'s frame at `step` of the run `RUN`.
fn render(
    view: &GribEngine,
    step: u32,
    parameter: &str,
    tile: usize,
) -> Result<RasterTile, DataServerError> {
    let west = tile as f64;
    view.get_raster_tile(
        [west, 0.0, west + 1.0, 1.0],
        4,
        4,
        Some(at(RUN, step)),
        &OutputCrs::Wgs84,
        Some(parameter),
        Some(150.0),
        None,
    )
}

/// [`render`], as bits.
fn frame(view: &GribEngine, step: u32, parameter: &str, tile: usize) -> Vec<u32> {
    let RasterValues::F32 { data, .. } = render(view, step, parameter, tile).unwrap().values else {
        panic!("a GRIB map tile is f32");
    };
    data.iter().map(|v| v.to_bits()).collect()
}

/// A client animating `parameter` over `steps`, one tile, slower than the
/// store: each frame waits for the read-ahead it started.
fn animate(owner: &GribEngine, view: &GribEngine, steps: std::ops::Range<u32>, parameter: &str) {
    for step in steps {
        frame(view, step, parameter, 0);
        owner.wait_for_read_ahead();
    }
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

fn file(step: u32) -> String {
    format!("f{step:03}.grib2")
}

/// The first message of step `step`, `length` bytes, read by a request or
/// in the background.
fn whole(step: u32, length: u64, background: bool) -> Read {
    Read {
        file: file(step),
        offset: 0,
        length,
        background,
    }
}

/// [`whole`] for the real field.
fn q(step: u32, background: bool) -> Read {
    whole(step, len(), background)
}

/// `steps` read by requests, then `ahead` read ahead, `length` each.
fn expected(steps: &[u32], ahead: std::ops::Range<u32>, length: u64) -> Vec<Read> {
    let mut expected: Vec<_> = steps.iter().map(|&s| whole(s, length, false)).collect();
    expected.extend(ahead.map(|s| whole(s, length, true)));
    expected.sort();
    expected
}

/// The message key of `parameter` at step `step` in `view`.
fn key_of(view: &GribEngine, step: u32, parameter: &str) -> MessageKey {
    let catalog = view.catalog();
    let run = catalog.latest_run().unwrap();
    let drawn = view
        .map_message(
            &catalog,
            run,
            &run.steps[&step],
            Some(parameter),
            Some(150.0),
        )
        .unwrap();
    MessageKey::new(Path::from(drawn.url).as_ref(), drawn.entry)
}

fn cache(engine: &GribEngine) -> &MessageCache {
    engine.source.message_cache.as_ref().unwrap()
}

fn filler() -> MessageEntry {
    MessageEntry {
        source_url: None,
        param: "x".into(),
        step_kind: crate::wgrib2_index::StepKind::Instant,
        levtype: "pl".into(),
        level: Some(150),
        offset: 0,
        length: None,
    }
}

/// Fill `engine`'s hot ring: 36 messages of 1 MiB, each read twice, all
/// it holds of a 64 MiB message cache. What arrives next waits in the cold
/// ring, which the hot share leaves 28.5 MB.
fn heat(engine: &GribEngine, store: &TestStore) {
    put(store, "hot.grib2", vec![7; (36 * MIB) as usize]);
    let path = ObjectPath::from("hot.grib2");
    let accept = |_: &[u8]| Ok::<_, DataServerError>(());
    for _ in 0..2 {
        for i in 0..36 {
            let entry = MessageEntry {
                offset: i * MIB,
                length: Some(MIB),
                ..filler()
            };
            cache(engine)
                .get_with(&engine.source.store, &path, &entry, false, accept)
                .unwrap();
        }
    }
    let hot = cache(engine).metrics();
    assert_eq!((hot.hits, hot.churn.evicted_cold), (36, 0), "{hot:?}");
}

/// The acceptance case: a client stepping through a cold run, one frame
/// after another and each of three tiles, waits on storage for its first
/// two frames only, the second showing it animates. The read-ahead reads
/// every later step's message once, on the GRIB I/O runtime, planned once
/// per frame, and every one is read; the frames equal those of an engine
/// that reads each itself.
#[test]
fn a_sequential_map_loop_over_a_cold_run_reads_storage_only_for_its_first_frames() {
    let source = TestSource::new();
    write_q(&source, "", RUN, 0..8);
    let (owner, view, store) = served(&source, 64, 0, 3, TestStore::default());
    let (_, plain, _) = served(&source, 64, 0, 0, TestStore::default());
    reads(&store);
    let mut frames = Vec::new();
    for step in 0..8 {
        let tiles: Vec<_> = (0..3).map(|tile| frame(&view, step, "q", tile)).collect();
        frames.push(tiles);
        // A client slower than the store: the next step is fetched by
        // the time it asks.
        owner.wait_for_read_ahead();
    }
    assert_eq!(reads(&store), expected(&[0, 1], 2..8, len()));
    assert_eq!(owner.read_ahead_counts(), (6, 6 * len(), 6));
    let ahead = owner.source.read_ahead.as_ref().unwrap();
    assert_eq!(
        ahead.planned.lock().unwrap().len(),
        7,
        "once per frame after the first"
    );
    for (step, tiles) in frames.iter().enumerate() {
        for (tile, values) in tiles.iter().enumerate() {
            assert_eq!(*values, frame(&plain, step as u32, "q", tile));
        }
    }
}

/// Only an animation reads ahead: a view drawing one step, however often
/// and wherever it pans, costs no speculative GET, nor does a step drawn
/// a minute after the one before it; a view drawing the next step within
/// the minute reads the steps after it.
#[test]
fn a_view_drawing_one_step_reads_nothing_ahead() {
    let source = TestSource::new();
    write_q(&source, "", RUN, 0..6);
    let (owner, view, store) = served(&source, 64, 0, 3, TestStore::default());
    reads(&store);
    for tile in [0, 1, 2, 0, 0] {
        frame(&view, 0, "q", tile);
    }
    owner.wait_for_read_ahead();
    assert_eq!(reads(&store), [q(0, false)], "one step, many tiles");
    let ahead = owner.source.read_ahead.as_ref().unwrap();
    ahead.age(ANIMATION_WINDOW + Duration::from_secs(1));
    frame(&view, 1, "q", 0);
    owner.wait_for_read_ahead();
    assert_eq!(reads(&store), [q(1, false)], "the step before, long ago");
    frame(&view, 2, "q", 0);
    owner.wait_for_read_ahead();
    assert_eq!(reads(&store), expected(&[2], 3..6, len()));
}

/// The render that plans the read-ahead returns before its GETs do, here
/// two seconds each: it waits on none of them.
#[test]
fn the_triggering_render_does_not_wait_for_the_read_ahead() {
    let source = TestSource::new();
    write_q(&source, "", RUN, 0..4);
    let slow = Duration::from_secs(2);
    let store = TestStore {
        delays: [(file(2), slow), (file(3), slow)].into(),
        ..Default::default()
    };
    let (owner, view, store) = served(&source, 64, 0, 2, store);
    reads(&store);
    frame(&view, 0, "q", 0);
    let started = Instant::now();
    frame(&view, 1, "q", 0);
    let rendered = started.elapsed();
    let completed = store.reads.lock().unwrap().completed.clone();
    assert!(
        !completed.contains(&file(2)) && !completed.contains(&file(3)),
        "the render returned after {rendered:?}, past a read-ahead GET: {completed:?}"
    );
    owner.wait_for_read_ahead();
    assert!(started.elapsed() >= slow);
    assert_eq!(reads(&store), expected(&[0, 1], 2..4, len()));
}

/// A client asking for the next frame while its read-ahead GET is in
/// flight joins that GET: the message is read once, and the read counts.
#[test]
fn a_request_joins_the_read_ahead_fill_in_flight() {
    let source = TestSource::new();
    write_q(&source, "", RUN, 0..3);
    let store = TestStore {
        delays: [(file(2), Duration::from_millis(500))].into(),
        ..Default::default()
    };
    let (owner, view, store) = served(&source, 64, 0, 1, store);
    reads(&store);
    frame(&view, 0, "q", 0);
    frame(&view, 1, "q", 0);
    let waiting = Instant::now();
    while store.active.load(Ordering::SeqCst) == 0 {
        assert!(waiting.elapsed() < Duration::from_secs(10), "no read-ahead");
        std::thread::yield_now();
    }
    let drawn = frame(&view, 2, "q", 0);
    owner.wait_for_read_ahead();
    assert_eq!(reads(&store), expected(&[0, 1], 2..3, len()));
    assert_eq!(owner.read_ahead_counts(), (1, len(), 1));
    let (_, plain, _) = served(&source, 0, 0, 0, TestStore::default());
    assert_eq!(drawn, frame(&plain, 2, "q", 0));
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
    put(&store, path.as_ref(), ECMWF.repeat(2));
    let data = DataStore::new(store.clone());
    let cache = Arc::new(MessageCache::new(8).unwrap());
    let entry = |offset: u64| MessageEntry {
        offset,
        length: Some(len()),
        ..filler()
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

/// Scan resistance (#1340): a map's own fill is never looked up again, so
/// it leaves the cold ring unread and an animation pass evicts nothing the
/// hot ring keeps. A read-ahead fill read once must leave it the same way:
/// over a 64 MiB cache whose hot ring holds 36 messages read twice, a pass
/// of 60 frames of 1 MiB keeps all of them, with or without read-ahead.
#[test]
fn an_animation_read_ahead_keeps_what_the_hot_ring_holds() {
    let survivors = |read_ahead: bool| {
        let store = TestStore::default();
        put(&store, "hot.grib2", vec![1; (36 * MIB) as usize]);
        put(&store, "frames.grib2", vec![2; (60 * MIB) as usize]);
        let data = DataStore::new(Arc::new(store));
        let cache = MessageCache::new(64).unwrap();
        let accept = |_: &[u8]| Ok::<_, DataServerError>(());
        let entry = |i: u64| MessageEntry {
            offset: i * MIB,
            length: Some(MIB),
            ..filler()
        };
        let hot = ObjectPath::from("hot.grib2");
        for _ in 0..2 {
            for i in 0..36 {
                cache
                    .get_with(&data, &hot, &entry(i), false, accept)
                    .unwrap();
            }
        }
        let frames = ObjectPath::from("frames.grib2");
        for i in 0..60 {
            if read_ahead {
                let key = MessageKey::new(frames.as_ref(), &entry(i));
                let bytes = Bytes::from(vec![2; MIB as usize]);
                cache.read_ahead(&key, || Ok(bytes)).unwrap();
            }
            let (_, validated) = cache
                .get_with(&data, &frames, &entry(i), false, accept)
                .unwrap();
            assert_eq!(validated.is_none(), read_ahead, "frame {i}");
        }
        (0..36)
            .filter(|&i| cache.is_resident(&MessageKey::new(hot.as_ref(), &entry(i))))
            .count()
    };
    assert_eq!(survivors(false), 36);
    assert_eq!(survivors(true), 36);
}

/// The room, over a `TestStore` replay. A loop of 2.6 MB frames, depth 12,
/// in a 64 MiB cache whose hot ring is full: what the read-ahead has
/// queued, in flight and unread stays within half the cold ring, five of
/// these messages, so each next frame is resident when the client asks,
/// and a frame read gives its room back, so every later step is read
/// ahead, not just the first five. A client stopping leaves five unread;
/// once the cache evicts them, a second pass after the plans expire reads
/// ahead again.
#[test]
fn a_loop_of_large_frames_in_a_full_cache_reads_ahead_within_half_the_cold_ring() {
    let large = large();
    let length = large.len() as u64;
    let source = TestSource::new();
    for step in 0..13 {
        write_step(&source, "", RUN, step, &[("q", &large)]);
    }
    let (owner, view, store) = served(&source, 64, 0, 12, TestStore::default());
    let cache = cache(&owner);
    let room = ReadAhead::room(cache);
    let weight = message_weight(&file(0), length);
    assert_eq!(room / weight, 5);
    assert!(room <= cache.cold_room() / 2);
    heat(&owner, &store);
    reads(&store);
    let ahead = owner.source.read_ahead.as_ref().unwrap();
    let pass = |steps: std::ops::Range<u32>| {
        for step in steps {
            if step >= 2 {
                let key = key_of(&view, step, "q");
                assert!(
                    cache.is_resident(&key),
                    "step {step} evicted before it is drawn"
                );
            }
            frame(&view, step, "q", 0);
            owner.wait_for_read_ahead();
            let outstanding = ahead.outstanding(cache);
            assert!(outstanding <= room, "{outstanding} past {room}");
        }
    };
    pass(0..7);
    assert_eq!(reads(&store), expected(&[0, 1], 2..12, length));
    assert_eq!(ahead.outstanding(cache), 5 * weight, "steps 7–11 unread");

    cache.clear();
    ahead.age(PLANNED_FOR + Duration::from_secs(1));
    assert_eq!(ahead.outstanding(cache), 0, "evicted unread: room again");
    pass(0..13);
    assert_eq!(reads(&store), expected(&[0, 1], 2..13, length));
    assert_eq!(owner.read_ahead_counts().0, 10 + 11);
}

/// A map of `q` reads ahead while a poll pre-warms `w` for position
/// clients: two GETs in flight between them, never more.
#[test]
fn the_read_ahead_keeps_two_gets_in_flight_with_the_prewarm() {
    let source = TestSource::new();
    for step in 0..2 {
        write_step(&source, "", RUN, step, &[("q", ECMWF), ("w", ECMWF)]);
    }
    let store = TestStore {
        delay: Duration::from_millis(100),
        ..Default::default()
    };
    let (owner, view, store) = served(&source, 64, 0, 3, store);
    series(&view, "w", &[NORTH], RUN, None);
    // Frame 1 is the run's last step: nothing to read ahead yet.
    animate(&owner, &view, 0..2, "q");
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
    frame(&view, 1, "q", 0);
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
}

/// No read-ahead when the config turns it off, without a message cache, or
/// for a parameter whose messages the room cannot take: a 32 MiB cache
/// keeps the default 3 % cold ring, half of which, 0.48 MiB, takes small
/// `w` but no 0.55 MB `q`.
#[test]
fn no_read_ahead_when_off_or_when_the_room_cannot_take_a_message() {
    let source = TestSource::new();
    let w = small();
    for step in 0..13 {
        write_step(&source, "", RUN, step, &[("q", ECMWF), ("w", &w)]);
    }
    for (message_mb, depth) in [(64, 0), (0, 3)] {
        let (owner, view, store) = served(&source, message_mb, 0, depth, TestStore::default());
        assert!(owner.source.read_ahead.is_none());
        reads(&store);
        animate(&owner, &view, 0..3, "q");
        assert_eq!(
            reads(&store),
            expected(&[0, 1, 2], 0..0, len()),
            "{message_mb} MiB, depth {depth}"
        );
        assert_eq!(owner.read_ahead_counts(), (0, 0, 0));
    }

    let (owner, view, store) = served(&source, 32, 0, 12, TestStore::default());
    reads(&store);
    animate(&owner, &view, 0..2, "q");
    animate(&owner, &view, 0..2, "w");
    let w = |step: u32, background: bool| Read {
        offset: len(),
        length: w.len() as u64,
        ..q(step, background)
    };
    let mut expected = vec![q(0, false), q(1, false), w(0, false), w(1, false)];
    expected.extend((2..13).map(|step| w(step, true)));
    expected.sort();
    assert_eq!(reads(&store), expected, "w at every later step, no q");
}

/// Only forward, and only what neither cache holds: a frame reads the
/// steps after it, none whose decoded grid the grid cache holds; the
/// run's last frame and an animation stepping back read nothing ahead.
#[test]
fn a_frame_reads_ahead_only_later_steps_that_neither_cache_holds() {
    let source = TestSource::new();
    write_q(&source, "", RUN, 0..8);
    let (owner, view, store) = served(&source, 64, 64, 3, TestStore::default());
    // Steps 2 and 3 decoded into the grid cache, their messages not kept.
    let grids = owner.source.grid_cache.as_ref().unwrap();
    let catalog = view.catalog();
    let run = catalog.latest_run().unwrap();
    for step in [2, 3] {
        let drawn = view
            .map_message(&catalog, run, &run.steps[&step], Some("q"), Some(150.0))
            .unwrap();
        let path = ObjectPath::from(drawn.url);
        let grid = crate::reader::read_message(&owner.source.store, &path, drawn.entry).unwrap();
        grids.insert(drawn.url, drawn.entry.offset, Arc::new(grid));
    }
    reads(&store);
    animate(&owner, &view, 0..2, "q");
    assert_eq!(
        reads(&store),
        expected(&[0, 1], 4..5, len()),
        "step 4 only: the grid cache holds 2 and 3"
    );
    animate(&owner, &view, 6..8, "q");
    assert_eq!(
        reads(&store),
        expected(&[6, 7], 0..0, len()),
        "the last step"
    );
    for step in [5, 4] {
        frame(&view, step, "q", 0);
    }
    owner.wait_for_read_ahead();
    assert_eq!(reads(&store), [q(5, false)], "stepping back");
}

/// A parameter some steps lack (#1005), here `w` at the even steps only:
/// a frame after the one before it among them reads ahead the next steps
/// carrying it, as the maps then select them, never the steps between.
#[test]
fn a_parameter_some_steps_lack_reads_ahead_the_steps_that_carry_it() {
    let source = TestSource::new();
    for step in 0..10 {
        let fields: &[(&str, &[u8])] = if step % 2 == 0 {
            &[("q", ECMWF), ("w", ECMWF)]
        } else {
            &[("q", ECMWF)]
        };
        write_step(&source, "", RUN, step, fields);
    }
    let (owner, view, store) = served(&source, 64, 0, 3, TestStore::default());
    reads(&store);
    for step in [0, 2] {
        frame(&view, step, "w", 0);
        owner.wait_for_read_ahead();
    }
    let w = |step: u32, background: bool| Read {
        offset: len(),
        ..q(step, background)
    };
    assert_eq!(
        reads(&store),
        [w(0, false), w(2, false), w(4, true), w(6, true), w(8, true)]
    );
}

/// The read-ahead admits a message without decoding it. One that does not
/// decode fails its frame's render, as it does read by the render itself,
/// and leaves the cache, so the next render fetches it again and draws it
/// once the store serves it whole.
#[test]
fn a_read_ahead_message_that_does_not_decode_is_fetched_again() {
    let source = TestSource::new();
    write_q(&source, "", RUN, 0..3);
    let (owner, view, store) = served(&source, 64, 0, 1, TestStore::default());
    let whole = std::fs::read(source.dir.join(file(2))).unwrap();
    let mut corrupt = whole.clone();
    corrupt[..4].copy_from_slice(b"XXXX");
    put(&store, &file(2), corrupt);
    reads(&store);
    animate(&owner, &view, 0..2, "q");
    assert_eq!(reads(&store), expected(&[0, 1], 2..3, len()));
    assert!(
        render(&view, 2, "q", 0).is_err(),
        "the corrupt message does not draw"
    );
    assert_eq!(reads(&store), [], "read from the cache");
    assert!(!cache(&owner).is_resident(&key_of(&view, 2, "q")));
    put(&store, &file(2), whole);
    let drawn = frame(&view, 2, "q", 0);
    assert_eq!(reads(&store), [q(2, false)], "fetched again");
    let (_, plain, _) = served(&source, 0, 0, 0, TestStore::default());
    assert_eq!(drawn, frame(&plain, 2, "q", 0));
}

/// The same through the point path: a position query over a read-ahead
/// fill it cannot index or decode samples nothing there, and the fill
/// leaves the cache, so the next query fetches the message again.
#[test]
fn a_read_ahead_fill_a_position_query_cannot_parse_is_fetched_again() {
    let source = TestSource::new();
    write_q(&source, "", RUN, 0..3);
    let (owner, view, store) = served(&source, 64, 0, 1, TestStore::default());
    let whole = std::fs::read(source.dir.join(file(2))).unwrap();
    let mut corrupt = whole.clone();
    corrupt[..4].copy_from_slice(b"XXXX");
    put(&store, &file(2), corrupt);
    animate(&owner, &view, 0..2, "q");
    reads(&store);
    let failed = series(&view, "q", &[NORTH], RUN, Some((2, 2)));
    assert_eq!(failed, [vec![None]]);
    assert_eq!(reads(&store), [], "read from the cache");
    assert!(!cache(&owner).is_resident(&key_of(&view, 2, "q")));
    put(&store, &file(2), whole);
    let sampled = series(&view, "q", &[NORTH], RUN, Some((2, 2)));
    // Off a server runtime a query's field jobs run on the GRIB I/O
    // runtime too: the read is the query's, not a read-ahead's.
    assert_eq!(reads(&store), [q(2, true)], "fetched again");
    assert_eq!(owner.read_ahead_counts().0, 1);
    let (plain, _) = crate::point_fetch_tests::counted(&source, 16, 0, 0);
    assert_eq!(sampled, series(&plain, "q", &[NORTH], RUN, Some((2, 2))));
}

/// Only a fill the read-ahead admitted undecoded leaves when a reader
/// cannot parse it, and only until one has: a message a request admitted
/// after validating it, or one a reader already accepted, stays, as does
/// one whose reader ran out of time.
#[test]
fn only_an_unchecked_read_ahead_fill_leaves_when_a_reader_cannot_parse_it() {
    let cache = MessageCache::new(8).unwrap();
    let store = TestStore::default();
    put(&store, "f000.grib2", ECMWF.repeat(4));
    let data = DataStore::new(Arc::new(store));
    let path = ObjectPath::from("f000.grib2");
    let entry = |i: u64| MessageEntry {
        offset: i * len(),
        length: Some(len()),
        ..filler()
    };
    let key = |i: u64| MessageKey::new(path.as_ref(), &entry(i));
    let corrupt = Err::<(), _>(DataServerError::Engine("corrupt".into()));
    let accept = |_: &[u8]| Ok::<_, DataServerError>(());
    cache
        .get_with(&data, &path, &entry(0), false, accept)
        .unwrap();
    for i in 1..4 {
        let bytes = Bytes::from_static(ECMWF);
        cache.read_ahead(&key(i), || Ok(bytes)).unwrap();
    }
    cache.decoded(&key(0), &corrupt);
    cache.decoded(&key(1), &corrupt);
    cache.decoded(&key(2), &Ok(()));
    cache.decoded(&key(2), &corrupt);
    cache.decoded(&key(3), &Err::<(), _>(DataServerError::DeadlineExceeded));
    let resident: Vec<_> = (0..4).map(|i| cache.is_resident(&key(i))).collect();
    assert_eq!(resident, [true, false, true, true]);
}

/// A fill the cache evicted unread, which a map then reads itself, is no
/// read-ahead hit, nor are the map's later reads of its own copy.
#[test]
fn a_fill_evicted_unread_counts_no_hit_when_the_map_reads_it_again() {
    let source = TestSource::new();
    write_q(&source, "", RUN, 0..3);
    let (owner, view, store) = served(&source, 64, 0, 1, TestStore::default());
    animate(&owner, &view, 0..2, "q");
    assert_eq!(owner.read_ahead_counts().0, 1);
    cache(&owner).clear();
    reads(&store);
    for _ in 0..2 {
        frame(&view, 2, "q", 0);
    }
    assert_eq!(reads(&store), [q(2, false)]);
    assert_eq!(owner.read_ahead_counts().2, 0);
}

/// A fetch that panics still leaves the queue's bookkeeping: its message
/// is no longer pending, its weight no longer queued, its worker gone.
#[test]
fn a_fetch_that_panics_gives_its_room_back() {
    let cache = MessageCache::new(64).unwrap();
    let ahead = ReadAhead::new(3, Some(&cache)).unwrap();
    let entry = MessageEntry {
        length: Some(len()),
        ..filler()
    };
    let fetch = Fetch {
        url: file(1),
        key: MessageKey::new(&file(1), &entry),
        entry,
        weight: len(),
    };
    assert_eq!(ahead.enqueue(vec![fetch], u64::MAX, 0), 1);
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut worker = Worker {
            ahead: &ahead,
            running: true,
        };
        let _job = ahead.next(&mut worker).unwrap();
        panic!("a fetch panicked");
    }));
    assert!(unwound.is_err());
    let state = ahead.state.lock().unwrap();
    assert!(state.pending.is_empty());
    assert_eq!((state.queued, state.workers), (0, 0));
}

/// The room is held against what is outstanding, not spent for the run's
/// lifetime. A pass over a run reads ahead more than a quarter of the
/// cache, and after its messages leave the cache and a poll publishes the
/// catalog again, a second pass over the same run reads ahead again.
#[test]
fn the_room_is_not_spent_for_the_runs_lifetime() {
    let large = large();
    let length = large.len() as u64;
    let source = TestSource::new();
    for step in 0..12 {
        write_step(&source, "", RUN, step, &[("q", &large)]);
    }
    let (owner, view, store) = served(&source, 64, 0, 3, TestStore::default());
    let share = 64 * MIB / 100 * SHARE_PERCENT;
    reads(&store);
    animate(&owner, &view, 0..12, "q");
    assert_eq!(reads(&store), expected(&[0, 1], 2..12, length));
    assert!(
        owner.read_ahead_counts().1 > share,
        "past a quarter of the cache"
    );
    cache(&owner).clear();
    owner.source.read_ahead.as_ref().unwrap().forget_plans();
    animate(&owner, &view, 0..12, "q");
    assert_eq!(reads(&store), expected(&[0, 1], 2..12, length));
    assert_eq!(owner.read_ahead_counts().0, 2 * 10);
}

/// Fills nobody reads stop holding the room after `UNREAD_FOR`, though a
/// cache under no pressure keeps them: an animation of `q` abandoned with
/// the room's five 2.6 MB fills unread leaves an animation of `w` nothing
/// to read ahead, until those fills are two minutes old.
#[test]
fn abandoned_fills_stop_holding_the_room_after_a_while() {
    let large = large();
    let length = large.len() as u64;
    let source = TestSource::new();
    for step in 0..9 {
        write_step(&source, "", RUN, step, &[("q", &large), ("w", &large)]);
    }
    let (owner, view, store) = served(&source, 64, 0, 12, TestStore::default());
    animate(&owner, &view, 0..2, "q");
    assert_eq!(
        owner.read_ahead_counts().0,
        5,
        "the room's five, then abandoned"
    );
    reads(&store);
    let w = |step: u32, background: bool| Read {
        offset: length,
        ..whole(step, length, background)
    };
    animate(&owner, &view, 0..2, "w");
    assert_eq!(
        reads(&store),
        [w(0, false), w(1, false)],
        "the room is full"
    );
    cache(&owner).age_read_ahead(UNREAD_FOR + Duration::from_secs(1));
    animate(&owner, &view, 2..3, "w");
    let mut expected = vec![w(2, false)];
    expected.extend((3..8).map(|step| w(step, true)));
    assert_eq!(reads(&store), expected);
    let unread = (2..7).filter(|&s| cache(&owner).is_resident(&key_of(&view, s, "q")));
    assert_eq!(unread.count(), 5, "the abandoned fills stay cached");
}
