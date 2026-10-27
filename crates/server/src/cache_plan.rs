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

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use ds_cache::plan::{self, Entry, Kind, Row, Scope, SizeSource, Usage};
use ds_cache::MIB;
use ds_core::config::{
    CollectionConfig, ServerConfig, DEFAULT_GRID_CACHE_MB, DEFAULT_METATILE_CACHE_MB,
    DEFAULT_POINT_INDEX_CACHE_MB, DEFAULT_TILE_CACHE_MB, DEFAULT_ZARR_CACHE_MB,
};
use serde_json::{json, Value};
use tracing::info;

use crate::admin::{AdminState, MemoryReading, ServerState};

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
        let (used, capacity, entries) = e.tile_cache_utilization();
        let (hits, misses) = e.tile_cache_stats();
        Usage {
            capacity_bytes: capacity,
            used_bytes: used,
            entries: Some(entries as u64),
            hits: Some(hits),
            misses: Some(misses),
        }
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
        let (used, capacity, entries) = e.grid_cache_utilization();
        let (hits, misses) = e.grid_cache_stats();
        Usage {
            capacity_bytes: capacity,
            used_bytes: used,
            entries: Some(entries as u64),
            hits: Some(hits),
            misses: Some(misses),
        }
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
    let point_index = SizeSource::unless_default(
        grib.point_index_cache_mb,
        DEFAULT_POINT_INDEX_CACHE_MB,
        SizeSource::Collection("[grib] point_index_cache_mb"),
    );
    plan::register(
        Entry::cache("grib_point_index_cache", scope(), point_index),
        engine,
        |e| Usage::cache(e.point_index_cache_metrics(), None),
    );
}

/// A Zarr collection's decoded-chunk cache (Icechunk only; a plain store
/// builds it empty) and a plain store's object cache.
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
}

/// The plan's entries: the registry, less the caches of collections the
/// accepted config no longer has, whose engines a request in flight may
/// still hold for a moment.
pub(crate) fn snapshot(state: &ServerState) -> Vec<Row> {
    let live: HashSet<String> = state
        .last_collections
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .cloned()
        .collect();
    plan::snapshot()
        .into_iter()
        .filter(|row| {
            row.entry
                .scope
                .collection()
                .is_none_or(|id| live.contains(id))
        })
        .collect()
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
    Ok(Json(to_json(&snapshot(&state), &memory)))
}

/// The endpoint's document: `caches` and `budgets`, one object each, their
/// totals, and the process and allocator memory `/metrics` reports.
pub(crate) fn to_json(rows: &[Row], memory: &MemoryReading) -> Value {
    let entries = |kind: Kind| -> Vec<Value> {
        rows.iter()
            .filter(|row| row.entry.kind == kind)
            .map(|row| {
                let (entry, usage) = (&row.entry, &row.usage);
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
    })
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
/// the block lists what `/metrics` reports.
pub(crate) fn log(state: &AdminState, occasion: Occasion) {
    crate::admin::refresh_metrics(state);
    let rows = snapshot(state);
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
    info!("{}", render(&rows, heading, logged.as_ref()));
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

/// The log block: a heading with the totals, then one line per cache and
/// per budget, grouped by scope, with its capacity and source. A line whose
/// size differs from `previous` shows the size it had.
pub(crate) fn render(rows: &[Row], heading: &str, previous: Option<&Logged>) -> String {
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
    }
    out
}

#[cfg(test)]
mod tests;
