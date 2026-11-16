//! The memory governor (#1315): one task on `poll_runtime` that keeps the
//! memory plan fitted between reloads and holds the process to its total.
//!
//! - **Re-plan tick**, every [`decide::REPLAN_EVERY`]: fit the plan again
//!   with the engines' current demands (`memory_plan::fit`) and apply a
//!   grant only where it moves a capacity by more than 10 % and 64 MiB
//!   ([`decide::tick_target`]). The plan's `Least` holds a warm cache to a
//!   quarter of what it holds per tick and never plans a shrink below what
//!   the cache takes in place; a resize that stops short anyway is not
//!   tried again while its capacity, grant, in-place least and ceiling
//!   stay as they were. Changes are logged at INFO, the plan's WARN block
//!   only when what it cuts changes (`memory_plan::log_new_warnings`).
//! - **Pressure valve**, sampled every [`decide::SAMPLE_EVERY`]:
//!   `jemalloc_resident_bytes` above the total for a minute lowers every
//!   flexible cache to 90 % of what it holds, never below the plan's floor
//!   for it nor what it takes in place, and keeps what it reached as its
//!   ceiling; every arena's dirty pages are purged, and a WARN says so (at
//!   INFO when it lowers the same caches as the last relief). A cache that
//!   refuses its shrink keeps no ceiling and is named. With nothing it can
//!   lower it counts nothing, purges at most every 10 minutes and WARNs at
//!   most every 30, or when other caches refuse. Under 90 % of the total for 10 minutes, the ceilings
//!   rise by 5 % of the grant every 5 minutes, on their own clock, until
//!   the plan has the caches again. A ceiling never takes a cache below
//!   the plan's floor or its range's `min`. A cache its engines fill ahead
//!   of requests (satellite scans, PVOL pixels, station grids) is never
//!   lowered, nor a budget.
//! - **Elastic memory** (#1314), at each re-plan tick, after it has set its
//!   sizes: weigh what a byte more is worth in each cache the plan sizes,
//!   and what a byte of it serves ([`elastic_inputs`]), and move memory to
//!   the caches where more is worth most from those where it serves least
//!   ([`decide::elastic`]), within the pool's expected use. Report only
//!   until its switch exists ([`ELASTIC_APPLIES`]): logged when the moves
//!   change, shown on `/metrics` and in `/admin/cache-plan`. Applied, the
//!   ledger keeps each cache's offset, which the tick and a reload's plan
//!   add to its grant. Nothing is proposed while the valve holds.
//!
//! Gates: everything runs where `AcceptedLoad::plans_sizes` holds, `auto`
//! with a total; the valve only where that total is `[memory] total_mb` or
//! a cgroup limit's share (`memory_plan::valve_total`), never a share of a
//! shared host's memory. A pin (`[memory.caches]`, a variable, a collection
//! key) is never moved; a range moves within its bounds, which the plan's
//! grants and floors respect. Every resize goes through
//! `ds_cache::plan::apply`, so it is the plan's: report mode gives it back.
//!
//! Each step reads the gate, then takes `reload_lock` with `try_lock` and
//! skips when a reload holds it, as `radar_recovery_loop` does: it never
//! runs between a reload's staged `[memory.caches]` and their `set`, nor
//! while a reload applies its plan. An accepted reload fits and applies
//! the plan itself, keeping the valve's ceilings (`capped`) but those it
//! supersedes ([`reconcile`]), restarts the tick's 5 minutes and forgets
//! the resizes that stopped short ([`fitted`]); a recovery pass does
//! neither. The pressure timers and ceilings carry over, since the
//! process's memory does not change with its config. Where the gates
//! close (report mode, no total, a total from the host), the valve's state
//! is dropped: report mode gives every size the plan set back, and with
//! the plan still applying, the next tick gives the lowered caches their
//! grant.

pub(crate) mod decide;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

use ds_cache::plan::grant::{self, Grant, Reason};
use ds_cache::plan::{Entry, Row};
use prometheus::{IntCounterVec, IntGaugeVec, Opts};
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::admin::AdminState;
use crate::cache_plan::{self, mib};
use crate::memory_plan::{self, Change, Held};
use decide::{Ceiling, Elastic, Key, Ledger, Lent, Move, Pressure, Refused, Signal, Slot};

/// What the governor did at a time, for `/admin/cache-plan`.
#[derive(Debug, Clone)]
struct Record {
    at: chrono::DateTime<chrono::Utc>,
    changes: Vec<Change>,
}

/// What elastic memory weighed at a tick and the moves it made or would
/// make (#1314), for `/admin/cache-plan`.
#[derive(Debug, Clone)]
struct Proposal {
    at: chrono::DateTime<chrono::Utc>,
    /// Why it weighed nothing: the pressure valve holds.
    held: Option<&'static str>,
    /// Its moves were applied ([`elastic_applies`]); else only proposed.
    applied: bool,
    step_bytes: u64,
    /// What the plan's expected use left of the pool when it weighed.
    headroom_bytes: u64,
    caches: Vec<Elastic>,
    moves: Vec<Move>,
}

impl Proposal {
    fn mode(&self) -> &'static str {
        if self.applied {
            "applied"
        } else {
            "report"
        }
    }
}

/// Why elastic memory proposes nothing while the pressure valve holds.
const VALVE_HOLDS: &str = "the memory pressure valve holds";

/// Whether elastic memory applies its moves (#1314): false, so production
/// only reports them, until the switch that sets it exists. The tests run
/// the whole mechanism through [`run_elastic`], which takes it as an
/// argument.
static ELASTIC_APPLIES: AtomicBool = AtomicBool::new(false);

fn elastic_applies() -> bool {
    ELASTIC_APPLIES.load(Ordering::Relaxed)
}

/// The governor's state between steps.
#[derive(Debug, Default)]
struct Governor {
    /// When the plan was last fitted, by a tick, at boot or by a reload;
    /// not by a recovery pass, which refits the accepted config.
    fitted_at: Option<Instant>,
    pressure: Pressure,
    /// The valve's ceilings, by cache.
    caps: HashMap<Key, Ceiling>,
    /// Ceilings dropped without a restore, the valve's total gone while
    /// the plan still applies: the next tick gives these caches their
    /// grant whatever it moves.
    released: HashSet<Key>,
    /// When a tick last raised the ceilings: restores are on their own
    /// clock, which no other fit restarts.
    restored_at: Option<Instant>,
    /// The resizes that stopped short at the last tick.
    refused: HashMap<Key, Refused>,
    /// What the last relief of this spell of pressure lowered: a relief
    /// that lowers other caches WARNs, one that lowers the same again logs
    /// at INFO.
    lowered: Option<BTreeSet<Key>>,
    /// When the valve last purged.
    purged_at: Option<Instant>,
    /// When the valve, over the total with nothing it could lower, last
    /// WARNed, and the caches that refused their shrink then.
    stuck_warned_at: Option<Instant>,
    stuck: BTreeSet<Key>,
    /// The last sample: resident bytes and the valve's total.
    sample: Option<(u64, u64)>,
    last_tick: Option<Record>,
    last_relief: Option<Record>,
    last_restore: Option<Record>,
    /// Elastic memory's last proposal (#1314), and the moves it last
    /// proposed: the same moves again log and count nothing.
    elastic: Option<Proposal>,
    elastic_logged: Option<Vec<(Key, Key)>>,
    /// The elastic offsets the plan's grants take (#1314): empty while
    /// moves are only reported.
    ledger: Ledger,
}

static GOVERNOR: LazyLock<Mutex<Governor>> = LazyLock::new(Default::default);

fn governor() -> std::sync::MutexGuard<'static, Governor> {
    GOVERNOR.lock().unwrap_or_else(|e| e.into_inner())
}

/// The least a valve's ceiling takes `entry` to under `grant`: the plan's
/// floor for it and its range's `min`.
fn floor_of(entry: &Entry, grant: &Grant) -> u64 {
    grant
        .floor_bytes
        .max(entry.source.bounds().map_or(0, |b| b.min_bytes))
}

/// `grant` under the valve's ceiling on `entry`, if it holds one, never
/// below the plan's floor nor a range's `min`: what a reload's plan gives
/// a cache the valve lowered. A ceiling under that floor is raised to it,
/// so a restore steps up from there; one set under another source (a
/// reload pinned the cache, or gave it other bounds) is dropped.
pub(crate) fn capped(entry: &Entry, grant: &Grant) -> u64 {
    let floor = floor_of(entry, grant);
    let key = decide::key(entry);
    let mut governor = governor();
    // With its elastic offset (#1314), which a reload keeps.
    let target = elastic_grant(&governor.ledger, &key, entry, grant);
    let Some(ceiling) = governor.caps.get_mut(&key) else {
        return target;
    };
    if !ceiling.holds(&entry.source) {
        governor.caps.remove(&key);
        return target;
    }
    ceiling.bytes = ceiling.bytes.max(floor);
    decide::ceiled(target, ceiling.bytes, floor)
}

/// `grant` with `key`'s elastic offset in `ledger` (#1314), within the
/// plan's floor and ceiling for it: the size the re-plan tick and a
/// reload's plan hold the cache to.
fn elastic_grant(ledger: &Ledger, key: &Key, entry: &Entry, grant: &Grant) -> u64 {
    ledger.get(key).map_or(grant.bytes, |lent| {
        decide::elastic_target(
            grant.bytes,
            lent.offset,
            floor_of(entry, grant),
            grant.ceiling_bytes,
        )
    })
}

/// Drop the elastic offsets `rows` supersede (#1314): a cache now sized
/// from another source or configured size than when it moved (a reload
/// pinned, re-bounded or resized it), or gone.
fn reconcile_ledger(ledger: &mut Ledger, rows: &[Row]) {
    let rows: HashMap<Key, &Row> = rows
        .iter()
        .map(|row| (decide::key(&row.entry), row))
        .collect();
    ledger.retain(|key, lent| {
        rows.get(key).is_some_and(|row| {
            row.entry.source == lent.source && row.configured_bytes == lent.configured
        })
    });
}

/// Hold each elastic offset in `ledger` to its cache's floor and ceiling
/// under `plan` (#1314), dropping the ones that come to nothing.
fn bound_ledger(ledger: &mut Ledger, rows: &[Row], plan: &grant::Plan) {
    for row in rows {
        let key = decide::key(&row.entry);
        let (Some(lent), Some(grant)) = (ledger.get_mut(&key), plan.grant(&row.entry)) else {
            continue;
        };
        let target = decide::elastic_target(
            grant.bytes,
            lent.offset,
            floor_of(&row.entry, grant),
            grant.ceiling_bytes,
        );
        lent.offset = target as i64 - grant.bytes as i64;
    }
    ledger.retain(|_, lent| lent.offset != 0);
}

/// Drop the elastic offsets of the caches of `collections`, which a reload
/// rebuilt or removed (#1314): a new cache starts from its plan's grant.
pub(crate) fn forget_elastic(collections: &[&str]) {
    forget_collections(&mut governor().ledger, collections);
}

/// [`forget_elastic`] over `ledger`.
fn forget_collections(ledger: &mut Ledger, collections: &[&str]) {
    ledger.retain(|(_, _, scope), _| {
        scope
            .collection()
            .is_none_or(|collection| !collections.contains(&collection))
    });
}

/// Drop the ceilings `rows` supersede: a cache whose size now comes from
/// another source than the one its ceiling was set under, or that is gone.
/// Before a reload's plan is applied, which `capped` then leaves alone.
pub(crate) fn supersede(rows: &[Row]) {
    let sources: HashMap<Key, &ds_cache::plan::SizeSource> = rows
        .iter()
        .map(|row| (decide::key(&row.entry), &row.entry.source))
        .collect();
    governor()
        .caps
        .retain(|key, ceiling| sources.get(key).is_some_and(|s| ceiling.holds(s)));
}

/// Before a plan from boot, a reload or a recovery pass is applied: drop
/// the ceilings it supersedes ([`supersede`]) and, where the accepted
/// config gives the valve no total, every ceiling, released for the next
/// tick to give those caches their grant. The plan applies no stale
/// ceiling.
pub(crate) fn reconcile(state: &AdminState, rows: &[Row]) {
    supersede(rows);
    reconcile_ledger(&mut governor().ledger, rows);
    let valve = gate(state).and_then(|accepted| {
        memory_plan::valve_total(accepted.memory_mode(), accepted.memory_total_mb())
    });
    if valve.is_none() {
        release(&mut governor());
    }
}

/// Drop every ceiling, marking its cache for the next tick to give its
/// grant whatever that moves: the valve's total is gone while the plan
/// still applies its sizes.
fn release(governor: &mut Governor) {
    let released: Vec<Key> = governor.caps.drain().map(|(key, _)| key).collect();
    governor.released.extend(released);
}

/// The plan was fitted and applied at `now`, at boot or by a reload: the
/// next tick is 5 minutes later, and the resizes that stopped short are
/// tried again. Not a recovery pass: it refits the accepted config, and
/// calling this from it would keep putting the tick off while a source is
/// down.
pub(crate) fn fitted(now: Instant) {
    let mut governor = governor();
    governor.fitted_at = Some(now);
    governor.refused.clear();
}

/// What one step did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// A reload holds `reload_lock`.
    Skipped,
    /// The plan does not apply its sizes: report mode, or no total.
    Gated,
    Ran {
        /// The valve sampled resident memory.
        sampled: bool,
        /// The valve lowered caches.
        relieved: bool,
        /// The plan was fitted again.
        replanned: bool,
    },
}

/// The governor's task: a step every [`decide::SAMPLE_EVERY`] until the
/// server shuts down. On `poll_runtime`: a step reads and resizes caches,
/// blocking work.
pub(crate) async fn run(state: AdminState) {
    init_counters();
    while state.recovery_shutdown.sleep(decide::SAMPLE_EVERY).await {
        step(&state, Instant::now());
    }
}

/// Whether the plan applies its sizes for the accepted config.
fn gate(state: &AdminState) -> Option<std::sync::Arc<crate::admin::AcceptedLoad>> {
    state
        .accepted_load
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .filter(|accepted| accepted.plans_sizes())
}

/// One step at `now`. The gate is read first, without `reload_lock`: in
/// report mode a step takes no lock a reload could find taken. Then it is
/// skipped while a reload holds the lock.
pub(crate) fn step(state: &AdminState, now: Instant) -> Step {
    if gate(state).is_none() {
        let mut governor = governor();
        let records = (
            governor.last_tick.take(),
            governor.last_relief.take(),
            governor.last_restore.take(),
        );
        *governor = Governor::default();
        (
            governor.last_tick,
            governor.last_relief,
            governor.last_restore,
        ) = records;
        return Step::Gated;
    }
    let Ok(_guard) = state.reload_lock.try_lock() else {
        return Step::Skipped;
    };
    // Again under the lock: a reload may have closed the gate meanwhile.
    let Some(accepted) = gate(state) else {
        return Step::Gated;
    };
    let valve = memory_plan::valve_total(accepted.memory_mode(), accepted.memory_total_mb());
    let resident = valve.and_then(|_| resident_bytes());
    let mut relieved = false;
    match (valve, resident) {
        (Some(total), Some(resident)) => {
            let signal = {
                let mut governor = governor();
                governor.sample = Some((resident, total));
                let signal = governor.pressure.observe(now, resident, total);
                if governor.pressure.over_since.is_none() {
                    // The spell of pressure is over: the next one's first
                    // relief WARNs, and so does its first stay at the floors.
                    governor.lowered = None;
                    governor.stuck_warned_at = None;
                    governor.stuck.clear();
                }
                signal
            };
            if signal == Signal::Relieve {
                relieved = relieve(state, resident, total, now);
            }
        }
        _ => {
            // No valve: its ceilings go, and the next tick gives those
            // caches their grant at once rather than by the thresholds.
            let mut governor = governor();
            release(&mut governor);
            governor.sample = None;
            governor.pressure = Pressure::default();
        }
    }
    let due = {
        let governor = governor();
        let since = |at: Option<Instant>| {
            at.is_none_or(|at| now.saturating_duration_since(at) >= decide::REPLAN_EVERY)
        };
        since(governor.fitted_at) || restore_due(&governor, now)
    };
    if due {
        tick(state, now);
    }
    Step::Ran {
        sampled: resident.is_some(),
        relieved,
        replanned: due,
    }
}

/// Whether the valve's ceilings may be raised at `now`: there are some, and
/// the process has been under 90 % of its total for 10 minutes. On their
/// own clock ([`Governor::restored_at`]): no fit puts a restore off.
fn restore_due(governor: &Governor, now: Instant) -> bool {
    !governor.caps.is_empty()
        && governor.pressure.may_restore(now)
        && governor
            .restored_at
            .is_none_or(|at| now.saturating_duration_since(at) >= decide::REPLAN_EVERY)
}

/// jemalloc's resident bytes, where the server runs on jemalloc.
fn resident_bytes() -> Option<u64> {
    #[cfg(not(target_env = "msvc"))]
    {
        crate::jemalloc::resident_bytes()
    }
    #[cfg(target_env = "msvc")]
    {
        None
    }
}

/// Purge every arena's dirty pages, where the server runs on jemalloc.
fn purge() -> bool {
    #[cfg(not(target_env = "msvc"))]
    {
        crate::jemalloc::purge()
    }
    #[cfg(target_env = "msvc")]
    {
        false
    }
}

/// The re-plan tick at `now`: fit the plan again, step the valve's
/// ceilings up when the process has been under 90 % of its total for 10
/// minutes, and apply the grants that move far enough (module docs).
fn tick(state: &AdminState, now: Instant) {
    let memory = crate::admin::refresh_metrics(state);
    let rows = cache_plan::snapshot(state);
    let report = memory_plan::fit(state, &rows, &memory);
    // The plan's grants, but for what no collection can reach.
    let grants: HashMap<Key, u64> = report
        .plan
        .iter()
        .flat_map(|plan| plan.caches.iter().chain(&plan.budgets))
        .filter(|g| !matches!(g.reason, Reason::Unreachable(_)))
        .map(|g| (decide::key(&g.entry), g.bytes))
        .collect();
    let grant_of = |key: &Key| grants.get(key).copied();
    let floors: HashMap<Key, u64> = report
        .plan
        .iter()
        .flat_map(|plan| {
            rows.iter().filter_map(move |row| {
                let grant = plan.grant(&row.entry)?;
                Some((decide::key(&row.entry), floor_of(&row.entry, grant)))
            })
        })
        .collect();
    supersede(&rows);
    // Copied out: applying calls back into `capped`.
    let (caps, refused, raised, restored, lifted, valve_holds, ledger) = {
        let mut governor = governor();
        reconcile_ledger(&mut governor.ledger, &rows);
        if let Some(plan) = &report.plan {
            bound_ledger(&mut governor.ledger, &rows, plan);
        }
        // Before the restore: a ceiling it lifts now still held this tick.
        let valve_holds = governor.pressure.over_since.is_some() || !governor.caps.is_empty();
        decide::floor_ceilings(&mut governor.caps, &|key| floors.get(key).copied());
        let (raised, restored) = if restore_due(&governor, now) {
            governor.restored_at = Some(now);
            decide::restore(&mut governor.caps, &grant_of)
        } else {
            (Vec::new(), HashSet::new())
        };
        let mut lifted = std::mem::take(&mut governor.released);
        lifted.extend(restored.iter().cloned());
        let caps: HashMap<Key, u64> = governor
            .caps
            .iter()
            .map(|(key, ceiling)| (key.clone(), ceiling.bytes))
            .collect();
        (
            caps,
            governor.refused.clone(),
            raised,
            restored,
            lifted,
            valve_holds,
            governor.ledger.clone(),
        )
    };
    // The grant a cache is held to: its elastic offset added (#1314).
    let target = |row: &Row, grant: &Grant| {
        elastic_grant(&ledger, &decide::key(&row.entry), &row.entry, grant)
    };
    let attempt = |row: &Row, grant: &Grant, key: &Key| Refused {
        capacity: row.usage.capacity_bytes,
        grant: target(row, grant),
        least: row.least_bytes,
        capped: caps.contains_key(key),
    };
    let wanted = |row: &Row, grant: &Grant| {
        let key = decide::key(&row.entry);
        decide::tick_target(
            row.usage.capacity_bytes,
            target(row, grant),
            floor_of(&row.entry, grant),
            caps.get(&key).copied(),
            lifted.contains(&key),
            refused.get(&key) == Some(&attempt(row, grant, &key)),
        )
    };
    let report = memory_plan::apply_tick(report, &rows, &wanted);
    let changed: Vec<Change> = report
        .changes
        .iter()
        .filter(|c| c.to_bytes != c.from_bytes || c.cleared)
        .cloned()
        .collect();
    {
        let mut governor = governor();
        governor.fitted_at = Some(now);
        for change in &report.changes {
            let key = decide::key(&change.entry);
            let tried = rows.iter().find(|row| decide::key(&row.entry) == key);
            let grant = report.plan.as_ref().and_then(|p| p.grant(&change.entry));
            match (change.held, tried, grant) {
                (Some(Held::Refused), Some(row), Some(grant)) => {
                    let refused = Refused {
                        capacity: change.to_bytes,
                        ..attempt(row, grant, &key)
                    };
                    governor.refused.insert(key, refused);
                }
                _ => {
                    governor.refused.remove(&key);
                }
            }
        }
        governor.last_tick = Some(Record {
            at: chrono::Utc::now(),
            changes: changed.clone(),
        });
        let restore: Vec<Change> = changed
            .iter()
            .filter(|c| {
                let key = decide::key(&c.entry);
                raised.contains(&key) || restored.contains(&key)
            })
            .cloned()
            .collect();
        if !restore.is_empty() {
            PRESSURE_ACTIONS.with_label_values(&["restore"]).inc();
            governor.last_restore = Some(Record {
                at: chrono::Utc::now(),
                changes: restore,
            });
        }
    }
    if let Some(block) = tick_block(&changed, raised.len() + restored.len(), restored.len()) {
        info!("{block}");
    }
    // What the tick left: elastic memory weighs the capacities it set.
    let rows = cache_plan::snapshot(state);
    if let Some(plan) = &report.plan {
        let applies = elastic_applies();
        let mut ledger = std::mem::take(&mut governor().ledger);
        let proposal = run_elastic(
            &rows,
            plan,
            &mut ledger,
            valve_holds,
            applies,
            &ds_cache::plan::apply,
        );
        governor().ledger = ledger;
        record_elastic(proposal);
    }
    memory_plan::log_new_warnings(&report, &rows);
}

/// The caches elastic memory weighs (#1314), from `rows` and the plan
/// fitted to them: every cache the plan sizes (not pinned, a collection can
/// reach it, enabled) with a ghost ring and a timed fill.
///
/// - As a borrower, a byte more at the best of the ring's depths
///   ([`decide::best_depth`]), from its window's ghost hits and miss cost.
///   Only a window that timed fills borrows: one with ghost hits and none
///   is refills failing or fills untimed.
/// - As a lender, the higher of that and what a byte of it serves: its
///   window's hits times its miss cost over its capacity. Without a fill in
///   the window, its mean fill since it was built prices it.
///
/// A cache without a ring, or that never timed a fill, neither borrows nor
/// lends. Ghost hits by depth are cumulative and never summed.
pub(crate) fn elastic_inputs(rows: &[Row], plan: &grant::Plan) -> Vec<Elastic> {
    rows.iter()
        .filter_map(|row| {
            let entry = &row.entry;
            let capacity = row.usage.capacity_bytes;
            if entry.kind != ds_cache::plan::Kind::Cache || capacity == 0 {
                return None;
            }
            if grant::pinned(&entry.source) {
                return None;
            }
            let grant = plan
                .grant(entry)
                .filter(|g| !matches!(g.reason, Reason::Unreachable(_)))?;
            let window = row.window.as_ref()?;
            let ghost = window.churn.ghost_hits?;
            let timed = window.miss_cost_seconds();
            let cost = timed.or_else(|| row.usage.churn.as_ref()?.mean_fill_seconds())?;
            let per_hour = ghost.map(|hits| window.per_hour(hits));
            let (value, depth) = decide::best_depth(
                capacity,
                per_hour,
                cost,
                ds_cache::plan::GHOST_DEPTH_FACTORS,
            );
            Some(Elastic {
                key: decide::key(entry),
                capacity,
                floor: floor_of(entry, grant).max(row.least_bytes),
                ceiling: grant.ceiling_bytes,
                value,
                ghost_hits: ghost[depth],
                depth,
                borrows: timed.is_some(),
                lend_value: decide::lend_value(value, window.per_hour(window.hits), cost, capacity),
                lends: !decide::spared(row),
                need: (grant.expected_bytes < grant.bytes).then_some(grant.expected_bytes),
            })
        })
        .collect()
}

/// What the plan's expected use leaves of its pool with the offsets of
/// `ledger` (#1314): each cache's expected use at its grant plus offset,
/// rather than at its grant, the others' as the plan counts them.
fn elastic_headroom(rows: &[Row], plan: &grant::Plan, ledger: &Ledger) -> u64 {
    let mut expected = plan.expected_bytes;
    for row in rows {
        let key = decide::key(&row.entry);
        let (Some(_), Some(grant)) = (ledger.get(&key), plan.grant(&row.entry)) else {
            continue;
        };
        let at = |bytes: u64| {
            if grant.expected_bytes < grant.bytes {
                bytes.min(grant.expected_bytes)
            } else {
                bytes
            }
        };
        let target = elastic_grant(ledger, &key, &row.entry, grant);
        expected = (expected + at(target)).saturating_sub(at(grant.bytes));
    }
    plan.pool_bytes.saturating_sub(expected)
}

/// One elastic step over `rows`, the capacities the re-plan tick left, and
/// the `plan` it fitted (#1314): weigh the caches ([`elastic_inputs`]) and
/// decide the moves ([`decide::elastic`]); nothing while the pressure valve
/// holds (`valve_holds`: over the total, or a ceiling of its still in
/// force), which comes first. Where it `applies`, each move resizes the
/// lender, then the borrower by what the lender gave, through `resize`
/// (`ds_cache::plan::apply`), and `ledger` keeps the offsets, which the
/// next ticks and reloads add to the grants; else nothing is resized and
/// the ledger stays as it is.
fn run_elastic(
    rows: &[Row],
    plan: &grant::Plan,
    ledger: &mut Ledger,
    valve_holds: bool,
    applies: bool,
    resize: &dyn Fn(&Entry, u64) -> Option<u64>,
) -> Proposal {
    let step_bytes = decide::elastic_step(plan.pool_bytes);
    let headroom_bytes = elastic_headroom(rows, plan, ledger);
    let (held, caches, mut moves) = if valve_holds {
        (Some(VALVE_HOLDS), Vec::new(), Vec::new())
    } else {
        let caches = elastic_inputs(rows, plan);
        let moves = decide::elastic(&caches, plan.pool_bytes, headroom_bytes);
        (None, caches, moves)
    };
    if applies {
        moves.retain_mut(|m| apply_move(m, rows, ledger, resize));
    }
    Proposal {
        at: chrono::Utc::now(),
        held,
        applied: applies,
        step_bytes,
        headroom_bytes,
        caches,
        moves,
    }
}

/// Apply `m` through `resize`: shrink the lender, grow the borrower by what
/// it gave, and keep both offsets in `ledger`, each with the size source
/// and configured size its cache moved under. `m.bytes` becomes what moved;
/// false when nothing did. Every size it changes counts in
/// `cache_plan_changes_total`.
fn apply_move(
    m: &mut Move,
    rows: &[Row],
    ledger: &mut Ledger,
    resize: &dyn Fn(&Entry, u64) -> Option<u64>,
) -> bool {
    let find = |key: &Key| rows.iter().find(|row| decide::key(&row.entry) == *key);
    let (Some(lender), Some(borrower)) = (find(&m.lender), find(&m.borrower)) else {
        return false;
    };
    let from = lender.usage.capacity_bytes;
    let to = resize(&lender.entry, from.saturating_sub(m.bytes)).unwrap_or(from);
    let given = from.saturating_sub(to);
    if given == 0 {
        return false;
    }
    let had = borrower.usage.capacity_bytes;
    let got = resize(&borrower.entry, had + given).unwrap_or(had);
    let gained = got.saturating_sub(had);
    for (row, delta) in [(lender, -(given as i64)), (borrower, gained as i64)] {
        if delta == 0 {
            continue;
        }
        memory_plan::count_change(row.entry.id);
        let lent = ledger.entry(decide::key(&row.entry)).or_insert(Lent {
            offset: 0,
            source: row.entry.source.clone(),
            configured: row.configured_bytes,
        });
        lent.offset += delta;
    }
    m.bytes = given;
    true
}

/// Keep `proposal` for `/admin/cache-plan`, show it on `/metrics` and log
/// it. In report mode the same moves recur every tick, since nothing
/// applies them: the gauges show the current ones, the counter and the log
/// only moves the previous tick did not propose. Applied moves count and
/// log every time.
fn record_elastic(proposal: Proposal) {
    let pairs: Vec<(Key, Key)> = proposal
        .moves
        .iter()
        .map(|m| (m.borrower.clone(), m.lender.clone()))
        .collect();
    ELASTIC_PROPOSED.reset();
    let mut proposed: HashMap<(&str, &str), u64> = HashMap::new();
    for m in &proposal.moves {
        *proposed.entry((m.borrower.1, "borrow")).or_default() += m.bytes;
        *proposed.entry((m.lender.1, "lend")).or_default() += m.bytes;
    }
    for ((cache, role), bytes) in proposed {
        ELASTIC_PROPOSED
            .with_label_values(&[cache, role])
            .set(bytes.min(i64::MAX as u64) as i64);
    }
    let block = {
        let mut governor = governor();
        let before = governor.elastic_logged.clone().unwrap_or_default();
        for m in &proposal.moves {
            let pair = (m.borrower.clone(), m.lender.clone());
            if proposal.applied || !before.contains(&pair) {
                for (key, role) in [(&m.borrower, "borrow"), (&m.lender, "lend")] {
                    ELASTIC_MOVES
                        .with_label_values(&[key.1, role, proposal.mode()])
                        .inc();
                }
            }
        }
        let changed = proposal.held.is_none()
            && (proposal.applied && !pairs.is_empty()
                || governor.elastic_logged.as_ref() != Some(&pairs));
        if proposal.held.is_none() {
            governor.elastic_logged = Some(pairs);
        }
        let block = changed.then(|| elastic_block(&proposal));
        governor.elastic = Some(proposal);
        block
    };
    if let Some(block) = block {
        info!("{block}");
    }
}

/// A cache's name in elastic memory's log lines: its id, and its
/// collection or source.
fn key_name((_, id, scope): &Key) -> String {
    match scope.collection() {
        Some(collection) => format!("{id} {collection}"),
        None => id.to_string(),
    }
}

/// Elastic memory's INFO block for `proposal`: each move it would make,
/// from the lender to the borrower, with what a MiB more is worth in each.
fn elastic_block(proposal: &Proposal) -> String {
    let mut out = format!(
        "Elastic memory, {} (#1314): {} cache{} weighed",
        if proposal.applied {
            "applied"
        } else {
            "report only, nothing resized"
        },
        proposal.caches.len(),
        if proposal.caches.len() == 1 { "" } else { "s" }
    );
    if proposal.moves.is_empty() {
        out.push_str(&format!(
            ", no move: no cache's MiB more is worth {} times another's",
            decide::ELASTIC_VALUE_RATIO
        ));
        return out;
    }
    let _ = write!(
        out,
        ", {} move{} {}, in steps of at most {}",
        proposal.moves.len(),
        if proposal.moves.len() == 1 { "" } else { "s" },
        if proposal.applied {
            "made"
        } else {
            "a tick would make"
        },
        mib(proposal.step_bytes)
    );
    for m in &proposal.moves {
        let _ = write!(
            out,
            "\n  {} → {}: {}; a MiB more saves {:.3} ms of fills an hour there, on average up to \
             {} × its size more, against {:.3} a MiB serves where it comes from",
            key_name(&m.lender),
            key_name(&m.borrower),
            mib(m.bytes),
            cache_plan::ms_per_mib(m.borrower_value),
            1.0 + ds_cache::plan::GHOST_DEPTH_FACTORS[m.depth],
            cache_plan::ms_per_mib(m.lender_value),
        );
    }
    out
}

/// The valve at `now`: the process holds `resident` bytes, above its
/// `total` for a minute. Lower every flexible cache to 90 % of what it
/// holds, keep what each reached as its ceiling, and purge every arena's
/// dirty pages. A cache that refuses its shrink in place keeps no ceiling,
/// and the WARN names it. The first relief of a spell of pressure WARNs,
/// and so does one that lowers other caches than the last; one lowering
/// the same again logs at INFO. With nothing it could lower, it purges at
/// most every 10 minutes and WARNs that the process is still over at most
/// every 30, or when other caches refuse. Only what it did is counted.
/// Returns whether it lowered anything.
fn relieve(state: &AdminState, resident: u64, total: u64, now: Instant) -> bool {
    let rows = cache_plan::snapshot(state);
    let report = memory_plan::last();
    let plan = report.as_ref().and_then(|r| r.plan.as_ref());
    let slots: Vec<Slot> = rows
        .iter()
        .map(|row| {
            let grant = plan.and_then(|p| p.grant(&row.entry));
            Slot {
                entry: row.entry.clone(),
                capacity: row.usage.capacity_bytes,
                used: row.usage.used_bytes,
                floor: grant
                    .map_or(row.usage.capacity_bytes, |g| floor_of(&row.entry, g))
                    .max(row.least_bytes),
                flexible: !grant::pinned(&row.entry.source)
                    && !decide::spared(row)
                    && grant.is_some_and(|g| !matches!(g.reason, Reason::Unreachable(_))),
            }
        })
        .collect();
    let (mut changes, mut refused) = (Vec::new(), Vec::new());
    for (key, target) in decide::relieve(&slots) {
        let Some(slot) = slots.iter().find(|s| decide::key(&s.entry) == key) else {
            continue;
        };
        // An owner gone first: nothing to lower, nothing to say.
        let Some(to) = ds_cache::plan::apply(&slot.entry, target) else {
            continue;
        };
        let change = Change {
            entry: slot.entry.clone(),
            from_bytes: slot.capacity,
            to_bytes: to,
            target_bytes: target,
            reason: "memory pressure: 90 % of what it holds".into(),
            held: (to != target).then_some(Held::Refused),
            cleared: false,
        };
        if to < slot.capacity {
            // Its ceiling is what it reached, where a shrink stopped short.
            governor().caps.insert(
                key,
                Ceiling {
                    bytes: to,
                    source: slot.entry.source.clone(),
                },
            );
            changes.push(change);
        } else {
            refused.push(change);
        }
    }
    let due =
        |at: Option<Instant>, every| at.is_none_or(|at| now.saturating_duration_since(at) >= every);
    if changes.is_empty() {
        // At the floors, or refused: purge and WARN, but not every minute.
        let stuck: BTreeSet<Key> = refused.iter().map(|c| decide::key(&c.entry)).collect();
        let (purge_due, warn_due) = {
            let mut governor = governor();
            let purge_due = due(governor.purged_at, decide::PURGE_EVERY);
            let warn_due =
                governor.stuck != stuck || due(governor.stuck_warned_at, decide::FLOORS_WARN_EVERY);
            if purge_due {
                governor.purged_at = Some(now);
            }
            if warn_due {
                governor.stuck_warned_at = Some(now);
                governor.stuck = stuck;
            }
            (purge_due, warn_due)
        };
        if purge_due && purge() {
            PRESSURE_ACTIONS.with_label_values(&["purge"]).inc();
        }
        if warn_due {
            warn!("{}", stuck_block(resident, total, &refused));
        }
        return false;
    }
    memory_plan::count_changes(&changes);
    PRESSURE_ACTIONS.with_label_values(&["relieve"]).inc();
    let purged = purge();
    if purged {
        PRESSURE_ACTIONS.with_label_values(&["purge"]).inc();
    }
    let mut governor = governor();
    governor.purged_at = Some(now);
    let lowered: BTreeSet<Key> = changes.iter().map(|c| decide::key(&c.entry)).collect();
    let block = relief_block(resident, total, &changes, &refused, purged);
    if governor.lowered.as_ref() == Some(&lowered) {
        info!("{block}");
    } else {
        warn!("{block}");
    }
    governor.lowered = Some(lowered);
    governor.last_relief = Some(Record {
        at: chrono::Utc::now(),
        changes,
    });
    true
}

/// The valve's WARN while the process stays over its total and it could
/// lower nothing: every flexible cache at its floor, or the `refused`
/// shrinks, each named. At most every 30 minutes, or when other caches
/// refuse.
pub(crate) fn stuck_block(resident: u64, total: u64, refused: &[Change]) -> String {
    let mut out = format!(
        "Memory pressure: jemalloc still holds {} resident, above the total of {}",
        mib(resident),
        mib(total)
    );
    if refused.is_empty() {
        out.push_str(
            ", and every flexible cache is at its floor: what holds the memory is outside the \
             caches the valve lowers",
        );
    } else {
        let _ = write!(
            out,
            ", and the valve could lower nothing: {}",
            refused_line(refused.len())
        );
        memory_plan::write_changes(&mut out, refused);
        out.push_str(RESTART);
    }
    out.push_str(
        "\n  Dirty pages are purged at most every 10 minutes; this repeats at most every 30, or \
         when other caches refuse",
    );
    out
}

/// What the valve says of the caches that refused their shrink in place.
fn refused_line(count: usize) -> String {
    format!(
        "{count} cache{} refused {} shrink in place and keep{} {} size, with no ceiling",
        if count == 1 { "" } else { "s" },
        if count == 1 { "its" } else { "their" },
        if count == 1 { "s" } else { "" },
        if count == 1 { "its" } else { "their" },
    )
}

/// Where a refused shrink leaves the memory.
const RESTART: &str = "\n  A shrink in place stops where each shard still holds its minimum of \
                       entries: a restart reaches the size, building each cache empty at its \
                       planned size";

/// The INFO block of a re-plan tick that changed sizes: each change, old →
/// new, and why; `restored` of them the valve had lowered, `lifted` back
/// at their grant. `None` when nothing changed.
pub(crate) fn tick_block(changed: &[Change], restored: usize, lifted: usize) -> Option<String> {
    if changed.is_empty() {
        return None;
    }
    let mut out = format!(
        "Memory plan re-planned (every 5 minutes, [memory] mode = \"auto\"): {} size{} changed",
        changed.len(),
        if changed.len() == 1 { "" } else { "s" }
    );
    if restored > 0 {
        let _ = write!(
            out,
            "; {restored} cache{} the pressure valve lowered restored by 5 % of {} grant, \
             {lifted} back at it",
            if restored == 1 { "" } else { "s" },
            if restored == 1 { "its" } else { "their" },
        );
    }
    memory_plan::write_changes(&mut out, changed);
    Some(out)
}

/// The valve's WARN block: what the process holds against its total, each
/// cache lowered, whether the dirty pages were purged, and each cache that
/// refused its shrink.
pub(crate) fn relief_block(
    resident: u64,
    total: u64,
    changes: &[Change],
    refused: &[Change],
    purged: bool,
) -> String {
    let mut out = format!(
        "Memory pressure: jemalloc holds {} resident, above the total of {} for a minute: \
         {} cache{} lowered to 90 % of what {} hold{}{}",
        mib(resident),
        mib(total),
        changes.len(),
        if changes.len() == 1 { "" } else { "s" },
        if changes.len() == 1 { "it" } else { "they" },
        if changes.len() == 1 { "s" } else { "" },
        if purged {
            ", dirty pages purged"
        } else {
            ", dirty pages not purged (jemalloc refused)"
        },
    );
    memory_plan::write_changes(&mut out, changes);
    out.push_str(
        "\n  They stay at most this large until the process is under 90 % of the total for 10 \
         minutes, then grow back by 5 % of their grant every 5 minutes",
    );
    if !refused.is_empty() {
        let _ = write!(out, "\n  {}:", refused_line(refused.len()));
        memory_plan::write_changes(&mut out, refused);
        out.push_str(RESTART);
    }
    out
}

static PRESSURE_ACTIONS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    let counter = IntCounterVec::new(
        Opts::new(
            "memory_pressure_actions_total",
            "What the memory pressure valve did (#1315), by action: relieve, jemalloc resident \
             above the [memory] total for a minute lowered the flexible caches to 90 % of what \
             they hold; purge, it purged every arena's dirty pages; restore, a re-plan tick \
             raised the lowered caches toward the plan after 10 minutes under 90 % of the total. \
             Only in [memory] mode = \"auto\" with total_mb or a cgroup limit",
        ),
        &["action"],
    )
    .unwrap();
    crate::admin::REGISTRY
        .register(Box::new(counter.clone()))
        .unwrap();
    for action in ["relieve", "purge", "restore"] {
        counter.with_label_values(&[action]).inc_by(0);
    }
    counter
});

static ELASTIC_MOVES: LazyLock<IntCounterVec> = LazyLock::new(|| {
    let counter = IntCounterVec::new(
        Opts::new(
            "cache_elastic_moves_total",
            "Moves of memory between caches elastic memory proposed or made at a re-plan \
             tick (#1314), by cache, role and mode. role: borrow, the cache whose MiB more \
             saves the most fill time; lend, one whose MiB serves at most half as much. \
             mode = report: proposed, never applied, counted once while the same move recurs \
             each tick (cache_elastic_proposed_bytes shows the current ones); applied: made, \
             counted each time. Per-collection caches share their cache label; \
             /admin/cache-plan has each collection's. Only in [memory] mode = \"auto\" with a \
             total, never while the pressure valve holds",
        ),
        &["cache", "role", "mode"],
    )
    .unwrap();
    crate::admin::REGISTRY
        .register(Box::new(counter.clone()))
        .unwrap();
    counter
});

static ELASTIC_PROPOSED: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    let gauge = IntGaugeVec::new(
        Opts::new(
            "cache_elastic_proposed_bytes",
            "Bytes the last re-plan tick's elastic memory moves would take from (role = lend) \
             or give to (role = borrow) each cache (#1314), summed over its collections",
        ),
        &["cache", "role"],
    )
    .unwrap();
    crate::admin::REGISTRY
        .register(Box::new(gauge.clone()))
        .unwrap();
    gauge
});

/// Register the governor's counters, so `/metrics` lists them before the
/// first change.
pub(crate) fn init_counters() {
    LazyLock::force(&PRESSURE_ACTIONS);
    LazyLock::force(&ELASTIC_MOVES);
    LazyLock::force(&ELASTIC_PROPOSED);
    memory_plan::init_change_counter();
}

/// The governor for `/admin/cache-plan`: the last re-plan tick, the
/// valve's state and last actions, and its ceilings.
pub(crate) fn json() -> Value {
    let governor = governor();
    let now = Instant::now();
    let record = |record: &Option<Record>| match record {
        Some(r) => json!({
            "at": r.at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "changes": r.changes.iter().map(memory_plan::change_json).collect::<Vec<_>>(),
        }),
        None => Value::Null,
    };
    let secs = |since: Option<Instant>| since.map(|s| now.saturating_duration_since(s).as_secs());
    let mut ceilings: Vec<Value> = governor
        .caps
        .iter()
        .map(|((kind, id, scope), ceiling)| {
            json!({
                "id": id,
                "kind": kind.as_str(),
                "collection": scope.collection(),
                "ceiling_bytes": ceiling.bytes,
                "source": ceiling.source.to_string(),
            })
        })
        .collect();
    ceilings.sort_by_key(|c| (c["id"].to_string(), c["collection"].to_string()));
    json!({
        "replan_every_secs": decide::REPLAN_EVERY.as_secs(),
        "sample_every_secs": decide::SAMPLE_EVERY.as_secs(),
        "last_tick": record(&governor.last_tick),
        "pressure": {
            "valve": governor.sample.is_some(),
            "resident_bytes": governor.sample.map(|(r, _)| r),
            "total_bytes": governor.sample.map(|(_, t)| t),
            "state": governor.pressure.state(),
            "over_secs": secs(governor.pressure.over_since),
            "under_secs": secs(governor.pressure.under_since),
            "ceilings": ceilings,
            "last_relief": record(&governor.last_relief),
            "last_restore": record(&governor.last_restore),
        },
        "elastic": governor
            .elastic
            .as_ref()
            .map_or(Value::Null, |p| elastic_json(p, &governor.ledger)),
    })
}

/// Elastic memory's last proposal for `/admin/cache-plan` (#1314): the
/// moves it would make and every cache it weighed, values in milliseconds
/// of fills an hour per MiB more.
fn elastic_json(proposal: &Proposal, ledger: &Ledger) -> Value {
    let name = |(_, id, scope): &Key| json!({"id": id, "collection": scope.collection()});
    let value = |v: f64| (cache_plan::ms_per_mib(v) * 1e4).round() / 1e4;
    let depth = |d: usize| ds_cache::GHOST_DEPTHS[d];
    let mut ledger: Vec<(&Key, &Lent)> = ledger.iter().collect();
    ledger.sort_by(|a, b| a.0.cmp(b.0));
    let ledger: Vec<Value> = ledger
        .into_iter()
        .map(|(key, lent)| {
            let mut entry = name(key);
            entry["offset_bytes"] = json!(lent.offset);
            entry
        })
        .collect();
    json!({
        "mode": proposal.mode(),
        "at": proposal.at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "held": proposal.held,
        "step_bytes": proposal.step_bytes,
        "headroom_bytes": proposal.headroom_bytes,
        "moves": proposal.moves.iter().map(|m| json!({
            "borrower": name(&m.borrower),
            "lender": name(&m.lender),
            "bytes": m.bytes,
            "borrower_value_ms_per_mib_hour": value(m.borrower_value),
            "borrower_depth": depth(m.depth),
            "lender_value_ms_per_mib_hour": value(m.lender_value),
        })).collect::<Vec<_>>(),
        "caches": proposal.caches.iter().map(|c| json!({
            "id": c.key.1,
            "collection": c.key.2.collection(),
            "capacity_bytes": c.capacity,
            "floor_bytes": c.floor,
            "ceiling_bytes": c.ceiling,
            "ghost_hits": c.ghost_hits,
            "depth": depth(c.depth),
            "value_ms_per_mib_hour": value(c.value),
            "lend_value_ms_per_mib_hour": value(c.lend_value),
            "borrows": c.borrows,
            "lends": c.lends,
        })).collect::<Vec<_>>(),
        "ledger": ledger,
    })
}

#[cfg(test)]
mod tests;
