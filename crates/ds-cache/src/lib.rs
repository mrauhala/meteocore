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

use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};

/// Re-exported so call sites can name the borrowed-key-lookup bound without
/// depending on `quick_cache` directly.
pub use quick_cache::Equivalent;

/// One mebibyte, for `*_MB` → bytes conversions.
pub const MIB: u64 = 1024 * 1024;

/// Parse a cache size (in MB) from the environment: `MC_*_CACHE_MB`-style
/// variables are trimmed and parsed as `u64`, with unset/unparseable values
/// falling back to `default_mb`. `0` conventionally disables the cache.
pub fn env_mb(var: &str, default_mb: u64) -> u64 {
    std::env::var(var)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(default_mb)
}

/// Snapshot of a cache's counters and occupancy for `/metrics`.
///
/// `hits`/`misses` are cumulative since construction (the server's metrics
/// layer delta-tracks them into Prometheus counters); `bytes` is the current
/// resident weight; `capacity_bytes` is the **configured** budget — `0` for a
/// disabled cache, even though the underlying store is clamped to a 1-byte
/// budget internally (see [`ByteBoundedCache::new`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheMetrics {
    pub hits: u64,
    pub misses: u64,
    pub bytes: u64,
    pub capacity_bytes: u64,
}

/// Adapts a plain `fn(&K, &V) -> u64` to `quick_cache`'s `Weighter` trait so
/// call sites pass a function instead of hand-rolling a unit-struct impl.
struct WeighFn<K, V> {
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
///   and [`Self::record_hit`]/[`Self::record_miss`] let wrappers with custom
///   accounting (e.g. a negative-cache check between lookup and miss) keep
///   the counters truthful.
pub struct ByteBoundedCache<K: Eq + Hash, V: Clone> {
    cache: quick_cache::sync::Cache<K, V, WeighFn<K, V>>,
    capacity_bytes: u64,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl<K: Eq + Hash, V: Clone> ByteBoundedCache<K, V> {
    /// Build a cache with a budget of `capacity_bytes`. `approx_entry_bytes`
    /// is only a hash-map sizing hint (expected typical entry weight); the
    /// eviction budget is always `capacity_bytes`.
    pub fn new(capacity_bytes: u64, approx_entry_bytes: u64, weigh: fn(&K, &V) -> u64) -> Self {
        // `max(16)` keeps a small/zero capacity valid (a near-disabled cache
        // that holds nothing still needs a non-zero item estimate).
        let estimated_items = (capacity_bytes / approx_entry_bytes.max(1)).max(16) as usize;
        ByteBoundedCache {
            cache: quick_cache::sync::Cache::with_weighter(
                estimated_items,
                capacity_bytes.max(1),
                WeighFn { weigh },
            ),
            capacity_bytes,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Build a cache sized from the environment: `var` (MB, [`env_mb`]) with
    /// `default_mb` as the fallback. `0` disables retention.
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

    /// Look up `key`, counting a hit or a miss.
    pub fn get<Q>(&self, key: &Q) -> Option<V>
    where
        Q: Hash + Equivalent<K> + ?Sized,
    {
        let result = self.cache.get(key);
        if result.is_some() {
            self.hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
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
        self.cache.get(key)
    }

    /// Whether `key` is resident — no recency bump, no counter change.
    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        Q: Hash + Equivalent<K> + ?Sized,
    {
        self.cache.contains_key(key)
    }

    /// Insert an entry. At capacity 0 the entry is never admitted (see the
    /// type-level docs), so this is effectively a no-op there.
    pub fn insert(&self, key: K, value: V) {
        self.cache.insert(key, value);
    }

    /// Keep only the entries for which `keep` returns `true`, dropping the
    /// rest. Used for targeted invalidation (e.g. evicting one collection's
    /// entries on an incremental reload) — visits every entry, so call it on
    /// reload-frequency paths, not per request.
    pub fn retain(&self, keep: impl Fn(&K, &V) -> bool) {
        self.cache.retain(keep);
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
        let mut computed = false;
        let value = self.cache.get_or_insert_with(key, || {
            computed = true;
            self.misses.fetch_add(1, Ordering::Relaxed);
            with()
        })?;
        if !computed {
            self.hits.fetch_add(1, Ordering::Relaxed);
        }
        Ok(value)
    }

    /// Count one hit (for wrappers using [`Self::get_untracked`]).
    pub fn record_hit(&self) {
        self.hits.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one miss (for wrappers using [`Self::get_untracked`]).
    pub fn record_miss(&self) {
        self.misses.fetch_add(1, Ordering::Relaxed);
    }

    /// Cumulative `(hits, misses)`.
    pub fn stats(&self) -> (u64, u64) {
        (
            self.hits.load(Ordering::Relaxed),
            self.misses.load(Ordering::Relaxed),
        )
    }

    /// Current resident weight (bytes, as measured by the weight function).
    pub fn weight(&self) -> u64 {
        self.cache.weight()
    }

    /// The **configured** capacity in bytes (`0` = disabled), not the
    /// internal 1-byte clamp.
    pub fn capacity_bytes(&self) -> u64 {
        self.capacity_bytes
    }

    /// Number of resident entries.
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// Whether the cache currently holds no entries.
    pub fn is_empty(&self) -> bool {
        self.cache.len() == 0
    }

    /// Snapshot for `/metrics`.
    pub fn metrics(&self) -> CacheMetrics {
        let (hits, misses) = self.stats();
        CacheMetrics {
            hits,
            misses,
            bytes: self.weight(),
            capacity_bytes: self.capacity_bytes,
        }
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
}
