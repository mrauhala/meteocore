//! Eviction, refusal, turnover and ghost-hit counting (#1308).
//!
//! A hit ratio or a full cache does not say whether a cache needs more
//! memory: a cache few requests reach is full and misses because nobody has
//! asked yet. What says "too small" is thrashing, entries evicted and then
//! asked for again soon after. This module counts what a cache evicts and,
//! for a cache that keeps a ghost ring, how many misses asked for a key it
//! evicted recently, and how recently.
//!
//! The counting runs in `quick_cache`'s [`Lifecycle`]. Under the shard lock
//! its hooks only move each evicted entry into the request state, as
//! `quick_cache`'s default lifecycle does so the entry drops outside the
//! lock, and note whether `before_evict` announced it. [`Counting::record`]
//! weighs, hashes and counts them once the insert has released the lock:
//! nothing extra runs under it.

use std::collections::{HashMap, VecDeque};
use std::hash::{BuildHasher, BuildHasherDefault, Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use quick_cache::{DefaultHashBuilder, Lifecycle};

use crate::WeighFn;

/// The depths of [`Churn::ghost_hits`], in capacities: a miss whose key was
/// evicted with at most this multiple of the capacity evicted since, its own
/// weight included. A cache that much larger would still have held it, so
/// the depths estimate the hit ratio at 1.5 ×, 2 × and 3 × the size.
pub const GHOST_DEPTHS: [&str; 3] = ["0.5", "1", "2"];

/// The most evictions a ghost ring remembers, whatever its reach: about
/// 32 MiB at the ring's ~32 bytes per eviction. A ring of a cache whose
/// entries are so small that twice its capacity is more evictions than this
/// sees less than 2 × back, and undercounts its deepest ghost hits.
const MAX_GHOSTS: usize = 1 << 20;

/// A ghost ring's stripes: keys spread over them by hash, each behind its
/// own lock, so concurrent misses and evictions rarely wait for each other.
const STRIPES: usize = 16;

/// `quick_cache`'s default share of a shard for its hot ring, which its
/// `options` module keeps private.
pub(crate) const DEFAULT_HOT_ALLOCATION: f64 = 0.97;

/// What a cache admitted, refused and evicted since it was built, as
/// cumulative counters. Part of [`crate::CacheMetrics`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Churn {
    /// Bytes of entries admitted for keys that were not resident. Turnover
    /// is these per hour over the capacity: evicted bytes trail them by a
    /// full cache while it fills, so they are the wrong numerator. A new
    /// value for a resident key displaces nothing and is not counted.
    pub inserted_bytes: u64,
    /// Inserts refused because the entry is heavier than its shard's hot
    /// target (`quick_cache` never admits one). Not evictions.
    pub refused: u64,
    /// Entries evicted from the cold ring. Once the cache is full, these are
    /// entries never read after their insert: the cold share is the share
    /// of entries evicted before their second use.
    pub evicted_cold: u64,
    /// Entries evicted from the hot ring: read at least once, or inserted
    /// while the cache filled.
    pub evicted_hot: u64,
    /// Bytes of the evicted entries.
    pub evicted_bytes: u64,
    /// Lookups that read around the cache: [`crate::ByteBoundedCache::get_or_join`]
    /// finding the key absent. Neither a hit nor a miss.
    pub bypasses: u64,
    /// Misses whose key was evicted within each of [`GHOST_DEPTHS`] times the
    /// capacity, cumulative (a ghost hit at 0.5 counts at 1 and 2 too).
    /// `None` for a cache without a ghost ring
    /// ([`crate::ByteBoundedCache::with_ghosts`]).
    pub ghost_hits: Option<[u64; 3]>,
    /// The cache is filled ahead of its reads, by a pre-warm: an entry is
    /// inserted long before it is read, and a recency signal undervalues it
    /// (#992). Set by [`crate::ByteBoundedCache::prewarmed`].
    pub prewarmed: bool,
}

impl Churn {
    /// Every eviction, cold and hot.
    pub fn evictions(&self) -> u64 {
        self.evicted_cold.saturating_add(self.evicted_hot)
    }

    /// The counters of `self` less those of `earlier`, the same cache read
    /// sooner; `None` if any went back, as a rebuilt cache's do.
    pub fn since(&self, earlier: &Churn) -> Option<Churn> {
        let ghost_hits = match (self.ghost_hits, earlier.ghost_hits) {
            (Some(now), Some(then)) => Some([
                now[0].checked_sub(then[0])?,
                now[1].checked_sub(then[1])?,
                now[2].checked_sub(then[2])?,
            ]),
            // The ring started after `earlier`: every hit it saw is since.
            (now, _) => now,
        };
        Some(Churn {
            inserted_bytes: self.inserted_bytes.checked_sub(earlier.inserted_bytes)?,
            refused: self.refused.checked_sub(earlier.refused)?,
            evicted_cold: self.evicted_cold.checked_sub(earlier.evicted_cold)?,
            evicted_hot: self.evicted_hot.checked_sub(earlier.evicted_hot)?,
            evicted_bytes: self.evicted_bytes.checked_sub(earlier.evicted_bytes)?,
            bypasses: self.bypasses.checked_sub(earlier.bypasses)?,
            ghost_hits,
            prewarmed: self.prewarmed,
        })
    }
}

/// Which ring an entry was evicted from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ring {
    Cold = 0,
    Hot = 1,
}

/// The counters of one cache, shared by its [`Counting`] lifecycles (one per
/// shard) and the cache itself.
pub(crate) struct Counters {
    /// The configured capacity; `0` for a disabled cache, which counts
    /// nothing.
    capacity: u64,
    /// The weight above which a shard refuses an entry: its hot target.
    /// Set once, when the cache is built and its shards are known.
    hot_target: AtomicU64,
    inserted_bytes: AtomicU64,
    refused: AtomicU64,
    evicted: [AtomicU64; 2],
    evicted_bytes: AtomicU64,
    bypasses: AtomicU64,
    prewarmed: AtomicBool,
    ghosts: OnceLock<Ghosts>,
    /// Hashes evicted keys and missed lookups alike for the ghost ring, and
    /// inserted keys to tell a new value of a resident key from an eviction.
    hasher: DefaultHashBuilder,
}

impl Counters {
    pub(crate) fn new(capacity: u64, hasher: DefaultHashBuilder) -> Self {
        Counters {
            capacity,
            hot_target: AtomicU64::new(0),
            inserted_bytes: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            evicted: [AtomicU64::new(0), AtomicU64::new(0)],
            evicted_bytes: AtomicU64::new(0),
            bypasses: AtomicU64::new(0),
            prewarmed: AtomicBool::new(false),
            ghosts: OnceLock::new(),
            hasher,
        }
    }

    /// Set the hot target from the shard budget `quick_cache` chose and the
    /// hot share, rounding as `quick_cache` does.
    pub(crate) fn set_hot_target(&self, shard_capacity: u64, hot_allocation: f64) {
        let target = ((shard_capacity as f64 * hot_allocation) as u64)
            .clamp(shard_capacity.min(1), shard_capacity);
        self.hot_target.store(target, Ordering::Relaxed);
    }

    /// The weight above which a shard refuses an entry.
    pub(crate) fn hot_target(&self) -> u64 {
        self.hot_target.load(Ordering::Relaxed)
    }

    pub(crate) fn set_prewarmed(&self) {
        self.prewarmed.store(true, Ordering::Relaxed);
    }

    /// Start remembering evictions, if not already. A disabled cache evicts
    /// nothing and keeps no ring.
    pub(crate) fn keep_ghosts(&self) {
        if self.capacity > 0 {
            self.ghosts.get_or_init(|| Ghosts::new(self.capacity));
        }
    }

    pub(crate) fn record_bypass(&self) {
        self.bypasses.fetch_add(1, Ordering::Relaxed);
    }

    /// Look a missed `key` up in the ghost ring, if the cache keeps one.
    pub(crate) fn record_ghost<Q: Hash + ?Sized>(&self, key: &Q) {
        if let Some(ghosts) = self.ghosts.get() {
            let now = self.evicted_bytes.load(Ordering::Relaxed);
            ghosts.lookup(self.hash(key), now);
        }
    }

    fn hash<Q: Hash + ?Sized>(&self, key: &Q) -> u64 {
        BuildHasher::hash_one(&self.hasher, key)
    }

    pub(crate) fn counting(&self) -> bool {
        self.capacity > 0
    }

    pub(crate) fn snapshot(&self) -> Churn {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        Churn {
            inserted_bytes: load(&self.inserted_bytes),
            refused: load(&self.refused),
            evicted_cold: load(&self.evicted[Ring::Cold as usize]),
            evicted_hot: load(&self.evicted[Ring::Hot as usize]),
            evicted_bytes: load(&self.evicted_bytes),
            bypasses: load(&self.bypasses),
            ghost_hits: self
                .ghosts
                .get()
                .map(|ghosts| ghosts.hits.each_ref().map(load)),
            prewarmed: self.prewarmed.load(Ordering::Relaxed),
        }
    }
}

/// An insert whose outcome [`Counting::record`] counts: the entry's weight.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Insert {
    pub weight: u64,
}

/// The cache's [`Lifecycle`]: counts what each insert evicted.
pub(crate) struct Counting<K, V> {
    counters: Arc<Counters>,
    weigh: WeighFn<K, V>,
}

impl<K, V> Counting<K, V> {
    pub(crate) fn new(counters: Arc<Counters>, weigh: WeighFn<K, V>) -> Self {
        Counting { counters, weigh }
    }

    pub(crate) fn counters(&self) -> &Counters {
        &self.counters
    }

    pub(crate) fn weigh(&self, key: &K, value: &V) -> u64 {
        (self.weigh.weigh)(key, value)
    }
}

// Manual: `#[derive]` would require `K: Clone, V: Clone`.
impl<K, V> Clone for Counting<K, V> {
    fn clone(&self) -> Self {
        Counting {
            counters: Arc::clone(&self.counters),
            weigh: self.weigh,
        }
    }
}

impl<K: Hash, V> Counting<K, V> {
    /// Count what one request evicted, after the shard lock is released,
    /// and the outcome of its `insert`, if it inserted.
    ///
    /// - An entry heavier than the hot target is a refused insert, never
    ///   admitted. Such a request evicts nothing else: what else it reports
    ///   is the key's old value, removed for the refused new one.
    /// - Otherwise an entry `before_evict` announced was evicted, and one it
    ///   did not is the inserted key's old value, replaced: `quick_cache`
    ///   calls `before_evict` for every eviction and refusal, never for a
    ///   replacement.
    pub(crate) fn record(&self, evicted: Evicted<K, V>, insert: Option<Insert>) {
        let counters = &*self.counters;
        if !counters.counting() {
            return;
        }
        let hot_target = counters.hot_target.load(Ordering::Relaxed);
        if insert.is_some_and(|insert| insert.weight > hot_target) {
            counters.refused.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let ghosts = counters.ghosts.get();
        let mut replaced = false;
        for entry in evicted.into_iter() {
            let weight = self.weigh(&entry.key, &entry.value);
            if weight > hot_target {
                // A refused fill: `quick_cache` held the claim's
                // placeholder, so there was no old value.
                return;
            }
            if !entry.announced {
                replaced = true;
                continue;
            }
            counters.evicted[entry.from as usize].fetch_add(1, Ordering::Relaxed);
            // The evicted bytes are the ghost ring's clock.
            let before = counters.evicted_bytes.fetch_add(weight, Ordering::Relaxed);
            if let Some(ghosts) = ghosts {
                ghosts.remember(counters.hash(&entry.key), before, before + weight);
            }
        }
        if let (Some(insert), false) = (insert, replaced) {
            counters
                .inserted_bytes
                .fetch_add(insert.weight, Ordering::Relaxed);
        }
    }
}

impl<K: Hash, V> Lifecycle<K, V> for Counting<K, V> {
    type RequestState = Evicted<K, V>;

    #[inline]
    fn begin_request(&self) -> Evicted<K, V> {
        Evicted::default()
    }

    #[inline]
    fn before_evict(&self, state: &mut Evicted<K, V>, _key: &K, _val: &mut V) {
        state.announced = true;
    }

    #[inline]
    fn on_evict_cold(&self, state: &mut Evicted<K, V>, key: K, val: V) {
        state.push(key, val, Ring::Cold);
    }

    #[inline]
    fn on_evict_hot(&self, state: &mut Evicted<K, V>, key: K, val: V) {
        state.push(key, val, Ring::Hot);
    }

    /// For the requests `quick_cache` finishes itself. The cache's own
    /// inserts take the `_with_lifecycle` forms and call
    /// [`Counting::record`] with what they inserted. Note that
    /// `quick_cache`'s `set_capacity` calls this under the shard lock.
    fn end_request(&self, state: Evicted<K, V>) {
        self.record(state, None);
    }
}

/// The entries one request evicted, held so they are counted and dropped
/// after the shard lock is released. Two fit without an allocation, which
/// covers nearly every insert; more spill to a vector.
pub(crate) struct Evicted<K, V> {
    first: [Option<Removed<K, V>>; 2],
    rest: Vec<Removed<K, V>>,
    /// `before_evict` announced the next entry.
    announced: bool,
}

/// An entry a request took out of the cache.
struct Removed<K, V> {
    key: K,
    value: V,
    from: Ring,
    /// `before_evict` announced it: evicted or refused, not replaced.
    announced: bool,
}

impl<K, V> Default for Evicted<K, V> {
    fn default() -> Self {
        Evicted {
            first: [None, None],
            rest: Vec::new(),
            announced: false,
        }
    }
}

impl<K, V> Evicted<K, V> {
    fn push(&mut self, key: K, value: V, from: Ring) {
        let entry = Removed {
            key,
            value,
            from,
            announced: std::mem::take(&mut self.announced),
        };
        match self.first.iter_mut().find(|slot| slot.is_none()) {
            Some(slot) => *slot = Some(entry),
            None => self.rest.push(entry),
        }
    }

    fn into_iter(self) -> impl Iterator<Item = Removed<K, V>> {
        self.first.into_iter().flatten().chain(self.rest)
    }
}

/// A cache's ghost ring and its hit counters. The ring's clock is the
/// cache's evicted bytes: a key evicted when it read `before` was evicted
/// `now - before` bytes ago, its own included.
pub(crate) struct Ghosts {
    stripes: [Stripe; STRIPES],
    hits: [AtomicU64; 3],
    /// 0.5 ×, 1 × and 2 × the capacity: [`GHOST_DEPTHS`] in bytes. The
    /// deepest is how far back the ring remembers.
    depths: [u64; 3],
}

/// One stripe of a ghost ring, on a cache line of its own.
#[repr(align(128))]
#[derive(Default)]
struct Stripe(Mutex<Remembered>);

/// A stripe's recent evictions.
#[derive(Default)]
struct Remembered {
    /// `(key hash, clock before its eviction)`, in eviction order.
    order: VecDeque<(u64, u64)>,
    /// The clock before each remembered key's latest eviction.
    index: HashMap<u64, u64, BuildHasherDefault<HashIsKey>>,
}

impl Ghosts {
    fn new(capacity: u64) -> Self {
        Ghosts {
            stripes: Default::default(),
            hits: Default::default(),
            depths: [capacity / 2, capacity, capacity.saturating_mul(2)],
        }
    }

    /// The stripe of `hash`, picked by bits that neither the stripe's hash
    /// map (low bits, top seven) nor anything else uses.
    fn stripe(&self, hash: u64) -> std::sync::MutexGuard<'_, Remembered> {
        let stripe = &self.stripes[(hash >> 32) as usize % STRIPES];
        stripe.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Remember that `hash`'s key was evicted when the clock read `before`,
    /// and forget what the clock, now at `after`, has left behind: more than
    /// twice the capacity ago, or past the stripe's share of
    /// [`MAX_GHOSTS`].
    fn remember(&self, hash: u64, before: u64, after: u64) {
        let mut stripe = self.stripe(hash);
        stripe.index.insert(hash, before);
        stripe.order.push_back((hash, before));
        while let Some(&(oldest, at)) = stripe.order.front() {
            let reached = after.saturating_sub(at) <= self.depths[2];
            if reached && stripe.order.len() <= MAX_GHOSTS / STRIPES {
                break;
            }
            stripe.order.pop_front();
            // The index keeps a key's latest eviction; an older one leaves
            // it alone.
            if stripe.index.get(&oldest) == Some(&at) {
                stripe.index.remove(&oldest);
            }
        }
    }

    /// Bytes evicted since `hash`'s key was evicted, its own included, the
    /// clock reading `now`: what a larger cache would need to have kept it.
    /// `None` if not remembered.
    fn distance(&self, hash: u64, now: u64) -> Option<u64> {
        let before = self.stripe(hash).index.get(&hash).copied()?;
        Some(now.saturating_sub(before))
    }

    /// Count a miss of `hash`'s key at each depth it was evicted within.
    fn lookup(&self, hash: u64, now: u64) {
        let Some(distance) = self.distance(hash, now) else {
            return;
        };
        for (hits, depth) in self.hits.iter().zip(self.depths) {
            if distance <= depth {
                hits.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// A hasher for keys that are hashes already.
#[derive(Default)]
struct HashIsKey(u64);

impl Hasher for HashIsKey {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 = self.0.rotate_left(8) ^ u64::from(*byte);
        }
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Evict `hash`'s key of `weight` bytes on the ring's clock.
    fn evict(ghosts: &Ghosts, clock: &mut u64, hash: u64, weight: u64) {
        ghosts.remember(hash, *clock, *clock + weight);
        *clock += weight;
    }

    #[test]
    fn the_ring_measures_the_weight_evicted_since_a_key() {
        let ghosts = Ghosts::new(100);
        let mut clock = 0;
        evict(&ghosts, &mut clock, 1, 10);
        evict(&ghosts, &mut clock, 2, 30);
        evict(&ghosts, &mut clock, 3 << 32, 20);
        assert_eq!(ghosts.distance(1, clock), Some(60));
        assert_eq!(ghosts.distance(2, clock), Some(50));
        assert_eq!(ghosts.distance(3 << 32, clock), Some(20), "another stripe");
        assert_eq!(ghosts.distance(4, clock), None);
        // A key evicted again counts from its latest eviction.
        evict(&ghosts, &mut clock, 1, 10);
        assert_eq!(ghosts.distance(1, clock), Some(10));
        ghosts.lookup(2, clock);
        ghosts.lookup(1, clock);
        ghosts.lookup(4, clock);
        let hits = ghosts.hits.each_ref().map(|h| h.load(Ordering::Relaxed));
        assert_eq!(hits, [1, 2, 2], "60 is past 0.5 × 100");
    }

    /// A key is forgotten once more than twice the capacity was evicted
    /// after it; its older entry going does not drop a later eviction.
    #[test]
    fn the_ring_reaches_back_twice_the_capacity() {
        let ghosts = Ghosts::new(100);
        let mut clock = 0;
        evict(&ghosts, &mut clock, 1, 50);
        evict(&ghosts, &mut clock, 2, 50);
        evict(&ghosts, &mut clock, 1, 50);
        evict(&ghosts, &mut clock, 3, 100);
        // Key 2: 50 + 50 + 100 = 200 evicted since its eviction began.
        assert_eq!(ghosts.distance(2, clock), Some(200));
        evict(&ghosts, &mut clock, 4, 1);
        assert_eq!(ghosts.distance(2, clock), None, "past 2 × capacity");
        assert_eq!(
            ghosts.distance(1, clock),
            Some(151),
            "its later eviction stays"
        );
        assert_eq!(ghosts.stripe(0).order.len(), 3);
    }

    #[test]
    fn churn_since_subtracts_and_refuses_a_backward_step() {
        let earlier = Churn {
            inserted_bytes: 10,
            evicted_cold: 1,
            ghost_hits: Some([1, 2, 3]),
            ..Churn::default()
        };
        let later = Churn {
            inserted_bytes: 25,
            evicted_cold: 4,
            evicted_hot: 2,
            ghost_hits: Some([2, 4, 6]),
            prewarmed: true,
            ..Churn::default()
        };
        assert_eq!(
            later.since(&earlier),
            Some(Churn {
                inserted_bytes: 15,
                evicted_cold: 3,
                evicted_hot: 2,
                ghost_hits: Some([1, 2, 3]),
                prewarmed: true,
                ..Churn::default()
            })
        );
        assert_eq!(earlier.since(&later), None);
    }
}
