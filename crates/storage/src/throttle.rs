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
//!   and its requests wait for nothing.
//! - **The retry**: a request refused without `Retry-After` is retried
//!   after [`RETRY_DELAY`], not object_store's 0.1 s; its request permit
//!   is held meanwhile.
//! - **The adaptive limit** of a store [`crate::build_http_store_limited`]
//!   built ([`AdaptiveLimit`]): it halves, never below 1, at most once per
//!   [`CUT_WINDOW`], and grows by 1 for each [`QUIET_INTERVAL`] without a
//!   429, back to the configured limit.
//!
//! [`throttled_answers`] counts each host's 429s, for its callers and
//! `/metrics`. S3 stores are not throttled here: S3 answers 503 SlowDown,
//! which object_store retries.

mod limit;
#[cfg(test)]
mod tests;

pub(crate) use limit::{AdaptiveLimit, Limited};

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, SystemTime};

use object_store::client::{HttpError, HttpRequest, HttpResponse, HttpService};
use reqwest::header::{HeaderMap, RETRY_AFTER};
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

/// How long a request refused with a 429 without `Retry-After` waits
/// before object_store or the document layer retries it, in place of
/// their 0.1 s. Past [`CUT_WINDOW`], so the retry goes out under the cut
/// limit and the raised pace, not in the burst that was refused; three
/// attempts then span at least 4 s, past a per-second limiter's window.
pub(crate) const RETRY_DELAY: Duration = Duration::from_secs(2);

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

/// What one host asked for with its 429s, shared by every HTTP store on
/// its scheme, host and port ([`for_url`]).
#[derive(Debug)]
pub(crate) struct HostThrottle {
    /// `host[:port]`, as the `storage` span and `/metrics` name it.
    host: String,
    state: Mutex<HostState>,
    /// The 429s the host has answered.
    answers: AtomicU64,
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
}

impl HostThrottle {
    pub(crate) fn new(host: String) -> Self {
        Self {
            host,
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
            }),
            answers: AtomicU64::new(0),
        }
    }

    /// The 429s the host has answered, to every store on it.
    pub(crate) fn answers(&self) -> u64 {
        self.answers.load(Ordering::Relaxed)
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

    /// Wait for the request's turn under the pace, if the host has one,
    /// and until the host's `Retry-After` has passed: at once when it has
    /// neither. A later 429 can push the `Retry-After` out while this waits.
    pub(crate) async fn wait(&self) {
        let turn = {
            let mut state = self.state();
            let now = Instant::now();
            self.decay(&mut state, now);
            if state.pace.is_zero() {
                None
            } else {
                let turn = [Some(now), state.next_at, state.paused_until]
                    .into_iter()
                    .flatten()
                    .max()
                    .unwrap_or(now);
                state.next_at = Some(turn + state.pace);
                Some(turn)
            }
        };
        if let Some(turn) = turn {
            tokio::time::sleep_until(turn).await;
        }
        loop {
            let until = self.state().paused_until;
            match until {
                Some(until) if until > Instant::now() => tokio::time::sleep_until(until).await,
                _ => return,
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
                    "{} has answered no 429 for {} s: its requests are no longer paced",
                    self.host,
                    quiet.unwrap_or_default().as_secs()
                );
            }
        }
        state.pace_quiet_since = Some(since);
    }

    /// The host answered 429, with `retry_after` when it said how long to
    /// wait: pause new requests that long, at most [`MAX_RETRY_AFTER`],
    /// raise the pace and cut the host's limits. Returns how long the
    /// refused request waits before it is retried: nothing more when the
    /// host said (the pause holds the retry too), else the retry delay.
    pub(crate) fn throttled(&self, retry_after: Option<Duration>) -> Option<Duration> {
        let now = Instant::now();
        self.answers.fetch_add(1, Ordering::Relaxed);
        let wait = retry_after
            .map(|wait| wait.min(MAX_RETRY_AFTER))
            .filter(|wait| !wait.is_zero());
        let (limits, paced, retry_delay) = {
            let mut state = self.state();
            if let Some(wait) = wait {
                let until = now + wait;
                state.paused_until = Some(state.paused_until.map_or(until, |at| at.max(until)));
            }
            self.decay(&mut state, now);
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
            (state.limits.clone(), paced, timing.retry_delay)
        };
        let mut cut = false;
        let mut sum = None;
        for limit in &limits {
            cut |= limit.cut(now);
            let (current, configured) = limit.limits(now);
            let (all_current, all_configured) = sum.unwrap_or((0, 0));
            sum = Some((all_current + current, all_configured + configured));
        }
        let delay = retry_after.is_none().then_some(retry_delay);
        if !cut && wait.is_none() && paced.is_none() {
            return delay;
        }
        {
            let mut state = self.state();
            if state.warned_at.is_some_and(|at| now < at + WARN_INTERVAL) {
                return delay;
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
            "{} answered 429 Too Many Requests: {}",
            self.host,
            detail.join("; ")
        );
        delay
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

/// An HTTP transport whose every request waits for its turn under its
/// host's pace and `Retry-After` before it is sent, and whose every 429 is
/// reported to the host ([`HostThrottle::throttled`]) and handed back after
/// the retry delay, if any. object_store calls it once per attempt, so it
/// sees the attempts object_store retries as well.
#[derive(Debug, Clone)]
pub(crate) struct Throttled<S> {
    pub inner: S,
    pub host: Arc<HostThrottle>,
}

#[async_trait::async_trait]
impl<S: HttpService> HttpService for Throttled<S> {
    async fn call(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        self.host.wait().await;
        let response = self.inner.call(request).await?;
        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let wait = retry_after(response.headers(), SystemTime::now());
            if let Some(delay) = self.host.throttled(wait) {
                tokio::time::sleep(delay).await;
            }
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
