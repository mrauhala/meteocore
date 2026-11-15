use std::hash::Hash;
use std::sync::Arc;
use std::time::Instant;

use crate::churn::{Counters, Counting};
use crate::Placeholder;

/// An uncounted lookup that separates single-flight ownership from computing.
pub enum CacheEntry<'a, K, V> {
    /// A cached value or the result of another caller's completed fill.
    Value(V),
    /// Exclusive ownership of this fill until insertion or guard drop.
    Vacant(CacheFillGuard<'a, K, V>),
    /// Another caller still owns the fill when the wait expires.
    Timeout,
}

/// What [`crate::ByteBoundedCache::get_or_join`] found: a lookup that shares
/// another caller's fill but never claims one of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Joined<V> {
    /// A resident value, or the value another caller's fill published while
    /// this caller waited.
    Value(V),
    /// Neither resident nor being filled, or the fill it waited for failed:
    /// the caller computes the value itself and the cache stays as it was.
    Absent,
    /// Another caller's fill was still running when the wait expired.
    Timeout,
}

/// A single-flight fill claim. Drop without inserting to permit a retry.
/// This holds no cache lock while the caller admits or computes a value.
///
/// The fill is timed from the claim to its insert, the cache's miss cost
/// ([`crate::Churn::fills`], #1314): a caller that waits for an admission
/// budget after claiming restarts the clock when it starts computing
/// ([`Self::restart`]). A claim dropped without an insert counts nothing.
pub struct CacheFillGuard<'a, K, V> {
    pub(crate) claim: Placeholder<'a, K, V>,
    /// The claimed key, to weigh the value it is filled with (`None` for a
    /// disabled cache, which counts nothing).
    pub(crate) key: Option<K>,
    pub(crate) lifecycle: &'a Counting<K, V>,
    pub(crate) started: Instant,
}

impl<K: Eq + Hash, V: Clone> CacheFillGuard<'_, K, V> {
    /// Publish the value to waiters and attempt cache retention. Returns the
    /// value if an explicit cache insertion/invalidation displaced this claim.
    /// Neither claiming nor publishing updates the cache's hit/miss counters;
    /// a retained value counts as inserted ([`crate::Churn`]). The fill
    /// counts with the time since the claim, or since [`Self::restart`].
    pub fn insert(self, value: V) -> Result<(), V> {
        self.insert_batched(value, 1)
    }

    /// [`Self::insert`] for one of `batch` claims a single computation
    /// filled (one GET of several blocks): it counts a fill of a
    /// `batch`th of the time since the claim.
    pub fn insert_batched(self, value: V, batch: u32) -> Result<(), V> {
        let elapsed = self.started.elapsed() / batch.max(1);
        self.lifecycle.counters().record_fills(1, elapsed);
        let weight = self
            .key
            .as_ref()
            .map(|key| self.lifecycle.weigh(key, &value));
        crate::publish(self.lifecycle, self.claim, value, weight)
    }

    /// Time the fill from now: for a caller that claims, then waits for an
    /// admission budget or other claims before it computes.
    pub fn restart(&mut self) {
        self.started = Instant::now();
    }
}

/// Times the fill of a missed entry for a cache whose caller looks a key up
/// and inserts it apart ([`crate::ByteBoundedCache::get`], then
/// [`crate::ByteBoundedCache::insert`]): take one at the miss, before
/// computing ([`crate::ByteBoundedCache::fill_timer`]), and finish it once
/// the value is inserted. Its time over the fills is the cache's miss cost
/// ([`crate::Churn::fills`], #1314). A fill that fails is dropped unfinished
/// and counts nothing: its time is not the cost of an entry the cache could
/// hold. One `Instant` and two atomic adds a fill; owned, so it can travel
/// with the computation to another task or thread.
#[must_use = "finish the timer once the value is inserted"]
pub struct FillTimer {
    pub(crate) counters: Arc<Counters>,
    pub(crate) started: Instant,
}

impl FillTimer {
    /// Count one fill, timed from the timer's start.
    pub fn finish(self) {
        self.finish_many(1);
    }

    /// Count `entries` fills that one computation since the timer's start
    /// filled together (a volume read decoding several moments), each at
    /// an equal share of the time. `0` counts nothing.
    pub fn finish_many(self, entries: u64) {
        self.counters.record_fills(entries, self.started.elapsed());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ByteBoundedCache, MIB};
    use std::time::Duration;

    #[test]
    fn uncounted_claims_can_be_abandoned_moved_and_retried() {
        let cache = ByteBoundedCache::new(MIB, 1024, |_: &String, _: &u64| 128);
        let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let CacheEntry::Vacant(_guard) =
                cache.get_value_or_guard_untracked("a", Duration::ZERO)
            else {
                panic!("expected an unowned fill");
            };
            assert!(matches!(
                cache.get_value_or_guard_untracked("a", Duration::ZERO),
                CacheEntry::Timeout
            ));
            let CacheEntry::Vacant(other) = cache.get_value_or_guard_untracked("b", Duration::ZERO)
            else {
                panic!("an unrelated key must remain available");
            };
            other.insert(7).unwrap();
            panic!("failed resource admission or compute");
        }));
        assert!(failed.is_err());
        let CacheEntry::Vacant(guard) = cache.get_value_or_guard_untracked("a", Duration::ZERO)
        else {
            panic!("unwinding must release the claim");
        };
        std::thread::scope(|scope| {
            scope
                .spawn(move || guard.insert(42).unwrap())
                .join()
                .unwrap();
        });
        assert!(matches!(
            cache.get_value_or_guard_untracked("a", Duration::ZERO),
            CacheEntry::Value(42)
        ));
        assert_eq!(
            cache.stats(),
            (0, 0),
            "admission probes must not count fills"
        );
        cache.record_miss();
        cache.record_hit();
        assert_eq!(cache.stats(), (1, 1));
    }
}
