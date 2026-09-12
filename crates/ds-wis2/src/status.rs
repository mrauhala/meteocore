//! Lock-free ingest status shared between the pipeline tasks and the engine
//! that owns them (read by `/health` and `/metrics`).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Why a notification was dropped before reaching the engine. The variant
/// name (lowercase) is the `reason` label of
/// `wis2_messages_dropped_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// Not a parseable WIS2 notification.
    Parse,
    /// Same `data_id` / message id already processed.
    Duplicate,
    /// Download URL rejected by the [`crate::DownloadPolicy`].
    Policy,
    /// Checksum in `properties.integrity` did not match the payload.
    Integrity,
    /// Inline content or download exceeded the size cap.
    Size,
    /// HTTP download failed (network / non-2xx).
    Download,
    /// Inline content could not be decoded (bad base64 / gzip).
    Decode,
}

impl DropReason {
    pub const ALL: [DropReason; 7] = [
        DropReason::Parse,
        DropReason::Duplicate,
        DropReason::Policy,
        DropReason::Integrity,
        DropReason::Size,
        DropReason::Download,
        DropReason::Decode,
    ];

    pub fn label(self) -> &'static str {
        match self {
            DropReason::Parse => "parse",
            DropReason::Duplicate => "duplicate",
            DropReason::Policy => "policy",
            DropReason::Integrity => "integrity",
            DropReason::Size => "size",
            DropReason::Download => "download",
            DropReason::Decode => "decode",
        }
    }

    fn index(self) -> usize {
        match self {
            DropReason::Parse => 0,
            DropReason::Duplicate => 1,
            DropReason::Policy => 2,
            DropReason::Integrity => 3,
            DropReason::Size => 4,
            DropReason::Download => 5,
            DropReason::Decode => 6,
        }
    }
}

/// Counters and connection flags. All fields are monotonic counters or
/// last-value gauges; the engine layer turns them into Prometheus series.
#[derive(Debug, Default)]
pub struct Status {
    connected: AtomicBool,
    subscribed: AtomicBool,
    /// Epoch millis when the connection was last lost (0 = never / connected).
    disconnected_since_millis: AtomicU64,
    /// Epoch millis of the last accepted notification (0 = none yet).
    last_message_millis: AtomicU64,
    /// Epoch millis of the `pubtime` of the last accepted notification.
    last_pubtime_millis: AtomicU64,
    messages_received_total: AtomicU64,
    dropped: [AtomicU64; 7],
    reconnects_total: AtomicU64,
    downloads_total: AtomicU64,
    download_failures_total: AtomicU64,
    /// Integrity methods this build cannot verify (sha3-*), counted so an
    /// operator can see that "verified" is not "checked".
    integrity_unverified_total: AtomicU64,
}

/// Point-in-time copy of [`Status`] for metrics / health.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusSnapshot {
    pub connected: bool,
    pub subscribed: bool,
    /// Seconds since the connection was lost (`None` when connected or never
    /// connected).
    pub disconnected_for_secs: Option<u64>,
    /// Seconds since the last accepted notification (`None` = none yet).
    pub last_message_age_secs: Option<u64>,
    /// `now − pubtime` of the last accepted notification, seconds.
    pub last_lag_secs: Option<u64>,
    pub messages_received_total: u64,
    pub dropped_total: [u64; 7],
    pub reconnects_total: u64,
    pub downloads_total: u64,
    pub download_failures_total: u64,
    pub integrity_unverified_total: u64,
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Status {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a successful CONNACK. Returns `true` on a transition from
    /// disconnected (for log spam control).
    pub fn set_connected(&self) -> bool {
        let was = self.connected.swap(true, Ordering::AcqRel);
        self.disconnected_since_millis.store(0, Ordering::Release);
        !was
    }

    /// Record a lost connection. Returns `true` on a transition.
    pub fn set_disconnected(&self) -> bool {
        let was = self.connected.swap(false, Ordering::AcqRel);
        self.subscribed.store(false, Ordering::Release);
        if was {
            self.disconnected_since_millis
                .store(now_millis().max(1), Ordering::Release);
            self.reconnects_total.fetch_add(1, Ordering::Relaxed);
        } else if self.disconnected_since_millis.load(Ordering::Acquire) == 0 {
            // Never connected yet: start the outage clock at the first
            // failure so `degrade_after_secs` also covers a broker that is
            // unreachable from boot.
            self.disconnected_since_millis
                .store(now_millis().max(1), Ordering::Release);
        }
        was
    }

    pub fn set_subscribed(&self) {
        self.subscribed.store(true, Ordering::Release);
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    pub fn is_subscribed(&self) -> bool {
        self.subscribed.load(Ordering::Acquire)
    }

    pub fn record_received(&self) {
        self.messages_received_total.fetch_add(1, Ordering::Relaxed);
    }

    /// An accepted notification (parsed + not a duplicate) with its pubtime
    /// as epoch millis.
    pub fn record_accepted(&self, pubtime_millis: u64) {
        self.last_message_millis
            .store(now_millis().max(1), Ordering::Release);
        self.last_pubtime_millis
            .store(pubtime_millis, Ordering::Release);
    }

    pub fn record_dropped(&self, reason: DropReason) {
        self.dropped[reason.index()].fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_download(&self, ok: bool) {
        self.downloads_total.fetch_add(1, Ordering::Relaxed);
        if !ok {
            self.download_failures_total.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_integrity_unverified(&self) {
        self.integrity_unverified_total
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> StatusSnapshot {
        let now = now_millis();
        let age = |millis: u64| (millis > 0).then(|| now.saturating_sub(millis) / 1000);
        let connected = self.is_connected();
        let mut dropped_total = [0u64; 7];
        for (i, c) in self.dropped.iter().enumerate() {
            dropped_total[i] = c.load(Ordering::Relaxed);
        }
        StatusSnapshot {
            connected,
            subscribed: self.is_subscribed(),
            disconnected_for_secs: if connected {
                None
            } else {
                age(self.disconnected_since_millis.load(Ordering::Acquire))
            },
            last_message_age_secs: age(self.last_message_millis.load(Ordering::Acquire)),
            last_lag_secs: {
                let last = self.last_message_millis.load(Ordering::Acquire);
                let pt = self.last_pubtime_millis.load(Ordering::Acquire);
                (last > 0).then(|| last.saturating_sub(pt) / 1000)
            },
            messages_received_total: self.messages_received_total.load(Ordering::Relaxed),
            dropped_total,
            reconnects_total: self.reconnects_total.load(Ordering::Relaxed),
            downloads_total: self.downloads_total.load(Ordering::Relaxed),
            download_failures_total: self.download_failures_total.load(Ordering::Relaxed),
            integrity_unverified_total: self.integrity_unverified_total.load(Ordering::Relaxed),
        }
    }
}

impl StatusSnapshot {
    /// Dropped count for one reason.
    pub fn dropped(&self, reason: DropReason) -> u64 {
        self.dropped_total[reason.index()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transitions_and_counters() {
        let s = Status::new();
        let snap = s.snapshot();
        assert!(!snap.connected && !snap.subscribed);
        assert_eq!(snap.disconnected_for_secs, None);
        assert_eq!(snap.last_message_age_secs, None);

        // Unreachable from boot: the outage clock starts at the first failure.
        assert!(!s.set_disconnected());
        assert!(s.snapshot().disconnected_for_secs.is_some());

        assert!(s.set_connected());
        assert!(!s.set_connected());
        s.set_subscribed();
        assert!(s.snapshot().subscribed);
        assert_eq!(s.snapshot().disconnected_for_secs, None);

        s.record_received();
        s.record_accepted(now_millis() - 90_000);
        s.record_dropped(DropReason::Duplicate);
        s.record_dropped(DropReason::Duplicate);
        s.record_download(true);
        s.record_download(false);
        let snap = s.snapshot();
        assert_eq!(snap.messages_received_total, 1);
        assert_eq!(snap.dropped(DropReason::Duplicate), 2);
        assert_eq!(snap.dropped(DropReason::Parse), 0);
        assert_eq!(snap.downloads_total, 2);
        assert_eq!(snap.download_failures_total, 1);
        assert_eq!(snap.last_message_age_secs, Some(0));
        assert!(snap.last_lag_secs.unwrap() >= 89);

        assert!(s.set_disconnected());
        assert!(!s.set_disconnected());
        let snap = s.snapshot();
        assert_eq!(snap.reconnects_total, 1);
        assert!(!snap.subscribed);
        assert_eq!(snap.disconnected_for_secs, Some(0));
    }

    #[test]
    fn drop_reason_labels_are_unique() {
        let mut labels: Vec<&str> = DropReason::ALL.iter().map(|r| r.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), DropReason::ALL.len());
    }
}
