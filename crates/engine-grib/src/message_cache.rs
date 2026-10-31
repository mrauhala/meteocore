//! Optional compressed-message retention behind the decoded-grid cache.
//!
//! Long time windows can exceed the grid cache. Keeping the smaller source
//! messages lets the next coordinate reuse their bytes, paying only decoding.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

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
    prewarmed: Unread,
    /// What the map read-ahead admitted and readers have not yet accepted
    /// (#1335).
    read_ahead: AheadFills,
    /// The messages whose fill a map read-ahead holds right now.
    filling: Filling,
}

/// The most messages an [`Unread`] or [`AheadFills`] waits on. Each
/// pre-warm first forgets those the cache no longer holds, as does a
/// read-ahead fill that finds the set full, so this binds only a cache of
/// tiny messages; a fetch past it is not counted when read.
const MAX_UNREAD: usize = 4096;

/// Messages the pre-warm fetched that no request has read yet, and how
/// many it fetched that one then read (#1335): how much of what it fetches
/// is used.
#[derive(Default)]
struct Unread {
    unread: Mutex<HashSet<MessageKey>>,
    /// `unread`'s length, so a hit skips the lock while nothing waits.
    pending: AtomicUsize,
    hits: AtomicU64,
}

impl Unread {
    /// A request read `key` from the cache: count it if the fill fetched
    /// it and no request had read it yet.
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

    /// Wait on a first read of `key`, just fetched. A full set first
    /// forgets the messages `cache` no longer holds.
    fn insert(&self, key: &MessageKey, cache: &ByteBoundedCache<MessageKey, Bytes>) {
        let mut unread = self.unread.lock().unwrap();
        if unread.len() >= MAX_UNREAD {
            unread.retain(|key| cache.contains_key(key));
        }
        if unread.len() < MAX_UNREAD {
            unread.insert(key.clone());
        }
        self.pending.store(unread.len(), Ordering::Relaxed);
    }

    /// A request filled `key` itself: what the pre-warm fetched of it is
    /// gone, so a later read of the request's copy counts nothing.
    fn forget(&self, key: &MessageKey) {
        if self.pending.load(Ordering::Relaxed) == 0 {
            return;
        }
        let mut unread = self.unread.lock().unwrap();
        unread.remove(key);
        self.pending.store(unread.len(), Ordering::Relaxed);
    }

    /// Stop waiting on a first read of the messages `cache` has evicted.
    fn forget_evicted(&self, cache: &ByteBoundedCache<MessageKey, Bytes>) {
        let mut unread = self.unread.lock().unwrap();
        unread.retain(|key| cache.contains_key(key));
        self.pending.store(unread.len(), Ordering::Relaxed);
    }
}

/// The messages the map read-ahead admitted without decoding them (#1335),
/// until a reader accepts their bytes, decoding or indexing them, or the
/// cache drops them. Their weight counts against the read-ahead's room
/// until a request reads them; that first read counts a hit.
#[derive(Default)]
struct AheadFills {
    fills: Mutex<HashMap<MessageKey, AheadFill>>,
    /// `fills`' length, so a lookup skips the lock while there are none.
    pending: AtomicUsize,
    hits: AtomicU64,
}

#[derive(Clone, Copy)]
struct AheadFill {
    /// Its weight in the cache.
    weight: u64,
    /// Whether a request has read it.
    read: bool,
    /// When it was admitted.
    at: Instant,
}

impl AheadFills {
    fn tracking(&self) -> bool {
        self.pending.load(Ordering::Relaxed) > 0
    }

    fn update(&self, fills: &HashMap<MessageKey, AheadFill>) {
        self.pending.store(fills.len(), Ordering::Relaxed);
    }

    /// A request read `key`, joining its fill: its first read counts.
    fn read(&self, key: &MessageKey) {
        if !self.tracking() {
            return;
        }
        let mut fills = self.fills.lock().unwrap();
        if let Some(fill) = fills.get_mut(key).filter(|fill| !fill.read) {
            fill.read = true;
            self.hits.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A request filled `key` itself: an earlier fill of it was evicted,
    /// and the request's copy is no read-ahead's.
    fn forget(&self, key: &MessageKey) {
        if !self.tracking() {
            return;
        }
        let mut fills = self.fills.lock().unwrap();
        fills.remove(key);
        self.update(&fills);
    }
}

/// The messages a map read-ahead is fetching (#1335), each with a signal
/// its fill raises once it is admitted, or not. The read-ahead holds no
/// single-flight claim: `quick_cache` admits a claimed fill that had
/// waiters as read, so a request joining it would promote it to the hot
/// ring. A request that finds a message marked waits on its signal instead
/// ([`MessageCache::get_with`]), for at most half its remaining time, and
/// then reads the fill as its first read, without touching its recency.
#[derive(Default)]
struct Filling {
    keys: Mutex<HashMap<MessageKey, Arc<Filled>>>,
    /// `keys`' length, so a lookup skips the lock while nothing fills.
    pending: AtomicUsize,
}

/// A read-ahead fill's signal: raised when its fetch is done, admitted or
/// not.
#[derive(Default)]
struct Filled {
    done: Mutex<bool>,
    raised: Condvar,
    /// Requests that waited for it, for tests to wait on.
    #[cfg(test)]
    waiters: AtomicUsize,
}

impl Filled {
    /// Wait for the signal, at most `timeout` (`None`: as long as the fill
    /// takes, which the background budget bounds): whether it was raised.
    fn wait(&self, timeout: Option<Duration>) -> bool {
        let done = self.done.lock().unwrap();
        #[cfg(test)]
        self.waiters.fetch_add(1, Ordering::SeqCst);
        match timeout {
            None => *self.raised.wait_while(done, |done| !*done).unwrap(),
            Some(timeout) => {
                let (done, _) = self
                    .raised
                    .wait_timeout_while(done, timeout, |done| !*done)
                    .unwrap();
                *done
            }
        }
    }
}

impl Filling {
    fn contains(&self, key: &MessageKey) -> bool {
        self.pending.load(Ordering::Relaxed) > 0 && self.keys.lock().unwrap().contains_key(key)
    }

    /// The signal of `key`'s fill in flight, if one is.
    fn signal(&self, key: &MessageKey) -> Option<Arc<Filled>> {
        if self.pending.load(Ordering::Relaxed) == 0 {
            return None;
        }
        self.keys.lock().unwrap().get(key).cloned()
    }

    /// Mark `key` filled ahead until the mark drops, which raises its
    /// signal; `None` when it is marked already.
    fn mark(&self, key: &MessageKey) -> Option<FillMark<'_>> {
        let mut keys = self.keys.lock().unwrap();
        if keys.contains_key(key) {
            return None;
        }
        let filled = Arc::new(Filled::default());
        keys.insert(key.clone(), filled.clone());
        self.pending.store(keys.len(), Ordering::Relaxed);
        Some(FillMark {
            filling: self,
            key: key.clone(),
            filled,
        })
    }
}

struct FillMark<'a> {
    filling: &'a Filling,
    key: MessageKey,
    filled: Arc<Filled>,
}

impl Drop for FillMark<'_> {
    fn drop(&mut self) {
        {
            let mut keys = self.filling.keys.lock().unwrap();
            keys.remove(&self.key);
            self.filling.pending.store(keys.len(), Ordering::Relaxed);
        }
        *self.filled.done.lock().unwrap() = true;
        self.filled.raised.notify_all();
    }
}

/// What a request finds of a read-ahead fetching the message it reads.
enum Join {
    /// None is.
    None,
    /// The fetch is done; its fill, if admitted, waits for its first read.
    Done,
    /// It did not finish within half the request's remaining time.
    TimedOut,
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
                prewarmed: Unread::default(),
                read_ahead: AheadFills::default(),
                filling: Filling::default(),
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

    /// The room an unkept fill has in the cache once it is full: its cold
    /// ring, what the hot share leaves of the one shard. A map's fill and a
    /// read-ahead's wait there until read; about 27–31 MiB up to 1 GiB, the
    /// size of a decoded 0.1° field (#1332), then 3 %, as below 52 MiB.
    pub(crate) fn cold_room(&self) -> u64 {
        let shard = self.cache.shard_capacity_bytes() as f64;
        ((1.0 - self.cache.hot_allocation()) * shard) as u64
    }

    /// The message's bytes when resident, counting a hit; nothing when
    /// absent, for a reader that then fetches only part of the message and
    /// never fills the cache (#1301). The caller reports how its bytes
    /// parsed ([`Self::decoded`]).
    pub(crate) fn get_resident(&self, key: &MessageKey) -> Option<Bytes> {
        if let Some(bytes) = self.first_read_ahead(key) {
            return Some(bytes);
        }
        let bytes = self.cache.get_untracked(key)?;
        self.cache.record_hit();
        self.note_read(key);
        Some(bytes)
    }

    /// A request read `key` from the cache: its first read of a message a
    /// background fill fetched counts (#1335).
    fn note_read(&self, key: &MessageKey) {
        self.prewarmed.read(key);
        self.read_ahead.read(key);
    }

    /// A request's first read of a message the read-ahead admitted (#1335):
    /// its bytes, peeked without touching its recency, counting a hit. A
    /// map's own fill is read from the filler's copy and never looked up
    /// again, so it stays in the cold ring unreferenced and leaves first;
    /// read with a lookup, a read-ahead fill would be promoted to the hot
    /// ring, and an animation would evict what the hot ring keeps. Later
    /// reads count and promote as usual. `None` when `key` is no unread
    /// read-ahead fill, or one evicted unread, which is forgotten.
    fn first_read_ahead(&self, key: &MessageKey) -> Option<Bytes> {
        if !self.read_ahead.tracking() {
            return None;
        }
        let mut fills = self.read_ahead.fills.lock().unwrap();
        let fill = fills.get_mut(key).filter(|fill| !fill.read)?;
        if let Some(bytes) = self.cache.peek(key) {
            fill.read = true;
            self.read_ahead.hits.fetch_add(1, Ordering::Relaxed);
            self.cache.record_hit();
            return Some(bytes);
        }
        // Not yet published: the request joins the fill.
        if !self.filling.contains(key) {
            fills.remove(key);
            self.read_ahead.update(&fills);
        }
        None
    }

    /// How the bytes of `key` a reader took from the cache parsed (#1335):
    /// a message the read-ahead admitted undecoded is accepted once one
    /// decodes or indexes, and evicted when one cannot, so the next read
    /// fetches it again. A message a reader admitted itself, already
    /// validated, stays whatever another path makes of it, and a deadline
    /// says nothing about the bytes.
    pub(crate) fn decoded<T>(&self, key: &MessageKey, result: &Result<T, DataServerError>) {
        if !self.read_ahead.tracking() {
            return;
        }
        let mut fills = self.read_ahead.fills.lock().unwrap();
        if !fills.contains_key(key) {
            return;
        }
        match result {
            Ok(_) => {
                fills.remove(key);
            }
            Err(DataServerError::DeadlineExceeded) => {}
            Err(_) => {
                fills.remove(key);
                self.cache.remove(key);
            }
        }
        self.read_ahead.update(&fills);
    }

    /// The weight of what the read-ahead admitted since `since` that no
    /// request has read yet (#1335), its fills the cache evicted first
    /// forgotten. Older ones may stay resident, but in a cache that has not
    /// evicted them they do not crowd the cold ring.
    pub(crate) fn read_ahead_unread(&self, since: Instant) -> u64 {
        if !self.read_ahead.tracking() {
            return 0;
        }
        let mut fills = self.read_ahead.fills.lock().unwrap();
        fills.retain(|key, _| self.cache.contains_key(key) || self.filling.contains(key));
        self.read_ahead.update(&fills);
        fills
            .values()
            .filter(|fill| !fill.read && fill.at >= since)
            .map(|fill| fill.weight)
            .sum()
    }

    /// Make every read-ahead fill `by` older, for tests of its accounting.
    #[cfg(test)]
    pub(crate) fn age_read_ahead(&self, by: Duration) {
        let mut fills = self.read_ahead.fills.lock().unwrap();
        for fill in fills.values_mut() {
            fill.at = fill.at.checked_sub(by).unwrap_or(fill.at);
        }
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
            None => {
                let grid = reader::decode_message(&bytes, &entry.param);
                if self.read_ahead.tracking() {
                    self.decoded(&MessageKey::new(path.as_ref(), entry), &grid);
                }
                grid?
            }
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
    ///
    /// A map read-ahead's fill of the message in flight (#1335) is waited
    /// on for at most half the request's remaining time, which keeps the
    /// other half for a GET of its own, read around the cache: the
    /// read-ahead's GET runs under the background budget, and a stalled one
    /// must not fail the frame it was meant to speed up. Another request's
    /// fill, and any fill without a deadline in scope, is waited on as long
    /// as it takes, as before. A request's first read of a read-ahead fill
    /// peeks it ([`Self::first_read_ahead`]).
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
        let fill = || {
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
        };
        let mut joined = false;
        let bytes = loop {
            // A fetch in flight first: looked for after the first read, a
            // fetch finishing between the two would be found by a lookup,
            // which promotes it.
            if !joined {
                match self.join_read_ahead(&key) {
                    Join::None => {}
                    Join::Done => {
                        joined = true;
                        continue;
                    }
                    Join::TimedOut => {
                        // Read around the stalled read-ahead, admitting
                        // nothing: its fill will.
                        self.cache.record_bypass();
                        let bytes = fill()?;
                        deadline::check()?;
                        return Ok((bytes, validated));
                    }
                }
            }
            if let Some(bytes) = self.first_read_ahead(&key) {
                deadline::check()?;
                return Ok((bytes, None));
            }
            match self
                .cache
                .get_value_or_guard_untracked(&key, Duration::ZERO)
            {
                CacheEntry::Value(bytes) => {
                    self.cache.record_hit();
                    break bytes;
                }
                // The read-ahead marks a fetch before it looks for a claim,
                // and this claim is taken before the mark is read: one of
                // the two sees the other, and the message is fetched once.
                CacheEntry::Vacant(claim) if !joined && self.filling.contains(&key) => {
                    drop(claim);
                }
                CacheEntry::Vacant(claim) => break self.fill_claimed(&key, claim, fill)?,
                CacheEntry::Timeout => break self.cache.get_or_insert_with(&key, fill)?,
            }
        };
        if validated.is_none() {
            // A hit, or another reader's fill: the first read of a
            // pre-warmed or read-ahead message counts (#1335).
            self.note_read(&key);
        } else {
            // Filled by this request: what a background fill fetched of it
            // was evicted, and this copy is no background fill's.
            self.prewarmed.forget(&key);
            self.read_ahead.forget(&key);
            if window {
                self.cache.keep(key, bytes.clone());
            }
        }
        deadline::check()?;
        Ok((bytes, validated))
    }

    /// Fill `claim` with what `fill` reads, counting a miss first, as
    /// [`ByteBoundedCache::get_or_insert_with`] does: a failed fill still
    /// counts, and releases the claim for a later reader.
    fn fill_claimed(
        &self,
        key: &MessageKey,
        claim: ds_cache::CacheFillGuard<'_, MessageKey, Bytes>,
        fill: impl FnOnce() -> Result<Bytes, DataServerError>,
    ) -> Result<Bytes, DataServerError> {
        self.cache.record_miss_for(key);
        let bytes = fill()?;
        let _ = claim.insert(bytes.clone());
        Ok(bytes)
    }

    /// Wait for the read-ahead fetching `key` (#1335), if one is: for half
    /// the request's remaining time, as long as the fetch takes without a
    /// deadline in scope.
    fn join_read_ahead(&self, key: &MessageKey) -> Join {
        let Some(filled) = self.filling.signal(key) else {
            return Join::None;
        };
        let wait = deadline::current().map(|end| end.saturating_duration_since(Instant::now()) / 2);
        if filled.wait(wait) {
            Join::Done
        } else {
            Join::TimedOut
        }
    }

    /// Fill `key` ahead of a map's read of it (#1335): `fetch` the message
    /// and admit it. A request for the message meanwhile waits for the fetch
    /// rather than fetch it again, for at most half its remaining time
    /// ([`Self::get_with`]). The fill holds no single-flight claim and is
    /// admitted with a plain insert, unread: `quick_cache` admits a claimed
    /// fill that had waiters as read, and a request catching up with the
    /// read-ahead would promote it. Not kept, and its first read peeks it
    /// ([`Self::first_read_ahead`]), as a map's own fill is never looked up
    /// again: in a full cache it waits in the cold ring, and leaves first
    /// unless read twice.
    ///
    /// The one fill admitted without a decode: `fetch` checks the length,
    /// and a reader whose decode or index of it fails evicts it
    /// ([`Self::decoded`]), so a corrupt payload stays retryable as a failed
    /// decode of any fill does.
    ///
    /// Returns the bytes fetched; `None` without a fetch for a message
    /// resident or being filled by a request. Counts no hit and no miss: no
    /// request asked for it.
    pub(crate) fn read_ahead(
        &self,
        key: &MessageKey,
        fetch: impl FnOnce() -> Result<Bytes, DataServerError>,
    ) -> Result<Option<usize>, DataServerError> {
        if self.cache.contains_key(key) {
            return Ok(None);
        }
        // Raised when this returns, the fill admitted or not.
        let Some(_mark) = self.filling.mark(key) else {
            return Ok(None);
        };
        // Marked before a claim is looked for, where `get_with` takes its
        // claim before it reads the mark: of a request filling the message
        // meanwhile, one of the two sees the other.
        match self.cache.get_value_or_guard_untracked(key, Duration::ZERO) {
            CacheEntry::Vacant(claim) => drop(claim),
            CacheEntry::Value(_) | CacheEntry::Timeout => return Ok(None),
        }
        let bytes = fetch()?;
        let fetched = bytes.len();
        let weight = weight(key, &bytes);
        if weight > self.cache.capacity_bytes() {
            return Ok(Some(fetched));
        }
        // Recorded before the fill is admitted, so that a request waiting
        // for it takes its first read.
        {
            let mut fills = self.read_ahead.fills.lock().unwrap();
            if fills.len() >= MAX_UNREAD {
                fills.retain(|key, _| self.cache.contains_key(key));
            }
            if fills.len() < MAX_UNREAD {
                let (read, at) = (false, Instant::now());
                fills.insert(key.clone(), AheadFill { weight, read, at });
            }
            self.read_ahead.update(&fills);
        }
        self.cache
            .insert(key.clone(), Bytes::copy_from_slice(&bytes));
        if !self.cache.contains_key(key) {
            self.read_ahead.forget(key);
        }
        Ok(Some(fetched))
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
            self.prewarmed.insert(key, &self.cache);
        }
        Ok(Some(fetched))
    }

    /// Stop waiting on a first read of pre-warmed messages the cache has
    /// evicted unread. Once per pre-warm, before it fetches.
    pub(crate) fn forget_evicted_prewarms(&self) {
        self.prewarmed.forget_evicted(&self.cache);
    }

    /// Pre-warmed messages a request then read, each counted at its first
    /// read (#1335).
    pub(crate) fn prewarm_hits(&self) -> u64 {
        self.prewarmed.hits.load(Ordering::Relaxed)
    }

    /// Messages the map read-ahead fetched that a request then read, each
    /// counted at its first read, a request that joined the fill included
    /// (#1335).
    pub(crate) fn read_ahead_hits(&self) -> u64 {
        self.read_ahead.hits.load(Ordering::Relaxed)
    }

    pub fn metrics(&self) -> CacheMetrics {
        self.cache.metrics()
    }

    /// How many requests waited for the read-ahead fetching `key`, for
    /// tests that start a request and must know it joined.
    #[cfg(test)]
    pub(crate) fn read_ahead_waiters(&self, key: &MessageKey) -> usize {
        self.filling
            .signal(key)
            .map_or(0, |filled| filled.waiters.load(Ordering::SeqCst))
    }

    /// Evict every message, for tests of what follows an eviction.
    #[cfg(test)]
    pub(crate) fn clear(&self) {
        self.cache.retain(|_, _| false);
    }
}

#[cfg(test)]
mod tests;
