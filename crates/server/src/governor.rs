//! The memory governor (#1315): one task on `poll_runtime` that keeps the
//! memory plan fitted between reloads and holds the process to its total.
//! #1314's elastic sharing is meant to be one more step of the same loop.
//!
//! - **Re-plan tick**, every [`decide::REPLAN_EVERY`]: fit the plan again
//!   with the engines' current demands (`memory_plan::fit`) and apply a
//!   grant only where it moves a capacity by more than 10 % and 64 MiB
//!   ([`decide::tick_target`]). The plan's `Least` holds a warm cache to a
//!   quarter of what it holds per tick and never plans a shrink below what
//!   the cache takes in place; a resize that stops short anyway is not
//!   tried again while its capacity and grant stay as they were. Changes
//!   are logged at INFO, the plan's WARN block only when what it cuts
//!   changes (`memory_plan::log_new_warnings`).
//! - **Pressure valve**, sampled every [`decide::SAMPLE_EVERY`]:
//!   `jemalloc_resident_bytes` above the total for a minute lowers every
//!   flexible cache to 90 % of what it holds, never below the plan's floor
//!   for it nor what it takes in place, and keeps that as its ceiling;
//!   every arena's dirty pages are purged, and a WARN says so. Under 90 %
//!   of the total for 10 minutes, each re-plan tick raises the ceilings by
//!   5 % of the grant, until the plan has the caches again. A cache its
//!   engines fill ahead of requests (satellite scans, PVOL pixels, station
//!   grids) is never lowered, nor a budget.
//!
//! Gates: everything runs where `AcceptedLoad::plans_sizes` holds, `auto`
//! with a total; the valve only where that total is `[memory] total_mb` or
//! a cgroup limit's share (`memory_plan::valve_total`), never a share of a
//! shared host's memory. A pin (`[memory.caches]`, a variable, a collection
//! key) is never moved; a range moves within its bounds, which the plan's
//! grants and floors respect. Every resize goes through
//! `ds_cache::plan::apply`, so it is the plan's: report mode gives it back.
//!
//! Each step takes `reload_lock` with `try_lock` and skips when a reload
//! holds it, as `radar_recovery_loop` does: it never runs between a
//! reload's staged `[memory.caches]` and their `set`, nor while a reload
//! applies its plan. An accepted reload fits and applies the plan itself,
//! keeping the valve's ceilings (`capped`), restarts the tick's 5 minutes
//! and forgets the resizes that stopped short ([`fitted`]); the pressure
//! timers and ceilings carry over, since the process's memory does not
//! change with its config. Where the gates close (report mode, no total,
//! a total from the host), the valve's state is dropped: report mode gives
//! every size the plan set back.

pub(crate) mod decide;

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

use ds_cache::plan::grant::{self, Reason};
use ds_cache::plan::Row;
use prometheus::{IntCounterVec, Opts};
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::admin::AdminState;
use crate::cache_plan::{self, mib};
use crate::memory_plan::{self, Change, Held};
use decide::{Key, Pressure, Signal, Slot};

/// What the governor did at a time, for `/admin/cache-plan`.
#[derive(Debug, Clone)]
struct Record {
    at: chrono::DateTime<chrono::Utc>,
    changes: Vec<Change>,
}

/// The governor's state between steps.
#[derive(Debug, Default)]
struct Governor {
    /// When the plan was last fitted, by a tick or a reload.
    fitted_at: Option<Instant>,
    pressure: Pressure,
    /// The valve's ceilings, by cache.
    caps: HashMap<Key, u64>,
    /// The resizes that stopped short at the last tick: the capacity each
    /// stopped at and the grant it was given.
    refused: HashMap<Key, (u64, u64)>,
    /// The last sample: resident bytes and the valve's total.
    sample: Option<(u64, u64)>,
    last_tick: Option<Record>,
    last_relief: Option<Record>,
    last_restore: Option<Record>,
}

static GOVERNOR: LazyLock<Mutex<Governor>> = LazyLock::new(Default::default);

fn governor() -> std::sync::MutexGuard<'static, Governor> {
    GOVERNOR.lock().unwrap_or_else(|e| e.into_inner())
}

/// `grant` capped by the valve's ceiling on `entry`, if it holds one: what
/// a reload's plan gives a cache the valve lowered.
pub(crate) fn capped(entry: &ds_cache::plan::Entry, grant: u64) -> u64 {
    governor()
        .caps
        .get(&decide::key(entry))
        .map_or(grant, |cap| grant.min(*cap))
}

/// The plan was fitted and applied at `now`, at boot or by a reload: the
/// next tick is 5 minutes later, and the resizes that stopped short are
/// tried again.
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

/// One step at `now`: skipped while a reload holds `reload_lock`.
pub(crate) fn step(state: &AdminState, now: Instant) -> Step {
    let Ok(_guard) = state.reload_lock.try_lock() else {
        return Step::Skipped;
    };
    let accepted = state
        .accepted_load
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let Some(accepted) = accepted.filter(|a| a.plans_sizes()) else {
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
    };
    let valve = memory_plan::valve_total(accepted.memory_mode(), accepted.memory_total_mb());
    let resident = valve.and_then(|_| resident_bytes());
    let mut relieved = false;
    match (valve, resident) {
        (Some(total), Some(resident)) => {
            let signal = {
                let mut governor = governor();
                governor.sample = Some((resident, total));
                governor.pressure.observe(now, resident, total)
            };
            if signal == Signal::Relieve {
                relieved = relieve(state, resident, total);
            }
        }
        _ => {
            let mut governor = governor();
            governor.sample = None;
            governor.pressure = Pressure::default();
            governor.caps.clear();
        }
    }
    let due = governor()
        .fitted_at
        .is_none_or(|at| now.saturating_duration_since(at) >= decide::REPLAN_EVERY);
    if due {
        tick(state, now);
    }
    Step::Ran {
        sampled: resident.is_some(),
        relieved,
        replanned: due,
    }
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
    // Copied out: applying calls back into `capped`.
    let (caps, refused, raised, lifted) = {
        let mut governor = governor();
        let (raised, lifted) = if governor.pressure.may_restore(now) && !governor.caps.is_empty() {
            decide::restore(&mut governor.caps, &grant_of)
        } else {
            (Vec::new(), HashSet::new())
        };
        (
            governor.caps.clone(),
            governor.refused.clone(),
            raised,
            lifted,
        )
    };
    let wanted = |row: &Row, grant: &grant::Grant| {
        let key = decide::key(&row.entry);
        decide::tick_target(
            row.usage.capacity_bytes,
            grant.bytes,
            caps.get(&key).copied(),
            lifted.contains(&key),
            refused.get(&key).copied(),
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
            if change.held == Some(Held::Refused) {
                if let Some(grant) = grant_of(&key) {
                    governor.refused.insert(key, (change.to_bytes, grant));
                }
            } else {
                governor.refused.remove(&key);
            }
        }
        governor.last_tick = Some(Record {
            at: chrono::Utc::now(),
            changes: changed.clone(),
        });
        if !raised.is_empty() || !lifted.is_empty() {
            let restored: Vec<Change> = changed
                .iter()
                .filter(|c| {
                    let key = decide::key(&c.entry);
                    raised.contains(&key) || lifted.contains(&key)
                })
                .cloned()
                .collect();
            PRESSURE_ACTIONS.with_label_values(&["restore"]).inc();
            governor.last_restore = Some(Record {
                at: chrono::Utc::now(),
                changes: restored,
            });
        }
    }
    if let Some(block) = tick_block(&changed, raised.len() + lifted.len(), lifted.len()) {
        info!("{block}");
    }
    let rows = cache_plan::snapshot(state);
    memory_plan::log_new_warnings(&report, &rows);
}

/// The valve: the process holds `resident` bytes, above its `total` for a
/// minute. Lower every flexible cache to 90 % of what it holds and keep
/// that as its ceiling, purge every arena's dirty pages, and WARN. Returns
/// whether anything was lowered or purged.
fn relieve(state: &AdminState, resident: u64, total: u64) -> bool {
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
                    .map_or(row.usage.capacity_bytes, |g| g.floor_bytes)
                    .max(row.least_bytes),
                flexible: !grant::pinned(&row.entry.source)
                    && !decide::spared(row)
                    && grant.is_some_and(|g| !matches!(g.reason, Reason::Unreachable(_))),
            }
        })
        .collect();
    let mut changes = Vec::new();
    for (key, target) in decide::relieve(&slots) {
        let Some(slot) = slots.iter().find(|s| decide::key(&s.entry) == key) else {
            continue;
        };
        governor().caps.insert(key, target);
        let applied = ds_cache::plan::apply(&slot.entry, target);
        let to = applied.unwrap_or(slot.capacity);
        changes.push(Change {
            entry: slot.entry.clone(),
            from_bytes: slot.capacity,
            to_bytes: to,
            target_bytes: target,
            reason: "memory pressure: 90 % of what it holds".into(),
            held: match applied {
                None => Some(Held::Gone),
                Some(to) if to != target => Some(Held::Refused),
                Some(_) => None,
            },
            cleared: false,
        });
    }
    memory_plan::count_changes(&changes);
    let purged = purge();
    if !changes.is_empty() {
        PRESSURE_ACTIONS.with_label_values(&["relieve"]).inc();
    }
    if purged {
        PRESSURE_ACTIONS.with_label_values(&["purge"]).inc();
    }
    warn!("{}", relief_block(resident, total, &changes, purged));
    governor().last_relief = Some(Record {
        at: chrono::Utc::now(),
        changes,
    });
    true
}

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
/// cache lowered, and whether the dirty pages were purged.
pub(crate) fn relief_block(resident: u64, total: u64, changes: &[Change], purged: bool) -> String {
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

/// Register the governor's counters, so `/metrics` lists them before the
/// first change.
pub(crate) fn init_counters() {
    LazyLock::force(&PRESSURE_ACTIONS);
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
        .map(|((kind, id, scope), bytes)| {
            json!({
                "id": id,
                "kind": kind.as_str(),
                "collection": scope.collection(),
                "ceiling_bytes": bytes,
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
    })
}

#[cfg(test)]
mod tests;
