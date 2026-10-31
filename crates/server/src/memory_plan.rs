//! The memory plan (#1312): every cache and admission budget fitted to the
//! memory the process may use, shown next to its configured size. For now
//! a report: nothing is resized.
//!
//! The total comes from [`detect`]: `[memory] total_mb`, else a cgroup
//! limit, else the host's memory, read once at boot ([`read_at_boot`]).
//! The plan is `ds_cache::plan::grant::plan` over the rows of
//! `cache_plan::snapshot`, with the memory outside the caches measured
//! where `/metrics` can read it, and with what no collection can reach
//! ([`unreachable`]). `cache_plan::log` computes it at boot and after every
//! accepted reload ([`replan`]); `/admin/cache-plan` and `/metrics` show
//! the last one ([`last`], [`update_gauges`]).

pub(crate) mod detect;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

use ds_cache::plan::grant::{self, Baseline, Grant, Inputs, Plan, Reason};
use ds_cache::plan::{Kind, Row, Scope, SizeSource};
use ds_core::config::CollectionConfig;
use prometheus::{IntGauge, IntGaugeVec, Opts};
use serde_json::{json, Value};
use tracing::warn;

use crate::admin::{MemoryReading, ServerState};
use crate::cache_plan::mib;

pub(crate) use detect::{gib, Total};

/// The memory facts, read once.
static READING: OnceLock<detect::Reading> = OnceLock::new();

/// Read the memory facts: the cgroup limit, and the host's memory with
/// what is available at boot. `main` calls this before the first load;
/// without it (tests), the first plan does.
pub(crate) fn read_at_boot() -> &'static detect::Reading {
    READING.get_or_init(|| detect::read(Path::new("/")))
}

/// A plan and the total it was fitted to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Report {
    /// `None` when neither `[memory] total_mb` nor the system gives one.
    pub total: Option<Total>,
    /// The cgroup's `memory.low`: reported, not used.
    pub low_bytes: Option<u64>,
    pub plan: Option<Plan>,
}

static LAST: Mutex<Option<Arc<Report>>> = Mutex::new(None);

/// The report of the last boot or accepted reload.
pub(crate) fn last() -> Option<Arc<Report>> {
    LAST.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The store gauges (#1307), whose sum the plan reserves as measured heap:
/// the families the dashboard's Memory outside caches panel stacks as
/// "stores".
const STORE_FAMILIES: &[&str] = &[
    "bufr_store_bytes",
    "bufr_snapshot_bytes",
    "bufr_deletion_index_bytes",
    "wis2_dedup_bytes",
    "cap_alert_bytes",
    "nowcast_frame_bytes",
    "pvol_catalog_bytes",
    "netcdf_open_file_bytes",
];

/// Fit `rows` to the accepted config's total and keep the report. `memory`
/// is what the `/metrics` refresh before it read.
pub(crate) fn replan(state: &ServerState, rows: &[Row], memory: &MemoryReading) -> Arc<Report> {
    let baseline = baseline(memory);
    let configured = state
        .accepted_load
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|accepted| accepted.memory_total_mb());
    let collections = state
        .last_collections
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let report = Arc::new(report(
        configured,
        read_at_boot(),
        rows,
        &collections,
        baseline,
    ));
    *LAST.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&report));
    report
}

/// The plan of `rows` for `collections`, its total from `configured_mb`
/// and `reading`.
pub(crate) fn report(
    configured_mb: Option<u64>,
    reading: &detect::Reading,
    rows: &[Row],
    collections: &HashMap<String, CollectionConfig>,
    baseline: Baseline,
) -> Report {
    let total = detect::total(configured_mb, reading);
    let plan = total.as_ref().map(|total| {
        grant::plan(&Inputs::of_rows(
            total.bytes,
            collections.len() as u64,
            baseline,
            rows,
            |row| unreachable(row, collections),
        ))
    });
    Report {
        total,
        low_bytes: reading.low_bytes,
        plan,
    }
}

/// The memory outside the caches: measured from resident memory, jemalloc
/// and the store gauges where `/metrics` reads them (RSS on Linux),
/// estimated elsewhere.
fn baseline(memory: &MemoryReading) -> Baseline {
    let read = |name: &str| {
        memory
            .0
            .iter()
            .find(|(n, _)| *n == name)
            .and_then(|(_, v)| *v)
    };
    match (
        read("process_resident_memory_bytes"),
        read("jemalloc_resident_bytes"),
    ) {
        (Some(resident), Some(jemalloc)) => Baseline::Measured {
            outside_bytes: resident.saturating_sub(jemalloc),
            store_bytes: store_bytes(),
        },
        _ => Baseline::Estimated,
    }
}

/// The store gauges summed, as `/metrics` last set them.
fn store_bytes() -> u64 {
    crate::admin::REGISTRY
        .gather()
        .iter()
        .filter(|family| STORE_FAMILIES.contains(&family.name()))
        .flat_map(|family| family.get_metric())
        .map(|metric| metric.get_gauge().get_value().max(0.0) as u64)
        .fold(0u64, u64::saturating_add)
}

/// Whether a GRIB source reads local files, through the page cache: its
/// message cache would hold nothing a read needs.
fn local_grib(collection: &CollectionConfig) -> bool {
    collection.grib.as_ref().is_some_and(|grib| {
        grib.endpoint.is_none()
            && grib.data_path.as_deref().is_none_or(|path| {
                let lower = path.to_ascii_lowercase();
                !["s3://", "http://", "https://"]
                    .iter()
                    .any(|scheme| lower.starts_with(scheme))
            })
    })
}

/// Why no collection of `collections` can reach `row`'s cache or budget,
/// when none can (rule 2 of the plan).
pub(crate) fn unreachable(
    row: &Row,
    collections: &HashMap<String, CollectionConfig>,
) -> Option<String> {
    let entry = &row.entry;
    let none = |why: &str, reaches: &dyn Fn(&CollectionConfig) -> bool| {
        (!collections.values().any(reaches)).then(|| why.to_string())
    };
    let serves = |c: &CollectionConfig, apis: &[&str]| c.apis.iter().any(|a| apis.contains(&&**a));
    // Built empty for its collection on purpose: a local GeoTIFF's tile
    // cache, a plain Zarr store's decoded-chunk cache.
    if entry.kind == Kind::Cache
        && entry.source == SizeSource::Constant
        && row.usage.capacity_bytes == 0
    {
        return Some("built empty for its source".into());
    }
    match (entry.id, &entry.scope) {
        ("grib_message_cache", Scope::Source { id, .. }) => collections
            .get(id)
            .is_some_and(local_grib)
            .then(|| "a local source reads through the page cache".into()),
        ("tiles3d_content_cache" | "pvol_voxel_grid_cache", _) => {
            none("no 3dtiles collection", &|c| serves(c, &["3dtiles"]))
        }
        ("metatile_cache", _) => none("no wms collection", &|c| serves(c, &["wms"])),
        ("rendered_cache" | "solid_image_cache", _) => none("no map collection", &|c| {
            serves(c, &["wms", "maps", "tiles"])
        }),
        ("render_budget", _) => none("no map or 3dtiles collection", &|c| {
            serves(c, &["wms", "maps", "tiles", "3dtiles"])
        }),
        ("vector_tile_cache", _) => none("no vector tile collection", &|c| {
            serves(c, &["tiles"]) && serves(c, &["features"])
        }),
        ("station_frame_cache", _) => none("no station symbol layer", &|c| {
            matches!(c.engine_type.as_str(), "csv" | "bufr") && serves(c, &["wms", "maps", "tiles"])
        }),
        ("edr_response", _) => none("no edr collection", &|c| serves(c, &["edr"])),
        (_, Scope::Engine(engine)) => none(&format!("no {engine} collection"), &|c| {
            c.engine_type == *engine
        }),
        _ => None,
    }
}

/// The plan's header for `/admin/cache-plan`: the total and its source,
/// the reserves, the pool and the sums it was fitted with; `null` without
/// a total.
pub(crate) fn header_json(report: &Report) -> Value {
    let (Some(total), Some(plan)) = (&report.total, &report.plan) else {
        return Value::Null;
    };
    let r = &plan.reserves;
    json!({
        "total_bytes": total.bytes,
        "total_source": total.source.to_string(),
        "cgroup_low_bytes": report.low_bytes,
        "reserves": {
            "outside_bytes": r.outside_bytes,
            "heap_bytes": r.heap_bytes,
            "baseline": if r.measured { "measured" } else { "estimated" },
            "render_budget_bytes": r.render_bytes,
            "read_budget_bytes": r.read_bytes,
            "read_budget": r.read_budget,
            "slack_bytes": r.slack_bytes,
        },
        "pool_bytes": plan.pool_bytes,
        "expected_bytes": plan.expected_bytes,
        "demand_bytes": plan.demand_bytes,
        "floor_bytes": plan.floor_bytes,
        "pinned_bytes": plan.pinned_bytes,
        "shortfall_bytes": plan.shortfall_bytes,
        "cut": plan.cut,
    })
}

/// The grant of `row`'s entry in `report`.
pub(crate) fn grant<'a>(report: Option<&'a Report>, row: &Row) -> Option<&'a Grant> {
    report?.plan.as_ref()?.grant(&row.entry)
}

/// The log block's line on the total, the reserves and the pool.
pub(crate) fn header_line(report: &Report) -> String {
    let (Some(total), Some(plan)) = (&report.total, &report.plan) else {
        return "memory total unknown: set [memory] total_mb for a plan".into();
    };
    let r = &plan.reserves;
    let mut line = format!(
        "memory total {} ({}): {} outside the caches ({}), render budget {}, read budget {}",
        gib(total.bytes),
        total.source,
        gib(r.baseline_bytes()),
        if r.measured { "measured" } else { "estimated" },
        gib(r.render_bytes),
        gib(r.read_bytes),
    );
    if let Some(budget) = r.read_budget {
        let _ = write!(line, " ({budget})");
    }
    let _ = write!(
        line,
        ", slack {}; caches share {}, expect {} (demand {}, floors {}, pins {})",
        gib(r.slack_bytes),
        gib(plan.pool_bytes),
        gib(plan.expected_bytes),
        gib(plan.demand_bytes),
        gib(plan.floor_bytes),
        gib(plan.pinned_bytes),
    );
    if let Some(low) = report.low_bytes {
        let _ = write!(line, "; cgroup memory.low {} (not a limit)", gib(low));
    }
    line.push_str(". A report: no size is changed");
    line
}

/// A log line's suffix: its planned size and why.
pub(crate) fn line_suffix(grant: &Grant) -> String {
    format!(", planned {} ({})", mib(grant.bytes), grant.reason)
}

/// The most collections the WARN block names by their demand.
const LARGEST_DEMANDS: usize = 5;

/// The WARN block of a plan that cuts caches or falls short: the total and
/// its source, the sums, each cache and budget cut (configured → planned),
/// and the collections with the largest demand. `None` when it fits.
pub(crate) fn warn_block(report: &Report, rows: &[Row]) -> Option<String> {
    let (total, plan) = (report.total.as_ref()?, report.plan.as_ref()?);
    let floors = plan
        .caches
        .iter()
        .chain(&plan.budgets)
        .any(|g| g.reason == Reason::Floor);
    if !plan.cut && !floors && plan.shortfall_bytes == 0 {
        return None;
    }
    let mut out = format!(
        "Memory plan: a total of {} ({}) does not hold every cache and budget at its size \
         today: demand {}, floors {}, pins {}",
        gib(total.bytes),
        total.source,
        gib(plan.demand_bytes),
        gib(plan.floor_bytes),
        gib(plan.pinned_bytes),
    );
    if plan.shortfall_bytes > 0 {
        let _ = write!(
            out,
            ", short by {} even with those cut to their floors",
            gib(plan.shortfall_bytes)
        );
    }
    if matches!(total.source, detect::Source::Host { .. }) {
        out.push_str(
            ". No memory limit was found, so the total is a share of the host's memory: set \
             [memory] total_mb to the memory this server may use",
        );
    }
    for g in plan.caches.iter().chain(&plan.budgets) {
        if matches!(g.reason, Reason::Cut(_) | Reason::Floor) && g.bytes < g.capacity_bytes {
            let _ = write!(out, "\n  {}", g.entry.id);
            if let Some(collection) = g.entry.scope.collection() {
                let _ = write!(out, " {collection}");
            }
            let _ = write!(
                out,
                " {} → {} ({})",
                mib(g.capacity_bytes),
                mib(g.bytes),
                g.reason
            );
        }
    }
    let mut demands: HashMap<&str, u64> = HashMap::new();
    for row in rows {
        for (collection, part) in row.demand.iter().flat_map(|d| &d.parts) {
            *demands.entry(collection).or_default() += part.bytes;
        }
    }
    let mut demands: Vec<(&str, u64)> = demands.into_iter().filter(|(_, b)| *b > 0).collect();
    demands.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    if !demands.is_empty() {
        out.push_str("\n  largest demand:");
        for (i, (collection, bytes)) in demands.iter().take(LARGEST_DEMANDS).enumerate() {
            let _ = write!(
                out,
                "{} {collection} {}",
                if i == 0 { "" } else { "," },
                mib(*bytes)
            );
        }
    }
    out.push_str(
        "\n  A report: no size is changed. [memory] total_mb sets the total; a pinned size is \
         kept",
    );
    Some(out)
}

/// The total last warned about.
static WARNED: Mutex<Option<Total>> = Mutex::new(None);

/// Log `report`'s WARNs: the total's, when it changed, and its cut block.
pub(crate) fn log_warnings(report: &Report, rows: &[Row]) {
    let mut warned = WARNED.lock().unwrap_or_else(|e| e.into_inner());
    if warned.as_ref() != report.total.as_ref() {
        match &report.total {
            Some(total) => total.warnings.iter().for_each(|w| warn!("{w}")),
            None => warn!(
                "The memory this server may use is unknown: set [memory] total_mb for a \
                 memory plan"
            ),
        }
        warned.clone_from(&report.total);
    }
    if let Some(block) = warn_block(report, rows) {
        warn!("{block}");
    }
}

static PLAN_TOTAL: LazyLock<IntGauge> = LazyLock::new(|| {
    gauge(
        "memory_plan_total_bytes",
        "The memory the plan fits every cache and budget into: [memory] total_mb, else 80 % of \
         a cgroup limit, else the smaller of 50 % of MemTotal and 80 % of MemAvailable at boot. \
         0 when unknown",
    )
});

static PLAN_POOL: LazyLock<IntGauge> = LazyLock::new(|| {
    gauge(
        "memory_plan_pool_bytes",
        "What the caches share of memory_plan_total_bytes after the reserves: memory outside \
         the caches, the render budget, the largest read budget and allocator slack",
    )
});

static PLAN_SHORTFALL: LazyLock<IntGauge> = LazyLock::new(|| {
    gauge(
        "memory_plan_shortfall_bytes",
        "How much more memory the plan needs with its pins kept and every other cache and \
         budget cut to its floor; 0 when it fits. A report: no size is changed",
    )
});

static CACHE_PLANNED: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    crate::admin::per_collection(
        IntGaugeVec::new(
            Opts::new(
                "cache_planned_bytes",
                "The size the memory plan gives each cache and admission budget of \
                 /admin/cache-plan, labelled as cache_size_source is. A report: the configured \
                 size is unchanged",
            ),
            &["cache", "collection"],
        )
        .unwrap(),
    )
});

fn gauge(name: &str, help: &str) -> IntGauge {
    let gauge = IntGauge::new(name, help).unwrap();
    crate::admin::REGISTRY
        .register(Box::new(gauge.clone()))
        .unwrap();
    gauge
}

fn clamp(bytes: u64) -> i64 {
    bytes.min(i64::MAX as u64) as i64
}

/// Set the plan gauges from the last report, per `/metrics` scrape.
pub(crate) fn update_gauges() {
    let report = last();
    let plan = report.as_ref().and_then(|r| r.plan.as_ref());
    let total = report.as_ref().and_then(|r| r.total.as_ref());
    PLAN_TOTAL.set(clamp(total.map_or(0, |t| t.bytes)));
    PLAN_POOL.set(clamp(plan.map_or(0, |p| p.pool_bytes)));
    PLAN_SHORTFALL.set(clamp(plan.map_or(0, |p| p.shortfall_bytes)));
    CACHE_PLANNED.reset();
    for g in plan.iter().flat_map(|p| p.caches.iter().chain(&p.budgets)) {
        let collection = g.entry.scope.collection().unwrap_or("");
        CACHE_PLANNED
            .with_label_values(&[g.entry.id, collection])
            .set(clamp(g.bytes));
    }
}

#[cfg(test)]
mod tests;
