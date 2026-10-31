//! Shared byte-bounded LRU cache plumbing (#480).
//!
//! Every process-global or per-collection cache in the workspace follows the
//! same shape: a [`quick_cache`] LRU whose eviction budget is **bytes** (a
//! weigher measures each entry) rather than entry count, sized from an
//! `MC_*_CACHE_MB` environment variable or a config field, with cumulative
//! hit/miss counters surfaced to `/metrics`. Before this crate that shape was
//! copy-pasted twelve times across the engines, render layer, and API crates
//! (the top duplication finding of the 2026-07 code-quality audit); this
//! module is the single home for the weigher/env-parse/counter plumbing.
//! Call sites keep what is genuinely theirs: the key type, the weight
//! function, the env var name, and the default/entry-size constants.
//!
//! Deliberately dependency-light (only `quick_cache` + `std`) so every crate
//! — including the framework-free `ds-render`/`ds-mvt`/`ds-3dtiles` tier —
//! can depend on it. Domain types stay out; this is mechanism only.
//!
//! [`plan`] is the process-wide registry of every cache and admission
//! budget, with where each size came from (#1306).
//!
//! Every cache also counts what it evicts, refuses and admits, and a cache
//! built [`ByteBoundedCache::with_ghosts`] how many misses asked for a key it
//! evicted recently: the signal that more memory would help ([`Churn`],
//! #1308).
//!
//! Every cache can be resized in place ([`ByteBoundedCache::set_capacity`],
//! #1311), keeping what still fits.

use std::hash::Hash;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Re-exported so call sites can name the borrowed-key-lookup bound without
/// depending on `quick_cache` directly.
pub use quick_cache::Equivalent;

mod churn;
mod entry;
pub mod plan;
pub use churn::{Churn, GHOST_DEPTHS};
pub use entry::{CacheEntry, CacheFillGuard, Joined};

use churn::{Counters, Counting, Insert};

/// One mebibyte, for `*_MB` → bytes conversions.
pub const MIB: u64 = 1024 * 1024;

/// A cache built [`ByteBoundedCache::with_entry_floor`] is never resized
/// below this many of its heaviest entry.
pub const FLOOR_ENTRIES: u64 = 4;

/// Parse a cache size (in MB) from the environment: `MC_*_CACHE_MB`-style
/// variables are trimmed and parsed as `u64`, with unset/unparseable values
/// falling back to `default_mb`. `0` conventionally disables the cache.
/// [`plan::env_size`] also says whether the variable set it.
pub fn env_mb(var: &str, default_mb: u64) -> u64 {
    parse_env_mb(var).unwrap_or(default_mb)
}

/// The hot share for a one-shard cache of `capacity_bytes` whose cold ring
/// keeps a fill of `entry_bytes` until its next read:
/// `1 − 1.1 × entry_bytes / capacity_bytes`, at most `quick_cache`'s
/// default `0.97` (#1332).
///
/// Once the hot ring is full, `quick_cache` admits a new entry to the cold
/// ring. A single-flight fill ([`ByteBoundedCache::get_or_insert_with`] and
/// the other claims) links the entry there before it evicts, oldest first,
/// so a fill heavier than the room the hot ring leaves evicts itself,
/// unread. The default leaves 3 % of the budget. A cache too small to give
/// the cold ring that room and still admit the entry to the hot ring
/// (under 2.1 entries) keeps the default.
pub fn hot_allocation_holding(capacity_bytes: u64, entry_bytes: u64) -> f64 {
    let share = entry_bytes as f64 / capacity_bytes.max(1) as f64;
    let hot = 1.0 - 1.1 * share;
    if hot < share {
        churn::DEFAULT_HOT_ALLOCATION
    } else {
        hot.min(churn::DEFAULT_HOT_ALLOCATION)
    }
}

/// Whether a one-shard cache of `capacity_bytes` whose hot ring targets
/// `hot_allocation` of it both admits an entry of `entry_bytes` and keeps
/// such a fill in its cold ring until its next read, as
/// [`hot_allocation_holding`] arranges: what a cache resized in place, which
/// keeps its share ([`ByteBoundedCache::set_capacity`]), still does.
pub fn holding_keeps(hot_allocation: f64, capacity_bytes: u64, entry_bytes: u64) -> bool {
    let capacity = capacity_bytes as f64;
    let entry = entry_bytes as f64;
    // A byte of slack: the share `hot_allocation_holding` gives leaves the
    // cold ring exactly the room, up to rounding.
    hot_allocation * capacity >= entry && (1.0 - hot_allocation) * capacity + 1.0 >= 1.1 * entry
}

/// `var` trimmed and parsed as a whole number, if it is set to one.
fn parse_env_mb(var: &str) -> Option<u64> {
    std::env::var(var)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
}

/// Snapshot of a cache's counters and occupancy for `/metrics`.
///
/// `hits`/`misses` are cumulative since construction (the server's metrics
/// layer delta-tracks them into Prometheus counters); `bytes` is the current
/// resident weight; `capacity_bytes` is the **configured** budget — `0` for a
/// disabled cache, even though the underlying store is clamped to a 1-byte
/// budget internally (see [`ByteBoundedCache::new`]). `churn` is cumulative
/// like the hits, and all zero for a disabled cache.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheMetrics {
    pub hits: u64,
    pub misses: u64,
    pub bytes: u64,
    pub capacity_bytes: u64,
    pub churn: Churn,
}

/// Adapts a plain `fn(&K, &V) -> u64` to `quick_cache`'s `Weighter` trait so
/// call sites pass a function instead of hand-rolling a unit-struct impl.
pub(crate) struct WeighFn<K, V> {
    weigh: fn(&K, &V) -> u64,
}

// Manual Clone/Copy: a fn pointer is always copyable, but `#[derive]` would
// add unwanted `K: Clone, V: Clone` bounds.
impl<K, V> Clone for WeighFn<K, V> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<K, V> Copy for WeighFn<K, V> {}

impl<K, V> quick_cache::Weighter<K, V> for WeighFn<K, V> {
    fn weight(&self, key: &K, val: &V) -> u64 {
        (self.weigh)(key, val)
    }
}

/// A thread-safe LRU cache bounded by total **bytes** (per-entry weight from
/// a caller-supplied function), with cumulative hit/miss counters.
///
/// Semantics shared by every call site:
///
/// - **Weight includes overhead.** Weight functions should count the payload
///   plus key strings plus a small fixed allowance for `Arc`/node overhead,
///   so the configured budget approximates real resident memory.
/// - **Capacity 0 disables retention.** `quick_cache` has no zero capacity,
///   so the store is built with a 1-byte budget: every real entry outweighs
///   it and is never admitted, making each `get` a miss and each `insert` a
///   no-op. [`Self::capacity_bytes`] still reports the configured `0`.
/// - **Counting is explicit.** [`Self::get`] and [`Self::get_or_insert_with`]
///   count hits/misses; [`Self::get_untracked`]/[`Self::contains_key`] don't,
///   and [`Self::record_hit`]/[`Self::record_miss_for`] let wrappers with
///   custom accounting (e.g. a negative-cache check between lookup and miss)
///   keep the counters truthful.
/// - **Churn is counted on every insert** ([`Churn`], #1308): bytes admitted,
///   entries refused, entries evicted from each ring. A cache built
///   [`Self::with_ghosts`] also counts the misses that ask for a key it
///   evicted recently.
/// - **The capacity can change** ([`Self::set_capacity`], #1311); the shard
///   count cannot. A cache that may grow is built with a ceiling
///   ([`Self::new_resizable`]).
pub struct ByteBoundedCache<K: Eq + Hash, V: Clone> {
    /// Shared so the memory plan can read it weakly ([`Self::register`]).
    inner: Arc<Inner<K, V>>,
}

/// The `quick_cache` store under a [`ByteBoundedCache`].
type Store<K, V> =
    quick_cache::sync::Cache<K, V, WeighFn<K, V>, quick_cache::DefaultHashBuilder, Counting<K, V>>;

/// A single-flight claim on the store.
type Placeholder<'a, K, V> = quick_cache::sync::PlaceholderGuard<
    'a,
    K,
    V,
    WeighFn<K, V>,
    quick_cache::DefaultHashBuilder,
    Counting<K, V>,
>;

struct Inner<K: Eq + Hash, V: Clone> {
    cache: Store<K, V>,
    /// The store's lifecycle, to count the requests the cache finishes.
    lifecycle: Counting<K, V>,
    shape: Shape,
    /// The configured capacity; a resize moves it.
    capacity_bytes: AtomicU64,
    /// Held by a resize, so two never interleave their steps.
    resizing: Mutex<()>,
    /// Built [`ByteBoundedCache::with_entry_floor`].
    entry_floor: AtomicBool,
    hits: AtomicU64,
    misses: AtomicU64,
}

/// What a cache was built from besides its capacity: what decides its
/// shards, so a resize can tell what shards a cache built at another size
/// would have.
#[derive(Clone, Copy)]
struct Shape {
    approx_entry_bytes: u64,
    /// Shards asked for; `quick_cache`'s four per CPU when `None`.
    shards: Option<usize>,
    ceiling_bytes: Option<u64>,
}

impl Shape {
    /// `quick_cache`'s options for a cache of `capacity_bytes`, but its hot
    /// share.
    fn options(&self, capacity_bytes: u64) -> quick_cache::OptionsBuilder {
        // `max(16)` keeps a small/zero capacity valid (a near-disabled cache
        // that holds nothing still needs a non-zero item estimate).
        let items = |bytes: u64| (bytes / self.approx_entry_bytes.max(1)).max(16);
        let capacity_items = items(capacity_bytes);
        let estimated_items = self
            .ceiling_bytes
            .map_or(capacity_items, |ceiling| items(ceiling).max(capacity_items));
        let mut options = quick_cache::OptionsBuilder::new();
        options
            .estimated_items_capacity(estimated_items as usize)
            .weight_capacity(capacity_bytes.max(1));
        if estimated_items > capacity_items {
            // `quick_cache` sizes its list of recently evicted keys from the
            // item estimate and scales it with each resize: sized from the
            // ceiling, growing to it would multiply the list by the growth.
            options.ghost_allocation(
                churn::DEFAULT_GHOST_ALLOCATION * capacity_items as f64 / estimated_items as f64,
            );
        }
        if let Some(shards) = self.shards {
            options.shards(shards);
        }
        options
    }

    /// The shards `quick_cache` gives a cache of this shape built at
    /// `capacity_bytes` without a ceiling: the most that keep about 32
    /// estimated entries each. An empty one, built and dropped, to ask.
    fn shards_at(&self, capacity_bytes: u64) -> usize {
        let uncapped = Shape {
            ceiling_bytes: None,
            ..*self
        };
        let options = uncapped
            .options(capacity_bytes)
            .build()
            .expect("item estimate and weight capacity are set");
        quick_cache::sync::Cache::<u8, u8>::with_options(
            options,
            quick_cache::UnitWeighter,
            quick_cache::DefaultHashBuilder::default(),
            quick_cache::sync::DefaultLifecycle::default(),
        )
        .num_shards()
    }

    /// The smallest capacity at which a cache built without a ceiling
    /// would have `shards` shards, or more: where each slice still holds
    /// `quick_cache`'s 32 estimated entries. `0` for one shard, which
    /// `quick_cache` keeps at any size.
    fn slice_floor(&self, shards: usize) -> u64 {
        if shards <= 1 {
            return 0;
        }
        // `quick_cache` halves its shards while ⌈items / shards⌉ < 32.
        (31 * shards as u64 + 1).saturating_mul(self.approx_entry_bytes.max(1))
    }
}

impl<K: Eq + Hash, V: Clone> Inner<K, V> {
    fn capacity_bytes(&self) -> u64 {
        self.capacity_bytes.load(Ordering::Relaxed)
    }

    fn usage(&self) -> plan::Usage {
        plan::Usage {
            capacity_bytes: self.capacity_bytes(),
            used_bytes: self.cache.weight(),
            entries: Some(self.cache.len() as u64),
            hits: Some(self.hits.load(Ordering::Relaxed)),
            misses: Some(self.misses.load(Ordering::Relaxed)),
            churn: Some(self.counters().snapshot()),
        }
    }

    fn counters(&self) -> &Counters {
        self.lifecycle.counters()
    }

    /// Count a miss of `key`, and look it up among recent evictions.
    fn miss<Q: Hash + ?Sized>(&self, key: &Q) {
        self.misses.fetch_add(1, Ordering::Relaxed);
        self.counters().record_ghost(key);
    }

    /// Publish `value` to a single-flight claim on `key` and count the
    /// insert. The weight function takes the owned key, so `key` is cloned:
    /// only on a fill, which follows a computation.
    fn fill<Q>(&self, key: &Q, claim: Placeholder<'_, K, V>, value: V)
    where
        Q: ToOwned<Owned = K> + ?Sized,
    {
        let weight = self
            .counters()
            .counting()
            .then(|| self.lifecycle.weigh(&key.to_owned(), &value));
        let _ = publish(&self.lifecycle, claim, value, weight);
    }

    /// One step of a resize: set the store to `store_bytes` (`0` only on the
    /// way up from a disabled cache), count and drop what it evicted once
    /// `quick_cache` has released every shard lock, then let the counters
    /// follow, `capacity_bytes` the configured capacity after the step.
    fn resize_store(&self, store_bytes: u64, capacity_bytes: u64) {
        let old_shard = self.cache.shard_capacity();
        self.lifecycle.park();
        self.cache.set_capacity(store_bytes);
        for evicted in self.lifecycle.unpark() {
            self.lifecycle.record_evictions(evicted);
        }
        self.counters()
            .resize(capacity_bytes, old_shard, self.cache.shard_capacity());
        self.capacity_bytes.store(capacity_bytes, Ordering::Relaxed);
    }
}

/// The capacities a shrink from `from` to `to` bytes passes through: steps
/// of at most a tenth of `from`, so no shard lock is held while more than a
/// tenth of the cache is evicted, ending at `to`.
fn shrink_steps(from: u64, to: u64) -> impl Iterator<Item = u64> {
    let step = from.div_ceil(10).max(1);
    let mut at = from;
    std::iter::from_fn(move || {
        (at > to).then(|| {
            at = at.saturating_sub(step).max(to);
            at
        })
    })
}

/// Publish `value` (of weight `weight`, `None` for a claim taken while the
/// cache was disabled) to a single-flight claim and count the insert, if it
/// has a weight. Returns the value if the claim was displaced by an
/// explicit insert or removal of the key.
fn publish<K: Eq + Hash, V: Clone>(
    lifecycle: &Counting<K, V>,
    claim: Placeholder<'_, K, V>,
    value: V,
    weight: Option<u64>,
) -> Result<(), V> {
    let evicted = claim.insert_with_lifecycle(value)?;
    let insert = weight.map(|weight| Insert { weight });
    lifecycle.record(evicted, insert);
    Ok(())
}

impl<K: Eq + Hash, V: Clone> ByteBoundedCache<K, V> {
    /// Build a cache with a budget of `capacity_bytes`. `approx_entry_bytes`
    /// is only a hash-map sizing hint (expected typical entry weight); the
    /// eviction budget is always `capacity_bytes`.
    pub fn new(capacity_bytes: u64, approx_entry_bytes: u64, weigh: fn(&K, &V) -> u64) -> Self {
        Self::build(capacity_bytes, approx_entry_bytes, weigh, None, None, None)
    }

    /// Like [`Self::new`], for a cache that may grow ([`Self::set_capacity`]):
    /// its shards are counted from `ceiling_bytes`, the largest it is
    /// expected to reach, not from `capacity_bytes`. `quick_cache` fixes the
    /// shard count when the cache is built, halving four shards per CPU
    /// until each one's item estimate reaches 32, so a cache built at 0 or
    /// 1 MiB would stay one shard, one lock, however large it grows. Each
    /// shard holds an equal slice of the capacity, and admits at most
    /// [`Self::new_with_hot_allocation`]'s bound of it: a ceiling far above
    /// the capacity refuses entries the capacity alone would admit.
    ///
    /// For the computed memory plan (#1312), which grants caches sizes
    /// between a floor and a ceiling; nothing uses it yet. Its limit: a
    /// shrink stops where each of its shards still holds 32 estimated
    /// entries, so a cache built under that, with the ceiling's shards,
    /// grows but never shrinks back to its build size.
    pub fn new_resizable(
        capacity_bytes: u64,
        ceiling_bytes: u64,
        approx_entry_bytes: u64,
        weigh: fn(&K, &V) -> u64,
    ) -> Self {
        Self::build(
            capacity_bytes,
            approx_entry_bytes,
            weigh,
            None,
            None,
            Some(ceiling_bytes),
        )
    }

    /// [`Self::new_resizable`] with `hot_allocation` (in `(0, 1]`) of each
    /// shard's budget as the hot ring's target
    /// ([`Self::new_with_hot_allocation`]).
    pub fn new_resizable_with_hot_allocation(
        capacity_bytes: u64,
        ceiling_bytes: u64,
        approx_entry_bytes: u64,
        weigh: fn(&K, &V) -> u64,
        hot_allocation: f64,
    ) -> Self {
        assert!(
            hot_allocation > 0.0 && hot_allocation <= 1.0,
            "hot_allocation must be in (0, 1], got {hot_allocation}"
        );
        Self::build(
            capacity_bytes,
            approx_entry_bytes,
            weigh,
            None,
            Some(hot_allocation),
            Some(ceiling_bytes),
        )
    }

    /// Like [`Self::new`], but with `hot_allocation` (in `(0, 1]`) of each
    /// shard's budget as the target for the "hot" ring instead of
    /// `quick_cache`'s default `0.97`.
    ///
    /// `quick_cache` is not a plain LRU. An insert enters the hot ring only
    /// while the hot ring is under its target; otherwise it enters the cold
    /// ring, which holds the rest of the budget and evicts first-in,
    /// first-out unless an entry was read while there (it is then promoted
    /// to hot). Under the default, once the first 97 % of the budget has
    /// been inserted, every later entry that is not read soon competes for
    /// the remaining 3 %. A miss of a key the cold ring evicted recently is
    /// admitted to the hot ring, so an evicted working set sticks only over
    /// repeats. A cache filled ahead of reads (a background pre-warm) needs
    /// a lower value so fresh entries survive until they are wanted (#992);
    /// a caller that will read an entry again can instead [`Self::keep`]
    /// it.
    ///
    /// The cost: `quick_cache` never admits an entry heavier than the hot
    /// target of its shard, `hot_allocation × capacity / shards`. Shards are
    /// halved until each one's item estimate reaches 32, so with
    /// `capacity_bytes ≥ 32 × approx_entry_bytes` a shard holds at least
    /// about `31 × approx_entry_bytes`, and the largest admitted entry is at
    /// least about `hot_allocation × 31 × approx_entry_bytes` whatever the
    /// CPU count. Below that the cache is one shard and the bound is
    /// `hot_allocation × capacity_bytes`.
    pub fn new_with_hot_allocation(
        capacity_bytes: u64,
        approx_entry_bytes: u64,
        weigh: fn(&K, &V) -> u64,
        hot_allocation: f64,
    ) -> Self {
        assert!(
            hot_allocation > 0.0 && hot_allocation <= 1.0,
            "hot_allocation must be in (0, 1], got {hot_allocation}"
        );
        Self::build(
            capacity_bytes,
            approx_entry_bytes,
            weigh,
            None,
            Some(hot_allocation),
            None,
        )
    }

    /// Like [`Self::new`], but the whole budget is one LRU. `quick_cache`
    /// otherwise splits a cache into shards by key hash, each holding an
    /// equal slice of the budget, so entries that together fit the budget
    /// can still overflow one shard and be evicted. With one shard, entries
    /// whose weights sum to at most `capacity_bytes` are never evicted: a
    /// caller can plan what stays resident. For caches of few, large entries
    /// looked up at a low rate, where one lock costs nothing.
    pub fn new_single_shard(
        capacity_bytes: u64,
        approx_entry_bytes: u64,
        weigh: fn(&K, &V) -> u64,
    ) -> Self {
        Self::build(
            capacity_bytes,
            approx_entry_bytes,
            weigh,
            Some(1),
            None,
            None,
        )
    }

    /// [`Self::new_single_shard`] with `hot_allocation` (in `(0, 1]`) of the
    /// budget as the hot ring's target, as [`Self::new_with_hot_allocation`]
    /// describes. [`hot_allocation_holding`] picks one whose cold ring keeps
    /// a fill until its next read.
    pub fn new_single_shard_with_hot_allocation(
        capacity_bytes: u64,
        approx_entry_bytes: u64,
        weigh: fn(&K, &V) -> u64,
        hot_allocation: f64,
    ) -> Self {
        assert!(
            hot_allocation > 0.0 && hot_allocation <= 1.0,
            "hot_allocation must be in (0, 1], got {hot_allocation}"
        );
        Self::build(
            capacity_bytes,
            approx_entry_bytes,
            weigh,
            Some(1),
            Some(hot_allocation),
            None,
        )
    }

    fn build(
        capacity_bytes: u64,
        approx_entry_bytes: u64,
        weigh: fn(&K, &V) -> u64,
        shards: Option<usize>,
        hot_allocation: Option<f64>,
        ceiling_bytes: Option<u64>,
    ) -> Self {
        let shape = Shape {
            approx_entry_bytes,
            shards,
            ceiling_bytes,
        };
        let mut options = shape.options(capacity_bytes);
        if let Some(hot_allocation) = hot_allocation {
            options.hot_allocation(hot_allocation);
        }
        let hasher = quick_cache::DefaultHashBuilder::default();
        let counters = Arc::new(Counters::new(capacity_bytes, hasher.clone()));
        let lifecycle = Counting::new(Arc::clone(&counters), WeighFn { weigh });
        let cache = quick_cache::sync::Cache::with_options(
            options
                .build()
                .expect("item estimate and weight capacity are set"),
            WeighFn { weigh },
            hasher,
            lifecycle.clone(),
        );
        counters.set_hot_target(
            cache.shard_capacity(),
            hot_allocation.unwrap_or(churn::DEFAULT_HOT_ALLOCATION),
        );
        ByteBoundedCache {
            inner: Arc::new(Inner {
                cache,
                lifecycle,
                shape,
                capacity_bytes: AtomicU64::new(capacity_bytes),
                resizing: Mutex::new(()),
                entry_floor: AtomicBool::new(false),
                hits: AtomicU64::new(0),
                misses: AtomicU64::new(0),
            }),
        }
    }

    /// Resize the cache to `capacity_bytes` in place (#1311), keeping the
    /// entries that still fit: `quick_cache` evicts each shard's coldest
    /// until it fits its slice. `0` disables the cache, as at construction,
    /// and drops everything. Evictions count as any other ([`Churn`]), and
    /// the ghost depths and [`Self::shard_capacity_bytes`] follow. Returns
    /// the capacity applied: a shrink can stop short of `capacity_bytes`.
    ///
    /// - A shrink goes in steps of at most a tenth of the capacity, each
    ///   holding one shard's lock at a time while it evicts. The evicted
    ///   entries are counted, hashed into the ghost ring and dropped once
    ///   each step has released the locks, as an insert's are.
    /// - The shard count stays what the cache was built with, so a shrink
    ///   stops where each shard's slice still holds the 32 estimated
    ///   entries a cache built at that size would give it: below, slices
    ///   would refuse entries such a cache admits. A growth keeps fewer,
    ///   larger shards than such a cache: more contention, nothing refused.
    /// - The hot share stays what the cache was built with: `quick_cache`
    ///   keeps each shard's hot target in proportion. A cache sized by
    ///   [`hot_allocation_holding`] keeps its share at the new size, where
    ///   it may no longer hold a fill ([`holding_keeps`]).
    /// - A cache grown from `0` takes `quick_cache`'s defaults instead: a
    ///   hot share of `0.97` whatever it was built with, and no list of
    ///   recently evicted keys to readmit to the hot ring. `quick_cache`
    ///   resets a shard's share only from a capacity of `0`, and only to
    ///   its default.
    /// - A cache built [`Self::with_entry_floor`] shrinks no further than
    ///   [`FLOOR_ENTRIES`] of its heaviest entry.
    ///
    /// No floor makes a shrink grow the cache, or stops a disable.
    /// [`Self::resizes_in_place_to`] says when a cache built at the new
    /// size would differ. Resizes are serialized; lookups and inserts go on
    /// during one, an insert racing a step counted against the hot target
    /// before or after it.
    pub fn set_capacity(&self, capacity_bytes: u64) -> u64 {
        let inner = &*self.inner;
        let _resizing = inner.resizing.lock().unwrap_or_else(|e| e.into_inner());
        let from = inner.capacity_bytes();
        let mut target = capacity_bytes;
        if target > 0 && target < from {
            let mut floor = inner.shape.slice_floor(inner.cache.num_shards());
            if inner.entry_floor.load(Ordering::Relaxed) {
                floor = floor.max(FLOOR_ENTRIES.saturating_mul(inner.counters().largest()));
            }
            target = target.max(floor.min(from));
        }
        if target > from {
            if from == 0 {
                // From its 1-byte clamp `quick_cache` would keep a hot
                // share of 1 and scale its evicted-key list from one byte.
                // From 0 it starts both again from its defaults.
                inner.resize_store(0, 0);
            }
            inner.resize_store(target, target);
        } else {
            for step in shrink_steps(from, target) {
                inner.resize_store(step.max(1), step);
            }
        }
        target
    }

    /// Whether [`Self::set_capacity`] to `capacity_bytes` leaves a cache
    /// close enough to one built at that size, without a ceiling and with
    /// the same hot share, for a caller to keep it rather than build a new
    /// one (#1311):
    ///
    /// - a shrink, when the cache has no more shards than such a cache
    ///   would: never smaller slices, which would refuse entries;
    /// - a growth, when it has at least a quarter of their shards: fewer
    ///   shards are more contention, and below a quarter the cache holds
    ///   under about a quarter of the new size, little to keep;
    /// - never a growth of a disabled cache, which takes `quick_cache`'s
    ///   defaults and holds nothing to keep.
    ///
    /// A share from [`hot_allocation_holding`] is the caller's to check
    /// ([`holding_keeps`]).
    pub fn resizes_in_place_to(&self, capacity_bytes: u64) -> bool {
        let from = self.capacity_bytes();
        if capacity_bytes == 0 || capacity_bytes == from {
            return true;
        }
        if from == 0 {
            return false;
        }
        let shards = self.shards();
        let fresh = self.inner.shape.shards_at(capacity_bytes);
        if capacity_bytes < from {
            shards <= fresh
        } else {
            shards.saturating_mul(4) >= fresh
        }
    }

    /// The number of shards, fixed when the cache was built.
    pub fn shards(&self) -> usize {
        self.inner.cache.num_shards()
    }

    /// The share of each shard's budget the hot ring targets: what the
    /// cache was built with, as `quick_cache` rounds it.
    pub fn hot_allocation(&self) -> f64 {
        let shard = self.inner.cache.shard_capacity();
        self.inner.counters().hot_target() as f64 / shard.max(1) as f64
    }

    /// Never shrink this cache below [`FLOOR_ENTRIES`] of the heaviest
    /// entry it has admitted, except to disable it. For a cache sized from
    /// its engine's demand, of few large entries (satellite scans of
    /// 30–157 MB): a shard admits at most its hot share of its slice, so a
    /// smaller cache would refuse every entry like it and the engine would
    /// read each one again on every request.
    pub fn with_entry_floor(self) -> Self {
        self.inner.entry_floor.store(true, Ordering::Relaxed);
        self
    }

    /// Keep a ghost ring: the hashes of recently evicted keys, back over
    /// twice the capacity of evicted weight, so [`Churn::ghost_hits`] counts
    /// the misses that a cache 1.5 ×, 2 × or 3 × as large would have hit
    /// (#1308). About 32 bytes per remembered eviction, up to 32 MiB. A
    /// miss takes one more hash of its key and the lock of one of the ring's
    /// stripes, and an eviction the same; a hit costs nothing (the
    /// `churn_bench` example measures it). Misses counted by
    /// [`Self::record_miss`] are not looked up: count them with
    /// [`Self::record_miss_for`]. No ring for a disabled cache.
    pub fn with_ghosts(self) -> Self {
        self.inner.counters().keep_ghosts();
        self
    }

    /// Mark the cache as filled ahead of its reads, by a pre-warm: its
    /// entries are inserted long before they are read, so a recency signal
    /// (evictions, ghost hits) undervalues them (#992). Reports show its
    /// numbers with that caveat ([`Churn::prewarmed`]).
    pub fn prewarmed(self) -> Self {
        self.inner.counters().set_prewarmed();
        self
    }

    /// Build a cache sized from the environment: `var` (MB, [`env_mb`]) with
    /// `default_mb` as the fallback. `0` disables retention. Not in the
    /// memory plan: a cache the server keeps is sized by
    /// [`plan::env_bytes`], which also says where the size came from, and
    /// built [`Self::registered`].
    pub fn from_env(
        var: &str,
        default_mb: u64,
        approx_entry_bytes: u64,
        weigh: fn(&K, &V) -> u64,
    ) -> Self {
        Self::new(
            env_mb(var, default_mb).saturating_mul(MIB),
            approx_entry_bytes,
            weigh,
        )
    }

    /// [`Self::from_env`] with one shard ([`Self::new_single_shard`]).
    pub fn from_env_single_shard(
        var: &str,
        default_mb: u64,
        approx_entry_bytes: u64,
        weigh: fn(&K, &V) -> u64,
    ) -> Self {
        Self::new_single_shard(
            env_mb(var, default_mb).saturating_mul(MIB),
            approx_entry_bytes,
            weigh,
        )
    }

    /// Look up `key`, counting a hit or a miss.
    pub fn get<Q>(&self, key: &Q) -> Option<V>
    where
        Q: Hash + Equivalent<K> + ?Sized,
    {
        let result = self.inner.cache.get(key);
        if result.is_some() {
            self.inner.hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.inner.miss(key);
        }
        result
    }

    /// Look up `key` without touching the hit/miss counters (LRU recency is
    /// still bumped). For wrappers whose accounting doesn't map 1:1 onto
    /// lookups — pair with [`Self::record_hit`]/[`Self::record_miss`].
    pub fn get_untracked<Q>(&self, key: &Q) -> Option<V>
    where
        Q: Hash + Equivalent<K> + ?Sized,
    {
        self.inner.cache.get(key)
    }

    /// Keep `value`, just filled for `key`, until its filler reads it again
    /// (#1340), counting no hit. For an entry the filler samples from its
    /// own copy now: unread, it waits in the cold ring and is evicted when
    /// later fills reach it. Marked read, it is promoted to the hot ring
    /// instead. Concurrent fills can evict it before the mark, when their
    /// burst outweighs the cold ring: it is then inserted again, which
    /// `quick_cache` admits to the hot ring for a key it evicted recently,
    /// and an insert never evicts its own entry. An entry heavier than the
    /// hot target was refused, not evicted, and stays out. Fills not kept
    /// leave the cache resistant to a scan of one-off reads.
    pub fn keep(&self, key: K, value: V)
    where
        K: Clone,
    {
        if self.inner.cache.get(&key).is_some() {
            return;
        }
        if self.inner.lifecycle.weigh(&key, &value) > self.inner.counters().hot_target() {
            return;
        }
        self.insert(key.clone(), value);
        let _ = self.inner.cache.get(&key);
    }

    /// Look up `key` without touching the hit/miss counters or its recency:
    /// for a caller that serves the value on behalf of a read that will
    /// look it up itself, which then counts and promotes it once.
    pub fn peek<Q>(&self, key: &Q) -> Option<V>
    where
        Q: Hash + Equivalent<K> + ?Sized,
    {
        self.inner.cache.peek(key)
    }

    /// Whether `key` is resident — no recency bump, no counter change.
    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        Q: Hash + Equivalent<K> + ?Sized,
    {
        self.inner.cache.contains_key(key)
    }

    /// Insert an entry. At capacity 0 the entry is never admitted (see the
    /// type-level docs), so this is effectively a no-op there.
    pub fn insert(&self, key: K, value: V) {
        let insert = self.inner.counters().counting().then(|| Insert {
            weight: self.inner.lifecycle.weigh(&key, &value),
        });
        let evicted = self.inner.cache.insert_with_lifecycle(key, value);
        self.inner.lifecycle.record(evicted, insert);
    }

    /// Remove `key`, returning its value if it was resident. No counter
    /// change.
    pub fn remove<Q>(&self, key: &Q) -> Option<V>
    where
        Q: Hash + Equivalent<K> + ?Sized,
    {
        self.inner.cache.remove(key).map(|(_, value)| value)
    }

    /// Keep only the entries for which `keep` returns `true`, dropping the
    /// rest. Used for targeted invalidation (e.g. evicting one collection's
    /// entries on an incremental reload) — visits every entry, so call it on
    /// reload-frequency paths, not per request.
    pub fn retain(&self, keep: impl Fn(&K, &V) -> bool) {
        self.inner.cache.retain(keep);
    }

    /// Fetch `key` from the cache, or run `with` and insert its result.
    ///
    /// `quick_cache`'s placeholder guard is the single-flight: concurrent
    /// callers for the SAME key block on one compute. An error is returned to
    /// this caller without inserting (no key poisoning); the miss is counted
    /// *before* the fallible compute so failures still register as misses.
    pub fn get_or_insert_with<Q, E>(
        &self,
        key: &Q,
        with: impl FnOnce() -> Result<V, E>,
    ) -> Result<V, E>
    where
        Q: Hash + Equivalent<K> + ToOwned<Owned = K> + ?Sized,
    {
        match self.inner.cache.get_value_or_guard(key, None) {
            quick_cache::sync::GuardResult::Value(value) => {
                self.record_hit();
                Ok(value)
            }
            quick_cache::sync::GuardResult::Guard(claim) => {
                self.inner.miss(key);
                let value = with()?;
                self.inner.fill(key, claim, value.clone());
                Ok(value)
            }
            quick_cache::sync::GuardResult::Timeout => {
                unreachable!("a lookup without a timeout waits for the fill")
            }
        }
    }

    /// Single-flight lookup with a bounded wait for another caller's fill.
    /// The timeout applies to waiting, not to `with`; the caller must bound
    /// its own I/O/compute. Failed fills release the guard for a later retry.
    pub fn get_or_insert_with_timeout<Q, E>(
        &self,
        key: &Q,
        timeout: std::time::Duration,
        with: impl FnOnce() -> Result<V, E>,
        timed_out: impl FnOnce() -> E,
    ) -> Result<V, E>
    where
        Q: Hash + Equivalent<K> + ToOwned<Owned = K> + ?Sized,
    {
        match self.inner.cache.get_value_or_guard(key, Some(timeout)) {
            quick_cache::sync::GuardResult::Value(value) => {
                self.record_hit();
                Ok(value)
            }
            quick_cache::sync::GuardResult::Guard(claim) => {
                self.inner.miss(key);
                let value = with()?;
                self.inner.fill(key, claim, value.clone());
                Ok(value)
            }
            quick_cache::sync::GuardResult::Timeout => Err(timed_out()),
        }
    }

    /// Look up a value or claim its single-flight fill without counting a hit
    /// or miss. A zero timeout probes without waiting for an existing owner.
    ///
    /// Callers can acquire compute resources only after receiving a vacant
    /// entry, and keep the guard through that compute. Dropping the guard on
    /// admission failure, error or panic lets another caller claim the fill;
    /// that caller must acquire its own resources before computing. Pair this
    /// with [`Self::record_hit`] / [`Self::record_miss`] when work actually runs.
    pub fn get_value_or_guard_untracked<Q>(
        &self,
        key: &Q,
        timeout: std::time::Duration,
    ) -> CacheEntry<'_, K, V>
    where
        Q: Hash + Equivalent<K> + ToOwned<Owned = K> + ?Sized,
    {
        match self.inner.cache.get_value_or_guard(key, Some(timeout)) {
            quick_cache::sync::GuardResult::Value(value) => CacheEntry::Value(value),
            quick_cache::sync::GuardResult::Guard(claim) => CacheEntry::Vacant(CacheFillGuard {
                claim,
                key: self.inner.counters().counting().then(|| key.to_owned()),
                lifecycle: &self.inner.lifecycle,
            }),
            quick_cache::sync::GuardResult::Timeout => CacheEntry::Timeout,
        }
    }

    /// Look up `key` without ever inserting it: a resident value, else the
    /// value of a fill another caller already has in flight, waiting up to
    /// `timeout` for it (`None` waits as long as the fill takes), else
    /// [`Joined::Absent`]. The caller then computes the value itself and
    /// does not offer it to the cache, so nothing resident is evicted for
    /// it. For a reader whose values would evict more than they could ever
    /// reuse (a GRIB query over more fields than the cache holds, #1297),
    /// which should still share an identical concurrent fill.
    ///
    /// Counts a hit for a value and a bypass when absent
    /// ([`Churn::bypasses`]): the caller reads around the cache, so its read
    /// is not a miss of the cache's workload (#1308). A timeout counts
    /// nothing. Its own computation is not shared: a caller arriving while
    /// it runs claims a fill of its own. Finding the key absent takes and
    /// drops a fill claim at once, so a caller arriving in that instant may
    /// become the owner of the fill, as after a failed one.
    pub fn get_or_join<Q>(&self, key: &Q, timeout: Option<std::time::Duration>) -> Joined<V>
    where
        Q: Hash + Equivalent<K> + ToOwned<Owned = K> + ?Sized,
    {
        match self.inner.cache.get_value_or_guard(key, timeout) {
            quick_cache::sync::GuardResult::Value(value) => {
                self.record_hit();
                Joined::Value(value)
            }
            quick_cache::sync::GuardResult::Guard(claim) => {
                drop(claim);
                self.inner.counters().record_bypass();
                Joined::Absent
            }
            quick_cache::sync::GuardResult::Timeout => Joined::Timeout,
        }
    }

    /// Count one hit (for wrappers using [`Self::get_untracked`]).
    pub fn record_hit(&self) {
        self.inner.hits.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one miss (for wrappers using [`Self::get_untracked`]), without
    /// looking for it among recent evictions: a cache built
    /// [`Self::with_ghosts`] counts its misses with [`Self::record_miss_for`].
    pub fn record_miss(&self) {
        self.inner.misses.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one read around the cache (for wrappers using
    /// [`Self::get_untracked`]), as [`Self::get_or_join`] counts one that
    /// finds the key absent: neither a hit nor a miss ([`Churn::bypasses`]).
    pub fn record_bypass(&self) {
        self.inner.counters().record_bypass();
    }

    /// Count one miss of `key` (for wrappers using [`Self::get_untracked`]),
    /// and look it up among recent evictions as [`Self::get`] does.
    pub fn record_miss_for<Q>(&self, key: &Q)
    where
        Q: Hash + Equivalent<K> + ?Sized,
    {
        self.inner.miss(key);
    }

    /// Cumulative `(hits, misses)`.
    pub fn stats(&self) -> (u64, u64) {
        (
            self.inner.hits.load(Ordering::Relaxed),
            self.inner.misses.load(Ordering::Relaxed),
        )
    }

    /// Current resident weight (bytes, as measured by the weight function).
    pub fn weight(&self) -> u64 {
        self.inner.cache.weight()
    }

    /// The **configured** capacity in bytes (`0` = disabled), not the
    /// internal 1-byte clamp.
    pub fn capacity_bytes(&self) -> u64 {
        self.inner.capacity_bytes()
    }

    /// The budget of one shard in bytes: what entries that must all stay
    /// resident may weigh together. Equal to [`Self::capacity_bytes`] for a
    /// [`Self::new_single_shard`] cache; a sharded cache splits its budget,
    /// so entries that fit the total can still overflow the shard they hash
    /// to.
    pub fn shard_capacity_bytes(&self) -> u64 {
        if self.inner.capacity_bytes() == 0 {
            0
        } else {
            self.inner.cache.shard_capacity()
        }
    }

    /// Number of resident entries.
    pub fn len(&self) -> usize {
        self.inner.cache.len()
    }

    /// Whether the cache currently holds no entries.
    pub fn is_empty(&self) -> bool {
        self.inner.cache.len() == 0
    }

    /// Cumulative evictions, refusals, admitted bytes and ghost hits.
    pub fn churn(&self) -> Churn {
        self.inner.counters().snapshot()
    }

    /// Snapshot for `/metrics`.
    pub fn metrics(&self) -> CacheMetrics {
        let (hits, misses) = self.stats();
        CacheMetrics {
            hits,
            misses,
            bytes: self.weight(),
            capacity_bytes: self.inner.capacity_bytes(),
            churn: self.churn(),
        }
    }
}

impl<K, V> ByteBoundedCache<K, V>
where
    K: Eq + Hash + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    /// Enter this cache in the memory plan ([`plan`]) as `id`, its
    /// `/metrics` family, sized from `source`. Listed while the cache lives;
    /// registering it again replaces the entry.
    pub fn register(&self, id: &'static str, scope: plan::Scope, source: plan::SizeSource) {
        plan::register(
            plan::Entry::cache(id, scope, source),
            &self.inner,
            Inner::usage,
        );
    }

    /// [`Self::register`], returning the cache.
    pub fn registered(
        self,
        id: &'static str,
        scope: plan::Scope,
        source: plan::SizeSource,
    ) -> Self {
        self.register(id, scope, source);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The signature must match `fn(&K, &V)` with `K = String`, `V = Vec<u8>`.
    #[allow(clippy::ptr_arg)]
    fn weigh_str(key: &String, val: &Vec<u8>) -> u64 {
        key.len() as u64 + val.len() as u64 + 64
    }

    #[test]
    fn get_counts_hits_and_misses() {
        let cache = ByteBoundedCache::new(MIB, 1024, weigh_str);
        assert!(cache.get(&"a".to_string()).is_none());
        cache.insert("a".to_string(), vec![1, 2, 3]);
        assert_eq!(cache.get(&"a".to_string()).unwrap(), vec![1, 2, 3]);
        assert_eq!(cache.stats(), (1, 1));
    }

    #[test]
    fn zero_capacity_disables_retention_but_reports_configured_capacity() {
        let cache = ByteBoundedCache::new(0, 1024, weigh_str);
        cache.insert("a".to_string(), vec![1]);
        assert!(cache.get(&"a".to_string()).is_none(), "nothing admitted");
        assert_eq!(cache.capacity_bytes(), 0);
        assert_eq!(cache.metrics().capacity_bytes, 0);
        assert_eq!(cache.stats(), (0, 1));
    }

    #[test]
    fn evicts_by_weight() {
        let cache = ByteBoundedCache::new(500, 100, weigh_str);
        for i in 0..20 {
            cache.insert(format!("k{i}"), vec![0u8; 100]);
        }
        let found = (0..20)
            .filter(|i| cache.get_untracked(&format!("k{i}")).is_some())
            .count();
        assert!(found < 20, "expected evictions, all {found} survived");
        assert!(found > 0, "expected some entries to survive");
    }

    /// A sharded cache splits its budget: one shard holds less than the
    /// total, so a caller planning residency must compare against
    /// `shard_capacity_bytes`, which a single-shard cache makes the total.
    #[test]
    fn shard_capacity_is_the_planning_budget() {
        // 64 estimated items of 100 bytes: quick_cache starts from 4 shards
        // per CPU and halves them until each holds ≥ 32 items, so two.
        let sharded = ByteBoundedCache::new(64 * 100, 100, weigh_str);
        assert_eq!(sharded.shard_capacity_bytes(), 32 * 100);
        let single = ByteBoundedCache::new_single_shard(64 * 100, 100, weigh_str);
        assert_eq!(single.shard_capacity_bytes(), 64 * 100);
        let disabled = ByteBoundedCache::new_single_shard(0, 100, weigh_str);
        assert_eq!(disabled.shard_capacity_bytes(), 0);
    }

    /// One shard: entries that together weigh the whole budget all stay.
    /// Sharded, each shard holds an equal slice, so the same entries could
    /// overflow the shard most of them hash to.
    #[test]
    fn single_shard_keeps_everything_that_fits_the_budget() {
        let cache = ByteBoundedCache::new_single_shard(64 * 100, 100, weigh_str);
        let keys: Vec<String> = (0..64).map(|i| format!("{i:02}")).collect();
        for key in &keys {
            // 2 key bytes + 34 value bytes + 64 overhead = 100.
            cache.insert(key.clone(), vec![0u8; 34]);
        }
        assert_eq!(cache.weight(), 64 * 100);
        assert!(keys.iter().all(|key| cache.contains_key(key)));
        // One more byte of anything evicts.
        cache.insert("x".to_string(), vec![0u8; 35]);
        assert!(cache.weight() <= 64 * 100);
        assert!(keys.iter().any(|key| !cache.contains_key(key)));
    }

    /// Weight = the value itself, so tests can model large entries without
    /// allocating them.
    #[allow(clippy::ptr_arg)]
    fn weigh_declared(_key: &String, val: &u64) -> u64 {
        *val
    }

    /// Fill a one-shard cache past its budget without reading anything (the
    /// hot ring is then full of the oldest entries), insert 10 fresh entries,
    /// then 30 more, and count the fresh entries still resident.
    fn fresh_survivors(hot_allocation: Option<f64>) -> usize {
        let cache = ByteBoundedCache::build(
            100 * 1000,
            1000,
            weigh_declared,
            Some(1),
            hot_allocation,
            None,
        );
        for i in 0..300 {
            cache.insert(format!("old{i}"), 1000);
        }
        for i in 0..10 {
            cache.insert(format!("fresh{i}"), 1000);
        }
        for i in 0..30 {
            cache.insert(format!("later{i}"), 1000);
        }
        (0..10)
            .filter(|i| cache.contains_key(&format!("fresh{i}")))
            .count()
    }

    /// #992: under the default 0.97 hot target, unread inserts share a cold
    /// ring of 3 % of the budget, so 30 % of the budget inserted later evicts
    /// them all. At 0.5 the cold ring is half the budget and they stay.
    #[test]
    fn hot_allocation_keeps_fresh_unread_inserts_resident() {
        assert_eq!(fresh_survivors(None), 0, "default policy evicts them");
        assert_eq!(fresh_survivors(Some(0.5)), 10, "0.5 keeps them");
    }

    /// The hot target bounds the largest admitted entry per shard. Asking for
    /// 1024 shards forces the item-count floor (`32` items of
    /// `approx_entry_bytes` per shard): 1 GiB / 256 KiB = 4096 items gives
    /// 128 shards of 8 MiB, so at 0.5 the largest entry is 4 MiB (the
    /// documented floor, `0.5 × 31 × approx_entry_bytes`, is just below).
    #[test]
    fn hot_allocation_bounds_the_largest_admitted_entry() {
        let cache = ByteBoundedCache::build(
            1024 * MIB,
            256 * 1024,
            weigh_declared,
            Some(1024),
            Some(0.5),
            None,
        );
        cache.insert("fits".to_string(), 4 * MIB);
        assert!(cache.contains_key(&"fits".to_string()));
        cache.insert("too-big".to_string(), 4 * MIB + 1);
        assert!(!cache.contains_key(&"too-big".to_string()));
    }

    #[test]
    #[should_panic(expected = "hot_allocation must be in (0, 1]")]
    fn zero_hot_allocation_is_rejected() {
        let _ = ByteBoundedCache::new_with_hot_allocation(MIB, 1024, weigh_declared, 0.0);
    }

    #[test]
    #[should_panic(expected = "hot_allocation must be in (0, 1]")]
    fn a_single_shard_hot_allocation_over_one_is_rejected() {
        let _ =
            ByteBoundedCache::new_single_shard_with_hot_allocation(MIB, 1024, weigh_declared, 1.5);
    }

    /// A 0.1° global field decoded to f32, as the GRIB grid cache weighs it.
    const TENTH_DEGREE_GRID: u64 = 25_934_700;

    /// Fill `key` through a single-flight claim, as a cache's readers do.
    fn fill(cache: &ByteBoundedCache<String, u64>, key: &str, weight: u64) {
        let _ = cache.get_or_insert_with(&key.to_string(), || Ok::<_, ()>(weight));
    }

    /// #1332: once the hot ring is full, a fill heavier than the room it
    /// leaves evicts itself. At the default 0.97, 256 MiB holds ten 0.1°
    /// grids in the hot ring, leaves 8.7 MiB, and keeps neither of the next
    /// two; a plain insert evicts before it links, so it keeps its own.
    /// [`hot_allocation_holding`] leaves room for one.
    #[test]
    fn a_fill_heavier_than_the_cold_ring_evicts_itself() {
        let resident = |cache: &ByteBoundedCache<String, u64>| {
            (0..12)
                .filter(|i| cache.contains_key(&format!("grid{i}")))
                .collect::<Vec<_>>()
        };
        let default = ByteBoundedCache::new_single_shard(256 * MIB, 4 * MIB, weigh_declared);
        for i in 0..12 {
            fill(&default, &format!("grid{i}"), TENTH_DEGREE_GRID);
        }
        assert_eq!(resident(&default), (0..10).collect::<Vec<_>>());
        assert_eq!(default.churn().evicted_cold, 2);

        let inserted = ByteBoundedCache::new_single_shard(256 * MIB, 4 * MIB, weigh_declared);
        for i in 0..12 {
            inserted.insert(format!("grid{i}"), TENTH_DEGREE_GRID);
        }
        assert!(resident(&inserted).contains(&11));

        let hot = hot_allocation_holding(256 * MIB, TENTH_DEGREE_GRID);
        let holding = ByteBoundedCache::new_single_shard_with_hot_allocation(
            256 * MIB,
            4 * MIB,
            weigh_declared,
            hot,
        );
        for i in 0..12 {
            fill(&holding, &format!("grid{i}"), TENTH_DEGREE_GRID);
            assert!(holding.contains_key(&format!("grid{i}")), "grid{i}");
        }
    }

    /// The cold ring keeps a tenth more than the entry; a cache that cannot
    /// also admit it to the hot ring, or a large one, keeps the default.
    #[test]
    fn hot_allocation_holding_leaves_the_entry_and_a_tenth() {
        let hot = hot_allocation_holding(256 * MIB, TENTH_DEGREE_GRID);
        assert!((0.89..0.90).contains(&hot), "{hot}");
        let cold = 256 * MIB - (256.0 * MIB as f64 * hot) as u64;
        assert!(cold >= TENTH_DEGREE_GRID * 11 / 10, "{cold}");
        let hot = hot_allocation_holding(64 * MIB, TENTH_DEGREE_GRID);
        assert!((0.57..0.58).contains(&hot), "{hot}");
        // Over 2.1 entries: both fit. Under: neither can, so the default.
        assert!(hot_allocation_holding(52 * MIB, TENTH_DEGREE_GRID) < 0.5);
        assert_eq!(hot_allocation_holding(51 * MIB, TENTH_DEGREE_GRID), 0.97);
        assert_eq!(hot_allocation_holding(1024 * MIB, TENTH_DEGREE_GRID), 0.97);
        assert_eq!(hot_allocation_holding(0, TENTH_DEGREE_GRID), 0.97);
        assert_eq!(hot_allocation_holding(MIB, 0), 0.97);
    }

    /// Concurrent fills that outweigh the cold ring can evict a fill before
    /// its filler marks it: at 1 GiB, ten threads filling a window of 164
    /// entries of 1.4 MB lost one or two in most runs when only marked
    /// read.
    /// [`ByteBoundedCache::keep`] inserts such a fill again.
    #[test]
    fn kept_fills_survive_a_burst_of_concurrent_fills() {
        use std::sync::atomic::AtomicU64;
        const MESSAGE: u64 = 1_400_000;
        for _ in 0..20 {
            let cache = ByteBoundedCache::new_single_shard(1024 * MIB, MIB / 2, weigh_declared);
            for i in 0..800 {
                fill(&cache, &format!("old{i}"), MESSAGE);
            }
            let next = AtomicU64::new(0);
            std::thread::scope(|scope| {
                for _ in 0..10 {
                    scope.spawn(|| loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= 164 {
                            break;
                        }
                        let key = format!("window{i}");
                        fill(&cache, &key, MESSAGE);
                        cache.keep(key, MESSAGE);
                    });
                }
            });
            let lost: Vec<_> = (0..164)
                .filter(|i| !cache.contains_key(&format!("window{i}")))
                .collect();
            assert!(lost.is_empty(), "{lost:?}");
        }
    }

    /// A refused fill stays out: keeping it inserts nothing.
    #[test]
    fn keeping_a_refused_fill_inserts_nothing() {
        let cache = ghost_cache(10);
        fill(&cache, "big", 20 * 1000);
        cache.keep("big".to_string(), 20 * 1000);
        assert!(!cache.contains_key(&"big".to_string()));
        assert_eq!(
            (cache.churn().refused, cache.churn().inserted_bytes),
            (1, 0)
        );
    }

    /// A one-shard cache of 200 entries of 1000 bytes, filled past its
    /// budget by fills nobody read again; then a window of `window` fills,
    /// each kept when `keep`; then `scan` one-off fills. How many of the
    /// window are resident at the end.
    fn window_resident(hot_allocation: Option<f64>, keep: bool, window: u64, scan: u64) -> u64 {
        let cache = ByteBoundedCache::build(
            200 * 1000,
            1000,
            weigh_declared,
            Some(1),
            hot_allocation,
            None,
        );
        for i in 0..400 {
            fill(&cache, &format!("old{i}"), 1000);
        }
        for i in 0..window {
            let key = format!("window{i}");
            fill(&cache, &key, 1000);
            if keep {
                cache.keep(key, 1000);
            }
        }
        for i in 0..scan {
            fill(&cache, &format!("scan{i}"), 1000);
        }
        (0..window)
            .filter(|i| cache.contains_key(&format!("window{i}")))
            .count() as u64
    }

    /// #1340: in a full cache, a window of fills nobody reads until its
    /// next query keeps what the cold ring holds, at the default 3 % six
    /// of 200. Kept, every window up to the whole budget stays, and
    /// survives a scan of one-off fills twice the budget. A lower hot share
    /// alone keeps a window up to its cold ring, until a scan pushes it out.
    #[test]
    fn a_kept_window_stays_resident_in_a_full_cache() {
        assert_eq!(window_resident(None, false, 100, 0), 6);
        for window in 1..=200 {
            assert_eq!(window_resident(None, true, window, 0), window);
        }
        assert_eq!(window_resident(None, true, 100, 400), 100);
        assert_eq!(window_resident(Some(0.5), false, 100, 0), 100);
        assert_eq!(window_resident(Some(0.5), false, 150, 0), 100);
        assert_eq!(window_resident(Some(0.5), false, 100, 400), 0);
    }

    #[test]
    fn remove_returns_the_value_without_counting() {
        let cache = ByteBoundedCache::new(MIB, 1024, weigh_str);
        cache.insert("a".to_string(), vec![1, 2]);
        assert_eq!(cache.remove(&"a".to_string()), Some(vec![1, 2]));
        assert_eq!(cache.remove(&"a".to_string()), None);
        assert!(!cache.contains_key(&"a".to_string()));
        assert_eq!(cache.weight(), 0);
        assert_eq!(cache.stats(), (0, 0));
    }

    #[test]
    fn get_or_insert_with_counts_and_coalesces() {
        let cache = ByteBoundedCache::new(MIB, 1024, weigh_str);
        let mut computes = 0;
        for _ in 0..3 {
            let v = cache
                .get_or_insert_with(&"k".to_string(), || {
                    computes += 1;
                    Ok::<_, ()>(vec![7])
                })
                .unwrap();
            assert_eq!(v, vec![7]);
        }
        assert_eq!(computes, 1, "one compute, then cached");
        assert_eq!(cache.stats(), (2, 1));
    }

    #[test]
    fn get_or_insert_with_error_does_not_poison_and_counts_a_miss() {
        let cache = ByteBoundedCache::new(MIB, 1024, weigh_str);
        let err = cache.get_or_insert_with(&"k".to_string(), || Err::<Vec<u8>, _>("boom"));
        assert!(err.is_err());
        let ok = cache
            .get_or_insert_with(&"k".to_string(), || Ok::<_, &str>(vec![42]))
            .unwrap();
        assert_eq!(ok, vec![42]);
        assert_eq!(cache.stats(), (0, 2), "both attempts were misses");
    }

    #[test]
    fn timed_single_flight_wait_does_not_cancel_owner_or_block_other_keys() {
        use std::{sync::mpsc, time::Duration};
        let cache = ByteBoundedCache::new(MIB, 1024, weigh_str);
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        std::thread::scope(|scope| {
            let cache = &cache;
            let owner = scope.spawn(move || {
                cache.get_or_insert_with_timeout(
                    &"key".to_string(),
                    Duration::from_secs(2),
                    || {
                        started_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        Ok::<_, &str>(vec![42])
                    },
                    || "owner timeout",
                )
            });
            started_rx.recv().unwrap();
            let waiter = cache.get_or_insert_with_timeout(
                &"key".to_string(),
                Duration::from_millis(10),
                || panic!("waiter must not start another fill"),
                || "timeout",
            );
            let other = cache.get_or_insert_with_timeout(
                &"other".to_string(),
                Duration::ZERO,
                || Ok::<_, &str>(vec![7]),
                || "other timeout",
            );
            release_tx.send(()).unwrap();
            assert_eq!(waiter, Err("timeout"));
            assert_eq!(other, Ok(vec![7]));
            assert_eq!(owner.join().unwrap(), Ok(vec![42]));
            assert_eq!(
                cache.get_or_insert_with_timeout(
                    &"key".to_string(),
                    Duration::ZERO,
                    || panic!("owner populated the cache"),
                    || "timeout"
                ),
                Ok(vec![42])
            );
        });
    }

    #[test]
    fn timed_single_flight_failure_can_be_retried() {
        let cache = ByteBoundedCache::new(MIB, 1024, weigh_str);
        let key = "key".to_string();
        assert_eq!(
            cache.get_or_insert_with_timeout(
                &key,
                std::time::Duration::ZERO,
                || Err::<Vec<u8>, _>("failed"),
                || "timeout"
            ),
            Err("failed")
        );
        assert_eq!(
            cache.get_or_insert_with_timeout(
                &key,
                std::time::Duration::ZERO,
                || Ok::<_, &str>(vec![42]),
                || "timeout"
            ),
            Ok(vec![42])
        );
        assert_eq!(cache.stats(), (0, 2));
    }

    /// A joining lookup reads what is resident and never inserts: a full
    /// cache keeps every entry it had, and the absent key leaves no claim
    /// behind for the next caller.
    #[test]
    fn get_or_join_reads_resident_values_and_never_inserts() {
        let cache = ByteBoundedCache::new_single_shard(4 * 100, 100, weigh_str);
        let keys: Vec<String> = (0..4).map(|i| format!("{i:02}")).collect();
        for key in &keys {
            cache.insert(key.clone(), vec![0u8; 34]); // 100 bytes each: full
        }
        assert_eq!(
            cache.get_or_join(&keys[1], None),
            Joined::Value(vec![0u8; 34])
        );
        assert_eq!(cache.get_or_join(&"new".to_string(), None), Joined::Absent);
        // The absent key is read around the cache: a bypass, not a miss.
        assert_eq!(cache.stats(), (1, 0));
        assert_eq!(cache.churn().bypasses, 1);
        assert_eq!((cache.len(), cache.weight()), (4, 400));
        assert!(keys.iter().all(|key| cache.contains_key(key)));
        // No claim was kept: the next filling caller computes at once.
        assert_eq!(
            cache.get_or_insert_with_timeout(
                &"new".to_string(),
                std::time::Duration::ZERO,
                || Ok::<_, &str>(vec![1]),
                || "a claim was left behind",
            ),
            Ok(vec![1])
        );
    }

    /// A joining lookup waits for a fill another caller has in flight and
    /// takes its value; a short wait times out, and a failed fill leaves it
    /// absent rather than claimed.
    #[test]
    fn get_or_join_shares_a_fill_in_flight() {
        use std::{sync::mpsc, time::Duration};
        let cache = ByteBoundedCache::new(MIB, 1024, weigh_str);
        for (outcome, expected) in [
            (Ok(vec![42]), Joined::Value(vec![42])),
            (Err("offline"), Joined::Absent),
        ] {
            let key = format!("{outcome:?}");
            let (started_tx, started_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel::<()>();
            std::thread::scope(|scope| {
                let (cache, key, fill) = (&cache, &key, outcome.clone());
                let owner = scope.spawn(move || {
                    cache.get_or_insert_with(key, || {
                        started_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        fill
                    })
                });
                started_rx.recv().unwrap();
                assert_eq!(
                    cache.get_or_join(key, Some(Duration::from_millis(10))),
                    Joined::Timeout
                );
                let joiner = scope.spawn(move || cache.get_or_join(key, None));
                std::thread::sleep(Duration::from_millis(20));
                release_tx.send(()).unwrap();
                assert_eq!(owner.join().unwrap(), outcome);
                assert_eq!(joiner.join().unwrap(), expected);
            });
        }
        // Owner misses for both fills, the joined value's hit, the failed
        // fill's bypass; timeouts count nothing.
        assert_eq!(cache.stats(), (1, 2));
        assert_eq!(cache.churn().bypasses, 1);
        assert!(!cache.contains_key(&format!("{:?}", Err::<Vec<u8>, _>("offline"))));
    }

    #[test]
    fn untracked_and_manual_counting_compose() {
        let cache = ByteBoundedCache::new(MIB, 1024, weigh_str);
        cache.insert("a".to_string(), vec![1]);
        assert!(cache.get_untracked(&"a".to_string()).is_some());
        assert!(cache.contains_key(&"a".to_string()));
        assert_eq!(cache.stats(), (0, 0), "untracked lookups don't count");
        cache.record_hit();
        cache.record_miss();
        assert_eq!(cache.stats(), (1, 1));
    }

    #[test]
    fn env_mb_parses_and_falls_back() {
        // Unset → default.
        assert_eq!(env_mb("DS_CACHE_TEST_UNSET_VAR", 42), 42);
        // Set/whitespace/garbage via a uniquely-named var per case.
        std::env::set_var("DS_CACHE_TEST_SET_VAR", " 128 ");
        assert_eq!(env_mb("DS_CACHE_TEST_SET_VAR", 42), 128);
        std::env::set_var("DS_CACHE_TEST_BAD_VAR", "lots");
        assert_eq!(env_mb("DS_CACHE_TEST_BAD_VAR", 42), 42);
    }

    /// A registered cache is in the plan with what it holds now, and leaves
    /// it when dropped (#1306).
    #[test]
    fn a_registered_cache_reports_its_usage_until_dropped() {
        let _serial = plan::serial();
        let listed = || {
            plan::snapshot()
                .into_iter()
                .find(|row| row.entry.id == "ds_cache_test_registered")
        };
        let cache = ByteBoundedCache::new(MIB, 1024, weigh_str).registered(
            "ds_cache_test_registered",
            plan::Scope::Engine("test"),
            plan::SizeSource::Env("MC_DS_CACHE_TEST_MB"),
        );
        cache.insert("a".to_string(), vec![0u8; 36]); // 1 + 36 + 64 bytes
        assert!(cache.get(&"a".to_string()).is_some());
        assert!(cache.get(&"b".to_string()).is_none());
        let row = listed().expect("registered");
        assert_eq!(row.entry.kind, plan::Kind::Cache);
        assert_eq!(row.entry.source.to_string(), "env:MC_DS_CACHE_TEST_MB");
        assert_eq!(
            row.usage,
            plan::Usage {
                capacity_bytes: MIB,
                used_bytes: 101,
                entries: Some(1),
                hits: Some(1),
                misses: Some(1),
                churn: Some(Churn {
                    inserted_bytes: 101,
                    ..Churn::default()
                }),
            }
        );
        assert_eq!(row.usage, plan::Usage::cache(cache.metrics(), Some(1)));
        drop(cache);
        assert!(listed().is_none());
    }

    /// #1311: the memory plan reads a resized cache's capacity live, as
    /// `/admin/cache-plan` and the `/metrics` gauges then report it.
    #[test]
    fn the_plan_reports_a_resized_capacity() {
        let _serial = plan::serial();
        let cache = ByteBoundedCache::new(MIB, 1024, weigh_str).registered(
            "ds_cache_test_resized",
            plan::Scope::Engine("test"),
            plan::SizeSource::Env("MC_DS_CACHE_TEST_MB"),
        );
        cache.set_capacity(3 * MIB);
        let capacity = || {
            plan::snapshot()
                .into_iter()
                .find(|row| row.entry.id == "ds_cache_test_resized")
                .map(|row| row.usage.capacity_bytes)
        };
        assert_eq!(capacity(), Some(3 * MIB));
        cache.set_capacity(0);
        assert_eq!(capacity(), Some(0));
    }

    /// Weight = the value, in a one-shard cache of `entries` entries of
    /// 1000 bytes with a ghost ring.
    fn ghost_cache(entries: u64) -> ByteBoundedCache<String, u64> {
        ByteBoundedCache::new_single_shard(entries * 1000, 1000, weigh_declared).with_ghosts()
    }

    /// Look `key` up and insert it on a miss, as a cache's caller does.
    fn read_through(cache: &ByteBoundedCache<String, u64>, key: String) {
        if cache.get(&key).is_none() {
            cache.insert(key, 1000);
        }
    }

    /// #1308 acceptance: a cyclic access over 1.2 × the entries a cache
    /// holds asks for every evicted key again after 0.2 × its capacity of
    /// other evictions, so its misses are ghost hits within 0.5 × and the
    /// cache is labelled thrashing.
    ///
    /// The issue expected ~0 hits, an LRU's answer. `quick_cache` is not an
    /// LRU: its hot ring keeps most of the first pass and the rest churn
    /// through the cold ring, so about 70 % of the cycle hits here. Either
    /// way, every miss after the first pass is a ghost hit at the shallowest
    /// depth.
    #[test]
    fn cyclic_access_over_capacity_is_thrashing() {
        const N: u64 = 100;
        let cache = ghost_cache(N);
        let keys: Vec<String> = (0..N * 12 / 10).map(|i| format!("k{i}")).collect();
        for key in &keys {
            read_through(&cache, key.clone());
        }
        let start = plan::Usage::cache(cache.metrics(), None);
        for _ in 0..20 {
            for key in &keys {
                read_through(&cache, key.clone());
            }
        }
        let end = plan::Usage::cache(cache.metrics(), None);
        let window = plan::Activity::between(&start, &end, 3600).unwrap();
        let ghost_hits = window.churn.ghost_hits.unwrap();
        assert!(window.misses >= 20 * (N / 5), "{window:?}");
        assert_eq!(ghost_hits[0], window.misses, "{window:?}");
        assert_eq!(ghost_hits, [window.misses; 3]);
        assert!(window.churn.evictions() >= window.misses - 1, "{window:?}");
        assert_eq!(window.label(), Some(plan::Label::Thrashing), "{window:?}");
        assert_eq!(
            window.estimated_hit_ratios(),
            Some([1.0; 3]),
            "a cache 1.5 × as large holds the cycle"
        );
    }

    /// #1308 acceptance: one pass over 3 × the entries a cache holds evicts
    /// two thirds of them and asks for none again: no ghost hits. It turns
    /// over twice in the window and is labelled scan.
    #[test]
    fn one_pass_over_three_capacities_evicts_without_ghost_hits() {
        const N: u64 = 100;
        let cache = ghost_cache(N);
        let start = plan::Usage::cache(cache.metrics(), None);
        for i in 0..3 * N {
            read_through(&cache, format!("k{i}"));
        }
        let churn = cache.churn();
        assert_eq!(cache.stats(), (0, 3 * N));
        assert_eq!(churn.evictions(), 2 * N);
        assert_eq!(churn.evicted_bytes, 2 * N * 1000);
        assert_eq!(churn.inserted_bytes, 3 * N * 1000);
        assert_eq!(churn.ghost_hits, Some([0; 3]));
        let end = plan::Usage::cache(cache.metrics(), None);
        let window = plan::Activity::between(&start, &end, 3600).unwrap();
        assert_eq!(window.turnover_per_hour(), Some(3.0));
        assert_eq!(window.label(), Some(plan::Label::Scan));
    }

    /// Entries inserted once the hot ring is full enter the cold ring and,
    /// never read, leave it first: cold evictions.
    #[test]
    fn unread_inserts_past_the_hot_ring_are_cold_evictions() {
        let cache = ByteBoundedCache::new_single_shard(100 * 1000, 1000, weigh_declared);
        for i in 0..200 {
            cache.insert(format!("k{i}"), 1000);
        }
        let churn = cache.churn();
        assert_eq!((churn.evicted_cold, churn.evicted_hot), (100, 0));
        assert_eq!(churn.ghost_hits, None, "no ring unless asked for");
    }

    /// #1308 acceptance: an entry heavier than its shard's hot target is
    /// refused, not admitted, and counts as a refusal, not an eviction,
    /// whatever the shards and hot share; one at the target is admitted.
    #[test]
    fn a_refusal_is_not_an_eviction() {
        for (shards, hot_allocation) in [(Some(1), None), (Some(1), Some(0.5)), (None, None)] {
            let cache = ByteBoundedCache::build(
                64 * 1000,
                1000,
                weigh_declared,
                shards,
                hot_allocation,
                None,
            )
            .with_ghosts();
            let shard = cache.shard_capacity_bytes();
            let target = (shard as f64 * hot_allocation.unwrap_or(0.97)) as u64;
            cache.insert("resident".to_string(), 1000);
            cache.insert("too-big".to_string(), target + 1);
            assert!(!cache.contains_key(&"too-big".to_string()));
            // A refused fill of a claim, too.
            let refused =
                cache.get_or_insert_with(&"too-big-fill".to_string(), || Ok::<_, ()>(target + 1));
            assert_eq!(refused, Ok(target + 1));
            let churn = cache.churn();
            assert_eq!(churn.refused, 2, "{shards:?} {hot_allocation:?}");
            assert_eq!(churn.evictions(), 0);
            assert_eq!(churn.inserted_bytes, 1000);
            cache.insert("fits".to_string(), target);
            assert!(cache.contains_key(&"fits".to_string()));
            assert_eq!(cache.churn().refused, 2);
            assert_eq!(cache.churn().inserted_bytes, 1000 + target);
        }
    }

    /// A new value for a resident key replaces the old one: no eviction and
    /// nothing admitted, and a refused new value removes the old one without
    /// an eviction either.
    #[test]
    fn a_replaced_value_is_not_an_eviction() {
        let cache = ghost_cache(10);
        cache.insert("a".to_string(), 1000);
        cache.insert("a".to_string(), 900);
        assert_eq!(cache.weight(), 900);
        cache.insert("a".to_string(), 10 * 1000);
        assert!(!cache.contains_key(&"a".to_string()));
        let churn = cache.churn();
        assert_eq!(churn.evictions(), 0);
        assert_eq!((churn.inserted_bytes, churn.refused), (1000, 1));
    }

    /// Every fill path counts its insert, and every miss path looks the key
    /// up among recent evictions: `get`, the single-flight fills, a claim's
    /// guard, and `record_miss_for`; `record_miss` does not.
    #[test]
    fn every_fill_counts_and_every_keyed_miss_finds_ghosts() {
        let cache = ghost_cache(10);
        let keys: Vec<String> = (0..20).map(|i| format!("k{i}")).collect();
        for key in &keys {
            cache.insert(key.clone(), 1000);
        }
        let evicted: Vec<&String> = keys.iter().filter(|k| !cache.contains_key(*k)).collect();
        assert_eq!(evicted.len(), 10);
        assert!(cache.get(evicted[0]).is_none());
        let _ = cache.get_or_insert_with(evicted[1], || Ok::<_, ()>(1000));
        let _ = cache.get_or_insert_with_timeout(
            evicted[2],
            std::time::Duration::ZERO,
            || Ok::<_, ()>(1000),
            || (),
        );
        let CacheEntry::Vacant(claim) =
            cache.get_value_or_guard_untracked(evicted[3], std::time::Duration::ZERO)
        else {
            panic!("{} was evicted", evicted[3]);
        };
        cache.record_miss_for(evicted[3]);
        claim.insert(1000).unwrap();
        cache.record_miss();
        let churn = cache.churn();
        assert_eq!(churn.inserted_bytes, 23 * 1000);
        assert_eq!(cache.stats(), (0, 5));
        // Each missed key had 9 or 10 capacity-tenths evicted since its own
        // eviction began: within 1 ×, not 0.5 ×.
        assert_eq!(churn.ghost_hits, Some([0, 4, 4]), "{churn:?}");
    }

    /// A disabled cache counts lookups only: it admits nothing, so it
    /// refuses and evicts nothing either, and keeps no ring.
    #[test]
    fn a_disabled_cache_counts_no_churn() {
        let cache = ByteBoundedCache::new(0, 1000, weigh_declared).with_ghosts();
        cache.insert("a".to_string(), 1000);
        assert!(cache.get(&"a".to_string()).is_none());
        let _ = cache.get_or_insert_with(&"b".to_string(), || Ok::<_, ()>(1000));
        assert_eq!(cache.churn(), Churn::default());
        assert_eq!(cache.stats(), (0, 2));
    }

    /// The pre-warm flag travels with the cache's metrics.
    #[test]
    fn a_prewarmed_cache_says_so() {
        let cache = ByteBoundedCache::new(MIB, 1000, weigh_declared).prewarmed();
        assert!(cache.metrics().churn.prewarmed);
        assert!(!ghost_cache(1).churn().prewarmed);
    }

    // --- resize in place (#1311) ---

    /// Fill `cache` with `count` entries of 1000 bytes, keyed `k{i}`.
    fn fill_declared(cache: &ByteBoundedCache<String, u64>, count: u64) {
        for i in 0..count {
            cache.insert(format!("k{i}"), 1000);
        }
    }

    /// A shrink keeps what fits its new size, and reports it at once; a
    /// growth keeps everything.
    #[test]
    fn a_resize_keeps_what_fits() {
        let cache = ghost_cache(100);
        fill_declared(&cache, 100);
        assert_eq!((cache.len(), cache.weight()), (100, 100 * 1000));
        cache.set_capacity(200 * 1000);
        assert_eq!((cache.len(), cache.capacity_bytes()), (100, 200 * 1000));
        assert_eq!(cache.metrics().capacity_bytes, 200 * 1000);
        assert_eq!(cache.shard_capacity_bytes(), 200 * 1000);
        // Growing evicts nothing, and the growth is room for more.
        fill_declared(&cache, 200);
        assert_eq!((cache.len(), cache.churn().evictions()), (200, 0));

        cache.set_capacity(150 * 1000);
        // `quick_cache` evicts until the shard fits, and may evict a few hot
        // entries more as it moves read entries to the smaller hot ring.
        let kept = cache.len() as u64;
        assert!((145..=150).contains(&kept), "{kept}");
        assert_eq!(cache.weight(), kept * 1000);
        assert_eq!(cache.metrics().capacity_bytes, 150 * 1000);
        let churn = cache.churn();
        assert_eq!(churn.evictions(), 200 - kept);
        assert_eq!(churn.evicted_bytes, (200 - kept) * 1000);
        // The hot share stays the default, now of the smaller shard.
        assert!((cache.hot_allocation() - 0.97).abs() < 1e-6);
    }

    /// The steps of a shrink: at most a tenth of the starting capacity
    /// each, ending at the target.
    #[test]
    fn a_shrink_steps_a_tenth_at_a_time() {
        let steps = |from, to| shrink_steps(from, to).collect::<Vec<_>>();
        assert_eq!(steps(100, 0), [90, 80, 70, 60, 50, 40, 30, 20, 10, 0]);
        assert_eq!(steps(8192, 6144), [7372, 6552, 6144]);
        assert_eq!(steps(5, 0), [4, 3, 2, 1, 0]);
        assert_eq!(steps(1, 0), [0]);
        assert_eq!(steps(100, 100), Vec::<u64>::new());
    }

    /// One step of a shrink counts what it evicts exactly as `quick_cache`'s
    /// own `set_capacity` counted it under the shard lock before #1311:
    /// the same rings, bytes and remembered keys.
    #[test]
    fn a_shrink_counts_its_evictions_as_before() {
        let build = || {
            let cache = ghost_cache(100);
            fill_declared(&cache, 150);
            // Read a few, so some evictions come from the hot ring.
            for i in 50..60 {
                assert!(cache.get(&format!("k{i}")).is_some());
            }
            cache
        };
        let resized = build();
        resized.set_capacity(90 * 1000);
        // Before: quick_cache ends the request under the lock, unparked.
        // The counters then follow the size as a resize's do.
        let before = build();
        before.inner.cache.set_capacity(90 * 1000);
        let shard = before.inner.cache.shard_capacity();
        before.inner.counters().resize(90 * 1000, 100 * 1000, shard);
        let (now, then) = (resized.churn(), before.churn());
        assert!(now.evictions() > 0);
        assert_eq!(now, then);
        let resident = |cache: &ByteBoundedCache<String, u64>| {
            (0..150)
                .filter(|i| cache.contains_key(&format!("k{i}")))
                .collect::<Vec<_>>()
        };
        assert_eq!(resident(&resized), resident(&before));
        // The evicted keys are in the ring: a miss of each is a ghost hit.
        for cache in [&resized, &before] {
            for i in 0..150 {
                let _ = cache.get(&format!("k{i}"));
            }
        }
        assert_eq!(resized.churn(), before.churn());
        assert!(resized.churn().ghost_hits.unwrap()[0] > 0);
    }

    /// A shrink in steps counts every entry it evicts once, down to a
    /// disabled cache, which then counts nothing more.
    #[test]
    fn a_shrink_to_zero_counts_every_eviction_and_disables() {
        let cache = ghost_cache(100);
        fill_declared(&cache, 100);
        cache.set_capacity(0);
        let churn = cache.churn();
        assert_eq!((churn.evictions(), churn.evicted_bytes), (100, 100 * 1000));
        assert_eq!((cache.len(), cache.weight()), (0, 0));
        assert_eq!(cache.capacity_bytes(), 0);
        assert_eq!(cache.shard_capacity_bytes(), 0);
        cache.insert("a".to_string(), 1000);
        assert!(cache.get(&"a".to_string()).is_none());
        assert_eq!(cache.churn(), churn, "a disabled cache counts no churn");
    }

    /// A cache grown from disabled admits, counts and keeps a ghost ring
    /// it asked for, with `quick_cache`'s default hot share: the 1-byte
    /// clamp's share would be 1.
    #[test]
    fn a_disabled_cache_grows_with_the_default_hot_share() {
        let cache =
            ByteBoundedCache::new_single_shard_with_hot_allocation(0, 1000, weigh_declared, 0.5)
                .with_ghosts();
        assert!(!cache.resizes_in_place_to(100 * 1000));
        assert!(cache.resizes_in_place_to(0));
        cache.set_capacity(100 * 1000);
        assert!((cache.hot_allocation() - 0.97).abs() < 1e-6);
        assert!(cache.resizes_in_place_to(50 * 1000));
        fill_declared(&cache, 150);
        assert_eq!(cache.len(), 100);
        let churn = cache.churn();
        assert_eq!(churn.inserted_bytes, 150 * 1000);
        assert_eq!(churn.evictions(), 50);
        // The ring was started by the growth: misses of evicted keys hit it.
        for i in 0..150 {
            let _ = cache.get(&format!("k{i}"));
        }
        assert!(cache.churn().ghost_hits.unwrap()[0] > 0);
    }

    /// A resized cache counts inserts against its new hot target: an entry
    /// the smaller shard refuses is a refusal, and an insert that evicts an
    /// entry admitted under the larger target, now heavier than it, is
    /// counted with that eviction.
    #[test]
    fn a_resized_cache_counts_against_its_new_hot_target() {
        let cache = ghost_cache(100);
        cache.insert("big".to_string(), 70 * 1000);
        cache.set_capacity(72 * 1000);
        assert!(cache.contains_key(&"big".to_string()), "it still fits");
        assert!(cache.hot_allocation() * 72.0 * 1000.0 < 70.0 * 1000.0);
        cache.insert("refused".to_string(), 70 * 1000);
        assert_eq!(cache.churn().refused, 1);
        cache.insert("next".to_string(), 3 * 1000);
        assert!(!cache.contains_key(&"big".to_string()));
        let churn = cache.churn();
        assert_eq!(churn.refused, 1);
        assert_eq!(churn.inserted_bytes, 73 * 1000);
        assert_eq!((churn.evictions(), churn.evicted_bytes), (1, 70 * 1000));
    }

    /// A cache with an entry floor is never shrunk below four of its
    /// heaviest entry, except to disable it.
    #[test]
    fn an_entry_floor_keeps_room_for_four_of_the_heaviest_entry() {
        let cache =
            ByteBoundedCache::new_single_shard(100 * 1000, 1000, weigh_declared).with_entry_floor();
        cache.insert("scan".to_string(), 20 * 1000);
        cache.insert("small".to_string(), 1000);
        cache.set_capacity(10 * 1000);
        assert_eq!(cache.capacity_bytes(), FLOOR_ENTRIES * 20 * 1000);
        assert!(cache.contains_key(&"scan".to_string()));
        cache.set_capacity(0);
        assert_eq!(cache.capacity_bytes(), 0);
        // A floor above the size stops a shrink there, never grows it.
        let tight =
            ByteBoundedCache::new_single_shard(50 * 1000, 1000, weigh_declared).with_entry_floor();
        tight.insert("scan".to_string(), 20 * 1000);
        tight.set_capacity(40 * 1000);
        assert_eq!(tight.capacity_bytes(), 50 * 1000);
        // Without the floor the same shrink goes all the way.
        let plain = ByteBoundedCache::new_single_shard(100 * 1000, 1000, weigh_declared);
        plain.insert("scan".to_string(), 20 * 1000);
        plain.set_capacity(10 * 1000);
        assert_eq!(plain.capacity_bytes(), 10 * 1000);
        assert!(!plain.contains_key(&"scan".to_string()));
    }

    /// A cache built small with a large ceiling has the ceiling's shards:
    /// as many as a cache built at the ceiling, where a cache built small
    /// without one is a single shard.
    #[test]
    fn a_ceiling_fixes_the_shard_count() {
        const ENTRY: u64 = 256 * 1024;
        let shards = |cache: &ByteBoundedCache<String, u64>| cache.inner.cache.num_shards();
        let at_ceiling = ByteBoundedCache::new(1024 * MIB, ENTRY, weigh_declared);
        // 4096 items: four shards per CPU, at least four.
        assert!(shards(&at_ceiling) >= 4);
        for capacity in [0, MIB] {
            let plain = ByteBoundedCache::new(capacity, ENTRY, weigh_declared);
            assert_eq!(shards(&plain), 1, "{capacity}");
            let resizable =
                ByteBoundedCache::new_resizable(capacity, 1024 * MIB, ENTRY, weigh_declared);
            assert_eq!(shards(&resizable), shards(&at_ceiling), "{capacity}");
            assert_eq!(resizable.capacity_bytes(), capacity);
            resizable.set_capacity(1024 * MIB);
            assert_eq!(shards(&resizable), shards(&at_ceiling));
            assert_eq!(
                resizable.shard_capacity_bytes(),
                at_ceiling.shard_capacity_bytes()
            );
        }
        let hot = ByteBoundedCache::new_resizable_with_hot_allocation(
            MIB,
            1024 * MIB,
            ENTRY,
            weigh_declared,
            0.5,
        );
        assert_eq!(shards(&hot), shards(&at_ceiling));
        assert!((hot.hot_allocation() - 0.5).abs() < 1e-3);
        // A ceiling under the capacity is the capacity.
        let under = ByteBoundedCache::new_resizable(1024 * MIB, MIB, ENTRY, weigh_declared);
        assert_eq!(shards(&under), shards(&at_ceiling));
    }

    /// #1311: against a cache built at the new size without a ceiling, a
    /// shrink is in place with no more shards, a growth with at least a
    /// quarter of them; a disabled cache grown never is.
    #[test]
    fn a_resize_is_in_place_while_the_shards_suit_the_new_size() {
        const ENTRY: u64 = 256 * 1024;
        let fresh = |capacity| ByteBoundedCache::new(capacity, ENTRY, weigh_declared).shards();
        // Meta-tiles at 8 GiB: 32 768 items, four shards per CPU, at least
        // four. At 16 MiB a new cache has two: smaller slices refuse tiles.
        let large = ByteBoundedCache::new(8192 * MIB, ENTRY, weigh_declared);
        assert!(large.shards() >= 4 && fresh(16 * MIB) == 2);
        assert!(!large.resizes_in_place_to(16 * MIB));
        assert_eq!(
            large.resizes_in_place_to(6144 * MIB),
            large.shards() <= fresh(6144 * MIB)
        );
        assert!(large.resizes_in_place_to(8192 * MIB));
        assert!(large.resizes_in_place_to(0));
        // A growth keeps a quarter of a new cache's shards or more.
        let small = ByteBoundedCache::new(MIB, ENTRY, weigh_declared);
        assert_eq!(small.shards(), 1);
        for to in [4 * MIB, 32 * MIB, 8192 * MIB] {
            assert_eq!(small.resizes_in_place_to(to), 4 >= fresh(to), "{to}");
        }
        assert!(!small.resizes_in_place_to(8192 * MIB) || fresh(8192 * MIB) <= 4);
        // A ceiling is no excuse for a shrink to smaller slices.
        let ceiling = ByteBoundedCache::new_resizable(MIB, 8192 * MIB, ENTRY, weigh_declared);
        assert_eq!(ceiling.shards(), large.shards());
        assert!(!ceiling.resizes_in_place_to(MIB / 2));
        assert!(ceiling.resizes_in_place_to(8192 * MIB));
        let single = ByteBoundedCache::new_single_shard(256 * MIB, ENTRY, weigh_declared);
        assert!(single.resizes_in_place_to(MIB));
        assert!(single.resizes_in_place_to(1024 * 1024 * MIB));
        let disabled = ByteBoundedCache::new_single_shard(0, ENTRY, weigh_declared);
        assert!(!disabled.resizes_in_place_to(MIB));
        assert!(disabled.resizes_in_place_to(0));
    }

    /// A shrink stops where each shard's slice still holds the 32
    /// estimated entries a cache built there would give it, never grows
    /// the cache, and still disables it; a one-shard cache shrinks to any
    /// size. It returns the capacity applied.
    #[test]
    fn a_shrink_stops_where_the_slices_hold_32_entries() {
        let sharded =
            ByteBoundedCache::build(256 * 1000, 1000, weigh_declared, Some(4), None, None);
        assert_eq!(sharded.shards(), 4);
        let floor = (31 * 4 + 1) * 1000;
        assert_eq!(sharded.set_capacity(64 * 1000), floor);
        assert_eq!(sharded.capacity_bytes(), floor);
        // `quick_cache` itself gives a cache built at the floor 4 shards,
        // and one an entry smaller 2.
        let built = |bytes| {
            ByteBoundedCache::build(bytes, 1000, weigh_declared, Some(4), None, None).shards()
        };
        assert_eq!((built(floor), built(floor - 1000)), (4, 2));
        assert_eq!(sharded.set_capacity(100 * 1000), floor, "never grows");
        assert_eq!(sharded.set_capacity(0), 0);
        let single = ghost_cache(100);
        assert_eq!(single.set_capacity(1000), 1000);
    }

    /// #1311: a fill claimed while the cache is disabled carries no weight.
    /// Grown before the fill lands, the cache refuses a value heavier than
    /// its new hot target, and counts neither a refusal nor an eviction.
    #[test]
    fn a_fill_claimed_while_disabled_counts_nothing() {
        let cache = ByteBoundedCache::new_single_shard(0, 1000, weigh_declared).with_ghosts();
        let CacheEntry::Vacant(claim) =
            cache.get_value_or_guard_untracked("big", std::time::Duration::ZERO)
        else {
            panic!("a disabled cache claims every fill");
        };
        cache.set_capacity(10 * 1000);
        claim.insert(20 * 1000).unwrap();
        assert!(!cache.contains_key(&"big".to_string()));
        assert_eq!(cache.churn().evictions(), 0);
        assert_eq!(cache.churn().inserted_bytes, 0);
        // A plain insert weighed while enabled still counts.
        cache.insert("small".to_string(), 1000);
        assert_eq!(cache.churn().inserted_bytes, 1000);
    }

    /// A holding share keeps a fill at its own size and above; below, the
    /// cold ring is too small, and a share too large admits no fill.
    #[test]
    fn a_holding_share_keeps_a_fill_from_its_own_size_up() {
        let hot = hot_allocation_holding(256 * MIB, TENTH_DEGREE_GRID);
        assert!(holding_keeps(hot, 256 * MIB, TENTH_DEGREE_GRID));
        assert!(holding_keeps(hot, 1024 * MIB, TENTH_DEGREE_GRID));
        assert!(!holding_keeps(hot, 128 * MIB, TENTH_DEGREE_GRID));
        assert!(!holding_keeps(0.97, 256 * MIB, TENTH_DEGREE_GRID));
        assert!(holding_keeps(0.97, 1024 * MIB, TENTH_DEGREE_GRID));
        assert!(!holding_keeps(0.1, 64 * MIB, TENTH_DEGREE_GRID));
    }

    /// A value whose drop looks its key up in [`DROP_PROBE`] while armed: a
    /// drop under that cache's shard lock never returns.
    #[derive(Clone)]
    struct Probe(u64);

    static DROP_PROBE: std::sync::OnceLock<ByteBoundedCache<u64, Probe>> =
        std::sync::OnceLock::new();
    static PROBE_ARMED: AtomicBool = AtomicBool::new(false);
    static PROBED: AtomicU64 = AtomicU64::new(0);

    impl Drop for Probe {
        fn drop(&mut self) {
            if PROBE_ARMED.load(Ordering::SeqCst) {
                if let Some(cache) = DROP_PROBE.get() {
                    let _ = cache.contains_key(&self.0);
                    PROBED.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
    }

    /// The entries a shrink evicts are counted and dropped once the shard
    /// lock is released: a drop that reads the shard it was evicted from
    /// would otherwise never return.
    #[test]
    fn a_shrink_drops_its_evictions_outside_the_shard_lock() {
        let cache = DROP_PROBE.get_or_init(|| {
            ByteBoundedCache::new_single_shard(100 * 1000, 1000, |_, _| 1000).with_ghosts()
        });
        for i in 0..100 {
            cache.insert(i, Probe(i));
        }
        PROBE_ARMED.store(true, Ordering::SeqCst);
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            cache.set_capacity(40 * 1000);
            done.send(()).unwrap();
        });
        let shrunk = finished.recv_timeout(std::time::Duration::from_secs(30));
        PROBE_ARMED.store(false, Ordering::SeqCst);
        assert!(
            shrunk.is_ok(),
            "an evicted value dropped under the shard lock"
        );
        assert_eq!(PROBED.load(Ordering::SeqCst), 60);
        assert_eq!(cache.churn().evictions(), 60);
    }

    /// Blocks the first weighing, under a shard lock of [`GATED`]'s
    /// resize, of a key in shard [`GATED_SHARD`] in each of two steps,
    /// until the test releases it.
    static GATED: std::sync::OnceLock<ByteBoundedCache<u64, u64>> = std::sync::OnceLock::new();
    static GATED_SHARD: AtomicU64 = AtomicU64::new(u64::MAX);
    static GATE: std::sync::Mutex<(u64, bool)> = std::sync::Mutex::new((0, false));
    static GATE_MOVED: std::sync::Condvar = std::sync::Condvar::new();

    fn weigh_gated(key: &u64, value: &u64) -> u64 {
        let Some(cache) = GATED.get() else {
            return *value;
        };
        // Under a shard lock of a resize, `quick_cache` is still evicting:
        // its requests are being parked.
        let resizing = cache.inner.lifecycle.parking();
        let shard = cache.inner.cache.shard_index(key) as u64;
        if !resizing || shard != GATED_SHARD.load(Ordering::SeqCst) {
            return *value;
        }
        let mut gate = GATE.lock().unwrap();
        let step = cache.capacity_bytes();
        if gate.0 < 2 && !gate.1 && STEPS_SEEN.lock().unwrap().insert(step) {
            gate.1 = true;
            GATE_MOVED.notify_all();
            while gate.1 {
                gate = GATE_MOVED.wait(gate).unwrap();
            }
        }
        *value
    }

    /// The steps whose first weighing [`weigh_gated`] blocked.
    static STEPS_SEEN: std::sync::Mutex<std::collections::BTreeSet<u64>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());

    /// Wait until [`weigh_gated`] blocks a step, and return that step's
    /// starting capacity; fail after 30 s rather than hang.
    fn wait_for_gate(cache: &ByteBoundedCache<u64, u64>) -> u64 {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut gate = GATE.lock().unwrap();
        while !gate.1 {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!left.is_zero(), "the shrink never reached the gated shard");
            gate = GATE_MOVED.wait_timeout(gate, left).unwrap().0;
        }
        cache.capacity_bytes()
    }

    fn release_gate() {
        let mut gate = GATE.lock().unwrap();
        gate.0 += 1;
        gate.1 = false;
        GATE_MOVED.notify_all();
    }

    /// While a shrink holds one shard's lock, a lookup in another shard
    /// returns; and the shrink releases every lock between its steps: the
    /// second step starts after the first set the capacity it reached.
    #[test]
    fn lookups_go_on_while_a_shard_shrinks() {
        let cache = GATED.get_or_init(|| {
            ByteBoundedCache::build(256 * 1000, 1000, weigh_gated, Some(4), None, None)
        });
        assert_eq!(cache.inner.cache.num_shards(), 4);
        for i in 0..1024 {
            cache.insert(i, 1000);
        }
        let shard_of = |key: u64| cache.inner.cache.shard_index(&key) as u64;
        let resident: Vec<u64> = (0..1024).filter(|k| cache.contains_key(k)).collect();
        let held = shard_of(resident[0]);
        let other = *resident.iter().find(|k| shard_of(**k) != held).unwrap();
        GATED_SHARD.store(held, Ordering::SeqCst);
        // A lookup in another shard, on a thread of its own: it must return,
        // a hit or, once its shard has shrunk too, maybe a miss.
        let look_up = move || {
            let (found, returned) = std::sync::mpsc::channel();
            std::thread::spawn(move || found.send(cache.get(&other)).unwrap());
            returned.recv_timeout(std::time::Duration::from_secs(30))
        };
        let shrink = std::thread::spawn(move || cache.set_capacity(128 * 1000));

        assert_eq!(wait_for_gate(cache), 256 * 1000, "the first step");
        assert!(look_up().is_ok(), "blocked by another shard's shrink");
        release_gate();
        let second = wait_for_gate(cache);
        assert!(second < 256 * 1000 && second > 128 * 1000, "{second}");
        assert!(look_up().is_ok(), "blocked by another shard's shrink");
        release_gate();
        shrink.join().unwrap();
        assert_eq!(cache.capacity_bytes(), 128 * 1000);
        assert!(cache.weight() <= 128 * 1000);
    }
}
