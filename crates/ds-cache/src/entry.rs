use std::hash::Hash;

use crate::churn::Counting;
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
pub struct CacheFillGuard<'a, K, V> {
    pub(crate) claim: Placeholder<'a, K, V>,
    /// The claimed key, to weigh the value it is filled with (`None` for a
    /// disabled cache, which counts nothing).
    pub(crate) key: Option<K>,
    pub(crate) lifecycle: &'a Counting<K, V>,
}

impl<K: Eq + Hash, V: Clone> CacheFillGuard<'_, K, V> {
    /// Publish the value to waiters and attempt cache retention. Returns the
    /// value if an explicit cache insertion/invalidation displaced this claim.
    /// Neither claiming nor publishing updates the cache's hit/miss counters;
    /// a retained value counts as inserted ([`crate::Churn`]).
    pub fn insert(self, value: V) -> Result<(), V> {
        let weight = self
            .key
            .as_ref()
            .map(|key| self.lifecycle.weigh(key, &value));
        crate::publish(self.lifecycle, self.claim, value, weight)
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
