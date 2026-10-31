use super::*;
use crate::runtime::run_fetches;
use crate::test_support::{message, store::TestStore, TestSource};
use crate::GribEngine;
use ds_core::{
    config::GribLevelType,
    edr_engine::EdrEngine,
    map_engine::{MapEngine, OutputCrs},
    model::{CoverageResponse, DomainDescription},
};
use ds_storage::object_store::ObjectStoreExt;
use std::{
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

const ECMWF: &[u8] = include_bytes!("../../../../testdata/grib-local/sample-message.grib2");

fn entry(length: Option<usize>) -> MessageEntry {
    MessageEntry {
        source_url: None,
        param: "TMP".into(),
        levtype: "sfc".into(),
        level: None,
        offset: 0,
        length: length.map(|n| n as u64),
        step_kind: crate::wgrib2_index::StepKind::Instant,
    }
}

fn instrumented(delay: Duration) -> Arc<TestStore> {
    Arc::new(TestStore {
        suffix: ".grib2",
        delay,
        allow_heads: true,
        ..Default::default()
    })
}

async fn put(store: &TestStore, path: &Path, bytes: &[u8]) {
    store
        .inner
        .put(path, Bytes::copy_from_slice(bytes).into())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cached_ranges_keep_paths_offsets_and_lengths_separate_and_skip_tail_head() {
    let cache = MessageCache::new(1).unwrap();
    let observed = instrumented(Duration::ZERO);
    let store = DataStore::new(observed.clone());
    let path = Path::from("fields.grib2");
    let other = Path::from("other.grib2");
    let first = message(0, 280.0, [0; 4], 1, 0);
    let second = message(0, 290.0, [0; 4], 1, 0);
    put(&observed, &path, &[first.clone(), second.clone()].concat()).await;
    put(&observed, &other, &second).await;
    let mut tail = entry(None);
    tail.offset = first.len() as u64;
    for _ in 0..2 {
        assert_eq!(
            cache
                .read(&store, &path, &entry(Some(first.len())))
                .unwrap()
                .values[0],
            280.0
        );
        assert_eq!(cache.read(&store, &path, &tail).unwrap().values[0], 290.0);
        assert_eq!(
            cache
                .read(&store, &other, &entry(Some(second.len())))
                .unwrap()
                .values[0],
            290.0
        );
    }
    assert_eq!(store.bytes_read(), 3 * first.len() as u64);
    assert_eq!(observed.heads.load(Ordering::SeqCst), 1);
    assert_eq!(cache.metrics().hits, 3);
    // A changed index length must miss, even at an already cached offset.
    assert!(cache.read(&store, &path, &entry(Some(1))).is_err());
    assert_eq!(store.bytes_read(), 3 * first.len() as u64 + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_or_invalid_fills_are_retryable_and_ranges_are_checked() {
    let valid = message(0, 280.0, [0; 4], 1, 0);
    let path = Path::from("retry.grib2");
    for failure in ["get", "short", "corrupt", "tail"] {
        let cache = MessageCache::new(1).unwrap();
        let observed = instrumented(Duration::ZERO);
        let store = DataStore::new(observed.clone());
        let bytes = match failure {
            "short" | "tail" => valid[..10].to_vec(),
            "corrupt" => vec![0; valid.len()],
            _ => valid.clone(),
        };
        put(&observed, &path, &bytes).await;
        if failure == "get" {
            observed
                .reads
                .lock()
                .unwrap()
                .fail_once
                .insert(path.to_string());
        }
        let request = entry(if failure == "tail" {
            None
        } else {
            Some(valid.len())
        });
        assert!(cache.read(&store, &path, &request).is_err(), "{failure}");
        assert_eq!(cache.metrics().bytes, 0);
        put(&observed, &path, &valid).await;
        assert_eq!(
            cache.read(&store, &path, &request).unwrap().values[0],
            280.0
        );
        let bytes = store.bytes_read();
        cache.read(&store, &path, &request).unwrap();
        assert_eq!(store.bytes_read(), bytes);
        assert_eq!((cache.metrics().hits, cache.metrics().misses), (1, 2));
    }

    let cache = MessageCache::new(1).unwrap();
    let observed = instrumented(Duration::ZERO);
    let store = DataStore::new(observed.clone());
    for (offset, length) in [(u64::MAX, 2), (0, 0)] {
        let mut request = entry(Some(length));
        request.offset = offset;
        assert!(matches!(
            cache.read(&store, &path, &request),
            Err(DataServerError::Storage(_))
        ));
    }
    assert!(observed.reads.lock().unwrap().attempts.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_bounds_payloads_and_does_not_retain_a_whole_file_for_a_range() {
    assert!(MessageCache::new(0).is_none());
    let cache = MessageCache::new(1).unwrap();
    let observed = instrumented(Duration::ZERO);
    let store = DataStore::new(observed.clone());
    for number in 0..6 {
        let path = Path::from(format!("f{number}.grib2"));
        put(&observed, &path, ECMWF).await;
        cache
            .read(&store, &path, &entry(Some(ECMWF.len())))
            .unwrap();
        assert!(cache.metrics().bytes <= MIB);
    }
    assert!(
        cache.cache.len() < 6,
        "the byte budget must evict older fields"
    );

    let small = message(0, 280.0, [0; 4], 1, 0);
    let mut whole_file = small.clone();
    whole_file.resize(2 * MIB as usize, 0);
    let path = Path::from("large-file.grib2");
    put(&observed, &path, &whole_file).await;
    let backing = observed
        .inner
        .get(&path)
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    cache
        .read(&store, &path, &entry(Some(small.len())))
        .unwrap();
    let retained = cache
        .cache
        .get_untracked(&MessageKey {
            path: Arc::from(path.as_ref()),
            offset: 0,
            length: Some(small.len() as u64),
        })
        .unwrap();
    assert_ne!(
        retained.as_ptr(),
        backing.as_ptr(),
        "retain an owned message, not a slice of the full object"
    );
    assert_eq!(retained.as_ref(), small);

    // A valid message with a large local-use section exceeds the whole budget.
    let mut section = vec![0; 2 * MIB as usize];
    let section_len = section.len() as u32;
    section[..4].copy_from_slice(&section_len.to_be_bytes());
    section[4] = 2;
    let mut large = small[..37].to_vec();
    large.extend(section);
    large.extend_from_slice(&small[37..]);
    let size = large.len() as u64;
    large[8..16].copy_from_slice(&size.to_be_bytes());
    let path = Path::from("oversized.grib2");
    put(&observed, &path, &large).await;
    let cache = MessageCache::new(1).unwrap();
    let before = store.bytes_read();
    for _ in 0..2 {
        assert_eq!(
            cache
                .read(&store, &path, &entry(Some(large.len())))
                .unwrap()
                .values[0],
            280.0
        );
    }
    assert_eq!(cache.metrics().bytes, 0);
    assert_eq!(store.bytes_read() - before, 2 * size);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_misses_share_one_download_and_deadlines_do_not_fill_the_cache() {
    let observed = instrumented(Duration::from_millis(20));
    let store = DataStore::new(observed.clone());
    let path = Path::from("shared.grib2");
    let bytes = message(0, 280.0, [0; 4], 1, 0);
    put(&observed, &path, &bytes).await;
    let cache = Arc::new(MessageCache::new(1).unwrap());
    let barrier = Arc::new(tokio::sync::Barrier::new(8));
    let mut jobs = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let (cache, store, path, barrier) =
            (cache.clone(), store.clone(), path.clone(), barrier.clone());
        let request = entry(Some(bytes.len()));
        jobs.spawn(async move {
            barrier.wait().await;
            tokio::task::block_in_place(|| cache.read(&store, &path, &request))
                .unwrap()
                .values[0]
        });
    }
    while let Some(result) = jobs.join_next().await {
        assert_eq!(result.unwrap(), 280.0);
    }
    assert_eq!(observed.reads.lock().unwrap().attempts[path.as_ref()], 1);
    assert_eq!((cache.metrics().hits, cache.metrics().misses), (7, 1));

    // Even cached data must respect an already-expired request deadline.
    {
        let _deadline = deadline::enter(Some(Instant::now()));
        assert!(matches!(
            cache.read(&store, &path, &entry(Some(bytes.len()))),
            Err(DataServerError::DeadlineExceeded)
        ));
    }
    let slow = instrumented(Duration::from_secs(30));
    put(&slow, &path, &bytes).await;
    let slow_store = DataStore::new(slow.clone());
    let empty = MessageCache::new(1).unwrap();
    {
        let _deadline = deadline::enter(Some(Instant::now() + Duration::from_millis(50)));
        assert!(empty
            .read(&slow_store, &path, &entry(Some(bytes.len())))
            .is_err());
    }
    assert_eq!(empty.metrics().bytes, 0);
    assert_eq!(slow.active.load(Ordering::SeqCst), 0);
    // The same key can be filled after the failed/deadline-limited read.
    empty
        .read(&store, &path, &entry(Some(bytes.len())))
        .unwrap();
    assert!(empty.metrics().bytes > 0);
}

/// #1340: in a full message cache, a window of messages one point query
/// filled is resident at its next run, for every window [`MessageCache::holds`]
/// approves. The cache is full of fills nobody read again, a map's or an
/// earlier query's; not kept, as those are, a window keeps what the cold
/// ring holds, 3 % of the budget: two of these.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_cache_keeps_every_window_holds_approves() {
    const SIZE: usize = 16 * 1024;
    let observed = instrumented(Duration::ZERO);
    let store = DataStore::new(observed.clone());
    let (old, new) = (Path::from("old.grib2"), Path::from("window.grib2"));
    put(&observed, &old, &vec![1; 128 * SIZE]).await;
    put(&observed, &new, &vec![2; 128 * SIZE]).await;
    let at = |offset: usize| MessageEntry {
        offset: (offset * SIZE) as u64,
        ..entry(Some(SIZE))
    };
    let held = |window| {
        let cache = MessageCache::new(1).unwrap();
        for offset in 0..128 {
            cache
                .get_with(&store, &old, &at(offset), false, |_| Ok(()))
                .unwrap();
        }
        assert!(cache.metrics().bytes > MIB - 2 * SIZE as u64, "full");
        for offset in 0..window {
            cache
                .get_with(&store, &new, &at(offset), true, |_| Ok(()))
                .unwrap();
        }
        let unkept = MessageCache::new(1).unwrap();
        for (path, count) in [(&old, 128), (&new, window)] {
            for offset in 0..count {
                unkept
                    .get_with(&store, path, &at(offset), false, |_| Ok(()))
                    .unwrap();
            }
        }
        let resident = |cache: &MessageCache| {
            (0..window)
                .filter(|&offset| {
                    cache
                        .cache
                        .contains_key(&MessageKey::new(new.as_ref(), &at(offset)))
                })
                .count()
        };
        (resident(&cache), resident(&unkept))
    };
    let cache = MessageCache::new(1).unwrap();
    let largest = (1..)
        .take_while(|&window| cache.holds(window, (window * SIZE) as u64))
        .last()
        .unwrap();
    assert_eq!(largest, 62);
    for window in 1..=largest {
        assert_eq!(held(window).0, window, "a window of {window}");
    }
    assert_eq!(held(largest), (largest, 2));
}

/// #1340 replay: once the message cache is full of earlier windows, a cold
/// position query's window is resident for its next run, which reads
/// nothing from storage. Not kept, its fills displaced each other in the
/// cold ring, and the next run fetched every message again.
#[test]
fn a_full_cache_keeps_a_cold_position_window_for_its_next_run() {
    // 20 MiB holds 36 of these messages, `holds` approving 36.
    let (_source, owner, _observed) = forecast(73, 1, 20, 16, Duration::ZERO);
    let engine = owner.level_collections().remove(0);
    let run = chrono::DateTime::parse_from_rfc3339("2026-04-05T00:00:00Z")
        .unwrap()
        .to_utc();
    let steps = |first: i64, last: i64| {
        let hours = chrono::TimeDelta::hours;
        Some((run + hours(first), run + hours(last)))
    };
    let query = |window| {
        engine
            .query_position(
                "POINT(25 60)",
                window,
                Some(&["q".into()]),
                Some(&[150.0]),
                None,
            )
            .unwrap();
    };
    query(steps(0, 17));
    query(steps(18, 36));
    let full = owner.message_cache_metrics();
    assert!(
        full.bytes > full.capacity_bytes - ECMWF.len() as u64,
        "{full:?}"
    );
    query(steps(37, 72));
    let before = owner.storage_bytes_read();
    query(steps(37, 72));
    assert_eq!(owner.storage_bytes_read(), before);
    assert_eq!(owner.message_cache_metrics().churn.ghost_hits, Some([0; 3]));
}

fn forecast(
    count: u32,
    grid_mb: u64,
    message_mb: u64,
    point_index_mb: u64,
    delay: Duration,
) -> (TestSource, GribEngine, Arc<TestStore>) {
    let source = TestSource::new();
    for step in 0..count {
        source.write(
            &format!("f{step:03}"),
            &[
                ("q", "150 mb", ECMWF.to_vec()),
                ("TAIL", "surface", message(0, 280.0, [0; 4], 1, 0)),
            ],
            step,
        );
    }
    let mut config = source.config();
    config.grid_cache_mb = grid_mb;
    config.message_cache_mb = message_mb;
    config.point_index_cache_mb = Some(point_index_mb);
    config.level_types = Some(vec![GribLevelType::Pressure]);
    config.parameters = Some(vec!["q".into()]);
    let mut engine = GribEngine::new("packed", &config).unwrap();
    assert_eq!(
        engine.message_cache_metrics().bytes,
        0,
        "header discovery must not fill the message cache"
    );
    let observed = instrumented(delay);
    run_fetches(async {
        for step in 0..count {
            let path = Path::from(format!("f{step:03}.grib2"));
            put(
                &observed,
                &path,
                &std::fs::read(source.dir.join(path.as_ref())).unwrap(),
            )
            .await;
        }
    });
    Arc::get_mut(&mut engine.source).unwrap().store = DataStore::new(observed.clone());
    (source, engine, observed)
}

fn sample(engine: &GribEngine, coords: &str) -> Vec<Option<f64>> {
    let CoverageResponse::Single(result) = engine
        .query_position(coords, None, Some(&["q".into()]), Some(&[150.0]), None)
        .unwrap()
    else {
        panic!()
    };
    assert!(matches!(
        result.domain,
        DomainDescription::PointSeries { z: Some(_), .. }
    ));
    assert_eq!(result.parameters["q"].unit, "kg kg-1");
    result.ranges["q"].values.clone()
}

#[test]
fn changed_coordinates_and_apis_reuse_messages_when_decoded_grids_do_not_fit() {
    let expected = reader::decode_message(ECMWF, "q").unwrap();
    // The full decode, and the CCSDS point path (#1300), which loads no grid
    // and so counts no grid-cache miss, and without a message cache fetches
    // only the RSIs of messages it has indexed (#1301).
    for (message_mb, index_mb) in [(0, 0), (4, 0), (0, 16), (4, 16)] {
        // One global decoded field is already larger than this grid cache.
        let (_source, owner, observed) = forecast(6, 1, message_mb, index_mb, Duration::ZERO);
        let engine = owner.level_collections().remove(0);
        for (lon, lat) in [(25.0, 60.0), (28.0, 63.0)] {
            let samples = sample(&engine, &format!("POINT({lon} {lat})"));
            assert_eq!(samples, vec![expected.bilinear_value(lon, lat); 6]);
        }
        assert_eq!(engine.grid_cache_utilization().0, 0);
        // The full decode reads each field around the grid cache: a bypass,
        // not a miss (#1308).
        let bypasses = if index_mb == 0 { 12 } else { 0 };
        assert_eq!(engine.grid_cache_stats(), (0, 0));
        assert_eq!(engine.grid_cache_metrics().churn.bypasses, bypasses);
        assert_eq!(
            owner.message_cache_metrics(),
            engine.message_cache_metrics()
        );
        let downloads = if message_mb == 0 { 12 } else { 6 };
        let read = owner.storage_bytes_read();
        let message = ECMWF.len() as u64;
        if message_mb == 0 && index_mb > 0 {
            // Without a message cache the second coordinate fetches only the
            // RSIs it needs, one range GET per field (#1301).
            assert!(
                read > 6 * message && read < 6 * message + 6 * 16384,
                "{read}"
            );
        } else {
            assert_eq!(read, downloads * message);
        }
        assert_eq!(
            observed
                .reads
                .lock()
                .unwrap()
                .attempts
                .values()
                .sum::<usize>(),
            downloads as usize
        );
        if message_mb > 0 {
            let before = owner.storage_bytes_read();
            engine
                .query_area(
                    "24,60,24.5,60.5",
                    None,
                    Some(&["q".into()]),
                    Some(&[150.0]),
                    None,
                )
                .unwrap();
            engine
                .get_raster_tile(
                    [24.0, 60.0, 24.5, 60.5],
                    2,
                    2,
                    None,
                    &OutputCrs::Wgs84,
                    Some("q"),
                    Some(150.0),
                    None,
                )
                .unwrap();
            assert_eq!(
                owner.storage_bytes_read(),
                before,
                "area and Maps must share the same source cache"
            );
            let metrics = owner.message_cache_metrics();
            assert_eq!(metrics.hits, 8);
            assert_eq!(metrics.misses, 6);
            assert!(metrics.bytes >= 6 * ECMWF.len() as u64);
            assert!(metrics.bytes <= metrics.capacity_bytes);
        }
    }
}

#[test]
#[ignore = "manual repeated-window replay; real global GRIB field with simulated GET latency"]
fn repeated_forecast_latency_replay() {
    let mut baseline = None;
    // The full decode (point index off) and the CCSDS point path (#1300).
    for (message_mb, index_mb) in [(0, 0), (128, 0), (0, 16), (128, 16)] {
        let (_source, owner, _store) =
            forecast(120, 256, message_mb, index_mb, Duration::from_millis(150));
        let engine = owner.level_collections().remove(0);
        let mut outputs = Vec::new();
        for (pass, coords) in ["POINT(25 60)", "POINT(28 63)"].into_iter().enumerate() {
            let before = owner.storage_bytes_read();
            let start = Instant::now();
            outputs.push(sample(&engine, coords));
            let elapsed = start.elapsed();
            let bytes = owner.storage_bytes_read() - before;
            eprintln!("message_cache_mb={message_mb} point_index_cache_mb={index_mb} pass={pass} elapsed_ms={} downloaded_bytes={bytes} decoded_bytes={} packed_bytes={}", elapsed.as_millis(), owner.grid_cache_utilization().0, owner.message_cache_metrics().bytes);
            if pass == 1 && message_mb > 0 {
                assert_eq!(bytes, 0);
            }
        }
        if let Some(expected) = &baseline {
            assert_eq!(&outputs, expected);
        } else {
            baseline = Some(outputs);
        }
    }
}

/// A message of `length` bytes at offset 0 of `name`.
fn named(name: &str, length: u64) -> MessageKey {
    MessageKey {
        path: Arc::from(name),
        offset: 0,
        length: Some(length),
    }
}

/// A 64 MiB cache whose hot ring is at its target, as a production cache's
/// is, with one more entry in its cold ring.
fn hot_at_target() -> (MessageCache, Vec<MessageKey>) {
    let cache = MessageCache::new(64).unwrap();
    let hot = (cache.cache.hot_allocation() * cache.capacity_bytes() as f64) as u64;
    let keys: Vec<_> = (0..hot / MIB + 1)
        .map(|i| named(&format!("hot{i}"), MIB))
        .collect();
    for key in &keys {
        cache
            .cache
            .insert(key.clone(), Bytes::from(vec![0u8; MIB as usize]));
    }
    (cache, keys)
}

/// #1335: map read-ahead fills are not kept, so they share the cold ring
/// with the maps' own fills. In a cache whose hot ring is at its target,
/// twelve steps of 2.5 MiB, which the whole cache would hold, evict the
/// step a client asks for next before it is read; the five the read-ahead
/// takes, half the cold ring, all wait for their read.
#[test]
fn the_read_ahead_room_keeps_its_fills_in_a_cache_with_a_full_hot_ring() {
    let length = 5 * MIB / 2;
    let fill = |cache: &MessageCache, steps: u64| {
        let keys: Vec<_> = (0..steps)
            .map(|s| named(&format!("step{s:02}"), length))
            .collect();
        for key in &keys {
            let bytes = Bytes::from(vec![1u8; length as usize]);
            assert!(cache.read_ahead(key, || Ok(bytes)).unwrap().is_some());
        }
        keys.iter()
            .map(|key| cache.is_resident(key))
            .collect::<Vec<_>>()
    };
    let (cache, _) = hot_at_target();
    assert!(cache.holds(12, 12 * length), "the whole cache holds them");
    assert!(!fill(&cache, 12)[0], "twelve evict the next step");

    let (cache, _) = hot_at_target();
    let room = crate::readahead::ReadAhead::room(&cache);
    let admitted = room / message_weight("step00", length);
    assert_eq!(admitted, 5);
    assert!(room <= cache.cold_room() / 2);
    assert_eq!(fill(&cache, admitted), [true; 5]);
}

/// #1335: a read-ahead fill the cache evicted unread, which the map then
/// reads itself, is no read-ahead hit, nor is a later hit on the map's own
/// copy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_copy_the_request_fetched_itself_is_no_read_ahead_hit() {
    let cache = MessageCache::new(1).unwrap();
    let observed = instrumented(Duration::ZERO);
    let store = DataStore::new(observed.clone());
    let path = Path::from("fields.grib2");
    let first = message(0, 280.0, [0; 4], 1, 0);
    put(&observed, &path, &first).await;
    let e = entry(Some(first.len()));
    let key = MessageKey::new(path.as_ref(), &e);
    let copy = first.clone();
    tokio::task::block_in_place(|| {
        cache
            .read_ahead(&key, move || Ok(Bytes::from(copy)))
            .unwrap();
        let mut i = 0;
        while cache.is_resident(&key) {
            let filler = named(&format!("filler{i}"), 64 * 1024);
            cache
                .cache
                .insert(filler, Bytes::from(vec![0u8; 64 * 1024]));
            i += 1;
        }
        for _ in 0..2 {
            cache.read(&store, &path, &e).unwrap();
        }
    });
    assert_eq!(observed.reads.lock().unwrap().attempts[path.as_ref()], 1);
    assert_eq!(cache.read_ahead_hits(), 0);
}

/// #1335 against #1340's scan resistance: a map's own one-off fill leaves
/// from the cold ring without touching the hot ring, and so does a
/// read-ahead fill the map reads once. An animation pass of 60 frames of
/// 1 MiB over a cache whose hot ring is at its target keeps as many hot
/// entries either way.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_ahead_fill_read_once_leaves_the_hot_ring_alone() {
    let observed = instrumented(Duration::ZERO);
    let store = DataStore::new(observed.clone());
    let path = Path::from("frames.grib2");
    let accept = |_: &[u8]| Ok::<_, DataServerError>(());
    let mut survivors = Vec::new();
    for read_ahead in [false, true] {
        let (cache, hot) = hot_at_target();
        tokio::task::block_in_place(|| {
            for frame in 0..60u64 {
                let mut e = entry(Some(MIB as usize));
                e.offset = frame * MIB;
                let key = MessageKey::new(path.as_ref(), &e);
                let bytes = || Bytes::from(vec![1u8; MIB as usize]);
                if read_ahead {
                    cache.read_ahead(&key, || Ok(bytes())).unwrap();
                    let (_, validated) = cache.get_with(&store, &path, &e, false, accept).unwrap();
                    assert!(validated.is_none(), "the read-ahead's copy");
                } else {
                    cache
                        .cache
                        .get_or_insert_with(&key, || Ok::<_, DataServerError>(bytes()))
                        .unwrap();
                }
            }
        });
        survivors.push(hot.iter().filter(|key| cache.is_resident(key)).count());
    }
    assert_eq!(survivors[1], survivors[0], "{survivors:?}");
}

/// #1335: a request catching up with the read-ahead waits for its fetch,
/// and the fill it joined stays as cold as a map's own. `quick_cache`
/// admits a claimed fill that had waiters as read: held under a claim, the
/// fills of 60 frames each joined by its request left 3 of 36 hot entries
/// of a cache whose hot ring is at its target. Each request joins, with a
/// deadline and without one, fetches nothing, and every hot entry stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_ahead_fill_a_request_joined_leaves_the_hot_ring_alone() {
    let observed = instrumented(Duration::ZERO);
    let store = DataStore::new(observed.clone());
    let path = Path::from("frames.grib2");
    let accept = |_: &[u8]| Ok::<_, DataServerError>(());
    // The deadline bounds only how long a request may wait: the fetch ends
    // as soon as it waits.
    for deadline in [Some(Duration::from_secs(60)), None] {
        let cache = Arc::new(MessageCache::new(64).unwrap());
        let hot = (cache.cache.hot_allocation() * cache.capacity_bytes() as f64) as u64;
        let hot_keys: Vec<_> = (0..hot / MIB)
            .map(|i| named(&format!("hot{i}"), MIB))
            .collect();
        for key in &hot_keys {
            cache
                .cache
                .insert(key.clone(), Bytes::from(vec![0u8; MIB as usize]));
        }
        let mut joined = 0;
        for frame in 0..60u64 {
            let mut e = entry(Some(MIB as usize));
            e.offset = frame * MIB;
            let key = MessageKey::new(path.as_ref(), &e);
            let (claimed, started) = std::sync::mpsc::channel();
            let fill = std::thread::spawn({
                let cache = cache.clone();
                move || {
                    let fetch = || {
                        claimed.send(()).unwrap();
                        // Finished only once the frame's request waits.
                        until(|| cache.read_ahead_waiters(&key) > 0);
                        Ok(Bytes::from(vec![1u8; MIB as usize]))
                    };
                    cache.read_ahead(&key, fetch).unwrap()
                }
            });
            started.recv().unwrap();
            let (_, validated) = tokio::task::block_in_place(|| {
                let _deadline = deadline::enter(deadline.map(|wait| Instant::now() + wait));
                cache.get_with(&store, &path, &e, false, accept).unwrap()
            });
            if validated.is_none() {
                joined += 1;
            }
            assert!(fill.join().unwrap().is_some());
        }
        let kept = hot_keys.iter().filter(|k| cache.is_resident(k)).count();
        assert_eq!(
            (joined, kept),
            (60, hot_keys.len()),
            "deadline {deadline:?}"
        );
    }
    let attempts = observed.reads.lock().unwrap().attempts.clone();
    assert!(
        attempts.is_empty(),
        "no request fetched a frame: {attempts:?}"
    );
}

/// Wait until `done`, polling; a test that waits a minute has hung.
fn until(done: impl Fn() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "never happened"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}
