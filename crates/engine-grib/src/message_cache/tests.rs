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

fn forecast(
    count: u32,
    grid_mb: u64,
    message_mb: u64,
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
    for message_mb in [0, 4] {
        // One global decoded field is already larger than this grid cache.
        let (_source, owner, observed) = forecast(6, 1, message_mb, Duration::ZERO);
        let engine = owner.level_collections().remove(0);
        for (lon, lat) in [(25.0, 60.0), (28.0, 63.0)] {
            let samples = sample(&engine, &format!("POINT({lon} {lat})"));
            assert_eq!(samples, vec![expected.bilinear_value(lon, lat); 6]);
        }
        assert_eq!(engine.grid_cache_utilization().0, 0);
        assert_eq!(engine.grid_cache_stats().1, 12);
        assert_eq!(
            owner.message_cache_metrics(),
            engine.message_cache_metrics()
        );
        let downloads = if message_mb == 0 { 12 } else { 6 };
        assert_eq!(owner.storage_bytes_read(), downloads * ECMWF.len() as u64);
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
    for message_mb in [0, 128] {
        let (_source, owner, _store) = forecast(120, 256, message_mb, Duration::from_millis(150));
        let engine = owner.level_collections().remove(0);
        let mut outputs = Vec::new();
        for (pass, coords) in ["POINT(25 60)", "POINT(28 63)"].into_iter().enumerate() {
            let before = owner.storage_bytes_read();
            let start = Instant::now();
            outputs.push(sample(&engine, coords));
            let elapsed = start.elapsed();
            let bytes = owner.storage_bytes_read() - before;
            eprintln!("message_cache_mb={message_mb} pass={pass} elapsed_ms={} downloaded_bytes={bytes} decoded_bytes={} packed_bytes={}", elapsed.as_millis(), owner.grid_cache_utilization().0, owner.message_cache_metrics().bytes);
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
