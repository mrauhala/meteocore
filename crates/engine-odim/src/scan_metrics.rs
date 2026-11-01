//! What a polar-volume source's scans do (#193): atomics the scan bumps as
//! it goes, read by `/metrics` at scrape time through
//! [`crate::PolarVolumeEngine::scan_metrics`].

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// A [`crate::PolarVolumeEngine`]'s scans since it was built, its
/// construction scan included, and the catalog the last good one left.
/// The counts are cumulative: the server turns them into Prometheus
/// counters, rebaselining when a reload rebuilds the engine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanMetrics {
    /// Scans that built a catalog.
    pub scans_ok: u64,
    /// Scans that could not list the source. The catalog before them is
    /// kept.
    pub scans_failed: u64,
    /// How long the last scan took, failed or not: listing, downloads,
    /// parses, the pixel pre-warm and the cell sets built from the bytes in
    /// hand.
    pub last_scan: Duration,
    /// Volumes a scan took from the parse cache, reading nothing: its hits.
    pub cached: u64,
    /// Volumes a scan read and parsed. With the two failures below, the
    /// parse cache's misses: new volumes, and volumes it no longer held,
    /// such as those `max_files` trims, which every scan reads again. Each
    /// miss is counted once its outcome is known.
    pub parsed: u64,
    /// Volumes whose read failed: a local read error or a failed download.
    /// The next scan reads them again.
    pub read_failures: u64,
    /// Volumes read whose metadata did not parse. Not remembered, so the
    /// next scan reads them again.
    pub parse_failures: u64,
    /// Volumes the catalog holds. The parse cache holds exactly these, so
    /// this is also its entry count; their heap is in
    /// [`crate::PolarVolumeEngine::catalog_bytes`].
    pub volumes: u64,
    /// Radar sites the catalog holds.
    pub sites: u64,
}

/// The counters behind [`ScanMetrics`], shared by every scan of one engine.
#[derive(Debug, Default)]
pub(crate) struct ScanCounts {
    scans_ok: AtomicU64,
    scans_failed: AtomicU64,
    last_scan_micros: AtomicU64,
    cached: AtomicU64,
    parsed: AtomicU64,
    read_failures: AtomicU64,
    parse_failures: AtomicU64,
}

impl ScanCounts {
    /// A scan finished after `took`, with a catalog when `ok`.
    pub(crate) fn scanned(&self, took: Duration, ok: bool) {
        let micros = u64::try_from(took.as_micros()).unwrap_or(u64::MAX);
        self.last_scan_micros.store(micros, Ordering::Relaxed);
        let count = if ok {
            &self.scans_ok
        } else {
            &self.scans_failed
        };
        count.fetch_add(1, Ordering::Relaxed);
    }

    /// A volume came from the parse cache.
    pub(crate) fn cached(&self) {
        self.cached.fetch_add(1, Ordering::Relaxed);
    }

    /// A volume was read and parsed.
    pub(crate) fn parsed(&self) {
        self.parsed.fetch_add(1, Ordering::Relaxed);
    }

    /// `files` reads failed.
    pub(crate) fn read_failed(&self, files: usize) {
        self.read_failures
            .fetch_add(files as u64, Ordering::Relaxed);
    }

    /// A volume read did not parse.
    pub(crate) fn parse_failed(&self) {
        self.parse_failures.fetch_add(1, Ordering::Relaxed);
    }

    /// The counts, with the catalog's `volumes` and `sites`.
    pub(crate) fn snapshot(&self, volumes: usize, sites: usize) -> ScanMetrics {
        let load = |count: &AtomicU64| count.load(Ordering::Relaxed);
        ScanMetrics {
            scans_ok: load(&self.scans_ok),
            scans_failed: load(&self.scans_failed),
            last_scan: Duration::from_micros(load(&self.last_scan_micros)),
            cached: load(&self.cached),
            parsed: load(&self.parsed),
            read_failures: load(&self.read_failures),
            parse_failures: load(&self.parse_failures),
            volumes: volumes as u64,
            sites: sites as u64,
        }
    }
}
