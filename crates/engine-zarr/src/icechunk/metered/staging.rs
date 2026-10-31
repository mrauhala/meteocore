//! Ranges a query fetched ahead of its decodes (#1353), held until a decode
//! asks for one.
//!
//! Not a cache: a decode takes its range out, and the read path then fills
//! the payload cache with it as with a range it had just read, so fetching
//! ahead leaves the payload cache's admission, recency and counts as a
//! serial read leaves them. A range no decode takes is dropped, oldest
//! first, once newer ones need its room or it has waited [`HELD_FOR`].
//! `ds-cache` cannot hold these: its hot ring never ages out an entry
//! nobody reads.
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use bytes::Bytes;

use super::{weigh_payload, PayloadKey};

/// The longest a range waits for its decode: a request's whole deadline.
const HELD_FOR: Duration = Duration::from_secs(30);

#[derive(Default)]
pub(super) struct Staging(Mutex<Staged>);

#[derive(Default)]
struct Staged {
    ranges: HashMap<PayloadKey, (u64, Bytes)>,
    /// Staging order: sequence number → key and when it was staged.
    order: BTreeMap<u64, (PayloadKey, Instant)>,
    weight: u64,
    next: u64,
}

impl Staging {
    /// Hold `bytes` for `key` in at most `room` bytes of staged ranges,
    /// dropping the oldest past it. Returns how many ranges were dropped
    /// unread: this one too if it cannot fit, or if `key` is staged already.
    pub(super) fn stage(&self, key: PayloadKey, bytes: Bytes, room: u64, now: Instant) -> u64 {
        let weight = weigh_payload(&key, &bytes);
        let mut staged = self.lock();
        let mut dropped = staged.expire(room, now);
        if weight > room || staged.ranges.contains_key(&key) {
            return dropped + 1;
        }
        staged.next += 1;
        let seq = staged.next;
        staged.order.insert(seq, (key.clone(), now));
        staged.ranges.insert(key, (seq, bytes));
        staged.weight += weight;
        dropped += staged.expire(room, now);
        dropped
    }

    /// The range staged for `key`, taken out: one decode reads it. Also
    /// drops ranges that have waited too long, and says how many.
    pub(super) fn take(&self, key: &PayloadKey, now: Instant) -> (Option<Bytes>, u64) {
        let mut staged = self.lock();
        let bytes = staged.remove(key);
        (bytes, staged.expire(u64::MAX, now))
    }

    /// The range staged for `key`, left staged for its decode.
    pub(super) fn peek(&self, key: &PayloadKey) -> Option<Bytes> {
        self.lock().ranges.get(key).map(|(_, bytes)| bytes.clone())
    }

    /// Bytes staged, weighed as the payload cache weighs its entries.
    pub(super) fn weight(&self) -> u64 {
        self.lock().weight
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Staged> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Staged {
    fn remove(&mut self, key: &PayloadKey) -> Option<Bytes> {
        let (seq, bytes) = self.ranges.remove(key)?;
        self.order.remove(&seq);
        self.weight -= weigh_payload(key, &bytes);
        Some(bytes)
    }

    /// Drop the oldest ranges while they weigh more than `room` or have
    /// waited [`HELD_FOR`]; how many.
    fn expire(&mut self, room: u64, now: Instant) -> u64 {
        let mut dropped = 0;
        while let Some((_, (key, at))) = self.order.first_key_value() {
            if self.weight <= room && now.saturating_duration_since(*at) < HELD_FOR {
                break;
            }
            let key = key.clone();
            self.remove(&key);
            dropped += 1;
        }
        dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(start: u64) -> PayloadKey {
        PayloadKey {
            path: "chunks/0123456789ABCDEF".into(),
            start,
            end: start + 100,
        }
    }

    fn range() -> Bytes {
        Bytes::from(vec![7u8; 100])
    }

    #[test]
    fn a_staged_range_is_taken_once() {
        let staging = Staging::default();
        let now = Instant::now();
        assert_eq!(staging.stage(key(0), range(), u64::MAX, now), 0);
        assert_eq!(staging.peek(&key(0)), Some(range()));
        assert_eq!(staging.take(&key(0), now), (Some(range()), 0));
        assert_eq!(staging.take(&key(0), now), (None, 0));
        assert_eq!(staging.weight(), 0);
    }

    #[test]
    fn unread_ranges_are_dropped_oldest_first_past_their_room_or_time() {
        let staging = Staging::default();
        let weight = weigh_payload(&key(0), &range());
        let start = Instant::now();
        for i in 0..3 {
            assert_eq!(staging.stage(key(i * 100), range(), 3 * weight, start), 0);
        }
        // A fourth needs the oldest's room.
        assert_eq!(staging.stage(key(300), range(), 3 * weight, start), 1);
        assert_eq!(staging.peek(&key(0)), None);
        assert!(staging.peek(&key(100)).is_some());
        // One that cannot fit at all, and one already staged, stay out.
        assert_eq!(staging.stage(key(400), range(), weight - 1, start), 4);
        let staging = Staging::default();
        staging.stage(key(0), range(), u64::MAX, start);
        assert_eq!(staging.stage(key(0), range(), u64::MAX, start), 1);
        // Past their time, a take drops them, whatever it takes.
        staging.stage(key(100), range(), u64::MAX, start + HELD_FOR / 2);
        let (taken, dropped) = staging.take(&key(500), start + HELD_FOR);
        assert_eq!((taken, dropped), (None, 1));
        assert!(staging.peek(&key(100)).is_some());
        assert_eq!(staging.weight(), weight);
    }
}
