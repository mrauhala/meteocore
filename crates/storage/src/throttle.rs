//! Backing off a throttling HTTP host as a whole (#1235).
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
//! - **The adaptive limit** of a store [`crate::build_http_store_limited`]
//!   built ([`AdaptiveLimit`]): it halves, never below 1, at most once per
//!   [`CUT_WINDOW`], and grows by 1 for each [`QUIET_INTERVAL`] without a
//!   429, back to the configured limit.
//!
//! A 429 without `Retry-After` pauses nothing: it only cuts the limit.
//! S3 stores are not throttled here: S3 answers 503 SlowDown, which
//! object_store retries.

mod limit;
#[cfg(test)]
mod tests;

pub(crate) use limit::{AdaptiveLimit, Limited};

use std::collections::BTreeMap;
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

/// At most one WARN per host per this long, however often it throttles.
const WARN_INTERVAL: Duration = Duration::from_secs(60);

/// What one host asked for with its 429s, shared by every HTTP store on
/// its scheme, host and port ([`for_url`]).
#[derive(Debug)]
pub(crate) struct HostThrottle {
    /// `host[:port]`, as the `storage` span and `/metrics` name it.
    host: String,
    state: Mutex<HostState>,
}

#[derive(Debug, Default)]
struct HostState {
    /// New requests wait until then: the latest `Retry-After`.
    paused_until: Option<Instant>,
    /// The adaptive limits of the limited stores on the host.
    limits: Vec<Arc<AdaptiveLimit>>,
    /// When the last WARN was logged.
    warned_at: Option<Instant>,
}

impl HostThrottle {
    pub(crate) fn new(host: String) -> Self {
        Self {
            host,
            state: Mutex::default(),
        }
    }

    fn state(&self) -> MutexGuard<'_, HostState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Make `limit` one of the host's, cut by its 429s.
    pub(crate) fn add_limit(&self, limit: Arc<AdaptiveLimit>) {
        self.state().limits.push(limit);
    }

    /// Wait until the host's `Retry-After` has passed: at once when it
    /// asked for none. A later 429 can push the time out while this waits.
    pub(crate) async fn wait(&self) {
        loop {
            let until = self.state().paused_until;
            match until {
                Some(until) if until > Instant::now() => tokio::time::sleep_until(until).await,
                _ => return,
            }
        }
    }

    /// The host answered 429, with `retry_after` when it said how long to
    /// wait: pause new requests that long, at most [`MAX_RETRY_AFTER`],
    /// and cut the host's limits.
    pub(crate) fn throttled(&self, retry_after: Option<Duration>) {
        let now = Instant::now();
        let wait = retry_after
            .map(|wait| wait.min(MAX_RETRY_AFTER))
            .filter(|wait| !wait.is_zero());
        let limits = {
            let mut state = self.state();
            if let Some(wait) = wait {
                let until = now + wait;
                state.paused_until = Some(state.paused_until.map_or(until, |at| at.max(until)));
            }
            state.limits.clone()
        };
        let mut cut = false;
        let mut sum = None;
        for limit in &limits {
            cut |= limit.cut(now);
            let (current, configured) = limit.limits(now);
            let (all_current, all_configured) = sum.unwrap_or((0, 0));
            sum = Some((all_current + current, all_configured + configured));
        }
        if !cut && wait.is_none() {
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
        let pause = wait.map(|wait| {
            format!(
                "new requests wait {:.1} s (Retry-After)",
                wait.as_secs_f64()
            )
        });
        let detail: Vec<String> = [limit, pause].into_iter().flatten().collect();
        tracing::warn!(
            host = %self.host,
            limit = sum.map(|(current, _)| current),
            configured_limit = sum.map(|(_, configured)| configured),
            retry_after_secs = wait.map(|wait| wait.as_secs_f64()),
            "{} answered 429 Too Many Requests: {}",
            self.host,
            detail.join("; ")
        );
    }
}

/// Every host's throttle, by scheme, host and port. Held weakly: a
/// throttle lives as long as a store on its host, and the entries of hosts
/// left without one are dropped when another host's throttle is made, so
/// stores built per poll for a feed's origins leave nothing behind.
static HOSTS: LazyLock<Mutex<BTreeMap<String, Weak<HostThrottle>>>> =
    LazyLock::new(Default::default);

/// The throttle of `url`'s scheme, host and port.
pub(crate) fn for_url(url: &url::Url) -> Arc<HostThrottle> {
    let key = format!(
        "{}://{}:{}",
        url.scheme(),
        url.host_str().unwrap_or_default(),
        url.port_or_known_default().unwrap_or_default()
    );
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

/// An HTTP transport whose every request waits for its host's
/// `Retry-After` before it is sent, and whose every 429 is reported to the
/// host ([`HostThrottle::throttled`]). object_store calls it once per
/// attempt, so it sees the attempts object_store retries as well.
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
            self.host
                .throttled(retry_after(response.headers(), SystemTime::now()));
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
