//! Read ahead of map animations (#1335).
//!
//! A WMS animation over a cold run waits on storage for every frame: about
//! 280 ms per GFS message on a pooled connection and 250 ms per IFS one
//! plus 503 retries, most of a cold frame once its sampling and grid cache
//! are fast (#1331, #1332). So after a map render (WMS, Maps or Tiles, a
//! meta-tile included: `MapEngine::get_raster_tile` and
//! `get_raster_tiles`) draws step k of a run, [`GribEngine::read_ahead_after`]
//! fetches the messages the same render would draw at the run's next
//! `[grib] read_ahead` steps carrying its parameters
//! ([`GribEngine::map_message`], [`CarriedSteps::after`]) into the
//! compressed-message cache, so a client
//! stepping forward finds them there. Forward in time and within the run
//! only: a frame past the run's last step, or a client stepping back or
//! across runs, reads what it draws itself.
//!
//! Once per frame, not per tile: the first tile of a (view, parameters,
//! level, run, step) plans, the others find it in [`ReadAhead::triggered`],
//! which a published catalog clears, since new steps may follow a frame
//! already planned. A message resident in the message cache, whose grid is
//! resident in the grid cache, or already queued or in flight is not
//! fetched.
//!
//! Bounds, per source:
//! - a parameter's next steps' messages must be resident together
//!   ([`MessageCache::holds`]), else nothing of it is read ahead: its fills
//!   would evict each other before a map read them;
//! - at most [`SHARE_PERCENT`] of the message cache per run, the earliest
//!   steps first, stopping at the first that does not fit; a share of its
//!   own beside the pre-warm's (below);
//! - [`CONCURRENCY`] GETs in flight, shared with the pre-warm
//!   ([`crate::prewarm::BackgroundGets`]): the bucket that answers 503
//!   SlowDown to a third counts both;
//! - at most [`MAX_QUEUED`] messages waiting.
//!
//! The fetches run on [`CONCURRENCY`] workers per source on the GRIB I/O
//! runtime ([`runtime::spawn_background`]), each GET through that runtime's
//! handle under [`FetchBudget::Background`] (root Critical Rules 6 and 7):
//! the render that plans them returns at once. A fill holds the message's
//! single-flight claim, so a request for it meanwhile joins the GET instead
//! of repeating it, for at most half its remaining time
//! ([`MessageCache::read_ahead`]). A fill is admitted once its length
//! checks, without a decode, and not kept: one a map reads is promoted, one
//! nobody reads leaves first. The render decodes it once, as any hit; a hit
//! that does not decode is evicted, so a corrupt fill stays retryable
//! ([`MessageCache::read`]). Decoding it here would only validate it: its
//! grid in the grid cache would save a 0.25° frame its decode, small beside
//! the GET, and not a 0.1° one, whose three grids ahead cannot share a
//! cold ring sized to hold one such field (#1332) and would evict each
//! other unread.
//!
//! Its share is its own, not the pre-warm's. The pre-warm fetches a run's
//! new steps as a poll catalogues them, the earliest first, up to its whole
//! share at once; sharing it, a pre-warm for position clients would leave
//! the run's maps no read-ahead, or a map loop would leave the next poll's
//! pre-warm nothing. Together they fill at most half the cache per run,
//! the pre-warm's kept, the read-ahead's only as long as maps read them.
//!
//! Map traffic is not recorded across restarts: a read-ahead follows the
//! frames being drawn now.

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};

use chrono::{DateTime, Utc};
use ds_core::error::DataServerError;
use ds_storage::object_store::path::Path;
use ds_storage::FetchBudget;

use crate::catalog::{CarriedSteps, Catalog, ForecastRun, MessageEntry, StepFile};
use crate::message_cache::{MessageCache, MessageKey};
use crate::prewarm::{message_range, CONCURRENCY, SHARE_PERCENT};
use crate::{runtime, GribEngine};

/// The most messages a source's read-ahead has waiting. A trigger plans a
/// few per parameter; a client scrubbing through many runs or parameters at
/// once fills it, and its later frames read what they draw.
const MAX_QUEUED: usize = 32;

/// The most frames a source remembers planning. Past it the set starts
/// over: a frame planned again fetches only what is not resident.
const MAX_TRIGGERED: usize = 4096;

/// A source's read-ahead: its depth, the frames it planned, its queue and
/// workers, and its counters.
pub(crate) struct ReadAhead {
    /// Steps read ahead of a frame: `[grib] read_ahead`.
    depth: usize,
    /// Frames planned since the catalog was last published, by
    /// [`GribEngine::frame_id`].
    triggered: Mutex<HashSet<u64>>,
    state: Mutex<State>,
    /// Signalled when the last worker stops.
    idle: Condvar,
    gets: AtomicU64,
    bytes: AtomicU64,
}

#[derive(Default)]
struct State {
    queue: VecDeque<Fetch>,
    /// The messages queued or being fetched.
    pending: HashSet<MessageKey>,
    workers: usize,
    /// Per run, the bytes fetched or queued against its share.
    spent: BTreeMap<DateTime<Utc>, Spent>,
}

#[derive(Default)]
struct Spent {
    bytes: u64,
    /// Whether the share has stopped a plan, logged once per run.
    capped: bool,
}

/// One message to read ahead.
struct Fetch {
    url: String,
    entry: MessageEntry,
    key: MessageKey,
    run: DateTime<Utc>,
    /// Its indexed length, held against the run's share until fetched.
    length: u64,
}

impl ReadAhead {
    /// A source's read-ahead: `None` when its config turns it off
    /// (`depth` 0) or it has no message cache to fill.
    pub(crate) fn new(depth: usize, cache: Option<&MessageCache>) -> Option<Self> {
        (depth > 0 && cache.is_some()).then(|| Self {
            depth,
            triggered: Mutex::default(),
            state: Mutex::default(),
            idle: Condvar::new(),
            gets: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        })
    }

    /// Whether frame `id` is planned for the first time since the catalog
    /// was published.
    fn first_trigger(&self, id: u64) -> bool {
        let mut triggered = self.triggered.lock().unwrap();
        if triggered.len() >= MAX_TRIGGERED {
            triggered.clear();
        }
        triggered.insert(id)
    }

    /// A catalog was published: frames already planned may have new steps
    /// after them.
    pub(crate) fn forget_triggers(&self) {
        self.triggered.lock().unwrap().clear();
    }

    /// Queue `fetches`, earliest steps first, within each run's share of
    /// `capacity` and [`MAX_QUEUED`], stopping at the first that does not
    /// fit, and skipping those already queued or in flight. Returns how
    /// many workers to start. `runs` are the source's retained runs: the
    /// shares of the others are forgotten.
    fn enqueue(
        &self,
        fetches: Vec<Fetch>,
        capacity: u64,
        runs: &BTreeMap<DateTime<Utc>, ForecastRun>,
        collection_id: &str,
    ) -> usize {
        let share = capacity / 100 * SHARE_PERCENT;
        let mut guard = self.state.lock().unwrap();
        let state = &mut *guard;
        state.spent.retain(|run, _| runs.contains_key(run));
        for fetch in fetches {
            if state.pending.contains(&fetch.key) {
                continue;
            }
            let spent = state.spent.entry(fetch.run).or_default();
            if spent.bytes.saturating_add(fetch.length) > share {
                if !spent.capped {
                    spent.capped = true;
                    tracing::debug!(
                        "Collection '{collection_id}': map read-ahead of run {} reached \
                         {SHARE_PERCENT} % of the message cache; maps read the rest",
                        fetch.run
                    );
                }
                break;
            }
            if state.queue.len() >= MAX_QUEUED {
                break;
            }
            spent.bytes += fetch.length;
            state.pending.insert(fetch.key.clone());
            state.queue.push_back(fetch);
        }
        let start = CONCURRENCY
            .saturating_sub(state.workers)
            .min(state.queue.len());
        state.workers += start;
        start
    }

    /// The next message to fetch; `None` once the queue is empty, which
    /// stops the calling worker.
    fn next(&self, worker: &mut Worker<'_>) -> Option<Fetch> {
        let mut state = self.state.lock().unwrap();
        let next = state.queue.pop_front();
        if next.is_none() {
            worker.stop(&mut state);
        }
        next
    }

    /// `fetch` is done, `fetched` bytes read: no longer pending, and what
    /// it did not read is the run's share again.
    fn finish(&self, fetch: &Fetch, fetched: u64) {
        let mut guard = self.state.lock().unwrap();
        let state = &mut *guard;
        state.pending.remove(&fetch.key);
        if let Some(spent) = state.spent.get_mut(&fetch.run) {
            spent.bytes = spent
                .bytes
                .saturating_sub(fetch.length.saturating_sub(fetched));
        }
    }

    /// Wait until every worker has stopped, at most `timeout`.
    #[cfg(test)]
    pub(crate) fn wait_idle(&self, timeout: std::time::Duration) {
        let state = self.state.lock().unwrap();
        let (state, result) = self
            .idle
            .wait_timeout_while(state, timeout, |state| state.workers > 0)
            .unwrap();
        assert!(
            !result.timed_out(),
            "read-ahead still running: {} queued",
            state.queue.len()
        );
    }
}

/// One running worker's slot in [`State::workers`], given back when the
/// queue is empty, or when the worker unwinds.
struct Worker<'a> {
    ahead: &'a ReadAhead,
    running: bool,
}

impl Worker<'_> {
    fn stop(&mut self, state: &mut State) {
        if std::mem::take(&mut self.running) {
            state.workers -= 1;
            if state.workers == 0 {
                self.ahead.idle.notify_all();
            }
        }
    }
}

impl Drop for Worker<'_> {
    fn drop(&mut self) {
        if self.running {
            let ahead = self.ahead;
            let mut state = ahead.state.lock().unwrap();
            self.stop(&mut state);
        }
    }
}

impl GribEngine {
    /// The read-ahead's GETs and bytes, and the messages it fetched that
    /// requests then read, each counted at its first read (#1335), per
    /// source; zeroes when it is off.
    pub fn read_ahead_counts(&self) -> (u64, u64, u64) {
        let Some(ahead) = &self.source.read_ahead else {
            return (0, 0, 0);
        };
        let hits = self
            .source
            .message_cache
            .as_ref()
            .map_or(0, MessageCache::read_ahead_hits);
        (
            ahead.gets.load(Ordering::Relaxed),
            ahead.bytes.load(Ordering::Relaxed),
            hits,
        )
    }

    /// Frame `step` of `run` for `parameters` (none: the default layer) at
    /// `z`, in this view: one id per frame, whatever its tiles.
    fn frame_id(&self, run: DateTime<Utc>, step: u32, parameters: &[&str], z: Option<f64>) -> u64 {
        let mut hasher = DefaultHasher::new();
        (self.family, run, step, parameters, z.map(f64::to_bits)).hash(&mut hasher);
        hasher.finish()
    }

    /// After a map render drew `step` of `run` for `parameters` (none: the
    /// default layer) at `z`: read its next steps ahead (see the module
    /// docs). On the request's thread a hash and a lock per tile, and once
    /// per frame the plan: no I/O, nothing it waits for.
    pub(crate) fn read_ahead_after(
        &self,
        catalog: &Catalog,
        run: &ForecastRun,
        step: u32,
        parameters: &[&str],
        z: Option<f64>,
    ) {
        let (Some(ahead), Some(cache)) = (&self.source.read_ahead, &self.source.message_cache)
        else {
            return;
        };
        if self.source.shutdown.is_shutdown()
            || !ahead.first_trigger(self.frame_id(run.reference_time, step, parameters, z))
        {
            return;
        }
        let carried = if parameters.is_empty() {
            CarriedSteps::Every(run)
        } else {
            catalog.carried_steps(run, parameters)
        };
        let steps: Vec<&StepFile> = carried
            .after(step)
            .take(ahead.depth)
            .map(|(_, file)| file)
            .collect();
        if steps.is_empty() {
            return;
        }
        let names: Vec<Option<&str>> = if parameters.is_empty() {
            vec![None]
        } else {
            parameters.iter().copied().map(Some).collect()
        };
        // What the render draws at a step: the same selection.
        let drawn = &|name: Option<&str>, file: &StepFile| {
            let drawn = self.map_message(catalog, run, file, name, z).ok()?;
            Some((
                drawn.url.to_owned(),
                drawn.entry.clone(),
                drawn.entry.length?,
            ))
        };
        // A parameter whose next steps the cache cannot hold together is
        // not read ahead: its fills would evict each other unread.
        let held: Vec<Option<&str>> = names
            .into_iter()
            .filter(|&name| {
                let (messages, bytes) = steps.iter().filter_map(|&file| drawn(name, file)).fold(
                    (0, 0u64),
                    |(messages, bytes), (_, _, length)| {
                        (messages + 1, bytes.saturating_add(length))
                    },
                );
                messages > 0 && cache.holds(messages, bytes)
            })
            .collect();
        let grids = self.source.grid_cache.as_ref();
        let fetches: Vec<Fetch> = steps
            .iter()
            .flat_map(|&file| held.iter().filter_map(move |&name| drawn(name, file)))
            .filter_map(|(url, entry, length)| {
                let key = MessageKey::new(Path::from(url.as_str()).as_ref(), &entry);
                let resident = cache.is_resident(&key)
                    || grids.is_some_and(|grids| grids.contains(&url, entry.offset));
                (!resident).then_some(Fetch {
                    url,
                    entry,
                    key,
                    run: run.reference_time,
                    length,
                })
            })
            .collect();
        if fetches.is_empty() {
            return;
        }
        let source_catalog = self.source.catalog.load();
        let start = ahead.enqueue(
            fetches,
            cache.capacity_bytes(),
            &source_catalog.runs,
            &self.collection_id,
        );
        for _ in 0..start {
            let engine = Self {
                collection_id: self.collection_id.clone(),
                family: None,
                source: self.source.clone(),
            };
            runtime::spawn_background(move || engine.drain_read_ahead());
        }
    }

    /// A worker: fetch queued messages until none is left. Runs inside
    /// `block_in_place` on a GRIB I/O runtime worker.
    fn drain_read_ahead(&self) {
        let Some(ahead) = &self.source.read_ahead else {
            return;
        };
        let mut worker = Worker {
            ahead,
            running: true,
        };
        while let Some(fetch) = ahead.next(&mut worker) {
            let fetched = if self.source.shutdown.is_shutdown() {
                Ok(None)
            } else {
                self.read_ahead_message(&fetch)
            };
            let bytes = match fetched {
                Ok(bytes) => bytes.unwrap_or(0),
                Err(error) => {
                    tracing::debug!(
                        "Collection '{}': map read-ahead of {} at {} failed; a map reads \
                         it itself: {error}",
                        self.collection_id,
                        fetch.url,
                        fetch.entry.offset
                    );
                    0
                }
            };
            ahead.finish(&fetch, bytes as u64);
        }
    }

    /// One message, fetched whole into the message cache once one of the
    /// source's background GETs is free; a short read is not admitted.
    /// `None` without a fetch: resident, or another reader fills it.
    fn read_ahead_message(&self, fetch: &Fetch) -> Result<Option<usize>, DataServerError> {
        let (Some(ahead), Some(cache)) = (&self.source.read_ahead, &self.source.message_cache)
        else {
            return Ok(None);
        };
        if cache.is_resident(&fetch.key) {
            return Ok(None);
        }
        let Some(range) = message_range(&fetch.entry) else {
            return Ok(None);
        };
        // The GET is taken before the fill is claimed: a request never
        // waits on a fill that waits for a GET the pre-warm holds.
        let wait = FetchBudget::Background.timeout();
        let permit = self.source.background_gets.acquire(wait).ok_or_else(|| {
            DataServerError::Storage(format!(
                "no background GET free within {} s",
                wait.as_secs()
            ))
        })?;
        let path = Path::from(fetch.url.as_str());
        let get = move || {
            let bytes = self.background_fetch(&path, range, (&ahead.gets, &ahead.bytes));
            drop(permit);
            bytes
        };
        cache.read_ahead(&fetch.key, get)
    }

    /// Wait until the read-ahead's workers have stopped, for tests.
    #[cfg(test)]
    pub(crate) fn wait_for_read_ahead(&self) {
        if let Some(ahead) = &self.source.read_ahead {
            ahead.wait_idle(std::time::Duration::from_secs(30));
        }
    }
}

#[cfg(test)]
mod tests;
