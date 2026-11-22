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
/// refuses, is retried [`RETRY_DELAY`] after the refusal, not after
/// object_store's 0.1 s backoff (#1290); one carrying `Retry-After` when
/// it says.
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
    assert_eq!(*retried - *refused, RETRY_DELAY);

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
    assert_eq!(spacing(&host, 0), [Duration::ZERO, PACE_FLOOR, RETRY_DELAY]);
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
    crate::bucket::s3_store(&label, bucket, "", |throttle: &Arc<super::HostThrottle>| {
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
    crate::bucket::bucket(label, name, "").throttle.pace()
}

/// S3's first back-off, [`crate::S3_RETRY`]'s `init_backoff`.
const S3_FIRST_BACKOFF: Duration = Duration::from_millis(500);

/// The limit a bucket starts at.
const INITIAL: usize = crate::bucket::S3_INITIAL_LIMIT;

/// A stray `SlowDown`, which S3 answers under ordinary load, is counted and
/// retried on S3's own back-off, as before #1530: no pace, no cut, no
/// retry delay, and the bucket does not report itself backing off.
#[tokio::test(start_paused = true)]
async fn a_stray_slowdown_is_only_counted() {
    const LABEL: &str = "stray.s3.test/radar";
    let host = Scripted::new(Duration::ZERO);
    let store = s3("stray.s3.test", "radar", &host);
    let path = Path::from("composite.tif");
    host.answer(503, None, Duration::ZERO);
    store.inner().head(&path).await.unwrap();
    let sent = host.sent();
    let [(refused, 503), (retried, 200)] = sent.as_slice() else {
        panic!("a SlowDown and its retry: {sent:?}");
    };
    assert_eq!(*retried - *refused, S3_FIRST_BACKOFF);
    let from = host.sent().len();
    heads(&store, &path, 3).await;
    assert_eq!(spacing(&host, from), [Duration::ZERO; 3], "not paced");
    assert_eq!(requests(LABEL), (INITIAL, 0, 0), "nor cut");
    assert_eq!(
        store.back_off(),
        Some(crate::HostBackOff {
            throttled: 1,
            backing_off: false,
        })
    );
}

/// A bucket that throttles, three `SlowDown`s within 10 s, backs off as a
/// 429 backs off an HTTP host (#1530): the refused GET is retried 2 s after
/// the refusal that made it throttle, the bucket's limit halves and every
/// store on it, another collection's too, starts its requests the pace
/// apart. `back_off` reports it on each, and `host_requests` under the
/// bucket's label. Another bucket on the same endpoint is not backed off:
/// S3 throttles a bucket.
#[tokio::test(start_paused = true)]
async fn a_throttling_bucket_backs_off_every_store_on_it_and_no_other() {
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

    for _ in 0..super::BUCKET_TRIGGER {
        host.answer(503, None, Duration::ZERO);
    }
    let got = ifs.inner().get(&path).await.unwrap();
    assert_eq!(got.bytes().await.unwrap(), BODY);
    let sent = host.sent();
    let [(first, 503), (second, 503), (third, 503), (retried, 200)] = sent.as_slice() else {
        panic!("three SlowDowns and the retry: {sent:?}");
    };
    assert_eq!(*second - *first, S3_FIRST_BACKOFF, "stray: S3's back-off");
    assert!(*third - *second < Duration::from_secs(1), "{sent:?}");
    assert_eq!(
        *retried - *third,
        RETRY_DELAY,
        "throttling: the retry delay"
    );
    let throttled = Some(crate::HostBackOff {
        throttled: 3,
        backing_off: true,
    });
    assert_eq!(ifs.back_off(), throttled);
    assert_eq!(aifs.back_off(), throttled, "one back-off for the bucket");
    assert_eq!(bucket_pace(LABEL, "forecasts"), PACE_FLOOR);
    assert_eq!(other.back_off(), calm, "and none for another bucket");
    assert_eq!(requests(LABEL), (INITIAL / 2, 0, 0));
    let all = crate::requests_to(|label| label == LABEL);
    assert_eq!(all[0].throttled, 3);

    let from = host.sent().len();
    heads(&aifs, &path, 3).await;
    let spaced = spacing(&host, from);
    assert_eq!(
        spaced[1] - spaced[0],
        PACE_FLOOR,
        "the other collection is paced"
    );
    assert_eq!(spaced[2] - spaced[1], PACE_FLOOR);
    let from = host.sent().len();
    heads(&other, &path, 3).await;
    assert_eq!(
        spacing(&host, from),
        [Duration::ZERO; 3],
        "another bucket is not"
    );

    tokio::time::sleep(QUIET_INTERVAL).await;
    assert_eq!(
        ifs.back_off(),
        Some(crate::HostBackOff {
            throttled: 3,
            backing_off: false,
        })
    );
}

/// A bucket that keeps answering `SlowDown` gets 7 attempts of a GET,
/// those after it throttles the retry delay apart, and the read is
/// `Unavailable` within the request budget, at its last refusal, so each
/// API answers its 503 (#1355) rather than timing out. Its pace and limit
/// are cut as the answers come.
#[tokio::test(start_paused = true)]
async fn a_bucket_that_keeps_refusing_gets_seven_attempts_and_fails_at_the_last() {
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
    for pair in sent[super::BUCKET_TRIGGER - 1..].windows(2) {
        assert!(pair[1].0 - pair[0].0 >= RETRY_DELAY, "{sent:?}");
    }
    let last = sent.last().unwrap().0;
    assert_eq!(
        Instant::now(),
        last,
        "no retry delay after the last attempt"
    );
    assert!(last - start < FetchBudget::Request.timeout(), "{sent:?}");
    assert_eq!(bucket_pace(LABEL, "forecasts"), PACE_CAP);
    assert_eq!(requests(LABEL).0, 1);
}

/// A 500 from a bucket is not a throttle: it is retried on S3's back-off
/// alone, and the bucket neither paces nor cuts its limit.
#[tokio::test(start_paused = true)]
async fn a_bucket_error_that_is_not_a_slowdown_backs_nothing_off() {
    const LABEL: &str = "error.s3.test/forecasts";
    let host = Scripted::new(Duration::ZERO);
    let store = s3("error.s3.test", "forecasts", &host);
    for _ in 0..4 {
        host.answer(500, None, Duration::ZERO);
    }
    store.inner().head(&Path::from("index")).await.unwrap();
    let sent = host.sent();
    assert_eq!(sent.len(), 5, "{sent:?}");
    assert_eq!(sent[1].0 - sent[0].0, S3_FIRST_BACKOFF);
    assert_eq!(bucket_pace(LABEL, "forecasts"), Duration::ZERO);
    assert_eq!(requests(LABEL), (INITIAL, 0, 0));
    assert_eq!(
        store.back_off(),
        Some(crate::HostBackOff {
            throttled: 0,
            backing_off: false
        })
    );
}

/// A bucket's `SlowDown` carrying `Retry-After` pauses its new requests
/// that long, as a host's 429 does, the first one already.
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

/// Make the bucket of `store` on `host` throttle: three `SlowDown`s to one
/// HEAD, served at its retry. Returns when the retry was sent.
async fn throttle_bucket(host: &Scripted, store: &DataStore, path: &Path) -> Instant {
    for _ in 0..super::BUCKET_TRIGGER {
        host.answer(503, None, Duration::ZERO);
    }
    store.inner().head(path).await.unwrap();
    host.sent().last().unwrap().0
}

/// A read that gives up its wait for a turn under the pace, at its
/// deadline, holds no turn (#1530): with a render's reads dropped at their
/// deadline, the next read waits one pace, not one for each of them. Only
/// the reads that had their turn were sent.
#[tokio::test(start_paused = true)]
async fn a_read_dropped_at_its_deadline_holds_no_turn() {
    let host = Scripted::new(Duration::ZERO);
    let store = s3("dropped.s3.test", "radar", &host);
    let path = Path::from("composite.tif");
    let at = throttle_bucket(&host, &store, &path).await;
    assert_eq!(Instant::now(), at);
    let sent_before = host.sent().len();
    let deadline = (at + Duration::from_millis(900)).into_std();
    let render = futures::future::join_all((0..20).map(|_| {
        let (store, path) = (&store, &path);
        crate::within(FetchBudget::Request, Some(deadline), None, async move {
            store
                .inner()
                .head(path)
                .await
                .map_err(|e| DataServerError::from(StorageError::from(e)))
        })
    }))
    .await;
    let served = render.iter().filter(|read| read.is_ok()).count();
    let late = render
        .iter()
        .filter(|read| matches!(read, Err(DataServerError::DeadlineExceeded)))
        .count();
    assert_eq!((served, late), (3, 17), "turns at 0.25, 0.5 and 0.75 s");
    assert_eq!(host.sent().len(), sent_before + 3);
    store.inner().head(&path).await.unwrap();
    let next = host.sent().last().unwrap().0;
    assert_eq!(next - at, PACE_FLOOR * 4, "one pace after the last sent");
}

/// `n` reads of `path` through `store` in a loop each, one after another,
/// until `end`.
async fn read_until(store: &DataStore, path: &Path, n: usize, end: Instant) {
    futures::future::join_all((0..n).map(|_| async move {
        while Instant::now() < end {
            store.inner().head(path).await.unwrap();
        }
    }))
    .await;
}

/// One read of `path` through `store` under `budget`, `after` from now:
/// when it was asked for and when it was served.
async fn timed_read(
    store: &DataStore,
    path: &Path,
    budget: FetchBudget,
    after: Duration,
) -> (Instant, Instant) {
    tokio::time::sleep(after).await;
    let asked = Instant::now();
    crate::within(budget, None, None, async {
        store
            .inner()
            .head(path)
            .await
            .map_err(|e| DataServerError::from(StorageError::from(e)))
    })
    .await
    .unwrap();
    (asked, Instant::now())
}

/// A read under the background budget that a request waits on, as
/// engine-netcdf's detached field fill, is no background work: under six
/// request loops on a host limited to 2, with no 429 at all, it queues
/// first come, first served and is served within a few reads (#1530).
#[tokio::test(start_paused = true)]
async fn a_background_budget_read_a_request_waits_on_queues_in_turn() {
    let host = Scripted::new(Duration::from_millis(100));
    let (store, path) = limited("http://detached.backoff.test/meps.nc", 2, &host);
    let end = Instant::now() + Duration::from_secs(4);
    let (_, (asked, served)) = tokio::join!(
        read_until(&store, &path, 6, end),
        timed_read(
            &store,
            &path,
            FetchBudget::Background,
            Duration::from_millis(550)
        ),
    );
    let waited = served - asked;
    assert!(waited <= Duration::from_millis(500), "{waited:?}");
}

/// Background work is served under sustained request load: a bucket
/// backing off, its limit cut, four request loops for 20 s, and a scan's
/// read still has its turn within a few paces (#1530).
#[tokio::test(start_paused = true)]
async fn background_work_is_served_under_sustained_request_load() {
    let host = Scripted::new(Duration::from_millis(20));
    let store = s3("busy.s3.test", "goes", &host);
    let path = Path::from("scan.nc");
    throttle_bucket(&host, &store, &path).await;
    let scan = store.clone().for_background();
    let end = Instant::now() + Duration::from_secs(20);
    let (_, (asked, served)) = tokio::join!(
        read_until(&store, &path, 4, end),
        timed_read(
            &scan,
            &path,
            FetchBudget::Background,
            Duration::from_secs(1)
        ),
    );
    let waited = served - asked;
    assert!(waited <= PACE_FLOOR * 6, "{waited:?}");
}

/// Background work holds at most half the limit: a satellite scan's
/// sixteen reads of a bucket starting at 16 hold eight permits, and a
/// request read arriving after them has one at once (#1530). The scan's
/// other reads go after.
#[tokio::test(start_paused = true)]
async fn a_request_read_has_a_permit_while_background_holds_its_share() {
    const LABEL: &str = "share.s3.test/goes";
    let slow = Duration::from_secs(5);
    let host = Scripted::new(slow);
    let store = s3("share.s3.test", "goes", &host);
    let path = Path::from("tile.nc");
    let scan = store.clone().for_background();
    let tiles = futures::future::join_all(
        (0..16).map(|_| timed_read(&scan, &path, FetchBudget::Background, Duration::ZERO)),
    );
    let request = async {
        let read = timed_read(
            &store,
            &path,
            FetchBudget::Request,
            Duration::from_millis(10),
        );
        let (asked, served) = read.await;
        (asked, served, requests(LABEL))
    };
    let (tiles, (asked, served, during)) = tokio::join!(tiles, request);
    assert_eq!(served - asked, slow, "a permit at once");
    assert_eq!(during.0, INITIAL);
    let last = tiles.iter().map(|(_, served)| *served).max().unwrap();
    assert_eq!(last - tiles[0].0, slow * 2, "eight, then the other eight");
    assert_eq!(host.take_peak(), INITIAL / 2 + 1);
}

/// At a limit of 1 background work has no share: a poll's 4 s read waits,
/// and a request's read arriving after it has the only permit at once
/// (#1530). The poll's read is not starved: once it has waited
/// `BACKGROUND_PATIENCE` it takes a permit past the share.
#[tokio::test(start_paused = true)]
async fn at_a_limit_of_one_background_work_waits_its_patience() {
    use super::limit::BACKGROUND_PATIENCE;
    let slow = Duration::from_secs(4);
    let host = Scripted::new(slow);
    let (store, path) = limited("http://one.backoff.test/x", 1, &host);
    let poll = store.clone().for_background();
    let ((poll_asked, poll_served), (asked, served)) = tokio::join!(
        timed_read(&poll, &path, FetchBudget::Background, Duration::ZERO),
        timed_read(
            &store,
            &path,
            FetchBudget::Request,
            Duration::from_millis(100)
        ),
    );
    assert_eq!(served - asked, slow, "the request has the permit at once");
    assert_eq!(poll_served - poll_asked, BACKGROUND_PATIENCE + slow);
}

/// Places in the background share go first come, first served: one
/// collection's reads, each asking again as it ends, do not keep another's
/// read out of the share until they stop (#1530).
#[tokio::test(start_paused = true)]
async fn background_places_go_first_come_first_served() {
    let host = Scripted::new(Duration::from_millis(100));
    let store = s3("places.s3.test", "goes", &host);
    let path = Path::from("scan.nc");
    let pipeline = store.clone().for_background();
    let other = store.clone().for_background();
    let end = Instant::now() + Duration::from_secs(5);
    let share = INITIAL / 2;
    let (_, (asked, served)) = tokio::join!(
        read_until(&pipeline, &path, share, end),
        timed_read(
            &other,
            &path,
            FetchBudget::Background,
            Duration::from_millis(1050)
        ),
    );
    let waited = served - asked;
    assert!(waited <= Duration::from_millis(250), "{waited:?}");
}

/// Only a request naming the bucket, by path after its endpoint's own path
/// or by host, goes through its throttle: a credential endpoint's, which
/// object_store sends through the same connector, passes by. A bucket
/// counts a 503 and a 429; an HTTP host only a 429.
#[test]
fn a_bucket_governs_the_requests_that_name_it() {
    let bucket = super::Kind::bucket("forecasts", "");
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

    let prefixed = super::Kind::bucket("forecasts", "/s3/");
    let governs = |uri: &str| prefixed.governs(&uri.parse().unwrap());
    assert!(governs("http://minio.example/s3/forecasts/a.index"));
    assert!(governs("http://minio.example/s3/forecasts?list-type=2"));
    assert!(!governs("http://minio.example/forecasts/a.index"));
    assert_eq!(
        crate::bucket::endpoint_path(Some("http://minio.example:9000/s3/")),
        "/s3"
    );
    assert_eq!(
        crate::bucket::endpoint_path(Some("https://s3.amazonaws.com")),
        ""
    );

    assert!(bucket.refuses(StatusCode::SERVICE_UNAVAILABLE));
    assert!(bucket.refuses(StatusCode::TOO_MANY_REQUESTS));
    assert!(!bucket.refuses(StatusCode::INTERNAL_SERVER_ERROR));
    assert!(!super::Kind::Http.refuses(StatusCode::SERVICE_UNAVAILABLE));
}

/// A throttling bucket logs the WARN naming the bucket and the answer, and
/// its return the INFO lines naming `SlowDown`; the limit, cut to 1 by six
/// seconds of them, is back in four minutes, an eighth of its ceiling a
/// quiet interval.
#[tokio::test(start_paused = true)]
async fn a_throttling_bucket_warns_and_its_limit_is_back_in_minutes() {
    const LABEL: &str = "log.s3.test/forecasts";
    let log = JsonLog::default();
    let _logging = tracing::subscriber::set_default(log.subscriber());
    let host = Scripted::new(Duration::ZERO);
    let store = s3("log.s3.test", "forecasts", &host).for_collection("ifs");
    let path = Path::from("index");
    throttle_bucket(&host, &store, &path).await;
    let warns = at_level(&log, "WARN");
    assert_eq!(warns.len(), 1, "{warns:#?}");
    assert_eq!(warns[0]["host"], LABEL);
    assert_eq!(
        warns[0]["message"],
        format!(
            "{LABEL} answered 503 SlowDown: request limit cut to 8 of 64; \
             requests start 250 ms apart"
        )
    );
    assert_eq!(warns[0]["span"]["collection"], "ifs");
    for _ in 0..6 {
        tokio::time::sleep(CUT_WINDOW).await;
        host.answer(503, None, Duration::ZERO);
        store.inner().head(&path).await.unwrap();
    }
    assert_eq!(requests(LABEL).0, 1);
    let mut limits = Vec::new();
    for _ in 0..8 {
        tokio::time::sleep(QUIET_INTERVAL).await;
        limits.push(requests(LABEL).0);
    }
    assert_eq!(limits, [9, 17, 25, 33, 41, 49, 57, 64]);
    assert_eq!(store.back_off().map(|b| b.backing_off), Some(false));
    let infos: Vec<_> = at_level(&log, "INFO")
        .into_iter()
        .map(|line| line["message"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        infos,
        [
            format!("{LABEL} has answered no SlowDown for 30 s: its request limit is back at 64"),
            format!("{LABEL} has answered no SlowDown for 120 s: its requests are no longer paced"),
        ]
    );
}

/// A bucket starts at 16 requests in flight and grows to its ceiling of
/// 64 by 8 each quiet interval, logging nothing; requests waiting for its
/// permits at the ceiling are logged once.
#[tokio::test(start_paused = true)]
async fn a_bucket_starts_at_sixteen_and_warns_when_its_ceiling_binds() {
    const LABEL: &str = "start.s3.test/goes";
    let log = JsonLog::default();
    let _logging = tracing::subscriber::set_default(log.subscriber());
    let host = Scripted::new(Duration::from_secs(1));
    let store = s3("start.s3.test", "goes", &host);
    let path = Path::from("scan.nc");
    assert_eq!(peak(&host, &store, &path, 24).await, INITIAL);
    assert!(at_level(&log, "WARN").is_empty(), "below its ceiling");
    tokio::time::sleep(QUIET_INTERVAL * 6).await;
    assert_eq!(requests(LABEL).0, crate::bucket::S3_REQUEST_LIMIT);
    assert!(at_level(&log, "INFO").is_empty(), "never cut");
    assert_eq!(peak(&host, &store, &path, 80).await, 64);
    let warns = at_level(&log, "WARN");
    assert_eq!(warns.len(), 1, "{warns:#?}");
    assert_eq!(warns[0]["host"], LABEL);
    assert!(
        warns[0]["message"]
            .as_str()
            .unwrap()
            .contains("MC_COG_TILE_CONCURRENCY above 64"),
        "{warns:#?}"
    );
}

/// One bucket has one label, and so one back-off, however its endpoint is
/// spelt; a bucket whose stores are all dropped leaves `host_requests`.
#[test]
fn a_bucket_has_one_label_and_goes_with_its_stores() {
    use crate::traced::s3_label;
    for endpoint in [
        "https://s3.eu-west-1.amazonaws.com",
        "https://s3-eu-west-1.amazonaws.com",
        "https://S3.EU-WEST-1.amazonaws.com/",
        "https://s3.dualstack.eu-west-1.amazonaws.com",
        "https://radar.s3.eu-west-1.amazonaws.com",
    ] {
        assert_eq!(
            s3_label(Some(endpoint), None, "radar"),
            "s3.eu-west-1.amazonaws.com/radar",
            "{endpoint}"
        );
    }
    assert_eq!(
        s3_label(Some("https://s3.amazonaws.com"), None, "radar"),
        "s3.us-east-1.amazonaws.com/radar"
    );
    assert_eq!(
        s3_label(Some("https://minio.example:9000/s3"), None, "radar"),
        "minio.example:9000/radar"
    );

    let (dashed, _) = crate::build_store("https://s3-eu-west-1.amazonaws.com/onelabel/x/").unwrap();
    let (hosted, _) = crate::build_store("https://onelabel.s3.eu-west-1.amazonaws.com/x/").unwrap();
    let parts =
        crate::build_s3_store_from_parts("https://s3.eu-west-1.amazonaws.com", "onelabel").unwrap();
    let label = "s3.eu-west-1.amazonaws.com/onelabel";
    let reported = || crate::requests_to(|host| host == label).len();
    assert_eq!(reported(), 1);
    assert!(Arc::ptr_eq(
        dashed.throttle().unwrap(),
        hosted.throttle().unwrap()
    ));
    assert!(Arc::ptr_eq(
        dashed.throttle().unwrap(),
        parts.throttle().unwrap()
    ));
    drop((dashed, hosted));
    assert_eq!(reported(), 1, "a store is still built on it");
    drop(parts);
    assert_eq!(reported(), 0, "gone with its stores");
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

/// Over a real server: a bucket's three `503 Slow Down`s and then a 200
/// serve the read, cut the bucket's limit and count for it.
#[tokio::test(flavor = "multi_thread")]
async fn a_slowdown_over_http_backs_off_the_bucket() {
    let modified = "2026-11-22T06:00:00Z".parse().unwrap();
    let server = RangeServer::new(BODY, modified);
    {
        let mut object = server.object.lock().unwrap();
        object.path = "/forecasts/object".into();
        object
            .fail_next
            .extend(["503 Slow Down", "503 Slow Down", "503 Slow Down"]);
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
    assert_eq!((bucket.limit, bucket.throttled), (INITIAL / 2, 3));
    assert_eq!(store.back_off().map(|b| b.backing_off), Some(true));
}
