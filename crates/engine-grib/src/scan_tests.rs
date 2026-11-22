//! Discovery regressions with controllable object-store completion and failures.

use super::*;
use crate::test_support::store::TestStore as ScanStore;
use ds_storage::object_store::{path::Path, ObjectStoreExt};
use std::sync::atomic::Ordering;

fn index_body(format: &str, file: usize) -> String {
    let offset = file * 200;
    if format == "wgrib2" {
        format!(
            "1:{offset}:d=2026040500:TMP:surface:0 hour fcst:\n2:{}:d=2026040500:TAIL:surface:0 hour fcst:\n",
            offset + 183
        )
    } else {
        serde_json::json!({
            "date": "20260405", "time": "0000", "step": "0", "param": "TMP",
            "levtype": "sfc", "_offset": offset, "_length": 183
        })
        .to_string()
    }
}

async fn engine_with_indexes(
    count: usize,
    format: &str,
    delay: Duration,
) -> (test_support::TestSource, GribEngine, Arc<ScanStore>) {
    let source = test_support::TestSource::new();
    let mut config = source.config();
    config.index_format = Some(format.into());
    config.parameters = Some(vec!["TMP".into()]);
    // Initial construction sees an empty directory. Install the instrumented
    // store before the first scan that discovers any indexes.
    let mut engine = GribEngine::new("scan", &config).unwrap();
    let store = Arc::new(ScanStore {
        delay,
        ..Default::default()
    });
    for file in 0..count {
        store
            .inner
            .put(
                &Path::from(format!("f{file:03}.idx")),
                index_body(format, file).into(),
            )
            .await
            .unwrap();
    }
    Arc::get_mut(&mut engine.source).unwrap().store = ds_storage::DataStore::new(store.clone());
    (source, engine, store)
}

fn origins(engine: &GribEngine) -> Vec<(String, u64)> {
    let catalog = engine.catalog();
    catalog.latest_run().unwrap().steps[&0]
        .messages
        .iter()
        .map(|entry| (entry.source_url.as_ref().unwrap().to_string(), entry.offset))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn index_batches_preserve_order_bound_reads_and_retry_only_failed_files() {
    // Cross two chunk boundaries and exercise the final partial chunk.
    let count = 2 * INDEX_FETCH_CONCURRENCY + 3;
    for format in ["wgrib2", "ecmwf-json"] {
        let (_source, engine, store) = engine_with_indexes(count, format, Duration::ZERO).await;
        store
            .reads
            .lock()
            .unwrap()
            .fail_once
            .insert("f002.idx".into());
        store
            .inner
            .put(&Path::from("f009.idx"), vec![255].into())
            .await
            .unwrap();
        store
            .inner
            .put(&Path::from("f010.idx"), "invalid index".into())
            .await
            .unwrap();
        engine.scan_once().unwrap();

        assert_eq!(store.peak.load(Ordering::SeqCst), INDEX_FETCH_CONCURRENCY);
        assert_eq!(store.active.load(Ordering::SeqCst), 0);
        assert_eq!(store.reads.lock().unwrap().attempts.len(), count);
        assert_ne!(store.reads.lock().unwrap().completed[0], "f000.idx");
        let expected: Vec<_> = (0..count)
            .filter(|file| ![2, 9, 10].contains(file))
            .map(|file| (format!("f{file:03}.grib2"), (file * 200) as u64))
            .collect();
        assert_eq!(origins(&engine), expected);
        assert_eq!(engine.source.known_indexes.lock().unwrap().len(), count - 3);

        for file in [9, 10] {
            store
                .inner
                .put(
                    &Path::from(format!("f{file:03}.idx")),
                    index_body(format, file).into(),
                )
                .await
                .unwrap();
        }
        engine.scan_once().unwrap();
        assert_eq!(engine.source.known_indexes.lock().unwrap().len(), count);
        let mut expected = expected;
        expected.extend([2, 9, 10].map(|file| (format!("f{file:03}.grib2"), file * 200)));
        assert_eq!(origins(&engine), expected);
        let attempts = store.reads.lock().unwrap().attempts.clone();
        for file in 0..count {
            assert_eq!(
                attempts[&format!("f{file:03}.idx")],
                if [2, 9, 10].contains(&file) { 2 } else { 1 }
            );
        }
        let version = engine.catalog().content_version;
        engine.scan_once().unwrap();
        assert_eq!(store.reads.lock().unwrap().attempts, attempts);
        assert_eq!(engine.catalog().content_version, version);
        assert_eq!(store.active.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual latency replay; timings are measurements, not CI thresholds"]
async fn index_scan_latency_replay() {
    use ds_storage::object_store::limit::LimitStore;

    let mut baseline = None;
    for concurrency in [1, INDEX_FETCH_CONCURRENCY] {
        let (_source, mut engine, store) =
            engine_with_indexes(120, "wgrib2", Duration::from_millis(150)).await;
        Arc::get_mut(&mut engine.source).unwrap().store =
            ds_storage::DataStore::new(Arc::new(LimitStore::new(store.clone(), concurrency)));
        let start = Instant::now();
        engine.scan_once().unwrap();
        let elapsed = start.elapsed();
        let result = (
            origins(&engine),
            engine.catalog().content_version,
            engine.storage_bytes_read(),
        );
        if let Some(expected) = &baseline {
            assert_eq!(&result, expected);
        } else {
            baseline = Some(result);
        }
        assert_eq!(engine.source.known_indexes.lock().unwrap().len(), 120);
        assert_eq!(store.peak.load(Ordering::SeqCst), concurrency);
        eprintln!(
            "indexes=120 latency_ms=150 concurrency={concurrency} elapsed_ms={} bytes={}",
            elapsed.as_millis(),
            engine.storage_bytes_read()
        );
    }
}

/// A publisher's in-progress index (`.name.idx`) is never fetched or
/// marked known (#1009): parsed before its rename, it would point at a
/// data file that does not exist under that name yet.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hidden_indexes_are_never_fetched() {
    let (_source, engine, store) = engine_with_indexes(2, "wgrib2", Duration::ZERO).await;
    store
        .inner
        .put(&Path::from(".f002.idx"), index_body("wgrib2", 2).into())
        .await
        .unwrap();
    engine.scan_once().unwrap();

    let attempts = store.reads.lock().unwrap().attempts.clone();
    assert!(!attempts.contains_key(".f002.idx"), "{attempts:?}");
    assert_eq!(engine.source.known_indexes.lock().unwrap().len(), 2);
    assert_eq!(
        origins(&engine),
        [
            ("f000.grib2".to_string(), 0),
            ("f001.grib2".to_string(), 200)
        ]
    );
}

/// rsync `--delay-updates` stages files under their final names in a
/// `.~tmp~/` folder next to their destination, and the LIST is recursive:
/// an index there, at any depth below the prefix, is never fetched or
/// marked known (#1029).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indexes_in_hidden_folders_are_never_fetched() {
    let (_source, engine, store) = engine_with_indexes(2, "wgrib2", Duration::ZERO).await;
    for (key, file) in [(".~tmp~/f002.idx", 2), ("run/.~tmp~/f003.idx", 3)] {
        // `parse` keeps `~` as a listing returns it.
        store
            .inner
            .put(
                &Path::parse(key).unwrap(),
                index_body("wgrib2", file).into(),
            )
            .await
            .unwrap();
    }
    engine.scan_once().unwrap();

    let attempts = store.reads.lock().unwrap().attempts.clone();
    assert_eq!(
        attempts.keys().collect::<Vec<_>>(),
        ["f000.idx", "f001.idx"],
        "{attempts:?}"
    );
    assert_eq!(engine.source.known_indexes.lock().unwrap().len(), 2);
    assert_eq!(
        origins(&engine),
        [
            ("f000.grib2".to_string(), 0),
            ("f001.grib2".to_string(), 200)
        ]
    );
}

/// A source whose bucket keeps refusing index reads with `503 SlowDown`
/// (#1530): a chunk whose read failed while the bucket throttled stops the
/// scan, and the indexes after it are not read at all; a failure without a
/// throttle does not stop it. The collection keeps what it read, and the
/// next poll, the bucket answering again, reads the rest and only them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bucket_refusing_index_reads_stops_the_scan_until_the_next_poll() {
    let count = 2 * INDEX_FETCH_CONCURRENCY + 3;
    let (_source, mut engine, _) = engine_with_indexes(count, "ecmwf-json", Duration::ZERO).await;
    let bucket = ds_storage::test_util::ThrottledBucket::new("s3.test/forecasts");
    let store = Arc::new(ScanStore {
        bucket: Some(bucket.clone()),
        ..Default::default()
    });
    for file in 0..count {
        store
            .inner
            .put(
                &Path::from(format!("f{file:03}.idx")),
                index_body("ecmwf-json", file).into(),
            )
            .await
            .unwrap();
    }
    {
        let mut reads = store.reads.lock().unwrap();
        // The first chunk: a transient failure, no throttle.
        reads.fail_once.insert("f001.idx".into());
        // The second: a SlowDown through every retry.
        reads
            .throttled
            .insert(format!("f{:03}.idx", INDEX_FETCH_CONCURRENCY + 2));
    }
    let reads: Arc<dyn ds_storage::object_store::ObjectStore> = store.clone();
    Arc::get_mut(&mut engine.source).unwrap().store = bucket.store(reads);

    engine.scan_once().unwrap();
    let attempts = store.reads.lock().unwrap().attempts.clone();
    let read: Vec<usize> = (0..count)
        .filter(|file| attempts.contains_key(&format!("f{file:03}.idx")))
        .collect();
    assert_eq!(
        read,
        (0..2 * INDEX_FETCH_CONCURRENCY).collect::<Vec<_>>(),
        "two chunks read, the third not"
    );
    let known = engine.source.known_indexes.lock().unwrap().len();
    assert_eq!(known, 2 * INDEX_FETCH_CONCURRENCY - 2);
    assert_eq!(
        origins(&engine).len(),
        known,
        "the collection keeps what it read"
    );

    bucket.quiet();
    store.reads.lock().unwrap().throttled.clear();
    engine.scan_once().unwrap();
    assert_eq!(engine.source.known_indexes.lock().unwrap().len(), count);
    assert_eq!(origins(&engine).len(), count);
    let attempts = store.reads.lock().unwrap().attempts.clone();
    for file in 0..count {
        let again = [1, INDEX_FETCH_CONCURRENCY + 2].contains(&file);
        assert_eq!(
            attempts[&format!("f{file:03}.idx")],
            if again { 2 } else { 1 },
            "f{file:03}"
        );
    }
}

/// An ECMWF JSON index of the run at `hour` on 2026-04-05, step `step`.
fn run_index(hour: u32, step: u32) -> String {
    serde_json::json!({
        "date": "20260405", "time": format!("{hour:02}00"), "step": step.to_string(),
        "param": "TMP", "levtype": "sfc", "_offset": 0, "_length": 183
    })
    .to_string()
}

/// A remote source of runs `%Y%m%d/{run}z/` at 00 and 06 UTC, in a bucket
/// whose back-off a test drives: the 06z run's `newest` indexes and the
/// 00z run's `older`, `f000.idx` on. Its scans run at 07 UTC.
async fn remote_runs(
    newest: u32,
    older: u32,
) -> (
    test_support::TestSource,
    GribEngine,
    Arc<ScanStore>,
    ds_storage::test_util::ThrottledBucket,
) {
    let source = test_support::TestSource::new();
    let mut config = source.config();
    config.index_format = Some("ecmwf-json".into());
    let mut engine = GribEngine::new("remote-scan", &config).unwrap();
    let bucket = ds_storage::test_util::ThrottledBucket::new("s3.test/forecasts");
    let store = Arc::new(ScanStore {
        bucket: Some(bucket.clone()),
        ..Default::default()
    });
    for (hour, count) in [(6, newest), (0, older)] {
        for step in 0..count {
            store
                .inner
                .put(
                    &Path::from(format!("20260405/{hour:02}z/f{step:03}.idx")),
                    run_index(hour, step).into(),
                )
                .await
                .unwrap();
        }
    }
    let inner = Arc::get_mut(&mut engine.source).unwrap();
    inner.scan_mode = ScanMode::Remote {
        prefix_pattern: "%Y%m%d/{run}z/".into(),
    };
    inner.config.run_hours = Some(vec![0, 6]);
    let reads: Arc<dyn ds_storage::object_store::ObjectStore> = store.clone();
    inner.store = bucket.store(reads);
    (source, engine, store, bucket)
}

fn scan_at_seven(engine: &GribEngine) {
    engine
        .scan_at("2026-04-05T07:00:00Z".parse().unwrap())
        .unwrap();
}

/// The indexes `store` has been asked for, sorted.
fn attempted(store: &ScanStore) -> Vec<String> {
    store
        .reads
        .lock()
        .unwrap()
        .attempts
        .keys()
        .cloned()
        .collect()
}

/// A scan reads the newest run first, each run in step order (#1530): a
/// bucket refusing its first chunk leaves the older run unread, not the
/// newest. The run with indexes left unread is not settled, so the next
/// poll lists it and reads them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scan_reads_the_newest_run_first_and_a_refused_one_leaves_the_older() {
    let (_source, engine, store, bucket) = remote_runs(3, 13).await;
    store
        .reads
        .lock()
        .unwrap()
        .throttled
        .insert("20260405/06z/f001.idx".into());
    scan_at_seven(&engine);
    let mut first_chunk: Vec<String> = (0..3)
        .map(|step| format!("20260405/06z/f{step:03}.idx"))
        .chain((0..5).map(|step| format!("20260405/00z/f{step:03}.idx")))
        .collect();
    first_chunk.sort();
    assert_eq!(
        attempted(&store),
        first_chunk,
        "the newest run's chunk only"
    );
    let known = engine.source.known_indexes.lock().unwrap().clone();
    assert!(known.contains("20260405/06z/f000.idx"));
    assert!(known.contains("20260405/06z/f002.idx"));
    assert_eq!(known.len(), 7);
    let settled = engine.source.settled_prefixes.lock().unwrap().clone();
    assert!(!settled.contains("20260405/00z"), "{settled:?}");

    bucket.quiet();
    store.reads.lock().unwrap().throttled.clear();
    scan_at_seven(&engine);
    assert_eq!(engine.source.known_indexes.lock().unwrap().len(), 16);
    let settled = engine.source.settled_prefixes.lock().unwrap().clone();
    assert!(settled.contains("20260405/00z"), "{settled:?}");
    let attempts = store.reads.lock().unwrap().attempts.clone();
    assert_eq!(attempts["20260405/06z/f001.idx"], 2);
    assert_eq!(attempts["20260405/00z/f012.idx"], 1);
}

/// The failed reads of the chunk a refusing bucket stopped the scan at are
/// unread too (#1530): with that chunk the last, their run is not settled
/// and the next poll reads them, not the hourly re-list.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_refused_chunks_failed_reads_leave_their_run_unsettled() {
    let (_source, engine, store, bucket) = remote_runs(3, 13).await;
    store
        .reads
        .lock()
        .unwrap()
        .throttled
        .insert("20260405/00z/f009.idx".into());
    scan_at_seven(&engine);
    assert_eq!(
        attempted(&store).len(),
        16,
        "the refused chunk was the last"
    );
    assert_eq!(engine.source.known_indexes.lock().unwrap().len(), 15);
    let settled = engine.source.settled_prefixes.lock().unwrap().clone();
    assert!(!settled.contains("20260405/00z"), "{settled:?}");

    bucket.quiet();
    store.reads.lock().unwrap().throttled.clear();
    scan_at_seven(&engine);
    assert_eq!(
        store.reads.lock().unwrap().attempts["20260405/00z/f009.idx"],
        2
    );
    assert_eq!(engine.source.known_indexes.lock().unwrap().len(), 16);
    let settled = engine.source.settled_prefixes.lock().unwrap().clone();
    assert!(settled.contains("20260405/00z"), "{settled:?}");
}

/// A failed read while the bucket does not back off, another collection's
/// stray `SlowDown` counted meanwhile included, stops nothing; a read that
/// fails while it backs off stops the scan whatever made it back off.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_a_read_failing_while_the_bucket_backs_off_stops_the_scan() {
    let (_source, engine, store, bucket) = remote_runs(3, 13).await;
    store
        .reads
        .lock()
        .unwrap()
        .fail_once
        .insert("20260405/06z/f001.idx".into());
    scan_at_seven(&engine);
    assert_eq!(
        attempted(&store).len(),
        16,
        "a transient failure stops nothing"
    );
    assert_eq!(engine.source.known_indexes.lock().unwrap().len(), 15);

    let (_source, engine, store, bucket_b) = remote_runs(3, 13).await;
    drop(bucket);
    bucket_b.slow_down();
    store
        .reads
        .lock()
        .unwrap()
        .fail_once
        .insert("20260405/06z/f001.idx".into());
    scan_at_seven(&engine);
    assert_eq!(
        attempted(&store).len(),
        8,
        "a failure while it backs off stops it"
    );
}

/// Within a run, indexes are read in step order, numbers as numbers.
#[test]
fn natural_order_compares_steps_as_numbers() {
    let mut names = vec![
        "20261122000000-102h-oper-fc.index",
        "20261122000000-12h-oper-fc.index",
        "20261122000000-6h-oper-fc.index",
        "20261122000000-0h-oper-fc.index",
        "20261122000000-06h-oper-fc.index",
        "20261122000000-9h-enfo-ef.index",
    ];
    names.sort_by(|a, b| natural_order(a, b));
    assert_eq!(
        names,
        [
            "20261122000000-0h-oper-fc.index",
            "20261122000000-06h-oper-fc.index",
            "20261122000000-6h-oper-fc.index",
            "20261122000000-9h-enfo-ef.index",
            "20261122000000-12h-oper-fc.index",
            "20261122000000-102h-oper-fc.index",
        ]
    );
}

/// A scan's LISTs and index reads are background work (#1530): they hold
/// at most half a bucket's permits, so a request has room behind a poll's
/// burst. A read through the source's store is a request's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scans_index_reads_are_background_work() {
    let count = INDEX_FETCH_CONCURRENCY + 3;
    let (_source, mut engine, store) =
        engine_with_indexes(count, "ecmwf-json", Duration::ZERO).await;
    let classes = Arc::new(ds_storage::test_util::ReadClasses::new(store.clone()));
    let bucket = ds_storage::test_util::ThrottledBucket::new("s3.test/forecasts");
    let reads: Arc<dyn ds_storage::object_store::ObjectStore> = classes.clone();
    Arc::get_mut(&mut engine.source).unwrap().store = bucket.store(reads);
    engine.scan_once().unwrap();
    let read = classes.take();
    let indexes: Vec<_> = read
        .iter()
        .filter(|(path, _)| path.ends_with(".idx"))
        .collect();
    assert_eq!(indexes.len(), count);
    assert!(
        indexes.iter().all(|(_, background)| *background),
        "{read:?}"
    );

    engine.source.store.get(&Path::from("f000.idx")).unwrap();
    assert_eq!(classes.take(), [("f000.idx".to_string(), false)]);
}
