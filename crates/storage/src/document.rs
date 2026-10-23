//! Whole documents over HTTP, `Content-Length` or not (#1170).
//!
//! object_store's HTTP GET takes an object's size from `Content-Length` and
//! fails a response without one. A generated document, such as a THREDDS
//! `catalog.xml`, is streamed chunked: thredds.met.no answers
//! `Transfer-Encoding: chunked` and no `Content-Length`. A GET whose
//! options carry [`Document`] ([`crate::DataStore::get_documents`]) is sent
//! by [`Documents`], the bottom of every HTTP store this crate builds,
//! instead:
//!
//! - through the same transport as every other request of the store: the
//!   host's [`Throttled`] `reqwest` client, so its `Retry-After` and 429s
//!   are the host's (#1235), no redirect is followed and nothing is
//!   decompressed;
//! - under every layer above it, as any GET: the `storage` span naming the
//!   collection (#1233), the host's adaptive limit and the request gauges,
//!   and a store's request share, each holding the request until the body
//!   has been read;
//! - read to its end with the size capped while it streams: a declared
//!   `Content-Length` over the cap fails before the body is read, and a
//!   body that grows past it fails at that chunk;
//! - retried as object_store retries: a 429, a 5xx or a failed connection
//!   up to [`ATTEMPTS`] times, each attempt after the host's `Retry-After`,
//!   within the caller's budget and deadline;
//! - with the query string the mark carries, if any: an OPeNDAP constraint
//!   expression (#1264). The result is marked [`Sent`], so a caller can
//!   tell that the query reached the server: any other store ignores it.
//!
//! Any other operation, and a GET without [`Document`], is the HTTP store's.

use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures::stream::{BoxStream, StreamExt};
use object_store::client::{HttpErrorKind, HttpRequest, HttpRequestBody, HttpService};
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, GetResultPayload, ListResult, MultipartUpload, ObjectMeta,
    ObjectStore, PutMultipartOptions, PutOptions, PutPayload, PutResult, RenameOptions, Result,
};
use reqwest::header::{CONTENT_LENGTH, ETAG, LAST_MODIFIED};
use reqwest::StatusCode;

use crate::throttle::Throttled;

/// Attempts at one document: the first and two retries.
pub(crate) const ATTEMPTS: u32 = 3;

/// The wait before the first retry, doubled before each next one, after
/// whatever `Retry-After` the host asked for.
const BACKOFF: Duration = Duration::from_millis(100);

/// The mark on a GET's [`GetOptions::extensions`] that asks for the whole
/// object as a document of at most `max_bytes`, with `query` as the URL's
/// query string (after the base URL's own, if it has one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Document {
    pub max_bytes: u64,
    pub query: Option<String>,
}

/// The mark on a [`GetResult::extensions`] this layer answered: the
/// document's query, if any, was sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sent;

/// The HTTP store `http`, sending GETs marked [`Document`] itself through
/// `transport`, the store's own.
#[derive(Debug)]
pub(crate) struct Documents<S> {
    pub http: Arc<dyn ObjectStore>,
    pub transport: Throttled<S>,
    /// The store's base, which keys are appended to as object_store does.
    pub base_url: url::Url,
}

/// An object_store error of this layer.
fn generic(source: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> object_store::Error {
    object_store::Error::Generic {
        store: "HTTP",
        source: source.into(),
    }
}

/// Why one attempt failed, and whether another may succeed.
enum Failure {
    Retry(object_store::Error),
    Fail(object_store::Error),
}

impl<S: HttpService + Clone> Documents<S> {
    /// GET `location` whole, at most `max_bytes`, retrying what object_store
    /// would retry.
    async fn document(&self, location: &Path, document: &Document) -> Result<GetResult> {
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .map_err(|()| generic(format!("{} cannot have a path", self.base_url)))?
            .extend(location.parts());
        if let Some(query) = &document.query {
            let joined = match url.query() {
                Some(base) if !base.is_empty() => [base, query.as_str()].join("&"),
                _ => query.clone(),
            };
            url.set_query(Some(&joined));
        }
        let mut attempt = 1;
        loop {
            match self.attempt(location, &url, document.max_bytes).await {
                Ok(result) => return Ok(result),
                Err(Failure::Retry(e)) if attempt < ATTEMPTS => {
                    let wait = BACKOFF * 2u32.pow(attempt - 1);
                    tracing::info!(
                        "retrying document GET of {url} in {} ms (attempt {attempt} of \
                         {ATTEMPTS}): {e}",
                        wait.as_millis()
                    );
                    tokio::time::sleep(wait).await;
                    attempt += 1;
                }
                Err(Failure::Retry(e) | Failure::Fail(e)) => return Err(e),
            }
        }
    }

    /// One GET of `url`, read whole, at most `max_bytes`.
    async fn attempt(
        &self,
        location: &Path,
        url: &url::Url,
        max_bytes: u64,
    ) -> std::result::Result<GetResult, Failure> {
        let request: HttpRequest = http::Request::builder()
            .method(http::Method::GET)
            .uri(url.as_str())
            .body(HttpRequestBody::empty())
            .map_err(|e| Failure::Fail(generic(e)))?;
        let response = match self.transport.call(request).await {
            Ok(response) => response,
            Err(e) => {
                let retry = matches!(
                    e.kind(),
                    HttpErrorKind::Connect
                        | HttpErrorKind::Request
                        | HttpErrorKind::Timeout
                        | HttpErrorKind::Interrupted
                );
                let e = generic(e);
                return Err(if retry {
                    Failure::Retry(e)
                } else {
                    Failure::Fail(e)
                });
            }
        };
        let status = response.status();
        if matches!(
            status,
            StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
        ) {
            return Err(Failure::Fail(object_store::Error::NotFound {
                path: location.to_string(),
                source: format!("{url} answered {status}").into(),
            }));
        }
        if !status.is_success() {
            // object_store's own wording, which `error::http_status` reads.
            let e = generic(format!(
                "Server returned non-2xx status code: {status}: {url}"
            ));
            let retry = status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
            return Err(if retry {
                Failure::Retry(e)
            } else {
                Failure::Fail(e)
            });
        }
        let headers = response.headers();
        let declared = headers
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok()?.parse::<u64>().ok());
        let over = |bytes: u64| {
            Failure::Fail(generic(format!(
                "document {url} is {bytes} bytes or more, over the {max_bytes}-byte limit"
            )))
        };
        if let Some(declared) = declared.filter(|&declared| declared > max_bytes) {
            return Err(over(declared));
        }
        let last_modified = headers
            .get(LAST_MODIFIED)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| chrono::DateTime::parse_from_rfc2822(value).ok())
            .map_or(chrono::DateTime::UNIX_EPOCH, |date| {
                date.with_timezone(&chrono::Utc)
            });
        let e_tag = headers
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let mut body = response.into_body().bytes_stream();
        let mut read = BytesMut::new();
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|e| Failure::Retry(generic(e)))?;
            let total = (read.len() + chunk.len()) as u64;
            if total > max_bytes {
                return Err(over(total));
            }
            read.extend_from_slice(&chunk);
        }
        let bytes = read.freeze();
        let size = bytes.len() as u64;
        let mut extensions = object_store::Extensions::new();
        extensions.insert(Sent);
        Ok(GetResult {
            payload: GetResultPayload::Stream(
                futures::stream::once(async move { Ok(bytes) }).boxed(),
            ),
            meta: ObjectMeta {
                location: location.clone(),
                last_modified,
                size,
                e_tag,
                version: None,
            },
            range: 0..size,
            attributes: Default::default(),
            extensions,
        })
    }
}

impl<S> std::fmt::Display for Documents<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.http.fmt(f)
    }
}

#[async_trait::async_trait]
impl<S: HttpService + Clone> ObjectStore for Documents<S> {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> Result<PutResult> {
        self.http.put_opts(location, payload, options).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        options: PutMultipartOptions,
    ) -> Result<Box<dyn MultipartUpload>> {
        self.http.put_multipart_opts(location, options).await
    }

    async fn get_opts(&self, location: &Path, options: GetOptions) -> Result<GetResult> {
        match options.extensions.get::<Document>() {
            Some(document) if !options.head && options.range.is_none() => {
                self.document(location, document).await
            }
            _ => self.http.get_opts(location, options).await,
        }
    }

    async fn get_ranges(&self, location: &Path, ranges: &[Range<u64>]) -> Result<Vec<Bytes>> {
        self.http.get_ranges(location, ranges).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, Result<Path>>,
    ) -> BoxStream<'static, Result<Path>> {
        self.http.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> {
        self.http.list(prefix)
    }

    fn list_with_offset(
        &self,
        prefix: Option<&Path>,
        offset: &Path,
    ) -> BoxStream<'static, Result<ObjectMeta>> {
        self.http.list_with_offset(prefix, offset)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> Result<ListResult> {
        self.http.list_with_delimiter(prefix).await
    }

    async fn copy_opts(&self, from: &Path, to: &Path, options: CopyOptions) -> Result<()> {
        self.http.copy_opts(from, to, options).await
    }

    async fn rename_opts(&self, from: &Path, to: &Path, options: RenameOptions) -> Result<()> {
        self.http.rename_opts(from, to, options).await
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::sync::Arc;

    use object_store::path::Path;

    use ds_core::error::DataServerError;

    use crate::FetchBudget;

    const CATALOG: &[u8] = b"<catalog><dataset name=\"a.nc\" urlPath=\"a.nc\"/></catalog>";

    /// A server answering as thredds.met.no answers a catalog: 200 with
    /// `Transfer-Encoding: chunked` and no `Content-Length`, the body in
    /// chunks of 7 bytes. `/big.xml` is 3000 bytes so; `/x/data.dods` with
    /// a query is the query it was asked with, `Last-Modified` 2026-10-22
    /// 21:30; anything else 404.
    fn chunked_server() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut head = Vec::new();
                let mut buf = [0; 1024];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&head).to_string();
                let path = head.split_whitespace().nth(1).unwrap_or_default();
                let mut modified = "";
                let body: Vec<u8> = match path.split_once('?') {
                    Some(("/x/data.dods", query)) => {
                        modified = "Last-Modified: Thu, 22 Oct 2026 21:30:00 GMT\r\n";
                        query.as_bytes().to_vec()
                    }
                    _ if path == "/catalog.xml" => CATALOG.to_vec(),
                    _ if path == "/big.xml" => vec![b'x'; 3000],
                    _ => {
                        let _ = stream.write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\
                              Connection: close\r\n\r\n",
                        );
                        continue;
                    }
                };
                let mut response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/xml;charset=UTF-8\r\n\
                     Transfer-Encoding: chunked\r\n{modified}Connection: close\r\n\r\n"
                )
                .into_bytes();
                for chunk in body.chunks(7) {
                    response.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
                    response.extend_from_slice(chunk);
                    response.extend_from_slice(b"\r\n");
                }
                response.extend_from_slice(b"0\r\n\r\n");
                let _ = stream.write_all(&response);
            }
        });
        url
    }

    /// Over the wire (#1170): a chunked document without `Content-Length`
    /// is read whole, where object_store's GET fails on the missing header;
    /// one over the cap fails while it streams; a missing one is not found.
    #[tokio::test(flavor = "multi_thread")]
    async fn chunked_documents_are_read_over_http() {
        let url = chunked_server();
        let (store, _) =
            crate::build_http_store_limited(&[url.as_str(), "x"].join("/"), 4).unwrap();
        let paths = ["catalog.xml", "big.xml", "missing.xml"].map(Path::from);
        let got = store
            .get_documents(&paths, 2, 1024, FetchBudget::Request)
            .unwrap();
        assert_eq!(got[0].as_ref().unwrap().as_ref(), CATALOG);
        let error = |i: usize| match &got[i] {
            Err(DataServerError::Storage(message)) => message.clone(),
            other => panic!("{other:?}"),
        };
        assert!(
            error(1).contains("over the 1024-byte limit"),
            "{}",
            error(1)
        );
        assert!(error(2).contains("not found"), "{}", error(2));
        assert_eq!(store.bytes_read(), CATALOG.len() as u64);

        let error = store.get(&paths[0]).unwrap_err().to_string();
        assert!(error.contains("Content-Length"), "{error}");
    }

    /// An OPeNDAP constraint (#1264) reaches the server as the query string,
    /// as given, and the response's `Last-Modified` comes back with the
    /// body; a missing document is `None`. A store that cannot send a query
    /// fails a request carrying one rather than answering without it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_query_reaches_the_server_and_the_version_comes_back() {
        use object_store::ObjectStoreExt;

        use crate::{DocumentRequest, FetchedDocument};

        let url = chunked_server();
        let (store, _) =
            crate::build_http_store_limited(&[url.as_str(), "x/"].join("/"), 4).unwrap();
        let query = "t.t%5B0%5D%5B0:1:3%5D,x";
        let requests = [
            DocumentRequest {
                path: Path::from("x/data.dods"),
                query: Some(query.to_string()),
            },
            DocumentRequest {
                path: Path::from("x/missing.dds"),
                query: None,
            },
        ];
        let handle = tokio::runtime::Handle::current();
        let got = tokio::task::block_in_place(|| {
            store.get_documents_on(&requests, 2, 1024, &handle, FetchBudget::Request)
        })
        .unwrap();
        assert_eq!(
            got[0].as_ref().unwrap(),
            &Some(FetchedDocument {
                bytes: query.as_bytes().to_vec().into(),
                last_modified: Some("2026-10-22T21:30:00Z".parse().unwrap()),
                e_tag: None,
            })
        );
        assert_eq!(got[1].as_ref().unwrap(), &None);

        let memory = crate::DataStore::new(Arc::new(object_store::memory::InMemory::new()));
        let put = object_store::PutPayload::from_static(b"whole");
        memory.inner().put(&requests[0].path, put).await.unwrap();
        let got = tokio::task::block_in_place(|| {
            memory.get_documents_on(&requests[..1], 1, 1024, &handle, FetchBudget::Request)
        })
        .unwrap();
        let error = got[0].as_ref().unwrap_err().to_string();
        assert!(error.contains("cannot send a query string"), "{error}");
    }
}
