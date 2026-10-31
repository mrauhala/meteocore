//! Ranges a query fetched ahead of its decodes (#1353), held until a decode
//! asks for one.
//!
//! Not a cache: a decode takes its range out, and the read path then fills
//! the payload cache with it as with a range it had just read, so fetching
//! ahead leaves the payload cache's admission, recency and counts as a
//! serial read leaves them.
//!
//! A plan reserves room for each range before fetching it
//! ([`Staging::reserve`]), against what is staged and reserved already, so
//! concurrent queries share the room without dropping one another's ranges:
//! a range that does not fit is refused, never made room for. A staged range
//! no decode takes is dropped once it has waited [`HELD_FOR`], at the next
//! staging call or scrape. `ds-cache` cannot hold these: its hot ring never
//! ages out an entry nobody reads.
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use bytes::Bytes;

use super::PayloadKey;

/// The longest a range waits for its decode: a request's whole deadline.
pub(super) const HELD_FOR: Duration = Duration::from_secs(30);

/// What staging charges for a key: its object path and the map entries.
const KEY_BYTES: u64 = 128;

/// What a range of `length` bytes takes of the room once staged: the one
/// weight a plan reserves and staging charges.
pub(crate) fn weight(length: u64) -> u64 {
    length.saturating_add(KEY_BYTES)
}

#[derive(Default)]
pub(super) struct Staging(Mutex<Staged>);

#[derive(Default)]
struct Staged {
    /// Each staged range: its staging sequence number, bytes and weight.
    ranges: HashMap<PayloadKey, (u64, Bytes, u64)>,
    /// Staging order: sequence number → key and when it was staged.
    order: BTreeMap<u64, (PayloadKey, Instant)>,
    /// Weight of the staged ranges, and of the room reserved for ranges
    /// being fetched.
    weight: u64,
    reserved: u64,
    next: u64,
}

impl Staging {
    /// Reserve `weight` of `room` for a range about to be fetched, if what
    /// is staged and reserved leaves it. Never makes room.
    pub(super) fn reserve(&self, weight: u64, room: u64) -> bool {
        let mut staged = self.lock();
        let fits = staged
            .weight
            .checked_add(staged.reserved)
            .and_then(|n| n.checked_add(weight))
            .is_some_and(|n| n <= room);
        if fits {
            staged.reserved += weight;
        }
        fits
    }

    /// Give back reserved room no range was staged into.
    pub(super) fn release(&self, weight: u64) {
        let mut staged = self.lock();
        staged.reserved = staged.reserved.saturating_sub(weight);
    }

    /// Stage `bytes` for `key` into the `weight` reserved for it. `false`,
    /// the reservation left to its holder, when `key` is staged already.
    pub(super) fn stage(&self, key: PayloadKey, bytes: Bytes, weight: u64, now: Instant) -> bool {
        let mut staged = self.lock();
        if staged.ranges.contains_key(&key) {
            return false;
        }
        staged.reserved = staged.reserved.saturating_sub(weight);
        staged.next += 1;
        let seq = staged.next;
        staged.order.insert(seq, (key.clone(), now));
        staged.ranges.insert(key, (seq, bytes, weight));
        staged.weight += weight;
        true
    }

    /// The range staged for `key`, taken out: one decode reads it. Also
    /// drops ranges that have waited too long, and says how many.
    pub(super) fn take(&self, key: &PayloadKey, now: Instant) -> (Option<Bytes>, u64) {
        let mut staged = self.lock();
        let bytes = staged.remove(key);
        (bytes, staged.expire(now))
    }

    /// The range staged for `key`, left staged for its decode.
    pub(super) fn peek(&self, key: &PayloadKey) -> Option<Bytes> {
        self.lock()
            .ranges
            .get(key)
            .map(|(_, bytes, _)| bytes.clone())
    }

    /// Drop the ranges that have waited [`HELD_FOR`]; how many.
    pub(super) fn expire(&self, now: Instant) -> u64 {
        self.lock().expire(now)
    }

    /// Weight of the room reserved for ranges being fetched.
    #[cfg(test)]
    pub(super) fn reserved(&self) -> u64 {
        self.lock().reserved
    }

    /// Weight of the staged ranges, reservations aside.
    pub(super) fn weight(&self) -> u64 {
        self.lock().weight
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Staged> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Staged {
    fn remove(&mut self, key: &PayloadKey) -> Option<Bytes> {
        let (seq, bytes, weight) = self.ranges.remove(key)?;
        self.order.remove(&seq);
        self.weight -= weight;
        Some(bytes)
    }

    fn expire(&mut self, now: Instant) -> u64 {
        let mut dropped = 0;
        while let Some((_, (key, at))) = self.order.first_key_value() {
            if now.saturating_duration_since(*at) < HELD_FOR {
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
        assert!(staging.reserve(weight(100), u64::MAX));
        assert!(staging.stage(key(0), range(), weight(100), now));
        assert_eq!(staging.peek(&key(0)), Some(range()));
        assert_eq!(staging.take(&key(0), now), (Some(range()), 0));
        assert_eq!(staging.take(&key(0), now), (None, 0));
        assert_eq!(staging.weight(), 0);
        // A second copy is refused, its reservation left to its holder.
        assert!(staging.reserve(weight(100), u64::MAX));
        assert!(staging.stage(key(0), range(), weight(100), now));
        assert!(staging.reserve(weight(100), u64::MAX));
        assert!(!staging.stage(key(0), range(), weight(100), now));
        staging.release(weight(100));
        assert_eq!(staging.lock().reserved, 0);
    }

    /// A plan that fills the room exactly stages every range of it, and a
    /// second plan gets only what is left: nothing staged is dropped for it.
    #[test]
    fn plans_share_the_room_without_dropping_each_others_ranges() {
        let staging = Staging::default();
        let room = 3 * weight(100);
        let now = Instant::now();
        for i in 0..3 {
            assert!(staging.reserve(weight(100), room), "{i}");
        }
        assert!(!staging.reserve(1, room), "the first plan holds the room");
        for i in 0..3 {
            assert!(staging.stage(key(i * 100), range(), weight(100), now));
        }
        assert!(!staging.reserve(1, room), "staged ranges hold it too");
        assert_eq!(staging.weight(), room);
        assert!((0..3).all(|i| staging.peek(&key(i * 100)).is_some()));
        staging.take(&key(0), now);
        assert!(
            staging.reserve(weight(100), room),
            "a taken range frees its room"
        );
    }

    #[test]
    fn unread_ranges_are_dropped_once_they_have_waited_their_time() {
        let staging = Staging::default();
        let start = Instant::now();
        staging.reserve(2 * weight(100), u64::MAX);
        staging.stage(key(0), range(), weight(100), start);
        staging.stage(key(100), range(), weight(100), start + HELD_FOR / 2);
        assert_eq!(staging.expire(start + HELD_FOR / 2), 0);
        let (taken, dropped) = staging.take(&key(500), start + HELD_FOR);
        assert_eq!((taken, dropped), (None, 1));
        assert!(staging.peek(&key(100)).is_some());
        assert_eq!(staging.expire(start + HELD_FOR * 2), 1);
        assert_eq!(staging.weight(), 0);
    }
}
