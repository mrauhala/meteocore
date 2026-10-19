//! Object stores for tests: one that instruments LISTs, for the concurrent
//! listing and catalog scan tests; one whose GET bodies stall, for the fetch
//! budget tests; and one that answers HEADs from scripted metadata, for the
//! template prober and request limit tests.

use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Utc};
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

/// Size and `Last-Modified` per key.
type Metadata = BTreeMap<String, (u64, Option<DateTime<Utc>>)>;

/// Answers HEADs, and GETs with an empty body, from scripted metadata: a
/// key without an entry in `objects` is NotFound, a key in `failing` fails.
/// Records every key HEADed, counts every GET, and counts the operations in
/// flight while each sleeps `delay`.
#[derive(Debug, Default)]
pub(crate) struct MetaStore {
    pub objects: Mutex<Metadata>,
    pub failing: Mutex<HashSet<String>>,
    pub headed: Mutex<Vec<String>>,
    pub delay: Duration,
    /// GETs (not HEADs) asked for.
    pub gets: Arc<AtomicUsize>,
    pub active: Arc<AtomicUsize>,
    pub peak: Arc<AtomicUsize>,
}

impl MetaStore {
    pub fn set(&self, key: &str, size: u64, last_modified: Option<DateTime<Utc>>) {
        self.objects
            .lock()
            .unwrap()
            .insert(key.to_string(), (size, last_modified));
    }

    pub fn remove(&self, key: &str) {
        self.objects.lock().unwrap().remove(key);
    }

    /// The keys HEADed since the last call, sorted.
    pub fn take_headed(&self) -> Vec<String> {
        let mut headed = std::mem::take(&mut *self.headed.lock().unwrap());
        headed.sort();
        headed
    }
}

impl std::fmt::Display for MetaStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MetaStore")
    }
}

#[async_trait::async_trait]
impl ObjectStore for MetaStore {
    async fn put_opts(
        &self,
        _path: &Path,
        _payload: PutPayload,
        _options: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        unimplemented!("MetaStore is read-only")
    }

    async fn put_multipart_opts(
        &self,
        _path: &Path,
        _options: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        unimplemented!("MetaStore is read-only")
    }

    async fn get_opts(
        &self,
        path: &Path,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        let key = path.to_string();
        if options.head {
            self.headed.lock().unwrap().push(key.clone());
        } else {
            self.gets.fetch_add(1, Ordering::SeqCst);
        }
        let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        if self.failing.lock().unwrap().contains(&key) {
            return Err(object_store::Error::Generic {
                store: "MetaStore",
                source: "server unavailable".into(),
            });
        }
        let Some((size, last_modified)) = self.objects.lock().unwrap().get(&key).copied() else {
            return Err(object_store::Error::NotFound {
                path: key,
                source: "absent".into(),
            });
        };
        Ok(object_store::GetResult {
            payload: object_store::GetResultPayload::Stream(futures::stream::empty().boxed()),
            meta: ObjectMeta {
                location: path.clone(),
                last_modified: last_modified.unwrap_or(DateTime::UNIX_EPOCH),
                size,
                e_tag: None,
                version: None,
            },
            range: 0..0,
            attributes: Default::default(),
            extensions: Default::default(),
        })
    }

    fn delete_stream(
        &self,
        paths: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        paths.boxed()
    }

    fn list(&self, _prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        futures::stream::empty().boxed()
    }

    async fn list_with_delimiter(
        &self,
        _prefix: Option<&Path>,
    ) -> object_store::Result<object_store::ListResult> {
        unimplemented!("MetaStore does not list")
    }

    async fn copy_opts(
        &self,
        _from: &Path,
        _to: &Path,
        _options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        unimplemented!("MetaStore is read-only")
    }
}

/// The object a [`RangeServer`] serves at `/object`.
#[derive(Debug)]
pub(crate) struct RangeObject {
    pub body: Vec<u8>,
    pub last_modified: DateTime<Utc>,
    /// Answer a GET whatever its `If-Unmodified-Since`, as a server that
    /// does not implement it would.
    pub ignore_preconditions: bool,
    /// Answer every request for the object with this status line instead,
    /// such as `403 Forbidden`.
    pub status: Option<&'static str>,
    /// The head of the last request, lowercased.
    pub last_request: String,
}

/// A one-object HTTP/1.1 server on 127.0.0.1, for the range read tests:
/// HEAD and ranged GET (206) of `/object`, a 412 when the object was
/// modified after a request's `If-Unmodified-Since`, a 416 for a range
/// starting past its end, and a 404 for any other path. One connection at
/// a time, each closed after its response.
pub(crate) struct RangeServer {
    pub url: String,
    pub object: Arc<Mutex<RangeObject>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl RangeServer {
    pub fn new(body: &[u8], last_modified: DateTime<Utc>) -> Self {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let object = Arc::new(Mutex::new(RangeObject {
            body: body.to_vec(),
            last_modified,
            ignore_preconditions: false,
            status: None,
            last_request: String::new(),
        }));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (serving, stopping) = (object.clone(), stop.clone());
        let worker = std::thread::spawn(move || {
            while !stopping.load(Ordering::Relaxed) {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("accept: {e}"),
                };
                // macOS hands an accepted socket the listener's O_NONBLOCK.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buf = [0; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = stream.read(&mut buf).unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&buf[..n]);
                }
                let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                let response = respond(&mut serving.lock().unwrap(), &request);
                stream.write_all(&response).unwrap();
            }
        });
        Self {
            url,
            object,
            stop,
            worker: Some(worker),
        }
    }

    /// Replace the object.
    pub fn replace(&self, body: &[u8], last_modified: DateTime<Utc>) {
        let mut object = self.object.lock().unwrap();
        object.body = body.to_vec();
        object.last_modified = last_modified;
    }
}

impl Drop for RangeServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}

/// The response to one lowercased request head.
fn respond(object: &mut RangeObject, request: &str) -> Vec<u8> {
    const HTTP_DATE: &str = "%a, %d %b %Y %H:%M:%S GMT";
    object.last_request = request.to_string();
    let mut fields = request.split_whitespace();
    let (method, path) = (fields.next().unwrap(), fields.next().unwrap());
    let header = |name: &str| {
        request
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
            .map(str::trim)
    };
    let status =
        |code: &str| format!("HTTP/1.1 {code}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    if path != "/object" {
        return status("404 Not Found").into_bytes();
    }
    if let Some(code) = object.status {
        return status(code).into_bytes();
    }
    let unmodified_since =
        header("if-unmodified-since").and_then(|date| DateTime::parse_from_rfc2822(date).ok());
    if !object.ignore_preconditions
        && unmodified_since.is_some_and(|since| object.last_modified > since)
    {
        return status("412 Precondition Failed").into_bytes();
    }
    let modified = object.last_modified.format(HTTP_DATE);
    let total = object.body.len();
    let (code, range, extra) = match header("range").and_then(|r| r.strip_prefix("bytes=")) {
        Some(range) => {
            let (start, end) = range.split_once('-').unwrap();
            let (start, end): (usize, usize) = (start.parse().unwrap(), end.parse().unwrap());
            if start >= total {
                return format!(
                    "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{total}\r\n\
                     Content-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .into_bytes();
            }
            let end = end.min(total - 1);
            (
                "206 Partial Content",
                start..end + 1,
                format!("Content-Range: bytes {start}-{end}/{total}\r\n"),
            )
        }
        None => ("200 OK", 0..total, String::new()),
    };
    let body = if method == "head" {
        &[][..]
    } else {
        &object.body[range.clone()]
    };
    let mut response = format!(
        "HTTP/1.1 {code}\r\nContent-Length: {}\r\n{extra}Last-Modified: {modified}\r\n\
         Connection: close\r\n\r\n",
        range.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}
