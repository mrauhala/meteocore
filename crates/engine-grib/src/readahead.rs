//! Read ahead of map animations (#1335).
//!
//! A WMS animation over a cold run waits on storage for every frame: about
//! 280 ms per GFS message on a pooled connection and 250 ms per IFS one
//! plus 503 retries, most of a cold frame once its sampling and grid cache
//! are fast (#1331, #1332). So when a map render (WMS, Maps or Tiles, a
//! meta-tile included: `MapEngine::get_raster_tile` and `get_raster_tiles`)
//! draws step k of a run in the middle of an animation,
//! [`GribEngine::read_ahead_after`] fetches the messages the same render
//! would draw at the run's next `[grib] read_ahead` steps carrying its
//! parameters ([`GribEngine::map_message`], [`CarriedSteps::after`]) into
//! the compressed-message cache, so the client stepping forward finds them
//! there. Forward in time and within the run only: a frame past the run's
//! last step, or a client stepping back or across runs, reads what it draws
//! itself.
//!
//! Only animations: a view (parameters, level, run, bbox, size and CRS: a
//! tile or an image) reads ahead from step k once it drew the step before k
//! within [`ANIMATION_WINDOW`], so a view that draws one step, however
//! often, costs no speculative GET. Once per frame, not per tile: the first
//! tile of a (view, parameters, level, run, step) to pass plans, for
//! [`PLANNED_FOR`], so a loop's later passes plan again; a published
//! catalog clears the plans, since new steps may follow a frame already
//! planned. A message resident in the message cache, whose grid is
//! resident in the grid cache, or already queued or in flight is not
//! fetched.
//!
//! Bounds, per source:
//! - room ([`ReadAhead::room`]): the weight queued, in flight and fetched
//!   but not yet read stays within half the message cache's cold ring
//!   ([`MessageCache::cold_room`]) and [`SHARE_PERCENT`] of the cache. A
//!   fill is not kept, so in a full cache it waits in the cold ring, where
//!   the maps' own fills arrive too: more than the ring holds would evict
//!   the next frame before it is drawn. Nearest steps first, stopping at
//!   the first that does not fit. A request's first read of a fill gives
//!   its weight back, as does its eviction unread or a failed fetch, so the
//!   room bounds what waits, not what a run may fetch; a fill nobody reads
//!   stops counting after [`UNREAD_FOR`], so an abandoned animation in a
//!   cache that never evicts its fills does not switch the read-ahead off;
//! - [`CONCURRENCY`] GETs in flight, shared with the pre-warm
//!   ([`crate::prewarm::BackgroundGets`]): the bucket that answers 503
//!   SlowDown to a third counts both;
//! - at most [`MAX_QUEUED`] messages waiting.
//!
//! The fetches run on [`CONCURRENCY`] workers per source on the GRIB I/O
//! runtime ([`runtime::spawn_background`]), each GET through that runtime's
//! handle under [`FetchBudget::Background`] (root Critical Rules 6 and 7):
//! the render that plans them returns at once. A request for a message
//! being fetched waits for the fetch instead of repeating it, for at most
//! half its remaining time, on a signal of the read-ahead's own rather
//! than a single-flight claim: `quick_cache` admits a claimed fill that had
//! waiters as read, and a client catching up with its read-ahead would
//! promote every frame ([`MessageCache::read_ahead`]).
//!
//! A fill is admitted once its length checks, without a decode, and not
//! kept; a request's first read of it peeks it, so it stays in the cold
//! ring unreferenced, as a map's own fill, never looked up again, does,
//! and an animation does not evict what the hot ring keeps. The render
//! decodes it once, as any hit; a fill a reader cannot decode or index is
//! evicted, so it stays retryable ([`MessageCache::decoded`]). Decoding it
//! here would only validate it: its grid in the grid cache would save a
//! 0.25° frame its decode, small beside the GET, and not a 0.1° one, whose
//! three grids ahead cannot share a cold ring sized to hold one such field
//! (#1332) and would evict each other unread.
//!
//! The room is the read-ahead's own, not the pre-warm's share: the
//! pre-warm fetches a run's new steps as a poll catalogues them, up to its
//! whole share at once, and sharing it would leave the run's maps no
//! read-ahead, or a map loop the next poll's pre-warm nothing.
//!
//! Map traffic is not recorded across restarts: a read-ahead follows the
//! frames being drawn now.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use ds_core::error::DataServerError;
use ds_storage::object_store::path::Path;
use ds_storage::FetchBudget;

use crate::catalog::{CarriedSteps, Catalog, ForecastRun, MessageEntry, StepFile};
use crate::message_cache::{message_weight, MessageCache, MessageKey};
use crate::prewarm::{message_range, CONCURRENCY, SHARE_PERCENT};
use crate::{runtime, GribEngine, MapField};

/// How recently a view must have drawn the step before a frame for that
/// frame to read ahead: an animation draws its next frame within seconds,
/// a client returning to a view a minute later is not animating it.
const ANIMATION_WINDOW: Duration = Duration::from_secs(60);

/// How long a frame stays planned: its other tiles plan nothing, and a
/// loop's later pass plans it again, fetching what the cache no longer
/// holds of it.
const PLANNED_FOR: Duration = Duration::from_secs(300);

/// How long a fill nobody reads counts against the room: an abandoned
/// animation's fills, which a cache under no pressure never evicts, would
/// otherwise switch the read-ahead off for the whole source. An animation
/// reads its fills within seconds.
pub(crate) const UNREAD_FOR: Duration = Duration::from_secs(120);

/// The most messages a source's read-ahead has waiting. A trigger plans a
/// few per parameter; a client scrubbing through many runs or parameters at
/// once fills it, and its later frames read what they draw.
const MAX_QUEUED: usize = 32;

/// The most views drawn and frames planned a source remembers. Past it the
/// stale ones are forgotten, and if none is, all of them.
const MAX_REMEMBERED: usize = 4096;

/// A source's read-ahead: its depth, the views drawn and frames planned,
/// its queue and workers, and its counters.
pub(crate) struct ReadAhead {
    /// Steps read ahead of a frame: `[grib] read_ahead`.
    depth: usize,
    /// When each view last drew each step, by [`GribEngine::view_id`] and
    /// step: the animation gate.
    drawn: Mutex<HashMap<(u64, u32), Instant>>,
    /// When each frame planned, by [`GribEngine::frame_id`].
    planned: Mutex<HashMap<u64, Instant>>,
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
    /// Their weight, against the room.
    queued: u64,
    workers: usize,
}

/// One message to read ahead.
struct Fetch {
    url: String,
    entry: MessageEntry,
    key: MessageKey,
    /// Its weight in the message cache ([`message_weight`]).
    weight: u64,
}

/// Fills admitted before this, unread, no longer count against the room.
fn unread_since() -> Instant {
    let now = Instant::now();
    now.checked_sub(UNREAD_FOR).unwrap_or(now)
}

/// Remember `key` at `now` in `seen`, first forgetting what is older than
/// `fresh` when it is full, and everything if that frees nothing.
fn remember<K: Eq + Hash>(seen: &mut HashMap<K, Instant>, key: K, now: Instant, fresh: Duration) {
    if seen.len() >= MAX_REMEMBERED {
        seen.retain(|_, at| now.saturating_duration_since(*at) < fresh);
        if seen.len() >= MAX_REMEMBERED {
            seen.clear();
        }
    }
    seen.insert(key, now);
}

impl ReadAhead {
    /// A source's read-ahead: `None` when its config turns it off
    /// (`depth` 0) or it has no message cache to fill.
    pub(crate) fn new(depth: usize, cache: Option<&MessageCache>) -> Option<Self> {
        (depth > 0 && cache.is_some()).then(|| Self {
            depth,
            drawn: Mutex::default(),
            planned: Mutex::default(),
            state: Mutex::default(),
            idle: Condvar::new(),
            gets: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        })
    }

    /// `view` draws `step`: whether it drew `previous`, the step before it,
    /// within [`ANIMATION_WINDOW`].
    fn animating(&self, view: u64, step: u32, previous: Option<u32>) -> bool {
        let now = Instant::now();
        let mut drawn = self.drawn.lock().unwrap();
        remember(&mut drawn, (view, step), now, ANIMATION_WINDOW);
        previous
            .and_then(|previous| drawn.get(&(view, previous)))
            .is_some_and(|at| now.saturating_duration_since(*at) < ANIMATION_WINDOW)
    }

    /// Whether frame `id` plans now: not planned within [`PLANNED_FOR`].
    fn plan_once(&self, id: u64) -> bool {
        let now = Instant::now();
        let mut planned = self.planned.lock().unwrap();
        if planned
            .get(&id)
            .is_some_and(|at| now.saturating_duration_since(*at) < PLANNED_FOR)
        {
            return false;
        }
        remember(&mut planned, id, now, PLANNED_FOR);
        true
    }

    /// A catalog was published: frames already planned may have new steps
    /// after them.
    pub(crate) fn forget_plans(&self) {
        self.planned.lock().unwrap().clear();
    }

    /// The weight the read-ahead may have queued, in flight and fetched
    /// but unread in `cache`: half its cold ring, and at most
    /// [`SHARE_PERCENT`] of it.
    pub(crate) fn room(cache: &MessageCache) -> u64 {
        (cache.capacity_bytes() / 100 * SHARE_PERCENT).min(cache.cold_room() / 2)
    }

    /// The weight queued, in flight and fetched but unread in `cache`.
    #[cfg(test)]
    fn outstanding(&self, cache: &MessageCache) -> u64 {
        let queued = self.state.lock().unwrap().queued;
        queued.saturating_add(cache.read_ahead_unread(unread_since()))
    }

    /// Queue `fetches`, nearest steps first, while what is outstanding,
    /// `unread` fetched but unread among it, stays within `room` and the
    /// queue within [`MAX_QUEUED`], stopping at the first that does not
    /// fit, and skipping those already queued or in flight. Returns how
    /// many workers to start.
    fn enqueue(&self, fetches: Vec<Fetch>, room: u64, unread: u64) -> usize {
        let mut guard = self.state.lock().unwrap();
        let state = &mut *guard;
        let mut outstanding = state.queued.saturating_add(unread);
        for fetch in fetches {
            if state.pending.contains(&fetch.key) {
                continue;
            }
            if outstanding.saturating_add(fetch.weight) > room || state.queue.len() >= MAX_QUEUED {
                break;
            }
            outstanding += fetch.weight;
            state.queued += fetch.weight;
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
    fn next(&self, worker: &mut Worker<'_>) -> Option<Job<'_>> {
        let mut state = self.state.lock().unwrap();
        let next = state.queue.pop_front();
        if next.is_none() {
            worker.stop(&mut state);
        }
        next.map(|fetch| Job { ahead: self, fetch })
    }

    /// `fetch` is done: no longer pending, and its weight is no longer
    /// queued; once admitted, the message cache counts it until read.
    fn finish(&self, fetch: &Fetch) {
        let mut guard = self.state.lock().unwrap();
        let state = &mut *guard;
        state.pending.remove(&fetch.key);
        state.queued = state.queued.saturating_sub(fetch.weight);
    }

    /// Wait until every worker has stopped, at most `timeout`.
    #[cfg(test)]
    pub(crate) fn wait_idle(&self, timeout: Duration) {
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

    /// Make every view drawn and frame planned `by` older, for tests of
    /// the windows.
    #[cfg(test)]
    pub(crate) fn age(&self, by: Duration) {
        let older = |at: &mut Instant| match at.checked_sub(by) {
            Some(earlier) => {
                *at = earlier;
                true
            }
            None => false,
        };
        self.drawn.lock().unwrap().retain(|_, at| older(at));
        self.planned.lock().unwrap().retain(|_, at| older(at));
    }
}

/// A fetch taken from the queue, finished when dropped, so that on every
/// path, a panic included, it leaves `pending` and gives its weight back.
struct Job<'a> {
    ahead: &'a ReadAhead,
    fetch: Fetch,
}

impl Drop for Job<'_> {
    fn drop(&mut self) {
        self.ahead.finish(&self.fetch);
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

    /// What a map field of `run` draws for `parameters` (none: the default
    /// layer) at `field`, whatever its step: one id per tile or image of an
    /// animation, in this view.
    fn view_id(&self, run: DateTime<Utc>, parameters: &[&str], field: &MapField<'_>) -> u64 {
        let mut hasher = DefaultHasher::new();
        (self.family, run, parameters, field.z.map(f64::to_bits)).hash(&mut hasher);
        (field.bbox.map(f64::to_bits), field.width, field.height).hash(&mut hasher);
        std::mem::discriminant(field.output_crs).hash(&mut hasher);
        hasher.finish()
    }

    /// Frame `step` of `run` for `parameters` (none: the default layer) at
    /// `z`, in this view: one id per frame, whatever its tiles.
    fn frame_id(&self, run: DateTime<Utc>, step: u32, parameters: &[&str], z: Option<f64>) -> u64 {
        let mut hasher = DefaultHasher::new();
        (self.family, run, step, parameters, z.map(f64::to_bits)).hash(&mut hasher);
        hasher.finish()
    }

    /// After a map render drew `step` of `run` for `parameters` (none: the
    /// default layer) at `field`: read its next steps ahead when the view
    /// is animating (see the module docs). On the request's thread two
    /// hashes and two locks per tile, and once per frame the plan: no I/O,
    /// nothing it waits for.
    pub(crate) fn read_ahead_after(
        &self,
        catalog: &Catalog,
        run: &ForecastRun,
        step: u32,
        parameters: &[&str],
        field: &MapField<'_>,
    ) {
        let (Some(ahead), Some(cache)) = (&self.source.read_ahead, &self.source.message_cache)
        else {
            return;
        };
        if self.source.shutdown.is_shutdown() {
            return;
        }
        let carried = if parameters.is_empty() {
            CarriedSteps::Every(run)
        } else {
            catalog.carried_steps(run, parameters)
        };
        let view = self.view_id(run.reference_time, parameters, field);
        if !ahead.animating(view, step, carried.before(step))
            || !ahead.plan_once(self.frame_id(run.reference_time, step, parameters, field.z))
        {
            return;
        }
        let steps: Vec<&StepFile> = carried
            .after(step)
            .take(ahead.depth)
            .map(|(_, file)| file)
            .collect();
        let names: Vec<Option<&str>> = if parameters.is_empty() {
            vec![None]
        } else {
            parameters.iter().copied().map(Some).collect()
        };
        let grids = self.source.grid_cache.as_ref();
        // What the render draws at each step, the same selection, nearest
        // steps first.
        let fetches: Vec<Fetch> = steps
            .iter()
            .flat_map(|&file| {
                names.iter().filter_map(move |&name| {
                    let drawn = self.map_message(catalog, run, file, name, field.z).ok()?;
                    let length = drawn.entry.length?;
                    let key = MessageKey::new(Path::from(drawn.url).as_ref(), drawn.entry);
                    let resident = cache.is_resident(&key)
                        || grids.is_some_and(|grids| grids.contains(drawn.url, drawn.entry.offset));
                    (!resident).then(|| Fetch {
                        url: drawn.url.to_owned(),
                        entry: drawn.entry.clone(),
                        key,
                        weight: message_weight(drawn.url, length),
                    })
                })
            })
            .collect();
        if fetches.is_empty() {
            return;
        }
        let unread = cache.read_ahead_unread(unread_since());
        let start = ahead.enqueue(fetches, ReadAhead::room(cache), unread);
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
        while let Some(job) = ahead.next(&mut worker) {
            if self.source.shutdown.is_shutdown() {
                continue;
            }
            if let Err(error) = self.read_ahead_message(&job.fetch) {
                tracing::debug!(
                    "Collection '{}': map read-ahead of {} at {} failed; a map reads it \
                     itself: {error}",
                    self.collection_id,
                    job.fetch.url,
                    job.fetch.entry.offset
                );
            }
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
            ahead.wait_idle(Duration::from_secs(30));
        }
    }
}

#[cfg(test)]
mod tests;
