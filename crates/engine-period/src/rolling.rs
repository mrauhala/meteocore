//! The poll side of the windows (#1379): what each window's grids hold,
//! and what one poll reads and changes to keep them ending at the source's
//! latest frame. No I/O of its own: a poll is handed the source's frame
//! times and a function that reads one strip of one frame.
//!
//! # Strips
//!
//! A frame is never read whole: it is read in strips of whole rows of the
//! working grid ([`Strips`]), top to bottom, each folded into every grid
//! that takes the frame and dropped before the next is read. Whatever type
//! the source returns (raw bytes, `f32`, boxed `f64` at 16 bytes a cell), a
//! poll holds one strip of one frame at a time. Live updates and rebuilds
//! read through the same strips, and a cell gets the same operations in
//! the same frame order whatever the strip size, so strip-wise grids equal
//! whole-frame grids bit for bit.
//!
//! # One poll
//!
//! 1. Each window's target is the frames it should hold now
//!    ([`plan::target`]). A *live* window (one whose grids are served) is
//!    brought to it in this poll or not at all: the frames that left are
//!    read again and subtracted, the new ones read and added, frame by
//!    frame and strip by strip, each strip under the window's write lock.
//!    Frames needed by several windows are read once.
//!
//! # Versions while a window changes
//!
//! A window's served version ([`Versions`], what `content_version_for`
//! reads, O(1)) keys the map caches. Before the first strip of a poll
//! changes a window's grids, before its grids are dropped, and before a
//! rebuild that changes what it shows replaces them, the window moves to a
//! fresh *in-flux* version: a request rendering it part way through is
//! keyed on that, never on the version the window had before, whose cache
//! entries keep the consistent pre-poll image. The engine stores the
//! window's next version, another fresh one, only after it has published
//! the poll's snapshot, so the in-flux version is asked for by no request
//! after the poll.
//! 2. A live window is *broken* (its grids dropped, nothing served) when a
//!    frame it must take out cannot be read: the source no longer has it,
//!    or the read fails, or more frames changed at once than one poll may
//!    read. It is then rebuilt.
//! 3. A *rebuild* sums a window from scratch into a second set of grids,
//!    newest frames first, with what is left of the poll's read budget, and
//!    replaces the live grids once it holds the target. It builds a new or
//!    broken window, and re-sums a live one once it has slid
//!    [`RESUM_AFTER_LENGTHS`] times its own length, so `f32` rounding
//!    cannot build up, or after the source revised its frames. One re-sum of a
//!    live window runs at a time, so the grids held are the windows' plus
//!    at most one.
//!
//! A frame that cannot be read is left out of the targets for
//! [`FAILED_RETRY_POLLS`] polls, then read again; while it is out, its
//! window's coverage shows the gap.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use chrono::{DateTime, Duration, Utc};
use ds_core::error::DataServerError;

use crate::accum::{Frame, Grids, Op, Statistic};
use crate::plan::{self, Ledger};

/// Frames one window may hold: the per-cell count is a `u16`.
pub const MAX_WINDOW_FRAMES: usize = u16::MAX as usize;

/// Polls a frame that could not be read stays out of the windows.
pub const FAILED_RETRY_POLLS: u64 = 20;

/// Window lengths a live window slides before it is re-summed from
/// scratch: `f32` rounding over that many steps stays below 1e-6 of the
/// field (`tests/rolling.rs`), and a re-sum reads the window's frames once,
/// a quarter of a frame per window per step.
pub const RESUM_AFTER_LENGTHS: u64 = 4;

/// The windows' served versions, shared by the poll and the requests: one
/// atomic per window, and one counter every fresh version comes from, so
/// versions only grow and are never reused, across polls and grid resets.
#[derive(Debug)]
pub struct Versions {
    windows: Vec<AtomicU64>,
    next: AtomicU64,
}

impl Versions {
    /// `windows` versions, all 1.
    pub fn new(windows: usize) -> Self {
        Self {
            windows: (0..windows).map(|_| AtomicU64::new(1)).collect(),
            next: AtomicU64::new(1),
        }
    }

    /// A version no window has had.
    pub fn fresh(&self) -> u64 {
        self.next.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Window `w`'s served version.
    pub fn get(&self, w: usize) -> u64 {
        self.windows[w].load(Ordering::Acquire)
    }

    /// The highest version handed out: moves whenever any window's does.
    pub fn top(&self) -> u64 {
        self.next.load(Ordering::Acquire)
    }

    /// Serve `version` for window `w`.
    pub fn set(&self, w: usize, version: u64) {
        self.windows[w].store(version, Ordering::Release);
    }

    /// Move window `w` to a fresh in-flux version: its grids are about to
    /// change, and a render part way through must not be keyed on the
    /// version it served until now.
    pub fn flux(&self, w: usize) {
        self.set(w, self.fresh());
    }
}

/// The fixed part of the windows' bookkeeping.
#[derive(Debug, Clone)]
pub struct RollerCfg {
    pub statistic: Statistic,
    /// Window lengths, one per window.
    pub lengths: Vec<Duration>,
    /// How frames are read: the working grid's size and its strips.
    pub strips: Strips,
    /// Frames one poll reads at most.
    pub max_reads: usize,
    /// The source's frame spacing, when configured; else estimated.
    pub cadence: Option<Duration>,
}

/// What a built window holds after a poll.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Built {
    /// The window's end: the source's latest frame.
    pub end: DateTime<Utc>,
    /// The share of the window its frames count for.
    pub coverage: f64,
    /// The frames it holds.
    pub frames: usize,
}

/// One window after a poll.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowStatus {
    /// `None` until the window is built, and while it is broken.
    pub built: Option<Built>,
    /// The version to serve once the poll's snapshot is published: moves
    /// whenever what the window's grids hold changes.
    pub version: u64,
    /// Why the window is not built, or what its rebuild has done so far.
    pub waiting: Option<String>,
}

/// What one poll did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PollStats {
    /// Frames read, each read counted once whatever its strips.
    pub reads: usize,
    /// Strips read: the source reads it was asked for.
    pub strip_reads: usize,
    /// The most bytes of frame values held at once: one strip.
    pub peak_frame_bytes: u64,
}

/// Reads one strip of one frame: `rows` of the working grid, every column,
/// as the statistic's input.
pub type ReadStrip<'a> =
    dyn FnMut(DateTime<Utc>, Range<u32>) -> Result<Frame, DataServerError> + 'a;

/// How frames are read: whole rows of the `width × height` working grid,
/// at most `rows` at a time, top to bottom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Strips {
    pub width: u32,
    pub height: u32,
    /// Rows per strip, at least one.
    pub rows: u32,
}

impl Strips {
    /// Strips of at most `max_cells` cells, and at least one row, of a
    /// `width × height` grid.
    pub fn new(width: u32, height: u32, max_cells: usize) -> Self {
        let rows = (max_cells / width.max(1) as usize).clamp(1, height.max(1) as usize) as u32;
        Self {
            width,
            height,
            rows,
        }
    }

    pub fn cells(&self) -> usize {
        self.width as usize * self.height as usize
    }

    /// The strips' row ranges, top to bottom: every row in exactly one.
    pub fn ranges(&self) -> impl Iterator<Item = Range<u32>> + '_ {
        (0..self.height)
            .step_by(self.rows as usize)
            .map(|r0| r0..(r0 + self.rows).min(self.height))
    }

    /// Read frame `t` strip by strip, handing each strip and the grid index
    /// of its first cell to `apply`. On a failed strip, the rows handed to
    /// `apply` before it and the error.
    fn read(
        &self,
        t: DateTime<Utc>,
        read: &mut ReadStrip<'_>,
        stats: &mut PollStats,
        mut apply: impl FnMut(&Frame, usize),
    ) -> Result<(), (u32, DataServerError)> {
        stats.reads += 1;
        for rows in self.ranges() {
            let start = rows.start;
            let cells = rows.len() * self.width as usize;
            stats.strip_reads += 1;
            let frame = read(t, rows).map_err(|e| (start, e))?;
            if frame.len() != cells {
                return Err((
                    start,
                    DataServerError::Engine(format!(
                        "source frame {t} gave {} cells for a strip of {cells}",
                        frame.len()
                    )),
                ));
            }
            stats.peak_frame_bytes = stats.peak_frame_bytes.max(frame.bytes());
            apply(&frame, start as usize * self.width as usize);
        }
        Ok(())
    }
}

/// The grid operations that bring one set of grids to its target.
#[derive(Debug, Default)]
struct Ops {
    /// Frames to subtract, with the seconds they were added with.
    remove: BTreeMap<DateTime<Utc>, u32>,
    /// Frames to add, with the seconds they count for.
    add: BTreeMap<DateTime<Utc>, u32>,
    /// Frames whose seconds changed where the statistic does not weigh
    /// time: the ledger changes, the grids do not.
    retime: Vec<(DateTime<Utc>, u32)>,
}

impl Ops {
    fn new(diff: plan::Diff, statistic: Statistic) -> Self {
        let mut ops = Ops {
            remove: diff.removed.into_iter().collect(),
            add: diff.added.into_iter().collect(),
            retime: Vec::new(),
        };
        for (t, old, new) in diff.retimed {
            if statistic.weighs_time() {
                ops.remove.insert(t, old);
                ops.add.insert(t, new);
            } else {
                ops.retime.push((t, new));
            }
        }
        ops
    }

    fn frames(&self) -> BTreeSet<DateTime<Utc>> {
        self.remove.keys().chain(self.add.keys()).copied().collect()
    }

    fn is_empty(&self) -> bool {
        self.remove.is_empty() && self.add.is_empty() && self.retime.is_empty()
    }
}

/// A live window's ledger: the frames its served grids hold.
struct Live {
    ledger: Ledger,
    /// Grid operations since the grids were last summed from scratch.
    ops: u64,
}

/// A rebuild in progress.
struct Rebuild {
    grids: Grids,
    ledger: Ledger,
    /// This poll's remaining operations.
    ops: Ops,
    /// Whether replacing the live grids changes what they show beyond
    /// rounding (a new or broken window, or revised source frames).
    changes_content: bool,
}

impl Rebuild {
    fn new(cells: usize, changes_content: bool) -> Self {
        Self {
            grids: Grids::new(cells),
            ledger: Ledger::new(),
            ops: Ops::default(),
            changes_content,
        }
    }

    /// Apply the operations frame `t` has here to one of its strips.
    fn feed_strip(&mut self, t: DateTime<Utc>, frame: &Frame, statistic: Statistic, offset: usize) {
        if let Some(&secs) = self.ops.remove.get(&t) {
            self.grids
                .apply_at(frame, statistic, secs, Op::Remove, offset);
        }
        if let Some(&secs) = self.ops.add.get(&t) {
            self.grids.apply_at(frame, statistic, secs, Op::Add, offset);
        }
    }

    /// Frame `t` was fed whole: its operations are done.
    fn commit(&mut self, t: DateTime<Utc>) {
        if self.ops.remove.remove(&t).is_some() {
            self.ledger.remove(&t);
        }
        if let Some(secs) = self.ops.add.remove(&t) {
            self.ledger.insert(t, secs);
        }
    }

    fn takes(&self, t: &DateTime<Utc>) -> bool {
        self.ops.remove.contains_key(t) || self.ops.add.contains_key(t)
    }

    /// Start over: part of a frame went in that cannot be taken out.
    fn reset(&mut self, target: &Ledger, statistic: Statistic) {
        let cells = self.grids.cells();
        *self = Rebuild::new(cells, self.changes_content);
        self.ops = Ops::new(plan::diff(&Ledger::new(), target), statistic);
    }
}

struct WindowState {
    live: Option<Live>,
    rebuild: Option<Rebuild>,
    version: u64,
    /// The source revised its frames: re-sum, and the swap changes content.
    revised: bool,
    /// A frame left the source while it was in the window: from now on the
    /// window is built only while the source holds a frame more than it.
    needs_margin: bool,
}

/// Every window's bookkeeping. The live grids themselves are the engine's
/// (`slots`), read by requests under each window's lock.
pub struct Roller {
    cfg: RollerCfg,
    windows: Vec<WindowState>,
    cadence: Option<Duration>,
    /// Frames that could not be read, until the poll they are read again.
    failed: BTreeMap<DateTime<Utc>, u64>,
    poll: u64,
    versions: std::sync::Arc<Versions>,
}

impl Roller {
    /// Fresh bookkeeping with nothing built, versions from `versions` (the
    /// engine's, kept across grid resets, so no version is reused).
    pub fn new(cfg: RollerCfg, versions: std::sync::Arc<Versions>) -> Self {
        // Fresh, never an in-flux value a reset served while it dropped the
        // old grids.
        let version = versions.fresh();
        let windows = cfg
            .lengths
            .iter()
            .map(|_| WindowState {
                live: None,
                rebuild: None,
                version,
                revised: false,
                needs_margin: false,
            })
            .collect();
        Self {
            cfg,
            windows,
            cadence: None,
            failed: BTreeMap::new(),
            poll: 0,
            versions,
        }
    }

    /// The source revised frames it had already served (its content
    /// version moved): re-sum every window from scratch.
    pub fn mark_revised(&mut self) {
        for w in &mut self.windows {
            w.revised = true;
        }
    }

    /// Bytes of the grids of rebuilds in progress (the live grids are the
    /// engine's).
    pub fn rebuild_bytes(&self) -> u64 {
        self.windows
            .iter()
            .filter_map(|w| w.rebuild.as_ref())
            .map(|r| r.grids.bytes())
            .sum()
    }

    /// The version window `w` serves once the engine publishes this poll:
    /// fresh, served only after the snapshot (`WindowStatus::version`).
    fn bump(&mut self, w: usize) {
        self.windows[w].version = self.versions.fresh();
    }

    /// One poll: bring every window to the frames it should hold for the
    /// source's frame times `times` (ascending), reading frames with
    /// `read`, at most `max_reads` of them. `slots` are the live grids, one
    /// per window.
    pub fn poll(
        &mut self,
        times: &[DateTime<Utc>],
        slots: &[RwLock<Option<Grids>>],
        read: &mut ReadStrip<'_>,
    ) -> (Vec<WindowStatus>, PollStats) {
        let statistic = self.cfg.statistic;
        let strips = self.cfg.strips;
        let cells = strips.cells();
        let n = self.windows.len();
        self.poll += 1;
        let poll = self.poll;
        self.failed.retain(|_, retry| *retry > poll);

        let in_source: BTreeSet<DateTime<Utc>> = times.iter().copied().collect();
        let usable: Vec<DateTime<Utc>> = times
            .iter()
            .copied()
            .filter(|t| !self.failed.contains_key(t))
            .collect();
        self.cadence = self
            .cfg
            .cadence
            .or_else(|| plan::cadence(&usable, self.cadence));
        let cap = self.cadence.unwrap_or_else(|| Duration::seconds(1));
        let end = usable.last().copied();
        let targets: Vec<Ledger> = self
            .cfg
            .lengths
            .iter()
            .map(|&length| plan::target(&usable, length, cap))
            .collect();
        let mut waiting: Vec<Option<String>> = vec![None; n];

        // 1. What each live window needs this poll; shortest first, so a
        // burst breaks the long windows, which a rebuild would read anyway.
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by_key(|&w| self.cfg.lengths[w]);
        let mut broken = vec![false; n];
        let mut live_ops: Vec<Option<Ops>> = (0..n).map(|_| None).collect();
        let mut needed: BTreeSet<DateTime<Utc>> = BTreeSet::new();
        for &w in &order {
            let Some(live) = &self.windows[w].live else {
                continue;
            };
            if targets[w].len() > MAX_WINDOW_FRAMES {
                broken[w] = true;
                waiting[w] = Some(too_many(targets[w].len()));
                continue;
            }
            let ops = Ops::new(plan::diff(&live.ledger, &targets[w]), statistic);
            if let Some(t) = ops.remove.keys().find(|t| !in_source.contains(t)) {
                broken[w] = true;
                self.windows[w].needs_margin = true;
                waiting[w] = Some(format!(
                    "rebuilding: frame {t} left the source while it was in the window"
                ));
                continue;
            }
            if let Some(t) = ops.remove.keys().find(|t| self.failed.contains_key(t)) {
                broken[w] = true;
                waiting[w] = Some(format!(
                    "rebuilding: frame {t} could not be read to take it out"
                ));
                continue;
            }
            let mut more = needed.clone();
            more.extend(ops.frames());
            if more.len() > self.cfg.max_reads {
                broken[w] = true;
                waiting[w] = Some(format!(
                    "rebuilding: {} frames changed at once, more than max_reads_per_cycle ({})",
                    ops.frames().len(),
                    self.cfg.max_reads
                ));
                continue;
            }
            needed = more;
            live_ops[w] = Some(ops);
        }

        // 2. Start, keep or drop rebuilds, and plan this poll's part.
        let mut resumming = (0..n).any(|w| {
            self.windows[w].rebuild.is_some() && self.windows[w].live.is_some() && !broken[w]
        });
        for w in 0..n {
            let has_live = self.windows[w].live.is_some() && !broken[w];
            let length = self.cfg.lengths[w];
            let margin = if self.windows[w].needs_margin {
                cap
            } else {
                Duration::zero()
            };
            let blocked = if targets[w].is_empty() {
                Some("waiting for the source's frames".to_string())
            } else if targets[w].len() > MAX_WINDOW_FRAMES {
                Some(too_many(targets[w].len()))
            } else if !plan::holds(&usable, length, margin) {
                Some(format!(
                    "the source holds {} of frames; this window needs {}{}",
                    span(&usable),
                    human(length + margin),
                    if margin > Duration::zero() {
                        " (the window and one frame before it: the source dropped a frame \
                         still in the window)"
                    } else {
                        ""
                    }
                ))
            } else {
                None
            };
            let state = &mut self.windows[w];
            if let Some(reason) = blocked {
                if state.rebuild.take().is_some() && has_live {
                    resumming = false;
                }
                if !has_live {
                    waiting[w].get_or_insert(reason);
                }
                continue;
            }
            if state.rebuild.is_none() {
                if !has_live {
                    state.rebuild = Some(Rebuild::new(cells, true));
                } else if !resumming {
                    let live = state.live.as_ref().expect("has_live");
                    // A step is two grid operations: one frame in, one out.
                    let due = live.ops >= 2 * RESUM_AFTER_LENGTHS * targets[w].len().max(1) as u64;
                    if due || state.revised {
                        state.rebuild = Some(Rebuild::new(cells, state.revised));
                        state.revised = false;
                        resumming = true;
                    }
                }
            }
            if let Some(rebuild) = &mut state.rebuild {
                let ops = Ops::new(plan::diff(&rebuild.ledger, &targets[w]), statistic);
                for &(t, secs) in &ops.retime {
                    rebuild.ledger.insert(t, secs);
                }
                rebuild.ops = Ops {
                    retime: Vec::new(),
                    ..ops
                };
            }
        }

        // 3. Live windows, frame by frame in time order, each frame strip
        // by strip: a strip goes into every window that takes the frame,
        // under that window's write lock, and into the rebuilds that need
        // it, and is dropped. A frame that fails part way breaks the windows
        // it reached and restarts the rebuilds: part of a frame cannot be
        // taken out again.
        let mut stats = PollStats::default();
        let frames: BTreeSet<DateTime<Utc>> =
            live_ops.iter().flatten().flat_map(Ops::frames).collect();
        let mut grid_ops = vec![0u64; n];
        let mut fluxed = vec![false; n];
        let versions = self.versions.clone();
        for t in frames {
            let takers: Vec<usize> = (0..n)
                .filter(|&w| !broken[w])
                .filter(|&w| {
                    live_ops[w]
                        .as_ref()
                        .is_some_and(|ops| ops.frames().contains(&t))
                })
                .collect();
            if takers.is_empty() {
                continue;
            }
            let done = if self.failed.contains_key(&t) {
                Err(0)
            } else {
                let windows = &mut self.windows;
                let fluxed = &mut fluxed;
                let result = strips.read(t, read, &mut stats, |frame, offset| {
                    for &w in &takers {
                        if !fluxed[w] {
                            versions.flux(w);
                            fluxed[w] = true;
                        }
                        let ops = live_ops[w].as_ref().expect("a taker has ops");
                        let mut slot = slots[w].write().unwrap_or_else(|e| e.into_inner());
                        let grids = slot.as_mut().expect("a live window has grids");
                        if let Some(&secs) = ops.remove.get(&t) {
                            grids.apply_at(frame, statistic, secs, Op::Remove, offset);
                        }
                        if let Some(&secs) = ops.add.get(&t) {
                            grids.apply_at(frame, statistic, secs, Op::Add, offset);
                        }
                    }
                    for state in windows.iter_mut() {
                        if let Some(rebuild) = &mut state.rebuild {
                            rebuild.feed_strip(t, frame, statistic, offset);
                        }
                    }
                });
                result.map_err(|(rows, e)| {
                    self.fail(t, &e);
                    rows
                })
            };
            match done {
                Ok(()) => {
                    for &w in &takers {
                        let ops = live_ops[w].as_ref().expect("a taker has ops");
                        let live = self.windows[w].live.as_mut().expect("live");
                        if ops.remove.contains_key(&t) {
                            live.ledger.remove(&t);
                            grid_ops[w] += 1;
                        }
                        if let Some(&secs) = ops.add.get(&t) {
                            live.ledger.insert(t, secs);
                            grid_ops[w] += 1;
                        }
                    }
                    self.commit(t);
                }
                Err(rows) => {
                    for &w in &takers {
                        let removes = live_ops[w]
                            .as_ref()
                            .is_some_and(|ops| ops.remove.contains_key(&t));
                        // An add that never started leaves the frame out,
                        // and the coverage shows it.
                        if removes || rows > 0 {
                            broken[w] = true;
                            waiting[w] =
                                Some(format!("rebuilding: frame {t} could not be read whole"));
                        }
                    }
                    if rows > 0 {
                        self.reset_rebuilds(t, &targets);
                    }
                }
            }
        }
        for w in 0..n {
            let Some(ops) = live_ops[w].as_ref().filter(|_| !broken[w]) else {
                continue;
            };
            let live = self.windows[w].live.as_mut().expect("live");
            for &(t, secs) in &ops.retime {
                live.ledger.insert(t, secs);
            }
            live.ops += grid_ops[w];
            if fluxed[w] || grid_ops[w] > 0 || !ops.retime.is_empty() {
                self.bump(w);
            }
        }

        // 4. Rebuilds with what is left of the budget, one frame at a time,
        // strip by strip into every rebuild that needs it: frames to take
        // out first (a rebuild that cannot starts over), then the newest
        // frames to add.
        let mut budget = self.cfg.max_reads.saturating_sub(stats.reads);
        let removals: BTreeSet<DateTime<Utc>> = self
            .windows
            .iter()
            .filter_map(|w| w.rebuild.as_ref())
            .flat_map(|r| r.ops.remove.keys().copied())
            .collect();
        for t in removals {
            if self.failed.contains_key(&t) {
                self.reset_rebuilds(t, &targets);
                continue;
            }
            // Out of budget: taken out next poll.
            if budget == 0 {
                continue;
            }
            budget -= 1;
            self.read_into_rebuilds(t, read, &mut stats, &targets);
        }
        let additions: BTreeSet<DateTime<Utc>> = self
            .windows
            .iter()
            .filter_map(|w| w.rebuild.as_ref())
            .filter(|r| r.ops.remove.is_empty())
            .flat_map(|r| r.ops.add.keys().copied())
            .collect();
        for t in additions.into_iter().rev() {
            if budget == 0 {
                break;
            }
            if self.failed.contains_key(&t) {
                continue;
            }
            budget -= 1;
            self.read_into_rebuilds(t, read, &mut stats, &targets);
        }

        // 5. Swap in finished rebuilds, drop broken windows, report.
        let mut statuses = Vec::with_capacity(n);
        for w in 0..n {
            if broken[w] && self.windows[w].live.is_some() {
                self.versions.flux(w);
                *slots[w].write().unwrap_or_else(|e| e.into_inner()) = None;
                self.windows[w].live = None;
                self.bump(w);
            }
            let finished = self.windows[w]
                .rebuild
                .as_ref()
                .is_some_and(|r| r.ops.is_empty() && !r.ledger.is_empty());
            if finished {
                let rebuild = self.windows[w].rebuild.take().expect("finished");
                let changes = rebuild.changes_content
                    || self.windows[w]
                        .live
                        .as_ref()
                        .is_none_or(|live| live.ledger != rebuild.ledger);
                if changes {
                    self.versions.flux(w);
                }
                *slots[w].write().unwrap_or_else(|e| e.into_inner()) = Some(rebuild.grids);
                self.windows[w].live = Some(Live {
                    ledger: rebuild.ledger,
                    ops: 0,
                });
                if changes {
                    self.bump(w);
                }
            }
            let state = &self.windows[w];
            if waiting[w].is_none() && state.live.is_none() {
                if let Some(r) = &state.rebuild {
                    waiting[w] = Some(format!(
                        "building: {} of {} frames read",
                        r.ledger.len(),
                        targets[w].len()
                    ));
                }
            }
            statuses.push(WindowStatus {
                built: state.live.as_ref().zip(end).map(|(live, end)| Built {
                    end,
                    coverage: plan::coverage(&live.ledger, self.cfg.lengths[w]),
                    frames: live.ledger.len(),
                }),
                version: state.version,
                waiting: waiting[w].take(),
            });
        }
        (statuses, stats)
    }

    /// Frame `t` went whole into the rebuilds that needed it.
    fn commit(&mut self, t: DateTime<Utc>) {
        for state in &mut self.windows {
            if let Some(rebuild) = &mut state.rebuild {
                rebuild.commit(t);
            }
        }
    }

    /// Restart every rebuild that needed frame `t`, which went in part way
    /// or cannot be taken out.
    fn reset_rebuilds(&mut self, t: DateTime<Utc>, targets: &[Ledger]) {
        let statistic = self.cfg.statistic;
        for (state, target) in self.windows.iter_mut().zip(targets) {
            if let Some(rebuild) = state.rebuild.as_mut().filter(|r| r.takes(&t)) {
                rebuild.reset(target, statistic);
            }
        }
    }

    /// Read frame `t` strip by strip into every rebuild that needs it.
    fn read_into_rebuilds(
        &mut self,
        t: DateTime<Utc>,
        read: &mut ReadStrip<'_>,
        stats: &mut PollStats,
        targets: &[Ledger],
    ) {
        let statistic = self.cfg.statistic;
        let strips = self.cfg.strips;
        let windows = &mut self.windows;
        let result = strips.read(t, read, stats, |frame, offset| {
            for state in windows.iter_mut() {
                if let Some(rebuild) = &mut state.rebuild {
                    rebuild.feed_strip(t, frame, statistic, offset);
                }
            }
        });
        match result {
            Ok(()) => self.commit(t),
            Err((rows, e)) => {
                self.fail(t, &e);
                // Nothing went in: the frame stays out until it reads. A
                // frame to take out restarts its rebuilds either way.
                let partial = rows > 0;
                let statistic = self.cfg.statistic;
                for (state, target) in self.windows.iter_mut().zip(targets) {
                    if let Some(rebuild) = state.rebuild.as_mut() {
                        if partial && rebuild.takes(&t) || rebuild.ops.remove.contains_key(&t) {
                            rebuild.reset(target, statistic);
                        }
                    }
                }
            }
        }
    }

    fn fail(&mut self, t: DateTime<Utc>, e: &DataServerError) {
        tracing::warn!("period: source frame {t} could not be read ({e}); left out for now");
        self.failed.insert(t, self.poll + FAILED_RETRY_POLLS);
    }
}

fn too_many(frames: usize) -> String {
    format!("the window would hold {frames} frames; at most {MAX_WINDOW_FRAMES}")
}

fn span(times: &[DateTime<Utc>]) -> String {
    match (times.first(), times.last()) {
        (Some(&a), Some(&b)) => human(b - a),
        _ => "none".to_string(),
    }
}

/// A duration as `1 h`, `30 min`, `2 d`, `1 h 5 min`.
pub fn human(d: Duration) -> String {
    let mut secs = d.num_seconds().max(0);
    if secs == 0 {
        return "0 s".to_string();
    }
    let mut parts = Vec::new();
    for (unit, size) in [("d", 86_400), ("h", 3_600), ("min", 60), ("s", 1)] {
        if secs >= size {
            parts.push(format!("{} {unit}", secs / size));
            secs %= size;
        }
    }
    parts.join(" ")
}
