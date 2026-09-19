//! Instrumented store shared by index and metadata discovery tests.

use ds_storage::object_store::{self, memory::InMemory, path::Path, ObjectStore};
use futures::stream::BoxStream;
use std::collections::{BTreeMap, HashSet};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};
use std::time::Duration;

#[derive(Debug, Default)]
pub struct Reads {
    pub attempts: BTreeMap<String, usize>,
    pub completed: Vec<String>,
    pub fail_once: HashSet<String>,
    pub ranges: Vec<(String, Option<object_store::GetRange>)>,
}

#[derive(Debug)]
pub struct TestStore {
    pub inner: InMemory,
    pub reads: Mutex<Reads>,
    pub active: AtomicUsize,
    pub peak: AtomicUsize,
    pub delay: Duration,
    pub suffix: &'static str,
}

impl Default for TestStore {
    fn default() -> Self {
        Self {
            inner: InMemory::new(),
            reads: Mutex::default(),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            delay: Duration::ZERO,
            suffix: ".idx",
        }
    }
}

struct InFlight<'a>(&'a AtomicUsize);
impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl std::fmt::Display for TestStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GRIB scan test store")
    }
}

#[async_trait::async_trait]
impl ObjectStore for TestStore {
    async fn get_opts(
        &self,
        path: &Path,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        assert!(
            !options.head,
            "scanning must not HEAD each index or data file"
        );
        if !path.as_ref().ends_with(self.suffix) {
            return self.inner.get_opts(path, options).await;
        }
        let fail = {
            let mut reads = self.reads.lock().unwrap();
            *reads.attempts.entry(path.to_string()).or_default() += 1;
            reads.ranges.push((path.to_string(), options.range.clone()));
            reads.fail_once.remove(path.as_ref())
        };
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        let _active = InFlight(&self.active);
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
