//! In-memory object stores for tests: one that instruments LISTs, for the
//! concurrent listing and catalog scan tests, and one whose GET bodies stall,
//! for the fetch budget tests.

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::stream::BoxStream;
use futures::StreamExt;
use object_store::memory::InMemory;
use object_store::path::Path;
use object_store::{ObjectMeta, ObjectStore, ObjectStoreExt, PutPayload};

/// Wraps [`InMemory`]: every LIST sleeps for `delay` (or its prefix's entry
/// in `delays`) while it counts as in flight, and a prefix in `failing`
/// fails to list.
#[derive(Debug, Default)]
pub(crate) struct ListProbe {
    pub inner: InMemory,
    pub delay: Duration,
    pub delays: BTreeMap<String, Duration>,
    pub failing: HashSet<String>,
    /// LISTs in flight now, and the most there ever were.
    pub active: Arc<AtomicUsize>,
    pub peak: Arc<AtomicUsize>,
    /// Every prefix listed, in the order the LISTs started.
    pub listed: Arc<Mutex<Vec<String>>>,
}

impl ListProbe {
    /// Store a one-byte object under each key.
    pub async fn with_objects(self, keys: &[&str]) -> Self {
        for key in keys {
            self.inner
                .put(&Path::from(*key), PutPayload::from_static(b"x"))
                .await
                .unwrap();
        }
        self
    }

    /// Store an object of `size` bytes under `key`.
    pub async fn with_object_of_size(self, key: &str, size: usize) -> Self {
        self.inner
            .put(&Path::from(key), PutPayload::from(vec![0u8; size]))
            .await
            .unwrap();
        self
    }
}

impl std::fmt::Display for ListProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ListProbe")
    }
}

#[async_trait::async_trait]
impl ObjectStore for ListProbe {
    async fn put_opts(
        &self,
        path: &Path,
        payload: PutPayload,
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

    async fn get_opts(
        &self,
        path: &Path,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        self.inner.get_opts(path, options).await
    }

    fn delete_stream(
        &self,
        paths: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(paths)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        let name = prefix.map(Path::to_string).unwrap_or_default();
        let delay = self.delays.get(&name).copied().unwrap_or(self.delay);
        let fail = self.failing.contains(&name);
        let inner = self.inner.list(prefix);
        let (active, peak, listed) = (self.active.clone(), self.peak.clone(), self.listed.clone());
        futures::stream::once(async move {
            listed.lock().unwrap().push(name);
            let now = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(delay).await;
            let objects: Vec<_> = if fail {
                vec![Err(object_store::Error::Generic {
                    store: "ListProbe",
                    source: "partition unavailable".into(),
                })]
            } else {
                inner.collect().await
            };
            active.fetch_sub(1, Ordering::SeqCst);
            futures::stream::iter(objects)
        })
        .flatten()
        .boxed()
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

/// Wraps [`InMemory`]: every GET answers at once with the first half of
/// the body, then the rest arrives `stall` later. That is what a whole-object
/// GET looks like from outside object_store when the first attempt's body
/// read stalls: the attempt's timeout fires, object_store backs off and its
/// retry delivers the bytes still missing (#1011).
#[derive(Debug)]
pub(crate) struct StalledBody {
    pub inner: InMemory,
    pub stall: Duration,
}

impl std::fmt::Display for StalledBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StalledBody")
    }
}

#[async_trait::async_trait]
impl ObjectStore for StalledBody {
    async fn put_opts(
        &self,
        path: &Path,
        payload: PutPayload,
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

    async fn get_opts(
        &self,
        path: &Path,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        let result = self.inner.get_opts(path, options).await?;
        let (meta, range, attributes, extensions) = (
            result.meta.clone(),
            result.range.clone(),
            result.attributes.clone(),
            result.extensions.clone(),
        );
        let mut body = result.bytes().await?;
        let head = body.split_to(body.len() / 2);
        let stall = self.stall;
        let tail = futures::stream::once(async move {
            tokio::time::sleep(stall).await;
            Ok(body)
        });
        Ok(object_store::GetResult {
            payload: object_store::GetResultPayload::Stream(
                futures::stream::once(async move { Ok(head) })
                    .chain(tail)
                    .boxed(),
            ),
            meta,
            range,
            attributes,
            extensions,
        })
    }

    fn delete_stream(
        &self,
        paths: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(paths)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
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
