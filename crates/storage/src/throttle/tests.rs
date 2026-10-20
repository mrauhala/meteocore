//! Host back-off (#1235) through the stores [`crate::build_http_store`] and
//! [`crate::build_http_store_limited`] build: over a scripted transport in
//! paused time, and once over a real server.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use ds_core::error::DataServerError;
use object_store::client::{HttpError, HttpRequest, HttpResponse, HttpResponseBody, HttpService};
use object_store::path::Path;
use object_store::ObjectStoreExt;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_LENGTH, RETRY_AFTER};
use reqwest::StatusCode;
use tokio::time::Instant;

use super::{retry_after, CUT_WINDOW, MAX_RETRY_AFTER, QUIET_INTERVAL, WARN_INTERVAL};
use crate::test_store::{JsonLog, RangeServer};
use crate::{DataStore, FetchBudget, StorageError};

/// The body of every object the scripted host serves.
const BODY: &[u8] = b"0123456789";

/// What the scripted host answers one request.
#[derive(Debug)]
struct Answer {
    status: u16,
    retry_after: Option<String>,
    /// How long the answer takes.
    after: Duration,
}

#[derive(Debug, Default)]
struct Script {
    /// The answers to the next requests, in order; a 200 after them.
    answers: Mutex<VecDeque<Answer>>,
    /// How long a 200 takes.
    delay: Duration,
    /// When each request was sent, and the status it was answered.
    sent: Mutex<Vec<(Instant, u16)>>,
    /// Requests in flight now, and the most since the last `take_peak`.
    active: AtomicUsize,
    peak: AtomicUsize,
}

/// A host answering from a script, in place of the `reqwest::Client` under
/// the throttle: HEAD and GET of any key, 200 with [`BODY`] unless the
/// script says otherwise.
#[derive(Debug, Clone)]
struct Scripted(Arc<Script>);

impl Scripted {
    /// A host whose 200s take `delay`.
    fn new(delay: Duration) -> Self {
        Self(Arc::new(Script {
            delay,
            ..Script::default()
        }))
    }

    /// Answer the next request without a script with `status` after
    /// `after`, carrying `retry_after`.
    fn answer(&self, status: u16, retry_after: Option<&str>, after: Duration) {
        self.0.answers.lock().unwrap().push_back(Answer {
            status,
            retry_after: retry_after.map(str::to_string),
            after,
        });
    }

    /// Answer the next request without a script with a 429 at once.
    fn throttle(&self, retry_after: Option<&str>) {
        self.answer(429, retry_after, Duration::ZERO);
    }

    fn sent(&self) -> Vec<(Instant, u16)> {
        self.0.sent.lock().unwrap().clone()
    }

    /// The most requests in flight at once since the last call.
    fn take_peak(&self) -> usize {
        self.0.peak.swap(0, Ordering::SeqCst)
    }
}

/// One request in flight at the scripted host, until dropped.
struct Active(Arc<Script>);

impl Active {
    fn new(script: &Arc<Script>) -> Self {
        let now = script.active.fetch_add(1, Ordering::SeqCst) + 1;
        script.peak.fetch_max(now, Ordering::SeqCst);
        Self(script.clone())
    }
}

impl Drop for Active {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl HttpService for Scripted {
    async fn call(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        let script = &self.0;
        let answer = script.answers.lock().unwrap().pop_front();
        let answer = answer.unwrap_or(Answer {
            status: 200,
            retry_after: None,
            after: script.delay,
        });
        script
            .sent
            .lock()
            .unwrap()
            .push((Instant::now(), answer.status));
        let _active = Active::new(script);
        tokio::time::sleep(answer.after).await;
        let ok = answer.status == 200;
        let body = match ok && request.method() == reqwest::Method::GET {
            true => Bytes::from_static(BODY),
            false => Bytes::new(),
        };
        let mut response = HttpResponse::new(HttpResponseBody::from(body));
        *response.status_mut() = StatusCode::from_u16(answer.status).unwrap();
        let headers = response.headers_mut();
        if ok {
            headers.insert(CONTENT_LENGTH, HeaderValue::from(BODY.len()));
        }
        if let Some(value) = answer.retry_after {
            headers.insert(RETRY_AFTER, HeaderValue::from_str(&value).unwrap());
        }
        Ok(response)
    }
}

/// The store [`crate::build_http_store_limited`] builds for `url`, on
/// `host`.
fn limited(url: &str, limit: usize, host: &Scripted) -> (DataStore, Path) {
    crate::limited_http_store(url, limit, |base| {
        crate::throttled_http_store(base, host.clone())
    })
    .unwrap()
}

/// The store [`crate::build_http_store`] builds for `url`, on `host`.
fn unlimited(url: &str, host: &Scripted) -> (DataStore, Path) {
    crate::http_store(url, |base| crate::throttled_http_store(base, host.clone())).unwrap()
}

/// `(limit, in flight, queued)` of `host`, as [`crate::host_requests`]
/// reports it. Only `host` is read: reading another test's host by this
/// test's paused clock would grow its limit.
fn requests(host: &str) -> (usize, usize, usize) {
    let all = crate::requests_to(|label| label == host);
    let [host] = all.as_slice() else {
        panic!("one entry for {host}: {all:?}");
    };
    (host.limit, host.in_flight, host.queued)
}

/// `n` HEADs of `path` at once through `store`, all successful.
async fn heads(store: &DataStore, path: &Path, n: usize) {
    let all = futures::future::join_all((0..n).map(|_| store.inner().head(path))).await;
    for head in all {
        head.unwrap();
    }
}

/// The most requests at `host` at once over `n` HEADs at once.
async fn peak(host: &Scripted, store: &DataStore, path: &Path, n: usize) -> usize {
    host.take_peak();
    heads(store, path, n).await;
    host.take_peak()
}

/// A 429 halves a limited host's limit; the 429s answering requests sent
/// together cut it once; each quiet interval adds one back, up to the
/// configured limit and no further.
#[tokio::test(start_paused = true)]
async fn a_429_halves_the_limit_once_per_window_and_quiet_time_restores_it() {
    const HOST: &str = "cut.backoff.test";
    let host = Scripted::new(Duration::from_millis(100));
    let (store, path) = limited(&format!("http://{HOST}/meps.nc"), 8, &host);
    assert_eq!(peak(&host, &store, &path, 16).await, 8);

    host.throttle(None);
    store.inner().head(&path).await.unwrap();
    assert_eq!(requests(HOST), (4, 0, 0));
    assert_eq!(peak(&host, &store, &path, 16).await, 4);

    tokio::time::sleep(CUT_WINDOW).await;
    for _ in 0..4 {
        host.throttle(None);
    }
    heads(&store, &path, 16).await;
    let throttled = host
        .sent()
        .iter()
        .filter(|(_, status)| *status == 429)
        .count();
    assert_eq!(throttled, 5);
    assert_eq!(requests(HOST), (2, 0, 0), "four 429s at once cut once");
    assert_eq!(peak(&host, &store, &path, 16).await, 2);

    tokio::time::sleep(QUIET_INTERVAL).await;
    assert_eq!(requests(HOST).0, 3);
    tokio::time::sleep(QUIET_INTERVAL * 5).await;
    assert_eq!(requests(HOST).0, 8);
    tokio::time::sleep(QUIET_INTERVAL * 5).await;
    assert_eq!(requests(HOST).0, 8, "never past the configured limit");
    assert_eq!(peak(&host, &store, &path, 16).await, 8);
}

/// `Retry-After: 2` holds every request to the host for 2 s: the retry of
/// the throttled one, another collection's on the host's limited store,
/// and an unlimited store's.
#[tokio::test(start_paused = true)]
async fn a_retry_after_pauses_every_store_on_the_host() {
    const HOST: &str = "pause.backoff.test";
    let host = Scripted::new(Duration::ZERO);
    let (sfc, path) = limited(&format!("http://{HOST}/sfc/meps.nc"), 8, &host);
    let (pl, _) = limited(&format!("http://{HOST}/pl/meps.nc"), 8, &host);
    let (feed, _) = unlimited(&format!("http://{HOST}/feed.xml"), &host);
    let (sfc, pl) = (sfc.for_collection("meps-sfc"), pl.for_collection("meps-pl"));
    assert!(sfc.same_store(&pl));
    let later = |store: DataStore| {
        let path = path.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            store.inner().head(&path).await.unwrap();
        }
    };

    host.throttle(Some("2"));
    let start = Instant::now();
    let (first, (), ()) = tokio::join!(sfc.inner().head(&path), later(pl), later(feed));
    first.unwrap();
    let resumed = start + Duration::from_secs(2);
    assert_eq!(
        host.sent(),
        [(start, 429), (resumed, 200), (resumed, 200), (resumed, 200)]
    );
}

/// The wait between a 429 carrying `retry_after` and the next request
/// `store` sends.
async fn pause(host: &Scripted, store: &DataStore, path: &Path, retry_after: &str) -> Duration {
    host.throttle(Some(retry_after));
    store.inner().head(path).await.unwrap();
    let sent = host.sent();
    let [.., (throttled, 429), (next, 200)] = sent.as_slice() else {
        panic!("a 429 and its retry: {sent:?}");
    };
    *next - *throttled
}

/// An HTTP-date `Retry-After` waits until then; a huge one waits
/// [`MAX_RETRY_AFTER`]; a date already past pauses nothing, leaving
/// object_store's own backoff.
#[tokio::test(start_paused = true)]
async fn retry_after_takes_dates_and_is_capped() {
    let host = Scripted::new(Duration::ZERO);
    let (store, path) = unlimited("http://dates.backoff.test/x", &host);
    let in_ten = chrono::Utc::now() + chrono::Duration::seconds(10);
    let date = in_ten.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
    let waited = pause(&host, &store, &path, &date).await;
    assert!(
        (Duration::from_secs(9)..=Duration::from_secs(10)).contains(&waited),
        "{waited:?}"
    );
    for huge in ["86400", "99999999999999999999999"] {
        assert_eq!(pause(&host, &store, &path, huge).await, MAX_RETRY_AFTER);
    }
    let past = pause(&host, &store, &path, "Sun, 06 Nov 1994 08:49:37 GMT").await;
    assert!(past < Duration::from_secs(1), "{past:?}");
}

/// A `Retry-After` past the request deadline fails the operation at the
/// deadline, and past the fetch budget at the budget, never later and
/// without sending another request.
#[tokio::test(start_paused = true)]
async fn a_retry_after_past_the_deadline_or_budget_fails_there() {
    let host = Scripted::new(Duration::ZERO);
    let (store, path) = limited("http://deadline.backoff.test/x", 8, &host);
    let (store, path) = (&store, &path);
    let head = |budget, deadline| {
        crate::within(budget, deadline, async move {
            store
                .inner()
                .head(path)
                .await
                .map_err(|e| DataServerError::from(StorageError::from(e)))
        })
    };

    host.throttle(Some("5"));
    for budget in [FetchBudget::Request, FetchBudget::Background] {
        let start = Instant::now();
        let deadline = start.into_std() + Duration::from_secs(1);
        let result = head(budget, Some(deadline)).await;
        assert!(
            matches!(result, Err(DataServerError::DeadlineExceeded)),
            "{result:?}"
        );
        assert_eq!(start.elapsed(), Duration::from_secs(1));
    }
    assert_eq!(host.sent().len(), 1, "only the throttled request was sent");

    tokio::time::sleep(Duration::from_secs(5)).await;
    host.throttle(Some("45"));
    let start = Instant::now();
    match head(FetchBudget::Request, None).await {
        Err(DataServerError::Storage(message)) => {
            assert_eq!(message, "Request timed out after 30s")
        }
        other => panic!("the request budget must time out, got {other:?}"),
    }
    assert_eq!(start.elapsed(), FetchBudget::Request.timeout());
    assert_eq!(host.sent().len(), 2);
    tokio::time::sleep(Duration::from_secs(15)).await;
    head(FetchBudget::Request, None).await.unwrap();
}

/// A cut neither waits for nor cancels the requests in flight: they
/// finish, the waiting ones go out under the new limit, and the gauges
/// return to zero.
#[tokio::test(start_paused = true)]
async fn requests_in_flight_during_a_cut_finish_and_the_gauges_settle() {
    const HOST: &str = "inflight.backoff.test";
    let host = Scripted::new(Duration::from_secs(1));
    let (store, path) = limited(&format!("http://{HOST}/x"), 4, &host);
    host.answer(429, None, Duration::from_millis(100));
    let all = {
        let (store, path) = (store.clone(), path.clone());
        tokio::spawn(async move {
            futures::future::join_all((0..8).map(|_| store.inner().head(&path))).await
        })
    };
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(requests(HOST), (2, 4, 4), "cut with four in flight");
    for head in all.await.unwrap() {
        head.unwrap();
    }
    assert_eq!(requests(HOST), (2, 0, 0));
    assert_eq!(peak(&host, &store, &path, 8).await, 2);
    assert_eq!(requests(HOST), (2, 0, 0));
}

/// Under the adaptive limit a GET keeps its permit until its body has
/// been read.
#[tokio::test(start_paused = true)]
async fn a_get_holds_its_permit_until_its_body_is_read() {
    const HOST: &str = "body.backoff.test";
    let host = Scripted::new(Duration::ZERO);
    let (store, path) = limited(&format!("http://{HOST}/x"), 1, &host);
    let got = store.inner().get(&path).await.unwrap();
    assert_eq!(requests(HOST), (1, 1, 0));
    let waiting = tokio::time::timeout(Duration::from_secs(1), store.inner().head(&path)).await;
    assert!(waiting.is_err(), "the unread body holds the only permit");
    assert_eq!(got.bytes().await.unwrap(), BODY);
    assert_eq!(requests(HOST), (1, 0, 0));
    store.inner().head(&path).await.unwrap();
}

/// A host that never answers 429 keeps its limit and its pace, and logs
/// nothing.
#[tokio::test(start_paused = true)]
async fn a_host_that_never_throttles_keeps_its_limit_and_pace() {
    const HOST: &str = "calm.backoff.test";
    let log = JsonLog::default();
    let _logging = tracing::subscriber::set_default(log.subscriber());
    let host = Scripted::new(Duration::from_millis(100));
    let (store, path) = limited(&format!("http://{HOST}/x"), 4, &host);
    let start = Instant::now();
    assert_eq!(peak(&host, &store, &path, 16).await, 4);
    assert_eq!(start.elapsed(), Duration::from_millis(400), "four rounds");
    tokio::time::sleep(QUIET_INTERVAL * 4).await;
    assert_eq!(requests(HOST), (4, 0, 0));
    assert_eq!(log.lines(), Vec::<serde_json::Value>::new());
}

/// The lines the back-off logged at `level`, without object_store's.
fn at_level(log: &JsonLog, level: &str) -> Vec<serde_json::Value> {
    let ours = |line: &serde_json::Value| {
        let target = line["target"].as_str().unwrap_or_default();
        target.starts_with("ds_storage::throttle")
    };
    let lines = log.lines().into_iter();
    lines
        .filter(|line| ours(line) && line["level"] == level)
        .collect()
}

/// A cut logs one WARN per host per minute naming the host, the limit,
/// the configured limit and the `Retry-After`; the limit's return to the
/// configured one logs one INFO.
#[tokio::test(start_paused = true)]
async fn a_cut_warns_once_a_minute_and_the_return_is_logged() {
    const HOST: &str = "log.backoff.test";
    let log = JsonLog::default();
    let _logging = tracing::subscriber::set_default(log.subscriber());
    let host = Scripted::new(Duration::ZERO);
    let (store, path) = limited(&format!("http://{HOST}/x"), 8, &host);
    let store = store.for_collection("meps-sfc");

    host.throttle(Some("2"));
    store.inner().head(&path).await.unwrap();
    tokio::time::sleep(CUT_WINDOW).await;
    host.throttle(None);
    store.inner().head(&path).await.unwrap();
    assert_eq!(requests(HOST).0, 2, "cut twice");
    let warns = at_level(&log, "WARN");
    assert_eq!(warns.len(), 1, "{warns:#?}");
    let warn = &warns[0];
    assert_eq!(warn["host"], HOST);
    assert_eq!(warn["limit"], 4);
    assert_eq!(warn["configured_limit"], 8);
    assert_eq!(warn["retry_after_secs"], 2.0);
    assert_eq!(
        warn["message"],
        format!(
            "{HOST} answered 429 Too Many Requests: request limit cut to 4 of 8; \
             new requests wait 2.0 s (Retry-After)"
        )
    );
    assert_eq!(warn["span"]["collection"], "meps-sfc", "{warn:#}");

    tokio::time::sleep(WARN_INTERVAL).await;
    assert_eq!(requests(HOST).0, 4, "two quiet intervals");
    host.throttle(None);
    store.inner().head(&path).await.unwrap();
    let warns = at_level(&log, "WARN");
    assert_eq!(warns.len(), 2, "{warns:#?}");
    assert_eq!(
        warns[1]["message"],
        format!("{HOST} answered 429 Too Many Requests: request limit cut to 2 of 8")
    );
    assert!(warns[1].get("retry_after_secs").is_none(), "{:#}", warns[1]);

    assert!(at_level(&log, "INFO").is_empty());
    tokio::time::sleep(QUIET_INTERVAL * 6).await;
    assert_eq!(requests(HOST).0, 8);
    assert_eq!(requests(HOST).0, 8);
    let infos = at_level(&log, "INFO");
    assert_eq!(infos.len(), 1, "{infos:#?}");
    assert_eq!(
        infos[0]["message"],
        format!("{HOST} has answered no 429 for 30 s: its request limit is back at 8")
    );
}

/// An unlimited store's 429 without `Retry-After` changes nothing for the
/// host: no pause, no line.
#[tokio::test(start_paused = true)]
async fn a_bare_429_on_an_unlimited_host_pauses_nothing() {
    let log = JsonLog::default();
    let _logging = tracing::subscriber::set_default(log.subscriber());
    let host = Scripted::new(Duration::ZERO);
    let (store, path) = unlimited("http://bare.backoff.test/x", &host);
    host.throttle(None);
    let (first, second) = tokio::join!(store.inner().head(&path), store.inner().head(&path));
    first.unwrap();
    second.unwrap();
    let sent = host.sent();
    assert_eq!(sent[0].0, sent[1].0, "the other request went out at once");
    assert!(at_level(&log, "WARN").is_empty());
}

/// `Retry-After` in delta-seconds and in each HTTP-date form, measured
/// from `now`.
#[test]
fn retry_after_reads_seconds_and_http_dates() {
    let now = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(784_111_717);
    let read = |value: &str| {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_str(value).unwrap());
        retry_after(&headers, now)
    };
    let secs = |s| Some(Duration::from_secs(s));
    assert_eq!(read("120"), secs(120));
    assert_eq!(read(" 0 "), secs(0));
    assert_eq!(read("99999999999999999999999"), secs(u64::MAX));
    // 1994-11-06T08:49:37Z is `now` + 60 s, in the three forms.
    assert_eq!(read("Sun, 06 Nov 1994 08:49:37 GMT"), secs(60));
    assert_eq!(read("Sunday, 06-Nov-94 08:49:37 GMT"), secs(60));
    assert_eq!(read("Sun Nov  6 08:49:37 1994"), secs(60));
    assert_eq!(read("Sun, 06 Nov 1994 08:47:37 GMT"), secs(0), "past");
    assert_eq!(read("soon"), None);
    assert_eq!(read("-5"), None);
    assert_eq!(read(""), None);
    assert_eq!(retry_after(&HeaderMap::new(), now), None);
}

/// Over a real server: the retry of a 429 with `Retry-After: 1` waits for
/// it, and the host's limit is cut.
#[tokio::test(flavor = "multi_thread")]
async fn a_retry_after_over_http_holds_the_retry() {
    let modified = "2026-10-17T08:00:00Z".parse().unwrap();
    let server = RangeServer::new(BODY, modified);
    server
        .object
        .lock()
        .unwrap()
        .fail_next
        .push_back("429 Too Many Requests\r\nRetry-After: 1");
    let url = format!("{}/object", server.url);
    let (store, path) = crate::build_http_store_limited(&url, 4).unwrap();
    let started = std::time::Instant::now();
    assert_eq!(store.get_range(&path, 2..5).unwrap(), &b"234"[..]);
    let waited = started.elapsed();
    assert!(
        (Duration::from_secs(1)..Duration::from_secs(10)).contains(&waited),
        "{waited:?}"
    );
    assert_eq!(
        requests(server.url.trim_start_matches("http://")),
        (2, 0, 0)
    );
}
