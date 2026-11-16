//! The memory governor's decisions (#1315), pure: inputs in, actions out,
//! no clock, no cache, no server. `super` reads the inputs and applies the
//! actions.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use ds_cache::plan::grant::{class, Class};
use ds_cache::plan::{Entry, Kind, Row, Scope, SizeSource};
use ds_cache::MIB;

/// A cache or budget, as the governor keeps state about it across ticks.
pub(crate) type Key = (Kind, &'static str, Scope);

/// The key of `entry`.
pub(crate) fn key(entry: &Entry) -> Key {
    (entry.kind, entry.id, entry.scope.clone())
}

/// How often the valve samples resident memory: a quarter of [`OVER_FOR`],
/// so pressure is acted on 60 to 75 s after it starts. A sample is one
/// jemalloc epoch advance and one stats read.
pub(crate) const SAMPLE_EVERY: Duration = Duration::from_secs(15);

/// How often the plan is fitted again between reloads.
pub(crate) const REPLAN_EVERY: Duration = Duration::from_secs(300);

/// The re-plan tick applies a grant only when it moves a capacity by more
/// than this share of it, in percent, ...
pub(crate) const MOVE_PERCENT: u64 = 10;

/// ... and by more than this.
pub(crate) const MOVE_BYTES: u64 = 64 * MIB;

/// Resident above the total this long: the valve acts.
pub(crate) const OVER_FOR: Duration = Duration::from_secs(60);

/// Resident under [`UNDER_PERCENT`] of the total this long: the valve's
/// ceilings are lifted, a step per re-plan tick.
pub(crate) const UNDER_FOR: Duration = Duration::from_secs(600);

/// See [`UNDER_FOR`]. Between it and the total the valve holds: the gap
/// keeps a restore from pushing the process straight back over.
pub(crate) const UNDER_PERCENT: u64 = 90;

/// The valve lowers a cache to this share of what it holds, in percent.
pub(crate) const RELIEVE_TO_PERCENT: u64 = 90;

/// A restore step raises a valve's ceiling by this share of the cache's
/// grant, in percent, at least 1 MiB.
pub(crate) const RESTORE_PERCENT: u64 = 5;

/// While over the total with nothing left to lower, the valve purges dirty
/// pages at most this often ...
pub(crate) const PURGE_EVERY: Duration = Duration::from_secs(600);

/// ... and WARNs that it is still over at most this often.
pub(crate) const FLOORS_WARN_EVERY: Duration = Duration::from_secs(1800);

/// A ceiling the valve set on a cache, with the source the cache's size had
/// then: a reload that pins the cache or gives it other bounds supersedes
/// it ([`Ceiling::holds`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ceiling {
    pub bytes: u64,
    pub source: SizeSource,
}

impl Ceiling {
    /// Whether the ceiling still holds for a cache whose size now comes
    /// from `source`: the source it was set under.
    pub(crate) fn holds(&self, source: &SizeSource) -> bool {
        self.source == *source
    }
}

/// `grant` under a valve's ceiling of `cap`, which never takes the cache
/// below `floor`: the plan's floor for it, a range's `min` included.
pub(crate) fn ceiled(grant: u64, cap: u64, floor: u64) -> u64 {
    grant.min(cap.max(floor))
}

/// A resize that stopped short, as the tick tried it: what it is not tried
/// again with. Any of them moving tries it again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Refused {
    /// The capacity it stopped at.
    pub capacity: u64,
    pub grant: u64,
    /// The least the cache took in place then.
    pub least: u64,
    /// Whether the valve held a ceiling on it.
    pub capped: bool,
}

/// Whether the re-plan tick applies `grant` to a capacity of `capacity`:
/// it moves it by more than [`MOVE_PERCENT`] of it and [`MOVE_BYTES`]. A
/// demand that jitters a few percent moves nothing.
pub(crate) fn moves_enough(capacity: u64, grant: u64) -> bool {
    let delta = capacity.abs_diff(grant);
    delta > MOVE_BYTES && delta.saturating_mul(100) > capacity.saturating_mul(MOVE_PERCENT)
}

/// The size the re-plan tick gives a cache or budget of `capacity` whose
/// grant is `grant`, or `None` to leave it:
///
/// - one the valve lowered (`cap`): its grant up to that ceiling, never
///   below `floor` ([`ceiled`]), however little it moves, since a restore
///   lifts the ceiling by small steps;
/// - one whose ceiling was just lifted or dropped (`lifted`): its grant;
/// - else its grant, when it moves far enough ([`moves_enough`]);
/// - never the move the tick tried last that stopped short, while
///   `refused` says nothing it depends on moved since: it is logged once,
///   not every tick.
pub(crate) fn tick_target(
    capacity: u64,
    grant: u64,
    floor: u64,
    cap: Option<u64>,
    lifted: bool,
    refused: bool,
) -> Option<u64> {
    if refused {
        return None;
    }
    match cap {
        Some(cap) => Some(ceiled(grant, cap, floor)),
        None if lifted || moves_enough(capacity, grant) => Some(grant),
        None => None,
    }
}

/// What the valve's timers say after a sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Signal {
    /// Over the total for [`OVER_FOR`]: lower the flexible caches.
    Relieve,
    /// Nothing to do now.
    Hold,
}

/// The valve's timers: since when resident memory has been over the total,
/// or under [`UNDER_PERCENT`] of it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Pressure {
    pub over_since: Option<Instant>,
    pub under_since: Option<Instant>,
}

impl Pressure {
    /// Note that the process holds `resident` bytes of `total` at `now`.
    /// Over the total for [`OVER_FOR`], [`Signal::Relieve`], once: the timer
    /// starts again, so pressure that lasts relieves again a minute later.
    /// Between [`UNDER_PERCENT`] and the total, both timers stop.
    pub(crate) fn observe(&mut self, now: Instant, resident: u64, total: u64) -> Signal {
        if resident > total {
            self.under_since = None;
            let since = *self.over_since.get_or_insert(now);
            if now.saturating_duration_since(since) >= OVER_FOR {
                self.over_since = Some(now);
                return Signal::Relieve;
            }
            return Signal::Hold;
        }
        self.over_since = None;
        if resident.saturating_mul(100) < total.saturating_mul(UNDER_PERCENT) {
            self.under_since.get_or_insert(now);
        } else {
            self.under_since = None;
        }
        Signal::Hold
    }

    /// Whether the process has been under [`UNDER_PERCENT`] of the total
    /// for [`UNDER_FOR`] at `now`: the valve's ceilings may be lifted.
    pub(crate) fn may_restore(&self, now: Instant) -> bool {
        self.under_since
            .is_some_and(|since| now.saturating_duration_since(since) >= UNDER_FOR)
    }

    /// `over`, `under` or `normal`, for `/admin/cache-plan`.
    pub(crate) fn state(&self) -> &'static str {
        match (self.over_since, self.under_since) {
            (Some(_), _) => "over",
            (None, Some(_)) => "under",
            (None, None) => "normal",
        }
    }
}

/// A cache as the valve reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Slot {
    pub entry: Entry,
    pub capacity: u64,
    pub used: u64,
    /// The least the valve takes it to: the plan's floor for it (a range's
    /// floor included) and the least it shrinks to in place.
    pub floor: u64,
    /// The valve may lower it: a cache no setting pins whose engines do not
    /// fill it ahead of requests ([`spared`]), and that a collection can
    /// reach.
    pub flexible: bool,
}

/// Whether the valve never lowers `row`'s cache: below what its engines
/// put there ahead of requests (a demand cache, satellite scans and station
/// grids, or a floor, the PVOL pre-warm) it turns into a re-download loop.
pub(crate) fn spared(row: &Row) -> bool {
    class(row.entry.id) == Class::Demand || row.demand.as_ref().is_some_and(|d| d.floor || d.ahead)
}

/// What the valve lowers each flexible cache holding anything to: 90 % of
/// what it holds ([`RELIEVE_TO_PERCENT`]), in whole MiB down, never below
/// its floor, and only where that is below its capacity.
pub(crate) fn relieve(slots: &[Slot]) -> Vec<(Key, u64)> {
    slots
        .iter()
        .filter(|slot| slot.flexible && slot.entry.kind == Kind::Cache && slot.used > 0)
        .filter_map(|slot| {
            let share =
                slot.used / 100 * RELIEVE_TO_PERCENT + slot.used % 100 * RELIEVE_TO_PERCENT / 100;
            let target = (share / MIB * MIB).max(slot.floor);
            (target < slot.capacity).then(|| (key(&slot.entry), target))
        })
        .collect()
}

/// Raise each of the valve's ceilings to its cache's floor (`floor_of`): a
/// ceiling under it (a range's `min` a reload gave the cache, a plan floor
/// that rose) holds nothing below it, and a restore steps up from there.
pub(crate) fn floor_ceilings(
    caps: &mut HashMap<Key, Ceiling>,
    floor_of: &dyn Fn(&Key) -> Option<u64>,
) {
    for (key, ceiling) in caps.iter_mut() {
        if let Some(floor) = floor_of(key) {
            ceiling.bytes = ceiling.bytes.max(floor);
        }
    }
}

/// A restore step: raise each of the valve's ceilings by
/// [`RESTORE_PERCENT`] of its cache's grant (`grant_of`), at least 1 MiB,
/// and lift those that reach their grant, or whose cache the plan no longer
/// grants. Returns the keys raised and the keys lifted.
pub(crate) fn restore(
    caps: &mut HashMap<Key, Ceiling>,
    grant_of: &dyn Fn(&Key) -> Option<u64>,
) -> (Vec<Key>, HashSet<Key>) {
    let mut raised = Vec::new();
    let mut lifted = HashSet::new();
    caps.retain(|key, Ceiling { bytes: cap, .. }| {
        let Some(grant) = grant_of(key) else {
            lifted.insert(key.clone());
            return false;
        };
        let step = (grant / 100 * RESTORE_PERCENT).div_ceil(MIB).max(1) * MIB;
        let raised_to = cap.saturating_add(step);
        if raised_to >= grant {
            lifted.insert(key.clone());
            return false;
        }
        *cap = raised_to;
        raised.push(key.clone());
        true
    });
    (raised, lifted)
}

/// Elastic memory (#1314): the step a move takes at most is this much, or
/// [`ELASTIC_STEP_POOL_PERCENT`] of the caches' pool when that is more.
pub(crate) const ELASTIC_STEP_BYTES: u64 = 64 * MIB;

/// See [`ELASTIC_STEP_BYTES`].
pub(crate) const ELASTIC_STEP_POOL_PERCENT: u64 = 1;

/// At most this many moves a tick, each cache in one at most.
pub(crate) const ELASTIC_MAX_MOVES: usize = 4;

/// A move only where the borrower's value per byte is at least this many
/// times the lender's: between half and twice the other's value no memory
/// moves either way, so values that wobble move nothing.
pub(crate) const ELASTIC_VALUE_RATIO: f64 = 2.0;

/// A move takes at most this share of the lender's capacity, and adds at
/// most this share of the borrower's, in percent.
pub(crate) const ELASTIC_MAX_PERCENT: u64 = 10;

/// A cache borrows only with at least this many ghost hits over its
/// window within the depth its value comes from: fewer are noise, whatever
/// their value per byte. It also keeps a cache whose misses are rare but
/// dear (a remote GRIB message at a few ghost hits an hour) from borrowing.
pub(crate) const ELASTIC_MIN_GHOST_HITS: u64 = 10;

/// A cache as elastic memory weighs it (#1314): what a byte more of it is
/// worth, what a byte of it serves, and how far it may move.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Elastic {
    pub key: Key,
    pub capacity: u64,
    /// The least it may lend down to: the plan's floor for it, its range's
    /// `min`, and what it shrinks to in place.
    pub floor: u64,
    /// The most it may borrow up to: the plan's ceiling for it
    /// (`Grant::ceiling_bytes`).
    pub ceiling: u64,
    /// What a byte more is worth, as it borrows: the best of the ring's
    /// depths of the fill time a byte of that much more memory would have
    /// saved an hour on average (the ghost hits within the depth, which are
    /// cumulative, times the miss cost, over all the depth's bytes). The
    /// first slope of the curve's concave hull, so a cliff past half the
    /// capacity counts. Seconds an hour per byte.
    pub value: f64,
    /// The ghost hits within the depth `value` comes from, over the window.
    pub ghost_hits: u64,
    /// That depth, an index of [`ds_cache::GHOST_DEPTHS`].
    pub depth: usize,
    /// It may borrow: its window timed fills, so its miss cost is now's. A
    /// window with ghost hits and no fill is refills failing (an outage) or
    /// fills untimed, not a cache that needs memory.
    pub borrows: bool,
    /// What a byte of it is worth, as it lends: the higher of `value` and
    /// the fill time its hits saved an hour over its capacity, the average
    /// its resident bytes serve. That overstates what its last bytes serve,
    /// so it lends conservatively; a cache nobody reads serves nothing, and
    /// lends first.
    pub lend_value: f64,
    /// It may give memory up: not a cache its engines fill ahead of
    /// requests ([`spared`]), which below that re-downloads in a loop.
    pub lends: bool,
    /// What its expected use is capped at, when the plan counts it at less
    /// than its size (a working-set or demand cache holding less than its
    /// grant): its expected use at a capacity of `c` is `min(c, need)`;
    /// otherwise it is `c`.
    pub need: Option<u64>,
    /// Its planned grant: its fair share, which a cache below it reclaims
    /// at an even ratio.
    pub grant: u64,
    /// What it borrowed less what it lent, from the ledger.
    pub offset: i64,
    /// What it holds now.
    pub used: u64,
    /// Its window's label is idle: under 60 lookups an hour.
    pub idle: bool,
    /// It may decay while idle: it lends, and nothing fills it ahead of
    /// reads (`Churn::prewarmed`: the PVOL pixels, GRIB messages and COG
    /// tiles that keep a quiet deployment warm).
    pub decays: bool,
    /// What it lent the pool (its decays, a lender's share of a valve's
    /// take-back): what it takes back once it is read again
    /// ([`MoveKind::Return`]).
    pub pool_lent: u64,
}

impl Elastic {
    /// Its expected use at a capacity of `bytes`, as the plan counts it.
    pub(crate) fn expected_at(&self, bytes: u64) -> u64 {
        self.need.map_or(bytes, |need| bytes.min(need))
    }
}

/// Why memory moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MoveKind {
    /// A cache whose byte more is worth twice what the lender's serves
    /// borrows it.
    Borrow,
    /// A cache below its grant takes back what a lower-valued borrower
    /// holds, up to its grant, at an even ratio.
    Reclaim,
    /// An idle cache gives the pool what it does not hold.
    Decay,
    /// A cache read again takes back what it lent the pool, at the
    /// restore's pace.
    Return,
}

impl MoveKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            MoveKind::Borrow => "borrow",
            MoveKind::Reclaim => "reclaim",
            MoveKind::Decay => "decay",
            MoveKind::Return => "return",
        }
    }
}

/// Memory elastic sharing moves: `bytes` from `lender` to `borrower`, each
/// a cache or, `None`, the pool's free headroom, with the value per byte of
/// each.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Move {
    pub kind: MoveKind,
    pub borrower: Option<Key>,
    pub lender: Option<Key>,
    pub bytes: u64,
    /// The borrower's value, and the depth it comes from.
    pub borrower_value: f64,
    pub depth: usize,
    /// The lender's value as it lends ([`Elastic::lend_value`]).
    pub lender_value: f64,
}

/// The step of a move over a pool of `pool_bytes`: [`ELASTIC_STEP_BYTES`],
/// or [`ELASTIC_STEP_POOL_PERCENT`] of the pool when that is more, in
/// whole MiB.
pub(crate) fn elastic_step(pool_bytes: u64) -> u64 {
    let share = pool_bytes / 100 * ELASTIC_STEP_POOL_PERCENT;
    ELASTIC_STEP_BYTES.max(share / MIB * MIB)
}

/// The value a byte more of a cache of `capacity` is worth over a window,
/// at the best of its ring's depths: `per_hour` of its cumulative `ghost`
/// hits there, times `cost`, over the depth's bytes. Returns the value and
/// its depth; the first of equal ones, the smaller step.
pub(crate) fn best_depth(
    capacity: u64,
    ghost: [f64; 3],
    cost: f64,
    factors: [f64; 3],
) -> (f64, usize) {
    let mut best = (0.0, 0);
    for depth in 0..3 {
        let value = ghost[depth] * cost / (capacity as f64 * factors[depth]);
        if value > best.0 {
            best = (value, depth);
        }
    }
    best
}

/// What a cache's byte serves as it lends ([`Elastic::lend_value`]): the
/// higher of `value`, a byte more's, and the fill time its `hits_per_hour`
/// saved at `cost` each, over its `capacity`.
pub(crate) fn lend_value(value: f64, hits_per_hour: f64, cost: f64, capacity: u64) -> f64 {
    value.max(hits_per_hour * cost / capacity.max(1) as f64)
}

/// What a tick may do besides weighing (#1314).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rules {
    /// Caches may borrow: the process is under [`UNDER_PERCENT`] of its
    /// total, or the valve has no total to hold it to, and no take-back of
    /// the valve's is waiting out the restore's [`UNDER_FOR`].
    pub may_borrow: bool,
    /// Caches may take back what they lent the pool: the process is under
    /// [`UNDER_PERCENT`] of its total, or the valve has no total.
    pub may_return: bool,
    /// Idle caches decay toward what they hold: only when moves apply.
    pub decays: bool,
}

/// What [`Rules`] a tick gets from the valve's last `sample` (resident
/// bytes and the total) and whether a take-back of the valve's still holds
/// borrowing off (`held`): `(may_borrow, may_return)`.
pub(crate) fn borrowing(sample: Option<(u64, u64)>, held: bool) -> (bool, bool) {
    let under = sample.is_none_or(|(resident, total)| {
        resident.saturating_mul(100) < total.saturating_mul(UNDER_PERCENT)
    });
    (under && !held, under)
}

/// An idle cache decays by at most this share of the step a tick...
pub(crate) const DECAY_STEP_PERCENT: u64 = 50;

/// ... and at most this share of its capacity, in percent: half of what a
/// busy cache grows by.
pub(crate) const DECAY_MAX_PERCENT: u64 = 5;

/// An idle cache decays toward what it holds plus this share, in percent.
pub(crate) const DECAY_HEADROOM_PERCENT: u64 = 10;

/// The moves one tick makes between `caches` over a pool of `pool_bytes`,
/// with `headroom` bytes of the pool the plan's expected use leaves
/// (#1314), in this order:
///
/// 0. **Return**, while `rules` allow it: a cache no longer idle takes back
///    what it lent the pool ([`Elastic::pool_lent`]), [`RESTORE_PERCENT`]
///    of its grant a tick, at least 1 MiB, as the valve's restore climbs,
///    with no ghost gate: its own memory. Within the headroom left.
/// 1. **Reclaim**: a cache below its grant, its fair share, that passes the
///    borrowing gate takes back what a borrower holds above its own grant,
///    the lowest-valued borrower first, while its value is at least that
///    borrower's ([`Elastic::lend_value`]), up to its grant.
/// 2. **Borrow**, while `rules` allow it: the caches whose byte more is
///    worth the most borrow, in that order, from the pool's free headroom
///    first (worth nothing), then from the cache whose byte serves the
///    least, while the borrower's value is at least [`ELASTIC_VALUE_RATIO`]
///    times the lender's.
/// 3. **Decay**, where `rules` say so: an idle cache that decays
///    ([`Elastic::decays`]) and moved nothing this tick gives the pool what
///    it does not hold, down to the
///    larger of its floor and what it holds plus
///    [`DECAY_HEADROOM_PERCENT`], by at most [`DECAY_STEP_PERCENT`] of the
///    step and [`DECAY_MAX_PERCENT`] of its capacity: slower than a busy
///    cache grows.
///
/// A borrower borrows ([`Elastic::borrows`]), with at least
/// [`ELASTIC_MIN_GHOST_HITS`] at its value's depth. A lender lends
/// ([`Elastic::lends`]). A reclaim or a borrow is the step
/// ([`elastic_step`]), at most [`ELASTIC_MAX_PERCENT`] of either cache,
/// within the lender's floor and the borrower's ceiling, in whole MiB, at
/// least one, and raises the expected use the plan fitted to the pool by no
/// more than the headroom left: it shrinks to what fits, or goes. At most
/// [`ELASTIC_MAX_MOVES`] reclaims and borrows, each cache in one move at
/// most.
pub(crate) fn elastic(
    caches: &[Elastic],
    pool_bytes: u64,
    headroom: u64,
    rules: Rules,
) -> Vec<Move> {
    let step = elastic_step(pool_bytes);
    let share = |c: &Elastic| c.capacity / 100 * ELASTIC_MAX_PERCENT;
    let gated = |c: &&Elastic| c.borrows && c.ghost_hits >= ELASTIC_MIN_GHOST_HITS && c.value > 0.0;
    let mut headroom = headroom;
    let mut moved: HashSet<&Key> = HashSet::new();
    let mut moves = Vec::new();
    // The expected use a move of `bytes` from `lender` to `borrower` adds.
    let raises = |borrower: &Elastic, lender: Option<&Elastic>, bytes: u64| {
        let up = borrower.expected_at(borrower.capacity + bytes)
            - borrower.expected_at(borrower.capacity);
        let down = lender.map_or(0, |l| {
            l.expected_at(l.capacity) - l.expected_at(l.capacity - bytes)
        });
        up.saturating_sub(down)
    };
    // The most whole MiB up to `most` whose move fits the headroom.
    let fits = |borrower: &Elastic, lender: Option<&Elastic>, most: u64, headroom: u64| {
        (1..=most / MIB)
            .rev()
            .map(|mib| mib * MIB)
            .find(|&bytes| raises(borrower, lender, bytes) <= headroom)
    };

    // 0. Return.
    if rules.may_return {
        for cache in caches.iter().filter(|c| !c.idle && c.pool_lent > 0) {
            let pace = (cache.grant / 100 * RESTORE_PERCENT).div_ceil(MIB).max(1) * MIB;
            let Some(bytes) = fits(cache, None, cache.pool_lent.min(pace), headroom) else {
                continue;
            };
            headroom -= raises(cache, None, bytes);
            moved.insert(&cache.key);
            moves.push(Move {
                kind: MoveKind::Return,
                borrower: Some(cache.key.clone()),
                lender: None,
                bytes,
                borrower_value: cache.value,
                depth: cache.depth,
                lender_value: 0.0,
            });
        }
    }

    // Returns are restores: the moves a tick makes are counted after them.
    let returns = moves.len();

    // 1. Reclaim.
    let mut reclaimers: Vec<&Elastic> = caches
        .iter()
        .filter(gated)
        .filter(|c| c.capacity < c.grant)
        .collect();
    reclaimers.sort_by(|a, b| b.value.total_cmp(&a.value).then_with(|| a.key.cmp(&b.key)));
    let mut donors: Vec<&Elastic> = caches
        .iter()
        .filter(|c| c.offset > 0 && c.capacity > c.floor)
        .collect();
    donors.sort_by(|a, b| {
        a.lend_value
            .total_cmp(&b.lend_value)
            .then_with(|| a.key.cmp(&b.key))
    });
    for reclaimer in reclaimers {
        if moves.len() - returns == ELASTIC_MAX_MOVES {
            break;
        }
        let room = step
            .min(share(reclaimer))
            .min(reclaimer.grant - reclaimer.capacity);
        for donor in &donors {
            if moved.contains(&donor.key) || donor.key == reclaimer.key {
                continue;
            }
            if reclaimer.value < donor.lend_value {
                break;
            }
            let most = room
                .min(share(donor))
                .min(donor.offset as u64)
                .min(donor.capacity - donor.floor);
            let Some(bytes) = fits(reclaimer, Some(donor), most, headroom) else {
                continue;
            };
            headroom -= raises(reclaimer, Some(donor), bytes);
            moved.insert(&reclaimer.key);
            moved.insert(&donor.key);
            moves.push(Move {
                kind: MoveKind::Reclaim,
                borrower: Some(reclaimer.key.clone()),
                lender: Some(donor.key.clone()),
                bytes,
                borrower_value: reclaimer.value,
                depth: reclaimer.depth,
                lender_value: donor.lend_value,
            });
            break;
        }
    }

    // 2. Borrow.
    if rules.may_borrow {
        let mut borrowers: Vec<&Elastic> = caches
            .iter()
            .filter(gated)
            .filter(|c| c.ceiling > c.capacity)
            .collect();
        borrowers.sort_by(|a, b| b.value.total_cmp(&a.value).then_with(|| a.key.cmp(&b.key)));
        let mut lenders: Vec<&Elastic> = caches
            .iter()
            .filter(|c| c.lends && c.capacity > c.floor)
            .collect();
        lenders.sort_by(|a, b| {
            a.lend_value
                .total_cmp(&b.lend_value)
                .then_with(|| a.key.cmp(&b.key))
        });
        for borrower in borrowers {
            if moves.len() - returns == ELASTIC_MAX_MOVES {
                break;
            }
            if moved.contains(&borrower.key) {
                continue;
            }
            let room = step
                .min(share(borrower))
                .min(borrower.ceiling - borrower.capacity);
            // The pool first: its headroom is worth nothing where it is.
            if let Some(bytes) = fits(borrower, None, room.min(headroom), headroom) {
                headroom -= raises(borrower, None, bytes);
                moved.insert(&borrower.key);
                moves.push(Move {
                    kind: MoveKind::Borrow,
                    borrower: Some(borrower.key.clone()),
                    lender: None,
                    bytes,
                    borrower_value: borrower.value,
                    depth: borrower.depth,
                    lender_value: 0.0,
                });
                continue;
            }
            // The lowest value first: the first that is not worth enough
            // ends it.
            for lender in &lenders {
                if moved.contains(&lender.key) || lender.key == borrower.key {
                    continue;
                }
                if borrower.value < ELASTIC_VALUE_RATIO * lender.lend_value {
                    break;
                }
                let most = room.min(share(lender)).min(lender.capacity - lender.floor);
                let Some(bytes) = fits(borrower, Some(lender), most, headroom) else {
                    continue;
                };
                headroom -= raises(borrower, Some(lender), bytes);
                moved.insert(&borrower.key);
                moved.insert(&lender.key);
                moves.push(Move {
                    kind: MoveKind::Borrow,
                    borrower: Some(borrower.key.clone()),
                    lender: Some(lender.key.clone()),
                    bytes,
                    borrower_value: borrower.value,
                    depth: borrower.depth,
                    lender_value: lender.lend_value,
                });
                break;
            }
        }
    }

    // 3. Decay.
    if rules.decays {
        for cache in caches.iter().filter(|c| c.idle && c.decays) {
            if moved.contains(&cache.key) {
                continue;
            }
            let held = cache
                .used
                .saturating_add(cache.used / 100 * DECAY_HEADROOM_PERCENT);
            let target = cache.floor.max(held.div_ceil(MIB) * MIB);
            let most = (step / 100 * DECAY_STEP_PERCENT)
                .min(cache.capacity / 100 * DECAY_MAX_PERCENT)
                .min(cache.capacity.saturating_sub(target));
            let bytes = most / MIB * MIB;
            if bytes == 0 {
                continue;
            }
            moves.push(Move {
                kind: MoveKind::Decay,
                borrower: None,
                lender: Some(cache.key.clone()),
                bytes,
                borrower_value: 0.0,
                depth: 0,
                lender_value: cache.lend_value,
            });
        }
    }
    moves
}

/// The size a cache whose grant is `grant` is held to with an elastic
/// `offset`: the grant plus the offset, within `floor` and `ceiling`.
pub(crate) fn elastic_target(grant: u64, offset: i64, floor: u64, ceiling: u64) -> u64 {
    let target = (grant as i128 + offset as i128).clamp(0, u64::MAX as i128) as u64;
    target.clamp(floor.min(ceiling), ceiling.max(floor))
}
