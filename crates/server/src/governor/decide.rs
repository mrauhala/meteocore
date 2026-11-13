//! The memory governor's decisions (#1315), pure: inputs in, actions out,
//! no clock, no cache, no server. `super` reads the inputs and applies the
//! actions.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use ds_cache::plan::grant::{class, Class};
use ds_cache::plan::{Entry, Kind, Row, Scope};
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
/// - one the valve lowered (`cap`): its grant up to that ceiling, however
///   little it moves, since a restore lifts the ceiling by small steps;
/// - one whose ceiling the tick just lifted (`lifted`): its grant;
/// - else its grant, when it moves far enough ([`moves_enough`]);
/// - never the move the tick tried last that stopped short (`refused`, the
///   capacity it stopped at and the grant it was given), while capacity and
///   grant are what they were then: it is logged once, not every tick.
pub(crate) fn tick_target(
    capacity: u64,
    grant: u64,
    cap: Option<u64>,
    lifted: bool,
    refused: Option<(u64, u64)>,
) -> Option<u64> {
    if refused == Some((capacity, grant)) {
        return None;
    }
    match cap {
        Some(cap) => Some(grant.min(cap)),
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

/// A restore step: raise each of the valve's ceilings by
/// [`RESTORE_PERCENT`] of its cache's grant (`grant_of`), at least 1 MiB,
/// and lift those that reach their grant, or whose cache the plan no longer
/// grants. Returns the keys raised and the keys lifted.
pub(crate) fn restore(
    caps: &mut HashMap<Key, u64>,
    grant_of: &dyn Fn(&Key) -> Option<u64>,
) -> (Vec<Key>, HashSet<Key>) {
    let mut raised = Vec::new();
    let mut lifted = HashSet::new();
    caps.retain(|key, cap| {
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
