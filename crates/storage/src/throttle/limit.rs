//! The adaptive request limit of a host store (#1235): additive increase,
//! multiplicative decrease over a semaphore.
//!
//! [`AdaptiveLimit`] holds the limit now, `1..=configured`. A 429 from the
//! host halves it, at most once per [`super::CUT_WINDOW`]: the cut forgets
//! the semaphore's free permits at once and owes the rest, which requests
//! in flight pay back as they release theirs, so a cut neither waits for
//! nor cancels them. Each [`super::QUIET_INTERVAL`] without a 429 adds an
//! eighth of the configured limit back, at least one permit, or pays as
//! many owed, up to the configured limit: a limit cut to 1 is back in
//! eight intervals, 4 minutes, whatever it was configured to (#1530).
//! Growth is applied when a permit is taken or released and when the limit
//! is read, so it needs no timer task.
//!
//! An S3 bucket's limit starts below its ceiling and grows to it the same
//! way ([`AdaptiveLimit::starting_at`]), and says when requests wait for a
//! limit at its ceiling ([`AdaptiveLimit::warning_when_full`]).
//!
//! Background work (`throttle::BACKGROUND`, a store's reads
//! `DataStore::for_background` marked) holds or queues for at most half
//! the limit now, rounded down ([`AdaptiveLimit::share`]): none at a limit
//! of 1, so a request never waits behind a poll's read for the only
//! permit. Its reads take their places in the share first come, first
//! served, and one that has waited [`BACKGROUND_PATIENCE`] may hold one
//! permit past it, so background work is never starved. Within the share
//! its reads queue for permits first come, first served with every other
//! read (#1530). After a cut, background reads keep the permits they hold
//! until their reads end, as every read does; new ones wait for the
//! smaller share.
//!
//! [`Limited`] is the store under the limit, as object_store's
//! `LimitStore` is under a fixed one: every operation holds a permit while
//! it runs, a GET until its body has been read or dropped.

use std::collections::BTreeSet;
use std::ops::Range;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::stream::{BoxStream, Stream, StreamExt};
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, GetResultPayload, ListResult, MultipartUpload, ObjectMeta,
    ObjectStore, PutMultipartOptions, PutOptions, PutPayload, PutResult, RenameOptions, Result,
};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

use super::{CUT_WINDOW, QUIET_INTERVAL};

/// Each quiet interval adds this share of the configured limit back:
/// an eighth, so a host configured at 8 grows by 1.
const GROWTH_PARTS: usize = 8;

/// How long a background read waits for its place in the share before it
/// may hold a permit past it: a quarter of its 120 s background budget, so
/// it still has the time to be read when requests keep the share full or a
/// limit of 1 leaves none.
pub(crate) const BACKGROUND_PATIENCE: std::time::Duration = std::time::Duration::from_secs(30);

/// At most one WARN per limit per this long that requests wait for a
/// limit at its ceiling.
const FULL_WARN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(600);

/// The requests a host store allows in flight now.
#[derive(Debug)]
pub(crate) struct AdaptiveLimit {
    /// `host[:port]`, for the line logged when the limit is back.
    host: String,
    /// The throttling answer the host has stopped sending, for that line:
    /// `429`, or an S3 bucket's `SlowDown` ([`super::Kind::quiet_name`]).
    quiet_name: &'static str,
    /// The limit the store was built with, which growth returns to.
    configured: usize,
    /// Whether a request waiting for a permit of the limit at its ceiling
    /// is logged (an S3 bucket's).
    warn_when_full: bool,
    semaphore: Arc<Semaphore>,
    state: Mutex<LimitState>,
    /// Background reads' places in the share, and those waiting for one.
    places: Mutex<Places>,
    /// Told when a background read may take its place in the share: one
    /// left it, or the limit grew.
    freed: Notify,
}

#[derive(Debug)]
struct LimitState {
    /// The limit now, `1..=configured`: the semaphore's free permits plus
    /// those held, less `owed`.
    current: usize,
    /// Permits a cut took from the limit while they were held: the next
    /// ones released are forgotten instead of freed.
    owed: usize,
    /// When the limit was last cut.
    cut_at: Option<Instant>,
    /// Since when the host has sent no 429: its last one, or the last
    /// growth step.
    quiet_since: Instant,
    /// Whether the limit was cut since it was last at its ceiling: its
    /// return is logged then, not after a start below it.
    was_cut: bool,
    /// When a request last waited for a permit of the limit at its
    /// ceiling was logged.
    full_warned_at: Option<Instant>,
}

impl AdaptiveLimit {
    /// A limit of `configured` requests (at least 1) to `host`, cut by
    /// the answers `kind` names.
    pub(crate) fn new(host: String, configured: usize, kind: &super::Kind) -> Self {
        let configured = configured.max(1);
        Self {
            host,
            quiet_name: kind.quiet_name(),
            configured,
            warn_when_full: false,
            semaphore: Arc::new(Semaphore::new(configured)),
            places: Mutex::new(Places::default()),
            freed: Notify::new(),
            state: Mutex::new(LimitState {
                current: configured,
                owed: 0,
                cut_at: None,
                quiet_since: Instant::now(),
                was_cut: false,
                full_warned_at: None,
            }),
        }
    }

    /// This limit, starting at `initial` (at least 1) and growing to its
    /// configured one as a cut limit grows back.
    pub(crate) fn starting_at(self, initial: usize) -> Self {
        let initial = initial.clamp(1, self.configured);
        self.semaphore.forget_permits(self.configured - initial);
        self.state().current = initial;
        self
    }

    /// This limit, logging at most once per [`FULL_WARN_INTERVAL`] that a
    /// request waits for a permit while the limit is at its ceiling: its
    /// readers together ask for more than it allows.
    pub(crate) fn warning_when_full(mut self) -> Self {
        self.warn_when_full = true;
        self
    }

    fn state(&self) -> MutexGuard<'_, LimitState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Wait for a permit, one of the requests the limit allows now, first
    /// come, first served; background work first waits for its place in
    /// the [`Self::share`].
    pub(crate) async fn acquire(self: &Arc<Self>) -> Permit {
        self.grow(&mut self.state(), Instant::now());
        let share = if super::background() {
            Some(self.background_place().await)
        } else {
            None
        };
        let permit = match self.semaphore.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                self.full();
                self.semaphore
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("the semaphore is never closed")
            }
        };
        Permit {
            permit: Some(permit),
            limit: self.clone(),
            _share: share,
        }
    }

    /// The permits background work may hold or queue for now: half the
    /// limit, rounded down, none at a limit of 1.
    pub(crate) fn share(&self) -> usize {
        self.state().current / 2
    }

    fn places(&self) -> MutexGuard<'_, Places> {
        self.places.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A background read's place in the share, in the order the reads
    /// asked: the first waiting takes one while fewer than the share hold
    /// one, or, once it has waited [`BACKGROUND_PATIENCE`], while no more
    /// than the share do.
    async fn background_place(self: &Arc<Self>) -> Share {
        let asked = Instant::now();
        let ticket = {
            let mut places = self.places();
            let id = places.next;
            places.next += 1;
            places.waiting.insert(id);
            PlaceTicket(self, id)
        };
        let patient = asked + BACKGROUND_PATIENCE;
        loop {
            let freed = self.freed.notified();
            tokio::pin!(freed);
            freed.as_mut().enable();
            let share = self.share();
            let now = Instant::now();
            let taken = {
                let mut places = self.places();
                let first = places.waiting.first() == Some(&ticket.1);
                let room = if now >= patient { share + 1 } else { share };
                let take = first && places.held < room;
                if take {
                    places.held += 1;
                }
                take
            };
            if taken {
                drop(ticket);
                return Share(self.clone());
            }
            if now < patient {
                // Told, or patient by then.
                let _ = tokio::time::timeout_at(patient, freed).await;
            } else {
                freed.await;
            }
        }
    }

    /// The host answered 429 at `now`: halve the limit, never below 1,
    /// unless it was cut within [`CUT_WINDOW`]. Whether it was cut.
    pub(crate) fn cut(&self, now: Instant) -> bool {
        let mut state = self.state();
        self.grow(&mut state, now);
        state.quiet_since = now;
        if state.current == 1 || state.cut_at.is_some_and(|at| now < at + CUT_WINDOW) {
            return false;
        }
        let cut_to = (state.current / 2).max(1);
        let removed = state.current - cut_to;
        let forgotten = self.semaphore.forget_permits(removed);
        state.owed += removed - forgotten;
        state.current = cut_to;
        state.cut_at = Some(now);
        state.was_cut = true;
        true
    }

    /// `(current, configured)` at `now`, after the growth the quiet time
    /// since the last 429 has earned.
    pub(crate) fn limits(&self, now: Instant) -> (usize, usize) {
        let mut state = self.state();
        self.grow(&mut state, now);
        (state.current, self.configured)
    }

    /// A request found no free permit: log it when the limit is at its
    /// ceiling and this limit says so, at most once per
    /// [`FULL_WARN_INTERVAL`].
    fn full(&self) {
        if !self.warn_when_full {
            return;
        }
        let now = Instant::now();
        {
            let mut state = self.state();
            if state.current < self.configured
                || state
                    .full_warned_at
                    .is_some_and(|at| now < at + FULL_WARN_INTERVAL)
            {
                return;
            }
            state.full_warned_at = Some(now);
        }
        tracing::warn!(
            host = %self.host,
            limit = self.configured,
            "{}: requests wait for one of its {} request permits: its readers together ask \
             for more at once, such as a MC_COG_TILE_CONCURRENCY above {}, and are held to it",
            self.host,
            self.configured,
            self.configured
        );
    }

    /// One step, an eighth of the configured limit and at least 1, for
    /// each [`QUIET_INTERVAL`] since the last 429 or step, up to the
    /// configured limit; logged once when a cut limit reaches it.
    fn grow(&self, state: &mut LimitState, now: Instant) {
        if state.current == self.configured {
            return;
        }
        let step = self.configured.div_ceil(GROWTH_PARTS);
        let before = state.current;
        while state.current < self.configured
            && now.saturating_duration_since(state.quiet_since) >= QUIET_INTERVAL
        {
            state.quiet_since += QUIET_INTERVAL;
            for _ in 0..step.min(self.configured - state.current) {
                state.current += 1;
                if state.owed > 0 {
                    state.owed -= 1;
                } else {
                    self.semaphore.add_permits(1);
                }
            }
        }
        if state.current > before {
            self.freed.notify_waiters();
        }
        if state.current == self.configured && std::mem::take(&mut state.was_cut) {
            tracing::info!(
                host = %self.host,
                limit = self.configured,
                "{} has answered no {} for {} s: its request limit is back at {}",
                self.host,
                self.quiet_name,
                QUIET_INTERVAL.as_secs(),
                self.configured
            );
        }
    }

    /// A request is done with `permit`: free it, or forget it when a cut
    /// is owed one.
    fn release(&self, permit: OwnedSemaphorePermit) {
        let mut state = self.state();
        if state.owed > 0 {
            state.owed -= 1;
            permit.forget();
        } else {
            drop(permit);
        }
        self.grow(&mut state, Instant::now());
    }
}

/// The background reads' places in the share ([`AdaptiveLimit::share`]).
#[derive(Debug, Default)]
struct Places {
    /// Places held, by reads holding a permit or queued for one.
    held: usize,
    /// The tickets of the reads waiting for a place, in the order they
    /// asked.
    waiting: BTreeSet<u64>,
    /// The next ticket.
    next: u64,
}

/// A background read's place in the queue for the share, given up when it
/// has a place or is dropped, which tells the next.
struct PlaceTicket<'a>(&'a AdaptiveLimit, u64);

impl Drop for PlaceTicket<'_> {
    fn drop(&mut self) {
        self.0.places().waiting.remove(&self.1);
        self.0.freed.notify_waiters();
    }
}

/// A background read's place in [`AdaptiveLimit::share`], left when
/// dropped: with its permit, or when it gives up waiting for one.
struct Share(Arc<AdaptiveLimit>);

impl Drop for Share {
    fn drop(&mut self) {
        let mut places = self.0.places();
        places.held = places.held.saturating_sub(1);
        drop(places);
        self.0.freed.notify_waiters();
    }
}

/// One request let through an [`AdaptiveLimit`], until dropped.
pub(crate) struct Permit {
    /// `None` once released.
    permit: Option<OwnedSemaphorePermit>,
    limit: Arc<AdaptiveLimit>,
    /// A background read's place in the share, left after the permit.
    _share: Option<Share>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        if let Some(permit) = self.permit.take() {
            self.limit.release(permit);
        }
    }
}

/// A stream holding a permit until it is dropped: a GET body or a LIST.
struct Permitted<T> {
    inner: BoxStream<'static, Result<T>>,
    _permit: Permit,
}

impl<T> Stream for Permitted<T> {
    type Item = Result<T>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.poll_next_unpin(cx)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

/// An object store under an [`AdaptiveLimit`]. A multipart upload is
/// passed through unlimited: the HTTP store does not implement one.
#[derive(Debug)]
pub(crate) struct Limited {
    pub inner: Arc<dyn ObjectStore>,
    pub limit: Arc<AdaptiveLimit>,
}

impl Limited {
    /// `open` run under a permit taken when the stream is first polled,
    /// held until it is dropped.
    fn stream<T: Send + 'static>(
        &self,
        open: impl FnOnce(&Arc<dyn ObjectStore>) -> BoxStream<'static, Result<T>> + Send + 'static,
    ) -> BoxStream<'static, Result<T>> {
        let (inner, limit) = (self.inner.clone(), self.limit.clone());
        futures::stream::once(async move {
            let permit = limit.acquire().await;
            Permitted {
                inner: open(&inner),
                _permit: permit,
            }
        })
        .flatten()
        .boxed()
    }
}

impl std::fmt::Display for Limited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "AdaptiveLimit({}, {})",
            self.limit.configured, self.inner
        )
    }
}

#[async_trait::async_trait]
impl ObjectStore for Limited {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> Result<PutResult> {
        let _permit = self.limit.acquire().await;
        self.inner.put_opts(location, payload, options).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        options: PutMultipartOptions,
    ) -> Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, options).await
    }

    async fn get_opts(&self, location: &Path, options: GetOptions) -> Result<GetResult> {
        let permit = self.limit.acquire().await;
        let mut result = self.inner.get_opts(location, options).await?;
        result.payload = match result.payload {
            GetResultPayload::Stream(inner) => GetResultPayload::Stream(
                Permitted {
                    inner,
                    _permit: permit,
                }
                .boxed(),
            ),
            local => local,
        };
        Ok(result)
    }

    async fn get_ranges(&self, location: &Path, ranges: &[Range<u64>]) -> Result<Vec<Bytes>> {
        let _permit = self.limit.acquire().await;
        self.inner.get_ranges(location, ranges).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, Result<Path>>,
    ) -> BoxStream<'static, Result<Path>> {
        self.stream(move |inner| inner.delete_stream(locations))
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> {
        let prefix = prefix.cloned();
        self.stream(move |inner| inner.list(prefix.as_ref()))
    }

    fn list_with_offset(
        &self,
        prefix: Option<&Path>,
        offset: &Path,
    ) -> BoxStream<'static, Result<ObjectMeta>> {
        let (prefix, offset) = (prefix.cloned(), offset.clone());
        self.stream(move |inner| inner.list_with_offset(prefix.as_ref(), &offset))
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> Result<ListResult> {
        let _permit = self.limit.acquire().await;
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(&self, from: &Path, to: &Path, options: CopyOptions) -> Result<()> {
        let _permit = self.limit.acquire().await;
        self.inner.copy_opts(from, to, options).await
    }

    async fn rename_opts(&self, from: &Path, to: &Path, options: RenameOptions) -> Result<()> {
        let _permit = self.limit.acquire().await;
        self.inner.rename_opts(from, to, options).await
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// The free permits of `limit`.
    fn free(limit: &AdaptiveLimit) -> usize {
        limit.semaphore.available_permits()
    }

    /// A cut takes the free permits at once and owes the held ones, which
    /// are forgotten as they are released; quiet time pays the debt before
    /// it frees new permits, and stops at the configured limit.
    #[tokio::test(start_paused = true)]
    async fn a_cut_owes_held_permits_and_growth_pays_them_first() {
        let limit = Arc::new(AdaptiveLimit::new(
            "host.test".into(),
            8,
            &super::super::Kind::Http,
        ));
        let held: Vec<Permit> = futures::future::join_all((0..6).map(|_| limit.acquire())).await;
        assert_eq!(free(&limit), 2);

        assert!(limit.cut(Instant::now()));
        assert_eq!(limit.limits(Instant::now()), (4, 8));
        assert_eq!(free(&limit), 0, "the free permits are taken at once");
        assert_eq!(limit.state().owed, 2, "and two held ones are owed");
        assert!(!limit.cut(Instant::now()), "one cut per window");

        tokio::time::advance(QUIET_INTERVAL).await;
        assert_eq!(limit.limits(Instant::now()), (5, 8));
        assert_eq!(limit.state().owed, 1, "growth pays a debt first");
        drop(held);
        assert_eq!(free(&limit), 5, "the last owed permit was forgotten");

        tokio::time::advance(QUIET_INTERVAL * 10).await;
        assert_eq!(limit.limits(Instant::now()), (8, 8));
        assert_eq!(free(&limit), 8);
        assert_eq!(limit.state().owed, 0);

        // Never below 1.
        for step in 1..=4 {
            limit.cut(Instant::now() + CUT_WINDOW * step);
        }
        assert_eq!(limit.limits(Instant::now() + CUT_WINDOW * 4), (1, 8));
        assert_eq!(free(&limit), 1);
        let _one = limit.acquire().await;
        let second = tokio::time::timeout(Duration::from_millis(10), limit.acquire()).await;
        assert!(second.is_err(), "a limit of 1 admits one");
    }
}
