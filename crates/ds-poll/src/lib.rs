//! Shared poll-loop lifecycle for engine background tasks (#481).
//!
//! Every engine runs the same loop on the background poll runtime: tick on
//! an interval, re-scan the source, exit when the server (reload or process
//! shutdown) says stop. Before this crate each engine hand-rolled the loop
//! and the shutdown signal, and the signal had drifted three ways
//! (`watch::Sender<()>`, `watch::Sender<bool>`, `AtomicBool` + `Notify`) —
//! each copy carrying its own handling of the fired-before-the-loop-started
//! race (#442 was a missed reload-path spawn of exactly this lifecycle).
//!
//! [`Shutdown`] is the one shutdown handle. It is the edge-triggered
//! `AtomicBool` + `Notify` design: unlike a `watch` channel, the signal
//! cannot be lost — `watch::Sender::send` returns `Err` and does **not**
//! bump the channel version when no receiver exists yet, so a `shutdown()`
//! that fires before the spawned loop subscribes would strand the loop
//! forever (the race the CAP retained-receiver and ODIM top-of-loop-recheck
//! workarounds each papered over locally). A set-once flag is observed by
//! every later check regardless of timing.
//!
//! The standard engine loop becomes:
//!
//! ```ignore
//! pub async fn poll_loop(&self) {
//!     let mut ticker = self.shutdown.ticker(self.poll_interval, FirstTick::Skip);
//!     while ticker.tick().await {
//!         self.poll_once();
//!     }
//!     tracing::info!("[{}] poll loop shutting down", self.collection_id);
//! }
//! ```
//!
//! Loops whose delay varies per iteration (failure backoff) use
//! [`Shutdown::sleep`] instead; loops with several cadences in one task
//! (ping + refresh) keep their own `tokio::select!` over [`Shutdown::wait`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::MissedTickBehavior;

/// Edge-triggered, set-once shutdown signal shared between an engine and its
/// background poll loop.
///
/// `shutdown()` may fire at any point relative to the loop — before the loop
/// task was even spawned, while it is waiting for a tick, or while it is in
/// the middle of a work cycle — and the loop exits promptly in every case.
#[derive(Debug, Default)]
pub struct Shutdown {
    fired: AtomicBool,
    notify: Notify,
}

impl Shutdown {
    pub fn new() -> Self {
        Self::default()
    }

    /// Signal shutdown. Idempotent — the first call sets the flag and wakes
    /// any waiter; later calls are no-ops. Safe to call before the poll loop
    /// has started: the flag persists and the loop exits on its first tick.
    pub fn shutdown(&self) {
        if !self.fired.swap(true, Ordering::Release) {
            self.notify.notify_waiters();
        }
    }

    /// Whether shutdown has been signalled.
    pub fn is_shutdown(&self) -> bool {
        self.fired.load(Ordering::Acquire)
    }

    /// Resolve when shutdown is signalled (immediately if it already was).
    pub async fn wait(&self) {
        if self.is_shutdown() {
            return;
        }
        let notified = self.notify.notified();
        tokio::pin!(notified);
        // Register interest *before* re-checking: `notify_waiters()` only
        // wakes already-registered waiters, so enable-then-check closes the
        // flag-set-between-check-and-await race.
        notified.as_mut().enable();
        if self.is_shutdown() {
            return;
        }
        notified.await;
    }

    /// A tick source for the standard fixed-cadence poll loop:
    /// `while ticker.tick().await { self.poll_once(); }`.
    ///
    /// Missed ticks are skipped, never replayed as a burst — a poll cycle
    /// can outlast the period (S3 scan, big grids) and must not be followed
    /// by a rapid catch-up volley (#443).
    pub fn ticker(&self, period: Duration, first: FirstTick) -> PollTicker<'_> {
        let mut interval = tokio::time::interval(period);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        if matches!(first, FirstTick::Skip) {
            // First tick one full period from now instead of immediately —
            // for engines whose constructor already did the initial load.
            interval.reset();
        }
        PollTicker {
            shutdown: self,
            interval,
        }
    }

    /// Interruptible sleep for loops whose delay varies per iteration (e.g.
    /// exponential failure backoff). Returns `true` when the full duration
    /// elapsed (run the work), `false` when shutdown fired (exit the loop).
    pub async fn sleep(&self, duration: Duration) -> bool {
        tokio::select! {
            biased;
            _ = self.wait() => false,
            _ = tokio::time::sleep(duration) => !self.is_shutdown(),
        }
    }
}

/// Whether a [`PollTicker`]'s first tick completes immediately or one full
/// period after creation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirstTick {
    /// First tick completes at once — the loop's first work cycle runs
    /// immediately (e.g. nowcast generating its first frame).
    Immediate,
    /// First tick fires one period after creation — for engines whose
    /// constructor already loaded/scanned at boot.
    Skip,
}

/// Fixed-cadence tick source bound to a [`Shutdown`]; see
/// [`Shutdown::ticker`].
pub struct PollTicker<'a> {
    shutdown: &'a Shutdown,
    interval: tokio::time::Interval,
}

impl PollTicker<'_> {
    /// Wait for the next tick; `false` means shutdown fired and the loop
    /// must exit. Shutdown wins over a simultaneously-ready tick
    /// (`biased`), and is re-checked after the tick so no extra work cycle
    /// runs on the way out.
    pub async fn tick(&mut self) -> bool {
        tokio::select! {
            biased;
            _ = self.shutdown.wait() => false,
            _ = self.interval.tick() => !self.shutdown.is_shutdown(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::time::Instant;

    /// `shutdown()` before the loop ever starts must still stop the loop on
    /// its first tick. This is the race that motivated the `AtomicBool`
    /// design (ported from the ODIM engine's synthetic
    /// `shutdown_before_poll_loop_takes_effect` test): a `watch`-based
    /// signal fired before `subscribe()` is silently dropped.
    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_before_loop_exits_immediately() {
        let shutdown = Shutdown::new();
        shutdown.shutdown();

        // Hour-long period so a buggy flag check would hit the timeout
        // below instead of being rescued by the timer.
        let mut ticker = shutdown.ticker(Duration::from_secs(3600), FirstTick::Skip);
        let first = tokio::time::timeout(Duration::from_secs(1), ticker.tick())
            .await
            .expect("tick must resolve immediately when shutdown pre-fired");
        assert!(!first, "pre-fired shutdown must stop the loop");
    }

    /// `shutdown()` from another task while the loop is parked in `tick()`
    /// wakes it promptly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_wakes_waiting_tick() {
        let shutdown = Arc::new(Shutdown::new());
        let s = shutdown.clone();
        let waiter = tokio::spawn(async move {
            let mut ticker = s.ticker(Duration::from_secs(3600), FirstTick::Skip);
            ticker.tick().await
        });
        // Give the waiter a chance to park before firing.
        tokio::time::sleep(Duration::from_millis(50)).await;
        shutdown.shutdown();
        let got = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("shutdown must wake the parked tick")
            .unwrap();
        assert!(!got);
    }

    #[tokio::test(start_paused = true)]
    async fn first_tick_immediate_vs_skip() {
        let shutdown = Shutdown::new();

        let start = Instant::now();
        let mut immediate = shutdown.ticker(Duration::from_secs(60), FirstTick::Immediate);
        assert!(immediate.tick().await);
        assert_eq!(start.elapsed(), Duration::ZERO, "Immediate ticks at once");

        let start = Instant::now();
        let mut skip = shutdown.ticker(Duration::from_secs(60), FirstTick::Skip);
        assert!(skip.tick().await);
        assert_eq!(
            start.elapsed(),
            Duration::from_secs(60),
            "Skip's first tick fires one full period in"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn ticker_holds_cadence() {
        let shutdown = Shutdown::new();
        let mut ticker = shutdown.ticker(Duration::from_secs(30), FirstTick::Skip);
        let start = Instant::now();
        assert!(ticker.tick().await);
        assert!(ticker.tick().await);
        assert_eq!(start.elapsed(), Duration::from_secs(60));
    }

    #[tokio::test(start_paused = true)]
    async fn sleep_runs_full_duration_without_shutdown() {
        let shutdown = Shutdown::new();
        let start = Instant::now();
        assert!(shutdown.sleep(Duration::from_secs(300)).await);
        assert_eq!(start.elapsed(), Duration::from_secs(300));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sleep_interrupted_by_shutdown() {
        let shutdown = Arc::new(Shutdown::new());
        let s = shutdown.clone();
        let sleeper = tokio::spawn(async move { s.sleep(Duration::from_secs(3600)).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        shutdown.shutdown();
        let got = tokio::time::timeout(Duration::from_secs(1), sleeper)
            .await
            .expect("shutdown must interrupt the sleep")
            .unwrap();
        assert!(!got);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_is_idempotent_and_observable() {
        let shutdown = Shutdown::new();
        assert!(!shutdown.is_shutdown());
        shutdown.shutdown();
        shutdown.shutdown(); // second call is a no-op, not a panic
        assert!(shutdown.is_shutdown());
        // wait() resolves immediately once fired.
        tokio::time::timeout(Duration::from_secs(1), shutdown.wait())
            .await
            .expect("wait() must resolve immediately after shutdown");
    }
}
