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
//!   within the caller's budget and deadline.
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
/// object as a document of at most `max_bytes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Document {
    pub max_bytes: u64,
}

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
    async fn document(&self, location: &Path, document: Document) -> Result<GetResult> {
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .map_err(|()| generic(format!("{} cannot have a path", self.base_url)))?
            .extend(location.parts());
        let mut attempt = 1;
        loop {
            match self.attempt(location, &url, document).await {
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

    /// One GET of `url`, read whole.
    async fn attempt(
        &self,
        location: &Path,
        url: &url::Url,
        document: Document,
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
                "document {url} is {bytes} bytes or more, over the {}-byte limit",
                document.max_bytes
            )))
        };
        if let Some(declared) = declared.filter(|&declared| declared > document.max_bytes) {
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
            if total > document.max_bytes {
                return Err(over(total));
            }
            read.extend_from_slice(&chunk);
        }
        let bytes = read.freeze();
        let size = bytes.len() as u64;
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
            extensions: Default::default(),
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
            Some(&document) if !options.head && options.range.is_none() => {
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

    use object_store::path::Path;

    use ds_core::error::DataServerError;

    use crate::FetchBudget;

    const CATALOG: &[u8] = b"<catalog><dataset name=\"a.nc\" urlPath=\"a.nc\"/></catalog>";

    /// A server answering as thredds.met.no answers a catalog: 200 with
    /// `Transfer-Encoding: chunked` and no `Content-Length`, the body in
    /// chunks of 7 bytes. `/big.xml` is 3000 bytes so; anything else 404.
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
                let body: Vec<u8> = match path {
                    "/catalog.xml" => CATALOG.to_vec(),
                    "/big.xml" => vec![b'x'; 3000],
                    _ => {
                        let _ = stream.write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\
                              Connection: close\r\n\r\n",
                        );
                        continue;
                    }
                };
                let mut response =
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/xml;charset=UTF-8\r\n\
                    Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
                        .to_vec();
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
}
