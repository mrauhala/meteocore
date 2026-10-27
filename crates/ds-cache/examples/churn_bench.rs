//! Lookup and insert cost of a `ByteBoundedCache` with and without its ghost
//! ring (#1308), against the bare `quick_cache` store it wrapped before.
//!
//! Run with `cargo run --release -p ds-cache --example churn_bench`.
//! Optional arguments: the thread count of the concurrent cases (default
//! 8), and `aa`, which builds every variant as `quick_cache`: the
//! differences it shows are the noise floor.
//!
//! Variants:
//! - `quick_cache`: the store with its default lifecycle and two atomic
//!   counters, as `ByteBoundedCache` was before #1308;
//! - `counters`: `ByteBoundedCache`, which counts evictions, refusals and
//!   admitted bytes on every insert;
//! - `ring`: `ByteBoundedCache::with_ghosts`, which also remembers each
//!   evicted key in the ring and looks each missed key up in it.
//!
//! Cases, in nanoseconds per operation:
//! - `hit`: `get` of a resident key;
//! - `miss, never held`: `get` of a key the cache never held;
//! - `miss, evicted`: `get` of a key evicted recently, which the ring finds;
//! - `read-through, scan`: `get`, then `insert` on a miss, cycling over
//!   8 × the entries the cache holds: nearly every lookup misses and every
//!   insert evicts, and the ring remembers too little to find them;
//! - `read-through, cycle`: the same over 1.2 × the entries, whose misses
//!   the ring finds;
//! - the two misses and the scan on N threads at once, each over its own
//!   keys (wall time per operation of one thread, the fastest of 30
//!   rounds).
//!
//! Each round times every variant once, in turn, over a short sample, and a
//! case reports the fastest of 300 rounds per variant: a sample another
//! process preempted only reads slower, so the minimum is the cost with
//! the machine to itself, and drift shows in all three alike. Keys are
//! 54-byte strings, like a meta-tile key's text; the cache holds 16 384
//! entries of 1000 bytes in its default shards.

use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use ds_cache::ByteBoundedCache;

const ENTRIES: usize = 16_384;
const WEIGHT: u64 = 1000;
/// A key's weight on top of [`WEIGHT`].
const KEY_BYTES: u64 = 54;
/// Calls per sample: short, so that some samples run unpreempted.
const OPS: usize = 4_000;
const ROUNDS: usize = 300;
const VARIANTS: usize = 3;
const CAPACITY: u64 = ENTRIES as u64 * (WEIGHT + KEY_BYTES);

#[allow(clippy::ptr_arg)]
fn weigh(key: &String, weight: &u64) -> u64 {
    *weight + key.len() as u64
}

fn key(i: usize) -> String {
    format!("metatile/radar-composite/2026-10-27T12:00:00Z/{i:08}")
}

fn keys(range: std::ops::Range<usize>) -> Vec<String> {
    range.map(key).collect()
}

#[derive(Clone)]
struct Weigher;

impl quick_cache::Weighter<String, u64> for Weigher {
    fn weight(&self, key: &String, weight: &u64) -> u64 {
        weigh(key, weight)
    }
}

/// The store as `ByteBoundedCache` wrapped it before #1308.
struct Bare {
    cache: quick_cache::sync::Cache<String, u64, Weigher>,
    hits: AtomicU64,
    misses: AtomicU64,
}

/// The operations each variant offers.
trait Cache: Sync {
    fn get(&self, key: &str) -> Option<u64>;
    fn insert(&self, key: String, weight: u64);
    fn contains(&self, key: &str) -> bool;

    fn read_through(&self, key: &str) {
        if self.get(key).is_none() {
            self.insert(key.to_owned(), WEIGHT);
        }
    }
}

impl Cache for Bare {
    fn get(&self, key: &str) -> Option<u64> {
        let found = self.cache.get(key);
        let counter = if found.is_some() {
            &self.hits
        } else {
            &self.misses
        };
        counter.fetch_add(1, Ordering::Relaxed);
        found
    }

    fn insert(&self, key: String, weight: u64) {
        self.cache.insert(key, weight);
    }

    fn contains(&self, key: &str) -> bool {
        self.cache.contains_key(key)
    }
}

impl Cache for ByteBoundedCache<String, u64> {
    fn get(&self, key: &str) -> Option<u64> {
        ByteBoundedCache::get(self, key)
    }

    fn insert(&self, key: String, weight: u64) {
        ByteBoundedCache::insert(self, key, weight);
    }

    fn contains(&self, key: &str) -> bool {
        self.contains_key(key)
    }
}

fn build(variant: usize, aa: bool) -> Box<dyn Cache> {
    let entry = WEIGHT + KEY_BYTES;
    match if aa { 0 } else { variant } {
        0 => Box::new(Bare {
            cache: quick_cache::sync::Cache::with_options(
                quick_cache::OptionsBuilder::new()
                    .estimated_items_capacity(ENTRIES)
                    .weight_capacity(CAPACITY)
                    .build()
                    .unwrap(),
                Weigher,
                Default::default(),
                Default::default(),
            ),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }),
        1 => Box::new(ByteBoundedCache::new(CAPACITY, entry, weigh)),
        _ => Box::new(ByteBoundedCache::new(CAPACITY, entry, weigh).with_ghosts()),
    }
}

/// Nanoseconds per call of `op` on each variant's cache, the fastest of
/// [`ROUNDS`] interleaved rounds of [`OPS`] calls; `op` gets the call's
/// index, which runs on across rounds.
fn time(caches: &[Box<dyn Cache>], op: impl Fn(&dyn Cache, usize)) -> [f64; VARIANTS] {
    let mut fastest = [f64::INFINITY; VARIANTS];
    for round in 0..ROUNDS {
        for (cache, fastest) in caches.iter().zip(&mut fastest) {
            let start = Instant::now();
            for i in round * OPS..(round + 1) * OPS {
                op(cache.as_ref(), i);
            }
            *fastest = fastest.min(start.elapsed().as_nanos() as f64 / OPS as f64);
        }
    }
    fastest
}

/// Like [`time`], with `threads` threads each calling `op` with its own
/// thread index and call index: wall time per call of one thread.
fn time_threads(
    caches: &[Box<dyn Cache>],
    threads: usize,
    op: impl Fn(&dyn Cache, usize, usize) + Sync,
) -> [f64; VARIANTS] {
    let per_thread = 4 * OPS;
    let mut fastest = [f64::INFINITY; VARIANTS];
    for round in 0..ROUNDS / 10 {
        for (cache, fastest) in caches.iter().zip(&mut fastest) {
            let start = Instant::now();
            std::thread::scope(|scope| {
                for thread in 0..threads {
                    let (op, cache) = (&op, cache.as_ref());
                    scope.spawn(move || {
                        for i in round * per_thread..(round + 1) * per_thread {
                            op(cache, thread, i);
                        }
                    });
                }
            });
            *fastest = fastest.min(start.elapsed().as_nanos() as f64 / per_thread as f64);
        }
    }
    fastest
}

/// Every variant's cache after inserting `keys` unread.
fn filled(keys: &[String], aa: bool) -> Vec<Box<dyn Cache>> {
    (0..VARIANTS)
        .map(|variant| {
            let cache = build(variant, aa);
            for key in keys {
                cache.insert(key.clone(), WEIGHT);
            }
            cache
        })
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let aa = args.iter().any(|arg| arg == "aa");
    let threads: usize = args.iter().find_map(|arg| arg.parse().ok()).unwrap_or(8);
    let mut rows: Vec<(String, [f64; VARIANTS])> = Vec::new();

    let resident = keys(0..ENTRIES / 2);
    let absent = keys(100_000_000..100_000_000 + ENTRIES / 2);
    let caches = filled(&resident, aa);
    rows.push((
        "hit".into(),
        time(&caches, |cache, i| {
            black_box(cache.get(&resident[i % resident.len()]));
        }),
    ));
    rows.push((
        "miss, never held".into(),
        time(&caches, |cache, i| {
            black_box(cache.get(&absent[i % absent.len()]));
        }),
    ));

    // Twice the entries inserted unread: half of them evicted, and
    // remembered by the ring.
    let twice = keys(0..2 * ENTRIES);
    let caches = filled(&twice, aa);
    let evicted: Vec<String> = twice
        .iter()
        .filter(|key| !caches[0].contains(key))
        .cloned()
        .collect();
    assert!(evicted.len() >= ENTRIES / 2);
    rows.push((
        "miss, evicted".into(),
        time(&caches, |cache, i| {
            black_box(cache.get(&evicted[i % evicted.len()]));
        }),
    ));

    let scan = keys(200_000_000..200_000_000 + 8 * ENTRIES);
    let caches = filled(&twice, aa);
    rows.push((
        "read-through, scan".into(),
        time(&caches, |cache, i| {
            cache.read_through(&scan[i % scan.len()])
        }),
    ));

    let cycle = keys(0..ENTRIES * 12 / 10);
    let caches = filled(&cycle, aa);
    rows.push((
        "read-through, cycle".into(),
        time(&caches, |cache, i| {
            cache.read_through(&cycle[i % cycle.len()])
        }),
    ));

    // Concurrent: each thread over its own share of the keys.
    let share = |set: &[String], thread: usize, i: usize| -> usize {
        let per = set.len() / threads;
        thread * per + i % per
    };
    let caches = filled(&twice, aa);
    rows.push((
        format!("miss, never held, {threads} threads"),
        time_threads(&caches, threads, |cache, thread, i| {
            black_box(cache.get(&absent[share(&absent, thread, i)]));
        }),
    ));
    rows.push((
        format!("miss, evicted, {threads} threads"),
        time_threads(&caches, threads, |cache, thread, i| {
            black_box(cache.get(&evicted[share(&evicted, thread, i)]));
        }),
    ));
    rows.push((
        format!("read-through, scan, {threads} threads"),
        time_threads(&caches, threads, |cache, thread, i| {
            cache.read_through(&scan[share(&scan, thread, i)]);
        }),
    ));

    if aa {
        println!("A/A: every variant is quick_cache; the differences are noise.");
    }
    println!(
        "| Case | quick_cache | counters | ring | counters vs quick_cache | ring vs counters |"
    );
    println!("|---|---|---|---|---|---|");
    for (name, [bare, counters, ring]) in rows {
        println!(
            "| {name} | {bare:.1} ns | {counters:.1} ns | {ring:.1} ns | {:+.1} % | {:+.1} % |",
            (counters / bare - 1.0) * 100.0,
            (ring / counters - 1.0) * 100.0,
        );
    }
}
