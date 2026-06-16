//! Per-engine live health state (#110).
//!
//! A 30 s `SELECT 1` ping (run by [`crate::engine::PostgisEngine::poll_loop`])
//! flips the collection between `Ready` and `Degraded` so `/health` reflects
//! current DB reachability instead of just the boot-time outcome. `Failed`
//! (couldn't construct the engine at all) is NOT represented here — a failed
//! collection has no engine; the server's boot health snapshot owns that case.
//!
//! The same struct carries the cheap counters the `/metrics` endpoint scrapes
//! (ping failures, metadata-refresh totals/failures, last-refresh duration).
//! Reads are lock-free atomics so both the ping loop and the `/health` and
//! `/metrics` handlers touch it without coordination.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// Live status of a PostGIS collection. Maps to `server::admin::CollectionStatus`
/// (`Ready`/`Degraded`) at the handler boundary — engine-postgis can't depend on
/// the server crate, so it keeps its own two-state enum.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HealthStatus {
    Ready,
    Degraded,
}

/// Lock-free live health + metrics counters for one collection.
#[derive(Debug)]
pub struct Health {
    /// DB reachable per the most recent `SELECT 1` ping. `/health` is `Ready`
    /// iff this is true (seeded `true`; the first ping runs within ~2 s of boot).
    db_reachable: AtomicBool,
    ping_failures: AtomicU64,
    refresh_total: AtomicU64,
    refresh_failures: AtomicU64,
    last_refresh_millis: AtomicU64,
}

impl Default for Health {
    fn default() -> Self {
        Self {
            db_reachable: AtomicBool::new(true),
            ping_failures: AtomicU64::new(0),
            refresh_total: AtomicU64::new(0),
            refresh_failures: AtomicU64::new(0),
            last_refresh_millis: AtomicU64::new(0),
        }
    }
}

impl Health {
    pub fn new() -> Self {
        Self::default()
    }

    /// Current status — `Ready` iff the last ping reached the DB.
    pub fn status(&self) -> HealthStatus {
        if self.db_reachable.load(Ordering::Acquire) {
            HealthStatus::Ready
        } else {
            HealthStatus::Degraded
        }
    }

    /// Record a `SELECT 1` ping outcome (the `/health` authority).
    pub fn record_ping(&self, ok: bool) {
        if !ok {
            self.ping_failures.fetch_add(1, Ordering::Relaxed);
        }
        self.db_reachable.store(ok, Ordering::Release);
    }

    /// Record a metadata-refresh outcome + duration (metrics only — a refresh
    /// failure keeps the last good snapshot and does NOT flip `/health`; the
    /// ping is the reachability authority and `refresh_failures` is observable).
    pub fn record_refresh(&self, ok: bool, dur: Duration) {
        self.refresh_total.fetch_add(1, Ordering::Relaxed);
        if !ok {
            self.refresh_failures.fetch_add(1, Ordering::Relaxed);
        }
        self.last_refresh_millis.store(
            dur.as_millis().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
    }

    /// Snapshot for the `/metrics` scrape.
    pub fn snapshot(&self) -> HealthSnapshot {
        HealthSnapshot {
            status: self.status(),
            ping_failures: self.ping_failures.load(Ordering::Relaxed),
            refresh_total: self.refresh_total.load(Ordering::Relaxed),
            refresh_failures: self.refresh_failures.load(Ordering::Relaxed),
            last_refresh_secs: self.last_refresh_millis.load(Ordering::Relaxed) as f64 / 1000.0,
        }
    }
}

/// Point-in-time view of [`Health`] for the `/metrics` handler.
#[derive(Clone, Copy, Debug)]
pub struct HealthSnapshot {
    pub status: HealthStatus,
    pub ping_failures: u64,
    pub refresh_total: u64,
    pub refresh_failures: u64,
    pub last_refresh_secs: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_ready() {
        assert_eq!(Health::new().status(), HealthStatus::Ready);
    }

    #[test]
    fn ping_flips_status_and_counts_failures() {
        let h = Health::new();
        h.record_ping(false);
        assert_eq!(h.status(), HealthStatus::Degraded);
        h.record_ping(false);
        h.record_ping(true);
        assert_eq!(h.status(), HealthStatus::Ready);
        assert_eq!(h.snapshot().ping_failures, 2); // only failures counted
    }

    #[test]
    fn refresh_records_metrics_but_not_status() {
        let h = Health::new();
        h.record_refresh(false, Duration::from_millis(1500));
        // A refresh failure must NOT degrade /health (ping is the authority).
        assert_eq!(h.status(), HealthStatus::Ready);
        let s = h.snapshot();
        assert_eq!(s.refresh_total, 1);
        assert_eq!(s.refresh_failures, 1);
        assert!((s.last_refresh_secs - 1.5).abs() < 1e-9);
    }
}
