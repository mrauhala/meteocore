//! Duplicate suppression.
//!
//! The Global Brokers relay every notification once per Global Cache that
//! mirrored the object (six copies observed in 2026), and a consumer that
//! subscribes to both `origin/` and `cache/` sees the producer's copy too. The
//! WIS2 Guide's rule: identical `data_id` ⇒ same object, keep the message with
//! the latest `pubtime`, remember what you processed for at least an hour.
//!
//! Two keys are tracked: the message `id` (exact re-delivery, e.g. a QoS-1
//! redelivery after a reconnect) and `data_id` → newest `pubtime` seen.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};

use crate::Notification;

/// Hard cap on remembered `data_id`s regardless of the window — bounds memory
/// on a firehose subscription (`cache/a/wis2/+/…`) at roughly 20 MB.
pub const MAX_ENTRIES: usize = 200_000;

#[derive(Debug)]
pub struct Dedup {
    window: Duration,
    /// data_id → newest pubtime accepted + the sequence number of its
    /// current expiry record.
    by_data_id: HashMap<String, (DateTime<Utc>, u64)>,
    /// Insertion order by *receipt* time for window pruning (a data_id is
    /// re-queued with a fresh sequence number when a newer pubtime replaces
    /// it; the stale record is then ignored when it expires).
    expiry: BTreeMap<(DateTime<Utc>, u64), String>,
    seq: u64,
    ids: HashSet<String>,
    id_expiry: BTreeMap<(DateTime<Utc>, u64), String>,
}

/// Why [`Dedup::accept`] refused a notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Accept,
    /// Same message `id` already processed (broker redelivery).
    DuplicateId,
    /// Same `data_id` with an equal or newer `pubtime` already processed
    /// (another Global Cache's copy, or an out-of-order older revision).
    DuplicateData,
}

impl Dedup {
    pub fn new(window: Duration) -> Self {
        Dedup {
            window,
            by_data_id: HashMap::new(),
            expiry: BTreeMap::new(),
            seq: 0,
            ids: HashSet::new(),
            id_expiry: BTreeMap::new(),
        }
    }

    /// Decide whether `n` is new. Accepting records it. `now` is the receipt
    /// time (injectable for tests).
    pub fn accept_at(&mut self, n: &Notification, now: DateTime<Utc>) -> Verdict {
        self.prune(now);
        if self.ids.contains(&n.id) {
            return Verdict::DuplicateId;
        }
        if let Some((seen, _)) = self.by_data_id.get(&n.data_id) {
            if *seen >= n.pubtime {
                return Verdict::DuplicateData;
            }
        }
        self.seq = self.seq.wrapping_add(1);
        self.ids.insert(n.id.clone());
        self.id_expiry.insert((now, self.seq), n.id.clone());
        self.by_data_id
            .insert(n.data_id.clone(), (n.pubtime, self.seq));
        self.expiry.insert((now, self.seq), n.data_id.clone());
        while self.by_data_id.len() > MAX_ENTRIES {
            let Some(((_, seq), oldest)) = self.expiry.pop_first() else {
                break;
            };
            self.remove_if_current(&oldest, seq);
        }
        while self.ids.len() > MAX_ENTRIES {
            let Some((_, id)) = self.id_expiry.pop_first() else {
                break;
            };
            self.ids.remove(&id);
        }
        Verdict::Accept
    }

    pub fn accept(&mut self, n: &Notification) -> Verdict {
        self.accept_at(n, Utc::now())
    }

    /// Number of `data_id`s currently remembered.
    pub fn len(&self) -> usize {
        self.by_data_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_data_id.is_empty()
    }

    /// Drop `data_id` only if `seq` is its current record — a later re-queue
    /// (newer pubtime) owns the entry and keeps it alive.
    fn remove_if_current(&mut self, data_id: &str, seq: u64) {
        if self.by_data_id.get(data_id).map(|e| e.1) == Some(seq) {
            self.by_data_id.remove(data_id);
        }
    }

    fn prune(&mut self, now: DateTime<Utc>) {
        let cutoff = now - self.window;
        while let Some(entry) = self.expiry.first_entry() {
            if entry.key().0 >= cutoff {
                break;
            }
            let seq = entry.key().1;
            let data_id = entry.remove();
            self.remove_if_current(&data_id, seq);
        }
        while let Some(entry) = self.id_expiry.first_entry() {
            if entry.key().0 >= cutoff {
                break;
            }
            let id = entry.remove();
            self.ids.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn n(id: &str, data_id: &str, pub_secs: i64) -> Notification {
        Notification {
            topic: "cache/a/wis2/x/data".into(),
            centre_id: Some("x".into()),
            id: id.into(),
            data_id: data_id.into(),
            pubtime: Utc.timestamp_opt(1_700_000_000 + pub_secs, 0).unwrap(),
            datetime: None,
            start_datetime: None,
            end_datetime: None,
            geometry: None,
            integrity: None,
            content: None,
            links: vec![],
            metadata_id: None,
            wigos_station_identifier: None,
            global_cache: None,
            extra: Default::default(),
        }
    }

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000 + secs, 0).unwrap()
    }

    #[test]
    fn six_cache_copies_collapse_to_one_and_newer_pubtime_wins() {
        let mut d = Dedup::new(Duration::hours(1));
        assert_eq!(d.accept_at(&n("m1", "d1", 0), at(0)), Verdict::Accept);
        for i in 2..=6 {
            assert_eq!(
                d.accept_at(&n(&format!("m{i}"), "d1", 0), at(i)),
                Verdict::DuplicateData
            );
        }
        // Same message id again = redelivery.
        assert_eq!(d.accept_at(&n("m1", "d1", 0), at(7)), Verdict::DuplicateId);
        // Older revision arriving late is dropped; a newer one is accepted.
        assert_eq!(
            d.accept_at(&n("m7", "d1", -5), at(8)),
            Verdict::DuplicateData
        );
        assert_eq!(d.accept_at(&n("m8", "d1", 30), at(9)), Verdict::Accept);
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn window_expiry_forgets_old_entries() {
        let mut d = Dedup::new(Duration::minutes(10));
        assert_eq!(d.accept_at(&n("a", "d1", 0), at(0)), Verdict::Accept);
        assert_eq!(
            d.accept_at(&n("b", "d1", 0), at(60)),
            Verdict::DuplicateData
        );
        // After the window the same data_id/pubtime is new again.
        assert_eq!(d.accept_at(&n("c", "d1", 0), at(700)), Verdict::Accept);
        assert_eq!(d.accept_at(&n("a", "d2", 0), at(701)), Verdict::Accept);
    }

    #[test]
    fn requeued_data_id_survives_first_expiry_record() {
        let mut d = Dedup::new(Duration::minutes(10));
        d.accept_at(&n("a", "d1", 0), at(0));
        d.accept_at(&n("b", "d1", 100), at(300)); // newer revision re-queues d1
                                                  // First record (t=0) expires at t=601 but d1 must still be known.
        assert_eq!(
            d.accept_at(&n("c", "d1", 100), at(650)),
            Verdict::DuplicateData
        );
    }

    #[test]
    fn hard_cap_evicts_oldest() {
        let mut d = Dedup::new(Duration::hours(24));
        // Same receipt second for all (the window never prunes); the
        // sequence number orders them.
        for i in 0..(MAX_ENTRIES + 10) {
            d.accept_at(&n(&format!("m{i}"), &format!("d{i}"), 0), at(0));
        }
        assert_eq!(d.len(), MAX_ENTRIES);
        // d0..d9 were evicted → d0 accepted again (which evicts d10); d11 is
        // still known.
        assert_eq!(d.accept_at(&n("again", "d0", 0), at(1)), Verdict::Accept);
        assert_eq!(
            d.accept_at(&n("again2", "d11", 0), at(1)),
            Verdict::DuplicateData
        );
    }
}
