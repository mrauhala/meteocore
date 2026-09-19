//! Discovery regressions with controllable object-store completion and failures.

use super::*;
use ds_storage::object_store::{self, memory::InMemory, path::Path, ObjectStore, ObjectStoreExt};
use futures::stream::BoxStream;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug, Default)]
struct Reads {
    attempts: BTreeMap<String, usize>,
    completed: Vec<String>,
    fail_once: HashSet<String>,
}

#[derive(Debug, Default)]
struct ScanStore {
    inner: InMemory,
    reads: Mutex<Reads>,
    active: AtomicUsize,
    peak: AtomicUsize,
    delay: Duration,
}

impl std::fmt::Display for ScanStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GRIB scan test store")
    }
}

#[async_trait::async_trait]
impl ObjectStore for ScanStore {
    async fn get_opts(
        &self,
        path: &Path,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        assert!(
            !options.head,
            "scanning must not HEAD each index or data file"
        );
        if !path.as_ref().ends_with(".idx") {
            return self.inner.get_opts(path, options).await;
        }
        let fail = {
            let mut reads = self.reads.lock().unwrap();
            *reads.attempts.entry(path.to_string()).or_default() += 1;
            reads.fail_once.remove(path.as_ref())
        };
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        // Suspend even for zero latency, exposing actual overlapping GETs.
        // Delay the first path further to force completion out of input order.
        for _ in 0..if path.as_ref() == "f000.idx" { 10 } else { 1 } {
            tokio::task::yield_now().await;
        }
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        let result = if fail {
            Err(object_store::Error::Generic {
                store: "scan test",
                source: std::io::Error::other("transient index failure").into(),
            })
        } else {
            self.inner.get_opts(path, options).await
        };
        self.reads.lock().unwrap().completed.push(path.to_string());
        self.active.fetch_sub(1, Ordering::SeqCst);
        result
    }

    async fn put_opts(
        &self,
        path: &Path,
        payload: object_store::PutPayload,
        options: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        self.inner.put_opts(path, payload, options).await
    }

    async fn put_multipart_opts(
        &self,
        path: &Path,
        options: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(path, options).await
    }

    fn delete_stream(
        &self,
        paths: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(paths)
    }

    fn list(
        &self,
        prefix: Option<&Path>,
    ) -> BoxStream<'static, object_store::Result<object_store::ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&Path>,
    ) -> object_store::Result<object_store::ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

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
