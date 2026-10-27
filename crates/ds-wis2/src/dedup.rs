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
use ds_core::heap::{btree, hash_table};

use crate::Notification;

/// Hard cap on remembered `data_id`s regardless of the window — bounds memory
/// on a firehose subscription (`cache/a/wis2/+/…`): at the cap, with
/// ~100-character `data_id`s and uuid message ids, about 130 MB
/// ([`Dedup::heap_bytes`]).
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
    /// Capacity of every string the four collections hold, kept as they
    /// change: the variable part of [`Self::heap_bytes`].
    string_bytes: usize,
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
            string_bytes: 0,
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
        let key = (now, self.seq);
        let id = self.held(&n.id);
        self.ids.insert(id);
        let id = self.held(&n.id);
        let replaced = self.id_expiry.insert(key, id);
        self.track_replaced(replaced);
        match self.by_data_id.get_mut(&n.data_id) {
            // The map keeps its own key: only the value changes.
            Some(entry) => *entry = (n.pubtime, self.seq),
            None => {
                let data_id = self.held(&n.data_id);
                self.by_data_id.insert(data_id, (n.pubtime, self.seq));
            }
        }
        let data_id = self.held(&n.data_id);
        let replaced = self.expiry.insert(key, data_id);
        self.track_replaced(replaced);
        while self.by_data_id.len() > MAX_ENTRIES {
            let Some(((_, seq), oldest)) = self.expiry.pop_first() else {
                break;
            };
            self.string_bytes -= oldest.capacity();
            self.remove_if_current(&oldest, seq);
        }
        while self.ids.len() > MAX_ENTRIES {
            let Some((_, id)) = self.id_expiry.pop_first() else {
                break;
            };
            self.string_bytes -= id.capacity();
            self.remove_id(&id);
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

    /// Estimated heap bytes of everything remembered: the strings (each
    /// `data_id` and message id is held twice, in its map and its expiry
    /// record), the two hash tables and the two expiry B-trees. O(1).
    pub fn heap_bytes(&self) -> u64 {
        let bytes = self.string_bytes
            + hash_table::<(String, (DateTime<Utc>, u64))>(self.by_data_id.capacity())
            + hash_table::<String>(self.ids.capacity())
            + btree::<(DateTime<Utc>, u64), String>(self.expiry.len())
            + btree::<(DateTime<Utc>, u64), String>(self.id_expiry.len());
        bytes as u64
    }

    /// A copy of `s` to hold, counted in [`Self::heap_bytes`].
    fn held(&mut self, s: &str) -> String {
        let held = s.to_owned();
        self.string_bytes += held.capacity();
        held
    }

    /// Account for the string an expiry insert replaced (a sequence number
    /// that wrapped onto a live record).
    fn track_replaced(&mut self, replaced: Option<String>) {
        if let Some(old) = replaced {
            self.string_bytes -= old.capacity();
        }
    }

    fn remove_id(&mut self, id: &str) {
        if let Some(removed) = self.ids.take(id) {
            self.string_bytes -= removed.capacity();
        }
    }

    /// Drop `data_id` only if `seq` is its current record — a later re-queue
    /// (newer pubtime) owns the entry and keeps it alive.
    fn remove_if_current(&mut self, data_id: &str, seq: u64) {
        if self.by_data_id.get(data_id).map(|e| e.1) == Some(seq) {
            if let Some((key, _)) = self.by_data_id.remove_entry(data_id) {
                self.string_bytes -= key.capacity();
            }
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
            self.string_bytes -= data_id.capacity();
            self.remove_if_current(&data_id, seq);
        }
        while let Some(entry) = self.id_expiry.first_entry() {
            if entry.key().0 >= cutoff {
                break;
            }
            let id = entry.remove();
            self.string_bytes -= id.capacity();
            self.remove_id(&id);
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

    /// The strings counted follow every insert, re-queue and expiry: once
    /// the window has passed, only the newest notification's are held.
    #[test]
    fn heap_bytes_follow_inserts_and_expiry() {
        let mut d = Dedup::new(Duration::minutes(10));
        assert_eq!(d.heap_bytes(), 0);
        d.accept_at(&n("m1", "data-1", 0), at(0));
        d.accept_at(&n("m2", "data-1", 30), at(1)); // re-queues data-1
        d.accept_at(&n("m3", "data-22", 0), at(2));
        // Each message id twice; data-1 in the map once and in two expiry
        // records, data-22 twice.
        assert_eq!(d.string_bytes, 3 * 2 * 2 + 3 * 6 + 2 * 7);
        assert!(d.heap_bytes() > d.string_bytes as u64);
        d.accept_at(&n("m4", "d4", 0), at(700));
        assert_eq!(d.string_bytes, 2 * 2 + 2 * 2);
        assert_eq!(d.len(), 1);
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
