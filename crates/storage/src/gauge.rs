//! Requests through a per-host limit, counted for `/metrics` (#1067): how
//! many hold one of the host's permits and how many wait for one.
//!
//! [`crate::build_http_store_limited`] wraps the host's limited store
//! (`throttle::Limited`) in a [`Gauged`] store and the HTTP store under it
//! in another: the outer one counts every request that has asked for a
//! permit, the inner one those that hold one. Each counts a GET until its
//! body has been read or dropped, as the limit holds its permit.

use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::stream::{BoxStream, Stream, StreamExt};
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, GetResultPayload, ListResult, MultipartUpload, ObjectMeta,
    ObjectStore, PutMultipartOptions, PutOptions, PutPayload, PutResult, Result,
};

/// An object store that adds one to `count` for each request in flight: a
/// GET or HEAD until its body has been read or dropped, a put, copy or
/// delimited list while it runs. Streamed lists and deletes are passed
/// through uncounted; the HTTP sources this measures never make them.
#[derive(Debug)]
pub(crate) struct Gauged {
    pub inner: Arc<dyn ObjectStore>,
    pub count: Arc<AtomicUsize>,
}

/// One operation counted, until dropped.
struct Held(Arc<AtomicUsize>);

impl Held {
    fn new(count: &Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::Relaxed);
        Self(count.clone())
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A GET body that keeps its operation counted until it ends or is dropped.
struct HeldStream {
    inner: BoxStream<'static, Result<Bytes>>,
    held: Option<Held>,
}

impl Stream for HeldStream {
    type Item = Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let next = self.inner.poll_next_unpin(cx);
        if matches!(next, Poll::Ready(None)) {
            self.held = None;
        }
        next
    }
}

impl std::fmt::Display for Gauged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Gauged({})", self.inner)
    }
}

#[async_trait::async_trait]
impl ObjectStore for Gauged {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> Result<PutResult> {
        let _held = Held::new(&self.count);
        self.inner.put_opts(location, payload, options).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        options: PutMultipartOptions,
    ) -> Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, options).await
    }

    async fn get_opts(&self, location: &Path, options: GetOptions) -> Result<GetResult> {
        let held = Held::new(&self.count);
        let mut result = self.inner.get_opts(location, options).await?;
        result.payload = match result.payload {
            GetResultPayload::Stream(inner) => GetResultPayload::Stream(
                HeldStream {
                    inner,
                    held: Some(held),
                }
                .boxed(),
            ),
            local => local,
        };
        Ok(result)
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, Result<Path>>,
    ) -> BoxStream<'static, Result<Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> Result<ListResult> {
        let _held = Held::new(&self.count);
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(&self, from: &Path, to: &Path, options: CopyOptions) -> Result<()> {
        let _held = Held::new(&self.count);
        self.inner.copy_opts(from, to, options).await
    }
}

/// The requests through one host's limit ([`crate::host_requests`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRequests {
    /// The host and port, as `thredds.met.no` or `127.0.0.1:8080`.
    pub host: String,
    /// Requests the host allows in flight at once now: the configured
    /// limit, or less while it backs off from a 429 (#1235). Requests in
    /// flight when it was cut finish, so `in_flight` can exceed it.
    pub limit: usize,
    /// Requests holding one of its permits, those waiting for the host's
    /// `Retry-After` included: the wait comes after the permit.
    pub in_flight: usize,
    /// Requests waiting for one.
    pub queued: usize,
    /// The 429 Too Many Requests answers the host has given since the
    /// process started, to every store on it (#1290).
    pub throttled: u64,
}

/// The two counters of one limited host store.
#[derive(Debug, Default)]
pub(crate) struct HostGauges {
    /// Requests that asked for a permit, waiting or holding one.
    pub requested: Arc<AtomicUsize>,
    /// Requests holding one.
    pub in_flight: Arc<AtomicUsize>,
}

impl HostGauges {
    /// `(in flight, queued)` now. The two counters are read one after the
    /// other, so a request in transit between them can show in neither.
    pub fn read(&self) -> (usize, usize) {
        let in_flight = self.in_flight.load(Ordering::Relaxed);
        let requested = self.requested.load(Ordering::Relaxed);
        (in_flight, requested.saturating_sub(in_flight))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use object_store::memory::InMemory;
    use object_store::ObjectStoreExt;

    use super::*;
    use crate::test_store::MetaStore;
    use crate::DataStore;

    /// A gauged store under a limit and another over it, as
    /// `build_http_store_limited` stacks them.
    fn stack(inner: Arc<dyn ObjectStore>, limit: usize) -> (Arc<dyn ObjectStore>, HostGauges) {
        let gauges = HostGauges::default();
        let counted: Arc<dyn ObjectStore> = Arc::new(Gauged {
            inner,
            count: gauges.in_flight.clone(),
        });
        let limited = DataStore::new(counted).with_request_limit(limit).inner;
        let store = Arc::new(Gauged {
            inner: limited,
            count: gauges.requested.clone(),
        });
        (store, gauges)
    }

    /// Three HEADs under a limit of one: one in flight, two waiting.
    #[tokio::test(start_paused = true)]
    async fn requests_in_flight_and_waiting_are_counted() {
        let meta = Arc::new(MetaStore {
            delay: Duration::from_millis(50),
            ..MetaStore::default()
        });
        for i in 0..3 {
            meta.set(&format!("k{i}"), 1, None);
        }
        let (store, gauges) = stack(meta, 1);
        let heads: Vec<_> = (0..3)
            .map(|i| {
                let store = store.clone();
                tokio::spawn(async move { store.head(&Path::from(format!("k{i}"))).await })
            })
            .collect();
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(gauges.read(), (1, 2));
        for head in heads {
            head.await.unwrap().unwrap();
        }
        assert_eq!(gauges.read(), (0, 0));
    }

    /// A GET counts until its body has been read.
    #[tokio::test]
    async fn a_get_is_in_flight_until_its_body_is_read() {
        let memory = InMemory::new();
        let path = Path::from("k");
        memory
            .put(&path, PutPayload::from_static(b"body"))
            .await
            .unwrap();
        let (store, gauges) = stack(Arc::new(memory), 2);
        let got = store.get(&path).await.unwrap();
        assert_eq!(gauges.read(), (1, 0));
        assert_eq!(got.bytes().await.unwrap(), &b"body"[..]);
        assert_eq!(gauges.read(), (0, 0));
        drop(store.get(&path).await.unwrap());
        assert_eq!(gauges.read(), (0, 0), "a dropped body is released");
    }
}
