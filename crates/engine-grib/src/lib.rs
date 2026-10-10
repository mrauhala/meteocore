mod area;
pub mod cache;
pub mod catalog;
mod cube;
mod diagnostics;
mod discovery;
pub mod index;
mod message_cache;
mod metadata;
#[cfg(test)]
mod parameter_times_tests;
mod position;
pub mod reader;
mod runtime;
#[cfg(test)]
mod scan_tests;
#[cfg(test)]
mod test_support;
mod trajectory;
pub mod units;
mod vertical;
pub mod wgrib2_index;
#[cfg(test)]
mod wind_tests;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use ds_poll::{FirstTick, Shutdown};

use ds_core::config::{GribConfig, GribLevelType};
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::instances::{self, RunInfo};
use ds_core::map_engine::{MapEngine, OutputCrs, RasterInfo, RasterTile, RasterValues};
use ds_core::model::*;
use ds_core::temp_files;

use crate::cache::{DecodedGrid, GridCache};
use crate::catalog::{Catalog, ForecastRun, ParameterKey, ParameterKeys, StepFile};
use crate::units::{DisplayConversion, SourceUnit};
use ds_storage::discovery::{
    expand_run_prefixes, validate_run_prefix_pattern, RunPrefix, TimeWindow,
};

/// Resolved metadata for a parameter, populated by a successful header probe
/// or decode of that parameter and level. Derived from the WMO triple
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
    window_qualifier: Option<String>,
    /// WMO `(discipline, category, number)` of the probed or decoded
    /// message; `None` until one is read.
    triple: Option<(u8, u8, u8)>,
    /// The message's GRIB2 flag table 3.3 bit 5 (#897).
    uv_frame: ds_core::wind::VectorFrame,
}

impl ParamMetadata {
    /// Placeholder used when metadata has not yet been populated (no message
    /// for this parameter has been read yet). The label falls back to the
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
            window_qualifier: None,
            triple: None,
            uv_frame: ds_core::wind::VectorFrame::Unknown,
        }
    }

    /// Render the full display label, composing the base WMO label with a
    /// level qualifier derived from the Table 4.5 surface type.
    fn label(&self) -> String {
        let qualifier = self
            .first_surface_type
            .and_then(|t| units::format_level_qualifier(t, self.first_surface_value));
        let label = units::compose_label(&self.base_label, qualifier.as_deref());
        units::compose_label(&label, self.window_qualifier.as_deref())
    }
}

/// Exact metadata plus one known representative per (level type, name).
/// Both indexes are updated under the source's single metadata write lock.
#[derive(Default)]
struct ParamMetadataCache {
    by_level: HashMap<ParameterKey, ParamMetadata>,
    // Nested maps allow borrowed string lookups without allocating a key on
    // every unprobed-level request. Representatives keep their exact identity.
    by_type: HashMap<String, HashMap<String, ParameterKey>>,
}

impl ParamMetadataCache {
    fn insert(&mut self, key: ParameterKey, meta: ParamMetadata) {
        self.by_level.entry(key.clone()).or_insert(meta);
        self.by_type
            .entry(key.levtype.clone())
            .or_default()
            .entry(key.param.clone())
            .or_insert(key);
    }

    fn get(&self, key: &ParameterKey, allow_level_fallback: bool) -> Option<&ParamMetadata> {
        self.by_level.get(key).or_else(|| {
            if !allow_level_fallback {
                return None;
            }
            let representative = self.by_type.get(&key.levtype)?.get(&key.param)?;
            self.by_level.get(representative)
        })
    }
}

/// Default model run hours for ECMWF IFS (4 runs per day).
const DEFAULT_RUN_HOURS: &[u32] = &[0, 6, 12, 18];

/// Number of days to scan back (today + yesterday handles overnight transitions).
const SCAN_DAYS: u32 = 2;

/// Bound both in-flight index GETs and the number of sidecar bodies retained
/// before parsing. Shared by all level collections of a source.
const INDEX_FETCH_CONCURRENCY: usize = 8;

/// How often to force a full re-list of all run prefixes, ignoring the settled
/// skip. NWP runs publish sequentially so older runs are normally static, but a
/// provider can append late/corrected step files to an already-scanned run; a
/// periodic full scan catches those new paths within this bound while still
/// skipping the per-poll re-list the rest of the time. (Same-key content
/// rewrites of an existing index file are a separate, pre-existing limitation
/// of the path-keyed `known_indexes` dedup, not addressed here.)
const SETTLED_REVALIDATE_INTERVAL: Duration = Duration::from_secs(3600);

/// How [`GribEngine::scan_once`] enumerates index files.
enum ScanMode {
    /// Remote S3/HTTP: expand `prefix_pattern` over recent dates × run hours
    /// (the "now"-relative NWP layout).
    Remote { prefix_pattern: String },
    /// A fixed-prefix source (a local directory, or a remote `data_path` URL):
    /// list a single literal prefix — no date/run-hour templating, since the
    /// data is static. `prefix` is the literal sub-prefix under the store root
    /// (`""` = root). Named for the *prefix* behavior, not the backend: the
    /// store may be local or remote.
    FixedPrefix { prefix: String },
}

fn fixed_source_prefix(
    source: &ds_storage::object_store::path::Path,
    sub_prefix: Option<&str>,
) -> String {
    [source.as_ref(), sub_prefix.unwrap_or_default()]
        .into_iter()
        .map(|part| part.trim_matches('/'))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

/// Engine for serving GRIB2 NWP forecast data.
///
/// Discovers GRIB files via index sidecar files on S3/HTTP/local, fetches
/// individual parameters via byte-range reads, and serves them through EDR and
/// Maps APIs.
pub struct GribEngine {
    collection_id: String,
    family: Option<GribLevelType>,
    source: Arc<GribSource>,
}

/// One discovery/poll/cache owner, shared by all collection views.
struct GribSource {
    config: GribConfig,
    /// `config.time_window`, parsed once at load (#817) rather than on
    /// every poll.
    time_window: Option<TimeWindow>,
    catalog: ArcSwap<Catalog>,
    store: ds_storage::DataStore,
    scan_mode: ScanMode,
    grid_cache: Option<GridCache>,
    message_cache: Option<message_cache::MessageCache>,
    /// Edge-triggered stop signal for `poll_loop` (shared lifecycle, #481).
    shutdown: Shutdown,
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
    /// Parameter metadata keyed by short name AND level identity. Populated
    /// on the first successful header probe or decode of each selected product.
    param_meta: RwLock<ParamMetadataCache>,
    discovery: RwLock<discovery::Discovery>,
    /// Last attempted name, so failing probes cannot starve later parameters.
    probe_cursor: Mutex<Option<String>>,
}

impl GribEngine {
    fn catalog(&self) -> Arc<Catalog> {
        let catalog = self.source.catalog.load_full();
        match self.family {
            Some(family) => catalog.families.get(&family).cloned().unwrap_or_default(),
            None => catalog,
        }
    }

    /// Cheap collection views: all share the source's single poll loop,
    /// storage client, metadata and decoded-grid cache.
    pub fn level_collections(&self) -> Vec<Self> {
        let catalog = self.source.catalog.load();
        catalog
            .families
            .iter()
            .filter(|(_, c)| !c.runs.is_empty())
            .map(|(&family, _)| Self {
                collection_id: format!("{}-{}", self.collection_id, family.suffix()),
                family: Some(family),
                source: self.source.clone(),
            })
            .collect()
    }

    pub fn level_type(&self) -> Option<GribLevelType> {
        self.family
    }

    /// Returns the collection ID.
    pub fn collection_id(&self) -> &str {
        &self.collection_id
    }

    /// Return total bytes read from storage.
    pub fn storage_bytes_read(&self) -> u64 {
        self.source.store.bytes_read()
    }

    /// Return (hits, misses) for the grid cache, or (0, 0) if disabled.
    pub fn grid_cache_stats(&self) -> (u64, u64) {
        self.source
            .grid_cache
            .as_ref()
            .map(|c| c.stats())
            .unwrap_or((0, 0))
    }

    /// Return grid cache utilization as (bytes_used, capacity_bytes, entries).
    /// Zeroes if the cache is disabled.
    pub fn grid_cache_utilization(&self) -> (u64, u64, usize) {
        self.source
            .grid_cache
            .as_ref()
            .map(|c| (c.weight(), c.capacity(), c.len()))
            .unwrap_or((0, 0, 0))
    }

    /// Compressed-message cache counters and resident byte budget, per source.
    pub fn message_cache_metrics(&self) -> ds_cache::CacheMetrics {
        self.source
            .message_cache
            .as_ref()
            .map(|cache| cache.metrics())
            .unwrap_or(ds_cache::CacheMetrics {
                hits: 0,
                misses: 0,
                bytes: 0,
                capacity_bytes: 0,
            })
    }

    /// Create a new GRIB engine from config.
    pub fn new(collection_id: &str, config: &GribConfig) -> Result<Self, DataServerError> {
        // Parsed once: an invalid window fails the load instead of silently
        // widening, and the poll reuses the parsed value.
        let time_window = config
            .time_window
            .as_deref()
            .map(TimeWindow::parse)
            .transpose()?;
        // Data source: local `data_path` (a directory, or a fixed-prefix remote
        // URL) vs S3 `endpoint`+`bucket`. Mutual exclusivity is enforced at
        // config load (`GribConfig` validation); re-check the presence here so
        // the engine has a clear error if constructed directly.
        let (store, scan_mode) = if let Some(data_path) = config.data_path.as_deref() {
            let (store, source_prefix) = ds_storage::build_store(data_path).map_err(|e| {
                DataServerError::Config(format!(
                    "Collection '{collection_id}': failed to build store for data_path \
                     '{data_path}': {e}"
                ))
            })?;
            // Local stores are rooted at data_path; remote stores return its
            // object prefix separately. Append the optional literal sub-prefix.
            let prefix = fixed_source_prefix(&source_prefix, config.prefix_pattern.as_deref());
            (store, ScanMode::FixedPrefix { prefix })
        } else {
            let endpoint = config.endpoint.as_deref().ok_or_else(|| {
                DataServerError::Config(format!(
                    "Collection '{collection_id}': GRIB engine requires 'data_path' or 'endpoint'"
                ))
            })?;
            let bucket = config.bucket.as_deref().ok_or_else(|| {
                DataServerError::Config(format!(
                    "Collection '{collection_id}': GRIB engine requires 'bucket'"
                ))
            })?;
            let prefix_pattern = config.prefix_pattern.clone().ok_or_else(|| {
                DataServerError::Config(format!(
                    "Collection '{collection_id}': remote GRIB engine requires 'prefix_pattern'"
                ))
            })?;
            // The run prefixes are expanded on every poll; reject a template
            // (or run hour) they cannot be expanded from now, not then.
            validate_run_prefix_pattern(&prefix_pattern, run_hours(config))?;
            // Construct URL from endpoint+bucket for S3 region detection.
            let store_url = format!("{endpoint}/{bucket}/");
            let (store, _prefix) = ds_storage::build_store(&store_url).map_err(|e| {
                DataServerError::Config(format!(
                    "Collection '{collection_id}': failed to build store: {e}"
                ))
            })?;
            (store, ScanMode::Remote { prefix_pattern })
        };

        let grid_cache = GridCache::new(config.grid_cache_mb);
        let message_cache = message_cache::MessageCache::new(config.message_cache_mb);

        let index_format = index::IndexFormat::from_config(config.index_format.as_deref())
            .ok_or_else(|| {
                DataServerError::Config(format!(
                    "Collection '{collection_id}': invalid grib index_format"
                ))
            })?;

        let engine = Self {
            collection_id: collection_id.to_string(),
            family: None,
            source: Arc::new(GribSource {
                config: config.clone(),
                catalog: ArcSwap::new(Arc::new(Catalog::new())),
                store,
                scan_mode,
                grid_cache,
                message_cache,
                shutdown: Shutdown::new(),
                param_filter: config.parameters.clone(),
                time_window,
                known_indexes: Mutex::new(HashSet::new()),
                settled_prefixes: Mutex::new(HashSet::new()),
                last_full_scan: Mutex::new(None),
                index_format,
                param_meta: RwLock::new(ParamMetadataCache::default()),
                probe_cursor: Mutex::new(None),
                discovery: RwLock::new(discovery::Discovery::default()),
            }),
        };

        engine.refresh_discovery();

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
        let interval = std::time::Duration::from_secs(self.source.config.poll_interval_secs);
        let mut ticker = self.source.shutdown.ticker(interval, FirstTick::Skip);
        while ticker.tick().await {
            if let Err(e) = self.scan_once() {
                tracing::warn!(
                    "Collection '{}': GRIB poll failed: {}",
                    self.collection_id,
                    e
                );
            }
        }
        tracing::info!(
            "Collection '{}': GRIB poll loop shutting down",
            self.collection_id
        );
    }

    /// Signal the poll loop to stop.
    pub fn shutdown(&self) {
        self.source.shutdown.shutdown();
    }

    /// Perform one scan cycle: list index files across multiple dates and
    /// run hours, parse new ones, merge into the catalog.
    fn scan_once(&self) -> Result<(), DataServerError> {
        self.scan_at(Utc::now())
    }

    fn scan_at(&self, now: DateTime<Utc>) -> Result<(), DataServerError> {
        let index_suffix = self
            .source
            .config
            .index_suffix
            .as_deref()
            .unwrap_or(".index");
        let data_suffix = self
            .source
            .config
            .data_suffix
            .as_deref()
            .unwrap_or(".grib2");
        // Generate all prefixes to scan. Remote: expand over recent dates × run
        // hours, newest-first (skipping future runs). Local: a single literal
        // prefix (static data — no date/run templating).
        let prefixes = match &self.source.scan_mode {
            ScanMode::Remote { prefix_pattern } => {
                build_scan_prefixes(prefix_pattern, now, run_hours(&self.source.config))?
            }
            ScanMode::FixedPrefix { prefix } => vec![RunPrefix {
                reference_time: now,
                prefix: prefix.clone(),
            }],
        };

        // Optional filename substring filter. Applied in addition to the
        // index suffix match so that, for example, a GFS atmos directory
        // containing pgrb2.0p25 / pgrb2.0p50 / pgrb2b / goessimpgrb2 can be
        // narrowed to just the 0.25-degree product.
        let filename_contains = self.source.config.filename_contains.as_deref();

        // Cap how many runs we actually scan. `max_runs` is the number of
        // runs we want to keep in the catalog; there is no point scanning
        // older prefixes that would immediately be evicted. We iterate
        // prefixes newest-first and stop after collecting hits from exactly
        // that many runs.
        //
        // Without `max_runs` set, we fall back to listing every prefix.
        let scan_budget = self.source.config.max_runs;

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
        // Pace full revalidation independently of individual listing failures;
        // the newest run is still retried on every poll.
        let force_full = self
            .source
            .last_full_scan
            .lock()
            .unwrap()
            .is_none_or(|t| t.elapsed() >= SETTLED_REVALIDATE_INTERVAL);
        let settled_snapshot = self.source.settled_prefixes.lock().unwrap().clone();
        let mut listed_with_hits: Vec<String> = Vec::new();
        for RunPrefix { prefix, .. } in &prefixes {
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
            match self.source.store.list(&obj_prefix) {
                Ok(objects) => {
                    for obj in objects {
                        let loc = obj.location.as_ref();
                        // A publisher's in-progress `.name.index` is not an
                        // index yet: once parsed it would be marked known
                        // under its temporary name (#1009).
                        if !loc.ends_with(index_suffix) || temp_files::is_temporary_key(loc) {
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
            let window: HashSet<&str> = prefixes.iter().map(|run| run.prefix.as_str()).collect();
            let mut settled = self.source.settled_prefixes.lock().unwrap();
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
            *self.source.last_full_scan.lock().unwrap() = Some(Instant::now());
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
        }

        // Filter to only new index files (not seen before)
        let mut new_paths: Vec<_> = {
            let known = self.source.known_indexes.lock().unwrap();
            all_index_paths
                .iter()
                .filter(|p| !known.contains(p.as_ref()))
                .cloned()
                .collect()
        };
        new_paths.sort_by(|a, b| a.as_ref().cmp(b.as_ref()));

        if new_paths.is_empty() {
            tracing::debug!(
                "Collection '{}': no new index files ({} already known)",
                self.collection_id,
                all_index_paths.len()
            );
            // Metadata work is independent of discovery. With retention enabled,
            // continue below so existing steps can expire even during an outage.
            if self.source.time_window.is_none() {
                self.probe_new_parameters();
                return Ok(());
            }
        }

        tracing::debug!(
            "Collection '{}': found {} new index files ({} total; listed {}/{} prefixes)",
            self.collection_id,
            new_paths.len(),
            all_index_paths.len(),
            listed_prefixes,
            prefixes.len()
        );

        // Start from existing catalog for incremental merge
        let mut new_catalog = (*self.source.catalog.load_full()).clone();
        let mut ambiguities = diagnostics::IndexAmbiguities::default();

        // Fetch one bounded chunk at a time: get_many retains the completed
        // bodies until it returns. Results are in input order, preserving the
        // sorted merge order even when downloads finish out of order.
        for chunk in new_paths.chunks(INDEX_FETCH_CONCURRENCY) {
            // No HEAD per sidecar: the indexes are already listed and only
            // their contents are needed. Each GET keeps its own timeout.
            let results = match self
                .source
                .store
                .get_many(chunk, INDEX_FETCH_CONCURRENCY, None)
            {
                Ok(results) => results,
                Err(e) => {
                    tracing::warn!(
                        "Collection '{}': index batch fetch failed: {}",
                        self.collection_id,
                        e
                    );
                    continue;
                }
            };
            for (path, result) in chunk.iter().zip(results) {
                let bytes = match result {
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

                // Parse the sidecar; a wgrib2 tail length is resolved on fetch.
                let Some(parsed) = Self::parse_and_resolve(self.source.index_format, content)
                else {
                    continue;
                };

                let ref_time = parsed.reference_time;

                // Filter messages if param_filter is set
                let mut messages: Vec<_> = if let Some(filter) = &self.source.param_filter {
                    parsed
                        .messages
                        .into_iter()
                        .filter(|m| {
                            filter
                                .iter()
                                .any(|p| *p == m.param || m.step_kind.parameter_name(p) == m.param)
                        })
                        .collect()
                } else {
                    parsed.messages
                };

                if self.source.index_format == index::IndexFormat::Wgrib2 {
                    ambiguities.record(
                        &grib_url,
                        &messages,
                        self.source.config.level_types.as_deref(),
                    );
                }

                let origin: Arc<str> = Arc::from(grib_url.as_str());
                for message in &mut messages {
                    message.source_url = Some(origin.clone());
                }

                let run = new_catalog
                    .runs
                    .entry(ref_time)
                    .or_insert_with(|| ForecastRun {
                        reference_time: ref_time,
                        steps: BTreeMap::new(),
                    });

                // Providers may publish separate surface/pressure/model files,
                // or one file per parameter, for the same valid time.
                run.steps
                    .entry(parsed.step)
                    .or_insert_with(|| StepFile {
                        grib_url,
                        messages: Vec::new(),
                    })
                    .messages
                    .extend(messages);

                // Mark as known
                self.source
                    .known_indexes
                    .lock()
                    .unwrap()
                    .insert(path.as_ref().to_string());
            }
        }

        ambiguities.emit(&self.collection_id);

        // Apply time_window filtering: remove steps whose valid times fall outside the window
        if let Some(tw) = &self.source.time_window {
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

        // Apply max_runs eviction
        if let Some(max_runs) = self.source.config.max_runs {
            new_catalog.evict(max_runs);
        }

        // Clean up known_indexes: remove entries for runs that were evicted
        {
            let mut known = self.source.known_indexes.lock().unwrap();
            let valid_prefixes: HashSet<String> = new_catalog
                .runs
                .values()
                .flat_map(|r| {
                    r.steps.values().flat_map(|s| {
                        std::iter::once(s.grib_url.clone())
                            .chain(s.messages.iter().map(|m| s.message_url(m).to_owned()))
                    })
                })
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

        new_catalog.refresh_metadata();
        new_catalog.refresh_families(
            self.source
                .config
                .level_types
                .as_deref()
                .unwrap_or_default(),
        );
        self.publish_catalog(new_catalog);

        // Probe one message per distinct short name in the newest run to
        // populate the parameter metadata cache. Without this, the EDR
        // /collections endpoint returns empty labels/units until the first
        // actual query is served.
        self.probe_new_parameters();

        Ok(())
    }

    /// For each distinct short name in the newest forecast run that has not
    /// yet been seen, read the message headers via byte-range so the
    /// WMO triple populates the parameter metadata cache.
    ///
    /// Any failures are logged and swallowed — the probe is best-effort and
    /// the metadata cache will eventually fill in as real queries land on
    /// the missing parameters anyway.
    fn probe_new_parameters(&self) {
        let catalog = self.source.catalog.load_full();
        let catalogs: Vec<&Catalog> = if self.source.config.level_types.is_some() {
            catalog.families.values().map(AsRef::as_ref).collect()
        } else {
            vec![&catalog]
        };
        let geometry_probes = self.geometry_probes(&catalog);
        let geometry_ids: HashSet<_> = geometry_probes
            .iter()
            .map(|&(file, entry)| (file.message_url(entry), entry.offset))
            .collect();
        let mut todo = {
            let cache = self.source.param_meta.read().unwrap();
            let mut todo = BTreeMap::new();
            for catalog in catalogs {
                let Some(run) = catalog.latest_run() else {
                    continue;
                };
                let Some(keys) = catalog.parameter_keys(&run.reference_time) else {
                    continue;
                };
                for key in keys
                    .values()
                    .filter(|key| !cache.by_level.contains_key(*key))
                {
                    if let Some((sf, m)) = run
                        .steps
                        .values()
                        .find_map(|sf| sf.messages.iter().find(|m| key.matches(m)).map(|m| (sf, m)))
                    {
                        todo.insert(
                            format!("{}:{}:{:?}", key.param, key.levtype, key.level),
                            (sf, m),
                        );
                    }
                }
            }
            for (file, entry) in geometry_probes {
                todo.insert(
                    format!("~geometry:{}:{}", file.message_url(entry), entry.offset),
                    (file, entry),
                );
            }
            let mut seen = HashSet::new();
            todo.into_iter()
                .filter(|(_, (file, entry))| seen.insert((file.message_url(entry), entry.offset)))
                .collect::<Vec<_>>()
        };

        if todo.is_empty() {
            return;
        }

        tracing::debug!(
            "Collection '{}': probing {} message headers to populate metadata",
            self.collection_id,
            todo.len()
        );

        // Cap the probe budget to avoid a 700-request burst on an unfiltered
        // wgrib2 catalog. Users are expected to set `parameters` when using
        // wgrib2 — see the warning emitted elsewhere.
        const MAX_PROBES_PER_SCAN: usize = 32;
        if let Some(last) = self.source.probe_cursor.lock().unwrap().as_deref() {
            let start = todo.partition_point(|(key, _)| key.as_str() <= last);
            let start = start % todo.len();
            todo.rotate_left(start);
        }
        // Publish bounds promptly even when hundreds of parameter labels are
        // pending. Stable ordering preserves the cursor among the other jobs;
        // these (at most four) reads remain inside the same 32-message budget.
        todo.sort_by_key(|(_, (file, entry))| {
            !geometry_ids.contains(&(file.message_url(entry), entry.offset))
        });
        todo.truncate(MAX_PROBES_PER_SCAN);
        let entries: Vec<_> = todo
            .iter()
            .map(|(_, (file, entry))| (file.message_url(entry), *entry))
            .collect();
        let results = metadata::read_batch(&self.source.store, &entries);
        // Apply in cursor order, preserving deterministic metadata fallback
        // representatives even when the reads complete out of order.
        let mut changed = false;
        for ((cursor, (file, entry)), result) in todo.into_iter().zip(results) {
            *self.source.probe_cursor.lock().unwrap() = Some(cursor);
            match result {
                Ok(meta) => {
                    changed |= self.populate_message_metadata(&entry.key(), &meta, entry.step_kind);
                    changed |=
                        self.populate_geometry(file.message_url(entry), entry, meta.geometry);
                }
                Err(e) => tracing::debug!(
                    "Collection '{}': probe for parameter '{}' failed: {e}",
                    self.collection_id,
                    entry.param
                ),
            }
        }
        if changed {
            self.refresh_discovery();
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
    fn parse_and_resolve(format: index::IndexFormat, content: &str) -> Option<index::IndexResult> {
        match format {
            index::IndexFormat::EcmwfJson => index::parse_ecmwf_json(content),
            index::IndexFormat::Wgrib2 => {
                let parsed = wgrib2_index::parse_wgrib2(content)?;

                // wgrib2 indexes cover a single forecast step per file. We
                // derive the nominal step from the first message (all
                // messages in the same file share it after aggregate filter).
                let nominal_step = parsed.messages.first()?.nominal_step;
                if parsed
                    .messages
                    .iter()
                    .any(|m| m.nominal_step != nominal_step)
                {
                    tracing::warn!(
                        "wgrib2 index contains mixed valid times; refusing to mislabel aggregates"
                    );
                    return None;
                }

                // Convert ParsedMessage → MessageEntry directly. The tail
                // record keeps `length = None` and is resolved lazily in
                // `fetch_grid` when someone actually asks for it.
                let messages: Vec<catalog::MessageEntry> = parsed
                    .messages
                    .into_iter()
                    .map(|m| catalog::MessageEntry {
                        source_url: None,
                        param: m.step_kind.parameter_name(&m.short_name),
                        step_kind: m.step_kind,
                        levtype: m.levtype.into_owned(),
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

    /// Resolve the run's canonical level exactly; never substitute another
    /// level when that product is absent from an individual forecast step.
    /// A parameter the run lacks is a bad request; a step without its field
    /// (a level one step of a pressure view lacks) is missing data, a 404.
    fn fetch_grid(
        &self,
        step_file: &StepFile,
        param: &str,
        keys: &ParameterKeys,
    ) -> Result<Arc<DecodedGrid>, DataServerError> {
        let key = keys.get(param).ok_or_else(|| {
            DataServerError::InvalidParameter(format!(
                "Parameter '{param}' is unavailable in this collection and run"
            ))
        })?;
        let entry = step_file
            .messages
            .iter()
            .find(|m| key.matches(m))
            .ok_or_else(|| {
                DataServerError::LocationNotFound(format!(
                    "Parameter '{param}' at its canonical level not found in forecast step"
                ))
            })?;
        self.fetch_grid_by_entry(step_file.message_url(entry), entry)
    }

    /// One map field of `run`'s `step_file`: `parameter` (else the step's
    /// default product) at the requested or the run's first level,
    /// resampled onto the request in display units.
    fn render_field(
        &self,
        catalog: &Catalog,
        run: &ForecastRun,
        step_file: &StepFile,
        parameter: Option<&str>,
        field: &MapField<'_>,
    ) -> Result<RasterTile, DataServerError> {
        let keys = catalog
            .parameter_keys(&run.reference_time)
            .cloned()
            .unwrap_or_default();
        let requested = field.z.map(|v| [v]);
        let levels = self.selected_levels(
            catalog,
            run.reference_time,
            requested.as_ref().map(|a| a.as_slice()),
        )?;
        let keys = Self::keys_at_level(&keys, levels[0]);

        // Determine parameter to render
        let param_name = parameter.unwrap_or_else(|| {
            // Default to first near-surface parameter
            step_file
                .default_message()
                .map(|m| m.param.as_str())
                .unwrap_or("2t")
        });

        let grid = self.fetch_grid(step_file, param_name, &keys)?;

        // Apply unit conversion so colormap ranges use display units.
        // fetch_grid populates the metadata cache from the decoded message's
        // WMO triple on first decode, so this lookup is safe here.
        let meta = self.param_metadata_for(&keys, param_name);
        let MapField {
            bbox,
            width,
            height,
            output_crs,
            ..
        } = *field;
        // Compact f32 tile (#475): 4 B/px instead of a boxed Option<f64>;
        // the conversion is fused into the sampling pass.
        let data = if meta.display.has_conversion() {
            grid.resample_f32(bbox, width, height, output_crs, |raw| {
                meta.display.convert(raw)
            })
        } else {
            grid.resample_f32(bbox, width, height, output_crs, |raw| raw)
        };

        Ok(RasterTile {
            width,
            height,
            values: RasterValues::F32 { data, nodata: None },
        })
    }

    fn fetch_grid_by_entry(
        &self,
        grib_url: &str,
        entry: &catalog::MessageEntry,
    ) -> Result<Arc<DecodedGrid>, DataServerError> {
        let load = || {
            let path = ds_storage::object_store::path::Path::from(grib_url);
            match &self.source.message_cache {
                Some(cache) => cache.read(&self.source.store, &path, entry),
                None => reader::read_message(&self.source.store, &path, entry),
            }
            .map(Arc::new)
        };
        let grid = match &self.source.grid_cache {
            Some(cache) => cache.get_or_insert_with(grib_url, entry.offset, load)?,
            None => load()?,
        };
        self.populate_metadata(&entry.key(), &grid, entry.step_kind);
        if self.populate_geometry(
            grib_url,
            entry,
            metadata::MessageMetadata::from(grid.as_ref()).geometry,
        ) {
            self.refresh_discovery();
        }
        Ok(grid)
    }

    /// Populate the parameter metadata cache from a decoded grid, using the
    /// WMO triple *and* the Code Table 4.5 surface type carried by the
    /// message itself (not a hardcoded short-name table). No-op if the
    /// parameter and level identity are already cached.
    fn populate_metadata(
        &self,
        key: &ParameterKey,
        grid: &DecodedGrid,
        step_kind: wgrib2_index::StepKind,
    ) {
        if self.populate_message_metadata(key, &metadata::MessageMetadata::from(grid), step_kind) {
            self.refresh_discovery();
        }
    }

    fn populate_message_metadata(
        &self,
        key: &ParameterKey,
        message: &metadata::MessageMetadata,
        step_kind: wgrib2_index::StepKind,
    ) -> bool {
        let short_name = &key.param;
        {
            let cache = self.source.param_meta.read().unwrap();
            if cache.by_level.contains_key(key) {
                return false;
            }
        }

        let (discipline, category, number) = message.triple;
        let centre = message.centre;
        let mut meta = match units::lookup(centre, discipline, category, number) {
            Some(info) => ParamMetadata {
                base_label: info.label.to_string(),
                source_unit: info.source_unit,
                display: units::default_display(info.source_unit),
                first_surface_type: None,
                first_surface_value: None,
                window_qualifier: None,
                triple: None,
                uv_frame: ds_core::wind::VectorFrame::Unknown,
            },
            None => {
                tracing::debug!(
                    "Unknown WMO triple ({centre}, {discipline}, {category}, {number}) \
                     for short name '{short_name}'; falling back to identity conversion"
                );
                ParamMetadata::placeholder(short_name)
            }
        };

        // Attach the surface type from the message headers so that
        // otherwise-identical parameters at different levels (e.g. msl vs
        // sp, both under WMO triple (0, 3, 0) "Pressure") can be told apart
        // in the rendered label.
        meta.window_qualifier = step_kind.qualifier();
        meta.first_surface_type = Some(message.first_surface_type);
        meta.first_surface_value = message.first_surface_value;
        meta.triple = Some(message.triple);
        meta.uv_frame = message.uv_frame;

        let mut cache = self.source.param_meta.write().unwrap();
        cache.insert(key.clone(), meta);
        true
    }

    /// Look up cached parameter metadata. Returns a placeholder (identity
    /// conversion, empty unit string) if the short name has not yet been
    /// populated — typically because its headers have not been read yet.
    fn param_metadata(&self, catalog: &Catalog, short_name: &str) -> ParamMetadata {
        catalog
            .latest_run()
            .and_then(|run| catalog.parameter_keys(&run.reference_time))
            .map(|keys| self.param_metadata_for(keys, short_name))
            .unwrap_or_else(|| ParamMetadata::placeholder(short_name))
    }

    fn param_metadata_for(&self, keys: &ParameterKeys, short_name: &str) -> ParamMetadata {
        let metadata = self.source.param_meta.read().unwrap();
        let mut meta = keys
            .get(short_name)
            .and_then(|key| metadata.get(key, self.vertical_kind().is_some()))
            .cloned()
            .unwrap_or_else(|| ParamMetadata::placeholder(short_name));
        // A vertical collection's parameter describes the whole axis. Its
        // level belongs in the domain, not a misleading fixed-level label.
        if self.vertical_kind().is_some() {
            meta.first_surface_type = None;
            meta.first_surface_value = None;
        }
        meta
    }
}

/// The request geometry of a map field: everything but the step and the
/// parameter.
#[derive(Clone, Copy)]
struct MapField<'a> {
    bbox: [f64; 4],
    width: u32,
    height: u32,
    output_crs: &'a OutputCrs,
    z: Option<f64>,
}

/// The run+step a map render (WMS/Maps/Tiles) of `parameters` reads at
/// `time`. Shared by `get_raster_tile(s)` and every `resolve_*` override,
/// so the #507/#521 cache keys name the step rendered. EDR queries match
/// times exactly instead ([`resolve_run`], [`grid_steps`]).
///
/// With no parameters (the collection's default layer): the nearest step
/// of [`covering_run`], so a map `TIME` snaps, or the run's last step for
/// `None`.
///
/// With parameters, only the steps carrying all of them count (#1005): an
/// hour-window aggregate exists at some steps of a run, never at its
/// analysis. Of the pinned run, else of the runs newest first:
/// 1. the first whose carrying steps span `time`, at its nearest such step,
///    so a newer run lacking the parameter then does not hide an older one;
/// 2. else, `time` within any run's steps, the carrying step nearest `time`
///    of any of them, the newer run's on a tie: what a WMS
///    `nearestValue` time dimension promises;
/// 3. else `time` is outside the data: the no-parameter error.
///
/// A pinned run without the parameters is
/// [`DataServerError::LocationNotFound`] (404), a parameter no run has
/// [`DataServerError::InvalidParameter`]. `None` is the last carrying step
/// of the pinned run, else of the newest run that has one.
///
/// Pinning the run this picks, as the API layers do, picks the same step
/// at `time` and at that step's valid time: the cache key names it.
fn select_run_step<'a>(
    catalog: &'a Catalog,
    parameters: &[&str],
    reference_time: Option<DateTime<Utc>>,
    time: Option<DateTime<Utc>>,
) -> Result<(&'a ForecastRun, u32, &'a StepFile), DataServerError> {
    if parameters.is_empty() {
        return select_default_run_step(catalog, reference_time, time);
    }
    let pinned = pinned_run(catalog, reference_time)?;
    if let Some(unknown) = parameters.iter().find(|&&p| !catalog.has_parameter(p)) {
        return Err(DataServerError::InvalidParameter(format!(
            "Parameter '{unknown}' is unavailable in this collection"
        )));
    }
    let runs = || {
        pinned.into_iter().chain(
            catalog
                .runs
                .values()
                .rev()
                .filter(move |_| pinned.is_none()),
        )
    };
    let carried = |run: &'a ForecastRun| catalog.carried_steps(run, parameters);
    let missing = || {
        DataServerError::LocationNotFound(format!(
            "Parameter '{}' has no forecast step in the selected run",
            parameters.join("', '")
        ))
    };
    let Some(time) = time else {
        return runs()
            .find_map(|run| carried(run).last().map(|(step, file)| (run, step, file)))
            .ok_or_else(missing);
    };
    let valid = |run: &ForecastRun, step: u32| {
        run.reference_time + chrono::Duration::hours(i64::from(step))
    };
    let spanning = runs().find_map(|run| {
        let steps = carried(run);
        steps
            .covers(time)
            .then(|| steps.nearest(time))
            .flatten()
            .map(|(step, file)| (run, step, file))
    });
    if let Some(found) = spanning {
        return Ok(found);
    }
    let within = |run: &ForecastRun| catalog::CarriedSteps::Every(run).covers(time);
    let outside = match pinned {
        None => !catalog.runs.values().any(within),
        // A pinned run serves a time outside its own steps only where the
        // unpinned selection chooses that run: the API layers resolve the
        // run first and pin it (#521), so the pinned render must reproduce
        // that choice. Any other pin keeps the default layer's error rather
        // than snapping to a step of a run that cannot answer the time.
        Some(pin) => {
            !within(pin)
                && !matches!(
                    select_run_step(catalog, parameters, None, Some(time)),
                    Ok((run, ..)) if run.reference_time == pin.reference_time
                )
        }
    };
    if outside {
        // Outside the data: the default layer's errors.
        return Err(DataServerError::InvalidParameter(match pinned {
            Some(_) => format!("No forecast step for time {time}"),
            None => format!("No forecast run covers time {time}"),
        }));
    }
    // `min_by_key` keeps the first of equals: the newer run.
    runs()
        .filter_map(|run| {
            carried(run)
                .nearest(time)
                .map(|(step, file)| (run, step, file))
        })
        .min_by_key(|&(run, step, _)| (valid(run, step) - time).abs())
        .ok_or_else(missing)
}

/// [`select_run_step`] without parameters: the collection's default layer.
fn select_default_run_step(
    catalog: &Catalog,
    reference_time: Option<DateTime<Utc>>,
    time: Option<DateTime<Utc>>,
) -> Result<(&ForecastRun, u32, &StepFile), DataServerError> {
    let run = covering_run(catalog, reference_time, time)?;
    let (step, sf) = match time {
        Some(time) => run.find_step_for_time(time).ok_or_else(|| {
            DataServerError::InvalidParameter(format!("No forecast step for time {time}"))
        })?,
        None => {
            let (&step, sf) =
                run.steps.iter().next_back().ok_or_else(|| {
                    DataServerError::Engine("forecast run has no steps".to_string())
                })?;
            (step, sf)
        }
    };
    Ok((run, step, sf))
}

/// The run `reference_time` pins: exactly that run (absent ⇒
/// [`DataServerError::ReferenceTimeNotFound`] → 404), or `None` to let the
/// caller choose. See [`instances::select_run`].
fn pinned_run(
    catalog: &Catalog,
    reference_time: Option<DateTime<Utc>>,
) -> Result<Option<&ForecastRun>, DataServerError> {
    if catalog.runs.is_empty() {
        return Err(DataServerError::Engine(
            "No forecast data available".to_string(),
        ));
    }
    reference_time
        .map(|rt| {
            instances::select_run(&catalog.runs, Some(rt))
                .map(|(_, r)| r)
                .ok_or_else(|| {
                    DataServerError::ReferenceTimeNotFound(format!(
                        "no forecast run for reference time {rt}"
                    ))
                })
        })
        .transpose()
}

/// The run a map render at `time`, or an M trajectory starting at `time`,
/// reads: the pinned run, else the newest run whose published valid-time
/// extent covers `time` (both snap to its nearest step), else the latest
/// run for `None`. An incomplete newer run does not hide a covering older
/// one.
fn covering_run(
    catalog: &Catalog,
    reference_time: Option<DateTime<Utc>>,
    time: Option<DateTime<Utc>>,
) -> Result<&ForecastRun, DataServerError> {
    if let Some(run) = pinned_run(catalog, reference_time)? {
        return Ok(run);
    }
    match time {
        Some(time) => catalog
            .runs
            .values()
            .rev()
            .find(|r| r.find_step_for_time(time).is_some())
            .ok_or_else(|| {
                DataServerError::InvalidParameter(format!("No forecast run covers time {time}"))
            }),
        None => Ok(catalog.latest_run().expect("runs is non-empty")),
    }
}

/// Select the forecast run an EDR query serves (position, area, radius,
/// cube, 2-D and Z trajectories), so run selection and its error mapping
/// are identical on every query.
///
/// The pinned run, else, for a `datetime` window, a run with a step valid
/// inside it (`/req/core/datetime-response` A and F): the newest that also
/// covers the window's start, else the newest with any step inside, which
/// is how an open start (`../end`) or a window starting before a run finds
/// one. An instant `(t, t)` needs a step valid at exactly `t`, never the
/// nearest. No run with a step inside ⇒ [`no_step_within`] (404). No
/// datetime: the latest run.
fn resolve_run(
    catalog: &Catalog,
    reference_time: Option<DateTime<Utc>>,
    datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> Result<&ForecastRun, DataServerError> {
    if let Some(run) = pinned_run(catalog, reference_time)? {
        return Ok(run);
    }
    let Some((start, end)) = datetime else {
        return Ok(catalog.latest_run().expect("runs is non-empty"));
    };
    let intersects = |run: &&ForecastRun| run.steps_within(start, end).next().is_some();
    let newest_first = || catalog.runs.values().rev().filter(intersects);
    newest_first()
        .find(|run| run.find_step_for_time(start).is_some())
        .or_else(|| newest_first().next())
        .ok_or_else(|| no_step_within(start, end))
}

/// The steps an area, radius or cube query reads from `run`: every step
/// valid inside the `datetime` window (an instant: the step at exactly that
/// time, never the nearest), else the run's last step. None inside ⇒
/// [`no_step_within`] (404).
fn grid_steps(
    run: &ForecastRun,
    datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> Result<Vec<(DateTime<Utc>, &StepFile)>, DataServerError> {
    let Some((start, end)) = datetime else {
        let (&step, file) = run
            .steps
            .iter()
            .next_back()
            .ok_or_else(|| DataServerError::Engine("forecast run has no steps".to_string()))?;
        let time = run.reference_time + chrono::Duration::hours(i64::from(step));
        return Ok(vec![(time, file)]);
    };
    let steps: Vec<_> = run.steps_within(start, end).collect();
    if steps.is_empty() {
        return Err(no_step_within(start, end));
    }
    Ok(steps)
}

/// No forecast step is valid inside the requested `datetime`: the 404 an
/// instant without data gets, which a datetime list skips.
fn no_step_within(start: DateTime<Utc>, end: DateTime<Utc>) -> DataServerError {
    let bound = |time: DateTime<Utc>| {
        if time == DateTime::<Utc>::MIN_UTC || time == DateTime::<Utc>::MAX_UTC {
            "..".to_string()
        } else {
            time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        }
    };
    let datetime = if start == end {
        bound(start)
    } else {
        format!("{}/{}", bound(start), bound(end))
    };
    DataServerError::LocationNotFound(format!("No forecast step matches datetime {datetime}"))
}

// ---------------------------------------------------------------------------
// Engine trait (EDR)
// ---------------------------------------------------------------------------

impl EdrEngine for GribEngine {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        // Gridded data has no discrete locations
        Ok(Vec::new())
    }

    /// Each forecast run is an EDR instance (latest last). Valid times are
    /// `reference_time + step` for every step retained in that run.
    fn get_instances(&self) -> Vec<RunInfo> {
        let catalog = self.catalog();
        instances::build_instances(&catalog.runs, |_, run| run.valid_times())
    }

    fn has_instances(&self) -> bool {
        !self.catalog().runs.is_empty()
    }

    fn instance_reference_times(&self) -> Vec<DateTime<Utc>> {
        self.catalog().runs.keys().copied().collect()
    }

    fn find_instance(&self, reference_time: DateTime<Utc>) -> Option<RunInfo> {
        let catalog = self.catalog();
        catalog.runs.get(&reference_time).map(|run| RunInfo {
            reference_time,
            valid_times: run.valid_times(),
        })
    }

    fn query_location(
        &self,
        _location_id: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Err(DataServerError::InvalidParameter(
            "GRIB engine does not support location queries; use position or area instead"
                .to_string(),
        ))
    }

    fn get_parameters(&self) -> Vec<String> {
        self.catalog().all_params()
    }

    fn get_parameter_descriptions(
        &self,
    ) -> std::collections::HashMap<String, ParameterDescription> {
        let catalog = self.catalog();
        let all = catalog.all_params();
        let mut map = std::collections::HashMap::new();
        for p in all {
            let meta = self.param_metadata(&catalog, &p);
            map.insert(
                p.clone(),
                ParameterDescription {
                    label: meta.label(),
                    unit: meta.display.display_unit.to_string(),
                    observed_property: p,
                    standard_name: None,
                },
            );
        }
        map
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        self.catalog().temporal_extent()
    }

    fn get_available_times(&self) -> Option<Vec<DateTime<Utc>>> {
        let times = self.catalog().all_valid_times();
        if times.is_empty() {
            None
        } else {
            Some(times)
        }
    }

    /// A parameter some steps lack (an hour-window aggregate, #1005)
    /// advertises the valid times of the steps that carry it: the map
    /// layers' axis (`MapEngine::parameter_times`).
    fn get_parameter_available_times(&self, parameter: &str) -> Option<Vec<DateTime<Utc>>> {
        self.catalog()
            .parameter_times(parameter)
            .map(|times| times.to_vec())
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        self.raster_info_shared().spatial_extent
    }

    fn get_vertical_extent(&self) -> Option<ds_core::vertical::VerticalDimension> {
        self.vertical_extent(&self.catalog())
    }

    /// Cube only on the pressure and model-level views: a cube is a
    /// bbox × vertical levels query (#925).
    fn supported_query_types(&self) -> Vec<String> {
        let mut types = vec![
            "position".to_string(),
            "area".to_string(),
            "radius".to_string(),
            "trajectory".to_string(),
        ];
        if self.vertical_kind().is_some() {
            types.push("cube".to_string());
        }
        types
    }

    /// Values along a WKT `LINESTRING` / `Z` / `M` / `ZM` path (#926), each
    /// field it crosses read once; see `trajectory.rs`.
    fn query_trajectory(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        self.query_batched_trajectory(coords, datetime, parameters, z, reference_time)
    }

    fn query_position(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        let mut result = None;
        self.query_positions(
            &[coords.to_owned()],
            datetime,
            parameters,
            z,
            reference_time,
            &mut |response| {
                result = Some(response);
                Ok(())
            },
        )?;
        result.ok_or_else(|| DataServerError::Engine("Position query produced no response".into()))
    }

    fn query_positions(
        &self,
        points: &[String],
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
        emit: &mut dyn FnMut(CoverageResponse) -> Result<(), DataServerError>,
    ) -> Result<(), DataServerError> {
        self.query_batched_positions(points, datetime, parameters, z, reference_time, emit)
    }

    fn query_area(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        self.query_batched_area(coords, datetime, parameters, z, reference_time)
    }

    fn query_cube(
        &self,
        bbox: &ds_core::feature::Bbox,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        resolution: ds_core::cube::CubeResolution,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        self.query_batched_cube(bbox, datetime, parameters, z, resolution, reference_time)
    }
}

// ---------------------------------------------------------------------------
// MapEngine trait (WMS/Maps/Tiles)
// ---------------------------------------------------------------------------

impl MapEngine for GribEngine {
    fn content_version(&self) -> u64 {
        // Separate files can extend the same run/step and change the default
        // level without changing either resolved time. Each view's immutable
        // snapshot carries its own version; unrelated families keep cache hits.
        // Nonzero also prevents explicit-TIME HTTP responses being immutable.
        self.catalog().content_version.max(1)
    }

    #[allow(clippy::too_many_arguments)] // bbox/size/time/crs/parameter/z/reference_time are all genuine selectors
    fn get_raster_tile(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        output_crs: &OutputCrs,
        parameter: Option<&str>,
        z: Option<f64>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        let catalog = self.catalog();
        let (run, _, step_file) =
            select_run_step(&catalog, parameter.as_slice(), reference_time, time)?;
        let field = MapField {
            bbox,
            width,
            height,
            output_crs,
            z,
        };
        self.render_field(&catalog, run, step_file, parameter, &field)
    }

    /// Every band from the one step carrying them all that
    /// `resolve_parameters_time` names (#507): never a band snapped to
    /// another step.
    #[allow(clippy::too_many_arguments)] // mirrors get_raster_tile
    fn get_raster_tiles(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        output_crs: &OutputCrs,
        parameters: &[&str],
        z: Option<f64>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<Vec<RasterTile>, DataServerError> {
        if parameters.is_empty() {
            return Ok(Vec::new());
        }
        let catalog = self.catalog();
        let (run, _, step_file) = select_run_step(&catalog, parameters, reference_time, time)?;
        let field = MapField {
            bbox,
            width,
            height,
            output_crs,
            z,
        };
        parameters
            .iter()
            .map(|&parameter| self.render_field(&catalog, run, step_file, Some(parameter), &field))
            .collect()
    }

    fn resolve_time(
        &self,
        time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        self.resolve_parameter_time(None, time, reference_time)
    }

    fn resolve_reference_time(
        &self,
        time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        self.resolve_parameter_reference_time(None, time, reference_time)
    }

    /// The cache-key authority (#507): the exact valid time
    /// `get_raster_tile` renders for `parameter`, through the SAME
    /// `select_run_step`, so an aggregate keys the step that carries it
    /// (#1005). Borrowing form: no `StepFile` clone on the per-request
    /// resolve path. A failed selection echoes the requested time: the
    /// render fails the same selection and caches nothing.
    fn resolve_parameter_time(
        &self,
        parameter: Option<&str>,
        time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        self.resolve_parameters_time(parameter.as_slice(), time, reference_time)
    }

    /// The run-axis cache-key authority (#521): the exact run
    /// `get_raster_tile` renders for `parameter`, through the SAME
    /// `select_run_step`, cross-run fallback included: a valid time the
    /// newest run does not carry the parameter at keys the older run
    /// rendered. A failed selection echoes the request, as above.
    fn resolve_parameter_reference_time(
        &self,
        parameter: Option<&str>,
        time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        let catalog = self.catalog();
        select_run_step(&catalog, parameter.as_slice(), reference_time, time)
            .map(|(run, _, _)| Some(run.reference_time))
            .unwrap_or(reference_time)
    }

    /// The step `get_raster_tiles` renders: the nearest carrying every one
    /// of `parameters`, of the run `select_run_step` picks for them all.
    /// GRIB's selection is not the default's latest-not-after over the
    /// union axes, which ignores the run.
    fn resolve_parameters_time(
        &self,
        parameters: &[&str],
        time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        let catalog = self.catalog();
        select_run_step(&catalog, parameters, reference_time, time)
            .map(|(run, step, _)| run.reference_time + chrono::Duration::hours(i64::from(step)))
            .ok()
            .or(time)
    }

    /// The valid times of the steps carrying `parameter` (#1005), over
    /// every retained run, where some steps lack it: from the snapshot.
    fn parameter_times(&self, parameter: &str) -> Option<Arc<[DateTime<Utc>]>> {
        self.catalog().parameter_times(parameter)
    }

    fn raster_info(&self) -> RasterInfo {
        (*self.raster_info_shared()).clone()
    }

    fn raster_info_shared(&self) -> Arc<RasterInfo> {
        let discovery = self.source.discovery.read().unwrap();
        discovery
            .views
            .get(&self.family)
            .unwrap_or(&discovery.empty)
            .clone()
    }
}

/// Each view's wind components for `ds_core::wind::DerivedWind` (#897): an
/// `Arc` clone of the snapshot rebuilt with the view's `RasterInfo`.
impl ds_core::wind::WindSource for GribEngine {
    fn wind_facts(&self) -> Arc<ds_core::wind::WindFacts> {
        let discovery = self.source.discovery.read().unwrap();
        discovery
            .wind
            .get(&self.family)
            .cloned()
            .unwrap_or_else(ds_core::wind::WindFacts::none)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse "POINT(lon lat)" or "lon,lat" coordinates.
fn parse_coords(coords: &str) -> Result<(f64, f64), DataServerError> {
    ds_core::feature::parse_point_coords(coords).map(|(lat, lon)| (lon, lat))
}

/// The run hours substituted into a remote `prefix_pattern`'s `{run}`.
fn run_hours(config: &GribConfig) -> &[u32] {
    config.run_hours.as_deref().unwrap_or(DEFAULT_RUN_HOURS)
}

/// The run prefixes to scan at `now`: every run of the last [`SCAN_DAYS`]
/// UTC days, today included, up to `now`.
///
/// The shared [`expand_run_prefixes`] formats each day's strftime date and
/// substitutes each run hour into `{run}`, zero-padded (`00`, `06`, …).
/// Runs come newest first, so the scan can stop once it has `max_runs`
/// runs, and `settle_completed_runs` relies on that order. A future run is
/// skipped, since a run cannot be published before its reference time. A
/// pattern without `{run}` is one prefix per day, ordered by the day's
/// 00 UTC.
fn build_scan_prefixes(
    pattern: &str,
    now: DateTime<Utc>,
    run_hours: &[u32],
) -> Result<Vec<RunPrefix>, DataServerError> {
    let first_day = now.date_naive() - chrono::Days::new(u64::from(SCAN_DAYS - 1));
    let start = first_day.and_time(chrono::NaiveTime::MIN).and_utc();
    expand_run_prefixes(pattern, run_hours, start, now)
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
/// which returns runs newest first. `window` is the current scan window;
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
    use crate::test_support::{message, TestSource};

    #[test]
    fn canonical_level_is_shared_by_metadata_position_area_and_maps() {
        let source = TestSource::new();
        // The pressure level has the same numeric value as the height level,
        // and occurs first. Matching (name, numeric level) is insufficient.
        source.write(
            "f000",
            &[
                ("TMP", "2 mb", message(0, 200.0, [0; 4], 100, 200)),
                ("TMP", "2 m above ground", message(0, 280.0, [0; 4], 103, 2)),
            ],
            0,
        );
        // Canonical temperature is missing here; it must not switch to 2 hPa.
        source.write(
            "f001",
            &[("TMP", "2 mb", message(0, 200.0, [0; 4], 100, 200))],
            1,
        );
        let engine = GribEngine::new("levels", &source.config()).unwrap();
        let reference: DateTime<Utc> = "2026-04-05T00:00:00Z".parse().unwrap();
        assert!(engine.get_parameter_descriptions()["TMP"]
            .label
            .contains("2 m above ground"));
        let CoverageResponse::Single(series) = engine
            .query_position("POINT(0.5 0.5)", None, None, None, None)
            .unwrap()
        else {
            panic!()
        };
        let values = &series.ranges["TMP"].values;
        assert!((values[0].unwrap() - 6.85).abs() < 1e-9);
        assert_eq!(values[1], None);
        assert!(series.parameters["TMP"].label.contains("2 m above ground"));
        let CoverageResponse::Single(area) = engine
            .query_area(
                "0,0,1,1",
                Some((reference, reference)),
                Some(&["TMP".into()]),
                None,
                None,
            )
            .unwrap()
        else {
            panic!()
        };
        assert!(area.ranges["TMP"]
            .values
            .iter()
            .all(|v| (v.unwrap() - 6.85).abs() < 1e-9));
        let tile = engine
            .get_raster_tile(
                [0.0, 0.0, 1.0, 1.0],
                1,
                1,
                Some(reference),
                &OutputCrs::Wgs84,
                Some("TMP"),
                None,
                None,
            )
            .unwrap();
        // Map tiles store f32 (#475): exact to f32 precision.
        assert_eq!(tile.values.value_at(0).unwrap() as f32, 6.85);
        // f001 lacks the canonical level: the map snaps to the nearest step
        // that has it (#1005), never to the 2 hPa field, and keys that step.
        let later = Some(reference + chrono::Duration::hours(1));
        let tile = engine
            .get_raster_tile(
                [0.0, 0.0, 1.0, 1.0],
                1,
                1,
                later,
                &OutputCrs::Wgs84,
                Some("TMP"),
                None,
                None,
            )
            .unwrap();
        assert_eq!(tile.values.value_at(0).unwrap() as f32, 6.85);
        assert_eq!(
            engine.resolve_parameter_time(Some("TMP"), later, None),
            Some(reference)
        );
        assert_eq!(engine.resolve_time(later, None), later);
    }

    #[test]
    fn named_surfaces_do_not_alias_or_replace_missing_ground_fields() {
        for split in [false, true] {
            let source = TestSource::new();
            source.write(
                "f000",
                &[
                    ("TMP", "tropopause", message(0, 200.0, [0; 4], 7, 0)),
                    ("TMP", "surface", message(0, 280.0, [0; 4], 1, 0)),
                ],
                0,
            );
            source.write(
                "f001",
                &[("TMP", "tropopause", message(0, 200.0, [0; 4], 7, 0))],
                1,
            );
            let mut config = source.config();
            if split {
                config.level_types = Some(vec![
                    GribLevelType::Single,
                    GribLevelType::Pressure,
                    GribLevelType::Model,
                ]);
            }
            let owner = GribEngine::new("named-levels", &config).unwrap();
            let views = owner.level_collections();
            let engine = if split {
                assert_eq!(views.len(), 1);
                assert_eq!(views[0].family, Some(GribLevelType::Single));
                &views[0]
            } else {
                &owner
            };
            assert!(engine.vertical_extent(&engine.catalog()).is_none());
            assert_eq!(
                engine.get_parameter_descriptions()["TMP"].label,
                "Temperature (surface)"
            );
            let CoverageResponse::Single(series) = engine
                .query_position("POINT(0.5 0.5)", None, Some(&["TMP".into()]), None, None)
                .unwrap()
            else {
                panic!()
            };
            assert!((series.ranges["TMP"].values[0].unwrap() - 6.85).abs() < 1e-9);
            assert_eq!(series.ranges["TMP"].values[1], None);
            let reference: DateTime<Utc> = "2026-04-05T00:00:00Z".parse().unwrap();
            let CoverageResponse::Single(area) = engine
                .query_area("0,0,1,1", Some((reference, reference)), None, None, None)
                .unwrap()
            else {
                panic!()
            };
            assert!(area.ranges["TMP"]
                .values
                .iter()
                .all(|v| (v.unwrap() - 6.85).abs() < 1e-9));
            let tile = engine
                .get_raster_tile(
                    [0.0, 0.0, 1.0, 1.0],
                    1,
                    1,
                    Some(reference),
                    &OutputCrs::Wgs84,
                    Some("TMP"),
                    None,
                    None,
                )
                .unwrap();
            // Map tiles store f32 (#475): exact to f32 precision.
            assert_eq!(tile.values.value_at(0).unwrap() as f32, 6.85);
            // f001 has only the tropopause: the map draws f000's surface
            // field (#1005), never the tropopause, and keys f000.
            let later = Some(reference + chrono::Duration::hours(1));
            let tile = engine
                .get_raster_tile(
                    [0.0, 0.0, 1.0, 1.0],
                    1,
                    1,
                    later,
                    &OutputCrs::Wgs84,
                    Some("TMP"),
                    None,
                    None,
                )
                .unwrap();
            assert_eq!(tile.values.value_at(0).unwrap() as f32, 6.85);
            assert_eq!(
                engine.resolve_parameter_time(Some("TMP"), later, None),
                Some(reference)
            );
        }
    }

    #[test]
    fn metadata_probes_continue_without_new_indexes_and_past_failures() {
        for failures in [false, true] {
            let source = TestSource::new();
            let names: Vec<_> = (0..40).map(|i| format!("P{i:02}")).collect();
            let records: Vec<_> = names
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    let data = if failures && i < 32 {
                        vec![0; 183]
                    } else {
                        message(0, 280.0, [0; 4], 103, 2)
                    };
                    (name.as_str(), "2 m above ground", data)
                })
                .collect();
            source.write("many", &records, 0);
            let engine = GribEngine::new("probes", &source.config()).unwrap();
            let ready = || {
                engine
                    .get_parameter_descriptions()
                    .values()
                    .filter(|m| !m.unit.is_empty())
                    .count()
            };
            assert_eq!(ready(), if failures { 0 } else { 32 });
            engine.scan_once().unwrap();
            assert_eq!(ready(), if failures { 8 } else { 40 });
        }
    }

    #[test]
    fn pinned_run_metadata_uses_its_own_level_identity() {
        let source = TestSource::new();
        source.write(
            "old",
            &[("TMP", "2 m above ground", message(0, 280.0, [0; 4], 103, 2))],
            0,
        );
        source.write(
            "new",
            &[(
                "TMP",
                "10 m above ground",
                message(0, 290.0, [0; 4], 103, 10),
            )],
            0,
        );
        let index_path = source.dir.join("new.idx");
        let index = std::fs::read_to_string(&index_path)
            .unwrap()
            .replace("d=2026040500", "d=2026040512");
        std::fs::write(index_path, index).unwrap();
        let engine = GribEngine::new("run-levels", &source.config()).unwrap();
        let old = "2026-04-05T00:00:00Z".parse().unwrap();
        let CoverageResponse::Single(series) = engine
            .query_position("POINT(0.5 0.5)", None, None, None, Some(old))
            .unwrap()
        else {
            panic!()
        };
        assert!(series.parameters["TMP"].label.contains("2 m above ground"));
        assert!((series.ranges["TMP"].values[0].unwrap() - 6.85).abs() < 1e-9);
        assert!(engine.get_parameter_descriptions()["TMP"]
            .label
            .contains("10 m above ground"));
    }

    #[test]
    fn retention_expires_without_new_indexes_or_listing_hits() {
        for remove_index in [false, true] {
            let source = TestSource::new();
            source.write(
                "f000",
                &[("TMP", "2 m above ground", message(0, 280.0, [0; 4], 103, 2))],
                0,
            );
            let mut engine = GribEngine::new("retention", &source.config()).unwrap();
            Arc::get_mut(&mut engine.source).unwrap().time_window =
                Some(TimeWindow::parse("-PT2H").unwrap());
            let version = engine.content_version();
            engine
                .scan_at("2026-04-05T01:00:00Z".parse().unwrap())
                .unwrap();
            assert!(engine.get_temporal_extent().is_some());
            assert_eq!(
                engine.content_version(),
                version,
                "unchanged retention rebuild"
            );
            if remove_index {
                std::fs::remove_file(source.dir.join("f000.idx")).unwrap();
            }
            engine
                .scan_at("2026-04-05T03:00:00Z".parse().unwrap())
                .unwrap();
            assert!(engine.get_temporal_extent().is_none());
            assert!(engine.get_parameters().is_empty());
            assert_ne!(engine.content_version(), version, "expired catalog");
            assert_ne!(
                engine.content_version(),
                0,
                "empty catalogs still revalidate"
            );
        }
    }

    #[test]
    fn fixed_remote_source_preserves_its_prefix() {
        // Store construction parses the URL without issuing network requests.
        let (_store, prefix) =
            ds_storage::build_store("https://bucket.s3.eu-west-1.amazonaws.com/subdirectory/")
                .unwrap();
        assert_eq!(fixed_source_prefix(&prefix, None), "subdirectory");
        assert_eq!(
            fixed_source_prefix(&prefix, Some("/nested/")),
            "subdirectory/nested"
        );
        let local = ds_storage::object_store::path::Path::from("");
        assert_eq!(fixed_source_prefix(&local, None), "");
        assert_eq!(fixed_source_prefix(&local, Some("nested")), "nested");
    }

    #[test]
    fn real_gfs_index_converts_to_qualified_catalog_and_rejects_mixed_end_times() {
        let fixture = include_str!("../../../testdata/gfs/gfs.t00z.pgrb2.0p25.f006.idx");
        let parsed = GribEngine::parse_and_resolve(index::IndexFormat::Wgrib2, fixture).unwrap();
        assert_eq!(parsed.step, 6);
        assert!(parsed.messages.iter().any(|m| m.param == "APCP_acc_6h"));
        assert!(parsed.messages.iter().any(|m| m.param == "DSWRF_avg_6h"));
        let duplicates: Vec<_> = catalog::duplicate_message_keys(&parsed.messages).collect();
        assert_eq!(duplicates.len(), 2);
        assert!(duplicates
            .iter()
            .all(|(m, _)| matches!(m.param.as_str(), "APCP_acc_6h" | "ACPCP_acc_6h")));
        let (first, duplicate) = duplicates
            .iter()
            .find(|(first, _)| first.param == "APCP_acc_6h")
            .unwrap();
        assert_eq!((first.offset, duplicate.offset), (426827357, 427200871));
        assert_eq!(first.length, Some(373514));
        assert_eq!(duplicate.length, Some(373514));
        let step = StepFile {
            grib_url: "fixture".into(),
            messages: parsed.messages.clone(),
        };
        assert_eq!(
            step.find_message("APCP_acc_6h", None).unwrap().offset,
            426827357
        );
        let mixed = fixture.replace("0-6 hour acc fcst", "0-12 hour acc fcst");
        assert!(GribEngine::parse_and_resolve(index::IndexFormat::Wgrib2, &mixed).is_none());
    }

    #[test]
    fn reported_gfs_index_has_no_ambiguous_keys() {
        let fixture = include_str!("../../../testdata/gfs/gfs.t00z.pgrb2.0p25.f384.idx");
        let parsed = GribEngine::parse_and_resolve(index::IndexFormat::Wgrib2, fixture).unwrap();
        assert_eq!(parsed.step, 384);
        assert!(parsed.messages.len() > 300);
        assert_eq!(catalog::duplicate_message_keys(&parsed.messages).count(), 0);
        for message in parsed.messages.iter().filter(|m| m.level.is_none()) {
            assert_eq!(message.level_type(), Some(GribLevelType::Single));
        }
        for (offset, param, levtype) in [
            (425405878, "HGT", "sfc"),
            (464273613, "HGT", "cloud_ceiling"),
            (424563425, "PRES", "sfc"),
            (465498833, "PRES", "convective_cloud_bottom"),
            (470130954, "PRES", "convective_cloud_top"),
            (462575838, "TCDC", "atmosphere"),
            (477887843, "TCDC", "convective_cloud"),
            (487869505, "PRES", "tropopause"),
            (490779458, "HGT", "tropopause"),
            (425898321, "TMP", "sfc"),
            (492265846, "TMP", "tropopause"),
            (5033274, "UGRD", "pbl"),
            (493248351, "UGRD", "tropopause"),
            (5640829, "VGRD", "pbl"),
            (493955379, "VGRD", "tropopause"),
        ] {
            let entry = parsed.messages.iter().find(|m| m.offset == offset).unwrap();
            assert_eq!(entry.param, param);
            assert_eq!(entry.levtype, levtype);
            assert_eq!(entry.level_type(), Some(GribLevelType::Single));
        }
        for message in parsed
            .messages
            .iter()
            .filter(|m| m.levtype.starts_with("sol:"))
        {
            assert_eq!(message.level_type(), None);
        }
        for reverse in [false, true] {
            let mut messages = parsed.messages.clone();
            if reverse {
                messages.reverse();
            }
            let mut catalog = Catalog::new();
            catalog.runs.insert(
                parsed.reference_time,
                ForecastRun {
                    reference_time: parsed.reference_time,
                    steps: [(
                        parsed.step,
                        StepFile {
                            grib_url: "fixture".into(),
                            messages,
                        },
                    )]
                    .into(),
                },
            );
            catalog.refresh_metadata();
            let keys = catalog.parameter_keys(&parsed.reference_time).unwrap();
            assert_eq!(keys["HGT"].levtype, "sfc");
            assert_eq!(keys["TCDC"].levtype, "atmosphere");
            assert_eq!(keys["PRMSL"].levtype, "msl");
            assert_eq!(keys["UGRD"].levtype, "hag");
            assert_eq!(keys["UGRD"].level, Some(10));
        }
    }

    #[test]
    fn aggregate_windows_coexist_and_reach_edr_and_maps() {
        let cfg: GribConfig = serde_json::from_value(serde_json::json!({
            "data_path": "../../testdata/grib-local", "index_format": "ecmwf-json",
            "grid_cache_mb": 16
        }))
        .unwrap();
        let engine = GribEngine::new("aggregate-test", &cfg).unwrap();
        let index = "1:0:d=2026040800:APCP:surface:6 hour fcst:\n2:100:d=2026040800:APCP:surface:0-6 hour acc fcst:\n3:200:d=2026040800:APCP:surface:3-6 hour acc fcst:\n4:300:d=2026040800:DSWRF:surface:0-6 hour ave fcst:\n";
        let parsed = GribEngine::parse_and_resolve(index::IndexFormat::Wgrib2, index).unwrap();
        assert_eq!(catalog::duplicate_message_keys(&parsed.messages).count(), 0);
        let rt = parsed.reference_time;
        let mut sf = StepFile {
            grib_url: "synthetic".into(),
            messages: parsed.messages,
        };
        for (i, entry) in sf.messages.iter().enumerate() {
            // Decoded-grid injection isolates catalog/statistic selection from
            // binary packing. The committed GFS index separately covers real
            // APCP/DSWRF descriptors; source units still come from WMO triples.
            engine.source.grid_cache.as_ref().unwrap().insert(
                "synthetic",
                entry.offset,
                Arc::new(DecodedGrid {
                    ni: 2,
                    nj: 2,
                    lon_first: 0.0,
                    lat_first: 1.0,
                    lon_inc: 1.0,
                    lat_inc: -1.0,
                    values: Arc::new(vec![(i + 1) as f32 * 10.0; 4]),
                    triple: if i == 3 { (0, 4, 7) } else { (0, 1, 8) },
                    centre: 7,
                    first_surface_type: 1,
                    first_surface_value: None,
                    uv_frame: ds_core::wind::VectorFrame::Earth,
                }),
            );
        }
        sf.messages.rotate_left(1); // aggregate records precede the instant in the index
        let empty_analysis = StepFile {
            grib_url: "analysis".into(),
            messages: vec![],
        };
        let mut catalog = Catalog::new();
        catalog.runs.insert(
            rt,
            ForecastRun {
                reference_time: rt,
                steps: [(0, empty_analysis), (6, sf)].into_iter().collect(),
            },
        );
        catalog.refresh_metadata();
        engine.publish_catalog(catalog);
        let params = engine.get_parameters();
        assert!(
            params.contains(&"APCP_acc_6h".to_owned()),
            "aggregate absent from f000 must be advertised"
        );
        assert!(params.contains(&"APCP_acc_3h".to_owned()));
        let CoverageResponse::Single(response) = engine
            .query_position("POINT(0.5 0.5)", None, None, None, None)
            .unwrap()
        else {
            panic!("expected point series")
        };
        assert_eq!(
            response.ranges["APCP_acc_6h"].values,
            vec![None, Some(20.0)]
        );
        assert_eq!(
            response.ranges["APCP_acc_3h"].values,
            vec![None, Some(30.0)]
        );
        assert_eq!(response.ranges["APCP"].values, vec![None, Some(10.0)]);
        assert_eq!(
            response.ranges["DSWRF_avg_6h"].values,
            vec![None, Some(40.0)],
            "averages are not divided by duration"
        );
        assert!(response.parameters["APCP_acc_6h"]
            .label
            .contains("6 h accumulation"));
        assert!(response.parameters["DSWRF_avg_6h"]
            .label
            .contains("6 h average"));
        let tile = engine
            .get_raster_tile(
                [0.0, 0.0, 1.0, 1.0],
                2,
                2,
                Some(rt + chrono::Duration::hours(6)),
                &OutputCrs::Wgs84,
                Some("APCP_acc_3h"),
                None,
                Some(rt),
            )
            .unwrap();
        assert_eq!(engine.raster_info().parameter, "APCP");
        let default = engine
            .get_raster_tile(
                [0.0, 0.0, 1.0, 1.0],
                2,
                2,
                Some(rt + chrono::Duration::hours(6)),
                &OutputCrs::Wgs84,
                None,
                None,
                Some(rt),
            )
            .unwrap();
        assert_eq!(
            default.values.value_at(0),
            Some(10.0),
            "default remains instant despite index ordering"
        );
        let CoverageResponse::Single(area) = engine
            .query_area("0,0,1,1", None, None, None, Some(rt))
            .unwrap()
        else {
            panic!("grid")
        };
        assert!(area.parameters.contains_key("APCP"));
        assert_eq!(tile.width, 2);
        assert_eq!(tile.height, 2);
        assert!((tile.values.value_at(0).unwrap() - 30.0).abs() < 1e-9);
        let DomainDescription::PointSeries { t, .. } = response.domain else {
            panic!("time axis")
        };
        assert_eq!(t, vec![rt, rt + chrono::Duration::hours(6)]);
        let mut catalog = (*engine.source.catalog.load_full()).clone();
        catalog
            .runs
            .get_mut(&rt)
            .unwrap()
            .steps
            .get_mut(&6)
            .unwrap()
            .messages
            .retain(|m| m.step_kind != wgrib2_index::StepKind::Instant);
        catalog.refresh_metadata();
        engine.publish_catalog(catalog);
        assert_eq!(engine.raster_info().parameter, "APCP_acc_6h");
        let aggregate_only = engine
            .get_raster_tile(
                [0.0, 0.0, 1.0, 1.0],
                2,
                2,
                Some(rt + chrono::Duration::hours(6)),
                &OutputCrs::Wgs84,
                None,
                None,
                Some(rt),
            )
            .unwrap();
        assert_eq!(aggregate_only.values.value_at(0), Some(20.0));
    }

    fn win<'a>(prefixes: &'a [&'a str]) -> HashSet<&'a str> {
        prefixes.iter().copied().collect()
    }

    fn two_runs() -> (Catalog, DateTime<Utc>, DateTime<Utc>) {
        fn step_file() -> StepFile {
            StepFile {
                grib_url: "unused".into(),
                messages: Vec::new(),
            }
        }
        let run_a: DateTime<Utc> = "2026-06-07T00:00:00Z".parse().unwrap();
        let run_b: DateTime<Utc> = "2026-06-07T12:00:00Z".parse().unwrap();
        let mut catalog = Catalog::new();
        catalog.runs.insert(
            run_a,
            ForecastRun {
                reference_time: run_a,
                steps: (0..=18).step_by(3).map(|s| (s, step_file())).collect(),
            },
        );
        catalog.runs.insert(
            run_b,
            ForecastRun {
                reference_time: run_b,
                steps: [(0, step_file())].into_iter().collect(),
            },
        );
        (catalog, run_a, run_b)
    }

    /// The #521 cross-run fallback contract of the map path: with no pinned
    /// run, `select_run_step` renders the newest run that COVERS the valid
    /// time, at its nearest step, so the fallback triggers for valid times
    /// BEFORE the newest run's reference time — animating past frames after
    /// a new run lands. `resolve_reference_time` returns this run via the
    /// same `select_run_step` authority, so the API cache keys track the run
    /// actually rendered.
    #[test]
    fn map_run_falls_back_across_runs_for_uncovered_times() {
        let (catalog, run_a, run_b) = two_runs();
        let t = |s: &str| Some(s.parse::<DateTime<Utc>>().unwrap());
        // 09Z predates the newest run's reference → fall back to run A
        // (the past-frame-after-new-run-lands case).
        let run = covering_run(&catalog, None, t("2026-06-07T09:00:00Z")).unwrap();
        assert_eq!(run.reference_time, run_a, "past valid time must fall back");
        // 15Z is beyond the newest run's published extent; the older run
        // actually has this forecast step and must serve it.
        let (run, step, _) =
            select_run_step(&catalog, &[], None, t("2026-06-07T15:00:00Z")).unwrap();
        assert_eq!((run.reference_time, step), (run_a, 15));
        // A map TIME between steps snaps to the nearest one.
        let (run, step, _) =
            select_run_step(&catalog, &[], None, t("2026-06-07T10:00:00Z")).unwrap();
        assert_eq!((run.reference_time, step), (run_a, 9));
        assert!(select_run_step(&catalog, &[], Some(run_b), t("2026-06-07T15:00:00Z")).is_err());
        assert!(select_run_step(&catalog, &[], None, t("2026-06-07T19:00:00Z")).is_err());
        // Explicit pin stays exact even when another run also covers.
        let run = covering_run(&catalog, Some(run_a), t("2026-06-07T15:00:00Z")).unwrap();
        assert_eq!(run.reference_time, run_a);
        // No time: latest run wins.
        let run = covering_run(&catalog, None, None).unwrap();
        assert_eq!(run.reference_time, run_b);
    }

    /// EDR selection is by intersection (`/req/core/datetime-response` A and
    /// F): an instant needs a step valid at exactly that time, never the
    /// nearest, and a window selects a run with a step inside it, open ends
    /// included. No step inside is a 404, which a datetime list skips.
    #[test]
    fn edr_run_and_steps_intersect_the_datetime() {
        let (catalog, run_a, run_b) = two_runs();
        let t = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        let (min, max) = (DateTime::<Utc>::MIN_UTC, DateTime::<Utc>::MAX_UTC);
        let select = |reference_time, window: (DateTime<Utc>, DateTime<Utc>)| {
            resolve_run(&catalog, reference_time, Some(window)).and_then(|run| {
                let steps = grid_steps(run, Some(window))?;
                Ok((
                    run.reference_time,
                    steps.into_iter().map(|(time, _)| time).collect::<Vec<_>>(),
                ))
            })
        };
        let no_step = |result: Result<_, DataServerError>| match result {
            Err(DataServerError::LocationNotFound(message)) => message,
            other => panic!("expected a 404, got {:?}", other.map(drop)),
        };

        // An instant: the step valid at it, from the newest run having one.
        let nine = t("2026-06-07T09:00:00Z");
        assert_eq!(select(None, (nine, nine)).unwrap(), (run_a, vec![nine]));
        let noon = t("2026-06-07T12:00:00Z");
        assert_eq!(select(None, (noon, noon)).unwrap(), (run_b, vec![noon]));
        // Between steps (the map path snaps 10Z to 09Z): no data.
        let ten = t("2026-06-07T10:00:00Z");
        let message = no_step(select(None, (ten, ten)));
        assert!(message.contains("2026-06-07T10:00:00Z"), "{message}");

        // A window: every step inside it, of the newest run covering its
        // start, else of the newest with any step inside.
        let steps = |hours: &[i64]| -> Vec<DateTime<Utc>> {
            hours
                .iter()
                .map(|&h| run_a + chrono::Duration::hours(h))
                .collect()
        };
        assert_eq!(
            select(None, (ten, t("2026-06-07T13:00:00Z"))).unwrap(),
            (run_a, steps(&[12]))
        );
        assert_eq!(
            select(None, (t("2026-06-07T11:00:00Z"), max)).unwrap(),
            (run_a, steps(&[12, 15, 18]))
        );
        // An open or early start covers no run: the newest intersecting.
        assert_eq!(
            select(None, (min, t("2026-06-07T11:00:00Z"))).unwrap(),
            (run_a, steps(&[0, 3, 6, 9]))
        );
        assert_eq!(select(None, (min, max)).unwrap(), (run_b, vec![noon]));
        assert_eq!(
            select(None, (t("2026-06-06T00:00:00Z"), t("2026-06-07T01:00:00Z"))).unwrap(),
            (run_a, steps(&[0]))
        );
        // A window between steps, or past every run: no data.
        no_step(select(
            None,
            (t("2026-06-07T04:00:00Z"), t("2026-06-07T05:00:00Z")),
        ));
        let message = no_step(select(None, (t("2026-06-07T19:00:00Z"), max)));
        assert!(message.contains("2026-06-07T19:00:00Z/.."), "{message}");
        // A pinned run never falls back: its own steps or no data.
        assert_eq!(
            select(Some(run_a), (noon, noon)).unwrap(),
            (run_a, vec![noon])
        );
        no_step(select(Some(run_b), (nine, nine)));
        assert!(matches!(
            select(Some(t("2026-06-06T00:00:00Z")), (nine, nine)),
            Err(DataServerError::ReferenceTimeNotFound(_))
        ));
        // No datetime: the latest run, and an area its last step.
        let run = resolve_run(&catalog, None, None).unwrap();
        assert_eq!(run.reference_time, run_b);
        let run = resolve_run(&catalog, Some(run_a), None).unwrap();
        assert_eq!(grid_steps(run, None).unwrap()[0].0, steps(&[18])[0]);
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

    /// The prefixes each shipped layout scans: `collections.d/ecmwf-ifs.toml`,
    /// `ecmwf-aifs.toml` and `noaa-gfs.toml` (also `testdata/gfs/test-config.toml`),
    /// plus the README example without `{run}`. Each list is what the pre-#817
    /// `build_scan_prefixes` returned for the same `now` and run hours, in the
    /// same order, without its trailing `/`: the shared expansion trims it,
    /// and the object-store path a prefix is listed as drops it anyway.
    #[test]
    fn scan_prefixes_for_each_shipped_layout() {
        const IFS: &str = "%Y%m%d/{run}z/ifs/0p25/oper/";
        const AIFS: &str = "%Y%m%d/{run}z/aifs-single/0p25/oper/";
        const GFS: &str = "gfs.%Y%m%d/{run}/atmos/";
        const README: &str = "%Y%m%d/00z/ifs/0p25/oper/";
        const RUNS: &[u32] = &[0, 6, 12, 18];
        let cases: &[(&str, &str, &[u32], &[&str])] = &[
            // Today's 18z is still in the future.
            (
                IFS,
                "2026-04-06T15:00:00Z",
                RUNS,
                &[
                    "20260406/12z/ifs/0p25/oper",
                    "20260406/06z/ifs/0p25/oper",
                    "20260406/00z/ifs/0p25/oper",
                    "20260405/18z/ifs/0p25/oper",
                    "20260405/12z/ifs/0p25/oper",
                    "20260405/06z/ifs/0p25/oper",
                    "20260405/00z/ifs/0p25/oper",
                ],
            ),
            // Today's 00z is listed while it may still be publishing.
            (
                IFS,
                "2026-04-06T02:00:00Z",
                RUNS,
                &[
                    "20260406/00z/ifs/0p25/oper",
                    "20260405/18z/ifs/0p25/oper",
                    "20260405/12z/ifs/0p25/oper",
                    "20260405/06z/ifs/0p25/oper",
                    "20260405/00z/ifs/0p25/oper",
                ],
            ),
            // Run hours in any order.
            (
                IFS,
                "2026-04-06T11:00:00Z",
                &[12, 0],
                &[
                    "20260406/00z/ifs/0p25/oper",
                    "20260405/12z/ifs/0p25/oper",
                    "20260405/00z/ifs/0p25/oper",
                ],
            ),
            // Exactly midnight on the 1st: the new day's 00z, then February.
            (
                AIFS,
                "2026-03-01T00:00:00Z",
                RUNS,
                &[
                    "20260301/00z/aifs-single/0p25/oper",
                    "20260228/18z/aifs-single/0p25/oper",
                    "20260228/12z/aifs-single/0p25/oper",
                    "20260228/06z/aifs-single/0p25/oper",
                    "20260228/00z/aifs-single/0p25/oper",
                ],
            ),
            (
                GFS,
                "2027-01-01T05:59:59Z",
                RUNS,
                &[
                    "gfs.20270101/00/atmos",
                    "gfs.20261231/18/atmos",
                    "gfs.20261231/12/atmos",
                    "gfs.20261231/06/atmos",
                    "gfs.20261231/00/atmos",
                ],
            ),
            // One second before midnight on a leap day: all of today's runs.
            (
                GFS,
                "2028-02-29T23:59:59Z",
                RUNS,
                &[
                    "gfs.20280229/18/atmos",
                    "gfs.20280229/12/atmos",
                    "gfs.20280229/06/atmos",
                    "gfs.20280229/00/atmos",
                    "gfs.20280228/18/atmos",
                    "gfs.20280228/12/atmos",
                    "gfs.20280228/06/atmos",
                    "gfs.20280228/00/atmos",
                ],
            ),
            // No `{run}`: one prefix per day, and the run hours are unused.
            (
                README,
                "2026-04-06T15:00:00Z",
                RUNS,
                &["20260406/00z/ifs/0p25/oper", "20260405/00z/ifs/0p25/oper"],
            ),
            (
                README,
                "2026-03-01T00:00:00Z",
                RUNS,
                &["20260301/00z/ifs/0p25/oper", "20260228/00z/ifs/0p25/oper"],
            ),
        ];
        for &(pattern, now, run_hours, expected) in cases {
            let now: DateTime<Utc> = now.parse().unwrap();
            let runs = build_scan_prefixes(pattern, now, run_hours).unwrap();
            let prefixes: Vec<&str> = runs.iter().map(|run| run.prefix.as_str()).collect();
            assert_eq!(prefixes, expected, "{pattern} at {now}");
            // Each prefix is its run's reference time formatted with the
            // run hour in place of `{run}`: 00 UTC without it.
            let with_hour = pattern.replace("{run}", "%H");
            for run in &runs {
                assert!(run.reference_time <= now, "{run:?}");
                let formatted = run.reference_time.format(&with_hour).to_string();
                assert_eq!(formatted.trim_end_matches('/'), run.prefix);
                if !pattern.contains("{run}") {
                    assert_eq!(run.reference_time.format("%H:%M").to_string(), "00:00");
                }
            }
            for pair in runs.windows(2) {
                assert!(pair[0].reference_time > pair[1].reference_time);
            }
        }
        // The trimmed prefix lists the same object-store path.
        use ds_storage::object_store::path::Path;
        assert_eq!(
            Path::from("20260406/12z/ifs/0p25/oper/"),
            Path::from("20260406/12z/ifs/0p25/oper")
        );
    }
}
