pub mod cache;
pub mod catalog;
pub mod index;
pub mod reader;
mod time_window;
pub mod units;
pub mod wgrib2_index;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use chrono::{DateTime, Datelike, Utc};
use tokio::sync::watch;

use ds_core::config::GribConfig;
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterInfo, RasterTile};
use ds_core::model::*;

use crate::cache::{DecodedGrid, GridCache};
use crate::catalog::{Catalog, ForecastRun, StepFile};
use crate::time_window::TimeWindow;
use crate::units::{DisplayConversion, SourceUnit};

/// Resolved metadata for a parameter, populated after the first successful
/// decode of a message carrying that short name. Derived from the WMO triple
/// **and the Code Table 4.5 fixed surface type** read out of the GRIB2
/// message itself — never from hardcoded name tables.
#[derive(Debug, Clone)]
struct ParamMetadata {
    /// Base WMO label from Code Table 4.2, without any level qualifier.
    /// E.g. "Temperature", "Pressure", "u-component of wind".
    base_label: String,
    /// Canonical source unit from the WMO table. Kept for diagnostics and
    /// for potential config-driven display-unit overrides.
    #[allow(dead_code)]
    source_unit: SourceUnit,
    display: DisplayConversion,
    /// GRIB2 Code Table 4.5 first fixed surface type. `None` means no
    /// message has been probed yet (placeholder state).
    first_surface_type: Option<u8>,
    /// Scaled value of the first fixed surface. Units depend on the type;
    /// `None` for types where no numeric value applies (e.g. 1 surface,
    /// 101 MSL, 200 entire atmosphere).
    first_surface_value: Option<f64>,
}

impl ParamMetadata {
    /// Placeholder used when metadata has not yet been populated (no message
    /// for this parameter has been decoded yet). The label falls back to the
    /// short name and the display conversion is identity.
    fn placeholder(short_name: &str) -> Self {
        Self {
            base_label: short_name.to_string(),
            source_unit: SourceUnit::Dimensionless,
            display: DisplayConversion {
                display_unit: "",
                scale: 1.0,
                offset: 0.0,
            },
            first_surface_type: None,
            first_surface_value: None,
        }
    }

    /// Render the full display label, composing the base WMO label with a
    /// level qualifier derived from the Table 4.5 surface type.
    fn label(&self) -> String {
        let qualifier = self
            .first_surface_type
            .and_then(|t| units::format_level_qualifier(t, self.first_surface_value));
        units::compose_label(&self.base_label, qualifier.as_deref())
    }
}

/// Default model run hours for ECMWF IFS (4 runs per day).
const DEFAULT_RUN_HOURS: &[u32] = &[0, 6, 12, 18];

/// Number of days to scan back (today + yesterday handles overnight transitions).
const SCAN_DAYS: u32 = 2;

/// How often to force a full re-list of all run prefixes, ignoring the settled
/// skip. NWP runs publish sequentially so older runs are normally static, but a
/// provider can append late/corrected step files to an already-scanned run; a
/// periodic full scan catches those new paths within this bound while still
/// skipping the per-poll re-list the rest of the time. (Same-key content
/// rewrites of an existing index file are a separate, pre-existing limitation
/// of the path-keyed `known_indexes` dedup, not addressed here.)
const SETTLED_REVALIDATE_INTERVAL: Duration = Duration::from_secs(3600);

/// Engine for serving GRIB2 NWP forecast data.
///
/// Discovers GRIB files via index sidecar files on S3/HTTP, fetches individual
/// parameters via byte-range reads, and serves them through EDR and Maps APIs.
pub struct GribEngine {
    collection_id: String,
    config: GribConfig,
    catalog: ArcSwap<Catalog>,
    store: ds_storage::DataStore,
    prefix_pattern: String,
    grid_cache: Option<GridCache>,
    /// Shutdown signal for the poll loop.
    shutdown_tx: watch::Sender<bool>,
    /// Allowed parameters (None = all).
    param_filter: Option<Vec<String>>,
    /// Index files already downloaded and parsed (by S3 path). Avoids
    /// re-downloading unchanged index files on every poll cycle.
    known_indexes: Mutex<HashSet<String>>,
    /// Run prefixes that have been fully listed in a previous scan and are no
    /// longer the newest run. Listing a run prefix returns *all* its (often
    /// hundreds of) step files; once a newer run exists, an older run is
    /// static (NWP runs publish sequentially), so we skip re-listing it. Only
    /// unknown prefixes (new/not-yet-published runs) and the single newest
    /// known run are listed each scan. See `scan_once`.
    settled_prefixes: Mutex<HashSet<String>>,
    /// When the last full re-list (ignoring `settled_prefixes`) ran. `None`
    /// until the first scan. Drives the periodic re-validation in `scan_once`
    /// (see [`SETTLED_REVALIDATE_INTERVAL`]).
    last_full_scan: Mutex<Option<Instant>>,
    /// Which index file format this collection uses.
    index_format: index::IndexFormat,
    /// Parameter metadata cache keyed by short name. Populated lazily on
    /// the first successful decode of each distinct short name.
    param_meta: RwLock<HashMap<String, ParamMetadata>>,
}

impl GribEngine {
    /// Returns the collection ID.
    pub fn collection_id(&self) -> &str {
        &self.collection_id
    }

    /// Return total bytes read from storage.
    pub fn storage_bytes_read(&self) -> u64 {
        self.store.bytes_read()
    }

    /// Return (hits, misses) for the grid cache, or (0, 0) if disabled.
    pub fn grid_cache_stats(&self) -> (u64, u64) {
        self.grid_cache
            .as_ref()
            .map(|c| c.stats())
            .unwrap_or((0, 0))
    }

    /// Return grid cache utilization as (bytes_used, capacity_bytes, entries).
    /// Zeroes if the cache is disabled.
    pub fn grid_cache_utilization(&self) -> (u64, u64, usize) {
        self.grid_cache
            .as_ref()
            .map(|c| (c.weight(), c.capacity(), c.len()))
            .unwrap_or((0, 0, 0))
    }

    /// Create a new GRIB engine from config.
    pub fn new(collection_id: &str, config: &GribConfig) -> Result<Self, DataServerError> {
        // Validate config
        let endpoint = config.endpoint.as_deref().ok_or_else(|| {
            DataServerError::Config(format!(
                "Collection '{collection_id}': GRIB engine requires 'endpoint'"
            ))
        })?;
        let bucket = config.bucket.as_deref().ok_or_else(|| {
            DataServerError::Config(format!(
                "Collection '{collection_id}': GRIB engine requires 'bucket'"
            ))
        })?;

        // Build data store. Construct URL from endpoint+bucket for S3 region detection.
        let store_url = format!("{endpoint}/{bucket}/");
        let (store, _prefix) = ds_storage::build_store(&store_url).map_err(|e| {
            DataServerError::Config(format!(
                "Collection '{collection_id}': failed to build store: {e}"
            ))
        })?;

        let grid_cache = GridCache::new(config.grid_cache_mb);

        let (shutdown_tx, _shutdown_rx) = watch::channel(false);

        let index_format = index::IndexFormat::from_config(config.index_format.as_deref())
            .ok_or_else(|| {
                DataServerError::Config(format!(
                    "Collection '{collection_id}': invalid grib index_format"
                ))
            })?;

        let engine = Self {
            collection_id: collection_id.to_string(),
            config: config.clone(),
            catalog: ArcSwap::new(Arc::new(Catalog::new())),
            store,
            prefix_pattern: config.prefix_pattern.clone(),
            grid_cache,
            shutdown_tx,
            param_filter: config.parameters.clone(),
            known_indexes: Mutex::new(HashSet::new()),
            settled_prefixes: Mutex::new(HashSet::new()),
            last_full_scan: Mutex::new(None),
            index_format,
            param_meta: RwLock::new(HashMap::new()),
        };

        // Do initial scan
        if let Err(e) = engine.scan_once() {
            tracing::warn!(
                "Collection '{}': initial GRIB scan failed (will retry on poll): {}",
                collection_id,
                e
            );
        }

        Ok(engine)
    }

    /// Run the poll loop. Call from a spawned tokio task.
    pub async fn poll_loop(&self) {
        let mut rx = self.shutdown_tx.subscribe();
        let interval = std::time::Duration::from_secs(self.config.poll_interval_secs);

        loop {
            tokio::select! {
                _ = tokio::time::sleep(interval) => {
                    if let Err(e) = self.scan_once() {
                        tracing::warn!(
                            "Collection '{}': GRIB poll failed: {}",
                            self.collection_id, e
                        );
                    }
                }
                _ = rx.changed() => {
                    tracing::info!(
                        "Collection '{}': GRIB poll loop shutting down",
                        self.collection_id
                    );
                    return;
                }
            }
        }
    }

    /// Signal the poll loop to stop.
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    /// Perform one scan cycle: list index files across multiple dates and
    /// run hours, parse new ones, merge into the catalog.
    fn scan_once(&self) -> Result<(), DataServerError> {
        let now = Utc::now();
        let index_suffix = self.config.index_suffix.as_deref().unwrap_or(".index");
        let data_suffix = self.config.data_suffix.as_deref().unwrap_or(".grib2");
        let run_hours = self
            .config
            .run_hours
            .as_deref()
            .unwrap_or(DEFAULT_RUN_HOURS);

        // Generate all prefixes to scan, newest-first (skipping future runs).
        let prefixes = build_scan_prefixes(&self.prefix_pattern, now, run_hours);

        // Optional filename substring filter. Applied in addition to the
        // index suffix match so that, for example, a GFS atmos directory
        // containing pgrb2.0p25 / pgrb2.0p50 / pgrb2b / goessimpgrb2 can be
        // narrowed to just the 0.25-degree product.
        let filename_contains = self.config.filename_contains.as_deref();

        // Cap how many runs we actually scan. `max_runs` is the number of
        // runs we want to keep in the catalog; there is no point scanning
        // older prefixes that would immediately be evicted. We iterate
        // prefixes newest-first and stop after collecting hits from exactly
        // that many runs.
        //
        // Without `max_runs` set, we fall back to listing every prefix.
        let scan_budget = self.config.max_runs;

        // Collect all index files across all prefixes, iterating newest-first
        // and stopping once we have collected enough runs to satisfy
        // `max_runs`.
        let mut all_index_paths: Vec<ds_storage::object_store::path::Path> = Vec::new();
        let mut runs_with_hits = 0usize;
        let mut listed_prefixes = 0usize;
        let mut skipped_settled = 0usize;
        // Run prefixes we've already fully scanned and that are no longer the
        // newest run are "settled": NWP runs publish sequentially, so an older
        // run is static and re-listing its hundreds of step files every poll is
        // wasted work (and wasted `block_in_place` round-trips — #221). We skip
        // them and only list unknown prefixes (new / not-yet-published runs)
        // plus the newest run still gaining steps. Prefixes with hits this scan
        // are collected newest-first; after the loop all but the newest are
        // settled.
        // Periodically force a full re-list so late/corrected step files added
        // to an already-settled run are still picked up (bounded by
        // SETTLED_REVALIDATE_INTERVAL).
        // Only *read* the timer here; reset it after the listing pass actually
        // succeeds, so an S3 outage (every list errors) doesn't reset the clock
        // and suppress re-validation for another full interval.
        let force_full = self
            .last_full_scan
            .lock()
            .unwrap()
            .is_none_or(|t| t.elapsed() >= SETTLED_REVALIDATE_INTERVAL);
        let settled_snapshot = self.settled_prefixes.lock().unwrap().clone();
        let mut listed_with_hits: Vec<String> = Vec::new();
        for (_ref_time, prefix) in &prefixes {
            if let Some(budget) = scan_budget {
                if runs_with_hits >= budget {
                    break;
                }
            }
            if !force_full && settled_snapshot.contains(prefix) {
                skipped_settled += 1;
                continue;
            }
            listed_prefixes += 1;
            let obj_prefix = ds_storage::object_store::path::Path::from(prefix.as_str());
            let mut hits_in_this_prefix = 0usize;
            match self.store.list(&obj_prefix) {
                Ok(objects) => {
                    for obj in objects {
                        let loc = obj.location.as_ref();
                        if !loc.ends_with(index_suffix) {
                            continue;
                        }
                        if let Some(needle) = filename_contains {
                            if !loc.contains(needle) {
                                continue;
                            }
                        }
                        all_index_paths.push(obj.location);
                        hits_in_this_prefix += 1;
                    }
                }
                Err(e) => {
                    tracing::debug!(
                        "Collection '{}': failed to list prefix '{}': {}",
                        self.collection_id,
                        prefix,
                        e
                    );
                }
            }
            if hits_in_this_prefix > 0 {
                runs_with_hits += 1;
                listed_with_hits.push(prefix.clone());
            }
        }

        // Settle every run we listed with hits except the newest (first in the
        // newest-first iteration order), which may still be gaining steps.
        // Prune the settled set to the current scan window so it cannot grow
        // unbounded as old runs age out (and so a prefix that ever reappears is
        // rescanned).
        {
            let window: HashSet<&str> = prefixes.iter().map(|(_, p)| p.as_str()).collect();
            let mut settled = self.settled_prefixes.lock().unwrap();
            settle_completed_runs(&mut settled, &listed_with_hits, &window);
        }

        // Reset the re-validation clock after a forced full pass. The clock
        // only *paces* a best-effort hourly re-list of settled runs to catch
        // late/corrected steps; it is intentionally not conditioned on per-
        // prefix list success. A transient outage during the one forced scan
        // in an interval simply defers re-validation to the next interval —
        // harmless, because the newest (unsettled) run is still listed every
        // poll regardless. (Conditioning the reset on "no list errors" instead
        // lets a single chronically-unreachable prefix pin force_full on
        // forever, defeating the settled-skip optimization entirely.)
        if force_full {
            *self.last_full_scan.lock().unwrap() = Some(Instant::now());
        }
        tracing::debug!(
            "Collection '{}': listed {}/{} prefixes ({} settled, skipped), {} runs produced hits, {} candidate index files",
            self.collection_id,
            listed_prefixes,
            prefixes.len(),
            skipped_settled,
            runs_with_hits,
            all_index_paths.len()
        );

        if all_index_paths.is_empty() {
            tracing::debug!(
                "Collection '{}': no index files found in {} prefixes",
                self.collection_id,
                prefixes.len()
            );
            return Ok(());
        }

        // Filter to only new index files (not seen before)
        let new_paths: Vec<_> = {
            let known = self.known_indexes.lock().unwrap();
            all_index_paths
                .iter()
                .filter(|p| !known.contains(p.as_ref()))
                .cloned()
                .collect()
        };

        if new_paths.is_empty() {
            tracing::debug!(
                "Collection '{}': no new index files ({} already known)",
                self.collection_id,
                all_index_paths.len()
            );
            return Ok(());
        }

        tracing::info!(
            "Collection '{}': found {} new index files ({} total; listed {}/{} prefixes)",
            self.collection_id,
            new_paths.len(),
            all_index_paths.len(),
            listed_prefixes,
            prefixes.len()
        );

        // Start from existing catalog for incremental merge
        let mut new_catalog = (*self.catalog.load_full()).clone();

        for path in &new_paths {
            // Read index file
            let bytes = match self.store.get(path) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(
                        "Collection '{}': failed to read index file {}: {}",
                        self.collection_id,
                        path,
                        e
                    );
                    continue;
                }
            };

            let content = match std::str::from_utf8(&bytes) {
                Ok(s) => s,
                Err(_) => continue,
            };

            // Derive GRIB file URL from index file path
            let grib_url = path.as_ref().replace(index_suffix, data_suffix);

            // Parse according to format. For wgrib2 this also resolves the
            // last-record length via a HEAD request on the data file.
            let Some(parsed) = self.parse_and_resolve(self.index_format, content, &grib_url) else {
                continue;
            };

            let ref_time = parsed.reference_time;

            // Filter messages if param_filter is set
            let messages = if let Some(filter) = &self.param_filter {
                parsed
                    .messages
                    .into_iter()
                    .filter(|m| filter.contains(&m.param))
                    .collect()
            } else {
                parsed.messages
            };

            let step_file = StepFile { grib_url, messages };

            let run = new_catalog
                .runs
                .entry(ref_time)
                .or_insert_with(|| ForecastRun {
                    reference_time: ref_time,
                    steps: BTreeMap::new(),
                });

            run.steps.insert(parsed.step, step_file);

            // Mark as known
            self.known_indexes
                .lock()
                .unwrap()
                .insert(path.as_ref().to_string());
        }

        // Apply time_window filtering: remove steps whose valid times fall outside the window
        if let Some(tw_str) = &self.config.time_window {
            if let Ok(tw) = TimeWindow::parse(tw_str) {
                let (tw_start, tw_end) = tw.to_range(now);
                for run in new_catalog.runs.values_mut() {
                    run.steps.retain(|&step, _| {
                        let vt = run.reference_time + chrono::Duration::hours(i64::from(step));
                        vt >= tw_start && vt <= tw_end
                    });
                }
                // Remove runs that have no steps left
                new_catalog.runs.retain(|_, run| !run.steps.is_empty());
            }
        }

        // Apply max_runs eviction
        if let Some(max_runs) = self.config.max_runs {
            new_catalog.evict(max_runs);
        }

        // Clean up known_indexes: remove entries for runs that were evicted
        {
            let mut known = self.known_indexes.lock().unwrap();
            let valid_prefixes: HashSet<String> = new_catalog
                .runs
                .values()
                .flat_map(|r| r.steps.values().map(|s| s.grib_url.clone()))
                .collect();
            known.retain(|path| {
                // Keep if the corresponding grib URL is still in the catalog
                let grib_path = path.replace(index_suffix, data_suffix);
                valid_prefixes.contains(&grib_path)
            });
        }

        let total_steps: usize = new_catalog.runs.values().map(|r| r.steps.len()).sum();
        tracing::info!(
            "Collection '{}': catalog updated: {} runs, {} total steps",
            self.collection_id,
            new_catalog.runs.len(),
            total_steps
        );

        self.catalog.store(Arc::new(new_catalog));

        // Probe one message per distinct short name in the newest run to
        // populate the parameter metadata cache. Without this, the EDR
        // /collections endpoint returns empty labels/units until the first
        // actual query is served.
        self.probe_new_parameters();

        Ok(())
    }

    /// For each distinct short name in the newest forecast run that has not
    /// yet been seen, fetch one message via byte-range and decode it so the
    /// WMO triple populates the parameter metadata cache.
    ///
    /// Any failures are logged and swallowed — the probe is best-effort and
    /// the metadata cache will eventually fill in as real queries land on
    /// the missing parameters anyway.
    fn probe_new_parameters(&self) {
        let catalog = self.catalog.load();
        let Some(run) = catalog.latest_run() else {
            return;
        };
        let Some(step_file) = run.steps.values().next() else {
            return;
        };

        // For each distinct short name not yet in the metadata cache,
        // pick the message at the most canonical surface level (see
        // `MessageEntry::surface_priority`). This ensures that when a
        // short name appears at multiple levels (e.g. GFS `TMP` at 1
        // hybrid / 2 m AGL / 500 hPa / 2 hPa / ...), the probed metadata
        // reflects the conventional default — 2 m temperature, 10 m wind,
        // surface pressure — instead of whichever level happens to come
        // first in the index file.
        //
        // We carry the index of the winning message so that the fetch can
        // bypass `find_message`'s `(param, level)`-only lookup — which
        // would otherwise collide between, e.g., `hag=2 m` and `pl=2 hPa`
        // for GFS `TMP` (both have the numeric level 2).
        let todo: Vec<usize> = {
            let cache = self.param_meta.read().unwrap();
            // short_name → (best_priority, index in step_file.messages)
            let mut chosen: std::collections::HashMap<String, (u8, usize)> =
                std::collections::HashMap::new();
            for (i, m) in step_file.messages.iter().enumerate() {
                if cache.contains_key(&m.param) {
                    continue;
                }
                let prio = m.surface_priority();
                chosen
                    .entry(m.param.clone())
                    .and_modify(|existing| {
                        if prio < existing.0 {
                            *existing = (prio, i);
                        }
                    })
                    .or_insert((prio, i));
            }
            chosen.into_values().map(|(_, i)| i).collect()
        };

        if todo.is_empty() {
            return;
        }

        tracing::debug!(
            "Collection '{}': probing {} new parameters to populate metadata",
            self.collection_id,
            todo.len()
        );

        // Cap the probe budget to avoid a 700-request burst on an unfiltered
        // wgrib2 catalog. Users are expected to set `parameters` when using
        // wgrib2 — see the warning emitted elsewhere.
        const MAX_PROBES_PER_SCAN: usize = 32;
        for i in todo.into_iter().take(MAX_PROBES_PER_SCAN) {
            let entry = &step_file.messages[i];
            let name = entry.param.clone();
            if let Err(e) = self.fetch_grid_by_entry(&step_file.grib_url, entry, &name) {
                tracing::debug!(
                    "Collection '{}': probe for parameter '{name}' failed: {e}",
                    self.collection_id
                );
            }
        }
    }

    /// Parse an index file's contents into the engine's catalog shape.
    ///
    /// For `EcmwfJson` this is a straight call into the JSON parser —
    /// lengths are explicit and every message entry has `length = Some(_)`.
    ///
    /// For `Wgrib2` the parser derives lengths from next-record offsets and
    /// leaves the final record with `length = None`. We deliberately do NOT
    /// resolve the tail via a HEAD request here — that would cost one HEAD
    /// per index file during scan (hundreds of serial round-trips) for a
    /// record that users typically never query. Instead, the length is
    /// resolved lazily on the first actual fetch of the tail message.
    fn parse_and_resolve(
        &self,
        format: index::IndexFormat,
        content: &str,
        _grib_url: &str,
    ) -> Option<index::IndexResult> {
        match format {
            index::IndexFormat::EcmwfJson => index::parse_ecmwf_json(content),
            index::IndexFormat::Wgrib2 => {
                let parsed = wgrib2_index::parse_wgrib2(content)?;

                // wgrib2 indexes cover a single forecast step per file. We
                // derive the nominal step from the first message (all
                // messages in the same file share it after aggregate filter).
                let nominal_step = parsed.messages.first()?.nominal_step;

                // Convert ParsedMessage → MessageEntry directly. The tail
                // record keeps `length = None` and is resolved lazily in
                // `fetch_grid` when someone actually asks for it.
                let messages: Vec<catalog::MessageEntry> = parsed
                    .messages
                    .into_iter()
                    .map(|m| catalog::MessageEntry {
                        param: m.short_name,
                        levtype: m.levtype.to_string(),
                        level: m.level,
                        offset: m.offset,
                        length: m.length,
                    })
                    .collect();

                if messages.is_empty() {
                    return None;
                }

                Some(index::IndexResult {
                    reference_time: parsed.reference_time,
                    step: nominal_step,
                    messages,
                })
            }
        }
    }

    /// Fetch and decode a grid for a specific parameter from a step file.
    fn fetch_grid(
        &self,
        step_file: &StepFile,
        param: &str,
        level: Option<u32>,
    ) -> Result<Arc<cache::DecodedGrid>, DataServerError> {
        let entry = step_file.find_message(param, level).ok_or_else(|| {
            DataServerError::InvalidParameter(format!(
                "Parameter '{param}' not found in forecast step"
            ))
        })?;
        self.fetch_grid_by_entry(&step_file.grib_url, entry, param)
    }

    /// Fetch and decode a specific `MessageEntry` from the given data file
    /// URL. Use this when the caller has already resolved the exact entry
    /// they want (e.g. via `surface_priority`) and does not want
    /// `find_message`'s `(param, level)`-only matching to pick a different
    /// message that happens to share the same numeric level.
    fn fetch_grid_by_entry(
        &self,
        grib_url: &str,
        entry: &catalog::MessageEntry,
        param: &str,
    ) -> Result<Arc<cache::DecodedGrid>, DataServerError> {
        // Check cache (keyed by url + offset — unique per message)
        if let Some(cache) = &self.grid_cache {
            if let Some(grid) = cache.get(grib_url, entry.offset) {
                self.populate_metadata(param, &grid);
                return Ok(grid);
            }
        }

        // Fetch via byte-range
        let path = ds_storage::object_store::path::Path::from(grib_url);
        let grid = reader::read_message(&self.store, &path, entry)?;
        let grid = Arc::new(grid);

        self.populate_metadata(param, &grid);

        if let Some(cache) = &self.grid_cache {
            cache.insert(grib_url, entry.offset, grid.clone());
        }

        Ok(grid)
    }

    /// Populate the parameter metadata cache from a decoded grid, using the
    /// WMO triple *and* the Code Table 4.5 surface type carried by the
    /// message itself (not a hardcoded short-name table). No-op if the
    /// short name is already cached.
    fn populate_metadata(&self, short_name: &str, grid: &DecodedGrid) {
        {
            let cache = self.param_meta.read().unwrap();
            if cache.contains_key(short_name) {
                return;
            }
        }

        let (discipline, category, number) = grid.triple;
        let centre = grid.centre;
        let mut meta = match units::lookup(centre, discipline, category, number) {
            Some(info) => ParamMetadata {
                base_label: info.label.to_string(),
                source_unit: info.source_unit,
                display: units::default_display(info.source_unit),
                first_surface_type: None,
                first_surface_value: None,
            },
            None => {
                tracing::debug!(
                    "Unknown WMO triple ({centre}, {discipline}, {category}, {number}) \
                     for short name '{short_name}'; falling back to identity conversion"
                );
                ParamMetadata::placeholder(short_name)
            }
        };

        // Attach the surface type from the decoded message so that
        // otherwise-identical parameters at different levels (e.g. msl vs
        // sp, both under WMO triple (0, 3, 0) "Pressure") can be told apart
        // in the rendered label.
        meta.first_surface_type = Some(grid.first_surface_type);
        meta.first_surface_value = grid.first_surface_value;

        let mut cache = self.param_meta.write().unwrap();
        cache.entry(short_name.to_string()).or_insert(meta);
    }

    /// Look up cached parameter metadata. Returns a placeholder (identity
    /// conversion, empty unit string) if the short name has not yet been
    /// populated — typically because no message for it has been decoded yet.
    fn param_metadata(&self, short_name: &str) -> ParamMetadata {
        self.param_meta
            .read()
            .unwrap()
            .get(short_name)
            .cloned()
            .unwrap_or_else(|| ParamMetadata::placeholder(short_name))
    }

    /// Find the best step file for a datetime query.
    fn resolve_time(
        &self,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
    ) -> Result<(u32, StepFile), DataServerError> {
        let catalog = self.catalog.load();

        if catalog.runs.is_empty() {
            return Err(DataServerError::Engine(
                "No forecast data available".to_string(),
            ));
        }

        let target = match datetime {
            Some((start, _end)) => start,
            None => {
                // Default to latest available time
                let run = catalog.latest_run().unwrap();
                let (&step, sf) = run.steps.iter().next_back().unwrap();
                return Ok((step, sf.clone()));
            }
        };

        catalog
            .find_for_time(target)
            .map(|(step, sf)| (step, sf.clone()))
            .ok_or_else(|| {
                DataServerError::InvalidParameter(format!(
                    "No forecast data available for time {target}"
                ))
            })
    }
}

// ---------------------------------------------------------------------------
// Engine trait (EDR)
// ---------------------------------------------------------------------------

impl EdrEngine for GribEngine {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        // Gridded data has no discrete locations
        Ok(Vec::new())
    }

    fn query_location(
        &self,
        _location_id: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
    ) -> Result<CoverageResponse, DataServerError> {
        Err(DataServerError::InvalidParameter(
            "GRIB engine does not support location queries; use position or area instead"
                .to_string(),
        ))
    }

    fn get_parameters(&self) -> Vec<String> {
        self.catalog.load().all_params()
    }

    fn get_parameter_descriptions(
        &self,
    ) -> std::collections::HashMap<String, ParameterDescription> {
        let all = self.catalog.load().all_params();
        let mut map = std::collections::HashMap::new();
        for p in all {
            let meta = self.param_metadata(&p);
            map.insert(
                p.clone(),
                ParameterDescription {
                    label: meta.label(),
                    unit: meta.display.display_unit.to_string(),
                    observed_property: p,
                },
            );
        }
        map
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        self.catalog.load().temporal_extent()
    }

    fn get_available_times(&self) -> Option<Vec<DateTime<Utc>>> {
        let times = self.catalog.load().all_valid_times();
        if times.is_empty() {
            None
        } else {
            Some(times)
        }
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        // Global grid
        if self.catalog.load().runs.is_empty() {
            return None;
        }
        Some([-180.0, -90.0, 180.0, 90.0])
    }

    fn supported_query_types(&self) -> Vec<String> {
        vec!["position".to_string(), "area".to_string()]
    }

    fn query_position(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _z: Option<&[f64]>,
    ) -> Result<CoverageResponse, DataServerError> {
        let (lon, lat) = parse_coords(coords)?;
        let catalog = self.catalog.load();

        if catalog.runs.is_empty() {
            return Err(DataServerError::Engine(
                "No forecast data available".to_string(),
            ));
        }

        // Find the best forecast run
        let run = match datetime {
            Some((start, _)) => {
                // Find the run whose valid times include the requested time
                catalog
                    .runs
                    .values()
                    .rev()
                    .find(|r| r.find_step_for_time(start).is_some())
                    .ok_or_else(|| {
                        DataServerError::InvalidParameter(format!(
                            "No forecast run covers time {start}"
                        ))
                    })?
            }
            None => catalog.latest_run().unwrap(),
        };

        // Determine which forecast steps to include
        let steps_to_query: Vec<(u32, &StepFile)> = match datetime {
            Some((start, end)) => {
                // If start == end (single instant), return just that step
                // If interval, return all steps within the range
                run.steps
                    .iter()
                    .filter(|(&step, _)| {
                        let vt = run.reference_time + chrono::Duration::hours(i64::from(step));
                        vt >= start && vt <= end
                    })
                    .map(|(&s, sf)| (s, sf))
                    .collect()
            }
            None => {
                // No datetime: return all steps in the latest run (full forecast time series)
                run.steps.iter().map(|(&s, sf)| (s, sf)).collect()
            }
        };

        if steps_to_query.is_empty() {
            return Err(DataServerError::InvalidParameter(
                "No forecast steps match the requested time range".to_string(),
            ));
        }

        // Determine which parameters to query
        let first_step = &steps_to_query[0].1;
        let query_params: Vec<String> = match parameters {
            Some(p) => p.to_vec(),
            None => {
                // Default to near-surface parameters only (surface + 2m/10m/etc.)
                let mut seen = std::collections::HashSet::new();
                first_step
                    .messages
                    .iter()
                    .filter(|m| m.is_near_surface())
                    .filter(|m| seen.insert(m.param.clone()))
                    .map(|m| m.param.clone())
                    .collect()
            }
        };

        // Build valid times
        let valid_times: Vec<DateTime<Utc>> = steps_to_query
            .iter()
            .map(|(step, _)| run.reference_time + chrono::Duration::hours(i64::from(*step)))
            .collect();

        let mut param_descs = std::collections::HashMap::new();
        let mut ranges = std::collections::HashMap::new();

        for param_name in &query_params {
            // Collect values across all forecast steps
            let mut values: Vec<Option<f64>> = Vec::with_capacity(steps_to_query.len());
            for (_step, step_file) in &steps_to_query {
                let level = step_file
                    .messages
                    .iter()
                    .find(|m| m.param == *param_name)
                    .and_then(|m| m.level);

                match self.fetch_grid(step_file, param_name, level) {
                    Ok(grid) => {
                        let raw = grid.bilinear_value(lon, lat);
                        // Metadata must be resolved AFTER fetch_grid (which
                        // populates the cache on first decode).
                        let meta = self.param_metadata(param_name);
                        values.push(raw.map(|v| meta.display.convert(v)));
                    }
                    Err(e) => {
                        tracing::warn!("Failed to fetch {param_name} for step: {e}");
                        values.push(None);
                    }
                }
            }

            let meta = self.param_metadata(param_name);
            param_descs.insert(
                param_name.to_string(),
                ParameterDescription {
                    label: meta.label(),
                    unit: meta.display.display_unit.to_string(),
                    observed_property: param_name.to_string(),
                },
            );

            ranges.insert(
                param_name.to_string(),
                NdArray {
                    shape: vec![valid_times.len()],
                    axis_names: vec!["t".to_string()],
                    values,
                },
            );
        }

        Ok(CoverageResponse::Single(QueryResult {
            domain: DomainDescription::PointSeries {
                x: lon,
                y: lat,
                t: valid_times,
                z: None,
            },
            parameters: param_descs,
            ranges,
        }))
    }

    fn query_area(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _z: Option<&[f64]>,
    ) -> Result<CoverageResponse, DataServerError> {
        let bbox = parse_bbox_from_wkt(coords)?;
        let (_step, step_file) = self.resolve_time(datetime)?;

        // Default to first near-surface parameter
        let query_params: Vec<&str> = match parameters {
            Some(p) => p.iter().map(|s| s.as_str()).collect(),
            None => {
                let surface: Vec<_> = step_file
                    .messages
                    .iter()
                    .filter(|m| m.is_near_surface())
                    .map(|m| m.param.as_str())
                    .take(1)
                    .collect();
                surface
            }
        };

        if query_params.is_empty() {
            return Err(DataServerError::InvalidParameter(
                "No parameters specified for area query".to_string(),
            ));
        }

        // Use the first parameter to determine grid extent
        let param_name = query_params[0];
        let level = step_file
            .messages
            .iter()
            .find(|m| m.param == param_name)
            .and_then(|m| m.level);

        let grid = self.fetch_grid(&step_file, param_name, level)?;

        let (x_coords, y_coords, _values) = grid.extract_bbox(bbox).ok_or_else(|| {
            DataServerError::InvalidParameter("Bbox does not intersect grid".to_string())
        })?;

        // Check area size limit
        let area_pixels = x_coords.len() * y_coords.len();
        if area_pixels > 1_000_000 {
            return Err(DataServerError::InvalidParameter(format!(
                "Area query would return {area_pixels} pixels, exceeding limit of 1,000,000"
            )));
        }

        let mut param_descs = std::collections::HashMap::new();
        let mut ranges = std::collections::HashMap::new();

        for &pname in &query_params {
            let plevel = step_file
                .messages
                .iter()
                .find(|m| m.param == pname)
                .and_then(|m| m.level);

            let pgrid = self.fetch_grid(&step_file, pname, plevel)?;
            let (_xc, _yc, values) = pgrid.extract_bbox(bbox).ok_or_else(|| {
                DataServerError::InvalidParameter(format!(
                    "Bbox does not intersect grid for {pname}"
                ))
            })?;

            // Metadata is populated by fetch_grid on first decode.
            let meta = self.param_metadata(pname);

            // Apply unit conversion
            let values: Vec<Option<f64>> = if meta.display.has_conversion() {
                values
                    .into_iter()
                    .map(|v| v.map(|raw| meta.display.convert(raw)))
                    .collect()
            } else {
                values
            };

            param_descs.insert(
                pname.to_string(),
                ParameterDescription {
                    label: meta.label(),
                    unit: meta.display.display_unit.to_string(),
                    observed_property: pname.to_string(),
                },
            );

            ranges.insert(
                pname.to_string(),
                NdArray {
                    shape: vec![y_coords.len(), x_coords.len()],
                    axis_names: vec!["y".to_string(), "x".to_string()],
                    values,
                },
            );
        }

        Ok(CoverageResponse::Single(QueryResult {
            domain: DomainDescription::Grid {
                x: x_coords,
                y: y_coords,
                t: None,
                z: None,
            },
            parameters: param_descs,
            ranges,
        }))
    }
}

// ---------------------------------------------------------------------------
// MapEngine trait (WMS/Maps/Tiles)
// ---------------------------------------------------------------------------

impl MapEngine for GribEngine {
    fn get_raster_tile(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        output_crs: &OutputCrs,
        parameter: Option<&str>,
        z: Option<f64>,
    ) -> Result<RasterTile, DataServerError> {
        let _ = z; // GRIB collections expose no vertical dimension yet (#185)
        let datetime = time.map(|t| (t, t));
        let (_step, step_file) = self.resolve_time(datetime)?;

        // Determine parameter to render
        let param_name = parameter.unwrap_or_else(|| {
            // Default to first near-surface parameter
            step_file
                .messages
                .iter()
                .find(|m| m.is_near_surface())
                .map(|m| m.param.as_str())
                .unwrap_or("2t")
        });

        let level = step_file
            .messages
            .iter()
            .find(|m| m.param == param_name)
            .and_then(|m| m.level);

        let grid = self.fetch_grid(&step_file, param_name, level)?;

        let web_mercator = matches!(output_crs, OutputCrs::WebMercator);
        let values = grid.resample(bbox, width, height, web_mercator);

        // Apply unit conversion so colormap ranges use display units.
        // fetch_grid populates the metadata cache from the decoded message's
        // WMO triple on first decode, so this lookup is safe here.
        let meta = self.param_metadata(param_name);
        let values = if meta.display.has_conversion() {
            values
                .into_iter()
                .map(|v| v.map(|raw| meta.display.convert(raw)))
                .collect()
        } else {
            values
        };

        Ok(RasterTile {
            width,
            height,
            values,
        })
    }

    fn raster_info(&self) -> RasterInfo {
        let catalog = self.catalog.load();
        let times = catalog.all_valid_times();

        // Build parameter list from catalog using cached metadata (populated
        // lazily as each parameter is first decoded).
        let params: Vec<(String, String)> = catalog
            .all_params()
            .into_iter()
            .map(|p| {
                let meta = self.param_metadata(&p);
                let label = meta.label();
                (p, label)
            })
            .collect();

        let default_param = params
            .first()
            .map(|(name, _)| name.clone())
            .unwrap_or_else(|| "2t".to_string());

        let default_unit = self
            .param_metadata(&default_param)
            .display
            .display_unit
            .to_string();

        RasterInfo {
            native_crs: "EPSG:4326".to_string(),
            spatial_extent: Some([-180.0, -90.0, 180.0, 90.0]),
            times,
            parameter: default_param,
            unit: default_unit,
            parameters: params,
            vertical: None,
            // Grid ni/nj are only known after a message is decoded; the
            // catalog metadata doesn't carry them, so leave the spatial grid
            // unadvertised for now (tracked as a follow-up).
            grid_size: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse "POINT(lon lat)" or "lon,lat" coordinates.
fn parse_coords(coords: &str) -> Result<(f64, f64), DataServerError> {
    let coords = coords.trim();

    // Try POINT(lon lat) format
    if let Some(inner) = coords
        .strip_prefix("POINT(")
        .or_else(|| coords.strip_prefix("POINT ("))
        .and_then(|s| s.strip_suffix(')'))
    {
        let parts: Vec<&str> = inner.split_whitespace().collect();
        if parts.len() == 2 {
            let lon: f64 = parts[0].parse().map_err(|_| {
                DataServerError::InvalidParameter(format!("Invalid longitude: {}", parts[0]))
            })?;
            let lat: f64 = parts[1].parse().map_err(|_| {
                DataServerError::InvalidParameter(format!("Invalid latitude: {}", parts[1]))
            })?;
            return Ok((lon, lat));
        }
    }

    // Try lon,lat format
    let parts: Vec<&str> = coords.split(',').collect();
    if parts.len() == 2 {
        let lon: f64 = parts[0].trim().parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("Invalid longitude: {}", parts[0]))
        })?;
        let lat: f64 = parts[1].trim().parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("Invalid latitude: {}", parts[1]))
        })?;
        return Ok((lon, lat));
    }

    Err(DataServerError::InvalidParameter(format!(
        "Cannot parse coordinates: {coords}"
    )))
}

/// Extract bbox [west, south, east, north] from WKT POLYGON or simple bbox string.
fn parse_bbox_from_wkt(coords: &str) -> Result<[f64; 4], DataServerError> {
    let coords = coords.trim();

    // Try "west,south,east,north" format first
    let parts: Vec<&str> = coords.split(',').collect();
    if parts.len() == 4 {
        let vals: Result<Vec<f64>, _> = parts.iter().map(|p| p.trim().parse::<f64>()).collect();
        if let Ok(v) = vals {
            return Ok([v[0], v[1], v[2], v[3]]);
        }
    }

    // Try WKT POLYGON((x1 y1, x2 y2, ...))
    if coords.starts_with("POLYGON") {
        let inner = coords
            .replace("POLYGON((", "")
            .replace("POLYGON ((", "")
            .replace("))", "");
        let mut min_x = f64::MAX;
        let mut min_y = f64::MAX;
        let mut max_x = f64::MIN;
        let mut max_y = f64::MIN;

        for pair in inner.split(',') {
            let xy: Vec<&str> = pair.split_whitespace().collect();
            if xy.len() >= 2 {
                let x: f64 = xy[0].parse().map_err(|_| {
                    DataServerError::InvalidParameter(format!("Invalid WKT coordinate: {pair}"))
                })?;
                let y: f64 = xy[1].parse().map_err(|_| {
                    DataServerError::InvalidParameter(format!("Invalid WKT coordinate: {pair}"))
                })?;
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }

        return Ok([min_x, min_y, max_x, max_y]);
    }

    Err(DataServerError::InvalidParameter(format!(
        "Cannot parse area coordinates: {coords}"
    )))
}

/// Build `(reference_time, prefix)` pairs to scan for the given pattern,
/// dates, and run hours.
///
/// The pattern supports strftime placeholders for the date part, plus `{run}`
/// which is expanded to each run hour (zero-padded, e.g., "00", "06", "12", "18").
///
/// Behaviour notes:
/// - Returned pairs are sorted by `reference_time` **descending** (newest
///   first), so callers can iterate and stop early once they have collected
///   enough runs.
/// - Future runs (reference time strictly after `now`) are skipped — a model
///   run cannot exist before its reference time.
/// - If the pattern contains no `{run}` placeholder, the reference time is
///   taken to be the UTC start of the scan date and only one prefix is
///   emitted per day (backward compatible with the date-only behaviour).
fn build_scan_prefixes(
    pattern: &str,
    now: DateTime<Utc>,
    run_hours: &[u32],
) -> Vec<(DateTime<Utc>, String)> {
    use chrono::{Duration, TimeZone};

    let mut out: Vec<(DateTime<Utc>, String)> = Vec::new();

    for days_back in 0..SCAN_DAYS {
        let date = now - Duration::days(i64::from(days_back));

        if pattern.contains("{run}") {
            for &hour in run_hours {
                let ref_time = Utc
                    .with_ymd_and_hms(
                        date.year_ce().1 as i32,
                        date.month(),
                        date.day(),
                        hour,
                        0,
                        0,
                    )
                    .single();
                let Some(ref_time) = ref_time else {
                    continue;
                };
                if ref_time > now {
                    continue;
                }
                let run_str = format!("{hour:02}");
                let with_run = pattern.replace("{run}", &run_str);
                let prefix = date.format(&with_run).to_string();
                out.push((ref_time, prefix));
            }
        } else {
            let ref_time = Utc
                .with_ymd_and_hms(date.year_ce().1 as i32, date.month(), date.day(), 0, 0, 0)
                .single();
            if let Some(ref_time) = ref_time {
                if ref_time > now {
                    continue;
                }
                out.push((ref_time, date.format(pattern).to_string()));
            }
        }
    }

    // Sort newest-first so callers can break early once they have enough runs.
    out.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    out
}

/// Update the set of "settled" run prefixes after a scan.
///
/// `listed_with_hits_newest_first` are the run prefixes that produced index
/// files this scan, **ordered newest-first** — this ordering is load-bearing:
/// the first element is treated as the still-active newest run and left
/// unsettled, every other element is marked settled. (NWP runs publish
/// sequentially, so once a newer run exists an older one is static and need not
/// be re-listed; the newest stays unsettled so its still-trickling steps keep
/// being picked up.) The caller derives the order from `build_scan_prefixes`,
/// which sorts `Reverse` by ref-time. `window` is the current scan window;
/// settled prefixes outside it are pruned so the set cannot grow unbounded as
/// old runs age out.
fn settle_completed_runs(
    settled: &mut HashSet<String>,
    listed_with_hits_newest_first: &[String],
    window: &HashSet<&str>,
) {
    for prefix in listed_with_hits_newest_first.iter().skip(1) {
        settled.insert(prefix.clone());
    }
    settled.retain(|p| window.contains(p.as_str()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win<'a>(prefixes: &'a [&'a str]) -> HashSet<&'a str> {
        prefixes.iter().copied().collect()
    }

    #[test]
    fn settle_keeps_newest_unsettled() {
        // First scan lists three runs (newest-first); the two older ones settle,
        // the newest stays unsettled so its trickling steps keep being scanned.
        let mut settled = HashSet::new();
        let listed = [
            "runN".to_string(),
            "runN-1".to_string(),
            "runN-2".to_string(),
        ];
        settle_completed_runs(&mut settled, &listed, &win(&["runN", "runN-1", "runN-2"]));
        assert!(!settled.contains("runN"), "newest run must not be settled");
        assert!(settled.contains("runN-1"));
        assert!(settled.contains("runN-2"));
    }

    #[test]
    fn settle_evolves_when_new_run_appears() {
        // Scan 1: runs N-1..N-3 published; N-2/N-3 settle, N-1 newest.
        let mut settled = HashSet::new();
        let window = win(&["runN-1", "runN-2", "runN-3"]);
        settle_completed_runs(
            &mut settled,
            &[
                "runN-1".to_string(),
                "runN-2".to_string(),
                "runN-3".to_string(),
            ],
            &window,
        );
        assert_eq!(
            settled,
            ["runN-2", "runN-3"].iter().map(|s| s.to_string()).collect()
        );

        // Scan 2: nothing new; only the unsettled newest (N-1) was listed.
        settle_completed_runs(&mut settled, &["runN-1".to_string()], &window);
        assert!(
            !settled.contains("runN-1"),
            "newest still re-listed, not settled"
        );

        // Scan 3: new run N appears; both N (new) and N-1 (was newest) were
        // listed → N-1 now settles, N becomes the newest unsettled run.
        let window3 = win(&["runN", "runN-1", "runN-2", "runN-3"]);
        settle_completed_runs(
            &mut settled,
            &["runN".to_string(), "runN-1".to_string()],
            &window3,
        );
        assert!(
            !settled.contains("runN"),
            "new newest run must stay unsettled"
        );
        assert!(settled.contains("runN-1"), "previous newest now settled");
    }

    #[test]
    fn settle_prunes_out_of_window_prefixes() {
        // Aged-out prefixes drop from the settled set so it can't grow forever.
        let mut settled: HashSet<String> = ["old1", "old2", "runN-1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        settle_completed_runs(&mut settled, &[], &win(&["runN", "runN-1"]));
        assert_eq!(settled, ["runN-1"].iter().map(|s| s.to_string()).collect());
    }

    #[test]
    fn test_parse_coords_point() {
        let (lon, lat) = parse_coords("POINT(25.5 60.2)").unwrap();
        assert!((lon - 25.5).abs() < 1e-10);
        assert!((lat - 60.2).abs() < 1e-10);
    }

    #[test]
    fn test_parse_coords_csv() {
        let (lon, lat) = parse_coords("25.5,60.2").unwrap();
        assert!((lon - 25.5).abs() < 1e-10);
        assert!((lat - 60.2).abs() < 1e-10);
    }

    #[test]
    fn test_parse_bbox() {
        let bbox = parse_bbox_from_wkt("20,55,30,65").unwrap();
        assert_eq!(bbox, [20.0, 55.0, 30.0, 65.0]);
    }

    #[test]
    fn test_build_scan_prefixes_with_run() {
        use chrono::TimeZone;
        // 2026-04-06 15:00 UTC: today's 00/06/12 have published, 18z is future.
        let dt = Utc.with_ymd_and_hms(2026, 4, 6, 15, 0, 0).unwrap();
        let prefixes = build_scan_prefixes("%Y%m%d/{run}z/ifs/0p25/oper/", dt, &[0, 6, 12, 18]);

        // 2 days × 4 run hours minus 1 future run (today's 18z) = 7 prefixes.
        assert_eq!(prefixes.len(), 7);

        // Newest-first ordering.
        assert_eq!(prefixes[0].1, "20260406/12z/ifs/0p25/oper/");
        assert_eq!(prefixes[1].1, "20260406/06z/ifs/0p25/oper/");
        assert_eq!(prefixes[2].1, "20260406/00z/ifs/0p25/oper/");
        assert_eq!(prefixes[3].1, "20260405/18z/ifs/0p25/oper/");
        assert_eq!(prefixes[6].1, "20260405/00z/ifs/0p25/oper/");

        // Reference times are strictly descending.
        for pair in prefixes.windows(2) {
            assert!(pair[0].0 > pair[1].0);
        }
    }

    #[test]
    fn test_build_scan_prefixes_skips_future_runs() {
        use chrono::TimeZone;
        // 2026-04-06 02:00 UTC: yesterday 18z is the latest published run.
        // Today's 00z technically has a reference time of 00:00 UTC which is
        // <= now, so it should still be listed (even if still publishing).
        let dt = Utc.with_ymd_and_hms(2026, 4, 6, 2, 0, 0).unwrap();
        let prefixes = build_scan_prefixes("%Y%m%d/{run}z/ifs/0p25/oper/", dt, &[0, 6, 12, 18]);

        // Today: 00z only. Yesterday: all 4. Total 5.
        assert_eq!(prefixes.len(), 5);
        assert_eq!(prefixes[0].1, "20260406/00z/ifs/0p25/oper/");
        assert_eq!(prefixes[1].1, "20260405/18z/ifs/0p25/oper/");
    }

    #[test]
    fn test_build_scan_prefixes_no_run_placeholder() {
        use chrono::TimeZone;
        let dt = Utc.with_ymd_and_hms(2026, 4, 6, 15, 0, 0).unwrap();
        let prefixes = build_scan_prefixes("%Y%m%d/00z/ifs/0p25/oper/", dt, &[0, 6, 12, 18]);

        // No {run} placeholder — 2 dates, 1 prefix each, newest-first.
        assert_eq!(prefixes.len(), 2);
        assert_eq!(prefixes[0].1, "20260406/00z/ifs/0p25/oper/");
        assert_eq!(prefixes[1].1, "20260405/00z/ifs/0p25/oper/");
    }
}
