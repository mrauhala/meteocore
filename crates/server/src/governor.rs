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
//!   sizes, as `[memory] elastic` says ([`elastic_mode`]): weigh what a
//!   byte more is worth in each cache the plan sizes, and what a byte of it
//!   serves ([`elastic_inputs`]), and move memory to the caches where more
//!   is worth most, from the pool's free headroom and the caches where it
//!   serves least; a cache read again takes back what it lent the pool, a
//!   cache below its grant what borrowers hold, then what idle caches its
//!   engines fill ahead of reads hold above their keep floor, and idle
//!   caches decay ([`decide::elastic`]), within the pool's expected use and
//!   above every cache's keep floor ([`keep_of`]): what a first view of a
//!   quiet collection needs, smaller after a day without a lookup. A cache
//!   filled ahead of its reads neither lends nor decays. `report` only
//!   proposes: logged when the moves change, shown on `/metrics` and in
//!   `/admin/cache-plan`. `apply` makes them, each a loan in the
//!   [`ledger`], whose offsets the tick and a reload's plan add to each
//!   cache's grant, held exactly. Nothing moves while the valve holds, and
//!   nothing borrows at or above 90 % of the total; the valve's relief takes
//!   back what caches borrowed first, only the excess and never below a
//!   keep floor ([`relief`]), and holds borrowing off for the restore's 10
//!   minutes after.
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
pub(crate) mod ledger;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

use ds_cache::plan::grant::{self, Grant, Reason};
use ds_cache::plan::{Entry, Row};
use ds_cache::MIB;
use prometheus::{IntCounterVec, IntGaugeVec, Opts};
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::admin::AdminState;
use crate::cache_plan::{self, mib};
use crate::memory_plan::{self, Change, Held};
use decide::{Ceiling, Elastic, Key, Move, MoveKind, Pressure, Refused, Signal, Slot};
use ds_core::config::ElasticMode;
use ledger::{Ledger, Party};

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
    /// `[memory] elastic`, as the tick read it.
    mode: ElasticMode,
    /// Why it weighed nothing: the pressure valve holds.
    held: Option<&'static str>,
    /// Caches could borrow: the process was under 90 % of its total, and
    /// no take-back of the valve's held borrowing off.
    may_borrow: bool,
    /// Caches could take back what they lent the pool: the process was
    /// under 90 % of its total.
    may_return: bool,
    step_bytes: u64,
    /// What the plan's expected use left of the pool when it weighed.
    headroom_bytes: u64,
    caches: Vec<Elastic>,
    moves: Vec<Move>,
}

impl Proposal {
    /// Its moves were applied, not only proposed.
    fn applied(&self) -> bool {
        self.mode == ElasticMode::Apply
    }

    /// The `mode` label of its moves on `/metrics`.
    fn label(&self) -> &'static str {
        if self.applied() {
            "applied"
        } else {
            "report"
        }
    }
}

/// Why elastic memory proposes nothing while the pressure valve holds.
const VALVE_HOLDS: &str = "the memory pressure valve holds";

/// What elastic memory does for the accepted config (#1314): `[memory]
/// elastic` in `mode = "auto"`, else off.
fn elastic_mode(state: &crate::admin::ServerState) -> ElasticMode {
    gate(state).map_or(ElasticMode::Off, |accepted| accepted.memory_elastic())
}

/// A move's kind, borrower and lender: what the log compares tick to tick.
type Pair = (MoveKind, Option<Key>, Option<Key>);

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
    elastic_logged: Option<Vec<Pair>>,
    /// A take-back of the valve's holds borrowing off (#1314) until the
    /// process has been under 90 % of its total for the restore's 10
    /// minutes ([`Pressure::may_restore`]).
    borrow_held: bool,
    /// The loans elastic memory made (#1314), whose offsets the plan's
    /// grants take: empty unless `[memory] elastic = "apply"`. In memory
    /// only: a restart starts every cache at its plan.
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
    match ledger.offset(key) {
        0 => grant.bytes,
        offset => decide::elastic_target(
            grant.bytes,
            offset,
            floor_of(entry, grant),
            grant.ceiling_bytes,
        ),
    }
}

/// The ledger at a re-plan tick, or before a reload's plan applies
/// (#1314). Under `mode = "apply"`: without the loans `rows` supersede
/// ([`reconcile_ledger`]), held to `plan` ([`bound_ledger`]), and cleared,
/// with a WARN, should it break its invariant ([`ledger_holds`]). Under
/// any other mode, empty: leaving `apply` ends every loan, and the re-plan
/// tick returns each cache to its grant as it moves any cache; entering it
/// starts with none.
fn keep_ledger(ledger: &mut Ledger, mode: ElasticMode, rows: &[Row], plan: Option<&grant::Plan>) {
    if mode != ElasticMode::Apply {
        ledger.clear();
        return;
    }
    reconcile_ledger(ledger, rows);
    let Some(plan) = plan else {
        return;
    };
    bound_ledger(ledger, rows, plan);
    if let Err(broken) = ledger_holds(rows, plan, ledger) {
        warn!(
            "Elastic memory (#1314): {broken}; every loan ended, so each cache returns to its \
             plan"
        );
        ledger.clear();
    }
}

/// The ledger's invariant under `plan` (#1314): what caches borrowed
/// equals what caches and the pool lent (every cache's offset sums to what
/// the pool lent less what it borrowed), no loan is empty, to itself or
/// between the pool and itself, and the loans lift the plan's expected
/// use past neither its pool nor, where the plan's own expected use is
/// over the pool already, that.
fn ledger_holds(rows: &[Row], plan: &grant::Plan, ledger: &Ledger) -> Result<(), String> {
    ledger.balanced()?;
    let expected = expected_use(rows, plan, ledger);
    let (limit, name) = if plan.pool_bytes >= plan.expected_bytes {
        (plan.pool_bytes, "the pool")
    } else {
        (
            plan.expected_bytes,
            "the plan's own expected use, already past its pool,",
        )
    };
    if expected > limit {
        return Err(format!(
            "its loans lift the expected use to {}, past {name} of {}",
            mib(expected),
            mib(limit)
        ));
    }
    Ok(())
}

/// Hold the ledger to a reload's `plan` before it is applied (#1314): its
/// grants take the offsets through `capped`, so a smaller total sheds them
/// now rather than at the next tick.
pub(crate) fn bound_to(
    state: &crate::admin::ServerState,
    rows: &[Row],
    plan: Option<&grant::Plan>,
) {
    let mut governor = governor();
    keep_ledger(&mut governor.ledger, elastic_mode(state), rows, plan);
    show_offsets(&governor.ledger);
}

/// End the loans `rows` supersede (#1314): of a cache now sized from
/// another source or configured size than when it moved (a reload pinned,
/// re-bounded or resized it), or gone. Its counterparts shed them.
fn reconcile_ledger(ledger: &mut Ledger, rows: &[Row]) {
    let rows: HashMap<Key, &Row> = rows
        .iter()
        .map(|row| (decide::key(&row.entry), row))
        .collect();
    ledger.reconcile(&|key| {
        rows.get(key)
            .map(|row| (row.entry.source.clone(), row.configured_bytes))
    });
}

/// Hold the ledger to `plan` (#1314): each cache's offset within its floor,
/// its keep floor included as far as its grant reaches, and ceiling, the
/// lowest-valued counterparts shedding first, and the
/// expected use it adds within the pool, the lowest-valued borrowers
/// shedding first. A cache the plan no longer sizes loses its loans.
fn bound_ledger(ledger: &mut Ledger, rows: &[Row], plan: &grant::Plan) {
    let values = elastic_values(rows, plan, ledger);
    let value = |key: &Key| values.get(key).copied().unwrap_or(0.0);
    let bounds = |key: &Key| {
        let row = rows.iter().find(|row| decide::key(&row.entry) == *key)?;
        let grant = plan
            .grant(&row.entry)
            .filter(|g| !matches!(g.reason, Reason::Unreachable(_)))?;
        // Its keep floor too (#1314), as far as its grant reaches: what a
        // cache lent comes back when its keep grows past what is left.
        let floor = floor_of(&row.entry, grant).max(keep_of(row).min(grant.bytes));
        Some((grant.bytes, floor, grant.ceiling_bytes))
    };
    ledger.bound(&bounds, &value);
    // The borrowers lowest-valued first, and from each its loans that add
    // to the expected use, until the pool holds it.
    let mut borrowers: Vec<Key> = ledger
        .keys()
        .into_iter()
        .filter(|key| ledger.borrowed(key) > 0)
        .collect();
    borrowers.sort_by(|a, b| value(a).total_cmp(&value(b)));
    for key in borrowers {
        loop {
            let expected = expected_use(rows, plan, ledger);
            if expected <= plan.pool_bytes || ledger.borrowed(&key) == 0 {
                break;
            }
            // A borrower whose loans lift nothing (a working set holding
            // less than it has) keeps them.
            let mut trial = ledger.clone();
            let over = expected - plan.pool_bytes;
            trial.shrink(&key, true, over.div_ceil(MIB) * MIB, &value);
            if expected_use(rows, plan, &trial) >= expected {
                break;
            }
            *ledger = trial;
        }
    }
}

/// Each weighed cache's value as a counterpart in the ledger: what a byte
/// of it serves ([`Elastic::lend_value`]).
fn elastic_values(rows: &[Row], plan: &grant::Plan, ledger: &Ledger) -> HashMap<Key, f64> {
    elastic_inputs(rows, plan, ledger)
        .into_iter()
        .map(|c| (c.key, c.lend_value))
        .collect()
}

/// End the loans of the caches of `collections`, which a reload rebuilt or
/// removed (#1314): a new cache starts from its plan's grant, and its
/// counterparts shed what it borrowed or lent.
pub(crate) fn forget_elastic(collections: &[&str]) {
    governor().ledger.forget(collections);
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
    keep_ledger(&mut governor().ledger, elastic_mode(state), rows, None);
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
fn gate(state: &crate::admin::ServerState) -> Option<std::sync::Arc<crate::admin::AcceptedLoad>> {
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
        _ => without_valve(&mut governor()),
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

/// Each cache's offset in `ledger` (#1314).
fn offsets(ledger: &Ledger) -> HashMap<Key, i64> {
    ledger
        .keys()
        .into_iter()
        .map(|key| {
            let offset = ledger.offset(&key);
            (key, offset)
        })
        .collect()
}

/// The caches whose offset differs between `before` and `after`, one
/// absent from either counting as 0: a loan booked, shed or ended.
fn changed_offsets(before: &HashMap<Key, i64>, after: &HashMap<Key, i64>) -> HashSet<Key> {
    before
        .keys()
        .chain(after.keys())
        .filter(|key| before.get(*key).unwrap_or(&0) != after.get(*key).unwrap_or(&0))
        .cloned()
        .collect()
}

/// No valve, with no total and no cgroup limit: its ceilings go, and the
/// next tick gives those caches their grant at once rather than by the
/// thresholds. A take-back's hold on borrowing goes with them: with no
/// total, nothing would ever end it (#1314).
fn without_valve(governor: &mut Governor) {
    release(governor);
    governor.sample = None;
    governor.pressure = Pressure::default();
    governor.borrow_held = false;
}

/// What a tick at `now` lets elastic memory do ([`decide::borrowing`]): a
/// take-back's hold on borrowing ends once the process has been under 90 %
/// of its total for the restore's 10 minutes.
fn tick_rules(governor: &mut Governor, now: Instant) -> (bool, bool) {
    if governor.borrow_held && governor.pressure.may_restore(now) {
        governor.borrow_held = false;
    }
    decide::borrowing(governor.sample, governor.borrow_held)
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
    let mode = elastic_mode(state);
    let (caps, refused, raised, restored, lifted, valve_holds, rules, ledger) = {
        let mut governor = governor();
        // A cache whose offset keeping the ledger changed or ended is held
        // to its grant plus its offset exactly, past the tick's dead band:
        // what a shed takes off reaches the cache (#1314). One whose offset
        // stands keeps the dead band, so a grant that wobbles with measured
        // memory resizes nothing; the headroom counts what it holds.
        let before = offsets(&governor.ledger);
        keep_ledger(&mut governor.ledger, mode, &rows, report.plan.as_ref());
        let moved = changed_offsets(&before, &offsets(&governor.ledger));
        // No borrowing at or above 90 % of the total, nor after a take-back
        // until the restore's 10 minutes under 90 % have passed: elastic
        // memory never grows into pressure. Returns only under 90 %.
        let rules = tick_rules(&mut governor, now);
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
        lifted.extend(moved);
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
            rules,
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
        // Booked in place, a move at a time: a panic loses at most the move
        // in flight, and /admin shows the loans meanwhile.
        let with_ledger = |book: &mut dyn FnMut(&mut Ledger)| book(&mut governor().ledger);
        let proposal = run_elastic(
            &rows,
            plan,
            &with_ledger,
            mode,
            valve_holds,
            rules,
            &ds_cache::plan::apply,
        );
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
pub(crate) fn elastic_inputs(rows: &[Row], plan: &grant::Plan, ledger: &Ledger) -> Vec<Elastic> {
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
            let keep = keep_of(row);
            Some(Elastic {
                key: decide::key(entry),
                capacity,
                floor: floor_of(entry, grant).max(row.least_bytes).max(keep),
                ceiling: grant.ceiling_bytes,
                value,
                ghost_hits: ghost[depth],
                depth,
                borrows: timed.is_some(),
                lend_value: decide::lend_value(value, window.per_hour(window.hits), cost, capacity),
                lends: !decide::spared(row) && !prewarmed(row),
                need: (grant.expected_bytes < grant.bytes).then_some(grant.expected_bytes),
                grant: grant.bytes,
                offset: ledger.offset(&decide::key(entry)),
                used: row.usage.used_bytes,
                idle: window.label() == Some(ds_cache::plan::Label::Idle),
                decays: !decide::spared(row) && !prewarmed(row),
                pool_lent: ledger.lent_to_pool(&decide::key(entry)),
                ahead: prewarmed(row) || row.demand.as_ref().is_some_and(|d| d.ahead),
                keep,
                recall: ledger.largest_loan_to_a_cache(&decide::key(entry)),
            })
        })
        .collect()
}

/// The keep floor a cache's whole size is fixed at (#1314): the meta-tile,
/// rendered-image and vector-tile caches, process-wide, keep enough for a
/// first view of any collection.
pub(crate) const FIXED_KEEP_BYTES: u64 = 64 * MIB;

/// The caches with a fixed keep floor ([`FIXED_KEEP_BYTES`]).
const FIXED_KEEP: [&str; 3] = ["metatile_cache", "rendered_cache", "vector_tile_cache"];

/// The least elastic memory leaves `row`'s cache (#1314): the larger of
///
/// - [`FIXED_KEEP_BYTES`] for the meta-tile, rendered-image and vector-tile
///   caches;
/// - its demand for a demand cache, or a floor its engines publish as its
///   demand (the PVOL pixels' two pre-warms): what it must hold;
/// - the keep floor its engines publish (`DemandSlot::keep`): the newest
///   frame's pre-warmed tiles, the default field of the newest run, that
///   run's point indexes; after a day without a lookup, the smaller floor
///   they publish for that (`CacheDemand::when_quiet`).
///
/// 0 for any other cache: the QueryData caches and the stores keep none.
pub(crate) fn keep_of(row: &Row) -> u64 {
    let fixed = if FIXED_KEEP.contains(&row.entry.id) {
        FIXED_KEEP_BYTES
    } else {
        0
    };
    let demand = row
        .demand
        .as_ref()
        .filter(|d| d.floor || grant::class(row.entry.id) == grant::Class::Demand)
        .map_or(0, |d| d.bytes);
    let published = row
        .keep
        .as_ref()
        .map_or(0, |keep| keep.floor(row.quiet_for));
    fixed.max(demand).max(published)
}

/// Whether `row`'s engines fill its cache ahead of its reads
/// (`Churn::prewarmed`): such a cache neither lends nor decays (#1314).
fn prewarmed(row: &Row) -> bool {
    row.usage.churn.as_ref().is_some_and(|c| c.prewarmed)
}

/// The plan's expected use with the offsets of `ledger` (#1314): each
/// cache's expected use at its grant plus offset, rather than at its grant,
/// the others' as the plan counts them.
fn expected_use(rows: &[Row], plan: &grant::Plan, ledger: &Ledger) -> u64 {
    let mut expected = plan.expected_bytes;
    for row in rows {
        let key = decide::key(&row.entry);
        let Some(grant) = plan.grant(&row.entry) else {
            continue;
        };
        if ledger.offset(&key) == 0 {
            continue;
        }
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
    expected
}

/// The plan's expected use with each cache at the larger of its capacity
/// and its grant plus its offset (#1314): what the caches can hold now,
/// a cache the tick's dead band or a refused shrink left above its target
/// counted where it is. Only the caches the plan's pool holds: a budget
/// sits outside it, and an unreachable cache, cleared but keeping its
/// capacity, holds nothing the pool counts.
fn held_use(rows: &[Row], plan: &grant::Plan, ledger: &Ledger) -> u64 {
    let mut expected = plan.expected_bytes;
    for row in rows {
        if row.entry.kind != ds_cache::plan::Kind::Cache {
            continue;
        }
        let Some(grant) = plan.grant(&row.entry) else {
            continue;
        };
        if matches!(grant.reason, Reason::Unreachable(_)) {
            continue;
        }
        let at = |bytes: u64| {
            if grant.expected_bytes < grant.bytes {
                bytes.min(grant.expected_bytes)
            } else {
                bytes
            }
        };
        let target = elastic_grant(ledger, &decide::key(&row.entry), &row.entry, grant);
        let held = target.max(row.usage.capacity_bytes);
        expected = (expected + at(held)).saturating_sub(at(grant.bytes));
    }
    expected
}

/// What the caches leave of the plan's pool ([`held_use`]): the headroom
/// elastic memory moves within.
fn elastic_headroom(rows: &[Row], plan: &grant::Plan, ledger: &Ledger) -> u64 {
    plan.pool_bytes.saturating_sub(held_use(rows, plan, ledger))
}

/// Run `book` on the ledger: in place in the governor, or a test's own.
type WithLedger<'a> = dyn Fn(&mut dyn FnMut(&mut Ledger)) + 'a;

/// One elastic step over `rows`, the capacities the re-plan tick left, and
/// the `plan` it fitted (#1314): weigh the caches ([`elastic_inputs`]) and
/// decide the moves ([`decide::elastic`]), as `mode` says:
///
/// - `off`: nothing;
/// - `report`: the moves, resizing nothing;
/// - `apply`: the moves, each applied through `resize`
///   (`ds_cache::plan::apply`) and booked in the ledger ([`apply_move`]),
///   with idle caches decaying.
///
/// Nothing while the pressure valve holds (`valve_holds`: over the total,
/// or a ceiling of its still in force), and no borrowing nor return unless
/// `(may_borrow, may_return)` allow it ([`decide::borrowing`]): the valve
/// comes first.
fn run_elastic(
    rows: &[Row],
    plan: &grant::Plan,
    with_ledger: &WithLedger<'_>,
    mode: ElasticMode,
    valve_holds: bool,
    (may_borrow, may_return): (bool, bool),
    resize: &dyn Fn(&Entry, u64) -> Option<u64>,
) -> Proposal {
    let step_bytes = decide::elastic_step(plan.pool_bytes);
    let mut ledger = Ledger::default();
    with_ledger(&mut |current| ledger = current.clone());
    let headroom_bytes = elastic_headroom(rows, plan, &ledger);
    let applies = mode == ElasticMode::Apply;
    let rules = decide::Rules {
        may_borrow,
        may_return,
        decays: applies,
    };
    let (held, caches, mut moves) = if mode == ElasticMode::Off {
        (None, Vec::new(), Vec::new())
    } else if valve_holds {
        (Some(VALVE_HOLDS), Vec::new(), Vec::new())
    } else {
        let caches = elastic_inputs(rows, plan, &ledger);
        let moves = decide::elastic(&caches, plan.pool_bytes, headroom_bytes, rules);
        (None, caches, moves)
    };
    if applies {
        moves.retain_mut(|m| apply_move(m, rows, with_ledger, resize));
    }
    Proposal {
        at: chrono::Utc::now(),
        mode,
        held,
        may_borrow,
        may_return,
        step_bytes,
        headroom_bytes,
        caches,
        moves,
    }
}

/// `row`'s cache as a party to a loan: its key and what its size came from.
fn party(row: &Row) -> Party {
    Party {
        key: decide::key(&row.entry),
        source: row.entry.source.clone(),
        configured: row.configured_bytes,
    }
}

/// Apply `m` through `resize` and book it in the ledger: shrink the
/// lender, grow the borrower by what the lender gave, and give the lender
/// back what the borrower could not take; a side that is the pool is not
/// resized. The loan is what the borrower gained, or, for a decay, what the
/// cache gave the pool, and so is `m.bytes`; false when nothing moved.
/// What the lender gave that neither the borrower took nor the lender's
/// regrow got back (a grow that stopped short) is its loan to the pool, so
/// the ledger holds what the sizes did. Every size it changes counts in
/// `cache_plan_changes_total`.
fn apply_move(
    m: &mut Move,
    rows: &[Row],
    with_ledger: &WithLedger<'_>,
    resize: &dyn Fn(&Entry, u64) -> Option<u64>,
) -> bool {
    let find = |key: &Option<Key>| {
        key.as_ref()
            .and_then(|key| rows.iter().find(|row| decide::key(&row.entry) == *key))
    };
    let (lender, borrower) = (find(&m.lender), find(&m.borrower));
    if m.lender.is_some() && lender.is_none() || m.borrower.is_some() && borrower.is_none() {
        return false;
    }
    let resized = |row: &Row, bytes: u64| {
        let had = row.usage.capacity_bytes;
        let got = resize(&row.entry, bytes).unwrap_or(had);
        if got != had {
            memory_plan::count_change(row.entry.id);
        }
        got
    };
    // What the lender gave: the pool gives what it is asked for.
    let (given, lent_at) = match lender {
        Some(row) => {
            let from = row.usage.capacity_bytes;
            let to = resized(row, from.saturating_sub(m.bytes));
            (from.saturating_sub(to), Some(to))
        }
        None => (m.bytes, None),
    };
    if given == 0 {
        return false;
    }
    // What the lender gave that it did not get back: to the pool.
    let mut stranded = 0;
    let moved = match borrower {
        Some(row) => {
            let had = row.usage.capacity_bytes;
            let gained = resized(row, had + given).saturating_sub(had);
            if gained < given {
                if let (Some(row), Some(at)) = (lender, lent_at) {
                    let back = resized(row, at + (given - gained)).saturating_sub(at);
                    stranded = (given - gained).saturating_sub(back);
                }
            }
            gained
        }
        // A decay: the pool takes all of it.
        None => given,
    };
    let (borrower, lender) = (borrower.map(party), lender.map(party));
    if stranded > 0 {
        with_ledger(&mut |ledger| ledger.book(None, lender.clone(), stranded));
    }
    if moved == 0 {
        return false;
    }
    with_ledger(&mut |ledger| {
        // A borrower that decays gives the pool what it borrowed first, its
        // lenders' share becoming their loans to the pool, which they take
        // back when read again ([`MoveKind::Return`]); then it lends.
        let released = match (&borrower, &lender) {
            (None, Some(party)) => ledger.release_part(&party.key, moved, &|_| 0.0),
            _ => 0,
        };
        ledger.book(borrower.clone(), lender.clone(), moved - released);
    });
    m.bytes = moved;
    true
}

/// The `role` labels of a move's borrower and lender on `/metrics`.
fn roles(kind: MoveKind) -> (&'static str, &'static str) {
    match kind {
        MoveKind::Borrow => ("borrow", "lend"),
        MoveKind::Reclaim => ("reclaim", "repay"),
        MoveKind::ReclaimPrewarmed => ("reclaim", "prewarmed"),
        MoveKind::Decay => ("borrow", "decay"),
        MoveKind::Return => ("return", "return"),
        MoveKind::Recall => ("return", "repay"),
    }
}

/// Keep `proposal` for `/admin/cache-plan`, show it on `/metrics` and log
/// it. In report mode the same moves recur every tick, since nothing
/// applies them: the gauges show the current ones, the counter and the log
/// only moves the previous tick did not propose. Applied moves count and
/// log every time, each with its reason. Off shows nothing.
fn record_elastic(proposal: Proposal) {
    ELASTIC_PROPOSED.reset();
    let mut proposed: HashMap<(&str, &str), u64> = HashMap::new();
    for m in &proposal.moves {
        let (borrow, lend) = roles(m.kind);
        if let Some(key) = &m.borrower {
            *proposed.entry((key.1, borrow)).or_default() += m.bytes;
        }
        if let Some(key) = &m.lender {
            *proposed.entry((key.1, lend)).or_default() += m.bytes;
        }
    }
    for ((cache, role), bytes) in proposed {
        ELASTIC_PROPOSED
            .with_label_values(&[cache, role])
            .set(bytes.min(i64::MAX as u64) as i64);
    }
    let pairs: Vec<Pair> = proposal
        .moves
        .iter()
        .map(|m| (m.kind, m.borrower.clone(), m.lender.clone()))
        .collect();
    let block = {
        let mut governor = governor();
        let before = governor.elastic_logged.clone().unwrap_or_default();
        for (m, pair) in proposal.moves.iter().zip(&pairs) {
            if proposal.applied() || !before.contains(pair) {
                let (borrow, lend) = roles(m.kind);
                for (key, role) in [(&m.borrower, borrow), (&m.lender, lend)] {
                    if let Some(key) = key {
                        ELASTIC_MOVES
                            .with_label_values(&[key.1, role, proposal.label()])
                            .inc();
                    }
                }
            }
        }
        let weighs = proposal.mode != ElasticMode::Off && proposal.held.is_none();
        let changed = weighs
            && (proposal.applied() && !pairs.is_empty()
                || governor.elastic_logged.as_ref() != Some(&pairs));
        if weighs {
            governor.elastic_logged = Some(pairs);
        }
        let block = changed.then(|| elastic_block(&proposal));
        governor.elastic = Some(proposal);
        show_offsets(&governor.ledger);
        block
    };
    if let Some(block) = block {
        info!("{block}");
    }
}

/// A cache's name in elastic memory's log lines: its id, and its
/// collection or source; the pool's free headroom for `None`.
fn key_name(key: &Option<Key>) -> String {
    match key {
        Some((_, id, scope)) => match scope.collection() {
            Some(collection) => format!("{id} {collection}"),
            None => id.to_string(),
        },
        None => "the pool".to_string(),
    }
}

/// Elastic memory's INFO block for `proposal`: each move it made or would
/// make, from the lender to the borrower, with its reason and what a MiB
/// is worth on each side.
fn elastic_block(proposal: &Proposal) -> String {
    let mut out = format!(
        "Elastic memory, {} (#1314): {} cache{} weighed",
        if proposal.applied() {
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
        if proposal.applied() {
            "made"
        } else {
            "a tick would make"
        },
        mib(proposal.step_bytes)
    );
    for m in &proposal.moves {
        let reason = match m.kind {
            MoveKind::Borrow => format!(
                "a MiB more saves {:.3} ms of fills an hour there, on average up to {} × its \
                 size more, against {:.3} a MiB serves where it comes from",
                cache_plan::ms_per_mib(m.borrower_value),
                1.0 + ds_cache::plan::GHOST_DEPTH_FACTORS[m.depth],
                cache_plan::ms_per_mib(m.lender_value),
            ),
            MoveKind::Reclaim => format!(
                "back to its plan, below which a MiB more saves {:.3} ms of fills an hour, \
                 against {:.3} a MiB serves where it was borrowed",
                cache_plan::ms_per_mib(m.borrower_value),
                cache_plan::ms_per_mib(m.lender_value),
            ),
            MoveKind::ReclaimPrewarmed => format!(
                "back to its plan, below which a MiB more saves {:.3} ms of fills an hour, \
                 from what an idle cache holds above its floor of what its engines fill ahead, \
                 against {:.3} a MiB serves there",
                cache_plan::ms_per_mib(m.borrower_value),
                cache_plan::ms_per_mib(m.lender_value),
            ),
            MoveKind::Decay => "idle: down toward what it holds plus 10 %".to_string(),
            MoveKind::Return => {
                "read again: what it lent the pool, back at the restore's pace".to_string()
            }
            MoveKind::Recall => {
                "read again: what tier 2 took from it, back at the restore's pace".to_string()
            }
        };
        let _ = write!(
            out,
            "\n  {} → {}: {} ({}); {reason}",
            key_name(&m.lender),
            key_name(&m.borrower),
            mib(m.bytes),
            m.kind.as_str(),
        );
    }
    out
}

/// What a take-back did ([`take_back`]).
#[derive(Debug, Default)]
struct TakeBack {
    /// The borrowers that gave back some of what they borrowed.
    taken: Vec<Change>,
    /// The borrowers whose shrink in place was refused: their loans stay.
    refused: Vec<Change>,
    /// What the shrinks freed of what the caches held.
    freed: u64,
}

/// Take back what elastic memory lent (#1314), the valve's first relief,
/// only as much as covers `excess` (resident over the total): each
/// borrower, the lowest-valued first, shrinks through `resize` by the part
/// of the excess still uncovered, in whole MiB, plus the capacity above
/// what it holds, which frees nothing, at most what it borrowed and never
/// below its floor; then gives back to the pool what its shrink actually
/// took off ([`Ledger::release_part`]), its lenders staying where they are.
/// A refused shrink gives back nothing and keeps its loans.
fn take_back(
    rows: &[Row],
    plan: &grant::Plan,
    with_ledger: &WithLedger<'_>,
    excess: u64,
    resize: &dyn Fn(&Entry, u64) -> Option<u64>,
) -> TakeBack {
    let mut ledger = Ledger::default();
    with_ledger(&mut |current| ledger = current.clone());
    let values = elastic_values(rows, plan, &ledger);
    let value = |key: &Key| values.get(key).copied().unwrap_or(0.0);
    let mut borrowers: Vec<Key> = ledger
        .keys()
        .into_iter()
        .filter(|key| ledger.borrowed(key) > 0)
        .collect();
    borrowers.sort_by(|a, b| value(a).total_cmp(&value(b)));
    let mut done = TakeBack::default();
    for key in borrowers {
        if done.freed >= excess {
            break;
        }
        let Some(row) = rows.iter().find(|row| decide::key(&row.entry) == key) else {
            continue;
        };
        let floor = plan
            .grant(&row.entry)
            .map_or(0, |g| floor_of(&row.entry, g))
            .max(keep_of(row));
        let (from, used) = (row.usage.capacity_bytes, row.usage.used_bytes);
        let need = (excess - done.freed).saturating_add(from.saturating_sub(used));
        let release = need
            .div_ceil(MIB)
            .saturating_mul(MIB)
            .min(ledger.borrowed(&key));
        let target = from.saturating_sub(release).max(floor).max(row.least_bytes);
        if target >= from {
            continue;
        }
        let to = resize(&row.entry, target).unwrap_or(from);
        let given = from.saturating_sub(to);
        if given > 0 {
            with_ledger(&mut |ledger| {
                ledger.release_part(&key, given, &value);
            });
        }
        done.freed += used.min(from).saturating_sub(used.min(to));
        let change = Change {
            entry: row.entry.clone(),
            from_bytes: from,
            to_bytes: to,
            target_bytes: target,
            reason: "memory pressure: what it borrowed, given back".into(),
            held: (to != target).then_some(Held::Refused),
            cleared: false,
        };
        if given > 0 {
            done.taken.push(change);
        } else {
            done.refused.push(change);
        }
    }
    done
}

/// What a relief did ([`relief`]).
#[derive(Debug, Default)]
struct Relief {
    /// The borrowers that gave back what elastic memory lent them.
    taken: Vec<Change>,
    /// The caches lowered to 90 % of what they hold.
    changes: Vec<Change>,
    /// The caches that refused their shrink in place.
    refused: Vec<Change>,
    /// The ceilings of the lowered caches: what each reached.
    caps: Vec<(Key, Ceiling)>,
}

/// The valve's relief over `rows` and the last `plan`, `excess` bytes
/// resident over the total, resizing through `resize`. Elastic memory's
/// loans go first (#1314), under `mode = "apply"`: what caches borrowed goes
/// back to the pool, the lowest-valued borrower first ([`take_back`]), and
/// when that covers the excess, nothing else is lowered. Otherwise every
/// other flexible cache is lowered to 90 % of what it holds, its loans end
/// ([`Ledger::end`]), and what it reached becomes its ceiling: its restore
/// climbs to its plan, and it borrows again only by elastic memory's steps.
fn relief(
    rows: &[Row],
    plan: Option<&grant::Plan>,
    with_ledger: &WithLedger<'_>,
    mode: ElasticMode,
    excess: u64,
    resize: &dyn Fn(&Entry, u64) -> Option<u64>,
) -> Relief {
    let back = match (mode, plan) {
        (ElasticMode::Apply, Some(plan)) => take_back(rows, plan, with_ledger, excess, resize),
        _ => TakeBack::default(),
    };
    let enough = !back.taken.is_empty() && back.freed >= excess;
    // A borrower that gave back, or refused to, is not lowered again.
    let taken_keys: HashSet<Key> = back
        .taken
        .iter()
        .chain(&back.refused)
        .map(|c| decide::key(&c.entry))
        .collect();
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
                    && grant.is_some_and(|g| !matches!(g.reason, Reason::Unreachable(_)))
                    && !enough
                    && !taken_keys.contains(&decide::key(&row.entry)),
            }
        })
        .collect();
    let mut relief = Relief {
        taken: back.taken,
        refused: back.refused,
        ..Relief::default()
    };
    for (key, target) in decide::relieve(&slots) {
        let Some(slot) = slots.iter().find(|s| decide::key(&s.entry) == key) else {
            continue;
        };
        // An owner gone first: nothing to lower, nothing to say.
        let Some(to) = resize(&slot.entry, target) else {
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
            with_ledger(&mut |ledger| ledger.end(&key));
            relief.caps.push((
                key,
                Ceiling {
                    bytes: to,
                    source: slot.entry.source.clone(),
                },
            ));
            relief.changes.push(change);
        } else {
            relief.refused.push(change);
        }
    }
    relief
}

/// The valve at `now`: the process holds `resident` bytes, above its
/// `total` for a minute. Take back what elastic memory lent and, unless
/// that covers the excess, lower every other flexible cache to 90 % of what
/// it holds, keeping what each reached as its ceiling ([`relief`]); purge
/// every arena's dirty pages. A cache that refuses its shrink in place keeps no ceiling,
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
    let with_ledger = |book: &mut dyn FnMut(&mut Ledger)| book(&mut governor().ledger);
    let Relief {
        taken,
        changes,
        refused,
        caps,
    } = relief(
        &rows,
        plan,
        &with_ledger,
        elastic_mode(state),
        resident.saturating_sub(total),
        &ds_cache::plan::apply,
    );
    {
        let mut governor = governor();
        governor.caps.extend(caps);
        // A take-back holds borrowing off until the restore's 10 minutes
        // under 90 % of the total have passed (#1314): a daily peak must
        // not take back and lend the same memory each day.
        if !taken.is_empty() {
            governor.borrow_held = true;
        }
        show_offsets(&governor.ledger);
    }
    let due =
        |at: Option<Instant>, every| at.is_none_or(|at| now.saturating_duration_since(at) >= every);
    if changes.is_empty() && taken.is_empty() {
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
    memory_plan::count_changes(&taken);
    memory_plan::count_changes(&changes);
    for change in &taken {
        ELASTIC_MOVES
            .with_label_values(&[change.entry.id, "give_back", "applied"])
            .inc();
    }
    PRESSURE_ACTIONS.with_label_values(&["relieve"]).inc();
    let purged = purge();
    if purged {
        PRESSURE_ACTIONS.with_label_values(&["purge"]).inc();
    }
    let mut governor = governor();
    governor.purged_at = Some(now);
    let lowered: BTreeSet<Key> = taken
        .iter()
        .chain(&changes)
        .map(|c| decide::key(&c.entry))
        .collect();
    let block = relief_block(resident, total, &taken, &changes, &refused, purged);
    if governor.lowered.as_ref() == Some(&lowered) {
        info!("{block}");
    } else {
        warn!("{block}");
    }
    governor.lowered = Some(lowered);
    governor.last_relief = Some(Record {
        at: chrono::Utc::now(),
        changes: taken.into_iter().chain(changes).collect(),
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
    taken: &[Change],
    changes: &[Change],
    refused: &[Change],
    purged: bool,
) -> String {
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    let mut out = format!(
        "Memory pressure: jemalloc holds {} resident, above the total of {} for a minute: ",
        mib(resident),
        mib(total),
    );
    if !taken.is_empty() {
        let _ = write!(
            out,
            "{} cache{} gave back what elastic memory lent {} (#1314)",
            taken.len(),
            plural(taken.len()),
            if taken.len() == 1 { "it" } else { "them" },
        );
        if !changes.is_empty() {
            out.push_str(", ");
        }
    }
    if !changes.is_empty() || taken.is_empty() {
        let _ = write!(
            out,
            "{} cache{} lowered to 90 % of what {} hold{}",
            changes.len(),
            plural(changes.len()),
            if changes.len() == 1 { "it" } else { "they" },
            if changes.len() == 1 { "s" } else { "" },
        );
    }
    out.push_str(if purged {
        ", dirty pages purged"
    } else {
        ", dirty pages not purged (jemalloc refused)"
    });
    memory_plan::write_changes(&mut out, taken);
    memory_plan::write_changes(&mut out, changes);
    if !changes.is_empty() {
        out.push_str(
            "\n  They stay at most this large until the process is under 90 % of the total for \
             10 minutes, then grow back by 5 % of their grant every 5 minutes",
        );
    }
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
             above the [memory] total for a minute took back what elastic memory lent (#1314) \
             and, unless that covered the excess, lowered the other flexible caches to 90 % of \
             what they hold; purge, it purged every arena's dirty pages; restore, a re-plan tick \
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
             saves the most fill time, from the pool's free headroom or a cache; lend, a cache \
             whose MiB serves at most half as much; reclaim, a cache below its grant taking \
             back what a borrower holds, at an even ratio; repay, that borrower; prewarmed, an \
             idle cache filled ahead of reads it then takes from, above its keep floor; return, \
             a cache read again taking back what it lent the pool, or a cache filled ahead of \
             reads what tier 2 took, at the restore's pace; decay, \
             an idle cache giving the pool what it does not hold; give_back, a borrower the \
             pressure valve made give back what it borrowed, as much as the excess. mode = report ([memory] elastic \
             = \"report\"): proposed, never applied, counted once while the same move recurs \
             each tick (cache_elastic_proposed_bytes shows the current ones); applied \
             (elastic = \"apply\"): made, counted each time. Per-collection caches share their \
             cache label; /admin/cache-plan has each collection's. Only in [memory] mode = \
             \"auto\", never while the pressure valve holds",
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
            "Bytes the last re-plan tick's elastic memory moves took or would take from (role \
             = lend, repay, prewarmed or decay) or give to (role = borrow, reclaim or return) \
             each cache (#1314), summed over its collections; none with [memory] elastic = \
             \"off\"",
        ),
        &["cache", "role"],
    )
    .unwrap();
    crate::admin::REGISTRY
        .register(Box::new(gauge.clone()))
        .unwrap();
    gauge
});

static ELASTIC_OFFSETS: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    let gauge = IntGaugeVec::new(
        Opts::new(
            "cache_elastic_offset_bytes",
            "What each cache's loans in elastic memory's ledger add up to (#1314): role = \
             borrowed, what it holds above its grant from other caches or the pool; lent, what \
             it gave other caches or the pool below its grant, its decays included. Its offset \
             is borrowed less lent. Summed over its collections; none outside [memory] elastic \
             = \"apply\"",
        ),
        &["cache", "role"],
    )
    .unwrap();
    crate::admin::REGISTRY
        .register(Box::new(gauge.clone()))
        .unwrap();
    gauge
});

/// Each cache's `borrowed` and `lent` bytes in `ledger`, summed over its
/// collections: what [`show_offsets`] sets, none for a cache without.
fn offset_sums(ledger: &Ledger) -> HashMap<(&'static str, &'static str), u64> {
    let mut sums = HashMap::new();
    for key in ledger.keys() {
        for (role, bytes) in [
            ("borrowed", ledger.borrowed(&key)),
            ("lent", ledger.lent(&key)),
        ] {
            if bytes > 0 {
                *sums.entry((key.1, role)).or_default() += bytes;
            }
        }
    }
    sums
}

/// Show `ledger`'s loans on `/metrics` (`cache_elastic_offset_bytes`),
/// each cache's borrowed and lent bytes summed over its collections.
fn show_offsets(ledger: &Ledger) {
    let sums = offset_sums(ledger);
    ELASTIC_OFFSETS.reset();
    for ((cache, role), bytes) in sums {
        ELASTIC_OFFSETS
            .with_label_values(&[cache, role])
            .set(bytes.min(i64::MAX as u64) as i64);
    }
}

/// Register the governor's counters, so `/metrics` lists them before the
/// first change.
pub(crate) fn init_counters() {
    LazyLock::force(&PRESSURE_ACTIONS);
    LazyLock::force(&ELASTIC_MOVES);
    LazyLock::force(&ELASTIC_PROPOSED);
    LazyLock::force(&ELASTIC_OFFSETS);
    memory_plan::init_change_counter();
}

/// The governor for `/admin/cache-plan`: the last re-plan tick, the
/// valve's state and last actions, its ceilings, and elastic memory: what
/// `[memory] elastic` does now (`elastic_mode`) and what the last tick
/// weighed and moved.
pub(crate) fn json(state: &AdminState) -> Value {
    let mode = elastic_mode(state);
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
        "elastic_mode": mode.as_str(),
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
    let name = |key: &Option<Key>| match key {
        Some((_, id, scope)) => json!({"id": id, "collection": scope.collection()}),
        None => json!("pool"),
    };
    let value = |v: f64| (cache_plan::ms_per_mib(v) * 1e4).round() / 1e4;
    let depth = |d: usize| ds_cache::GHOST_DEPTHS[d];
    let side = |party: &Option<Party>| name(&party.as_ref().map(|p| p.key.clone()));
    let loans: Vec<Value> = ledger
        .loans()
        .iter()
        .map(|loan| {
            json!({
                "borrower": side(&loan.borrower),
                "lender": side(&loan.lender),
                "bytes": loan.bytes,
            })
        })
        .collect();
    let offsets: Vec<Value> = ledger
        .keys()
        .into_iter()
        .map(|key| {
            let mut entry = name(&Some(key.clone()));
            entry["offset_bytes"] = json!(ledger.offset(&key));
            entry
        })
        .collect();
    json!({
        "mode": proposal.mode.as_str(),
        "at": proposal.at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "held": proposal.held,
        "may_borrow": proposal.may_borrow,
        "may_return": proposal.may_return,
        "step_bytes": proposal.step_bytes,
        "headroom_bytes": proposal.headroom_bytes,
        "moves": proposal.moves.iter().map(|m| json!({
            "kind": m.kind.as_str(),
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
            "grant_bytes": c.grant,
            "offset_bytes": c.offset,
            "floor_bytes": c.floor,
            "ceiling_bytes": c.ceiling,
            "used_bytes": c.used,
            "keep_bytes": c.keep,
            "idle": c.idle,
            "ahead": c.ahead,
            "ghost_hits": c.ghost_hits,
            "depth": depth(c.depth),
            "value_ms_per_mib_hour": value(c.value),
            "lend_value_ms_per_mib_hour": value(c.lend_value),
            "borrows": c.borrows,
            "lends": c.lends,
        })).collect::<Vec<_>>(),
        "ledger": {"loans": loans, "offsets": offsets},
    })
}

#[cfg(test)]
mod tests;
