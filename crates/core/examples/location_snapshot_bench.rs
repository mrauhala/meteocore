//! Per-request cost of an EDR location inventory (#1083): the old copy of
//! the whole list against the shared [`LocationSnapshot`], on a synthetic
//! list of 200 000 stations.
//!
//! Run with `cargo run --release -p ds-core --example location_snapshot_bench`.
//! Optional argument: the station count.
//!
//! Each case is the list handling of one request path, before and after
//! `EdrEngine::get_locations` returned the snapshot. Before, every call
//! handed the request a copy of the list (`Vec<Location>`); after, an `Arc`
//! clone. The response encoding is the same either way and is left out.
//!
//! - `/locations` page: copy, slice a page / share, slice a page.
//! - `/locations?bbox=` page: copy, `retain` the box / share, collect
//!   references to the locations in the box.
//! - `items/{id}` (`EdrMembers::new`): copy, keep the id in a map / look it
//!   up.
//! - `items` page of 10: the same for the page's 10 ids.
//! - location list of 10 ids, one unknown (`require_known_location`): copy,
//!   build a set of every id / look each up.
//! - its GeoJSON names (`station_names`): copy, build the point index, scan
//!   for each id / look each up.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::{HashMap, HashSet};
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ds_core::model::{Location, LocationSnapshot};

/// The system allocator, counting the bytes requested: what one request
/// allocates, the copy included.
struct Counting;

static ALLOCATED: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call forwards to `System` with the caller's arguments; the
// counter is a static atomic, so counting never allocates or re-enters the
// allocator.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        unsafe { System.dealloc(p, layout) }
    }
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATED.fetch_add(new_size.saturating_sub(layout.size()), Ordering::Relaxed);
        unsafe { System.realloc(p, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Median time and mean bytes allocated per call of `op`, timed in batches
/// of `batch` calls so a call far shorter than the clock's resolution is
/// still measured.
fn measure<T>(samples: usize, batch: usize, mut op: impl FnMut() -> T) -> (Duration, usize) {
    black_box(op());
    let mut times = Vec::with_capacity(samples);
    let before = ALLOCATED.load(Ordering::Relaxed);
    for _ in 0..samples {
        let started = Instant::now();
        for _ in 0..batch {
            black_box(op());
        }
        times.push(started.elapsed() / batch as u32);
    }
    let bytes = (ALLOCATED.load(Ordering::Relaxed) - before) / (samples * batch);
    times.sort_unstable();
    (times[times.len() / 2], bytes)
}

/// A WIGOS-style id and a station name, spread over a box of Finland. The
/// ids are a permutation of `0..n`, so the snapshot's id sort sees an
/// unsorted list, as a database's row order may be.
fn stations(n: usize) -> Vec<Location> {
    (0..n)
        .map(|i| Location {
            id: format!("0-20000-0-{:06}", i * 7919 % n),
            label: format!("Station name {i:06}"),
            latitude: 59.0 + (i % 1000) as f64 * 0.011,
            longitude: 20.0 + (i / 1000) as f64 * 0.055,
        })
        .collect()
}

/// The pre-#1083 `get_locations`: a copy of the engine's list per call.
fn copy_of(inventory: &[Location]) -> Vec<Location> {
    inventory.to_vec()
}

/// The pre-#1083 point index of `station_names` (`geojson::LocationIndex`).
fn point_index(locations: &[Location]) -> HashMap<(u64, u64), Option<usize>> {
    let mut by_point = HashMap::with_capacity(locations.len());
    for (i, l) in locations.iter().enumerate() {
        by_point
            .entry(((l.longitude + 0.0).to_bits(), (l.latitude + 0.0).to_bits()))
            .and_modify(|slot| *slot = None)
            .or_insert(Some(i));
    }
    by_point
}

/// The pre-#1083 `EdrMembers::new`: the page's items that are locations.
fn members_by_copy(inventory: &[Location], ids: &[&str]) -> HashMap<String, Location> {
    let wanted: HashSet<&str> = ids.iter().copied().collect();
    copy_of(inventory)
        .into_iter()
        .filter(|l| wanted.contains(l.id.as_str()))
        .map(|l| (l.id.clone(), l))
        .collect()
}

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .map(|s| s.parse().expect("a station count"))
        .unwrap_or(200_000);
    assert!(n >= 2000, "at least 2000 stations");
    assert!(
        !n.is_multiple_of(7919),
        "the ids permute 0..n by 7919, a prime"
    );
    let inventory = stations(n);

    let list = inventory.clone();
    let started = Instant::now();
    let shared = Arc::new(LocationSnapshot::new(list));
    println!(
        "{n} stations: snapshot built once per load or refresh in {:.1} ms; \
         its id index is {} B a station",
        started.elapsed().as_secs_f64() * 1e3,
        std::mem::size_of::<usize>()
    );

    let page = n / 2..n / 2 + 100;
    let in_box =
        |l: &Location| (20.0..=21.0).contains(&l.longitude) && (59.0..=60.0).contains(&l.latitude);
    let id = |i: usize| format!("0-20000-0-{:06}", i % n);
    let one = id(n / 3);
    let ten_owned: Vec<String> = (0..10).map(|k| id(k * n / 10 + 7)).collect();
    let ten: Vec<&str> = ten_owned.iter().map(String::as_str).collect();
    let mut list: Vec<&str> = ten[..9].to_vec();
    list.push("0-20000-0-unknown");

    type Case<'a> = (
        &'a str,
        Box<dyn FnMut() -> usize + 'a>,
        Box<dyn FnMut() -> usize + 'a>,
    );
    let cases: Vec<Case> = vec![
        (
            "/locations page (limit=100)",
            Box::new(|| {
                let locs = copy_of(&inventory);
                black_box(&locs[page.clone()]).len()
            }),
            Box::new(|| {
                let locs = Arc::clone(&shared);
                black_box(&locs[page.clone()]).len()
            }),
        ),
        (
            "/locations?bbox= page (limit=100)",
            Box::new(|| {
                let mut locs = copy_of(&inventory);
                locs.retain(|l| in_box(l));
                black_box(&locs[..100]).len()
            }),
            Box::new(|| {
                let locs = Arc::clone(&shared);
                let kept: Vec<&Location> = locs.iter().filter(|l| in_box(l)).collect();
                black_box(&kept[..100]).len()
            }),
        ),
        (
            "items/{id}",
            Box::new(|| members_by_copy(&inventory, &[one.as_str()]).len()),
            Box::new(|| {
                let locs = Arc::clone(&shared);
                usize::from(black_box(locs.by_id(&one)).is_some())
            }),
        ),
        (
            "items page of 10",
            Box::new(|| members_by_copy(&inventory, &ten).len()),
            Box::new(|| {
                let locs = Arc::clone(&shared);
                ten.iter().filter(|id| locs.by_id(id).is_some()).count()
            }),
        ),
        (
            "locations/{10 ids}, one unknown",
            Box::new(|| {
                let known: HashSet<String> =
                    copy_of(&inventory).into_iter().map(|l| l.id).collect();
                list.iter().filter(|id| known.contains(**id)).count()
            }),
            Box::new(|| {
                let locs = Arc::clone(&shared);
                list.iter().filter(|id| locs.contains(id)).count()
            }),
        ),
        (
            "GeoJSON names of 10 ids",
            Box::new(|| {
                let locs = copy_of(&inventory);
                let by_point = point_index(&locs);
                black_box(&by_point);
                ten.iter()
                    .filter_map(|id| locs.iter().find(|l| l.id == *id))
                    .count()
            }),
            Box::new(|| {
                let locs = Arc::clone(&shared);
                ten.iter().filter_map(|id| locs.by_id(id)).count()
            }),
        ),
    ];

    println!(
        "{:<36} {:>12} {:>12} {:>12} {:>12} {:>9}",
        "per request", "copy", "copy alloc", "borrow", "borrow alloc", "speedup"
    );
    for (name, mut old, mut new) in cases {
        assert_eq!(old(), new(), "{name}: both answer the same");
        let (old_time, old_bytes) = measure(25, 1, &mut old);
        let (new_time, new_bytes) = measure(25, 2000, &mut new);
        println!(
            "{name:<36} {:>9.2} ms {:>9.1} MB {:>9} ns {:>10} B {:>8.0}x",
            old_time.as_secs_f64() * 1e3,
            old_bytes as f64 / 1e6,
            new_time.as_nanos(),
            new_bytes,
            old_time.as_secs_f64() / new_time.as_secs_f64().max(1e-12),
        );
    }
}
