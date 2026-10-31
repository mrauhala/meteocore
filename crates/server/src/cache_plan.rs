//! The effective cache and memory plan (#1306): every cache and admission
//! budget this process holds, its size, and where that size came from.
//!
//! The list is `ds_cache::plan`'s registry. Process-wide caches and budgets
//! enter it from their own crates when they are built, sized from their
//! `MC_*` variables. This module enters the rest, whose sizes come from the
//! config: the render output caches of each load ([`register_render_caches`])
//! and the per-collection caches, through the accessors `/metrics` reads
//! ([`register_geotiff`], [`register_grib`], [`register_zarr`]).
//!
//! It is shown three ways from one [`snapshot`]: `GET /admin/cache-plan`
//! ([`handler`]), one INFO block at boot and after each accepted reload
//! ([`log`]), and the `cache_size_source` and `memory_capacity_total_bytes`
//! gauges. The first two refresh the `/metrics` families first: reading a
//! process-wide cache's figures builds it, and so enters it, so all three
//! list what `/metrics` reports.
//!
//! The endpoint also says what each cache did over the last hour and
//! whether it needs memory (#1308): idle, thrashing, scan or healthy, with
//! the hit ratio a cache 1.5, 2 or 3 times as large would have had. The
//! hour is sampled by `/metrics` scrapes ([`ds_cache::plan::SAMPLE_EVERY`]).
//!
//! Where an engine publishes what a cache must hold for its data (#1309),
//! each listing carries that demand and the size it suggests, and every
//! [`snapshot`] logs a WARN for a cache that has just fallen below it
//! (`ds_cache::plan::demand`): at boot, after a reload that resized it, or
//! when a scrape is the first to see it. Nothing applies a suggestion.
//!
//! Each listing also carries the size the memory plan gives it (#1312,
//! `crate::memory_plan`), fitted at boot and after each accepted reload to
//! the memory the process may use. A report: nothing applies it either.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use ds_cache::plan::{self, Activity, Demand, Entry, Kind, Row, Scope, SizeSource, Usage};
use ds_cache::{Churn, MIB};
use ds_core::config::{
    CollectionConfig, GribConfig, ServerConfig, DEFAULT_GRID_CACHE_MB, DEFAULT_METATILE_CACHE_MB,
    DEFAULT_POINT_INDEX_CACHE_MB, DEFAULT_TILE_CACHE_MB, DEFAULT_ZARR_CACHE_MB,
};
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::admin::{AdminState, MemoryReading, ServerState};
use crate::memory_plan::{self, Report};

/// Where the meta-tile and rendered-image cache sizes came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RenderCacheSources {
    pub metatile: SizeSource,
    pub rendered: SizeSource,
}

impl RenderCacheSources {
    pub(crate) fn of(config: &ServerConfig) -> Self {
        let rendered = match config.rendered_cache_key() {
            Some(key @ "[server] rendered_cache_mb") => SizeSource::Config(key),
            Some(key) => SizeSource::Collection(key),
            None => SizeSource::Default,
        };
        RenderCacheSources {
            metatile: SizeSource::unless_default(
                config.server.metatile_cache_mb,
                DEFAULT_METATILE_CACHE_MB,
                SizeSource::Config("[server] metatile_cache_mb"),
            ),
            rendered,
        }
    }
}

/// Enter the live render output caches in the plan: the meta-tile and
/// rendered-image caches, sized by the accepted config, and the MVT cache
/// and solid-image memo, constants. Again after every accepted load: a
/// cache a reload keeps is entered again, with the source of the config
/// that kept it.
pub(crate) fn register_render_caches(state: &ServerState) {
    let Some(accepted) = state
        .accepted_load
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    else {
        return;
    };
    let sources = accepted.render_cache_sources();
    let wms = state.wms.load();
    wms.rendered_cache.register(sources.rendered.clone());
    wms.tile_cache.register(sources.metatile.clone());
    state
        .tiles
        .load()
        .vector_tile_cache
        .register(SizeSource::Constant);
    ds_render::init_solid_image_cache();
}

/// A GeoTIFF collection's compressed tile cache. A local collection reads
/// its files through the page cache and builds it empty, whatever
/// `tile_cache_mb` says.
pub(crate) fn register_geotiff(
    collection: &CollectionConfig,
    engine: &Arc<engine_geotiff::GeoTiffEngine>,
) {
    let source = if engine.reads_local_files() {
        SizeSource::Constant
    } else {
        SizeSource::unless_default(
            collection
                .geotiff
                .as_ref()
                .map_or(DEFAULT_TILE_CACHE_MB, |g| g.tile_cache_mb),
            DEFAULT_TILE_CACHE_MB,
            SizeSource::Collection("[geotiff] tile_cache_mb"),
        )
    };
    let scope = Scope::Collection {
        engine: "geotiff",
        id: collection.id.clone(),
    };
    plan::register(Entry::cache("tile_cache", scope, source), engine, |e| {
        let entries = e.tile_cache_utilization().2 as u64;
        Usage::cache(e.tile_cache_metrics(), Some(entries))
    });
}

/// A GRIB source's decoded-grid, compressed-message and point-index
/// caches, shared by its level collections. A size of 0 builds no cache;
/// it is listed at 0 all the same, as `/metrics` reports it.
pub(crate) fn register_grib(collection: &CollectionConfig, engine: &Arc<engine_grib::GribEngine>) {
    let Some(grib) = collection.grib.as_ref() else {
        return;
    };
    let scope = || Scope::Source {
        engine: "grib",
        id: collection.id.clone(),
    };
    let grid = SizeSource::unless_default(
        grib.grid_cache_mb,
        DEFAULT_GRID_CACHE_MB,
        SizeSource::Collection("[grib] grid_cache_mb"),
    );
    plan::register(Entry::cache("grid_cache", scope(), grid), engine, |e| {
        let entries = e.grid_cache_utilization().2 as u64;
        Usage::cache(e.grid_cache_metrics(), Some(entries))
    });
    let message = SizeSource::unless_default(
        grib.message_cache_mb,
        0,
        SizeSource::Collection("[grib] message_cache_mb"),
    );
    plan::register(
        Entry::cache("grib_message_cache", scope(), message),
        engine,
        |e| Usage::cache(e.message_cache_metrics(), None),
    );
    plan::register(
        Entry::cache("grib_point_index_cache", scope(), point_index_source(grib)),
        engine,
        |e| Usage::cache(e.point_index_cache_metrics(), None),
    );
}

/// Where a GRIB source's point-index cache size came from: its key when
/// set, whatever the value; unset, `message_cache_mb` when its share of it
/// is above the default floor (#1333).
fn point_index_source(grib: &GribConfig) -> SizeSource {
    match grib.point_index_cache_mb {
        Some(_) => SizeSource::Collection("[grib] point_index_cache_mb"),
        None if grib.effective_point_index_cache_mb() > DEFAULT_POINT_INDEX_CACHE_MB => {
            SizeSource::Derived("[grib] message_cache_mb")
        }
        None => SizeSource::Default,
    }
}

/// A Zarr collection's decoded-chunk cache (Icechunk only; a plain store
/// builds it empty), a plain store's object cache and an Icechunk
/// repository's payload cache, both sized by `cache_mb`.
pub(crate) fn register_zarr(collection: &CollectionConfig, engine: &Arc<engine_zarr::ZarrEngine>) {
    let Some(zarr) = collection.zarr.as_ref() else {
        return;
    };
    let scope = || Scope::Collection {
        engine: "zarr",
        id: collection.id.clone(),
    };
    let decoded = match &zarr.icechunk {
        Some(icechunk) => SizeSource::unless_default(
            icechunk.decoded_cache_mb,
            DEFAULT_ZARR_CACHE_MB,
            SizeSource::Collection("[zarr.icechunk] decoded_cache_mb"),
        ),
        None => SizeSource::Constant,
    };
    plan::register(
        Entry::cache("zarr_decoded_cache", scope(), decoded),
        engine,
        |e| Usage::cache(e.decoded_cache_metrics(), None),
    );
    if engine.object_cache_metrics().is_some() {
        let object = SizeSource::unless_default(
            zarr.cache_mb,
            DEFAULT_ZARR_CACHE_MB,
            SizeSource::Collection("[zarr] cache_mb"),
        );
        plan::register(
            Entry::cache("zarr_object_cache", scope(), object),
            engine,
            |e| {
                e.object_cache_metrics()
                    .map(|m| Usage::cache(m, None))
                    .unwrap_or_default()
            },
        );
    }
    if engine.icechunk_payload_cache_metrics().is_some() {
        let payload = SizeSource::unless_default(
            zarr.cache_mb,
            DEFAULT_ZARR_CACHE_MB,
            SizeSource::Collection("[zarr] cache_mb"),
        );
        plan::register(
            Entry::cache("zarr_icechunk_payload_cache", scope(), payload),
            engine,
            |e| {
                e.icechunk_payload_cache_metrics()
                    .map(|m| Usage::cache(m, None))
                    .unwrap_or_default()
            },
        );
    }
}

/// The plan's entries: the registry, less the caches of collections the
/// accepted config no longer has, whose engines a request in flight may
/// still hold for a moment. Logs a WARN for each cache that has just
/// fallen below its demand (#1309).
pub(crate) fn snapshot(state: &ServerState) -> Vec<Row> {
    let live: HashSet<String> = state
        .last_collections
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .cloned()
        .collect();
    let rows: Vec<Row> = plan::snapshot()
        .into_iter()
        .filter(|row| {
            row.entry
                .scope
                .collection()
                .is_none_or(|id| live.contains(id))
        })
        .collect();
    for short in plan::check_rows(&rows) {
        warn!("{short}");
    }
    rows
}

/// Count, capacity and current use of the entries of one kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Totals {
    pub count: usize,
    pub capacity_bytes: u64,
    pub used_bytes: u64,
}

impl Totals {
    pub(crate) fn of(rows: &[Row], kind: Kind) -> Self {
        rows.iter()
            .filter(|row| row.entry.kind == kind)
            .fold(Totals::default(), |t, row| Totals {
                count: t.count + 1,
                capacity_bytes: t.capacity_bytes.saturating_add(row.usage.capacity_bytes),
                used_bytes: t.used_bytes.saturating_add(row.usage.used_bytes),
            })
    }
}

/// `GET /admin/cache-plan`, behind the `ADMIN_TOKEN` guard of
/// `/admin/collections/reload`.
pub async fn handler(
    State(state): State<AdminState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    crate::admin::require_admin_token(&state, &headers)?;
    let memory = crate::admin::refresh_metrics(&state);
    let report = memory_plan::last();
    Ok(Json(to_json(&snapshot(&state), &memory, report.as_deref())))
}

/// The endpoint's document: `caches` and `budgets`, one object each, their
/// totals, and the process and allocator memory `/metrics` reports. A cache
/// also has its cumulative `churn` and, once sampled for a while, what it
/// did over the `last_hour` ([`last_hour_json`]); both are null for a
/// budget. A cache whose engines publish its demand (#1309) has it:
/// `demand_bytes`, `demand_rule`, `demand_kind` (`demand`, or `floor` when
/// the requests' working set comes on top), `suggested_bytes` (the demand
/// plus 15 %, or the floor, in whole MiB) and `demand_inputs`
/// ([`demand_inputs_json`]); null otherwise. With the memory plan of the
/// last boot or reload (#1312), every entry has its `planned_bytes`,
/// `planned_reason` and `expected_bytes` (null for one entered since), and
/// `plan` has the total, the reserves and the pool
/// (`memory_plan::header_json`).
pub(crate) fn to_json(rows: &[Row], memory: &MemoryReading, report: Option<&Report>) -> Value {
    let entries = |kind: Kind| -> Vec<Value> {
        rows.iter()
            .filter(|row| row.entry.kind == kind)
            .map(|row| {
                let (entry, usage, demand) = (&row.entry, &row.usage, row.demand.as_ref());
                let grant = memory_plan::grant(report, row);
                json!({
                    "id": entry.id,
                    "scope": entry.scope.kind(),
                    "engine": entry.scope.engine(),
                    "collection": entry.scope.collection(),
                    "capacity_bytes": usage.capacity_bytes,
                    "source": entry.source.to_string(),
                    "used_bytes": usage.used_bytes,
                    "entries": usage.entries,
                    "hits": usage.hits,
                    "misses": usage.misses,
                    "churn": usage.churn.as_ref().map(churn_json),
                    "last_hour": row.window.as_ref().map(last_hour_json),
                    "demand_bytes": demand.map(|d| d.bytes),
                    "demand_rule": demand.map(|d| d.rule),
                    "demand_kind": demand.map(Demand::kind),
                    "suggested_bytes": demand.map(Demand::suggested_bytes),
                    "demand_inputs": demand.map(demand_inputs_json),
                    "planned_bytes": grant.map(|g| g.bytes),
                    "planned_reason": grant.map(|g| g.reason.to_string()),
                    "expected_bytes": grant.map(|g| g.expected_bytes),
                })
            })
            .collect()
    };
    let totals = |kind: Kind| {
        let t = Totals::of(rows, kind);
        json!({
            "count": t.count,
            "capacity_bytes": t.capacity_bytes,
            "used_bytes": t.used_bytes,
        })
    };
    let memory: serde_json::Map<String, Value> = memory
        .0
        .iter()
        .map(|(name, value)| (name.to_string(), json!(value)))
        .collect();
    json!({
        "caches": entries(Kind::Cache),
        "budgets": entries(Kind::Budget),
        "totals": {
            "caches": totals(Kind::Cache),
            "budgets": totals(Kind::Budget),
        },
        "memory": memory,
        "plan": report.map_or(Value::Null, memory_plan::header_json),
    })
}

/// Each collection's part of a cache's demand (#1309), with the facts its
/// engine computed it from.
fn demand_inputs_json(demand: &Demand) -> Value {
    demand
        .parts
        .iter()
        .map(|(collection, part)| {
            let inputs: serde_json::Map<String, Value> = part
                .inputs
                .iter()
                .map(|(name, value)| (name.to_string(), json!(value)))
                .collect();
            json!({"collection": collection, "bytes": part.bytes, "inputs": inputs})
        })
        .collect()
}

/// What a cache admitted, refused and evicted since it was built.
fn churn_json(churn: &Churn) -> Value {
    json!({
        "inserted_bytes": churn.inserted_bytes,
        "refused": churn.refused,
        "evictions": {"cold": churn.evicted_cold, "hot": churn.evicted_hot},
        "evicted_bytes": churn.evicted_bytes,
        "bypasses": churn.bypasses,
        "ghost_hits": churn.ghost_hits.map(by_depth),
        "prewarmed": churn.prewarmed,
    })
}

/// The caveat a pre-warmed cache's numbers carry.
const PREWARMED: &str = "pre-warmed: entries are inserted long before they are read, so \
                         evictions and ghost hits undervalue them (#992)";

/// What a cache did over its window, up to the last hour: lookups, hit
/// ratio, turnover (capacities admitted per hour), the ghost hits at each
/// depth as a share of misses, the hit ratio a cache 1.5, 2 or 3 times as
/// large would have had, and its label (#1308).
pub(crate) fn last_hour_json(window: &Activity) -> Value {
    let evictions = window.churn.evictions();
    json!({
        "window_secs": window.secs,
        "lookups": window.lookups(),
        "lookups_per_hour": rounded(window.per_hour(window.lookups())),
        "hit_ratio": window.hit_ratio().map(rounded),
        "turnover_per_hour": window.turnover_per_hour().map(rounded),
        "evictions": evictions,
        "cold_eviction_share": (evictions > 0)
            .then(|| rounded(window.churn.evicted_cold as f64 / evictions as f64)),
        "refused": window.churn.refused,
        "bypasses": window.churn.bypasses,
        "ghost_hit_share": window.ghost_share().map(|share| by_depth(share.map(rounded))),
        "estimated_hit_ratio": window.estimated_hit_ratios().map(|ratios| {
            let [larger_1_5, larger_2, larger_3] = ratios.map(rounded);
            json!({"1.5x": larger_1_5, "2x": larger_2, "3x": larger_3})
        }),
        "label": window.label().map(plan::Label::as_str),
        "caveat": window.churn.prewarmed.then_some(PREWARMED),
    })
}

/// Values at each of [`ds_cache::GHOST_DEPTHS`], keyed by depth.
fn by_depth<T: Into<Value>>(values: [T; 3]) -> Value {
    ds_cache::GHOST_DEPTHS
        .iter()
        .zip(values)
        .map(|(depth, value)| (depth.to_string(), value.into()))
        .collect::<serde_json::Map<String, Value>>()
        .into()
}

/// A ratio to four decimals.
fn rounded(value: f64) -> f64 {
    (value * 1e4).round() / 1e4
}

/// When the plan is logged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Occasion {
    Boot,
    Reload,
    /// A reload by the recovery loop: logged only when the plan changed,
    /// since the loop retries a pending source every few minutes.
    Recovery,
}

/// An entry's identity across logs, and the size and source last logged.
type Logged = HashMap<(Kind, &'static str, Scope), (u64, SizeSource)>;

static LOGGED: Mutex<Option<Logged>> = Mutex::new(None);

/// Log the plan as one INFO block. Process-wide caches and budgets enter the
/// plan when first read, so this refreshes the `/metrics` families first:
/// the block lists what `/metrics` reports. Fits the memory plan first
/// (#1312), whose WARNs follow the block.
pub(crate) fn log(state: &AdminState, occasion: Occasion) {
    let memory = crate::admin::refresh_metrics(state);
    let rows = snapshot(state);
    let report = memory_plan::replan(state, &rows, &memory);
    let current: Logged = rows
        .iter()
        .map(|row| {
            (
                (row.entry.kind, row.entry.id, row.entry.scope.clone()),
                (row.usage.capacity_bytes, row.entry.source.clone()),
            )
        })
        .collect();
    let mut logged = LOGGED.lock().unwrap_or_else(|e| e.into_inner());
    if occasion == Occasion::Recovery && logged.as_ref() == Some(&current) {
        return;
    }
    let heading = match occasion {
        Occasion::Boot => "at boot",
        Occasion::Reload | Occasion::Recovery => "after reload",
    };
    info!("{}", render(&rows, heading, logged.as_ref(), Some(&report)));
    memory_plan::log_warnings(&report, &rows);
    *logged = Some(current);
}

/// `bytes` in MiB, whole when it is a whole number of them.
pub(crate) fn mib(bytes: u64) -> String {
    if bytes.is_multiple_of(MIB) {
        format!("{} MiB", bytes / MIB)
    } else {
        format!("{:.2} MiB", bytes as f64 / MIB as f64)
    }
}

/// The log block: a heading with the totals and, with a memory plan
/// (#1312), a line on its total and pool, then one line per cache and per
/// budget, grouped by scope, with its capacity and source. A line whose
/// size differs from `previous` shows the size it had; a cache with a
/// demand (#1309) shows it and the size it suggests; an entry the plan
/// sizes, its planned size and why.
pub(crate) fn render(
    rows: &[Row],
    heading: &str,
    previous: Option<&Logged>,
    report: Option<&Report>,
) -> String {
    let caches = Totals::of(rows, Kind::Cache);
    let budgets = Totals::of(rows, Kind::Budget);
    let gib = |bytes: u64| bytes as f64 / (1024.0 * MIB as f64);
    let mut out = format!(
        "Cache and memory plan {heading}: {} caches {} ({:.2} GiB), {} budgets {} ({:.2} GiB)",
        caches.count,
        mib(caches.capacity_bytes),
        gib(caches.capacity_bytes),
        budgets.count,
        mib(budgets.capacity_bytes),
        gib(budgets.capacity_bytes),
    );
    if let Some(report) = report {
        let _ = write!(out, "\n  {}", memory_plan::header_line(report));
    }
    let name = |row: &Row| match row.entry.scope.collection() {
        Some(collection) => format!("{} {collection}", row.entry.id),
        None => row.entry.id.to_string(),
    };
    let width = rows.iter().map(|row| name(row).len()).max().unwrap_or(0);
    let mut group = None;
    for row in rows {
        let entry = &row.entry;
        let this_group = (entry.kind, entry.scope.kind(), entry.scope.engine());
        if group != Some(this_group) {
            group = Some(this_group);
            let _ = write!(out, "\n  {}s, {}", entry.kind.as_str(), entry.scope.kind());
            if let Some(engine) = entry.scope.engine() {
                let _ = write!(out, " {engine}");
            }
            out.push(':');
        }
        let _ = write!(
            out,
            "\n    {:<width$}  {:>10}  {}",
            name(row),
            mib(row.usage.capacity_bytes),
            entry.source
        );
        let was = previous
            .and_then(|p| p.get(&(entry.kind, entry.id, entry.scope.clone())))
            .map(|(capacity, _)| *capacity)
            .filter(|capacity| *capacity != row.usage.capacity_bytes);
        if let Some(was) = was {
            let _ = write!(out, " (was {})", mib(was));
        }
        if let Some(demand) = &row.demand {
            let _ = write!(
                out,
                ", {} {}, suggested {}",
                demand.kind(),
                mib(demand.bytes),
                mib(demand.suggested_bytes())
            );
        }
        if let Some(grant) = memory_plan::grant(report, row) {
            out.push_str(&memory_plan::line_suffix(grant));
        }
    }
    out
}

#[cfg(test)]
mod tests;
