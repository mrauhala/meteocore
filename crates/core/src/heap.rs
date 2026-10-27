//! Estimates of the heap an in-memory store holds (#1307): the BUFR report
//! store, the WIS2 dedup and deletion indexes, the CAP accumulator and
//! catalog, the PVOL catalog. Weighed the way cache entries are: what each
//! value owns, buffer capacities rather than lengths, plus the tables of
//! the maps that hold them. An estimate: a hash map's table and a B-tree's
//! nodes follow the standard library's layout, which no API states, and
//! the allocator's size classes are not counted.
//!
//! A store computes its figure when it changes and keeps it, so `/metrics`
//! reads a number, never walks the store (Critical Rule 10). [`Counting`]
//! is the allocator the stores' tests measure their estimates against.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::mem::{align_of, size_of};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Arc;

use chrono::{DateTime, TimeZone};

use crate::feature::{Feature, Geometry, PropertyValue, Related, RelatedTarget};
use crate::model::Location;

/// Bytes of an `Arc`'s strong and weak counts, ahead of its value.
pub const ARC_COUNTS: usize = 2 * size_of::<usize>();

/// Heap bytes a value owns.
pub trait HeapSize {
    /// The heap this value owns, without `size_of::<Self>()`, which the
    /// value's holder counts: a `Vec` its buffer, a map its table. An `Arc`
    /// counts its value as owned; a store whose entries share one counts it
    /// once itself.
    fn heap_size(&self) -> usize;
}

macro_rules! no_heap {
    ($($t:ty),*) => {
        $(impl HeapSize for $t {
            fn heap_size(&self) -> usize {
                0
            }
        })*
    };
}

no_heap!(
    bool,
    char,
    u8,
    u16,
    u32,
    u64,
    u128,
    usize,
    i8,
    i16,
    i32,
    i64,
    i128,
    isize,
    f32,
    f64,
    ()
);

impl<Tz: TimeZone> HeapSize for DateTime<Tz> {
    fn heap_size(&self) -> usize {
        0
    }
}

impl HeapSize for String {
    fn heap_size(&self) -> usize {
        self.capacity()
    }
}

impl HeapSize for Box<str> {
    fn heap_size(&self) -> usize {
        self.len()
    }
}

impl HeapSize for Arc<str> {
    fn heap_size(&self) -> usize {
        (ARC_COUNTS + self.len()).next_multiple_of(align_of::<usize>())
    }
}

impl<T: HeapSize> HeapSize for Box<T> {
    fn heap_size(&self) -> usize {
        size_of::<T>() + (**self).heap_size()
    }
}

impl<T: HeapSize> HeapSize for Box<[T]> {
    fn heap_size(&self) -> usize {
        size_of_val::<[T]>(self) + self.iter().map(HeapSize::heap_size).sum::<usize>()
    }
}

impl<T: HeapSize> HeapSize for Arc<T> {
    fn heap_size(&self) -> usize {
        (ARC_COUNTS + size_of::<T>()).next_multiple_of(align_of::<usize>().max(align_of::<T>()))
            + (**self).heap_size()
    }
}

impl<T: HeapSize> HeapSize for Arc<[T]> {
    fn heap_size(&self) -> usize {
        (ARC_COUNTS + size_of_val::<[T]>(self))
            .next_multiple_of(align_of::<usize>().max(align_of::<T>()))
            + self.iter().map(HeapSize::heap_size).sum::<usize>()
    }
}

impl<T: HeapSize> HeapSize for Option<T> {
    fn heap_size(&self) -> usize {
        self.as_ref().map_or(0, HeapSize::heap_size)
    }
}

impl<T: HeapSize> HeapSize for Vec<T> {
    fn heap_size(&self) -> usize {
        self.capacity() * size_of::<T>() + self.iter().map(HeapSize::heap_size).sum::<usize>()
    }
}

impl<T: HeapSize> HeapSize for VecDeque<T> {
    fn heap_size(&self) -> usize {
        self.capacity() * size_of::<T>() + self.iter().map(HeapSize::heap_size).sum::<usize>()
    }
}

impl<T: HeapSize, const N: usize> HeapSize for [T; N] {
    fn heap_size(&self) -> usize {
        self.iter().map(HeapSize::heap_size).sum()
    }
}

impl<A: HeapSize, B: HeapSize> HeapSize for (A, B) {
    fn heap_size(&self) -> usize {
        self.0.heap_size() + self.1.heap_size()
    }
}

impl<A: HeapSize, B: HeapSize, C: HeapSize> HeapSize for (A, B, C) {
    fn heap_size(&self) -> usize {
        self.0.heap_size() + self.1.heap_size() + self.2.heap_size()
    }
}

impl<K: HeapSize, V: HeapSize, S> HeapSize for HashMap<K, V, S> {
    fn heap_size(&self) -> usize {
        hash_table::<(K, V)>(self.capacity())
            + self
                .iter()
                .map(|(k, v)| k.heap_size() + v.heap_size())
                .sum::<usize>()
    }
}

impl<T: HeapSize, S> HeapSize for HashSet<T, S> {
    fn heap_size(&self) -> usize {
        hash_table::<T>(self.capacity()) + self.iter().map(HeapSize::heap_size).sum::<usize>()
    }
}

impl<K: HeapSize, V: HeapSize> HeapSize for BTreeMap<K, V> {
    fn heap_size(&self) -> usize {
        btree::<K, V>(self.len())
            + self
                .iter()
                .map(|(k, v)| k.heap_size() + v.heap_size())
                .sum::<usize>()
    }
}

impl<T: HeapSize> HeapSize for BTreeSet<T> {
    fn heap_size(&self) -> usize {
        btree::<T, ()>(self.len()) + self.iter().map(HeapSize::heap_size).sum::<usize>()
    }
}

/// The table of a hash map or set of `capacity` (its `capacity()`) holding
/// entries of type `T` (`(K, V)` for a map): one slot and one control byte
/// per bucket, and a group of control bytes more. hashbrown keeps 7 of 8
/// buckets usable, all of them below 8.
pub fn hash_table<T>(capacity: usize) -> usize {
    if capacity == 0 {
        return 0;
    }
    let buckets = if capacity < 8 {
        (capacity + 1).next_power_of_two()
    } else {
        (capacity / 7 * 8).next_power_of_two()
    };
    const GROUP: usize = 16;
    (buckets * size_of::<T>()).next_multiple_of(GROUP) + buckets + GROUP
}

/// The nodes of a `BTreeMap<K, V>` of `len` entries, grown by insertion as
/// a store grows (`collect` builds full nodes instead). A leaf holds up to
/// 11 entries and an internal node up to 12 children, a leaf with 12 child
/// pointers more. Filled in key order, a full node splits when one more
/// arrives, keeping 6 and handing one to its parent: every node but the
/// last holds 7 of what its level counts. Filled out of order, nodes hold
/// more, about 70 % of their capacity, and this overestimates by up to a
/// quarter.
pub fn btree<K, V>(len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    const CAPACITY: usize = 11;
    // The parent pointer, two `u16`s, then the keys and the values.
    let leaf = ((size_of::<usize>() + 2 * size_of::<u16>() + CAPACITY * size_of::<K>())
        .next_multiple_of(align_of::<V>())
        + CAPACITY * size_of::<V>())
    .next_multiple_of(align_of::<usize>());
    let internal = leaf + (CAPACITY + 1) * size_of::<usize>();
    let nodes = |items: usize, capacity: usize| {
        if items <= capacity {
            1
        } else {
            1 + (items - capacity).div_ceil(7)
        }
    };
    let leaves = nodes(len, CAPACITY);
    let mut internals = 0;
    let mut level = leaves;
    while level > 1 {
        level = nodes(level, CAPACITY + 1);
        internals += level;
    }
    leaves * leaf + internals * internal
}

impl HeapSize for Geometry {
    fn heap_size(&self) -> usize {
        match self {
            Geometry::Point { .. } | Geometry::Null => 0,
            Geometry::Polygon { exterior, holes } => exterior.heap_size() + holes.heap_size(),
            Geometry::MultiPolygon { polygons } => polygons.heap_size(),
        }
    }
}

impl HeapSize for PropertyValue {
    fn heap_size(&self) -> usize {
        match self {
            PropertyValue::String(s) => s.heap_size(),
            PropertyValue::List(values) => values.heap_size(),
            PropertyValue::Object(pairs) => pairs.heap_size(),
            PropertyValue::Float(_)
            | PropertyValue::Integer(_)
            | PropertyValue::Bool(_)
            | PropertyValue::Null => 0,
        }
    }
}

impl HeapSize for RelatedTarget {
    fn heap_size(&self) -> usize {
        match self {
            RelatedTarget::Collection(id) | RelatedTarget::EdrLocation(id) => id.heap_size(),
        }
    }
}

impl HeapSize for Related {
    fn heap_size(&self) -> usize {
        self.target.heap_size() + self.title.heap_size()
    }
}

impl HeapSize for Feature {
    fn heap_size(&self) -> usize {
        self.id.heap_size()
            + self.geometry.heap_size()
            + self.properties.heap_size()
            + self.related.heap_size()
    }
}

impl HeapSize for Location {
    fn heap_size(&self) -> usize {
        self.id.heap_size() + self.label.heap_size()
    }
}

/// Bytes live through [`Counting`]: allocated less freed.
static LIVE: AtomicIsize = AtomicIsize::new(0);

/// A global allocator over [`System`] that counts the bytes live through
/// it, as requested: what a store's estimate is measured against. Install
/// it in an integration test binary of its own, with one test, since every
/// thread's allocations count:
///
/// ```ignore
/// #[global_allocator]
/// static ALLOCATOR: ds_core::heap::Counting = ds_core::heap::Counting;
///
/// let before = ds_core::heap::Counting::live();
/// let store = build();
/// let held = ds_core::heap::Counting::live() - before;
/// ```
pub struct Counting;

impl Counting {
    /// Bytes allocated through this allocator and not yet freed.
    pub fn live() -> isize {
        LIVE.load(Ordering::SeqCst)
    }
}

// SAFETY: every call forwards to `System` with the caller's arguments; the
// counter is bookkeeping only.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        // SAFETY: as `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        // SAFETY: `ptr` came from this allocator, so from `System`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as `dealloc`.
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            LIVE.fetch_add(
                new_size as isize - layout.size() as isize,
                Ordering::Relaxed,
            );
        }
        new
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_and_vectors_count_their_capacity() {
        let mut s = String::with_capacity(40);
        s.push_str("abc");
        assert_eq!(s.heap_size(), 40);
        let v: Vec<String> = vec!["abcd".to_string(), String::with_capacity(10)];
        assert_eq!(v.heap_size(), 2 * size_of::<String>() + 4 + 10);
        let boxed: Box<[f32]> = vec![0.0; 22].into_boxed_slice();
        assert_eq!(boxed.heap_size(), 88);
        let id: Arc<str> = Arc::from("0-20000-0-02598");
        assert_eq!(
            id.heap_size(),
            32,
            "16 bytes of counts and 15 of text, aligned"
        );
    }

    /// hashbrown's bucket counts: 3 usable of 4, 7 of 8, then 7 of 8 per
    /// power of two.
    #[test]
    fn hash_tables_follow_hashbrown_bucket_counts() {
        assert_eq!(hash_table::<u64>(0), 0);
        assert_eq!(hash_table::<u64>(3), 32 + 4 + 16);
        assert_eq!(hash_table::<u64>(7), 64 + 8 + 16);
        assert_eq!(hash_table::<u64>(14), 16 * 8 + 16 + 16);
        assert_eq!(hash_table::<u64>(28), 32 * 8 + 32 + 16);
        let mut map: HashMap<u64, u64> = HashMap::new();
        map.insert(1, 2);
        assert_eq!(map.heap_size(), hash_table::<(u64, u64)>(map.capacity()));
    }

    #[test]
    fn btrees_grow_by_nodes() {
        let leaf = btree::<u64, u64>(1);
        assert_eq!(leaf, 8 + 4 + 11 * 16 + 4);
        assert_eq!(btree::<u64, u64>(11), leaf);
        assert!(btree::<u64, u64>(12) > 2 * leaf);
        // 24 entries in key order: leaves of 6, 6 and 10 under one root.
        let internal = leaf + 12 * 8;
        assert_eq!(btree::<u64, u64>(24), 3 * leaf + internal);
        // About one leaf per 7 entries, and an internal node per 7 below.
        let big = btree::<u64, u64>(7_000);
        assert!(big > 1_000 * leaf + 160 * internal && big < 1_000 * leaf + 175 * internal);
    }

    #[test]
    fn features_count_their_shared_parts_once_each() {
        let feature = Feature {
            id: "abc".into(),
            geometry: Arc::new(Geometry::Polygon {
                exterior: vec![[0.0, 0.0]; 5],
                holes: Vec::new(),
            }),
            properties: Arc::new(HashMap::from([(
                "name".to_string(),
                PropertyValue::String("x".into()),
            )])),
            related: Vec::new(),
        };
        let properties = hash_table::<(String, PropertyValue)>(feature.properties.capacity());
        assert_eq!(
            feature.heap_size(),
            3 + (16 + size_of::<Geometry>() + 5 * 16)
                + (16 + size_of::<HashMap<String, PropertyValue>>() + properties + 4 + 1)
        );
    }
}
