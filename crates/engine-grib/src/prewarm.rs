//! Pre-warm the messages position queries read as a new run's steps are
//! catalogued (#1335).
//!
//! The first EDR position query of a new run reads every field from object
//! storage: about 3 s per parameter over a 0.25° GFS run, 2 s over a 0.1°
//! IFS one, where the same query answers in milliseconds once its messages
//! are cached. Position queries therefore note, per source, the parameter
//! and level of every message they read ([`RecentReads`]). When a poll
//! catalogues new steps of the newest run, [`GribEngine::prewarm_new_steps`]
//! fetches those steps' messages of the keys read within [`READ_WINDOW`]
//! into the compressed-message cache and builds their complete point
//! index, for any point, so the first query afterwards reads nothing from
//! storage and builds no index.
//!
//! Bounds, per source: only keys a position query read within the window,
//! so the cost follows real use and a source nobody reads fetches nothing;
//! at most [`SHARE_PERCENT`] of the message cache per run, the earliest
//! steps first, and nothing of a key whose messages of the run the cache
//! cannot hold ([`MessageCache::holds`]), where a query over it fetches only
//! parts of them; [`CONCURRENCY`] GETs in flight, shared with the map
//! read-ahead ([`BackgroundGets`]), since a bucket answers 503 SlowDown to
//! three; [`BUDGET`] for one poll's pre-warm. The fetches run
//! as field jobs on the runtime the scan runs on, the poll runtime, each
//! GET through an explicit handle under [`FetchBudget::Background`] (root
//! Critical Rules 6, 7 and 9), outside the cache's single-flight so no
//! request waits on one. A message the cache holds is not fetched.
//!
//! The fills are kept ([`MessageCache::prewarm`]): in a full cache each
//! would otherwise wait in the cold ring until the next fill evicted it.
//!
//! What was read lives in memory only: after a restart or a rebuild of the
//! engine nothing pre-warms until clients read again. Area, radius, cube and
//! trajectory queries read decoded grids, not the point path, and do not
//! count, nor do map tiles; WMS GetFeatureInfo, a position query, does.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use ds_core::error::DataServerError;
use ds_storage::bytes::Bytes;
use ds_storage::object_store::path::Path;
use ds_storage::FetchBudget;

use crate::catalog::{Catalog, ForecastRun, MessageEntry, ParameterKey};
use crate::message_cache::{MessageCache, MessageKey};
use crate::point_index::PointField;
use crate::{runtime, FieldFetch, FieldPlan, GribEngine, GridFill};

/// How long after a position query read a parameter at a level its
/// messages are still pre-warmed.
const READ_WINDOW: Duration = Duration::from_secs(24 * 3600);

/// The most parameter levels a source remembers reads of. A query names a
/// few; a key past this replaces the one read longest ago.
const MAX_KEYS: usize = 256;

/// The share of the message cache, in percent, one run's pre-warm may fill:
/// the rest is the maps' and the queries' own. The map read-ahead has a
/// share of its own (`readahead.rs`).
pub(crate) const SHARE_PERCENT: u64 = 25;

/// GETs in flight per source, the pre-warm's and the map read-ahead's
/// together ([`BackgroundGets`]): a public bucket answers 503 SlowDown to a
/// third.
pub(crate) const CONCURRENCY: usize = 2;

/// The longest one poll's pre-warm runs: the poll loop waits for it, and a
/// stalled store must not hold back the next step's discovery. What it has
/// not fetched by then is left to the queries.
const BUDGET: Duration = Duration::from_secs(120);

/// Seconds on a monotonic clock, so a wall-clock step cannot make a key look
/// read or unread.
fn clock_secs() -> i64 {
    static START: LazyLock<Instant> = LazyLock::new(Instant::now);
    i64::try_from(START.elapsed().as_secs()).unwrap_or(i64::MAX)
}

/// Whether a read at `last` is within [`READ_WINDOW`] of `now`.
fn within_window(last: i64, now: i64) -> bool {
    now.saturating_sub(last) < READ_WINDOW.as_secs() as i64
}

/// The GETs a source's background fills have in flight, the pre-warm's and
/// the map read-ahead's together (#1335): at most [`CONCURRENCY`]. Each
/// bounds its own jobs too, but the bucket counts both: two of each would
/// be the four in flight that it answers 503 SlowDown to.
#[derive(Default)]
pub(crate) struct BackgroundGets {
    in_flight: Mutex<usize>,
    freed: Condvar,
}

impl BackgroundGets {
    /// One of the [`CONCURRENCY`] GETs, waiting at most `wait` for one to
    /// free; `None` past it.
    pub(crate) fn acquire(&self, wait: Duration) -> Option<GetPermit<'_>> {
        let in_flight = self.in_flight.lock().unwrap();
        let (mut in_flight, _) = self
            .freed
            .wait_timeout_while(in_flight, wait, |n| *n >= CONCURRENCY)
            .unwrap();
        if *in_flight >= CONCURRENCY {
            return None;
        }
        *in_flight += 1;
        Some(GetPermit(self))
    }
}

/// One GET of [`BackgroundGets`], freed when dropped.
pub(crate) struct GetPermit<'a>(&'a BackgroundGets);

impl Drop for GetPermit<'_> {
    fn drop(&mut self) {
        *self.0.in_flight.lock().unwrap() -= 1;
        self.0.freed.notify_one();
    }
}

/// The byte range of `entry` in its file; `None` for a message of unknown
/// length (a wgrib2 tail record) or one past the address space.
pub(crate) fn message_range(entry: &MessageEntry) -> Option<Range<usize>> {
    let length = entry.length?;
    let start = usize::try_from(entry.offset).ok()?;
    Some(start..start.checked_add(usize::try_from(length).ok()?)?)
}

/// When position queries last read each parameter level of a source.
#[derive(Default)]
pub(crate) struct RecentReads(Mutex<HashMap<ParameterKey, i64>>);

impl RecentReads {
    /// A position query reads the messages of `keys`, its parameter levels:
    /// note them read now. One lock per query, no I/O.
    pub(crate) fn note<'a>(&self, keys: impl IntoIterator<Item = &'a ParameterKey>) {
        self.note_at(keys, clock_secs());
    }

    /// [`Self::note`] `ago`, for tests of the window.
    #[cfg(test)]
    fn note_ago<'a>(&self, keys: impl IntoIterator<Item = &'a ParameterKey>, ago: Duration) {
        self.note_at(keys, clock_secs() - ago.as_secs() as i64);
    }

    fn note_at<'a>(&self, keys: impl IntoIterator<Item = &'a ParameterKey>, at: i64) {
        let mut reads = self.0.lock().unwrap();
        for key in keys {
            if let Some(last) = reads.get_mut(key) {
                *last = at;
                continue;
            }
            if reads.len() >= MAX_KEYS {
                let now = clock_secs();
                reads.retain(|_, &mut last| within_window(last, now));
            }
            if reads.len() >= MAX_KEYS {
                let oldest = reads
                    .iter()
                    .min_by_key(|(_, &last)| last)
                    .map(|(key, _)| key.clone());
                if let Some(oldest) = oldest {
                    reads.remove(&oldest);
                }
            }
            reads.insert(key.clone(), at);
        }
    }

    /// The keys read within [`READ_WINDOW`], in a stable order; older ones
    /// are forgotten.
    fn recent(&self) -> Vec<ParameterKey> {
        let now = clock_secs();
        let mut reads = self.0.lock().unwrap();
        reads.retain(|_, &mut last| within_window(last, now));
        let mut keys: Vec<ParameterKey> = reads.keys().cloned().collect();
        keys.sort_by(|a, b| (&a.param, &a.levtype, a.level).cmp(&(&b.param, &b.levtype, b.level)));
        keys
    }
}

/// A source's pre-warm: what position queries read, what the newest run's
/// pre-warm has fetched, and its counters.
pub(crate) struct Prewarm {
    pub(crate) reads: RecentReads,
    /// The run the pre-warm last fetched for and its bytes, against
    /// [`SHARE_PERCENT`].
    spent: Mutex<(Option<DateTime<Utc>>, u64)>,
    gets: AtomicU64,
    bytes: AtomicU64,
}

impl Prewarm {
    /// A source's pre-warm: `None` when its config turns it off or it has
    /// no message cache to fill.
    pub(crate) fn new(enabled: bool, cache: Option<&MessageCache>) -> Option<Self> {
        (enabled && cache.is_some()).then(|| Self {
            reads: RecentReads::default(),
            spent: Mutex::new((None, 0)),
            gets: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        })
    }

    /// Bytes `run`'s pre-warm may still fetch into `cache`.
    fn remaining(&self, run: DateTime<Utc>, cache: &MessageCache) -> u64 {
        let share = cache.capacity_bytes() / 100 * SHARE_PERCENT;
        let spent = self.spent.lock().unwrap();
        let spent = if spent.0 == Some(run) { spent.1 } else { 0 };
        share.saturating_sub(spent)
    }

    fn spend(&self, run: DateTime<Utc>, bytes: u64) {
        let mut spent = self.spent.lock().unwrap();
        if spent.0 != Some(run) {
            *spent = (Some(run), 0);
        }
        spent.1 = spent.1.saturating_add(bytes);
    }
}

/// One message to pre-warm.
struct Fetch {
    url: String,
    entry: MessageEntry,
}

/// What a poll pre-warms of the newest run: the new messages to fetch,
/// earliest steps first, within the run's remaining share; how many the
/// share left out; and the keys whose messages of the run the cache cannot
/// hold, which a query over them reads only parts of.
struct Plan<'a> {
    fetches: Vec<Fetch>,
    capped: usize,
    not_held: Vec<&'a ParameterKey>,
}

/// `keys`' messages in `run`'s steps that `previous` did not catalogue:
/// at each step, each key's first message, as a query selects it. `None`
/// when there are none. A key whose messages over every step of the run
/// `cache` cannot hold is left out. Messages of unknown length (a wgrib2
/// tail record) and those `cache` holds are skipped; of the rest, earliest
/// steps first, as many as fit in `remaining` bytes, stopping at the first
/// that does not.
fn plan<'a>(
    previous: &Catalog,
    run: &ForecastRun,
    keys: &'a [ParameterKey],
    cache: &MessageCache,
    remaining: u64,
) -> Option<Plan<'a>> {
    let before = previous.runs.get(&run.reference_time);
    // Each key's window over the run, which the cache must hold, and the
    // messages `previous` lacked.
    let mut windows = vec![(0usize, 0u64); keys.len()];
    let mut fresh = Vec::new();
    for (step, file) in &run.steps {
        let known = before.and_then(|run| run.steps.get(step));
        for (k, entry) in keys.iter().enumerate().filter_map(|(k, key)| {
            let entry = file.messages.iter().find(|m| key.matches(m))?;
            Some((k, entry))
        }) {
            if let Some(length) = entry.length {
                windows[k].0 += 1;
                windows[k].1 = windows[k].1.saturating_add(length);
            }
            let url = file.message_url(entry);
            let catalogued = known.is_some_and(|known| {
                known.messages.iter().any(|m| {
                    m.offset == entry.offset
                        && m.length == entry.length
                        && known.message_url(m) == url
                })
            });
            if !catalogued {
                fresh.push((k, url, entry));
            }
        }
    }
    if fresh.is_empty() {
        return None;
    }
    let held: Vec<bool> = windows
        .iter()
        .map(|&(messages, bytes)| cache.holds(messages, bytes))
        .collect();
    let mut fetches = Vec::new();
    let mut planned = 0u64;
    let mut capped = 0;
    for (k, url, entry) in fresh {
        let Some(length) = entry.length.filter(|_| held[k]) else {
            continue;
        };
        if cache.is_resident(&MessageKey::new(Path::from(url).as_ref(), entry)) {
            continue;
        }
        // Stop at the first that does not fit: later steps never take the
        // place of an earlier one.
        if capped > 0 || planned.saturating_add(length) > remaining {
            capped += 1;
            continue;
        }
        planned += length;
        fetches.push(Fetch {
            url: url.to_owned(),
            entry: entry.clone(),
        });
    }
    let not_held = keys
        .iter()
        .zip(held)
        .filter(|(_, held)| !held)
        .map(|(key, _)| key)
        .collect();
    Some(Plan {
        fetches,
        capped,
        not_held,
    })
}

/// What one fetch did.
enum Warmed {
    Fetched(usize),
    /// Resident by then, or the engine is shutting down.
    Skipped,
    Failed(String),
}

/// What one poll's pre-warm did, for its log line.
#[derive(Default)]
struct Outcome {
    messages: usize,
    bytes: u64,
    failed: usize,
    /// Not fetched by [`BUDGET`].
    unfinished: usize,
    first_error: Option<String>,
}

impl GribEngine {
    /// [`Self::background_fetch`] once one of the source's
    /// [`BackgroundGets`] is free, waiting at most `wait` for it; `None`
    /// past it.
    fn background_get(
        &self,
        path: &Path,
        range: Range<usize>,
        wait: Duration,
        counters: (&AtomicU64, &AtomicU64),
    ) -> Result<Option<Bytes>, DataServerError> {
        let Some(_permit) = self.source.background_gets.acquire(wait) else {
            return Ok(None);
        };
        self.background_fetch(path, range, counters).map(Some)
    }

    /// One GET of a background fill, the pre-warm's or the map
    /// read-ahead's (#1335), under a [`GetPermit`] the caller holds:
    /// `range` of `path` under [`FetchBudget::Background`]. The caller runs
    /// inside `block_in_place` on a runtime worker, whose handle drives the
    /// GET (root Critical Rule 7). `counters` count the GET and the bytes
    /// it read; a short read is an error.
    pub(crate) fn background_fetch(
        &self,
        path: &Path,
        range: Range<usize>,
        (gets, bytes_read): (&AtomicU64, &AtomicU64),
    ) -> Result<Bytes, DataServerError> {
        gets.fetch_add(1, Ordering::Relaxed);
        let handle = tokio::runtime::Handle::current();
        let bytes = self.source.store.get_range_on_with_budget(
            path,
            range.clone(),
            &handle,
            FetchBudget::Background,
        )?;
        bytes_read.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        if bytes.len() != range.len() {
            return Err(DataServerError::Storage(format!(
                "short GRIB message read: {} of {} bytes",
                bytes.len(),
                range.len()
            )));
        }
        Ok(bytes)
    }

    /// The pre-warm's GETs and bytes, and the pre-warmed messages requests
    /// then read, each counted at its first read (#1335), per source;
    /// zeroes when it is off.
    pub fn prewarm_counts(&self) -> (u64, u64, u64) {
        let Some(prewarm) = &self.source.prewarm else {
            return (0, 0, 0);
        };
        let hits = self
            .source
            .message_cache
            .as_ref()
            .map_or(0, MessageCache::prewarm_hits);
        (
            prewarm.gets.load(Ordering::Relaxed),
            prewarm.bytes.load(Ordering::Relaxed),
            hits,
        )
    }

    /// After a poll published its catalog: fetch the messages of the
    /// newest run's steps that `previous`, the catalog before it, lacked,
    /// for the keys position queries read within [`READ_WINDOW`] (see the
    /// module docs). Runs on the poll runtime; returns once every fetch
    /// has finished or [`BUDGET`] has passed.
    pub(crate) fn prewarm_new_steps(&self, previous: &Catalog) {
        let (Some(prewarm), Some(cache)) = (&self.source.prewarm, &self.source.message_cache)
        else {
            return;
        };
        let keys = prewarm.reads.recent();
        if keys.is_empty() || self.source.shutdown.is_shutdown() {
            return;
        }
        let catalog = self.source.catalog.load_full();
        let Some(run) = catalog.latest_run() else {
            return;
        };
        let reference_time = run.reference_time;
        let remaining = prewarm.remaining(reference_time, cache);
        let Some(Plan {
            fetches,
            capped,
            not_held,
        }) = plan(previous, run, &keys, cache, remaining)
        else {
            return;
        };
        if !not_held.is_empty() {
            let names: Vec<String> = not_held
                .iter()
                .map(|key| format!("{}:{}:{:?}", key.param, key.levtype, key.level))
                .collect();
            tracing::debug!(
                "Collection '{}': no pre-warm of run {reference_time} for {}: the message \
                 cache cannot hold their messages of the run",
                self.collection_id,
                names.join(", ")
            );
        }
        if capped > 0 {
            tracing::debug!(
                "Collection '{}': pre-warm of run {reference_time} leaves {capped} new \
                 messages to queries: {SHARE_PERCENT} % of the message cache reached",
                self.collection_id
            );
        }
        if fetches.is_empty() {
            return;
        }
        cache.forget_evicted_prewarms();
        let outcome = self.prewarm(reference_time, fetches);
        prewarm.spend(reference_time, outcome.bytes);
        if outcome.messages > 0 {
            tracing::info!(
                "Collection '{}': pre-warmed {} messages ({:.1} MiB) of run {reference_time} \
                 for position queries of {} parameter levels",
                self.collection_id,
                outcome.messages,
                outcome.bytes as f64 / ds_cache::MIB as f64,
                keys.len()
            );
        }
        if outcome.failed > 0 || outcome.unfinished > 0 {
            tracing::warn!(
                "Collection '{}': pre-warm of run {reference_time}: {} messages failed, {} not \
                 fetched within {} s; queries read them (first error: {})",
                self.collection_id,
                outcome.failed,
                outcome.unfinished,
                BUDGET.as_secs(),
                outcome.first_error.as_deref().unwrap_or("none")
            );
        }
    }

    /// Fetch `fetches` into the message cache, [`CONCURRENCY`] at a time,
    /// as field jobs on the calling thread's runtime (the shared GRIB I/O
    /// runtime off one), for at most [`BUDGET`].
    fn prewarm(&self, run: DateTime<Utc>, fetches: Vec<Fetch>) -> Outcome {
        let planned = fetches.len();
        let engine = Arc::new(Self {
            collection_id: self.collection_id.clone(),
            family: None,
            source: self.source.clone(),
        });
        let mut outcome = Outcome::default();
        let mut done = 0;
        let _deadline = ds_core::deadline::enter(Some(Instant::now() + BUDGET));
        let result = runtime::run_field_jobs(
            CONCURRENCY,
            fetches.into_iter(),
            move |fetch| engine.prewarm_message(run, &fetch),
            |warmed| {
                done += 1;
                match warmed {
                    Warmed::Fetched(bytes) => {
                        outcome.messages += 1;
                        outcome.bytes += bytes as u64;
                    }
                    Warmed::Skipped => {}
                    Warmed::Failed(error) => {
                        outcome.failed += 1;
                        outcome.first_error.get_or_insert(error);
                    }
                }
                Ok(())
            },
        );
        outcome.unfinished = planned - done;
        if let Err(error) = result {
            outcome.first_error.get_or_insert(error.to_string());
        }
        outcome
    }

    /// One message, fetched whole into the message cache and indexed for
    /// position queries: [`Warmed::Failed`] for a failed GET or a message
    /// that does not decode, which a query then reads itself. Only the
    /// budget's deadline stops the pre-warm; at shutdown the rest is
    /// skipped.
    fn prewarm_message(
        &self,
        run: DateTime<Utc>,
        fetch: &Fetch,
    ) -> Result<Warmed, DataServerError> {
        let (Some(prewarm), Some(cache)) = (&self.source.prewarm, &self.source.message_cache)
        else {
            return Ok(Warmed::Skipped);
        };
        if self.source.shutdown.is_shutdown() {
            return Ok(Warmed::Skipped);
        }
        let entry = &fetch.entry;
        let path = Path::from(fetch.url.as_str());
        let key = MessageKey::new(path.as_ref(), entry);
        let Some(range) = message_range(entry) else {
            return Ok(Warmed::Skipped);
        };
        let get = || {
            // A GET the read-ahead holds is waited for within the budget.
            let wait = ds_core::deadline::current()
                .map_or(BUDGET, |end| end.saturating_duration_since(Instant::now()));
            let counters = (&prewarm.gets, &prewarm.bytes);
            self.background_get(&path, range, wait, counters)?
                .ok_or(DataServerError::DeadlineExceeded)
        };
        // Validated before admission as a position query's first read is:
        // the complete point index when the message takes one, whatever
        // the coordinates (#1337), else a full decode.
        let field = PointField {
            url: &fetch.url,
            entry,
            run,
            coords: &[],
            plan: FieldPlan {
                concurrency: CONCURRENCY,
                fill: GridFill::Bypass,
                fetch: FieldFetch::Whole,
            },
        };
        match cache.prewarm(&key, get, |bytes| self.index_message(&field, bytes)) {
            Ok(Some(bytes)) => Ok(Warmed::Fetched(bytes)),
            Ok(None) => Ok(Warmed::Skipped),
            Err(DataServerError::DeadlineExceeded) => Err(DataServerError::DeadlineExceeded),
            Err(error) => Ok(Warmed::Failed(error.to_string())),
        }
    }
}

#[cfg(test)]
mod tests;
