//! The `storage` span around every operation on a remote store (#1233).
//!
//! object_store logs each retry of a throttled or failed request at INFO
//! (`object_store::client::retry`) with only the status, the backoff and the
//! attempt number. [`Traced`] runs each operation of a remote store inside
//! `info_span!("storage", collection, store, path)`, so that line, and any
//! other event the operation emits, names the collection the store reads
//! for, the source and the object key. The span is entered for the whole
//! operation: the request and object_store's retries, and the body stream
//! of a GET or the stream of a LIST, where object_store retries body reads.
//!
//! [`crate::DataStore`] keeps its store without this layer and rebuilds the
//! layer when it is limited or names its collection, so one operation has
//! exactly one `storage` span. Local and in-memory stores have no layer:
//! they never retry.

use std::ops::Range;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::stream::{BoxStream, Stream, StreamExt};
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, GetResultPayload, ListResult, MultipartUpload, ObjectMeta,
    ObjectStore, PutMultipartOptions, PutOptions, PutPayload, PutResult, RenameOptions, Result,
};
use tracing::Instrument;

/// What the `storage` span names.
#[derive(Debug, Clone)]
pub(crate) struct Labels {
    /// The source, without credentials or query string: `host[:port]` of an
    /// HTTP server, the endpoint's `host[:port]/bucket` of an S3 bucket.
    pub store: Arc<str>,
    /// The collection the store reads for, once an engine has named it.
    pub collection: Option<Arc<str>>,
}

/// `base`, inside the span `labels` name.
pub(crate) fn layer(base: Arc<dyn ObjectStore>, labels: Labels) -> Arc<dyn ObjectStore> {
    Arc::new(Traced {
        inner: base,
        labels,
    })
}

/// `host[:port]` of `url`: never its userinfo, path or query.
pub(crate) fn host_port(url: &url::Url) -> Option<String> {
    let host = url.host_str()?;
    Some(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    })
}

/// `host[:port]` of an endpoint, whether or not it names a scheme
/// (`https://minio.example:9000`, `minio.example:9000`): never its userinfo,
/// path or query.
pub(crate) fn authority(endpoint: &str) -> String {
    if let Some(host) = url::Url::parse(endpoint).ok().as_ref().and_then(host_port) {
        return host;
    }
    let rest = endpoint
        .split_once("://")
        .map_or(endpoint, |(_, rest)| rest);
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host)
        .to_string()
}

/// The `store` label of an S3 bucket: its endpoint's `host[:port]` and the
/// bucket, as `s3.us-east-1.amazonaws.com/noaa-goes19`. Without an
/// endpoint, AWS's regional one, in `us-east-1` without a region, as
/// object_store builds it.
pub(crate) fn s3_label(endpoint: Option<&str>, region: Option<&str>, bucket: &str) -> String {
    let host = match endpoint {
        Some(endpoint) => authority(endpoint),
        None => format!("s3.{}.amazonaws.com", region.unwrap_or("us-east-1")),
    };
    format!("{host}/{bucket}")
}

/// An object store whose every operation runs inside a `storage` span.
#[derive(Debug)]
pub(crate) struct Traced {
    pub inner: Arc<dyn ObjectStore>,
    pub labels: Labels,
}

impl Traced {
    /// The span of one operation on `path`, the object key or list prefix.
    fn span(&self, path: Option<&Path>) -> tracing::Span {
        tracing::info_span!(
            "storage",
            collection = self.labels.collection.as_deref(),
            store = &*self.labels.store,
            path = path.map(|path| -> &str { path.as_ref() }),
        )
    }
}

/// A stream polled inside `span`: a GET body or a LIST, whose retries run
/// while it is polled.
struct InSpan<T> {
    inner: BoxStream<'static, Result<T>>,
    span: tracing::Span,
}

impl<T> Stream for InSpan<T> {
    type Item = Result<T>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        let _entered = this.span.enter();
        this.inner.poll_next_unpin(cx)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl std::fmt::Display for Traced {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.inner.fmt(f)
    }
}

#[async_trait::async_trait]
impl ObjectStore for Traced {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> Result<PutResult> {
        self.inner
            .put_opts(location, payload, options)
            .instrument(self.span(Some(location)))
            .await
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        options: PutMultipartOptions,
    ) -> Result<Box<dyn MultipartUpload>> {
        self.inner
            .put_multipart_opts(location, options)
            .instrument(self.span(Some(location)))
            .await
    }

    async fn get_opts(&self, location: &Path, options: GetOptions) -> Result<GetResult> {
        let span = self.span(Some(location));
        let mut result = self
            .inner
            .get_opts(location, options)
            .instrument(span.clone())
            .await?;
        result.payload = match result.payload {
            GetResultPayload::Stream(inner) => {
                GetResultPayload::Stream(InSpan { inner, span }.boxed())
            }
            local => local,
        };
        Ok(result)
    }

    async fn get_ranges(&self, location: &Path, ranges: &[Range<u64>]) -> Result<Vec<Bytes>> {
        self.inner
            .get_ranges(location, ranges)
            .instrument(self.span(Some(location)))
            .await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, Result<Path>>,
    ) -> BoxStream<'static, Result<Path>> {
        let span = self.span(None);
        let inner = span.in_scope(|| self.inner.delete_stream(locations));
        InSpan { inner, span }.boxed()
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> {
        let span = self.span(prefix);
        let inner = span.in_scope(|| self.inner.list(prefix));
        InSpan { inner, span }.boxed()
    }

    fn list_with_offset(
        &self,
        prefix: Option<&Path>,
        offset: &Path,
    ) -> BoxStream<'static, Result<ObjectMeta>> {
        let span = self.span(prefix);
        let inner = span.in_scope(|| self.inner.list_with_offset(prefix, offset));
        InSpan { inner, span }.boxed()
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> Result<ListResult> {
        self.inner
            .list_with_delimiter(prefix)
            .instrument(self.span(prefix))
            .await
    }

    async fn copy_opts(&self, from: &Path, to: &Path, options: CopyOptions) -> Result<()> {
        self.inner
            .copy_opts(from, to, options)
            .instrument(self.span(Some(from)))
            .await
    }

    async fn rename_opts(&self, from: &Path, to: &Path, options: RenameOptions) -> Result<()> {
        self.inner
            .rename_opts(from, to, options)
            .instrument(self.span(Some(from)))
            .await
    }
}

#[cfg(test)]
mod tests {
    use futures::TryStreamExt;
    use object_store::memory::InMemory;
    use object_store::ObjectStoreExt;

    use super::*;
    use crate::test_store::JsonLog;
    use crate::DataStore;

    /// An in-memory store whose GET bodies and LISTs log a line each time
    /// they yield, as object_store's body-read retries do while a body is
    /// polled.
    #[derive(Debug, Default)]
    struct Chatty(InMemory);

    impl std::fmt::Display for Chatty {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("Chatty")
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for Chatty {
        async fn put_opts(
            &self,
            location: &Path,
            payload: PutPayload,
            options: PutOptions,
        ) -> Result<PutResult> {
            self.0.put_opts(location, payload, options).await
        }

        async fn put_multipart_opts(
            &self,
            location: &Path,
            options: PutMultipartOptions,
        ) -> Result<Box<dyn MultipartUpload>> {
            self.0.put_multipart_opts(location, options).await
        }

        async fn get_opts(&self, location: &Path, options: GetOptions) -> Result<GetResult> {
            let mut result = self.0.get_opts(location, options).await?;
            result.payload = match result.payload {
                GetResultPayload::Stream(body) => GetResultPayload::Stream(
                    body.inspect(|_| tracing::info!(target: "chatty", "body"))
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
            self.0.delete_stream(locations)
        }

        fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> {
            self.0
                .list(prefix)
                .inspect(|_| tracing::info!(target: "chatty", "listed"))
                .boxed()
        }

        async fn list_with_delimiter(&self, prefix: Option<&Path>) -> Result<ListResult> {
            self.0.list_with_delimiter(prefix).await
        }

        async fn copy_opts(&self, from: &Path, to: &Path, options: CopyOptions) -> Result<()> {
            self.0.copy_opts(from, to, options).await
        }
    }

    /// The `chatty` lines of reading `dir/obj` whole and listing `dir`.
    async fn read_and_list(store: &DataStore) -> Vec<serde_json::Value> {
        let log = JsonLog::default();
        let _default = tracing::subscriber::set_default(log.subscriber());
        let object = Path::from("dir/obj");
        // The GET has returned before its body is read.
        let got = store.inner().get(&object).await.unwrap();
        assert_eq!(got.bytes().await.unwrap(), &b"body"[..]);
        let listed: Vec<_> = store
            .inner()
            .list(Some(&Path::from("dir")))
            .try_collect()
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        log.lines()
            .into_iter()
            .filter(|line| line["target"] == "chatty")
            .collect()
    }

    async fn chatty() -> Arc<Chatty> {
        let chatty = Chatty::default();
        chatty
            .0
            .put(&Path::from("dir/obj"), PutPayload::from_static(b"body"))
            .await
            .unwrap();
        Arc::new(chatty)
    }

    /// A GET body and a LIST are polled inside the operation's span, after
    /// the call that made them has returned.
    #[tokio::test]
    async fn bodies_and_listings_are_polled_inside_the_span() {
        let store = DataStore::remote(chatty().await, "example.test").for_collection("radar");
        let lines = read_and_list(&store).await;
        assert_eq!(lines.len(), 2, "{lines:#?}");
        let span = |path| {
            serde_json::json!({
                "name": "storage",
                "collection": "radar",
                "store": "example.test",
                "path": path,
            })
        };
        assert_eq!(lines[0]["message"], "body");
        assert_eq!(lines[0]["span"], span("dir/obj"));
        assert_eq!(lines[1]["message"], "listed");
        assert_eq!(lines[1]["span"], span("dir"));
    }

    /// A store built without a source, local or in memory, has no span.
    #[tokio::test]
    async fn a_store_without_a_source_has_no_span() {
        let store = DataStore::new(chatty().await).for_collection("radar");
        let lines = read_and_list(&store).await;
        assert_eq!(lines.len(), 2, "{lines:#?}");
        assert!(lines.iter().all(|line| line.get("span").is_none()));
    }
}
