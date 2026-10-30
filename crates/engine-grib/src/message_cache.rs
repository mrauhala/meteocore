//! Optional compressed-message retention behind the decoded-grid cache.
//!
//! Long time windows can exceed the grid cache. Keeping the smaller source
//! messages lets the next coordinate reuse their bytes, paying only decoding.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ds_cache::{ByteBoundedCache, CacheEntry, CacheMetrics, MIB};
use ds_core::{deadline, error::DataServerError};
use ds_storage::{bytes::Bytes, object_store::path::Path, DataStore};

use crate::{
    cache::{DecodedGrid, GridCache, LARGEST_GRID_NODES, URL_ALLOWANCE},
    catalog::MessageEntry,
    reader,
};

/// One indexed message: source-local path, offset and indexed length. The
/// CCSDS point index cache keys its entries the same way (#1300).
#[derive(Clone, Hash, PartialEq, Eq)]
pub(crate) struct MessageKey {
    path: Arc<str>,
    offset: u64,
    // A changed indexed range must not reuse bytes from an earlier range.
    length: Option<u64>,
}

impl MessageKey {
    pub(crate) fn new(path: &str, entry: &MessageEntry) -> Self {
        Self {
            path: Arc::from(path),
            offset: entry.offset,
            length: entry.length,
        }
    }

    /// The key's own bytes and its path, for cache weights.
    pub(crate) fn size_bytes(&self) -> usize {
        self.path.len() + size_of::<Self>()
    }
}

fn weight(key: &MessageKey, bytes: &Bytes) -> u64 {
    entry_weight(bytes.len(), key.size_bytes())
}

/// A cached message's weight: the owned payload, the key's `key_bytes`
/// ([`MessageKey::size_bytes`]) and Arc/cache-node overhead.
fn entry_weight(payload: usize, key_bytes: usize) -> u64 {
    (payload + key_bytes + size_of::<Bytes>() + 96) as u64
}

/// The weight the cache gives the message of `length` bytes at `path`, for
/// the cache's demand before it is read (#1309).
pub(crate) fn message_weight(path: &str, length: u64) -> u64 {
    length.saturating_add(entry_weight(0, path.len() + size_of::<MessageKey>()))
}

pub(crate) struct MessageCache {
    cache: ByteBoundedCache<MessageKey, Bytes>,
    /// What the pre-warm fetched and requests then read (#1335).
    prewarmed: Prewarmed,
}

/// The most pre-warmed messages [`Prewarmed`] waits on a first read of.
/// Each pre-warm first forgets those the cache no longer holds, so this
/// binds only a cache of tiny messages; a fetch past it is not counted when
/// read.
const MAX_UNREAD: usize = 4096;

/// Messages the pre-warm fetched that no request has read yet, and how many
/// it fetched that one then read (#1335): how much of what it fetches is
/// used.
#[derive(Default)]
struct Prewarmed {
    unread: Mutex<HashSet<MessageKey>>,
    /// `unread`'s length, so a hit skips the lock while nothing waits.
    pending: AtomicUsize,
    hits: AtomicU64,
}

impl Prewarmed {
    /// A request read `key` from the cache: count it if the pre-warm
    /// fetched it and no request had read it yet.
    fn read(&self, key: &MessageKey) {
        if self.pending.load(Ordering::Relaxed) == 0 {
            return;
        }
        let mut unread = self.unread.lock().unwrap();
        if unread.remove(key) {
            self.pending.store(unread.len(), Ordering::Relaxed);
            self.hits.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl MessageCache {
    /// One shard: a position query plans whether its messages stay resident
    /// (#1301, [`Self::holds`]). Sharded from a 0.5 MiB entry hint, a
    /// 1024 MiB cache was four shards per CPU, 64 of 16 MiB on 12 CPUs, so
    /// a 41-step 0.1° window (55 MB) that fits the total could still
    /// overflow the shards its messages hash to. Lookups are one per field
    /// or map tile, so one lock costs nothing. A hot share whose cold ring
    /// holds a message the size of a decoded 0.1° field, which bounds a
    /// packed one: a fill heavier than that ring would evict itself
    /// (#1332). From 1 GiB that is `quick_cache`'s default. A ghost ring
    /// tells whether a larger cache would keep a window's messages (#1308).
    pub fn new(size_mb: u64) -> Option<Self> {
        (size_mb > 0).then(|| {
            let capacity = size_mb.saturating_mul(MIB);
            let largest = GridCache::estimated_weight(LARGEST_GRID_NODES);
            Self {
                cache: ByteBoundedCache::new_single_shard_with_hot_allocation(
                    capacity,
                    MIB / 2,
                    weight,
                    ds_cache::hot_allocation_holding(capacity, largest),
                )
                .with_ghosts(),
                prewarmed: Prewarmed::default(),
            }
        })
    }

    /// Mark the cache as one the pre-warm fills ahead of its reads (#1335):
    /// its eviction and ghost-hit numbers undervalue those messages
    /// ([`ByteBoundedCache::prewarmed`]).
    pub(crate) fn prewarmed(self) -> Self {
        Self {
            cache: self.cache.prewarmed(),
            ..self
        }
    }

    /// The cache's whole budget in bytes.
    pub(crate) fn capacity_bytes(&self) -> u64 {
        self.cache.capacity_bytes()
    }

    /// Whether [`Self::resize`] to `bytes` leaves the cache one built at
    /// that size would be: its hot share, kept from its build, still keeps
    /// a fill as large as a decoded 0.1° field (#1311, as
    /// [`GridCache::resizes_in_place_to`]).
    pub(crate) fn resizes_in_place_to(&self, bytes: u64) -> bool {
        let largest = GridCache::estimated_weight(LARGEST_GRID_NODES);
        let hot = self.cache.hot_allocation();
        let fresh = ds_cache::hot_allocation_holding(bytes, largest);
        self.cache.resizes_in_place_to(bytes)
            && ((fresh - hot).abs() < 1e-6 || ds_cache::holding_keeps(hot, bytes, largest))
    }

    /// Resize to `bytes` in place, keeping the messages that fit (#1311).
    /// The pre-warm's share follows the new budget.
    pub(crate) fn resize(&self, bytes: u64) {
        self.cache.set_capacity(bytes);
    }

    /// Whether `key` is resident, without touching its recency or a counter.
    pub(crate) fn is_resident(&self, key: &MessageKey) -> bool {
        self.cache.contains_key(key)
    }

    /// Whether `messages` messages of `bytes` together stay resident: their
    /// weight, each key counted with a long path ([`URL_ALLOWANCE`]),
    /// within one shard's budget, all of it for this one-shard cache. A
    /// window read with `window` set ([`Self::get_with`]) then does.
    pub(crate) fn holds(&self, messages: usize, bytes: u64) -> bool {
        let overhead = entry_weight(0, URL_ALLOWANCE + size_of::<MessageKey>());
        (messages as u64)
            .saturating_mul(overhead)
            .saturating_add(bytes)
            <= self.cache.shard_capacity_bytes()
    }

    /// The message's bytes when resident, counting a hit; nothing when
    /// absent, for a reader that then fetches only part of the message and
    /// never fills the cache (#1301).
    pub(crate) fn get_resident(&self, key: &MessageKey) -> Option<Bytes> {
        let bytes = self.cache.get_untracked(key)?;
        self.cache.record_hit();
        self.prewarmed.read(key);
        Some(bytes)
    }

    pub fn read(
        &self,
        store: &DataStore,
        path: &Path,
        entry: &MessageEntry,
    ) -> Result<DecodedGrid, DataServerError> {
        let (bytes, decoded) = self.get_with(store, path, entry, false, |bytes| {
            reader::decode_message(bytes, &entry.param)
        })?;
        let grid = match decoded {
            Some(grid) => grid, // reuse the validating decode on a cold fill
            None => reader::decode_message(&bytes, &entry.param)?,
        };
        deadline::check()?;
        Ok(grid)
    }

    /// The message's bytes, one fetch shared by concurrent readers. A miss
    /// fetches them and admits them only once `validate` accepts them, so a
    /// truncated, corrupt or unsupported payload stays retryable, like a
    /// failed GET. Returns what `validate` produced on that fill, `None` on
    /// a hit: [`Self::read`] decodes, a CCSDS point read (#1300) indexes.
    ///
    /// `window`: the message is one of a window [`Self::holds`] approved,
    /// which the query samples from its own copy and reads again on its
    /// next run. Its fill is kept ([`ByteBoundedCache::keep`]): promoted to
    /// the hot ring rather than evicted from the cold one by the window's
    /// later fills. Unkept, a full cache held only the last 3 % of a window
    /// (#1340). A map's fills are not kept: a one-off read leaves first.
    pub(crate) fn get_with<T>(
        &self,
        store: &DataStore,
        path: &Path,
        entry: &MessageEntry,
        window: bool,
        validate: impl FnOnce(&[u8]) -> Result<T, DataServerError>,
    ) -> Result<(Bytes, Option<T>), DataServerError> {
        deadline::check()?;
        let key = MessageKey::new(path.as_ref(), entry);
        let mut validated = None;
        let bytes = self.cache.get_or_insert_with(&key, || {
            let bytes = reader::fetch_message_bytes(store, path, entry)?;
            deadline::check()?;
            validated = Some(validate(&bytes)?);
            deadline::check()?;
            // Object stores can return a slice backed by the entire file.
            // Retain exactly this message's bytes so the byte budget is real.
            // Oversized values are never admitted: avoid copying those buffers.
            Ok::<_, DataServerError>(if weight(&key, &bytes) <= self.cache.capacity_bytes() {
                Bytes::copy_from_slice(&bytes)
            } else {
                bytes
            })
        })?;
        if validated.is_none() {
            // A hit, or another reader's fill: the first read of a
            // pre-warmed message counts (#1335).
            self.prewarmed.read(&key);
        } else if window {
            self.cache.keep(key, bytes.clone());
        }
        deadline::check()?;
        Ok((bytes, validated))
    }

    /// Fill `key` ahead of its first read (#1335): `fetch` the message and
    /// admit it once `validate` accepts it, as [`Self::get_with`] does,
    /// kept ([`ByteBoundedCache::keep`]). In a full cache a new entry is
    /// admitted to the cold ring, which the pre-warm's next fill would evict
    /// it from unread; kept, it is promoted to the hot ring instead, so a
    /// pre-warm does not evict its own earlier fills.
    ///
    /// The fetch runs outside the single-flight: a request for the message
    /// meanwhile fetches it itself rather than wait on a background GET
    /// past its deadline, and the pre-warm then leaves the request's copy.
    /// Returns the bytes fetched; `None` without a fetch for a resident
    /// message. Counts no hit and no miss: no request asked for it.
    pub(crate) fn prewarm(
        &self,
        key: &MessageKey,
        fetch: impl FnOnce() -> Result<Bytes, DataServerError>,
        validate: impl FnOnce(&[u8]) -> Result<(), DataServerError>,
    ) -> Result<Option<usize>, DataServerError> {
        if self.cache.contains_key(key) {
            return Ok(None);
        }
        let bytes = fetch()?;
        validate(&bytes)?;
        let fetched = bytes.len();
        let CacheEntry::Vacant(fill) = self.cache.get_value_or_guard_untracked(key, Duration::ZERO)
        else {
            return Ok(Some(fetched));
        };
        // An exact copy, as `get_with` admits; the byte cap keeps a
        // pre-warmed message far below the cache's capacity.
        let bytes = Bytes::copy_from_slice(&bytes);
        let _ = fill.insert(bytes.clone());
        self.cache.keep(key.clone(), bytes);
        if self.cache.contains_key(key) {
            let mut unread = self.prewarmed.unread.lock().unwrap();
            if unread.len() < MAX_UNREAD {
                unread.insert(key.clone());
                self.prewarmed
                    .pending
                    .store(unread.len(), Ordering::Relaxed);
            }
        }
        Ok(Some(fetched))
    }

    /// Stop waiting on a first read of pre-warmed messages the cache has
    /// evicted unread. Once per pre-warm, before it fetches.
    pub(crate) fn forget_evicted_prewarms(&self) {
        let mut unread = self.prewarmed.unread.lock().unwrap();
        unread.retain(|key| self.cache.contains_key(key));
        self.prewarmed
            .pending
            .store(unread.len(), Ordering::Relaxed);
    }

    /// Pre-warmed messages a request then read, each counted at its first
    /// read (#1335).
    pub(crate) fn prewarm_hits(&self) -> u64 {
        self.prewarmed.hits.load(Ordering::Relaxed)
    }

    pub fn metrics(&self) -> CacheMetrics {
        self.cache.metrics()
    }
}

#[cfg(test)]
mod tests;
