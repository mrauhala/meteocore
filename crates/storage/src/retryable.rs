//! The answers from a remote host that object_store retries, counted by
//! collection, host and status for `/metrics` (#1336).
//!
//! object_store retries a 5xx, a 429 Too Many Requests and a 408 Request
//! Timeout with back-off, and logs each retry at INFO in the `storage` span
//! (#1233). A public S3 bucket answers `503 SlowDown` to a large share of
//! GETs once a few are in flight, so a cold query or a poll can spend
//! seconds in retries. The HTTP hosts' 429s are counted per host
//! (`throttle.rs`), and nothing else was.
//!
//! Every remote store this crate builds sends its requests through a
//! [`Counted`] transport: an HTTP store's under its host's throttle, an S3
//! store's through [`CountingConnector`], over object_store's own client.
//! It sees the response to each attempt, object_store's retries and the
//! document layer's included, and counts each retryable one under the
//! collection the operation's `storage` span names, the `host[:port]` that
//! answered and the status ([`retryable_responses`]). The span's layer
//! (`traced.rs`) sets the collection for the transport to read
//! ([`COLLECTION`]), so the collections that share one host's store, as
//! [`crate::build_http_store_limited`] shares it, are counted apart, and a
//! store no engine has named counts under none. The labels stay bounded:
//! the configured collections, the hosts their stores read and the
//! statuses, never an object key or a query string.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};

use object_store::client::{
    HttpClient, HttpConnector, HttpError, HttpRequest, HttpResponse, HttpService, ReqwestConnector,
};
use object_store::ClientOptions;
use reqwest::StatusCode;

tokio::task_local! {
    /// The collection the operation in progress reads for, as its
    /// `storage` span names it: set around each operation of a remote
    /// store and each poll of its GET body or listing (`traced.rs`).
    pub(crate) static COLLECTION: Option<Arc<str>>;
}

/// One count: the collection, the `host[:port]` that answered, the status.
type Key = (Option<Arc<str>>, String, u16);

/// The retryable answers, since the process started or their collection
/// was last forgotten ([`forget_retryable_responses`]).
static ANSWERS: LazyLock<Mutex<BTreeMap<Key, u64>>> = LazyLock::new(Default::default);

fn answers() -> MutexGuard<'static, BTreeMap<Key, u64>> {
    ANSWERS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What object_store retries: a 5xx, 429 Too Many Requests or 408 Request
/// Timeout.
fn retryable(status: StatusCode) -> bool {
    status.is_server_error()
        || status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::REQUEST_TIMEOUT
}

/// Count one retryable answer from the host of `uri` to the operation in
/// progress: its `host[:port]` as the `storage` span names a store's, never
/// its userinfo, path or query.
fn count(uri: &http::Uri, status: StatusCode) {
    let collection = COLLECTION.try_with(Clone::clone).ok().flatten();
    let host = url::Url::parse(&uri.to_string())
        .ok()
        .as_ref()
        .and_then(crate::traced::host_port)
        .unwrap_or_default();
    *answers()
        .entry((collection, host, status.as_u16()))
        .or_default() += 1;
}

/// The retryable answers of one host, with one status, to the requests of
/// one collection ([`retryable_responses`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryableResponses {
    /// The collection the requests read for, as
    /// [`crate::DataStore::for_collection`] named it: `None` for a store
    /// no engine has named.
    pub collection: Option<String>,
    /// The `host[:port]` that answered, as `s3.eu-central-1.amazonaws.com`
    /// or `127.0.0.1:9000`.
    pub host: String,
    /// The status: a 5xx, 429 or 408.
    pub status: u16,
    /// The answers since the process started, or since the collection was
    /// last forgotten.
    pub count: u64,
}

/// Every answer object_store retries that a remote store this crate built
/// has had, object_store's retries included: one entry per collection,
/// host and status that has had one.
pub fn retryable_responses() -> Vec<RetryableResponses> {
    answers()
        .iter()
        .map(|((collection, host, status), count)| RetryableResponses {
            collection: collection.as_deref().map(str::to_string),
            host: host.clone(),
            status: *status,
            count: *count,
        })
        .collect()
}

/// Drop the counts of `collections`, which a reload removed: one configured
/// again counts from zero.
pub fn forget_retryable_responses<'a>(collections: impl IntoIterator<Item = &'a str>) {
    let removed: HashSet<&str> = collections.into_iter().collect();
    answers().retain(|(collection, _, _), _| {
        collection
            .as_deref()
            .is_none_or(|collection| !removed.contains(collection))
    });
}

/// A transport that counts each retryable answer before handing it back.
/// object_store calls it once per attempt, so it sees the attempts
/// object_store retries as well.
#[derive(Debug, Clone)]
pub(crate) struct Counted<S>(pub S);

#[async_trait::async_trait]
impl<S: HttpService> HttpService for Counted<S> {
    async fn call(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        let uri = request.uri().clone();
        let response = self.0.call(request).await?;
        if retryable(response.status()) {
            count(&uri, response.status());
        }
        Ok(response)
    }
}

/// An S3 store's connector: object_store's own, its client built from the
/// store's options as object_store builds it, with every request
/// [`Counted`].
#[derive(Debug, Default)]
pub(crate) struct CountingConnector;

impl HttpConnector for CountingConnector {
    fn connect(&self, options: &ClientOptions) -> object_store::Result<HttpClient> {
        let client = ReqwestConnector::default().connect(options)?;
        Ok(HttpClient::new(Counted(Client(client))))
    }
}

/// An object_store client as a transport.
#[derive(Debug)]
struct Client(HttpClient);

#[async_trait::async_trait]
impl HttpService for Client {
    async fn call(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        self.0.execute(request).await
    }
}

#[cfg(test)]
mod tests {
    use object_store::client::HttpRequestBody;
    use object_store::path::Path;

    use super::*;
    use crate::test_store::RangeServer;

    /// The counts of `host`, as `(collection, status, count)`.
    fn counts(host: &str) -> Vec<(Option<String>, u16, u64)> {
        retryable_responses()
            .into_iter()
            .filter(|answers| answers.host == host)
            .map(|answers| (answers.collection, answers.status, answers.count))
            .collect()
    }

    /// The counts of `host` that `read` adds. Another test's server may
    /// have had the port before, and its counts stay.
    fn added(host: &str, read: impl FnOnce()) -> Vec<(Option<String>, u16, u64)> {
        let before = counts(host);
        read();
        counts(host)
            .into_iter()
            .filter_map(|(collection, status, count)| {
                let was = before
                    .iter()
                    .find(|(c, s, _)| *c == collection && *s == status)
                    .map_or(0, |(_, _, count)| *count);
                (count > was).then_some((collection, status, count - was))
            })
            .collect()
    }

    fn server_with(path: &str) -> (RangeServer, String) {
        let server = RangeServer::new(b"0123456789", "2026-10-28T08:00:00Z".parse().unwrap());
        server.object.lock().unwrap().path = path.to_string();
        let host = server.url.trim_start_matches("http://").to_string();
        (server, host)
    }

    fn fail_next(server: &RangeServer, statuses: &[&'static str]) {
        server
            .object
            .lock()
            .unwrap()
            .fail_next
            .extend(statuses.iter().copied());
    }

    /// An S3 bucket's `503 SlowDown`, which object_store retries, counts
    /// once for the collection and host; the GET then succeeds and its 200
    /// counts nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_s3_slowdown_counts_for_the_collection_and_host() {
        let (server, host) = server_with("/bucket/object");
        fail_next(&server, &["503 Slow Down"]);
        let store = crate::build_s3_store_from_parts(&server.url, "bucket")
            .unwrap()
            .for_collection("nwp-slowdown");
        let added = added(&host, || {
            let got = store.get(&Path::from("object")).unwrap();
            assert_eq!(got, &b"0123456789"[..]);
        });
        assert_eq!(added, [(Some("nwp-slowdown".into()), 503, 1)]);
    }

    /// Two collections sharing one limited host store, and a store no
    /// engine has named: each answer counts under the collection whose
    /// operation it answered.
    #[tokio::test(flavor = "multi_thread")]
    async fn collections_sharing_a_host_store_are_counted_apart() {
        let (server, host) = server_with("/object");
        let url = format!("{}/object", server.url);
        let (sfc, path) = crate::build_http_store_limited(&url, 4).unwrap();
        let (pl, _) = crate::build_http_store_limited(&url, 4).unwrap();
        assert!(sfc.same_store(&pl));
        let (sfc, pl) = (
            sfc.for_collection("shared-sfc"),
            pl.for_collection("shared-pl"),
        );
        let (unnamed, _) = crate::build_http_store(&url).unwrap();
        let added = added(&host, || {
            for (store, statuses) in [
                (&sfc, &["502 Bad Gateway", "502 Bad Gateway"][..]),
                (&pl, &["500 Internal Server Error"][..]),
                (&unnamed, &["504 Gateway Timeout"][..]),
            ] {
                fail_next(&server, statuses);
                assert_eq!(store.get_range(&path, 2..5).unwrap(), &b"234"[..]);
            }
        });
        assert_eq!(
            added,
            [
                (None, 504, 1),
                (Some("shared-pl".into()), 500, 1),
                (Some("shared-sfc".into()), 502, 2),
            ]
        );
    }

    /// Only what object_store retries counts: a 5xx, 429 or 408, never a
    /// success, a redirect or another client error. The host is the
    /// URL's `host[:port]`, without its userinfo, a default port, the path
    /// or the query.
    #[tokio::test]
    async fn only_retryable_answers_count_under_the_host_alone() {
        #[derive(Debug, Clone)]
        struct Answer(StatusCode);

        #[async_trait::async_trait]
        impl HttpService for Answer {
            async fn call(&self, _: HttpRequest) -> Result<HttpResponse, HttpError> {
                let mut response = HttpResponse::new(Vec::new().into());
                *response.status_mut() = self.0;
                Ok(response)
            }
        }

        let send = |url: &'static str, status: u16| async move {
            let request = http::Request::builder()
                .uri(url)
                .body(HttpRequestBody::empty())
                .unwrap();
            let transport = Counted(Answer(StatusCode::from_u16(status).unwrap()));
            let call = transport.call(request);
            COLLECTION
                .scope(Some("statuses".into()), call)
                .await
                .unwrap();
        };
        for status in [200, 206, 304, 400, 403, 404, 409, 412, 416] {
            send("https://statuses.example/b/k", status).await;
        }
        assert_eq!(counts("statuses.example"), []);
        for status in [408, 429, 500, 503] {
            send("https://statuses.example/b/k", status).await;
        }
        send(
            "https://user:pw@Statuses.Example:443/b/k?X-Amz-Signature=s",
            503,
        )
        .await;
        send("https://statuses.example:8443/b/k?sig=s", 503).await;
        let statuses = Some("statuses".to_string());
        assert_eq!(
            counts("statuses.example"),
            [
                (statuses.clone(), 408, 1),
                (statuses.clone(), 429, 1),
                (statuses.clone(), 500, 1),
                (statuses.clone(), 503, 2),
            ]
        );
        assert_eq!(counts("statuses.example:8443"), [(statuses, 503, 1)]);
    }

    /// A forgotten collection's counts go, and only its own.
    #[test]
    fn a_forgotten_collection_counts_from_zero() {
        let uri: http::Uri = "https://forget.example/b/k".parse().unwrap();
        for collection in ["forget-a", "forget-b"] {
            COLLECTION.sync_scope(Some(collection.into()), || {
                count(&uri, StatusCode::SERVICE_UNAVAILABLE)
            });
        }
        count(&uri, StatusCode::SERVICE_UNAVAILABLE);
        forget_retryable_responses(["forget-a", "elsewhere"]);
        assert_eq!(
            counts("forget.example"),
            [(None, 503, 1), (Some("forget-b".into()), 503, 1)]
        );
    }
}
