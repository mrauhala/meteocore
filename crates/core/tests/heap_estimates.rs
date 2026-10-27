//! `ds_core::heap`'s estimates of the standard containers against the heap
//! they hold, measured with its counting allocator (#1307). One test: every
//! thread's allocations count, so nothing else may run alongside.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use ds_core::heap::{Counting, HeapSize};

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// `estimate` within `tolerance` of the bytes `build` leaves live.
fn check<T: HeapSize>(name: &str, tolerance: f64, build: impl FnOnce() -> T) {
    let before = Counting::live();
    let value = build();
    let held = (Counting::live() - before) as f64;
    let estimate = value.heap_size() as f64;
    let error = (estimate - held).abs() / held;
    println!(
        "{name}: held {held}, estimated {estimate} ({:+.1} %)",
        100.0 * (estimate - held) / held
    );
    assert!(
        error <= tolerance,
        "{name}: estimate {estimate} is {:.1} % off the {held} bytes held",
        100.0 * error
    );
    drop(value);
}

fn t(i: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_700_000_000 + i * 3600, 0).unwrap()
}

#[test]
fn container_estimates_match_the_heap_they_hold() {
    // Exact: buffers only.
    check("vec of strings", 0.0, || {
        (0..1000)
            .map(|i| format!("station-{i}"))
            .collect::<Vec<_>>()
    });
    check("boxed rows", 0.0, || {
        (0..1000)
            .map(|_| vec![0f32; 22].into_boxed_slice())
            .collect::<Vec<_>>()
    });
    check("arc str", 0.0, || {
        (0..100)
            .map(|i| Arc::<str>::from(format!("0-20000-0-{i:05}")))
            .collect::<Vec<_>>()
    });
    // Tables and nodes: estimated.
    check("hash map", 0.05, || {
        (0..10_000)
            .map(|i| (format!("data-id-{i}"), (t(i), i as u64)))
            .collect::<HashMap<_, _>>()
    });
    check("hash set", 0.05, || {
        (0..10_000)
            .map(|i| format!("id-{i}"))
            .collect::<HashSet<_>>()
    });
    // B-trees grown by insertion, as stores grow; `collect` would build
    // them with full nodes instead.
    check("btree in key order", 0.10, || {
        let mut map = BTreeMap::new();
        for i in 0..10_000 {
            map.insert((t(i), i as u64), format!("data-id-{i}"));
        }
        map
    });
    check("btree rows per station", 0.10, || {
        let mut stations = HashMap::new();
        for s in 0..500 {
            let mut rows: BTreeMap<DateTime<Utc>, Box<[f32]>> = BTreeMap::new();
            for h in 0..24 {
                rows.insert(t(h), vec![0f32; 22].into_boxed_slice());
            }
            stations.insert(format!("station-{s}"), rows);
        }
        stations
    });
    // Out of key order nodes fill fuller: the estimate is high.
    check("btree out of order", 0.25, || {
        let mut map = BTreeMap::new();
        for i in 0..10_000u64 {
            map.insert(i.wrapping_mul(2_654_435_761) % 1_000_003, i);
        }
        map
    });
    check("deque", 0.0, || {
        (0..1000).map(|i| format!("{i}")).collect::<VecDeque<_>>()
    });
}
