//! Lock-free ingest health + counters (the postgis `Health` shape).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ds_core::health::LiveStatus;

#[derive(Debug, Default)]
pub struct Health {
    /// Set once the first scan / first ingested report has happened, so
    /// `live_status()` stays `None` (boot snapshot stands) until then.
    probed: AtomicBool,
    /// `Local` mode: consecutive failed scans.
    scan_failures_in_a_row: AtomicU64,
    pub scans_total: AtomicU64,
    pub scan_failures_total: AtomicU64,
    pub files_total: AtomicU64,
    pub reports_ingested_total: AtomicU64,
    pub reports_replaced_total: AtomicU64,
    pub reports_out_of_window_total: AtomicU64,
    pub subsets_skipped_total: AtomicU64,
    pub decode_failures_total: AtomicU64,
    pub decode_unsupported_total: AtomicU64,
}

/// Consecutive scan failures before a `Local` collection degrades.
pub const DEGRADE_AFTER_SCAN_FAILURES: u64 = 3;

impl Health {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_scan(&self, ok: bool) {
        self.scans_total.fetch_add(1, Ordering::Relaxed);
        if ok {
            self.scan_failures_in_a_row.store(0, Ordering::Relaxed);
        } else {
            self.scan_failures_total.fetch_add(1, Ordering::Relaxed);
            self.scan_failures_in_a_row.fetch_add(1, Ordering::Relaxed);
        }
        self.probed.store(true, Ordering::Release);
    }

    pub fn mark_probed(&self) {
        self.probed.store(true, Ordering::Release);
    }

    pub fn is_probed(&self) -> bool {
        self.probed.load(Ordering::Acquire)
    }

    /// `Local` mode status: degraded after repeated scan failures.
    pub fn local_status(&self) -> Option<LiveStatus> {
        if !self.is_probed() {
            return None;
        }
        Some(
            if self.scan_failures_in_a_row.load(Ordering::Relaxed) >= DEGRADE_AFTER_SCAN_FAILURES {
                LiveStatus::Degraded {
                    reason: "BUFR source scans failing",
                }
            } else {
                LiveStatus::Ready
            },
        )
    }

    /// Monotonic counters in a fixed order (see `admin.rs` metrics):
    /// scans, scan failures, files, ingested, replaced, out of window,
    /// subsets skipped, decode failures, decode unsupported.
    pub fn counters(&self) -> [u64; 9] {
        [
            self.scans_total.load(Ordering::Relaxed),
            self.scan_failures_total.load(Ordering::Relaxed),
            self.files_total.load(Ordering::Relaxed),
            self.reports_ingested_total.load(Ordering::Relaxed),
            self.reports_replaced_total.load(Ordering::Relaxed),
            self.reports_out_of_window_total.load(Ordering::Relaxed),
            self.subsets_skipped_total.load(Ordering::Relaxed),
            self.decode_failures_total.load(Ordering::Relaxed),
            self.decode_unsupported_total.load(Ordering::Relaxed),
        ]
    }
}
