//! Host back-off (#1235, #1290) through the stores [`crate::build_http_store`] and
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

use super::{
    for_url, retry_after, throttled_answers, CUT_WINDOW, MAX_RETRY_AFTER, PACE_CAP, PACE_FLOOR,
    QUIET_INTERVAL, RETRY_DELAY, WARN_INTERVAL,
};
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
    /// The body of every 200 to a GET, sent without `Content-Length` as a
    /// chunked one is, in place of [`BODY`] (#1170).
    chunked: Mutex<Option<Bytes>>,
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

    /// Answer every GET with `body` and no `Content-Length`; HEADs keep
    /// [`BODY`]'s.
    fn chunked(&self, body: &'static [u8]) {
        *self.0.chunked.lock().unwrap() = Some(Bytes::from_static(body));
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
        let chunked = script.chunked.lock().unwrap().clone();
        let body = match ok && request.method() == reqwest::Method::GET {
            true => chunked.clone().unwrap_or(Bytes::from_static(BODY)),
            false => Bytes::new(),
        };
        let mut response = HttpResponse::new(HttpResponseBody::from(body));
        *response.status_mut() = StatusCode::from_u16(answer.status).unwrap();
        let headers = response.headers_mut();
        let get = request.method() == reqwest::Method::GET;
        if ok && (chunked.is_none() || !get) {
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

/// The pace of `host` now.
fn pace(host: &str) -> Duration {
    for_url(&url::Url::parse(&format!("http://{host}/")).unwrap()).pace()
}

/// The times `host`'s requests from `from` on were sent at, from the first
/// of them.
fn spacing(host: &Scripted, from: usize) -> Vec<Duration> {
    let sent = host.sent();
    let first = sent[from].0;
    sent[from..].iter().map(|(at, _)| *at - first).collect()
}

/// object_store's wait before its first retry: its `init_backoff`.
const FIRST_BACKOFF: Duration = Duration::from_millis(100);

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

/// A 429 halves a limited host's limit, and paces it; the 429s answering
/// requests sent together cut it once and raise the pace once; each quiet
/// interval adds one back, up to the configured limit and no further, and
/// the first one ends the pace.
#[tokio::test(start_paused = true)]
async fn a_429_halves_the_limit_once_per_window_and_quiet_time_restores_it() {
    const HOST: &str = "cut.backoff.test";
    let host = Scripted::new(Duration::from_secs(1));
    let (store, path) = limited(&format!("http://{HOST}/meps.nc"), 8, &host);
    assert_eq!(peak(&host, &store, &path, 16).await, 8);

    for _ in 0..4 {
        host.throttle(None);
    }
    heads(&store, &path, 16).await;
    let throttled = host
        .sent()
        .iter()
        .filter(|(_, status)| *status == 429)
        .count();
    assert_eq!(throttled, 4);
    assert_eq!(requests(HOST), (4, 0, 0), "four 429s at once cut once");
    assert_eq!(pace(HOST), PACE_FLOOR, "and raise the pace once");
    // Requests of 1 s starting 0.25 s apart: four at once, the limit.
    assert_eq!(peak(&host, &store, &path, 16).await, 4);

    tokio::time::sleep(QUIET_INTERVAL).await;
    assert_eq!(requests(HOST).0, 5);
    assert_eq!(pace(HOST), Duration::ZERO);
    tokio::time::sleep(QUIET_INTERVAL * 5).await;
    assert_eq!(requests(HOST).0, 8);
    tokio::time::sleep(QUIET_INTERVAL * 5).await;
    assert_eq!(requests(HOST).0, 8, "never past the configured limit");
    assert_eq!(peak(&host, &store, &path, 16).await, 8);
}

/// After a 429 every store on the host, limited or not, starts its
/// requests the pace apart (#1290). Each 429 in a later window doubles the
/// pace, up to [`PACE_CAP`]; each quiet interval halves it, and under
/// [`PACE_FLOOR`] it is off: requests go out at once again.
#[tokio::test(start_paused = true)]
async fn a_429_paces_every_store_on_the_host_until_it_is_quiet() {
    const HOST: &str = "pace.backoff.test";
    let host = Scripted::new(Duration::ZERO);
    let (sfc, path) = limited(&format!("http://{HOST}/sfc/meps.nc"), 8, &host);
    let (feed, _) = unlimited(&format!("http://{HOST}/feed.xml"), &host);
    host.throttle(Some("0"));
    sfc.inner().head(&path).await.unwrap();
    assert_eq!(pace(HOST), PACE_FLOOR);
    let from = host.sent().len();
    heads(&feed, &path, 3).await;
    let quarter = PACE_FLOOR;
    assert_eq!(spacing(&host, from), [Duration::ZERO, quarter, quarter * 2]);

    let mut paces = Vec::new();
    for _ in 0..5 {
        tokio::time::sleep(CUT_WINDOW).await;
        host.throttle(Some("0"));
        sfc.inner().head(&path).await.unwrap();
        paces.push(pace(HOST));
    }
    let ms = Duration::from_millis;
    assert_eq!(paces, [ms(500), ms(1000), PACE_CAP, PACE_CAP, PACE_CAP]);
    let from = host.sent().len();
    heads(&feed, &path, 3).await;
    assert_eq!(
        spacing(&host, from),
        [Duration::ZERO, PACE_CAP, PACE_CAP * 2]
    );

    let mut paces = Vec::new();
    for _ in 0..4 {
        tokio::time::sleep(QUIET_INTERVAL).await;
        paces.push(pace(HOST));
    }
    assert_eq!(paces, [ms(1000), ms(500), ms(250), Duration::ZERO]);
    let from = host.sent().len();
    heads(&feed, &path, 3).await;
    assert_eq!(spacing(&host, from), [Duration::ZERO; 3], "at once again");
    assert_eq!(throttled_answers(&format!("http://{HOST}/any/path")), 6);
}

/// A request refused with a 429 without `Retry-After`, as thredds.met.no
/// refuses, is retried after [`RETRY_DELAY`] and object_store's own
/// backoff, not after the backoff alone (#1290); one carrying
/// `Retry-After` when it says.
#[tokio::test(start_paused = true)]
async fn a_bare_429_is_retried_after_the_retry_delay() {
    let host = Scripted::new(Duration::ZERO);
    let (store, path) = limited("http://retry.backoff.test/x", 8, &host);
    host.throttle(None);
    store.inner().head(&path).await.unwrap();
    let sent = host.sent();
    let [(refused, 429), (retried, 200)] = sent.as_slice() else {
        panic!("a 429 and its retry: {sent:?}");
    };
    assert_eq!(*retried - *refused, RETRY_DELAY + FIRST_BACKOFF);

    tokio::time::sleep(QUIET_INTERVAL * 4).await;
    host.throttle(Some("1"));
    store.inner().head(&path).await.unwrap();
    let sent = host.sent();
    let [.., (refused, 429), (retried, 200)] = sent.as_slice() else {
        panic!("a 429 and its retry: {sent:?}");
    };
    assert_eq!(*retried - *refused, Duration::from_secs(1));
}

/// `throttled_answers` and `host_requests` count each 429 of a host once,
/// whichever store on it was refused: two bases of one host, which differ
/// in their query strings, share a throttle.
#[tokio::test(start_paused = true)]
async fn the_429s_of_a_host_are_counted_once() {
    const HOST: &str = "count.backoff.test";
    let host = Scripted::new(Duration::ZERO);
    let (sfc, path) = limited(&format!("http://{HOST}/sfc/meps.nc?a=1"), 8, &host);
    let (pl, _) = limited(&format!("http://{HOST}/pl/meps.nc?b=2"), 8, &host);
    let (feed, _) = unlimited(&format!("http://{HOST}/feed.xml"), &host);
    let throttled = || {
        let all = crate::requests_to(|label| label == HOST);
        let [host] = all.as_slice() else {
            panic!("one entry for {HOST}: {all:?}");
        };
        host.throttled
    };
    assert_eq!(throttled(), 0);
    for store in [&sfc, &pl, &feed] {
        host.throttle(Some("0"));
        store.inner().head(&path).await.unwrap();
    }
    assert_eq!(throttled(), 3);
    assert_eq!(throttled_answers(&format!("http://{HOST}/")), 3);
    assert_eq!(throttled_answers("http://unseen.backoff.test/"), 0);
    assert_eq!(throttled_answers("not a url"), 0);
}

/// `Retry-After: 2` holds every request to the host for 2 s: the retry of
/// the throttled one, another collection's on the host's limited store,
/// and an unlimited store's. Then they go out the pace apart, not at once.
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
        [
            (start, 429),
            (resumed, 200),
            (resumed + PACE_FLOOR, 200),
            (resumed + PACE_FLOOR * 2, 200)
        ]
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
/// [`MAX_RETRY_AFTER`]; a date already past pauses nothing: the retry
/// waits only for the pace, which the minutes between these 429s took back
/// to the floor.
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
    assert_eq!(past, PACE_FLOOR);
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
        crate::within(budget, deadline, None, async move {
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

/// A host that never answers 429 keeps its limit, has no pace, and logs
/// nothing: its requests wait for nothing (#1290).
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
    assert_eq!(pace(HOST), Duration::ZERO);
    assert_eq!(throttled_answers(&format!("http://{HOST}/")), 0);
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
/// the configured limit, the pace and the `Retry-After`; the pace's end
/// and the limit's return to the configured one log one INFO each.
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
    assert_eq!(warn["pace_secs"], 0.25);
    assert_eq!(warn["retry_after_secs"], 2.0);
    assert_eq!(
        warn["message"],
        format!(
            "{HOST} answered 429 Too Many Requests: request limit cut to 4 of 8; \
             requests start 250 ms apart; new requests wait 2.0 s (Retry-After)"
        )
    );
    assert_eq!(warn["span"]["collection"], "meps-sfc", "{warn:#}");

    tokio::time::sleep(WARN_INTERVAL).await;
    assert_eq!(requests(HOST).0, 4, "two quiet intervals");
    assert!(at_level(&log, "INFO").is_empty());
    host.throttle(None);
    store.inner().head(&path).await.unwrap();
    let infos = at_level(&log, "INFO");
    assert_eq!(infos.len(), 1, "{infos:#?}");
    assert_eq!(
        infos[0]["message"],
        format!("{HOST} has answered no 429 for 60 s: its requests are no longer paced")
    );
    let warns = at_level(&log, "WARN");
    assert_eq!(warns.len(), 2, "{warns:#?}");
    assert_eq!(
        warns[1]["message"],
        format!(
            "{HOST} answered 429 Too Many Requests: request limit cut to 2 of 8; \
             requests start 250 ms apart"
        )
    );
    assert!(warns[1].get("retry_after_secs").is_none(), "{:#}", warns[1]);

    tokio::time::sleep(QUIET_INTERVAL * 6).await;
    assert_eq!(requests(HOST).0, 8);
    assert_eq!(requests(HOST).0, 8);
    let infos = at_level(&log, "INFO");
    assert_eq!(infos.len(), 2, "{infos:#?}");
    assert_eq!(
        infos[1]["message"],
        format!("{HOST} has answered no 429 for 30 s: its request limit is back at 8")
    );
}

/// A bare 429 to an unlimited store paces the host and holds the refused
/// request's retry: the request asked for with it, not sent yet when the
/// 429 came back, waits its turn.
#[tokio::test(start_paused = true)]
async fn a_bare_429_on_an_unlimited_host_paces_it() {
    let log = JsonLog::default();
    let _logging = tracing::subscriber::set_default(log.subscriber());
    let host = Scripted::new(Duration::ZERO);
    let (store, path) = unlimited("http://bare.backoff.test/x", &host);
    host.throttle(None);
    let (first, second) = tokio::join!(store.inner().head(&path), store.inner().head(&path));
    first.unwrap();
    second.unwrap();
    assert_eq!(
        spacing(&host, 0),
        [Duration::ZERO, PACE_FLOOR, RETRY_DELAY + FIRST_BACKOFF]
    );
    assert_eq!(pace("bare.backoff.test"), PACE_FLOOR);
    let warns = at_level(&log, "WARN");
    assert_eq!(warns.len(), 1, "{warns:#?}");
    assert_eq!(
        warns[0]["message"],
        "bare.backoff.test answered 429 Too Many Requests: requests start 250 ms apart"
    );
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

/// A document (#1170) answered without `Content-Length`, as THREDDS streams
/// its catalogs, is read whole through the host's own transport, where
/// object_store's GET fails it, and capped while it streams. (Its 429s:
/// `a_429_on_a_catalog_poll_backs_off_the_host`.)
#[tokio::test(start_paused = true)]
async fn a_chunked_document_is_read_through_the_hosts_transport() {
    const HOST: &str = "document.backoff.test";
    const CATALOG: &[u8] = b"<catalog/>";
    let host = Scripted::new(Duration::ZERO);
    host.chunked(CATALOG);
    let (store, path) = limited(&format!("http://{HOST}/thredds/catalog.xml"), 8, &host);

    let error = store.inner().get(&path).await.unwrap_err().to_string();
    assert!(error.contains("Content-Length"), "{error}");
    let request = crate::DocumentRequest { path, query: None };
    let read = store.document(&request, 1024).await.unwrap().unwrap();
    assert_eq!(read.bytes, CATALOG);
    assert_eq!(host.sent().len(), 2, "one request each");
    assert_eq!(requests(HOST), (8, 0, 0), "and the permit given back");

    let error = store.document(&request, 5).await.unwrap_err().to_string();
    assert!(error.contains("over the 5-byte limit"), "{error}");
}

/// The catalog route's own fetch (#1170): a [`crate::discovery::CatalogProber`]
/// poll whose catalog the host first answers 429 with `Retry-After: 1`
/// backs off the host, cutting its limit and holding the retry for the
/// second, then reads the chunked catalog and lists its file. The retry is
/// logged in the `storage` span naming the collection (#1233). Real time:
/// the poll bridges to a runtime of its own, as off the poll runtime. The
/// only test that retries a document, so no other registers the retry's
/// log line while this one listens.
#[test]
fn a_429_on_a_catalog_poll_backs_off_the_host() {
    use crate::discovery::{CatalogProber, Schedule, TemplateSpec, TimeWindow};

    const HOST: &str = "catalog.backoff.test";
    // A catalog listing one file of the 06Z run, 10 bytes ([`BODY`]): a
    // literal, never `format!` (Critical Rule 3).
    const CATALOG: &[u8] = br#"<catalog><service name="files" serviceType="HTTPServer" base="/files/"/><dataset name="run_20261019T06Z.nc" urlPath="run_20261019T06Z.nc"><dataSize units="bytes">10</dataSize><date type="modified">2026-10-19T07:00:00Z</date></dataset></catalog>"#;
    let log = JsonLog::default();
    let _logging = tracing::subscriber::set_default(log.subscriber());
    let host = Scripted::new(Duration::ZERO);
    host.chunked(CATALOG);
    let (catalogs, _) = limited(&format!("http://{HOST}/catalog.xml"), 8, &host);
    let catalogs = catalogs.for_collection("meps-sfc");
    let (files, base) = limited(&format!("http://{HOST}/files/"), 8, &host);
    let spec = TemplateSpec {
        time_window: Some(TimeWindow::parse("-PT6H").unwrap()),
        ..TemplateSpec::new(
            base.as_ref(),
            "run_%Y%m%dT{run}Z.nc",
            Schedule::Runs { run_hours: vec![6] },
            "catalog-backoff",
        )
    };
    let mut prober = CatalogProber::new(
        spec,
        &format!("http://{HOST}/catalog.xml"),
        &format!("http://{HOST}/files/"),
    )
    .unwrap();

    host.throttle(Some("1"));
    let now = "2026-10-19T08:00:00Z".parse().unwrap();
    let report = prober.poll(&catalogs, &files, now).unwrap();
    assert_eq!((report.catalogs, report.catalogs_failed), (1, 0));
    let sent = host.sent();
    let [(throttled, 429), (retried, 200), (_, 200)] = sent.as_slice() else {
        panic!("a 429, the catalog's retry and the file's HEAD: {sent:?}");
    };
    assert!(*retried - *throttled >= Duration::from_secs(1), "{sent:?}");
    assert_eq!(requests(HOST).0, 4, "the 429 cut the host's limit");
    let retries: Vec<serde_json::Value> = log
        .lines()
        .into_iter()
        .filter(|line| line["target"] == "ds_storage::document")
        .collect();
    assert_eq!(retries.len(), 1, "{retries:#?}");
    assert_eq!(retries[0]["span"]["collection"], "meps-sfc");
    assert_eq!(retries[0]["span"]["path"], "catalog.xml");
    // The wait was the host's back-off, not the layer's 100 ms.
    let message = retries[0]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("after the host's 429 back-off (attempt 1 of 3)"),
        "{message}"
    );
    let listed = prober.files();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].key, "files/run_20261019T06Z.nc");
    assert_eq!(listed[0].version.size, BODY.len() as u64);
}

/// The S3 store [`crate::build_s3_store_from_parts`] builds for `bucket`
/// at `http://{endpoint}` (#1530), retrying as [`crate::S3_RETRY`] says,
/// on `host`: under the bucket's throttle, limit and gauges.
fn s3(endpoint: &str, bucket: &str, host: &Scripted) -> DataStore {
    let url = format!("http://{endpoint}");
    let label = crate::traced::s3_label(Some(&url), None, bucket);
    crate::bucket::s3_store(&label, bucket, |throttle| {
        let transport = super::Throttled {
            inner: crate::retryable::Counted(host.clone()),
            host: throttle.clone(),
        };
        let store = object_store::aws::AmazonS3Builder::new()
            .with_bucket_name(bucket)
            .with_region("auto")
            .with_endpoint(&url)
            .with_allow_http(true)
            .with_skip_signature(true)
            .with_retry(crate::S3_RETRY)
            .with_http_connector(crate::NoRedirectConnector(transport))
            .build()
            .unwrap();
        Ok(Arc::new(store))
    })
    .unwrap()
}

/// The pace of the S3 bucket `name` labelled `label` now.
fn bucket_pace(label: &str, name: &str) -> Duration {
    crate::bucket::bucket(label, name).throttle.pace()
}

/// S3's first back-off, [`crate::S3_RETRY`]'s `init_backoff`.
const S3_FIRST_BACKOFF: Duration = Duration::from_millis(500);

/// A `503 SlowDown` backs the bucket off as a 429 backs off an HTTP host
/// (#1530): the refused GET is retried after the retry delay and S3's own
/// first back-off, the bucket's limit halves from [`crate::bucket::S3_REQUEST_LIMIT`]
/// and every store on the bucket, another collection's too, starts its
/// requests the pace apart. `back_off` reports it on each, and
/// `host_requests` under the bucket's label. Another bucket on the same
/// endpoint is not backed off: S3 throttles a bucket.
#[tokio::test(start_paused = true)]
async fn a_slowdown_backs_off_every_store_on_the_bucket_and_no_other() {
    const ENDPOINT: &str = "slowdown.s3.test";
    const LABEL: &str = "slowdown.s3.test/forecasts";
    let host = Scripted::new(Duration::ZERO);
    let ifs = s3(ENDPOINT, "forecasts", &host).for_collection("ifs");
    let aifs = s3(ENDPOINT, "forecasts", &host).for_collection("aifs");
    let other = s3(ENDPOINT, "other", &host);
    let path = Path::from("20261122/00z/ifs/0h.index");
    let calm = Some(crate::HostBackOff {
        throttled: 0,
        backing_off: false,
    });
    assert_eq!(ifs.back_off(), calm);

    host.answer(503, None, Duration::ZERO);
    let got = ifs.inner().get(&path).await.unwrap();
    assert_eq!(got.bytes().await.unwrap(), BODY);
    let sent = host.sent();
    let [(refused, 503), (retried, 200)] = sent.as_slice() else {
        panic!("a SlowDown and its retry: {sent:?}");
    };
    assert_eq!(*retried - *refused, RETRY_DELAY + S3_FIRST_BACKOFF);
    let throttled = Some(crate::HostBackOff {
        throttled: 1,
        backing_off: true,
    });
    assert_eq!(ifs.back_off(), throttled);
    assert_eq!(aifs.back_off(), throttled, "one back-off for the bucket");
    assert_eq!(bucket_pace(LABEL, "forecasts"), PACE_FLOOR);
    assert_eq!(other.back_off(), calm, "and none for another bucket");
    let limit = crate::bucket::S3_REQUEST_LIMIT;
    assert_eq!(requests(LABEL), (limit / 2, 0, 0));
    let all = crate::requests_to(|label| label == LABEL);
    assert_eq!(all[0].throttled, 1);

    let from = host.sent().len();
    heads(&aifs, &path, 3).await;
    assert_eq!(
        spacing(&host, from),
        [Duration::ZERO, PACE_FLOOR, PACE_FLOOR * 2],
        "the other collection's requests are paced"
    );
    let from = host.sent().len();
    heads(&other, &path, 3).await;
    assert_eq!(
        spacing(&host, from),
        [Duration::ZERO; 3],
        "another bucket's are not"
    );

    tokio::time::sleep(QUIET_INTERVAL).await;
    assert_eq!(
        ifs.back_off(),
        Some(crate::HostBackOff {
            throttled: 1,
            backing_off: false,
        })
    );
    assert_eq!(requests(LABEL).0, limit / 2 + 1, "and the limit grows back");
}

/// A bucket that keeps answering `SlowDown` gets 5 attempts of a GET, each
/// retry after the retry delay, S3's back-off and the bucket's pace, and
/// the read is `Unavailable` within the request budget, so each API
/// answers its 503 (#1355) rather than timing out. Its pace and limit are
/// cut as the answers come.
#[tokio::test(start_paused = true)]
async fn a_bucket_that_keeps_refusing_gets_five_attempts_the_back_off_apart() {
    const LABEL: &str = "refusing.s3.test/forecasts";
    let host = Scripted::new(Duration::ZERO);
    let store = s3("refusing.s3.test", "forecasts", &host);
    for _ in 0..20 {
        host.answer(503, None, Duration::ZERO);
    }
    let start = Instant::now();
    let error = store
        .inner()
        .get(&Path::from("index"))
        .await
        .map(drop)
        .unwrap_err();
    assert!(
        matches!(StorageError::from(error), StorageError::Unavailable(_)),
        "a read that outlasts its retries is unavailable"
    );
    let sent = host.sent();
    assert_eq!(sent.len(), 1 + crate::S3_RETRY.max_retries, "{sent:?}");
    for pair in sent.windows(2) {
        assert!(
            pair[1].0 - pair[0].0 >= RETRY_DELAY + S3_FIRST_BACKOFF,
            "{sent:?}"
        );
    }
    let spent = start.elapsed();
    assert!(spent < FetchBudget::Request.timeout(), "{spent:?}");
    assert_eq!(bucket_pace(LABEL, "forecasts"), PACE_CAP);
    assert_eq!(requests(LABEL).0, crate::bucket::S3_REQUEST_LIMIT / 32);
}

/// A 500 from a bucket is not a throttle: it is retried on S3's back-off
/// alone, and the bucket neither paces nor cuts its limit.
#[tokio::test(start_paused = true)]
async fn a_bucket_error_that_is_not_a_slowdown_backs_nothing_off() {
    const LABEL: &str = "error.s3.test/forecasts";
    let host = Scripted::new(Duration::ZERO);
    let store = s3("error.s3.test", "forecasts", &host);
    host.answer(500, None, Duration::ZERO);
    store.inner().head(&Path::from("index")).await.unwrap();
    let sent = host.sent();
    let [(refused, 500), (retried, 200)] = sent.as_slice() else {
        panic!("a 500 and its retry: {sent:?}");
    };
    assert_eq!(*retried - *refused, S3_FIRST_BACKOFF);
    assert_eq!(bucket_pace(LABEL, "forecasts"), Duration::ZERO);
    let limit = crate::bucket::S3_REQUEST_LIMIT;
    assert_eq!(requests(LABEL), (limit, 0, 0));
    assert_eq!(
        store.back_off(),
        Some(crate::HostBackOff {
            throttled: 0,
            backing_off: false
        })
    );
}

/// A bucket's `SlowDown` carrying `Retry-After` pauses its new requests
/// that long, as a host's 429 does.
#[tokio::test(start_paused = true)]
async fn a_slowdown_with_retry_after_pauses_the_bucket() {
    let host = Scripted::new(Duration::ZERO);
    let store = s3("pause.s3.test", "forecasts", &host);
    host.answer(503, Some("3"), Duration::ZERO);
    store.inner().head(&Path::from("index")).await.unwrap();
    let sent = host.sent();
    let [(refused, 503), (retried, 200)] = sent.as_slice() else {
        panic!("a SlowDown and its retry: {sent:?}");
    };
    assert_eq!(*retried - *refused, Duration::from_secs(3));
}

/// Only a request naming the bucket, by path or by host, goes through its
/// throttle: a credential endpoint's, which object_store sends through the
/// same connector, passes by. A bucket counts a 503 and a 429; an HTTP host
/// only a 429.
#[test]
fn a_bucket_governs_the_requests_that_name_it() {
    let bucket = super::Kind::Bucket("forecasts".into());
    let governs = |uri: &str| bucket.governs(&uri.parse().unwrap());
    assert!(governs(
        "https://s3.eu-central-1.amazonaws.com/forecasts/20261122/a.index"
    ));
    assert!(governs(
        "https://s3.eu-central-1.amazonaws.com/forecasts?list-type=2"
    ));
    assert!(governs(
        "https://forecasts.s3.eu-central-1.amazonaws.com/20261122/a.index"
    ));
    assert!(!governs(
        "https://s3.eu-central-1.amazonaws.com/forecasts-archive/a"
    ));
    assert!(!governs("http://169.254.169.254/latest/api/token"));
    assert!(!governs("https://sts.eu-central-1.amazonaws.com/"));
    assert!(super::Kind::Http.governs(&"http://any.test/x".parse().unwrap()));

    assert!(bucket.refuses(StatusCode::SERVICE_UNAVAILABLE));
    assert!(bucket.refuses(StatusCode::TOO_MANY_REQUESTS));
    assert!(!bucket.refuses(StatusCode::INTERNAL_SERVER_ERROR));
    assert!(!super::Kind::Http.refuses(StatusCode::SERVICE_UNAVAILABLE));
}

/// A `SlowDown` logs the bucket's WARN, naming the bucket and the answer,
/// and its return logs the INFO lines naming `SlowDown`.
#[tokio::test(start_paused = true)]
async fn a_slowdown_warns_naming_the_bucket() {
    const LABEL: &str = "log.s3.test/forecasts";
    let log = JsonLog::default();
    let _logging = tracing::subscriber::set_default(log.subscriber());
    let host = Scripted::new(Duration::ZERO);
    let store = s3("log.s3.test", "forecasts", &host).for_collection("ifs");
    host.answer(503, None, Duration::ZERO);
    store.inner().head(&Path::from("index")).await.unwrap();
    let warns = at_level(&log, "WARN");
    assert_eq!(warns.len(), 1, "{warns:#?}");
    assert_eq!(warns[0]["host"], LABEL);
    assert_eq!(
        warns[0]["message"],
        format!(
            "{LABEL} answered 503 SlowDown: request limit cut to 32 of 64; \
             requests start 250 ms apart"
        )
    );
    assert_eq!(warns[0]["span"]["collection"], "ifs");
    tokio::time::sleep(QUIET_INTERVAL * 40).await;
    assert_eq!(store.back_off().map(|b| b.backing_off), Some(false));
    assert_eq!(requests(LABEL).0, 64);
    let infos: Vec<_> = at_level(&log, "INFO")
        .into_iter()
        .map(|line| line["message"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        infos,
        [
            format!("{LABEL} has answered no SlowDown for 30 s: its requests are no longer paced"),
            format!("{LABEL} has answered no SlowDown for 30 s: its request limit is back at 64"),
        ]
    );
}

/// An HTTP store reports its host's back-off through `back_off`, and so do
/// the stores `for_collection` and `with_request_limit` make of it; a
/// store built elsewhere and a local one report none.
#[tokio::test(start_paused = true)]
async fn an_http_store_reports_its_hosts_back_off() {
    let host = Scripted::new(Duration::ZERO);
    let (store, path) = limited("http://report.backoff.test/x", 8, &host);
    let named = store.clone().for_collection("meps").with_request_limit(2);
    host.throttle(None);
    store.inner().head(&path).await.unwrap();
    let throttled = Some(crate::HostBackOff {
        throttled: 1,
        backing_off: true,
    });
    assert_eq!(store.back_off(), throttled);
    assert_eq!(named.back_off(), throttled);
    let elsewhere = DataStore::remote(Arc::new(object_store::memory::InMemory::new()), "x.test");
    assert_eq!(elsewhere.back_off(), None);
    let local = DataStore::new(Arc::new(object_store::memory::InMemory::new()));
    assert_eq!(local.back_off(), None);
    tokio::time::sleep(QUIET_INTERVAL).await;
    assert_eq!(named.back_off().map(|b| b.backing_off), Some(false));
}

/// Over a real server: a bucket's `503 Slow Down` and then a 200 serve the
/// read, cut the bucket's limit and count once for it.
#[tokio::test(flavor = "multi_thread")]
async fn a_slowdown_over_http_backs_off_the_bucket() {
    let modified = "2026-11-22T06:00:00Z".parse().unwrap();
    let server = RangeServer::new(BODY, modified);
    {
        let mut object = server.object.lock().unwrap();
        object.path = "/forecasts/object".into();
        object.fail_next.push_back("503 Slow Down");
    }
    let _quick = crate::test_util::quick_bucket_backoff(&server.url, "forecasts");
    let store =
        crate::s3_store_from_parts(&server.url, "forecasts", crate::test_store::QUICK_S3_RETRY)
            .unwrap();
    assert_eq!(
        store.get_range(&Path::from("object"), 2..5).unwrap(),
        &b"234"[..]
    );
    let label = format!("{}/forecasts", server.url.trim_start_matches("http://"));
    let all = crate::requests_to(|host| host == label);
    let [bucket] = all.as_slice() else {
        panic!("one entry for {label}: {all:?}");
    };
    let limit = crate::bucket::S3_REQUEST_LIMIT;
    assert_eq!((bucket.limit, bucket.throttled), (limit / 2, 1));
    assert_eq!(store.back_off().map(|b| b.throttled), Some(1));
}
