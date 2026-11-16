//! A ghost ring's heap estimate (`Churn::ghost_ring_bytes`, #1314) against
//! the heap it holds, measured with a counting allocator: within 25 %, below
//! its cap of evictions and at it. One test: every thread's allocations
//! count, so nothing may run alongside.

use ds_cache::ByteBoundedCache;
use ds_core::heap::Counting;

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const TOLERANCE: f64 = 0.25;
const ENTRY: u64 = 64;

/// The heap a cache of `capacity` holds after `inserts` distinct keys of
/// `ENTRY` bytes, with a ghost ring or without, and its ring's estimate.
fn held(capacity: u64, inserts: u64, ring: bool) -> (isize, u64) {
    let before = Counting::live();
    let cache = ByteBoundedCache::new(capacity, ENTRY, |_: &u64, _: &()| ENTRY);
    let cache = if ring { cache.with_ghosts() } else { cache };
    for key in 0..inserts {
        cache.insert(key, ());
    }
    let held = Counting::live() - before;
    let estimate = cache.churn().ghost_ring_bytes;
    drop(cache);
    (held, estimate)
}

#[test]
fn a_rings_estimate_is_the_heap_it_holds() {
    let mib = 1u64 << 20;
    // 1 MiB reaches back over 2 MiB of evictions, 32 768 of them; 32 MiB
    // over 2^20, the cap, which 1.5 M evictions pass.
    for (capacity, inserts) in [(mib, 100_000), (32 * mib, 2_000_000)] {
        let (without, none) = held(capacity, inserts, false);
        let (with, estimate) = held(capacity, inserts, true);
        assert_eq!(none, 0, "no ring, no estimate");
        let measured = (with - without) as f64;
        let error = (estimate as f64 - measured).abs() / measured;
        eprintln!(
            "capacity {} MiB: ring {:.1} MiB measured, {:.1} MiB estimated",
            capacity / mib,
            measured / mib as f64,
            estimate as f64 / mib as f64
        );
        assert!(
            error <= TOLERANCE,
            "estimate {estimate}, measured {measured}"
        );
    }
}
