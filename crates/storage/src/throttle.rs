//! Backing off a throttling HTTP host as a whole (#1235, #1290).
//!
//! object_store retries a 429 Too Many Requests on its own, one request at
//! a time, after a backoff that starts at 0.1 s, while every other request
//! to the host goes on at full concurrency. A host that throttles asks for
//! fewer requests, so every HTTP store this crate builds sends its requests
//! through [`Throttled`]: it sees the response to each attempt, object_store's
//! retries included, and reports a 429 to the host's [`HostThrottle`], one
//! per scheme, host and port for the process, shared by every store on it:
//!
//! - **`Retry-After`**, in delta-seconds or as an HTTP-date: new requests
//!   to the host, from every store on it, limited or not, wait until then,
//!   at most [`MAX_RETRY_AFTER`]. The wait is part of the operation, so its
//!   fetch budget and the request deadline bound it, as they bound the wait
//!   for a limit permit.
//! - **The pace**: a limit on concurrency is not one on rate, and a 429
//!   comes back at once, so after one the host's requests, from every store
//!   on it, start at least the pace apart (#1290). It starts at
//!   [`PACE_FLOOR`] and doubles, at most once per [`CUT_WINDOW`], up to
//!   [`PACE_CAP`]; each [`QUIET_INTERVAL`] without a 429 halves it, and
//!   below the floor it is off. A host that never answered 429 has none,
//!   and its requests wait for nothing. Turns go in the order the requests
//!   asked for them, and a turn is taken when it comes, never booked
//!   ahead, so a request dropped at its deadline holds none (#1530).
//! - **The retry**: a request refused without `Retry-After` is retried
//!   [`RETRY_DELAY`] after the refusal, not object_store's 0.1 s: its retry
//!   waits for it, holding the request permit, and a last attempt fails at
//!   once.
//! - **The adaptive limit** of a store [`crate::build_http_store_limited`]
//!   built ([`AdaptiveLimit`]): it halves, never below 1, at most once per
//!   [`CUT_WINDOW`], and grows by an eighth of the configured limit, at
//!   least 1, for each [`QUIET_INTERVAL`] without a 429, back to it.
//!
//! [`throttled_answers`] counts each host's 429s, for its callers and
//! `/metrics`.
//!
//! An S3 bucket throttles with `503 SlowDown` instead, and it throttles
//! the bucket as a whole: on 2026-11-22 every GET of a public bucket, on
//! every prefix, was refused for 40 minutes while HEADs and LISTs were
//! answered (#1530). So every S3 store this crate builds has a throttle
//! per bucket (`bucket.rs`), [`Kind::Bucket`], shared by every store on
//! it: the requests that name the bucket go through it, and once the
//! bucket throttles, [`BUCKET_TRIGGER`] 503s or 429s within
//! [`BUCKET_TRIGGER_WINDOW`], it is backed off exactly as a 429 backs off
//! an HTTP host; a stray `SlowDown` is only counted. Requests to another
//! host, a credential endpoint, pass by.

mod limit;
#[cfg(test)]
mod tests;

pub(crate) use limit::{AdaptiveLimit, Limited};

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, SystemTime};

use object_store::client::{HttpError, HttpRequest, HttpResponse, HttpService};
use reqwest::header::{HeaderMap, RETRY_AFTER};
use reqwest::StatusCode;
use tokio::sync::Notify;
use tokio::time::Instant;

/// The longest a `Retry-After` stops new requests to a host. A server may
/// ask for an hour, or send a date far ahead by a clock error, and the
/// collections reading it must not stall for that long: past this, the
/// adaptive limit and object_store's own retries carry the back-off.
pub(crate) const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// A limit is cut at most once per this window. The 429s answering
/// requests sent together are one signal, not one per request, so a burst
/// of them cuts the limit once instead of driving it to 1.
pub(crate) const CUT_WINDOW: Duration = Duration::from_secs(1);

/// A cut limit grows by 1 after each this long without a 429 from its
/// host, up to the configured limit.
pub(crate) const QUIET_INTERVAL: Duration = Duration::from_secs(30);

/// The pace a host's first 429 sets: its requests start at least this far
/// apart, 4 a second. Two background requests turning over in tens of
/// milliseconds send dozens a second, which is what thredds.met.no refused.
pub(crate) const PACE_FLOOR: Duration = Duration::from_millis(250);

/// The most a pace grows to, doubling from [`PACE_FLOOR`] in four 429
/// windows: 30 requests a minute. A MEPS run's 67 OPeNDAP opens, three
/// requests each, still fit in under 7 minutes. Only a request holding one
/// of a limited host's permits takes a turn, and the 429s that raise the
/// pace cut the limit as well, so at the cap a request waits a turn or
/// two, seconds, inside a request's deadline.
pub(crate) const PACE_CAP: Duration = Duration::from_secs(2);

/// How long after a request was refused with a 429 without `Retry-After`
/// its retry, from object_store or the document layer, is sent, where
/// their own back-off is 0.1 s. Past [`CUT_WINDOW`], so the retry goes out
/// under the cut limit and the raised pace, not in the burst that was
/// refused; three attempts then span at least 4 s, past a per-second
/// limiter's window. The retry waits for it, not the refused answer, so
/// the last attempt fails at once.
pub(crate) const RETRY_DELAY: Duration = Duration::from_secs(2);

/// An S3 bucket backs off at its `BUCKET_TRIGGER`th throttling answer
/// within [`BUCKET_TRIGGER_WINDOW`] (#1530). S3 answers a stray `SlowDown`
/// under ordinary load, about one in ten minutes in production, which
/// object_store's own 0.5 s retry rides out: backing off for it would hold
/// the bucket's every read, a radar animation's included, for 30 s. Three
/// within 10 s is a bucket that throttles: one GET refused at its first
/// three attempts, or several refused together. An HTTP host's 429 says so
/// at once.
pub(crate) const BUCKET_TRIGGER: usize = 3;

/// See [`BUCKET_TRIGGER`].
pub(crate) const BUCKET_TRIGGER_WINDOW: Duration = Duration::from_secs(10);

tokio::task_local! {
    /// Whether the operation in progress is background work, which no
    /// request waits on: a read of a store
    /// [`crate::DataStore::for_background`] made, set around each of its
    /// operations by its `storage` span's layer (`traced.rs`). Such reads
    /// hold at most a share of a host's limit ([`AdaptiveLimit`]); unset,
    /// a read is one a request may wait on.
    pub(crate) static BACKGROUND: bool;
}

/// Whether the operation in progress is background work ([`BACKGROUND`]).
pub(crate) fn background() -> bool {
    BACKGROUND
        .try_with(|background| *background)
        .unwrap_or(false)
}

/// At most one WARN per host per this long, however often it throttles.
const WARN_INTERVAL: Duration = Duration::from_secs(60);

/// The back-off's times on one host: the constants, or shorter for other
/// crates' tests (`test_util::quick_backoff`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timing {
    pub pace_floor: Duration,
    pub pace_cap: Duration,
    pub retry_delay: Duration,
}

impl Timing {
    pub(crate) const DEFAULT: Timing = Timing {
        pace_floor: PACE_FLOOR,
        pace_cap: PACE_CAP,
        retry_delay: RETRY_DELAY,
    };
}

/// What a throttle governs, and which answers back it off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Kind {
    /// An HTTP host: every request to it, backed off by its first 429.
    Http,
    /// The S3 bucket `name` (#1530): the requests naming it, by path
    /// (`path`, the endpoint's own path and `/name`, as `/forecasts` or
    /// `/s3/forecasts`) or by host (`name.s3.…`), backed off by its
    /// [`BUCKET_TRIGGER`]th 503, S3's `SlowDown`, or 429 in
    /// [`BUCKET_TRIGGER_WINDOW`].
    Bucket { name: String, path: String },
}

impl Kind {
    /// The bucket `name` at an endpoint whose URL path is `endpoint_path`
    /// (empty, or `/s3` for `https://minio.example/s3`).
    pub(crate) fn bucket(name: &str, endpoint_path: &str) -> Self {
        Kind::Bucket {
            name: name.to_string(),
            path: format!("{}/{name}", endpoint_path.trim_end_matches('/')),
        }
    }

    /// Whether a request to `uri` goes through the throttle.
    fn governs(&self, uri: &http::Uri) -> bool {
        let Kind::Bucket { name, path } = self else {
            return true;
        };
        let by_path = uri
            .path()
            .strip_prefix(path.as_str())
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'));
        let by_host = uri
            .host()
            .and_then(|host| host.strip_prefix(name.as_str()))
            .is_some_and(|rest| rest.starts_with('.'));
        by_path || by_host
    }

    /// Whether an answer with `status` asks for fewer requests.
    pub(crate) fn refuses(&self, status: StatusCode) -> bool {
        status == StatusCode::TOO_MANY_REQUESTS
            || (matches!(self, Kind::Bucket { .. }) && status == StatusCode::SERVICE_UNAVAILABLE)
    }

    /// The throttling answer, as the quiet-time log lines name it.
    pub(crate) fn quiet_name(&self) -> &'static str {
        match self {
            Kind::Http => "429",
            Kind::Bucket { .. } => "SlowDown",
        }
    }
}

/// The identity of a request across its attempts: its method, URI and
/// range. object_store and the document layer resend a refused request
/// unchanged, so its retry has the refused attempt's.
fn request_key(request: &HttpRequest) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    request.method().hash(&mut hasher);
    request.uri().hash(&mut hasher);
    request
        .headers()
        .get(http::header::RANGE)
        .map(|range| range.as_bytes())
        .hash(&mut hasher);
    hasher.finish()
}

/// The answer `status` as the WARN names it: S3's 503 is its `SlowDown`.
fn answer_name(status: StatusCode) -> &'static str {
    if status == StatusCode::SERVICE_UNAVAILABLE {
        "503 SlowDown"
    } else {
        "429 Too Many Requests"
    }
}

/// What one host asked for with its 429s, shared by every HTTP store on
/// its scheme, host and port ([`for_url`]), or one S3 bucket with its
/// `SlowDown`s, shared by every S3 store on it (`bucket.rs`).
#[derive(Debug)]
pub(crate) struct HostThrottle {
    /// `host[:port]`, as the `storage` span and `/metrics` name it; an S3
    /// bucket's `host[:port]/bucket`.
    host: String,
    kind: Kind,
    state: Mutex<HostState>,
    /// The throttling answers the host has given: 429s, and an S3
    /// bucket's 503s.
    answers: AtomicU64,
    /// Told when the first waiter for a turn under the pace may have
    /// changed: one took its turn or gave up.
    turns: Notify,
}

#[derive(Debug)]
struct HostState {
    /// New requests wait until then: the latest `Retry-After`.
    paused_until: Option<Instant>,
    /// The least time between the starts of two requests: zero until the
    /// host answers 429.
    pace: Duration,
    /// When the next request may start under `pace`.
    next_at: Option<Instant>,
    /// When the pace was last raised.
    paced_at: Option<Instant>,
    /// When the host last answered 429.
    answered_at: Option<Instant>,
    /// Since when the host has sent no 429: its last one, or the pace's
    /// last halving.
    pace_quiet_since: Option<Instant>,
    timing: Timing,
    /// The adaptive limits of the limited stores on the host.
    limits: Vec<Arc<AdaptiveLimit>>,
    /// When the last WARN was logged.
    warned_at: Option<Instant>,
    /// A bucket's throttling answers within [`BUCKET_TRIGGER_WINDOW`], at
    /// most [`BUCKET_TRIGGER`] of them.
    recent: VecDeque<Instant>,
    /// The requests refused without `Retry-After` whose retry is still to
    /// wait its retry delay, by [`request_key`], and when.
    refused: HashMap<u64, Instant>,
    /// The tickets of the requests waiting for a turn under the pace, in
    /// the order they asked: the first takes the next turn.
    waiting: BTreeSet<u64>,
    /// The next ticket.
    next_ticket: u64,
}

impl HostThrottle {
    pub(crate) fn new(host: String, kind: Kind) -> Self {
        Self {
            host,
            kind,
            state: Mutex::new(HostState {
                paused_until: None,
                pace: Duration::ZERO,
                next_at: None,
                paced_at: None,
                answered_at: None,
                pace_quiet_since: None,
                timing: Timing::DEFAULT,
                limits: Vec::new(),
                warned_at: None,
                recent: VecDeque::new(),
                refused: HashMap::new(),
                waiting: BTreeSet::new(),
                next_ticket: 0,
            }),
            answers: AtomicU64::new(0),
            turns: Notify::new(),
        }
    }

    /// The throttling answers the host has given, to every store on it.
    pub(crate) fn answers(&self) -> u64 {
        self.answers.load(Ordering::Relaxed)
    }

    /// `host[:port]`, or an S3 bucket's `host[:port]/bucket`.
    #[cfg(any(test, feature = "test-util"))]
    pub(crate) fn host(&self) -> &str {
        &self.host
    }

    /// Whether the host's requests are paced or paused now: from its
    /// throttling answer until the pace has decayed below the floor, one
    /// to four [`QUIET_INTERVAL`]s later, and while a `Retry-After` lasts.
    /// The adaptive limit may still be growing back after that.
    pub(crate) fn backing_off(&self) -> bool {
        let mut state = self.state();
        let now = Instant::now();
        self.decay(&mut state, now);
        !state.pace.is_zero() || state.paused_until.is_some_and(|until| until > now)
    }

    /// End the back-off at once, as its quiet intervals would: no pace and
    /// no pause. The limits keep theirs.
    #[cfg(any(test, feature = "test-util"))]
    pub(crate) fn quiet(&self) {
        let mut state = self.state();
        state.pace = Duration::ZERO;
        state.next_at = None;
        state.paced_at = None;
        state.paused_until = None;
        state.recent.clear();
        state.refused.clear();
    }

    /// The pace now: zero while the host is not throttling.
    #[cfg(test)]
    pub(crate) fn pace(&self) -> Duration {
        let mut state = self.state();
        self.decay(&mut state, Instant::now());
        state.pace
    }

    /// Back off on `timing` instead of the constants.
    #[cfg(any(test, feature = "test-util"))]
    pub(crate) fn set_timing(&self, timing: Timing) {
        self.state().timing = timing;
    }

    fn state(&self) -> MutexGuard<'_, HostState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Make `limit` one of the host's, cut by its 429s.
    pub(crate) fn add_limit(&self, limit: Arc<AdaptiveLimit>) {
        self.state().limits.push(limit);
    }

    /// Wait to send `request` ([`request_key`]): until its retry delay
    /// has passed if it is the retry of a refused one, until the host's
    /// `Retry-After` has passed, and then for its turn under the pace, if
    /// the host has one: at once when there is none of them. A later 429
    /// can push the `Retry-After` out while this waits.
    ///
    /// Turns go first come, first served: a waiter takes a ticket and the
    /// first ticket takes the next turn when it comes, never booked ahead,
    /// so a waiter dropped at its deadline holds none and the next takes
    /// it. Only a request holding one of a limited host's permits waits
    /// here, so the queue is no longer than the limit.
    pub(crate) async fn wait(&self, request: u64) {
        /// What the waiter does next.
        enum Step {
            Send,
            Sleep(Instant),
            Wait,
        }
        let retry_at = {
            let mut state = self.state();
            let delay = state.timing.retry_delay;
            state.refused.remove(&request).map(|at| at + delay)
        };
        let mut ticket: Option<Ticket<'_>> = None;
        loop {
            let first = self.turns.notified();
            tokio::pin!(first);
            first.as_mut().enable();
            let step = {
                let mut state = self.state();
                let now = Instant::now();
                self.decay(&mut state, now);
                let blocked = [retry_at, state.paused_until]
                    .into_iter()
                    .flatten()
                    .filter(|until| *until > now)
                    .max();
                if let Some(until) = blocked {
                    Step::Sleep(until)
                } else if state.pace.is_zero() {
                    Step::Send
                } else {
                    let mine = match &ticket {
                        Some(ticket) => ticket.1,
                        None => {
                            let id = state.next_ticket;
                            state.next_ticket += 1;
                            state.waiting.insert(id);
                            ticket = Some(Ticket(self, id));
                            id
                        }
                    };
                    let turn = state.next_at.unwrap_or(now).max(now);
                    if state.waiting.first() != Some(&mine) {
                        Step::Wait
                    } else if turn > now {
                        Step::Sleep(turn)
                    } else {
                        state.next_at = Some(now + state.pace);
                        Step::Send
                    }
                }
            };
            match step {
                Step::Send => return,
                Step::Sleep(until) => tokio::time::sleep_until(until).await,
                Step::Wait => first.await,
            }
        }
    }

    /// One step down for each [`QUIET_INTERVAL`] since the last 429 or
    /// step: the pace halves, and below the floor it is off, logged once.
    fn decay(&self, state: &mut HostState, now: Instant) {
        let Some(mut since) = state.pace_quiet_since else {
            return;
        };
        while !state.pace.is_zero() && now.saturating_duration_since(since) >= QUIET_INTERVAL {
            since += QUIET_INTERVAL;
            state.pace /= 2;
            if state.pace < state.timing.pace_floor {
                state.pace = Duration::ZERO;
                state.next_at = None;
                let quiet = state
                    .answered_at
                    .map(|at| since.saturating_duration_since(at));
                tracing::info!(
                    host = %self.host,
                    "{} has answered no {} for {} s: its requests are no longer paced",
                    self.host,
                    self.kind.quiet_name(),
                    quiet.unwrap_or_default().as_secs()
                );
            }
        }
        state.pace_quiet_since = Some(since);
    }

    /// The host answered `request` ([`request_key`]) with `status`, a 429
    /// or an S3 bucket's 503, with `retry_after` when it said how long to
    /// wait: pause new requests that long, at most [`MAX_RETRY_AFTER`],
    /// raise the pace, cut the host's limits, and, when it did not say,
    /// hold the request's retry for the retry delay. A bucket does so from
    /// its [`BUCKET_TRIGGER`]th answer in [`BUCKET_TRIGGER_WINDOW`], or
    /// its first with `Retry-After`, and then while it backs off; before,
    /// the answer is only counted.
    pub(crate) fn throttled(
        &self,
        status: StatusCode,
        retry_after: Option<Duration>,
        request: u64,
    ) {
        let now = Instant::now();
        self.answers.fetch_add(1, Ordering::Relaxed);
        let wait = retry_after
            .map(|wait| wait.min(MAX_RETRY_AFTER))
            .filter(|wait| !wait.is_zero());
        let (limits, paced) = {
            let mut state = self.state();
            self.decay(&mut state, now);
            if matches!(self.kind, Kind::Bucket { .. }) {
                state.recent.push_back(now);
                while state.recent.len() > BUCKET_TRIGGER
                    || state.recent.front().is_some_and(|at| {
                        now.saturating_duration_since(*at) > BUCKET_TRIGGER_WINDOW
                    })
                {
                    state.recent.pop_front();
                }
                let throttling = !state.pace.is_zero()
                    || state.paused_until.is_some_and(|until| until > now)
                    || state.recent.len() >= BUCKET_TRIGGER;
                if !throttling && wait.is_none() {
                    return;
                }
            }
            if let Some(wait) = wait {
                let until = now + wait;
                state.paused_until = Some(state.paused_until.map_or(until, |at| at.max(until)));
            }
            if retry_after.is_none() {
                let delay = state.timing.retry_delay;
                state.refused.retain(|_, at| now < *at + delay);
                state.refused.insert(request, now);
            }
            state.answered_at = Some(now);
            state.pace_quiet_since = Some(now);
            let timing = state.timing;
            let raise = state.paced_at.is_none_or(|at| now >= at + CUT_WINDOW);
            let raised = raise && state.pace < timing.pace_cap;
            if raised {
                state.pace = (state.pace * 2).clamp(timing.pace_floor, timing.pace_cap);
                state.paced_at = Some(now);
            }
            // The next request waits a whole pace after this answer.
            let next = now + state.pace;
            state.next_at = Some(state.next_at.map_or(next, |at| at.max(next)));
            let paced = raised.then_some(state.pace);
            (state.limits.clone(), paced)
        };
        let mut cut = false;
        let mut sum = None;
        for limit in &limits {
            cut |= limit.cut(now);
            let (current, configured) = limit.limits(now);
            let (all_current, all_configured) = sum.unwrap_or((0, 0));
            sum = Some((all_current + current, all_configured + configured));
        }
        if !cut && wait.is_none() && paced.is_none() {
            return;
        }
        {
            let mut state = self.state();
            if state.warned_at.is_some_and(|at| now < at + WARN_INTERVAL) {
                return;
            }
            state.warned_at = Some(now);
        }
        let limit = sum.map(|(current, configured)| {
            let cut = if cut { "cut to " } else { "" };
            format!("request limit {cut}{current} of {configured}")
        });
        let pace = paced.map(|pace| format!("requests start {} ms apart", pace.as_millis()));
        let pause = wait.map(|wait| {
            format!(
                "new requests wait {:.1} s (Retry-After)",
                wait.as_secs_f64()
            )
        });
        let detail: Vec<String> = [limit, pace, pause].into_iter().flatten().collect();
        tracing::warn!(
            host = %self.host,
            limit = sum.map(|(current, _)| current),
            configured_limit = sum.map(|(_, configured)| configured),
            pace_secs = paced.map(|pace| pace.as_secs_f64()),
            retry_after_secs = wait.map(|wait| wait.as_secs_f64()),
            "{} answered {}: {}",
            self.host,
            answer_name(status),
            detail.join("; ")
        );
    }
}

/// A waiter's place in [`HostState::waiting`], given up when it has its
/// turn or is dropped, which tells the next.
struct Ticket<'a>(&'a HostThrottle, u64);

impl Drop for Ticket<'_> {
    fn drop(&mut self) {
        self.0.state().waiting.remove(&self.1);
        self.0.turns.notify_waiters();
    }
}

/// Every host's throttle, by scheme, host and port. Held weakly: a
/// throttle lives as long as a store on its host, and the entries of hosts
/// left without one are dropped when another host's throttle is made, so
/// stores built per poll for a feed's origins leave nothing behind.
static HOSTS: LazyLock<Mutex<BTreeMap<String, Weak<HostThrottle>>>> =
    LazyLock::new(Default::default);

/// The key of `url`'s throttle in [`HOSTS`]: its scheme, host and port.
fn key(url: &url::Url) -> String {
    format!(
        "{}://{}:{}",
        url.scheme(),
        url.host_str().unwrap_or_default(),
        url.port_or_known_default().unwrap_or_default()
    )
}

/// The throttle of `url`'s scheme, host and port.
pub(crate) fn for_url(url: &url::Url) -> Arc<HostThrottle> {
    let key = key(url);
    let mut hosts = HOSTS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(host) = hosts.get(&key).and_then(Weak::upgrade) {
        return host;
    }
    hosts.retain(|_, host| host.strong_count() > 0);
    let host = Arc::new(HostThrottle::new(
        crate::traced::host_port(url).unwrap_or_default(),
        Kind::Http,
    ));
    hosts.insert(key, Arc::downgrade(&host));
    host
}

/// The 429 Too Many Requests answers the host of `url` has given, to every
/// store this crate built on its scheme, host and port, since the first
/// such store was built: 0 for a host none has been built for, or one that
/// never throttled. A caller compares two readings to tell whether its host
/// throttled in between; `/metrics` reports it per host
/// ([`crate::host_requests`]).
pub fn throttled_answers(url: &str) -> u64 {
    let Ok(url) = url::Url::parse(url) else {
        return 0;
    };
    let hosts = HOSTS.lock().unwrap_or_else(PoisonError::into_inner);
    hosts
        .get(&key(&url))
        .and_then(Weak::upgrade)
        .map_or(0, |host| host.answers())
}

/// An HTTP transport whose every request the throttle governs waits for
/// its turn under its host's pace, its `Retry-After` and, a retry, its
/// retry delay before it is sent ([`HostThrottle::wait`]), and whose every
/// throttling answer (a 429, an S3 bucket's 503) is reported to the host
/// ([`HostThrottle::throttled`]) and handed back at once. object_store
/// calls it once per attempt, so it sees the attempts object_store retries
/// as well.
#[derive(Debug, Clone)]
pub(crate) struct Throttled<S> {
    pub inner: S,
    pub host: Arc<HostThrottle>,
}

#[async_trait::async_trait]
impl<S: HttpService> HttpService for Throttled<S> {
    async fn call(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        if !self.host.kind.governs(request.uri()) {
            return self.inner.call(request).await;
        }
        let key = request_key(&request);
        self.host.wait(key).await;
        let response = self.inner.call(request).await?;
        let status = response.status();
        if self.host.kind.refuses(status) {
            let wait = retry_after(response.headers(), SystemTime::now());
            self.host.throttled(status, wait, key);
        }
        Ok(response)
    }
}

/// How long a response's `Retry-After` asks to wait from `now`: its
/// delta-seconds, or the time to its HTTP-date in any of the three forms
/// RFC 9110 has recipients accept. A date already past asks for no wait;
/// a value that is neither is ignored.
pub(crate) fn retry_after(headers: &HeaderMap, now: SystemTime) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    if value.is_empty() {
        return None;
    }
    if value.bytes().all(|byte| byte.is_ascii_digit()) {
        // Too many digits for a u64 is a long wait all the same.
        return Some(Duration::from_secs(value.parse().unwrap_or(u64::MAX)));
    }
    let date = http_date(value)?;
    Some(date.duration_since(now).unwrap_or_default())
}

/// An HTTP-date: IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`), or the
/// obsolete RFC 850 (`Sunday, 06-Nov-94 08:49:37 GMT`) and asctime
/// (`Sun Nov  6 08:49:37 1994`) forms, all in UTC.
fn http_date(value: &str) -> Option<SystemTime> {
    use chrono::NaiveDateTime;
    if let Ok(date) = chrono::DateTime::parse_from_rfc2822(value) {
        return Some(date.into());
    }
    ["%A, %d-%b-%y %H:%M:%S GMT", "%a %b %e %H:%M:%S %Y"]
        .iter()
        .find_map(|format| NaiveDateTime::parse_from_str(value, format).ok())
        .map(|date| date.and_utc().into())
}
