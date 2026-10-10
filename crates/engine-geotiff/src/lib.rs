mod cache;
mod catalog;
pub mod decode_budget;
mod decoded_cache;
mod parse;
mod prewarm;
#[cfg(test)]
mod prewarm_tests;
mod range_batch;
mod reader;
#[cfg(test)]
mod remote_header_tests;
#[cfg(test)]
mod stac_preload_tests;

/// Snapshot of the process-global decoded-chunk cache (#463) for `/metrics`:
/// `(hits, misses, bytes, capacity_bytes)`.
pub use decoded_cache::metrics as decoded_chunk_cache_metrics;
pub mod stac;

/// Re-exports for fuzz testing. Not part of the public API.
#[cfg(feature = "fuzz")]
#[doc(hidden)]
pub mod fuzz_exports {
    pub use crate::reader::{DataSource, TiffMetadata};
    pub use ds_core::geo::{Crs, GeoTransform, SweepAxis};
}

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use ds_poll::Shutdown;
use ds_storage::discovery::{
    expand_prefix_for_range, expand_prefix_pattern, validate_prefix_pattern, FilenameMatcher,
    ScanSpec, TimeWindow,
};
use futures::StreamExt;
use std::sync::Arc;

use ds_core::config::GeoTiffConfig;
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::model::*;

use crate::catalog::{scan_directory, scan_remote, Catalog, PendingFile};

/// Tracks STAC entries currently being loaded, deduplicating concurrent
/// loads: the first caller claims the path and loads, later callers park on
/// the condvar until the loader finishes (success, error, or panic) instead
/// of sleep-polling with a render permit held (#213).
struct InFlightLoads {
    paths: Mutex<std::collections::HashSet<PathBuf>>,
    completed: Condvar,
}

/// Upper bound on how long a loser of the load race waits for the in-flight
/// loader. Waiters wake immediately when the loader finishes, so this only
/// bounds a stuck loader — it can be generous (a STAC asset fetch downloads
/// the whole file, which can legitimately take seconds).
const CONCURRENT_LOAD_WAIT: Duration = Duration::from_secs(10);

impl InFlightLoads {
    fn new() -> Self {
        Self {
            paths: Mutex::new(std::collections::HashSet::new()),
            completed: Condvar::new(),
        }
    }

    /// Park until no other thread is loading `path`, then atomically either
    /// observe the work done or claim the path. Returns `None` if `is_done`
    /// reports the work complete (nothing left to do); `Some(guard)` if the
    /// caller is now the loader — dropping the guard (on success, error, or
    /// panic) releases the claim and wakes waiters, so the pairing is
    /// compiler-enforced. If the in-flight loader exceeds
    /// `CONCURRENT_LOAD_WAIT` the caller claims the path anyway rather than
    /// failing the request.
    ///
    /// `is_done` is invoked while `self.paths` is held; it must not acquire
    /// that lock (directly or transitively) or the thread deadlocks itself.
    fn wait_then_claim(
        &self,
        path: &Path,
        is_done: impl Fn() -> bool,
    ) -> Option<InFlightGuard<'_>> {
        let mut in_flight = self.paths.lock().unwrap_or_else(|e| e.into_inner());
        let deadline = std::time::Instant::now() + CONCURRENT_LOAD_WAIT;
        while in_flight.contains(path) {
            let now = std::time::Instant::now();
            if now >= deadline {
                // Stuck loader: deliberately claim anyway rather than fail
                // the request. The insert below is a no-op and whichever
                // load finishes first clears the path — same liveness
                // behavior as the old polling code. Note dedup is best-effort
                // past this point: the early clear lets later callers claim
                // too, so a stuck loader can fan out to several parallel
                // fetches of the same asset, not just one duplicate.
                break;
            }
            let (guard, _) = self
                .completed
                .wait_timeout(in_flight, deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            in_flight = guard;
        }
        if is_done() {
            return None;
        }
        in_flight.insert(path.to_path_buf());
        drop(in_flight);
        Some(InFlightGuard::new(self, path.to_path_buf()))
    }

    /// Non-blocking claim for the async poll-cycle preload (#90): never
    /// parks. `None` if another loader holds `path` (it will finish the
    /// work) or `is_done` reports it complete; `Some(guard)` otherwise, and
    /// requests for the path then wait on the preload instead of fetching
    /// the same asset twice. Same lock rule for `is_done` as
    /// [`wait_then_claim`](Self::wait_then_claim).
    fn try_claim(&self, path: &Path, is_done: impl Fn() -> bool) -> Option<InFlightGuard<'_>> {
        let mut in_flight = self.paths.lock().unwrap_or_else(|e| e.into_inner());
        if in_flight.contains(path) || is_done() {
            return None;
        }
        in_flight.insert(path.to_path_buf());
        drop(in_flight);
        Some(InFlightGuard::new(self, path.to_path_buf()))
    }
}

/// RAII guard that removes a path from the `loading_in_flight` set on drop
/// and wakes waiting threads. Prevents paths from getting stuck (and waiters
/// from sleeping out their full timeout) if a thread panics during metadata
/// loading.
struct InFlightGuard<'a> {
    loads: &'a InFlightLoads,
    path: PathBuf,
}

impl<'a> InFlightGuard<'a> {
    fn new(loads: &'a InFlightLoads, path: PathBuf) -> Self {
        Self { loads, path }
    }
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        let mut guard = self.loads.paths.lock().unwrap_or_else(|e| e.into_inner());
        guard.remove(&self.path);
        drop(guard);
        self.loads.completed.notify_all();
    }
}

/// Whether the data source is local or remote.
#[derive(Debug)]
enum StoreMode {
    Local {
        directory: PathBuf,
        pending: Mutex<BTreeMap<PathBuf, PendingFile>>,
    },
    /// Fixed prefix (from data_path URL).
    Remote {
        store: ds_storage::DataStore,
        prefix: ds_storage::object_store::path::Path,
    },
    /// Dynamic prefix with strftime date templates (from endpoint+bucket+prefix_pattern).
    /// Prefix is expanded on each poll cycle so it stays current across date boundaries.
    RemoteDynamic {
        store: ds_storage::DataStore,
        prefix_pattern: String,
        scan_days: u32,
        time_window: Option<TimeWindow>,
    },
    /// STAC API catalog: items discovered on-demand via STAC, assets fetched as remote COGs.
    RemoteStac { client: stac::StacClient },
}

pub struct GeoTiffEngine {
    collection_id: String,
    catalog: ArcSwap<Catalog>,
    /// `RasterInfo` snapshot, rebuilt once per catalog swap (`refresh_raster_info`)
    /// so the per-request `raster_info()` is an O(1) `ArcSwap` read + cheap clone
    /// instead of re-deriving CRS/grid/timestamps — and, for STAC, never a
    /// metadata fetch — on every Maps/Tiles call (#211). The expensive
    /// derivation runs at startup and on the background poll runtime.
    raster_info: ArcSwap<ds_core::map_engine::RasterInfo>,
    tile_cache: cache::TileCache,
    store_mode: StoreMode,
    /// Matches data filenames and reads their timestamps. `None` for a STAC
    /// source, whose timestamps come from item properties.
    filename_matcher: Option<FilenameMatcher>,
    parameter: String,
    unit: String,
    poll_interval: Duration,
    exclude_patterns: Vec<String>,
    max_files: Option<usize>,
    band_index: usize,
    data_path_display: String,
    /// Config overrides for metadata values (applied after file parsing).
    override_nodata: Option<f64>,
    override_scale: Option<f64>,
    override_offset: Option<f64>,
    /// Edge-triggered stop signal for `poll_loop` (shared lifecycle, #481).
    shutdown: Shutdown,
    /// Consecutive poll failures/empty results (for escalating warnings).
    consecutive_poll_failures: AtomicU32,
    /// Tracks STAC entries currently being loaded to prevent concurrent loads.
    loading_in_flight: InFlightLoads,
    /// Circuit breaker: consecutive STAC API failures.
    stac_consecutive_failures: AtomicU32,
    /// Circuit breaker: last STAC API attempt time.
    stac_last_attempt: Mutex<Option<std::time::Instant>>,
    /// When the engine loaded or a poll last swapped in a catalog that
    /// found files ([`Self::poll_age`]). Not the data's age: a poll that finds
    /// the same files again stamps it too (#1007).
    catalog_updated_at: Mutex<Option<DateTime<Utc>>>,
}

/// Most newly discovered STAC items whose metadata one poll cycle preloads
/// (#90), newest first. A cold start or a poll after an outage can discover a
/// backlog; items past the cap keep the lazy request-path load.
const STAC_PRELOAD_MAX_ITEMS: usize = 24;
/// Asset metadata fetches in flight during a poll-cycle preload (#90). I/O
/// bound: enough to overlap round trips without fanning out a backlog.
const STAC_PRELOAD_CONCURRENCY: usize = 4;
/// Wall-clock cap on one poll cycle's preload (#90): a stalling asset host
/// delays the next catalog poll by at most this. Unfinished items are left to
/// the request path.
const STAC_PRELOAD_BUDGET: Duration = Duration::from_secs(60);

/// Most newly discovered remote frames one poll cycle pre-warms (#1004),
/// newest first: the frames a client animates first. A cold start or a poll
/// after an outage can discover a backlog; older frames load on first view.
const PREWARM_MAX_FRAMES: usize = 4;
/// Wall-clock cap on one poll cycle's pre-warm (#1004): a stalling store
/// delays the next catalog poll by at most this. Unfinished reads are left
/// to the request path.
const PREWARM_BUDGET: Duration = Duration::from_secs(60);

/// Circuit breaker threshold: number of consecutive failures before opening.
const STAC_CIRCUIT_BREAKER_THRESHOLD: u32 = 3;
/// How long the circuit breaker stays open before allowing a retry.
const STAC_CIRCUIT_BREAKER_COOLDOWN: Duration = Duration::from_secs(300); // 5 minutes

impl GeoTiffEngine {
    /// Check whether the STAC circuit breaker allows a request.
    /// Returns Ok(()) if the request should proceed, Err if the circuit is open.
    fn check_stac_circuit_breaker(&self) -> Result<(), DataServerError> {
        let failures = self.stac_consecutive_failures.load(Ordering::Relaxed);
        if failures >= STAC_CIRCUIT_BREAKER_THRESHOLD {
            let last_attempt = self
                .stac_last_attempt
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(last) = *last_attempt {
                if last.elapsed() < STAC_CIRCUIT_BREAKER_COOLDOWN {
                    return Err(DataServerError::Engine(format!(
                        "STAC API temporarily unavailable, circuit breaker open \
                         ({} consecutive failures, retry in {}s)",
                        failures,
                        (STAC_CIRCUIT_BREAKER_COOLDOWN - last.elapsed()).as_secs()
                    )));
                }
            }
        }
        Ok(())
    }

    /// Record a successful STAC API call — resets the circuit breaker.
    fn record_stac_success(&self) {
        let prev = self.stac_consecutive_failures.swap(0, Ordering::Relaxed);
        if prev >= STAC_CIRCUIT_BREAKER_THRESHOLD {
            tracing::warn!(
                "[{}] STAC circuit breaker closed (recovered after {} failures)",
                self.collection_id,
                prev
            );
        }
        *self
            .stac_last_attempt
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());
    }

    /// Record a failed STAC API call — increments the circuit breaker counter.
    fn record_stac_failure(&self) {
        let prev = self
            .stac_consecutive_failures
            .fetch_add(1, Ordering::Relaxed);
        *self
            .stac_last_attempt
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());
        if prev + 1 == STAC_CIRCUIT_BREAKER_THRESHOLD {
            tracing::warn!(
                "[{}] STAC circuit breaker open after {} consecutive failures — \
                 pausing STAC requests for {}s",
                self.collection_id,
                prev + 1,
                STAC_CIRCUIT_BREAKER_COOLDOWN.as_secs()
            );
        }
    }

    /// Store a STAC catalog update, applying max_files eviction if configured.
    fn store_stac_catalog(&self, mut catalog: Catalog) {
        if let Some(max) = self.max_files {
            catalog.trim_to_latest(max);
        }
        self.catalog.store(Arc::new(catalog));
        self.refresh_raster_info();
    }

    /// Rebuild the cached [`RasterInfo`] from the current catalog and publish it.
    /// Call after every catalog swap (#211). [`build_raster_info`](Self::build_raster_info)
    /// only *reads* already-loaded metadata (no I/O), so this is cheap and safe
    /// from any context — including the request path (`store_stac_catalog`,
    /// `do_load_metadata`), where it must never trigger `DataStore`/`block_in_place`.
    fn refresh_raster_info(&self) {
        self.raster_info.store(Arc::new(self.build_raster_info()));
    }

    /// Derive [`RasterInfo`] from the current catalog — a pure read of what's
    /// already loaded, with **no** metadata fetch. `times` come from the entry
    /// keys (fixed at scan); `native_crs`/`grid_size` from currently-loaded entry
    /// metadata. Local entries are `Loaded` at scan (CRS correct from
    /// construction); STAC entries are stubs until the poll preloads them (#90)
    /// or a request loads one, so `native_crs` is the `CRS:84` placeholder
    /// until `install_stac_metadata` calls
    /// [`refresh_raster_info`](Self::refresh_raster_info). The STAC cold-start
    /// `CRS:84` window (GetCapabilities / `/collections` before the first poll
    /// that discovers items) is tracked in #322.
    fn build_raster_info(&self) -> ds_core::map_engine::RasterInfo {
        let catalog = self.catalog.load();
        let crs_name = catalog
            .entries
            .values()
            .find_map(|entry| entry.metadata().map(|m| crs_label(&m.geo_transform.crs)))
            // No metadata could be read; the engine works internally in
            // lon-first geographic coordinates, so default to CRS:84.
            .unwrap_or_else(|| "CRS:84".to_string());

        let times: Vec<DateTime<Utc>> = catalog.entries.keys().cloned().collect();

        // Native full-resolution grid dimensions, taken from the first loaded
        // entry (all entries in a collection share the same grid).
        let grid_size = catalog
            .entries
            .values()
            .find_map(|entry| entry.metadata().map(|m| [m.width, m.height]));

        ds_core::map_engine::RasterInfo {
            native_crs: crs_name,
            spatial_extent: catalog.spatial_extent,
            times,
            reference_times: Vec::new(),
            parameter: self.parameter.clone(),
            unit: self.unit.clone(),
            parameters: vec![], // single-parameter engine
            vertical: None,     // single-layer raster, no vertical dimension
            grid_size,
            layer_subtitle: None,
        }
    }

    /// The collection ID this engine serves.
    pub fn collection_id(&self) -> &str {
        &self.collection_id
    }

    /// Return (hits, misses) for the tile cache.
    pub fn tile_cache_stats(&self) -> (u64, u64) {
        self.tile_cache.stats()
    }

    /// Return current tile cache utilization as (bytes_used, capacity_bytes, entries).
    pub fn tile_cache_utilization(&self) -> (u64, u64, usize) {
        (
            self.tile_cache.weight(),
            self.tile_cache.capacity(),
            self.tile_cache.len(),
        )
    }

    /// Return total bytes read from remote storage (0 for local engines).
    pub fn storage_bytes_read(&self) -> u64 {
        match &self.store_mode {
            StoreMode::Remote { store, .. } | StoreMode::RemoteDynamic { store, .. } => {
                store.bytes_read()
            }
            _ => 0,
        }
    }

    /// Age of the newest timestep, for `/health` `data_age_secs` and the
    /// `collection_data_age_seconds` gauge (#1007): now minus the newest
    /// timestamp in the catalog. It keeps growing while the feeder is
    /// stalled, even though every poll still finds the old files. Negative
    /// when the newest timestep lies in the future; `None` while the
    /// catalog is empty.
    pub fn data_age(&self) -> Option<chrono::Duration> {
        let newest = *self.catalog.load().entries.keys().next_back()?;
        Some(Utc::now() - newest)
    }

    /// Time since the engine loaded or a poll last found files, for
    /// `/health` `poll_age_secs`. It grows while the scan fails or comes back
    /// empty (the old catalog is kept), not while the files merely stop
    /// changing: that is [`Self::data_age`].
    pub fn poll_age(&self) -> Option<chrono::Duration> {
        let updated_at = self
            .catalog_updated_at
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        updated_at.map(|t| Utc::now() - t)
    }

    /// Create a new GeoTIFF engine, performing an initial scan.
    ///
    /// Data source is determined by config:
    /// - `endpoint` + `bucket` (+ optional `prefix_pattern`): S3 with dynamic prefix
    /// - `data_path` starting with `s3://` or `http(s)://`: S3/HTTP with fixed prefix
    /// - `data_path` otherwise: local directory
    pub fn new(
        collection_id: &str,
        data_path: Option<&str>,
        config: &GeoTiffConfig,
    ) -> Result<Self, DataServerError> {
        // Validate config early
        validate_config(collection_id, data_path, config)?;

        // Build the filename matcher from the template or the explicit fields
        let filename_matcher = resolve_filename_config(config)?;

        // Determine store mode from config
        // Parse time_window if configured
        let parsed_time_window = config
            .time_window
            .as_deref()
            .map(TimeWindow::parse)
            .transpose()?;

        let (store_mode, display) = if let Some(stac_url) = &config.stac_url {
            let allowlist = config.stac_asset_allowlist.clone().unwrap_or_default();
            let client = stac::StacClient::new(stac_url, &config.stac_asset_key, allowlist)?;
            let display = format!("stac:{}", stac_url);
            (StoreMode::RemoteStac { client }, display)
        } else if let (Some(endpoint), Some(bucket)) = (&config.endpoint, &config.bucket) {
            let store = ds_storage::build_s3_store_from_parts(endpoint, bucket)?;
            let prefix_pattern = config.prefix_pattern.clone().unwrap_or_default();
            let display = format!("s3://{}/{}", bucket, prefix_pattern);

            // scan_days is only used as fallback when time_window is not set.
            // When time_window is set, the prefixes follow its exact range.
            let scan_days = config
                .scan_days
                .or_else(|| parsed_time_window.as_ref().map(|tw| tw.max_scan_days()))
                .unwrap_or(2);

            (
                StoreMode::RemoteDynamic {
                    store,
                    prefix_pattern,
                    scan_days,
                    time_window: parsed_time_window.clone(),
                },
                display,
            )
        } else if let Some(data_path) = data_path {
            let is_remote = ds_storage::has_scheme(data_path, "s3://")
                || ds_storage::has_scheme(data_path, "http://")
                || ds_storage::has_scheme(data_path, "https://");

            if is_remote {
                let (store, prefix) = ds_storage::build_store(data_path)?;
                (StoreMode::Remote { store, prefix }, data_path.to_string())
            } else {
                let directory = PathBuf::from(data_path);
                if !directory.is_dir() {
                    return Err(DataServerError::Engine(format!(
                        "{data_path} is not a directory"
                    )));
                }
                let directory = directory.canonicalize().map_err(|e| {
                    DataServerError::Engine(format!("Cannot resolve directory {data_path}: {e}"))
                })?;
                // Local-file path uses `mmap` (#204). The kernel raises
                // `SIGBUS` — unrecoverable, kills the process — if a mapped
                // file is truncated while the mapping is live. Publishers MUST
                // use atomic rename (write a tempfile, then `rename(2)` into
                // place), not in-place overwrite (`cp`, `rsync --inplace`,
                // FTP). Atomic rename creates a new inode, leaving the
                // existing mmap valid; the catalog rescan picks up the new
                // inode on the next poll. Log this at startup so operators
                // don't silently enable the crash path.
                tracing::warn!(
                    collection = %collection_id,
                    data_path = %data_path,
                    "Local GeoTIFF data_path uses mmap for #204; publishers MUST use \
                     atomic rename (NOT in-place overwrite like `cp`/`rsync --inplace`). \
                     In-place overwrite can raise SIGBUS and kill the server."
                );
                (
                    StoreMode::Local {
                        directory,
                        pending: Mutex::new(BTreeMap::new()),
                    },
                    data_path.to_string(),
                )
            }
        } else {
            return Err(DataServerError::Engine(
                "Either data_path, endpoint+bucket, or stac_url must be configured".into(),
            ));
        };

        let cache_bytes = config.tile_cache_mb * 1024 * 1024;
        let tile_cache = cache::TileCache::new(cache_bytes);

        let band_index = (config.band.max(1) - 1) as usize; // 1-based config → 0-based index

        let engine = GeoTiffEngine {
            collection_id: collection_id.to_string(),
            catalog: ArcSwap::from_pointee(Catalog::empty()),
            // Placeholder until the initial scan populates it (refreshed below).
            raster_info: ArcSwap::from_pointee(ds_core::map_engine::RasterInfo {
                native_crs: "CRS:84".to_string(),
                spatial_extent: None,
                times: vec![],
                reference_times: Vec::new(),
                parameter: config.parameter.clone(),
                unit: config.unit.clone(),
                parameters: vec![],
                vertical: None,
                grid_size: None,
                layer_subtitle: None,
            }),
            tile_cache,
            store_mode,
            filename_matcher,
            parameter: config.parameter.clone(),
            unit: config.unit.clone(),
            poll_interval: Duration::from_secs(config.poll_interval_secs),
            exclude_patterns: config.exclude_patterns.clone(),
            max_files: config.max_files,
            band_index,
            data_path_display: display,
            override_nodata: config.nodata,
            override_scale: config.scale,
            override_offset: config.offset,
            shutdown: Shutdown::new(),
            consecutive_poll_failures: AtomicU32::new(0),
            loading_in_flight: InFlightLoads::new(),
            stac_consecutive_failures: AtomicU32::new(0),
            stac_last_attempt: Mutex::new(None),
            catalog_updated_at: Mutex::new(Some(Utc::now())),
        };

        // Initial scan — STAC mode fetches collection extent only (no items)
        let is_stac = matches!(engine.store_mode, StoreMode::RemoteStac { .. });
        if is_stac {
            if let StoreMode::RemoteStac { ref client, .. } = engine.store_mode {
                match client.fetch_extent() {
                    Ok(extent) => {
                        let initial = catalog::init_stac_from_extent(&extent);
                        tracing::info!(
                            "[{}] STAC catalog ready — extent: {:?}, temporal: {:?}",
                            collection_id,
                            extent.spatial_bbox,
                            extent.temporal_start.map(|s| format!(
                                "{} → {}",
                                s.format("%Y-%m-%d"),
                                extent.temporal_end.map_or("now".to_string(), |e| e
                                    .format("%Y-%m-%d")
                                    .to_string())
                            )),
                        );
                        engine.catalog.store(Arc::new(initial));
                    }
                    Err(e) => {
                        tracing::warn!(
                            "[{}] STAC extent fetch failed (will retry on poll): {}",
                            collection_id,
                            e
                        );
                        engine.catalog.store(Arc::new(Catalog::empty()));
                    }
                }
            }
        } else {
            let empty = Catalog::empty();
            let initial_catalog = engine.do_scan(&empty)?;
            let file_count = initial_catalog.entries.len();
            let total_bytes: u64 = initial_catalog.entries.values().map(|e| e.file_size).sum();
            engine.catalog.store(Arc::new(initial_catalog));

            if file_count == 0 {
                tracing::warn!(
                    "[{}] No matching GeoTIFF files found in {}",
                    collection_id,
                    engine.data_path_display
                );
            } else {
                tracing::info!(
                    "[{}] Loaded {} files from {} (metadata: {})",
                    collection_id,
                    file_count,
                    engine.data_path_display,
                    format_bytes(total_bytes)
                );
            }
        }

        // Populate the cached RasterInfo from the freshly-scanned catalog so the
        // first request is already O(1) (and any CRS metadata-load happens here
        // at startup, not on a request worker).
        engine.refresh_raster_info();

        Ok(engine)
    }

    /// The filename matcher of a directory or prefix source. Only a STAC
    /// source has none, and it never scans by filename.
    fn filename_matcher(&self) -> Result<&FilenameMatcher, DataServerError> {
        self.filename_matcher.as_ref().ok_or_else(|| {
            DataServerError::Engine(format!(
                "[{}] no filename matcher for a STAC source",
                self.collection_id
            ))
        })
    }

    /// The catalog scan of a directory or prefix source: its matcher and
    /// `exclude_patterns`, labelled with the collection id. Callers add the
    /// window and cap their mode needs.
    fn scan_spec(&self) -> Result<ScanSpec<'_>, DataServerError> {
        Ok(ScanSpec {
            exclude: &self.exclude_patterns,
            ..ScanSpec::new(self.filename_matcher()?, &self.collection_id)
        })
    }

    /// Perform a scan appropriate to the store mode.
    /// Applies max_files limit if configured.
    /// `current` is the previous catalog, used to reuse metadata for unchanged files.
    fn do_scan(&self, current: &Catalog) -> Result<Catalog, DataServerError> {
        // Build path-based index from current catalog (references only, no cloning)
        let path_index: HashMap<&Path, &catalog::FileEntry> = current
            .entries
            .values()
            .map(|e| (e.path.as_path(), e))
            .collect();

        let mut catalog = match &self.store_mode {
            StoreMode::Local { directory, pending } => {
                let mut pending = match pending.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => {
                        tracing::warn!(
                            "[{}] Pending file tracker poisoned, recovering",
                            self.collection_id
                        );
                        poisoned.into_inner()
                    }
                };
                scan_directory(
                    directory,
                    self.filename_matcher()?,
                    &self.exclude_patterns,
                    &mut pending,
                    &path_index,
                )?
            }
            StoreMode::Remote { store, prefix } => {
                let spec = ScanSpec {
                    max_files: self.max_files,
                    ..self.scan_spec()?
                };
                let (catalog, mut failed) =
                    scan_remote(store, std::slice::from_ref(prefix), &spec, &path_index)?;
                // The one prefix failing to list is the scan failing.
                if let Some((_, e)) = failed.pop() {
                    return Err(e);
                }
                catalog
            }
            StoreMode::RemoteDynamic {
                store,
                prefix_pattern,
                scan_days,
                time_window,
            } => {
                let now = Utc::now();
                let (prefixes, time_filter) = if let Some(tw) = time_window {
                    let (start, end) = tw.to_range(now);
                    (
                        expand_prefix_for_range(prefix_pattern, start, end)?,
                        Some((start, end)),
                    )
                } else {
                    (expand_prefix_pattern(prefix_pattern, *scan_days)?, None)
                };
                let prefixes: Vec<ds_storage::object_store::path::Path> = prefixes
                    .iter()
                    .map(|prefix| ds_storage::object_store::path::Path::from(prefix.as_str()))
                    .collect();
                // No cap before the metadata pass: `max_files` trims below,
                // after files that fail to parse are dropped.
                let spec = ScanSpec {
                    time_filter,
                    ..self.scan_spec()?
                };
                let (catalog, failed) = scan_remote(store, &prefixes, &spec, &path_index)?;
                if !failed.is_empty() {
                    tracing::warn!(
                        "[{}] {}/{} prefix scan(s) failed: {}",
                        self.collection_id,
                        failed.len(),
                        prefixes.len(),
                        failed
                            .iter()
                            .map(|(p, e)| format!("'{}': {}", p, e))
                            .collect::<Vec<_>>()
                            .join("; ")
                    );
                }
                catalog
            }
            StoreMode::RemoteStac { client } => {
                let current_catalog = self.catalog.load();
                catalog::poll_stac_latest(client, &current_catalog, &self.collection_id)?
            }
        };

        // Apply config overrides for nodata/scale/offset
        if self.override_nodata.is_some()
            || self.override_scale.is_some()
            || self.override_offset.is_some()
        {
            for entry in catalog.entries.values_mut() {
                if let Some(metadata) = entry.metadata_mut() {
                    Arc::make_mut(metadata).apply_overrides(
                        self.override_nodata,
                        self.override_scale,
                        self.override_offset,
                    );
                }
            }
        }

        if let Some(max) = self.max_files {
            catalog.trim_to_latest(max);
        }

        Ok(catalog)
    }

    /// Fetch GeoTIFF metadata for a STAC asset: the one loader behind both the
    /// poll-cycle preload (#90, awaited on the poll runtime) and the lazy
    /// request path (bridged by [`load_stac_entry_metadata`](Self::load_stac_entry_metadata)).
    ///
    /// Uses the StacClient's reqwest-based HTTP methods directly
    /// (bypassing object_store which URL-encodes path components and breaks
    /// servers like Ceph RGW that use colons in paths).
    ///
    /// Tries COG range read first (512KB header), falls back to full download.
    async fn fetch_stac_entry_metadata(
        &self,
        stac_client: &stac::StacClient,
        asset_url: &str,
        file_size: u64,
    ) -> Result<(reader::TiffMetadata, reader::DataSource), DataServerError> {
        // Get actual file size if not known from STAC
        let actual_size = if file_size == 0 {
            stac_client.head_asset_async(asset_url).await.unwrap_or(0)
        } else {
            file_size
        };

        if actual_size > catalog::MAX_REMOTE_FILE_SIZE {
            return Err(DataServerError::Engine(format!(
                "File too large ({} > {} max)",
                format_bytes(actual_size),
                format_bytes(catalog::MAX_REMOTE_FILE_SIZE)
            )));
        }

        // Try COG range read first (only 512KB header) — creates HttpDirect source
        // that fetches tiles on demand via byte-range reads.
        let http = stac_client.http_client();
        if actual_size > 0 {
            if let Some((metadata, tile_info)) =
                reader::TiffMetadata::from_http_header_read(&http, asset_url, actual_size).await
            {
                tracing::debug!(
                    "[{}] STAC COG range read '{}' (header only, {} tiles)",
                    self.collection_id,
                    asset_url,
                    tile_info.tile_offsets.len()
                );
                let source = reader::DataSource::HttpDirect {
                    url: asset_url.to_string(),
                    http,
                    tile_info,
                };
                return Ok((metadata, source));
            }
        }

        // Fallback: download the full file into memory (non-COG or range read failed)
        tracing::debug!(
            "[{}] STAC downloading '{}' ({})",
            self.collection_id,
            asset_url,
            format_bytes(actual_size)
        );

        let data = stac_client.get_asset_async(asset_url).await?;
        let source = reader::DataSource::from_bytes(data);
        let metadata = reader::TiffMetadata::from_source(&source)?;

        Ok((metadata, source))
    }

    /// Load GeoTIFF metadata for a STAC stub entry on the calling thread, for
    /// the lazy request-path fallback: one sync bridge over
    /// [`fetch_stac_entry_metadata`](Self::fetch_stac_entry_metadata).
    fn load_stac_entry_metadata(
        &self,
        asset_url: &str,
        file_size: u64,
    ) -> Result<(reader::TiffMetadata, reader::DataSource), DataServerError> {
        let stac_client = match &self.store_mode {
            StoreMode::RemoteStac { client } => client,
            _ => {
                return Err(DataServerError::Engine(
                    "load_stac_entry_metadata called on non-STAC engine".into(),
                ))
            }
        };
        stac_client.block_on(self.fetch_stac_entry_metadata(stac_client, asset_url, file_size))
    }

    /// Check if a catalog entry's metadata is already loaded.
    /// Returns `true` if loaded or entry doesn't exist (nothing to do).
    fn is_metadata_loaded(&self, timestamp: &DateTime<Utc>) -> bool {
        let catalog = self.catalog.load();
        match catalog.entries.get(timestamp) {
            Some(entry) => entry.is_loaded(),
            None => true, // Entry doesn't exist — nothing to load
        }
    }

    /// Publish fetched metadata for the STAC entry at `timestamp`: apply the
    /// config overrides, promote the stub and refresh the cached snapshot.
    /// `rcu`, not load-then-store: the poll-cycle preload (#90) and request
    /// loads of other entries install concurrently and must not drop each
    /// other's update.
    fn install_stac_metadata(
        &self,
        timestamp: &DateTime<Utc>,
        mut metadata: reader::TiffMetadata,
        source: reader::DataSource,
    ) {
        metadata.apply_overrides(
            self.override_nodata,
            self.override_scale,
            self.override_offset,
        );
        let (metadata, source) = (Arc::new(metadata), Arc::new(source));
        self.catalog.rcu(|current| {
            let mut next = (**current).clone();
            if let Some(entry) = next.entries.get_mut(timestamp) {
                entry.set_loaded(Arc::clone(&metadata), Arc::clone(&source));
            }
            next.recompute_extents();
            next
        });
        // A STAC stub just gained metadata (CRS/grid/extent) — refresh the
        // cached snapshot so `raster_info()` reflects it immediately rather than
        // waiting for the next poll. Cheap + I/O-free (the metadata is already
        // loaded), so it's safe here even on the render path (#211 review).
        self.refresh_raster_info();
    }

    /// Load metadata for a stub entry and update the catalog.
    fn do_load_metadata(
        &self,
        timestamp: &DateTime<Utc>,
        asset_url: &str,
        file_size: u64,
    ) -> Result<(), DataServerError> {
        let (metadata, source) = self.load_stac_entry_metadata(asset_url, file_size)?;
        self.install_stac_metadata(timestamp, metadata, source);
        Ok(())
    }

    /// Ensure a single entry's metadata is loaded. No-op if already loaded.
    /// Uses `loading_in_flight` to prevent concurrent loads for the same entry.
    fn ensure_metadata(&self, timestamp: &DateTime<Utc>) -> Result<(), DataServerError> {
        // Fast path: check if already loaded
        if self.is_metadata_loaded(timestamp) {
            return Ok(());
        }

        // Get the stub info we need before acquiring the in-flight lock
        let (path, asset_url, file_size) = {
            let catalog = self.catalog.load();
            let entry = match catalog.entries.get(timestamp) {
                Some(e) => e,
                None => return Ok(()),
            };
            let stub = match entry.stac_stub_info() {
                Some(s) => s,
                None => return Ok(()), // Not a stub, but metadata is None — shouldn't happen
            };
            (entry.path.clone(), stub.asset_url.clone(), entry.file_size)
        };

        // Single-flight: park until any in-flight load of this path finishes,
        // then atomically either observe its result or claim the load. The
        // done-check happens under the in-flight lock, so two losers of the
        // race can't both end up loading the same asset. The guard releases
        // the claim and wakes waiters on drop — success, error, or panic.
        let _guard = match self
            .loading_in_flight
            .wait_then_claim(&path, || self.is_metadata_loaded(timestamp))
        {
            None => return Ok(()),
            Some(guard) => guard,
        };

        self.do_load_metadata(timestamp, &asset_url, file_size)
    }

    /// Preload metadata for the STAC items this poll cycle discovered (#90),
    /// so the first request for a new timestep doesn't fetch the asset header
    /// on a request worker. Entries absent from `previous` (the snapshot from
    /// before the scan) are candidates, newest first, capped at
    /// `STAC_PRELOAD_MAX_ITEMS` with `STAC_PRELOAD_CONCURRENCY` fetches in
    /// flight, within `STAC_PRELOAD_BUDGET`. Async on the poll runtime: no
    /// `block_in_place`, no sequential round trips. Anything skipped, failed
    /// or cut off by the budget keeps the lazy
    /// [`ensure_metadata`](Self::ensure_metadata) fallback.
    async fn preload_stac_metadata(&self, previous: &Catalog) {
        let StoreMode::RemoteStac { client } = &self.store_mode else {
            return;
        };
        let targets: Vec<(DateTime<Utc>, PathBuf, String, u64)> = {
            let catalog = self.catalog.load();
            catalog
                .entries
                .iter()
                .rev()
                .filter(|(ts, _)| !previous.entries.contains_key(*ts))
                .filter_map(|(ts, entry)| {
                    let stub = entry.stac_stub_info()?;
                    Some((
                        *ts,
                        entry.path.clone(),
                        stub.asset_url.clone(),
                        entry.file_size,
                    ))
                })
                .take(STAC_PRELOAD_MAX_ITEMS)
                .collect()
        };
        if targets.is_empty() {
            return;
        }
        let attempted = targets.len();
        let preload = futures::stream::iter(targets)
            .map(|(ts, path, asset_url, file_size)| {
                self.preload_stac_entry(client, ts, path, asset_url, file_size)
            })
            .buffer_unordered(STAC_PRELOAD_CONCURRENCY)
            .filter(|loaded| futures::future::ready(*loaded))
            .count();
        // On timeout the unfinished fetches drop, releasing their claims.
        match tokio::time::timeout(STAC_PRELOAD_BUDGET, preload).await {
            Ok(loaded) => tracing::debug!(
                "[{}] STAC preload: metadata for {}/{} new items",
                self.collection_id,
                loaded,
                attempted
            ),
            Err(_) => tracing::warn!(
                "[{}] STAC preload of {} new items exceeded {}s; the rest is left to the request path",
                self.collection_id,
                attempted,
                STAC_PRELOAD_BUDGET.as_secs()
            ),
        }
    }

    /// Preload one entry for [`preload_stac_metadata`](Self::preload_stac_metadata).
    /// Claims the path like a request would, so a request for this item
    /// meanwhile waits for the preload instead of fetching it again; skips
    /// the item if a request already holds the claim. `true` if it loaded.
    async fn preload_stac_entry(
        &self,
        client: &stac::StacClient,
        timestamp: DateTime<Utc>,
        path: PathBuf,
        asset_url: String,
        file_size: u64,
    ) -> bool {
        let Some(_guard) = self
            .loading_in_flight
            .try_claim(&path, || self.is_metadata_loaded(&timestamp))
        else {
            return false;
        };
        match self
            .fetch_stac_entry_metadata(client, &asset_url, file_size)
            .await
        {
            Ok((metadata, source)) => {
                // Installed before the guard drops, so woken waiters see it.
                self.install_stac_metadata(&timestamp, metadata, source);
                true
            }
            Err(e) => {
                tracing::warn!(
                    "[{}] STAC preload of '{}' failed, left to the request path: {e}",
                    self.collection_id,
                    asset_url
                );
                false
            }
        }
    }

    /// Ensure entries exist and have metadata loaded for the requested datetime range.
    ///
    /// For STAC mode: if the catalog has no entries for the requested range,
    /// fetches items from the STAC API first (on-demand discovery), then
    /// lazy-loads GeoTIFF metadata for the matching entries.
    fn ensure_entries_loaded(
        &self,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
    ) -> Result<(), DataServerError> {
        // For STAC mode: fetch items on-demand if we don't have entries for this range
        if let StoreMode::RemoteStac { ref client, .. } = self.store_mode {
            self.check_stac_circuit_breaker()?;

            if let Some(range) = datetime {
                let catalog = self.catalog.load();
                let existing = filter_by_datetime(&catalog.entries, Some(range));
                if existing.is_empty() {
                    // No entries for this range — fetch from STAC API
                    drop(catalog);
                    let current = self.catalog.load();
                    match catalog::fetch_stac_range(client, &current, range, &self.collection_id) {
                        Ok(updated) => {
                            self.record_stac_success();
                            self.store_stac_catalog(updated);
                        }
                        Err(e) => {
                            self.record_stac_failure();
                            return Err(e);
                        }
                    }
                }
            } else {
                // No datetime filter (e.g., "latest") — fetch recent items if catalog is empty
                let catalog = self.catalog.load();
                if catalog.entries.is_empty() {
                    drop(catalog);
                    let now = Utc::now();
                    let since = now - chrono::Duration::hours(1);
                    let current = self.catalog.load();
                    match catalog::fetch_stac_range(
                        client,
                        &current,
                        (since, now),
                        &self.collection_id,
                    ) {
                        Ok(updated) => {
                            self.record_stac_success();
                            self.store_stac_catalog(updated);
                        }
                        Err(e) => {
                            self.record_stac_failure();
                            return Err(e);
                        }
                    }
                }
            }
        }

        let catalog = self.catalog.load();
        let entries = filter_by_datetime(&catalog.entries, datetime);

        // Collect timestamps of unloaded entries (most recent first)
        let unloaded: Vec<DateTime<Utc>> = entries
            .iter()
            .rev()
            .filter(|(_, entry)| !entry.is_loaded())
            .map(|(ts, _)| **ts)
            .collect();

        drop(catalog);

        for ts in &unloaded {
            self.ensure_metadata(ts)?;
        }

        Ok(())
    }

    /// Run the polling loop. Call this from a tokio::spawn task.
    /// The loop exits gracefully when `shutdown()` is called.
    ///
    /// On consecutive failures, the poll interval backs off exponentially
    /// (2×, 4×, 8×, up to 16× the base interval) to avoid hammering a
    /// failing remote. Resets to base interval on first success.
    pub async fn poll_loop(&self) {
        let base = self.poll_interval;
        // No poll has warmed what the startup scan catalogued: warm its
        // newest frames before the first sleep (#1004).
        self.prewarm_new_frames(&Catalog::empty()).await;

        loop {
            let failures = self.consecutive_poll_failures.load(Ordering::Relaxed);
            let backoff_multiplier = if failures == 0 {
                1
            } else {
                (1u32 << failures.min(4)).min(16) // 2, 4, 8, 16 cap
            };
            // Per-iteration delay (backoff), so an interruptible sleep rather
            // than the fixed-cadence ticker the other engines use.
            if !self.shutdown.sleep(base * backoff_multiplier).await {
                break;
            }
            self.poll_cycle().await;
        }
        tracing::info!("[{}] Poll loop shutting down", self.collection_id);
    }

    /// Signal the polling loop to stop.
    pub fn shutdown(&self) {
        self.shutdown.shutdown();
    }

    /// One poll cycle: rescan, then preload metadata for the STAC items the
    /// scan discovered (#90), then pre-warm the tiles of the remote frames it
    /// discovered (#1004). The preload is a no-op for other sources, whose
    /// scan already parses every header; the pre-warm for local ones.
    async fn poll_cycle(&self) {
        let previous = self.catalog.load_full();
        self.poll_once();
        self.preload_stac_metadata(&previous).await;
        self.prewarm_new_frames(&previous).await;
    }

    /// Read the encoded tiles of the newest remote frames absent from
    /// `previous` into the tile cache (#1004), so the first view of a new
    /// frame decodes from memory instead of paying a storage round trip per
    /// meta-tile. At most `PREWARM_MAX_FRAMES` frames, within the per-frame
    /// cap of [`prewarm::frame_cap`] and `PREWARM_BUDGET`; runs on the poll
    /// runtime with bounded concurrency, see [`prewarm`]. Returns the paths
    /// of the frames it had tiles to fetch for.
    async fn prewarm_new_frames(&self, previous: &Catalog) -> Vec<PathBuf> {
        if matches!(self.store_mode, StoreMode::Local { .. }) || self.shutdown.is_shutdown() {
            return Vec::new();
        }
        let cap = prewarm::frame_cap(self.tile_cache.capacity());
        if cap == 0 {
            return Vec::new();
        }
        let plans: Vec<prewarm::FramePlan> = {
            let catalog = self.catalog.load();
            catalog
                .entries
                .iter()
                .rev()
                .filter(|(ts, _)| !previous.entries.contains_key(*ts))
                .filter_map(|(_, entry)| {
                    prewarm::plan_frame(
                        &entry.path,
                        entry.metadata()?,
                        entry.source()?,
                        &self.tile_cache,
                        cap,
                    )
                })
                .take(PREWARM_MAX_FRAMES)
                .filter(|plan| !plan.is_empty())
                .collect()
        };
        if plans.is_empty() {
            return Vec::new();
        }
        let started = std::time::Instant::now();
        let outcome = prewarm::warm(&plans, &self.tile_cache, PREWARM_BUDGET, || {
            self.shutdown.is_shutdown()
        })
        .await;
        let capped = if outcome.capped_levels > 0 {
            format!(
                "; {} finest level(s) over the {} per-frame cap left to requests",
                outcome.capped_levels,
                format_bytes(cap as u64)
            )
        } else {
            String::new()
        };
        if outcome.reads > 0 {
            tracing::info!(
                "[{}] Pre-warmed {} new frame(s) for first views: {} tiles, {} in {} range reads, {} ms{}",
                self.collection_id,
                outcome.frames,
                outcome.tiles,
                format_bytes(outcome.bytes as u64),
                outcome.reads,
                started.elapsed().as_millis(),
                capped
            );
        }
        if outcome.failed > 0 || outcome.unfinished > 0 || outcome.busy > 0 {
            tracing::warn!(
                "[{}] Pre-warm left tiles to first views: {} range read(s) failed, {} cut off after {}s, \
                 {} skipped while requests held over half the decode budget{}",
                self.collection_id,
                outcome.failed,
                outcome.unfinished,
                PREWARM_BUDGET.as_secs(),
                outcome.busy,
                outcome
                    .first_error
                    .as_deref()
                    .map(|e| format!(" (first error: {e})"))
                    .unwrap_or_default()
            );
        }
        if outcome.deferred > 0 {
            tracing::debug!(
                "[{}] Pre-warm skipped {} range read(s): shutting down, or a read over half the decode budget",
                self.collection_id,
                outcome.deferred
            );
        }
        plans.iter().map(|plan| plan.path().to_path_buf()).collect()
    }

    fn poll_once(&self) {
        let current = self.catalog.load();
        let result = self.do_scan(&current);

        match result {
            Ok(mut new_catalog) => {
                let old_count = current.entries.len();

                // Merge loaded metadata from current catalog into new catalog.
                // Prevents race where lazy loads completed between scan-start
                // and catalog-swap are overwritten by stubs.
                // This is correct: Arc-cloning state is cheap (pointer bump).
                for (timestamp, current_entry) in &current.entries {
                    if current_entry.is_loaded() {
                        if let Some(new_entry) = new_catalog.entries.get_mut(timestamp) {
                            if !new_entry.is_loaded() {
                                new_entry.state = current_entry.state.clone();
                            }
                        }
                    }
                }

                let count = new_catalog.entries.len();
                let total_bytes: u64 = new_catalog.entries.values().map(|e| e.file_size).sum();

                if count == 0 && old_count > 0 {
                    let failures = self
                        .consecutive_poll_failures
                        .fetch_add(1, Ordering::Relaxed)
                        + 1;
                    if failures >= 10 {
                        tracing::error!(
                            "[{}] Poll returned 0 files for {} consecutive cycles (was {}). \
                             Data source may be permanently unavailable. Serving stale data.",
                            self.collection_id,
                            failures,
                            old_count
                        );
                    } else {
                        tracing::warn!(
                            "[{}] Poll returned 0 files (was {}, {} consecutive). \
                             Keeping old catalog. Check data source connectivity and filename pattern.",
                            self.collection_id,
                            old_count,
                            failures
                        );
                    }
                    return;
                }

                self.consecutive_poll_failures.store(0, Ordering::Relaxed);
                self.catalog.store(Arc::new(new_catalog));
                // Rebuild the cached RasterInfo off the request path (#211).
                self.refresh_raster_info();
                // Stamp the poll age only when the scan found files: an
                // empty scan over a catalog that was already empty must not
                // reset it (#1007).
                if count > 0 {
                    *self
                        .catalog_updated_at
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = Some(Utc::now());
                }
                let (hits, misses) = self.tile_cache.stats();
                tracing::debug!(
                    "[{}] Poll: {} files ({}), tile cache: {} hits / {} misses",
                    self.collection_id,
                    count,
                    format_bytes(total_bytes),
                    hits,
                    misses
                );
            }
            Err(e) => {
                let failures = self
                    .consecutive_poll_failures
                    .fetch_add(1, Ordering::Relaxed)
                    + 1;
                if failures >= 10 {
                    tracing::error!(
                        "[{}] Scan failed for {} consecutive cycles: {e}. Serving stale data.",
                        self.collection_id,
                        failures
                    );
                } else {
                    tracing::warn!(
                        "[{}] Scan failed (attempt {}), keeping old catalog: {e}",
                        self.collection_id,
                        failures
                    );
                }
            }
        }
    }
}

impl GeoTiffEngine {
    /// Core query: extract a time series of pixel values at a given (lat, lon).
    fn query_point(
        &self,
        lat: f64,
        lon: f64,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
    ) -> Result<QueryResult, DataServerError> {
        ds_core::edr_engine::select_parameters(parameters, &[self.parameter.as_str()])?;

        // Lazily load STAC stubs for the requested time range
        self.ensure_entries_loaded(datetime)?;

        let catalog = self.catalog.load();
        let entries = filter_by_datetime(&catalog.entries, datetime);

        if entries.is_empty() {
            return Err(DataServerError::LocationNotFound(
                "No data available for the requested coordinates/time range".into(),
            ));
        }

        let mut times = Vec::with_capacity(entries.len());
        let mut values = Vec::with_capacity(entries.len());

        for (timestamp, entry) in &entries {
            times.push(**timestamp);

            let (metadata, source) = match (entry.metadata(), entry.source()) {
                (Some(m), Some(s)) => (m, s),
                _ => {
                    values.push(None);
                    continue;
                }
            };

            let pixel = metadata.geo_transform.world_to_pixel(lon, lat);
            let value = match pixel {
                Some((col, row)) => {
                    match reader::read_pixel(
                        source,
                        metadata,
                        col,
                        row,
                        Some(&self.tile_cache),
                        &entry.path,
                        self.band_index,
                    ) {
                        Ok(v) => v,
                        Err(
                            e @ (DataServerError::ResourceExhausted
                            | DataServerError::DeadlineExceeded),
                        ) => return Err(e),
                        Err(e) => {
                            tracing::warn!(
                                "Failed to read pixel from {}: {e}",
                                entry.path.display()
                            );
                            None
                        }
                    }
                }
                None => None,
            };
            values.push(value);
        }

        let domain = DomainDescription::PointSeries {
            x: lon,
            y: lat,
            t: times,
            z: None,
        };

        let mut param_descs = HashMap::new();
        param_descs.insert(
            self.parameter.clone(),
            ParameterDescription {
                label: self.parameter.replace('_', " "),
                unit: self.unit.clone(),
                observed_property: self.parameter.clone(),
                standard_name: None,
            },
        );

        let mut ranges = HashMap::new();
        ranges.insert(
            self.parameter.clone(),
            NdArray {
                shape: vec![values.len()],
                axis_names: vec!["t".to_string()],
                values,
            },
        );

        Ok(QueryResult {
            domain,
            parameters: param_descs,
            ranges,
        })
    }

    /// Area query: extract a grid of pixel values within a bounding box.
    fn query_bbox(
        &self,
        west: f64,
        south: f64,
        east: f64,
        north: f64,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
    ) -> Result<QueryResult, DataServerError> {
        ds_core::edr_engine::select_parameters(parameters, &[self.parameter.as_str()])?;

        // Lazily load STAC stubs for the requested time range
        self.ensure_entries_loaded(datetime)?;

        let catalog = self.catalog.load();
        let entries = filter_by_datetime(&catalog.entries, datetime);

        if entries.is_empty() {
            return Err(DataServerError::LocationNotFound(
                "No data available for the requested time range".into(),
            ));
        }

        // Use first entry's geo_transform to compute pixel range and axis values
        let first_entry = entries[0].1;
        let first_metadata = first_entry
            .metadata()
            .ok_or_else(|| DataServerError::Engine("First entry has no metadata loaded".into()))?;
        let (col_start, row_start, col_end, row_end) = first_metadata
            .geo_transform
            .bbox_to_pixels(west, south, east, north)
            .ok_or_else(|| {
                DataServerError::InvalidParameter(
                    "Requested bbox does not intersect the raster".into(),
                )
            })?;

        let nx = (col_end - col_start) as usize;
        let ny = (row_end - row_start) as usize;
        // The shared per-response budget (#673) over EVERY matching timestep,
        // before the `timesteps × ny × nx` result is allocated or a file read
        // (#858). The answer stays at native resolution, like GRIB: no
        // `MAX_AREA_DIM` coarsening, so an over-budget window is the 400.
        ds_core::feature::check_area_budget(entries.len(), ny, nx, 1)?;

        // Build x and y axis values (pixel centers).
        // For projected CRS (LCC, TM, etc.) WGS84 axes aren't truly separable —
        // lon depends on both col and row. Use the center row/col of the range
        // for the best approximation in a CoverageJSON Grid domain.
        let mid_row = (row_start + row_end) / 2;
        let mid_col = (col_start + col_end) / 2;
        let x_values: Vec<f64> = (col_start..col_end)
            .map(|c| first_metadata.geo_transform.pixel_to_world(c, mid_row).0)
            .collect();
        let y_values: Vec<f64> = (row_start..row_end)
            .map(|r| first_metadata.geo_transform.pixel_to_world(mid_col, r).1)
            .collect();

        let has_time = entries.len() > 1;
        let mut times = Vec::with_capacity(entries.len());
        let mut all_values = Vec::with_capacity(entries.len() * ny * nx);

        for (timestamp, entry) in &entries {
            times.push(**timestamp);

            let (metadata, source) = match (entry.metadata(), entry.source()) {
                (Some(m), Some(s)) => (m, s),
                _ => {
                    all_values.extend(std::iter::repeat_n(None, nx * ny));
                    continue;
                }
            };

            match reader::read_bbox(
                source,
                metadata,
                col_start,
                row_start,
                col_end,
                row_end,
                Some(&self.tile_cache),
                &entry.path,
                self.band_index,
            ) {
                Ok(grid_values) => {
                    all_values.extend(grid_values);
                }
                Err(e) if is_unreadable_file(&e) => {
                    tracing::warn!("Failed to read bbox from {}: {e}", entry.path.display());
                    // Fill with None for this timestep
                    all_values.extend(std::iter::repeat_n(None, nx * ny));
                }
                Err(e) => return Err(e),
            }
        }

        let domain = if has_time {
            DomainDescription::Grid {
                x: x_values.clone(),
                y: y_values.clone(),
                t: Some(times),
                z: None,
            }
        } else {
            DomainDescription::Grid {
                x: x_values.clone(),
                y: y_values.clone(),
                t: None,
                z: None,
            }
        };

        let (shape, axis_names) = if has_time {
            (
                vec![entries.len(), ny, nx],
                vec!["t".to_string(), "y".to_string(), "x".to_string()],
            )
        } else {
            (vec![ny, nx], vec!["y".to_string(), "x".to_string()])
        };

        let mut param_descs = HashMap::new();
        param_descs.insert(
            self.parameter.clone(),
            ParameterDescription {
                label: self.parameter.replace('_', " "),
                unit: self.unit.clone(),
                observed_property: self.parameter.clone(),
                standard_name: None,
            },
        );

        let mut ranges = HashMap::new();
        ranges.insert(
            self.parameter.clone(),
            NdArray {
                shape,
                axis_names,
                values: all_values,
            },
        );

        Ok(QueryResult {
            domain,
            parameters: param_descs,
            ranges,
        })
    }
}

/// Whether an EDR area read error means the file itself could not be read
/// (fetch, decode or I/O failure), which the query answers as a timestep of
/// nulls. Every other error belongs to the request and fails it (#858):
/// admission and deadline (503/504), and client errors such as
/// `QueryTooLarge` / `InvalidParameter` (400) — never nulls with HTTP 200.
fn is_unreadable_file(e: &DataServerError) -> bool {
    matches!(
        e,
        DataServerError::Engine(_) | DataServerError::Storage(_) | DataServerError::Io(_)
    )
}

/// Filter catalog entries by an optional datetime range.
/// Returns references to matching (timestamp, entry) pairs.
fn filter_by_datetime(
    entries: &BTreeMap<DateTime<Utc>, catalog::FileEntry>,
    datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> Vec<(&DateTime<Utc>, &catalog::FileEntry)> {
    match datetime {
        Some((start, end)) => entries.range(start..=end).collect(),
        None => entries.iter().collect(),
    }
}

/// Map a parsed [`Crs`](ds_core::geo::Crs) to the engine's native-CRS label
/// (stored in `RasterInfo.native_crs`, surfaced as OGC `storageCrs`).
///
/// Projected variants only claim a specific EPSG code when their parameters
/// match a grid with a stable code (TM35FIN → EPSG:3067, ETRS89-LAEA Europe →
/// EPSG:3035); any other zone gets a generic label so the API layer omits
/// `storageCrs` rather than asserting a wrong code. WGS84 maps to `"CRS:84"`
/// (lon-first), not `"EPSG:4326"` (lat-first), to match the engine's internal
/// axis order. Rotated lat/lon has no standard EPSG code.
fn crs_label(crs: &ds_core::geo::Crs) -> String {
    // Parameter tolerances: angles compared in radians, offsets in metres.
    const ANG: f64 = 1e-9;
    const OFF: f64 = 1e-3;
    match crs {
        ds_core::geo::Crs::Wgs84 => "CRS:84".to_string(),
        ds_core::geo::Crs::TransverseMercator {
            lat0,
            lon0,
            k0,
            false_e,
            false_n,
        } => {
            let is_tm35fin = lat0.abs() < ANG
                && (lon0 - 27.0_f64.to_radians()).abs() < ANG
                && (k0 - 0.9996).abs() < 1e-9
                && (false_e - 500_000.0).abs() < OFF
                && false_n.abs() < OFF;
            if is_tm35fin {
                "EPSG:3067".to_string()
            } else {
                "TM".to_string()
            }
        }
        ds_core::geo::Crs::LambertAzimuthalEqualArea {
            lat0,
            lon0,
            false_e,
            false_n,
        } => {
            let is_etrs89_laea = (lat0 - 52.0_f64.to_radians()).abs() < ANG
                && (lon0 - 10.0_f64.to_radians()).abs() < ANG
                && (false_e - 4_321_000.0).abs() < OFF
                && (false_n - 3_210_000.0).abs() < OFF;
            if is_etrs89_laea {
                "EPSG:3035".to_string()
            } else {
                "LAEA".to_string()
            }
        }
        // No stable EPSG code for arbitrary LCC/Stereographic params; use the
        // same descriptive labels as engine-odim so native_crs_uri (the single
        // storageCrs source of truth) treats both engines identically.
        ds_core::geo::Crs::LambertConformalConic { .. } => "LCC".to_string(),
        ds_core::geo::Crs::Stereographic { .. } => "stere".to_string(),
        // Rotated lat/lon is NOT EPSG:4326 — it has no standard EPSG code.
        ds_core::geo::Crs::RotatedLatLon { .. } => "rotated_ll".to_string(),
        ds_core::geo::Crs::Geostationary { .. } => "geos".to_string(),
        ds_core::geo::Crs::WebMercator => "EPSG:3857".to_string(),
    }
}

impl ds_core::map_engine::MapEngine for GeoTiffEngine {
    #[allow(clippy::too_many_arguments)]
    fn get_raster_tile(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        output_crs: &ds_core::map_engine::OutputCrs,
        parameter: Option<&str>,
        z: Option<f64>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<ds_core::map_engine::RasterTile, DataServerError> {
        let _ = (parameter, z); // GeoTIFF engine serves a single 2-D band per collection
                                // For STAC: ensure we have items around the requested time
        if let StoreMode::RemoteStac { ref client, .. } = self.store_mode {
            self.check_stac_circuit_breaker()?;

            let catalog = self.catalog.load();
            let need_fetch = if let Some(t) = time {
                // Check if we have any entry near this time
                catalog.entries.range(..=t).next_back().is_none()
                    && catalog.entries.iter().next().is_none()
            } else {
                catalog.entries.is_empty()
            };
            if need_fetch {
                drop(catalog);
                let now = time.unwrap_or_else(Utc::now);
                let range = (
                    now - chrono::Duration::hours(1),
                    now + chrono::Duration::minutes(5),
                );
                let current = self.catalog.load();
                match catalog::fetch_stac_range(client, &current, range, &self.collection_id) {
                    Ok(updated) => {
                        self.record_stac_success();
                        self.store_stac_catalog(updated);
                    }
                    Err(e) => {
                        self.record_stac_failure();
                        return Err(e);
                    }
                }
            }
        }

        // Find the target timestamp first, then ensure it's loaded. The
        // selection lives on Catalog so resolve_time (the cache-key
        // authority, #507) shares it verbatim.
        let target_timestamp = self.catalog.load().select_timestamp(time).ok_or_else(|| {
            DataServerError::Engine("No data available for the requested time".into())
        })?;

        // Lazily load STAC stub if needed
        self.ensure_metadata(&target_timestamp)?;

        let catalog = self.catalog.load();
        let (_timestamp, entry) = catalog
            .entries
            .get_key_value(&target_timestamp)
            .ok_or_else(|| DataServerError::Engine("Entry disappeared after loading".into()))?;

        let metadata = entry
            .metadata()
            .ok_or_else(|| DataServerError::Engine("Entry metadata not loaded".into()))?;
        let source = entry
            .source()
            .ok_or_else(|| DataServerError::Engine("Entry source not loaded".into()))?;

        let [west, south, east, north] = bbox;
        let total_pixels = (width as usize) * (height as usize);

        // Select the best overview level for the output resolution.
        // This avoids reading millions of full-resolution pixels for zoomed-out views.
        // If no overview matches but full res is too large, force the smallest overview.
        let overview = metadata
            .select_overview(west, south, east, north, width, height)
            .or_else(|| {
                // select_overview returned None, which now means one of: no
                // overviews exist; full resolution already fits the output; or
                // the output exceeds the biggest overview by more than the
                // bounded-upscale factor (i.e. it is at/near native resolution).
                // In the last two cases we'd read full resolution — but if that
                // would blow the MAX_MAP_PIXELS decode budget we must fall back
                // to an overview regardless of the upscale bound: the hard pixel
                // cap overrides the soft 2× quality preference (an over-cap
                // upscale is still logged by the `Using overview …` debug below).
                // Pick the finest overview that fits under the limit.
                if let Some((c0, r0, c1, r1)) = metadata
                    .geo_transform
                    .bbox_to_pixels(west, south, east, north)
                {
                    let full_pixels = ((c1 - c0) as usize) * ((r1 - r0) as usize);
                    if full_pixels > reader::max_map_pixels() {
                        // Pick the finest overview that fits under the limit.
                        // Overviews are sorted finest-first, so iterate in order
                        // and take the first that fits — the highest quality
                        // within the pixel budget.
                        for ov in &metadata.overviews {
                            let ov_gt = metadata.overview_geo_transform(ov);
                            if let Some((oc0, or0, oc1, or1)) =
                                ov_gt.bbox_to_pixels(west, south, east, north)
                            {
                                let ov_pixels = ((oc1 - oc0) as usize) * ((or1 - or0) as usize);
                                if ov_pixels <= reader::max_map_pixels() {
                                    return Some(ov);
                                }
                            }
                        }
                        // Even the coarsest overview exceeds the limit — use it
                        // anyway as the smallest available source.
                        metadata.overviews.last()
                    } else {
                        None
                    }
                } else {
                    None
                }
            });
        let gt = if let Some(ov) = overview {
            tracing::debug!(
                "Using overview {}x{} (IFD {}) for {}x{} output",
                ov.width,
                ov.height,
                ov.ifd_index,
                width,
                height
            );
            metadata.overview_geo_transform(ov)
        } else {
            metadata.geo_transform.clone()
        };

        // Compute the source pixel range in the selected level
        let source_range = gt.bbox_to_pixels(west, south, east, north);

        let values = if let Some((col_start, row_start, col_end, row_end)) = source_range {
            let src_nx = (col_end - col_start) as usize;

            tracing::debug!(
                "Reading source pixels: cols {}..{} ({}), rows {}..{} ({}), total {} px",
                col_start,
                col_end,
                col_end - col_start,
                row_start,
                row_end,
                row_end - row_start,
                src_nx * ((row_end - row_start) as usize)
            );

            // Map output pixels to source pixels through a coarse projection
            // grid instead of projecting every pixel: the CRS forward transform
            // dominates render CPU for projected sources. The output→world axis
            // mapping (linear lon/lat, Mercator Y, or a projected output CRS) is
            // the shared `OutputCrs::project_node`, so EPSG:3067/3035 output is
            // handled here without per-engine axis math (#160).
            let grid = ds_core::resample::ProjectionGrid::build_2d(
                width,
                height,
                gt.width,
                gt.height,
                |fx, fy| output_crs.project_node(bbox, fx, fy),
                |lon, lat| gt.world_to_pixel_f64(lon, lat),
            );

            // Domain guard against "ghost" echoes. The coarse projection grid
            // only approximates the output→source mapping; at low zoom / extreme
            // viewports (e.g. a whole-world Web Mercator view whose pixels wrap
            // past ±180° or reach the poles) it — and the source projection's own
            // out-of-domain forward — can map a far-away output pixel onto a valid
            // source pixel, painting the radar far from where it belongs (ghosts
            // in the Arctic/Antarctic, smear north of the data). An output pixel
            // may only carry data if its TRUE geography lies within the source's
            // footprint, so bound, in output-pixel space, the window the footprint
            // can occupy and treat everything outside it as nodata. Computed once
            // from the source's WGS84 envelope (no per-pixel projection).
            let footprint = output_crs.footprint_pixel_window(bbox, gt.bbox(), width, height);

            // Native u8 fast path (#206): keep raw bytes end to end — window
            // read, nearest resample, and (in ds-render) a 256-entry-LUT
            // colorize — instead of boxing every sample to a 16-byte
            // `Option<f64>`. `read_bbox_u8` gates itself (local source,
            // u8 chunks, integer u8 nodata) and returns `None` to fall back;
            // both branches render pixel-identically by construction.
            let native = reader::read_bbox_u8(
                source,
                metadata,
                overview,
                col_start,
                row_start,
                col_end,
                row_end,
                self.band_index,
            )?;
            match native {
                Some(window) => {
                    let mut data = Vec::with_capacity(total_pixels);
                    resample_nearest(
                        &grid,
                        width,
                        height,
                        &window.data,
                        src_nx,
                        (col_start, row_start, col_end, row_end),
                        window.nodata,
                        footprint,
                        &mut data,
                    );
                    ds_core::map_engine::RasterValues::U8 {
                        data,
                        nodata: Some(window.nodata),
                        gain: metadata.scale.unwrap_or(1.0),
                        offset: metadata.offset.unwrap_or(0.0),
                    }
                }
                None => {
                    // Boxed fallback: read from overview or full resolution.
                    let pixels = if let Some(ov) = overview {
                        reader::read_bbox_overview(
                            source,
                            metadata,
                            ov,
                            col_start,
                            row_start,
                            col_end,
                            row_end,
                            Some(&self.tile_cache),
                            &entry.path,
                            self.band_index,
                        )
                    } else {
                        reader::read_bbox_map(
                            source,
                            metadata,
                            col_start,
                            row_start,
                            col_end,
                            row_end,
                            Some(&self.tile_cache),
                            &entry.path,
                            self.band_index,
                        )
                    }?;
                    let mut values = Vec::with_capacity(total_pixels);
                    resample_nearest(
                        &grid,
                        width,
                        height,
                        &pixels,
                        src_nx,
                        (col_start, row_start, col_end, row_end),
                        None,
                        footprint,
                        &mut values,
                    );
                    ds_core::map_engine::RasterValues::F64(values)
                }
            }
        } else {
            // Bbox doesn't intersect raster at all — all nodata
            ds_core::map_engine::RasterValues::F64(vec![None; total_pixels])
        };

        Ok(ds_core::map_engine::RasterTile {
            width,
            height,
            values,
        })
    }

    fn raster_info(&self) -> ds_core::map_engine::RasterInfo {
        // O(1): clone the snapshot rebuilt at the last catalog swap
        // (`refresh_raster_info`). No per-request CRS scan, timestamp Vec
        // allocation, or STAC metadata fetch on the request path (#211).
        (*self.raster_info.load_full()).clone()
    }

    fn resolve_time(
        &self,
        time: Option<DateTime<Utc>>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        // The cache-key authority (#507): the exact timestep get_raster_tile
        // will render, via the SAME Catalog::select_timestamp the render
        // path uses. Snapshot-only — no STAC on-demand fetch here (this
        // runs before the cache lookup on the hot path). An empty catalog
        // falls back to the requested time: the render will error and cache
        // nothing, so the key value is moot.
        self.catalog.load().select_timestamp(time).or(time)
    }
}

/// Nearest-neighbour resample of a source window into the output grid,
/// generic over the sample representation (`Option<f64>` boxed path, raw
/// `u8` native path — #206). Byte-for-byte the same mapping in both: out-of
/// -footprint, non-finite-grid, and out-of-window pixels get `fill` (the
/// nodata representation of `T`), everything else copies the enclosing
/// source sample.
#[allow(clippy::too_many_arguments)]
fn resample_nearest<T: Copy>(
    grid: &ds_core::resample::ProjectionGrid,
    width: u32,
    height: u32,
    window: &[T],
    src_nx: usize,
    (col_start, row_start, col_end, row_end): (u32, u32, u32, u32),
    fill: T,
    (px_lo, px_hi, py_lo, py_hi): (u32, u32, u32, u32),
    out: &mut Vec<T>,
) {
    for oy in 0..height {
        let in_y = oy >= py_lo && oy <= py_hi;
        for ox in 0..width {
            if !in_y || ox < px_lo || ox > px_hi {
                out.push(fill);
                continue;
            }
            let (col_f, row_f) = grid.sample(ox, oy);
            if !col_f.is_finite() || !row_f.is_finite() {
                out.push(fill);
                continue;
            }
            let col = col_f.floor();
            let row = row_f.floor();
            if col >= col_start as f64
                && col < col_end as f64
                && row >= row_start as f64
                && row < row_end as f64
            {
                let sc = col as usize - col_start as usize;
                let sr = row as usize - row_start as usize;
                let idx = sr * src_nx + sc;
                out.push(window.get(idx).copied().unwrap_or(fill));
            } else {
                out.push(fill);
            }
        }
    }
}

impl EdrEngine for GeoTiffEngine {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        Ok(vec![])
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
            "GeoTIFF engine does not support named location queries. \
             Use the position query endpoint instead (e.g., /position?coords=POINT(lon lat))."
                .into(),
        ))
    }

    fn query_position(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        let (lat, lon) = parse_coords(coords)?;
        Ok(CoverageResponse::Single(
            self.query_point(lat, lon, datetime, parameters)?,
        ))
    }

    fn query_area(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        let polygon = ds_core::feature::parse_area_coords(coords)?;

        // Get pixel range and geo_transform for accurate per-pixel coordinates.
        // For projected CRS (LCC, TM, etc.), the Grid x/y axes are approximations —
        // each pixel's true WGS84 position depends on both its column and row.
        // We must use pixel_to_world(col, row) for polygon containment tests.
        self.ensure_entries_loaded(datetime)?;
        let catalog = self.catalog.load();
        let entries = filter_by_datetime(&catalog.entries, datetime);
        let geo_transform = entries
            .first()
            .and_then(|(_, e)| e.metadata())
            .map(|m| &m.geo_transform);
        let pixel_range = geo_transform.and_then(|gt| {
            gt.bbox_to_pixels(
                polygon.bbox.west,
                polygon.bbox.south,
                polygon.bbox.east,
                polygon.bbox.north,
            )
        });

        let mut result = self.query_bbox(
            polygon.bbox.west,
            polygon.bbox.south,
            polygon.bbox.east,
            polygon.bbox.north,
            datetime,
            parameters,
        )?;

        // Mask pixels outside the polygon using true per-pixel WGS84 coordinates
        if let DomainDescription::Grid {
            ref x,
            ref y,
            ref t,
            ..
        } = result.domain
        {
            let nt = t.as_ref().map_or(1, |tv| tv.len());
            let ny = y.len();
            let nx = x.len();
            let expected_len = nt * ny * nx;
            let (col_start, row_start) = pixel_range.map_or((0, 0), |(c, r, _, _)| (c, r));
            for ndarray in result.ranges.values_mut() {
                if ndarray.values.len() != expected_len {
                    tracing::error!(
                        "NdArray length mismatch: expected {} ({}*{}*{}), got {}",
                        expected_len,
                        nt,
                        ny,
                        nx,
                        ndarray.values.len()
                    );
                    continue;
                }
                for (iy, y_val) in y.iter().enumerate() {
                    for (ix, x_val) in x.iter().enumerate() {
                        // Compute actual WGS84 coords for this pixel via
                        // pixel_to_world — correct for projected CRS where
                        // lon/lat depend on both column and row.
                        let (lon, lat) = geo_transform.map_or((*x_val, *y_val), |gt| {
                            gt.pixel_to_world(col_start + ix as u32, row_start + iy as u32)
                        });
                        if !polygon.contains(lon, lat) {
                            for it in 0..nt {
                                let idx = it * ny * nx + iy * nx + ix;
                                ndarray.values[idx] = None;
                            }
                        }
                    }
                }
            }
        }

        Ok(CoverageResponse::Single(result))
    }

    fn supported_query_types(&self) -> Vec<String> {
        vec![
            "position".to_string(),
            "area".to_string(),
            "radius".to_string(),
        ]
    }

    fn get_parameters(&self) -> Vec<String> {
        vec![self.parameter.clone()]
    }

    fn get_parameter_descriptions(&self) -> HashMap<String, ParameterDescription> {
        let mut map = HashMap::new();
        map.insert(
            self.parameter.clone(),
            ParameterDescription {
                label: self.parameter.replace('_', " "),
                unit: self.unit.clone(),
                observed_property: self.parameter.clone(),
                standard_name: None,
            },
        );
        map
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        self.catalog.load().temporal_extent
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        self.catalog.load().spatial_extent
    }
}

/// Validate GeoTIFF config for common mistakes that would otherwise cause
/// confusing runtime behavior.
fn validate_config(
    collection_id: &str,
    data_path: Option<&str>,
    config: &GeoTiffConfig,
) -> Result<(), DataServerError> {
    // endpoint and bucket must both be set or both absent
    match (&config.endpoint, &config.bucket) {
        (Some(_), None) => {
            return Err(DataServerError::Engine(format!(
                "[{collection_id}] 'endpoint' is set but 'bucket' is missing — both are required for S3 access"
            )));
        }
        (None, Some(_)) => {
            return Err(DataServerError::Engine(format!(
                "[{collection_id}] 'bucket' is set but 'endpoint' is missing — both are required for S3 access"
            )));
        }
        _ => {}
    }

    // stac_url is mutually exclusive with data_path and endpoint+bucket
    if config.stac_url.is_some() {
        if data_path.is_some() {
            return Err(DataServerError::Engine(format!(
                "[{collection_id}] 'stac_url' and 'data_path' are mutually exclusive"
            )));
        }
        if config.endpoint.is_some() {
            return Err(DataServerError::Engine(format!(
                "[{collection_id}] 'stac_url' and 'endpoint+bucket' are mutually exclusive"
            )));
        }
        // stac_asset_allowlist is required for SSRF protection
        match &config.stac_asset_allowlist {
            None => {
                return Err(DataServerError::Engine(format!(
                    "[{collection_id}] 'stac_asset_allowlist' is required when 'stac_url' is set (SSRF protection)"
                )));
            }
            Some(list) if list.is_empty() => {
                return Err(DataServerError::Engine(format!(
                    "[{collection_id}] 'stac_asset_allowlist' must not be empty"
                )));
            }
            _ => {}
        }
    }

    // Warn if both endpoint+bucket and data_path are set (data_path is silently ignored)
    if config.endpoint.is_some() && data_path.is_some() {
        tracing::warn!(
            "[{}] Both endpoint+bucket and data_path are set; data_path will be ignored in favor of S3",
            collection_id
        );
    }

    // A dynamic S3 prefix is expanded on every poll; reject a template
    // discovery cannot expand now rather than on the first poll.
    if config.endpoint.is_some() && config.stac_url.is_none() {
        if let Some(pattern) = &config.prefix_pattern {
            let time_window = config
                .time_window
                .as_deref()
                .map(TimeWindow::parse)
                .transpose()?;
            validate_prefix_pattern(pattern, time_window.as_ref())?;
        }
    }

    if config.poll_interval_secs == 0 {
        return Err(DataServerError::Engine(format!(
            "[{collection_id}] poll_interval_secs must be > 0"
        )));
    }

    if config.band == 0 {
        return Err(DataServerError::Engine(format!(
            "[{collection_id}] band must be >= 1 (1-based index)"
        )));
    }

    Ok(())
}

/// Build the collection's shared [`FilenameMatcher`] from
/// `filename_template`, or from `filename_pattern` + `timestamp_format`.
///
/// In STAC mode filenames are not matched (timestamps come from STAC
/// properties), so there is no matcher.
fn resolve_filename_config(
    config: &GeoTiffConfig,
) -> Result<Option<FilenameMatcher>, DataServerError> {
    // STAC mode: timestamps come from STAC item properties, not filenames
    if config.stac_url.is_some() {
        return Ok(None);
    }

    let matcher = if let Some(template) = &config.filename_template {
        FilenameMatcher::from_template(template)
    } else if let (Some(pattern), Some(format)) =
        (&config.filename_pattern, &config.timestamp_format)
    {
        FilenameMatcher::from_pattern(pattern, format)
    } else {
        return Err(DataServerError::Engine(
            "Either filename_template or both filename_pattern + timestamp_format must be set"
                .into(),
        ));
    }
    .map_err(|e| DataServerError::Engine(e.to_string()))?;
    tracing::debug!(
        "Filename matcher: regex='{}', format='{}'",
        matcher.pattern(),
        matcher.timestamp_format()
    );
    Ok(Some(matcher))
}

/// Format byte count as human-readable string.
fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

use parse::parse_coords;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_flight_claim_then_done_wakes_waiter() {
        // Loser of the load race parks on the condvar and wakes as soon as
        // the loader's guard drops — no sleep-polling (#213).
        let loads = Arc::new(InFlightLoads::new());
        let path = PathBuf::from("a.tif");
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Loader claims the path.
        let loader_guard = loads.wait_then_claim(&path, || false);
        assert!(loader_guard.is_some());

        let barrier = Arc::new(std::sync::Barrier::new(2));
        let waiter = {
            let loads = Arc::clone(&loads);
            let path = path.clone();
            let done = Arc::clone(&done);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                loads
                    .wait_then_claim(&path, || done.load(Ordering::SeqCst))
                    .is_none()
            })
        };

        // Sync to just before the waiter's call, give it a moment to park,
        // then finish the load successfully.
        barrier.wait();
        std::thread::sleep(Duration::from_millis(50));
        let start = std::time::Instant::now();
        done.store(true, Ordering::SeqCst);
        drop(loader_guard);

        // Waiter observed the completed work — nothing left to do. If the
        // guard's notify were broken, a parked waiter would sleep out the
        // full CONCURRENT_LOAD_WAIT, tripping the elapsed bound.
        assert!(waiter.join().unwrap());
        assert!(start.elapsed() < CONCURRENT_LOAD_WAIT / 2);
        let in_flight = loads.paths.lock().unwrap();
        assert!(!in_flight.contains(&path));
    }

    #[test]
    fn in_flight_loader_failure_hands_claim_to_waiter() {
        // If the loader errors out (guard drops without the work done), the
        // waiter takes over the claim instead of returning success.
        let loads = Arc::new(InFlightLoads::new());
        let path = PathBuf::from("b.tif");

        let loader_guard = loads.wait_then_claim(&path, || false);
        assert!(loader_guard.is_some());

        let barrier = Arc::new(std::sync::Barrier::new(2));
        let waiter = {
            let loads = Arc::clone(&loads);
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                let takeover = loads.wait_then_claim(&path, || false);
                let claimed = takeover.is_some();
                // While the takeover guard is alive, the claim is held.
                if claimed {
                    assert!(loads.paths.lock().unwrap().contains(&path));
                }
                claimed
            })
        };

        barrier.wait();
        std::thread::sleep(Duration::from_millis(50));
        let start = std::time::Instant::now();
        drop(loader_guard);

        assert!(waiter.join().unwrap());
        assert!(start.elapsed() < CONCURRENT_LOAD_WAIT / 2);
        // The waiter's guard dropped at thread end, releasing its claim.
        let in_flight = loads.paths.lock().unwrap();
        assert!(!in_flight.contains(&path));
    }

    #[test]
    fn in_flight_unclaimed_path_claims_immediately() {
        let loads = InFlightLoads::new();
        let path = PathBuf::from("c.tif");
        let guard = loads.wait_then_claim(&path, || false);
        assert!(guard.is_some());
        assert!(loads.paths.lock().unwrap().contains(&path));
        // Already-done work short-circuits without claiming.
        let other = PathBuf::from("d.tif");
        assert!(loads.wait_then_claim(&other, || true).is_none());
        assert!(!loads.paths.lock().unwrap().contains(&other));
        // Dropping the guard releases the claim.
        drop(guard);
        assert!(!loads.paths.lock().unwrap().contains(&path));
    }

    #[test]
    fn s3_prefix_pattern_is_validated_at_load() {
        let s3 = |prefix: &str, time_window: Option<&str>| GeoTiffConfig {
            endpoint: Some("https://s3.example.com".to_string()),
            bucket: Some("radar".to_string()),
            prefix_pattern: Some(prefix.to_string()),
            time_window: time_window.map(str::to_string),
            ..tm35fin_test_config()
        };
        assert!(validate_config("c", None, &s3("%Y/%m/%d/", None)).is_ok());
        assert!(validate_config("c", None, &s3("%Y/%j/%H/", Some("-PT3H"))).is_ok());
        // Hourly without a window, finer than an hour, or unknown: all
        // load errors, never a panic on the first poll.
        assert!(validate_config("c", None, &s3("%Y/%j/%H/", None)).is_err());
        assert!(validate_config("c", None, &s3("%Y/%H%M/", Some("-PT3H"))).is_err());
        assert!(validate_config("c", None, &s3("%Y/%!/", None)).is_err());
    }

    #[test]
    fn crs_label_wgs84_is_crs84_not_epsg4326() {
        // Internal data is lon-first; EPSG:4326 would imply lat-first.
        assert_eq!(crs_label(&ds_core::geo::Crs::Wgs84), "CRS:84");
    }

    fn tm35fin_test_config() -> ds_core::config::GeoTiffConfig {
        ds_core::config::GeoTiffConfig {
            filename_template: Some("radar_tm35_%Y%m%dT%H%MZ.tif".to_string()),
            filename_pattern: None,
            timestamp_format: None,
            parameter: "reflectivity".to_string(),
            unit: "dBZ".to_string(),
            poll_interval_secs: 3600,
            tile_cache_mb: 16,
            band: 1,
            max_files: None,
            nodata: None,
            scale: None,
            offset: None,
            exclude_patterns: vec![],
            endpoint: None,
            bucket: None,
            prefix_pattern: None,
            time_window: None,
            scan_days: None,
            stac_url: None,
            stac_asset_key: "data".to_string(),
            stac_asset_allowlist: None,
        }
    }

    /// #211 review: guards the STAC cold-start behaviour change. A stub-backed
    /// catalog (STAC, pre-render) must report `CRS:84` / no grid — with **no**
    /// metadata fetch (`build_raster_info` is pure) — while `times` still derive
    /// from the entry keys; once an entry is `Loaded` (as `do_load_metadata`
    /// does) and the snapshot is refreshed, the real CRS/grid reappear. Built
    /// over the committed local fixture so a real loaded entry is available.
    #[test]
    fn raster_info_stub_reports_crs84_then_real_crs_after_load() {
        use ds_core::map_engine::MapEngine;
        let config = tm35fin_test_config();
        let engine = GeoTiffEngine::new(
            "radar-tm35fin",
            Some("../../testdata/radar-tm35fin"),
            &config,
        )
        .expect("engine builds from the committed TM35FIN fixture");

        // Local entries are Loaded at scan → real projected CRS from construction.
        assert_ne!(engine.raster_info().native_crs, "CRS:84");

        // Borrow a real loaded entry to reuse for the transition half.
        let (loaded_ts, loaded_entry) = {
            let cat = engine.catalog.load();
            let (ts, e) = cat.entries.iter().next().expect("fixture has an entry");
            (*ts, e.clone())
        };

        // Cold-start: a stub-only catalog reports the placeholder CRS, no grid,
        // and no fetch — but `times` come from the keys.
        let stub_ts = loaded_ts + chrono::Duration::minutes(5);
        let mut stub_catalog = crate::catalog::Catalog::empty();
        stub_catalog.entries.insert(
            stub_ts,
            crate::catalog::FileEntry::stac_stub(
                std::path::PathBuf::from("stub.tif"),
                0,
                crate::catalog::StacStub {
                    bbox: None,
                    asset_url: "https://example.com/stub.tif".to_string(),
                },
            ),
        );
        engine.catalog.store(Arc::new(stub_catalog));
        engine.refresh_raster_info();
        let stub_info = engine.raster_info();
        assert_eq!(
            stub_info.native_crs, "CRS:84",
            "stub entries report the placeholder CRS without a metadata fetch"
        );
        assert!(stub_info.grid_size.is_none());
        assert_eq!(
            stub_info.times,
            vec![stub_ts],
            "times derive from entry keys even for stubs"
        );

        // Transition: a Loaded entry + refresh restores the real CRS/grid.
        let mut loaded_catalog = crate::catalog::Catalog::empty();
        loaded_catalog.entries.insert(loaded_ts, loaded_entry);
        engine.catalog.store(Arc::new(loaded_catalog));
        engine.refresh_raster_info();
        let reloaded = engine.raster_info();
        assert_ne!(
            reloaded.native_crs, "CRS:84",
            "a Loaded entry yields the real CRS after refresh (do_load_metadata path)"
        );
        assert!(reloaded.grid_size.is_some());
    }

    #[test]
    fn crs_label_tm_only_claims_3067_for_tm35fin() {
        let tm35fin = ds_core::geo::Crs::TransverseMercator {
            lat0: 0.0,
            lon0: 27.0_f64.to_radians(),
            k0: 0.9996,
            false_e: 500_000.0,
            false_n: 0.0,
        };
        assert_eq!(crs_label(&tm35fin), "EPSG:3067");
        // A different TM zone (e.g. UTM 33N central meridian 15°E) is not 3067.
        let utm33n = ds_core::geo::Crs::TransverseMercator {
            lat0: 0.0,
            lon0: 15.0_f64.to_radians(),
            k0: 0.9996,
            false_e: 500_000.0,
            false_n: 0.0,
        };
        assert_eq!(crs_label(&utm33n), "TM");
    }

    #[test]
    fn crs_label_laea_only_claims_3035_for_etrs89() {
        let etrs89 = ds_core::geo::Crs::LambertAzimuthalEqualArea {
            lat0: 52.0_f64.to_radians(),
            lon0: 10.0_f64.to_radians(),
            false_e: 4_321_000.0,
            false_n: 3_210_000.0,
        };
        assert_eq!(crs_label(&etrs89), "EPSG:3035");
        let other = ds_core::geo::Crs::LambertAzimuthalEqualArea {
            lat0: 0.0,
            lon0: 0.0,
            false_e: 0.0,
            false_n: 0.0,
        };
        assert_eq!(crs_label(&other), "LAEA");
    }

    #[test]
    fn crs_label_rotated_has_no_epsg() {
        let rot = ds_core::geo::Crs::RotatedLatLon {
            south_pole_lat: (-30.0_f64).to_radians(),
            south_pole_lon: 0.0,
        };
        assert_eq!(crs_label(&rot), "rotated_ll");
        // And the generic labels have no storageCrs URI.
        assert!(ds_core::geo::native_crs_uri("rotated_ll").is_none());
        assert!(ds_core::geo::native_crs_uri("TM").is_none());
        assert_eq!(
            ds_core::geo::native_crs_uri("CRS:84"),
            Some("http://www.opengis.net/def/crs/OGC/1.3/CRS84")
        );
    }

    #[test]
    fn crs_label_lcc_and_stereographic_match_odim_vocabulary() {
        let lcc = ds_core::geo::Crs::LambertConformalConic {
            lat1: 0.0,
            lat2: 0.0,
            lat0: 0.0,
            lon0: 0.0,
            false_e: 0.0,
            false_n: 0.0,
            radius: None,
        };
        let stere = ds_core::geo::Crs::Stereographic {
            lat0: 0.0,
            lon0: 0.0,
            k0: 1.0,
            false_e: 0.0,
            false_n: 0.0,
        };
        assert_eq!(crs_label(&lcc), "LCC");
        assert_eq!(crs_label(&stere), "stere");
        // Neither resolves to a storageCrs URI (no stable EPSG code).
        assert!(ds_core::geo::native_crs_uri("LCC").is_none());
        assert!(ds_core::geo::native_crs_uri("stere").is_none());
    }

    /// The shared matcher `resolve_filename_config` builds for `template`.
    fn template_matcher(template: &str) -> FilenameMatcher {
        let config = GeoTiffConfig {
            filename_template: Some(template.to_string()),
            ..tm35fin_test_config()
        };
        resolve_filename_config(&config)
            .unwrap()
            .expect("a directory source has a filename matcher")
    }

    fn utc(s: &str) -> Option<DateTime<Utc>> {
        Some(s.parse().unwrap())
    }

    #[test]
    fn template_opera_acrr() {
        let m = template_matcher("OPERA@%Y%m%dT%H%M@0@ACRR.tiff");
        assert_eq!(m.timestamp_format(), "%Y%m%dT%H%M");
        // Verify the matcher actually reads real filenames
        assert_eq!(
            m.parse_timestamp("OPERA@20260324T2040@0@ACRR.tiff"),
            utc("2026-03-24T20:40:00Z")
        );
    }

    #[test]
    fn template_radar_with_trailing_z() {
        let m = template_matcher("radar_%Y%m%dT%H%MZ.tif");
        assert_eq!(m.timestamp_format(), "%Y%m%dT%H%MZ");
        assert_eq!(
            m.parse_timestamp("radar_20260324T2315Z.tif"),
            utc("2026-03-24T23:15:00Z")
        );
    }

    #[test]
    fn template_fmi_leading_timestamp() {
        let m = template_matcher("%Y%m%d%H%M_composite_cappi_600_dbzh_finrad_qc.tif");
        assert_eq!(m.timestamp_format(), "%Y%m%d%H%M");
        assert_eq!(
            m.parse_timestamp("202603251955_composite_cappi_600_dbzh_finrad_qc.tif"),
            utc("2026-03-25T19:55:00Z")
        );
    }

    #[test]
    fn template_with_dashes() {
        let m = template_matcher("data_%Y-%m-%dT%H:%M:%S.tif");
        assert_eq!(m.timestamp_format(), "%Y-%m-%dT%H:%M:%S");
        assert_eq!(
            m.parse_timestamp("data_2026-03-25T19:30:00.tif"),
            utc("2026-03-25T19:30:00Z")
        );
    }

    /// Partial uploads and names that merely contain a match are not the
    /// template (#817).
    #[test]
    fn template_matches_the_whole_name_only() {
        let m = template_matcher("radar_%Y%m%dT%H%MZ.tif");
        assert!(m.parse_timestamp("radar_20260324T2315Z.tif").is_some());
        for partial in [
            "radar_20260324T2315Z.tif.tmp",
            "radar_20260324T2315Z.tif.part",
            "old_radar_20260324T2315Z.tif",
        ] {
            assert_eq!(m.parse_timestamp(partial), None, "{partial}");
        }
    }

    /// A temporary directory removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "meteocore_{tag}_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn radar_fixture() -> PathBuf {
        ["testdata/radar", "../../testdata/radar"]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_dir())
            .expect("testdata/radar fixture")
            .join("radar_20260324T2315Z.tif")
    }

    /// The remote scan end to end, over two day prefixes of a
    /// filesystem-backed store: partial uploads next to finished files are
    /// never catalogued, and each finished file is, with its metadata
    /// (#817). The partial copies are real TIFFs, so only the anchored
    /// template keeps them out.
    #[test]
    fn remote_scan_skips_partial_uploads_across_prefixes() {
        let src = radar_fixture();
        let dir = TempDir::new("remote_partial_upload_test");
        for (day, name) in [
            ("2026/03/24", "radar_20260324T2315Z.tif"),
            ("2026/03/24", "radar_20260324T2320Z.tif.tmp"),
            ("2026/03/25", "radar_20260325T0005Z.tif"),
            ("2026/03/25", "radar_20260325T0010Z.tif.part"),
        ] {
            std::fs::create_dir_all(dir.0.join(day)).unwrap();
            std::fs::copy(&src, dir.0.join(day).join(name)).unwrap();
        }
        let (store, _) = ds_storage::build_store(dir.0.to_str().unwrap()).unwrap();
        let prefixes = ["2026/03/24", "2026/03/25"].map(ds_storage::object_store::path::Path::from);

        let matcher = template_matcher("radar_%Y%m%dT%H%MZ.tif");
        let (catalog, failed) = catalog::scan_remote(
            &store,
            &prefixes,
            &ScanSpec::new(&matcher, "radar"),
            &HashMap::new(),
        )
        .unwrap();
        assert!(failed.is_empty());
        let entries: Vec<_> = catalog
            .entries
            .iter()
            .map(|(time, entry)| (*time, entry.path.clone(), entry.is_loaded()))
            .collect();
        assert_eq!(
            entries,
            [
                (
                    utc("2026-03-24T23:15:00Z").unwrap(),
                    PathBuf::from("2026/03/24/radar_20260324T2315Z.tif"),
                    true
                ),
                (
                    utc("2026-03-25T00:05:00Z").unwrap(),
                    PathBuf::from("2026/03/25/radar_20260325T0005Z.tif"),
                    true
                ),
            ]
        );
    }

    /// An unanchored `filename_pattern` also matches partial uploads, which
    /// the default `exclude_patterns` must drop before the scan picks one
    /// file per timestamp and caps: `….tif.part` sorts after its finished
    /// `….tif` and would win the timestamp, and a newest-only `….tif.tmp`
    /// would take a `max_files` slot (#817 review). All four files are real
    /// TIFFs, so only the exclusion keeps the partial ones out.
    const PARTIAL_LAYOUT: [&str; 4] = [
        "radar_20260324T2310Z.tif",
        "radar_20260324T2315Z.tif",
        "radar_20260324T2315Z.tif.part",
        "radar_20260324T2320Z.tif.tmp",
    ];
    const UNANCHORED_PATTERN: &str = r"radar_(?P<timestamp>\d{8}T\d{4}Z)\.tif";

    fn default_exclude_patterns() -> Vec<String> {
        vec!["*.tmp".to_string(), "*.part".to_string()]
    }

    /// The finished files the partial layout must yield: 23:10 and 23:15.
    fn finished_files(catalog: &Catalog) -> Vec<(DateTime<Utc>, String)> {
        catalog
            .entries
            .iter()
            .map(|(time, entry)| {
                let name = entry.path.file_name().unwrap().to_str().unwrap();
                (*time, name.to_string())
            })
            .collect()
    }

    fn expected_finished_files() -> Vec<(DateTime<Utc>, String)> {
        vec![
            (
                utc("2026-03-24T23:10:00Z").unwrap(),
                "radar_20260324T2310Z.tif".to_string(),
            ),
            (
                utc("2026-03-24T23:15:00Z").unwrap(),
                "radar_20260324T2315Z.tif".to_string(),
            ),
        ]
    }

    #[test]
    fn local_scan_excludes_partial_uploads_before_picking_a_file() {
        let src = radar_fixture();
        let dir = TempDir::new("local_excluded_partial_test");
        for name in PARTIAL_LAYOUT {
            std::fs::copy(&src, dir.0.join(name)).unwrap();
        }
        let config = GeoTiffConfig {
            filename_template: None,
            filename_pattern: Some(UNANCHORED_PATTERN.to_string()),
            timestamp_format: Some("%Y%m%dT%H%MZ".to_string()),
            exclude_patterns: default_exclude_patterns(),
            max_files: Some(2),
            ..tm35fin_test_config()
        };
        let engine = GeoTiffEngine::new("radar", dir.0.to_str(), &config).unwrap();
        assert_eq!(
            finished_files(&engine.catalog.load()),
            expected_finished_files()
        );
    }

    #[test]
    fn remote_scan_excludes_partial_uploads_before_picking_a_file() {
        let src = radar_fixture();
        let dir = TempDir::new("remote_excluded_partial_test");
        std::fs::create_dir_all(dir.0.join("d")).unwrap();
        for name in PARTIAL_LAYOUT {
            std::fs::copy(&src, dir.0.join("d").join(name)).unwrap();
        }
        let (store, _) = ds_storage::build_store(dir.0.to_str().unwrap()).unwrap();
        let matcher = FilenameMatcher::from_pattern(UNANCHORED_PATTERN, "%Y%m%dT%H%MZ").unwrap();
        let exclude = default_exclude_patterns();
        let spec = ScanSpec {
            exclude: &exclude,
            max_files: Some(2),
            ..ScanSpec::new(&matcher, "radar")
        };
        let prefixes = [ds_storage::object_store::path::Path::from("d")];
        let (catalog, failed) =
            catalog::scan_remote(&store, &prefixes, &spec, &HashMap::new()).unwrap();
        assert!(failed.is_empty());
        assert_eq!(finished_files(&catalog), expected_finished_files());
    }

    /// A local scan never catalogues a partial upload next to its finished
    /// file, even with `exclude_patterns` emptied: the template itself is
    /// anchored (#817). The partial copies are real TIFFs, so only the
    /// filename match keeps them out.
    #[test]
    fn local_scan_skips_partial_uploads_without_exclude_patterns() {
        let src = ["testdata/radar", "../../testdata/radar"]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_dir())
            .expect("testdata/radar fixture")
            .join("radar_20260324T2315Z.tif");
        let dir = std::env::temp_dir().join(format!(
            "meteocore_partial_upload_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(dir.clone());
        for name in [
            "radar_20260324T2315Z.tif",
            "radar_20260324T2320Z.tif.tmp",
            "radar_20260324T2325Z.tif.part",
        ] {
            std::fs::copy(&src, dir.join(name)).unwrap();
        }

        let config = GeoTiffConfig {
            filename_template: Some("radar_%Y%m%dT%H%MZ.tif".to_string()),
            exclude_patterns: vec![],
            ..tm35fin_test_config()
        };
        let engine = GeoTiffEngine::new("radar", dir.to_str(), &config).unwrap();
        let times: Vec<_> = engine.catalog.load().entries.keys().copied().collect();
        assert_eq!(times, [utc("2026-03-24T23:15:00Z").unwrap()]);
    }

    /// #1007: `data_age` is now minus the newest timestep, not the time
    /// since the last poll. A stalled feeder leaves its files in place, so
    /// every poll still finds them: that resets `poll_age` but not
    /// `data_age`, which drops only once a newer file is catalogued.
    #[test]
    fn data_age_follows_the_newest_timestep_not_the_last_poll() {
        let src = ["testdata/radar-tm35fin", "../../testdata/radar-tm35fin"]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_dir())
            .expect("testdata/radar-tm35fin fixture")
            .join("radar_tm35_20260406T0640Z.tif");
        let dir = TempDir::new("data_age_test");
        std::fs::copy(&src, dir.0.join("radar_tm35_20260406T0640Z.tif")).unwrap();
        let engine = GeoTiffEngine::new("radar", dir.0.to_str(), &tm35fin_test_config()).unwrap();

        // `data_age` is now minus `newest`, bracketed by clock reads.
        let assert_data_age = |newest: &str| {
            let newest = utc(newest).unwrap();
            let before = Utc::now();
            let age = engine.data_age().expect("a catalogued timestep");
            let after = Utc::now();
            assert!(
                before - newest <= age && age <= after - newest,
                "data age {age} is not now minus {newest}"
            );
        };
        assert_data_age("2026-04-06T06:40:00Z");

        // The feeder stalls: a poll finds the same file. It resets the poll
        // age, set an hour back here, and leaves the data age growing.
        *engine.catalog_updated_at.lock().unwrap() = Some(Utc::now() - chrono::Duration::hours(1));
        engine.poll_once();
        let poll_age = engine.poll_age().unwrap();
        assert!(
            poll_age < chrono::Duration::minutes(1),
            "poll age {poll_age}"
        );
        assert_data_age("2026-04-06T06:40:00Z");

        // A newer file arrives. The scan takes a new file once its size held
        // for two more polls.
        std::fs::copy(&src, dir.0.join("radar_tm35_20260406T0645Z.tif")).unwrap();
        for _ in 0..3 {
            engine.poll_once();
        }
        assert_data_age("2026-04-06T06:45:00Z");
    }

    /// #1007: a poll that finds no files leaves `poll_age` growing, also
    /// when the catalog was empty already, and there is no data age.
    #[test]
    fn poll_age_grows_while_the_source_stays_empty() {
        let dir = TempDir::new("poll_age_empty_test");
        let engine = GeoTiffEngine::new("radar", dir.0.to_str(), &tm35fin_test_config()).unwrap();
        assert!(engine.data_age().is_none());

        *engine.catalog_updated_at.lock().unwrap() = Some(Utc::now() - chrono::Duration::hours(1));
        engine.poll_once();
        let poll_age = engine.poll_age().unwrap();
        assert!(
            poll_age >= chrono::Duration::hours(1),
            "poll age {poll_age}"
        );
        assert!(engine.data_age().is_none());
    }

    #[test]
    fn template_no_codes_rejected() {
        let config = GeoTiffConfig {
            filename_template: Some("radar_data.tif".to_string()),
            ..tm35fin_test_config()
        };
        assert!(resolve_filename_config(&config).is_err());
    }

    #[test]
    fn explicit_filename_pattern_needs_a_timestamp_capture() {
        let explicit = |pattern: &str| GeoTiffConfig {
            filename_template: None,
            filename_pattern: Some(pattern.to_string()),
            timestamp_format: Some("%Y%m%dT%H%MZ".to_string()),
            ..tm35fin_test_config()
        };
        let m = resolve_filename_config(&explicit(r"^radar_(?P<timestamp>\d{8}T\d{4}Z)\.tif$"))
            .unwrap()
            .unwrap();
        assert_eq!(
            m.parse_timestamp("radar_20260324T2315Z.tif"),
            utc("2026-03-24T23:15:00Z")
        );
        assert!(resolve_filename_config(&explicit(r"^radar_(\d{8}T\d{4}Z)\.tif$")).is_err());
    }

    /// The committed five-timestep WGS84 radar fixture: 3249 × 1750 pixels
    /// of ~0.0116°, so the whole raster (5.7 M) is over the 1 M area budget at
    /// one timestep, and an 8° × 4.6° window (~275 k) only across all five.
    fn radar_engine() -> GeoTiffEngine {
        let config = GeoTiffConfig {
            filename_template: Some("radar_%Y%m%dT%H%MZ.tif".to_string()),
            ..tm35fin_test_config()
        };
        GeoTiffEngine::new("radar", Some("../../testdata/radar"), &config)
            .expect("engine builds from the committed radar fixture")
    }

    fn area_values(result: &QueryResult) -> &[Option<f64>] {
        &result.ranges["reflectivity"].values
    }

    /// #858: the area budget counts every matching timestep and is checked
    /// before the `timesteps × ny × nx` result is allocated. Each timestep of
    /// this window is within budget on its own, so the old per-timestep check
    /// in `read_bbox` let the query return ~1.4 M values.
    #[test]
    fn area_budget_counts_every_timestep() {
        let engine = radar_engine();
        let times: Vec<_> = engine.catalog.load().entries.keys().copied().collect();
        assert_eq!(times.len(), 5);
        let (west, south, east, north) = (15.0, 60.0, 23.0, 64.6);

        match engine.query_bbox(west, south, east, north, None, None) {
            Err(DataServerError::QueryTooLarge(msg)) => {
                assert!(msg.contains("5 timesteps"), "{msg}");
                let limit = ds_core::feature::MAX_AREA_VALUES.to_string();
                assert!(msg.contains(&limit), "names the limit: {msg}");
            }
            other => panic!(
                "expected QueryTooLarge, got {:?} values",
                other.map(|r| area_values(&r).len())
            ),
        }

        let one = engine
            .query_bbox(west, south, east, north, Some((times[0], times[0])), None)
            .expect("one timestep of the window is within budget");
        let cells = area_values(&one).len();
        assert!(cells <= ds_core::feature::MAX_AREA_VALUES);
        assert!(cells * times.len() > ds_core::feature::MAX_AREA_VALUES);
    }

    /// #858: a window over the budget at a single timestep used to reach
    /// `read_bbox`'s own check, whose `QueryTooLarge` was logged and served as
    /// a timestep of nulls with HTTP 200.
    #[test]
    fn over_budget_area_is_query_too_large_not_nulls() {
        let engine = radar_engine();
        let t0 = *engine.catalog.load().entries.keys().next().unwrap();
        assert!(matches!(
            engine.query_bbox(0.5, 54.6, 37.9, 74.8, Some((t0, t0)), None),
            Err(DataServerError::QueryTooLarge(_))
        ));
    }

    #[test]
    fn small_area_answers_every_timestep() {
        let engine = radar_engine();
        let result = engine
            .query_bbox(24.5, 60.0, 25.0, 60.5, None, None)
            .unwrap();
        let DomainDescription::Grid { x, y, t, .. } = &result.domain else {
            panic!("expected a Grid domain, got {:?}", result.domain);
        };
        assert_eq!(t.as_ref().map(Vec::len), Some(5));
        let ndarray = &result.ranges["reflectivity"];
        assert_eq!(ndarray.shape, vec![5, y.len(), x.len()]);
        assert_eq!(ndarray.values.len(), 5 * y.len() * x.len());
    }

    /// The null fill stays for a file that cannot be read (#858): its
    /// timestep is nulls and the readable ones are answered.
    #[test]
    fn unreadable_file_is_a_timestep_of_nulls() {
        let engine = radar_engine();
        let (t0, readable) = {
            let catalog = engine.catalog.load();
            let (ts, entry) = catalog.entries.iter().next().unwrap();
            (*ts, entry.clone())
        };
        let metadata = (**readable.metadata().unwrap()).clone();
        let missing = PathBuf::from("../../testdata/radar/missing_20260324T2316Z.tif");
        let t1 = t0 + chrono::Duration::minutes(1);
        let mut catalog = crate::catalog::Catalog::empty();
        catalog.entries.insert(t0, readable);
        catalog.entries.insert(
            t1,
            crate::catalog::FileEntry::loaded(
                missing.clone(),
                reader::DataSource::from_path(&missing),
                metadata,
                0,
                None,
                None,
            ),
        );
        engine.catalog.store(Arc::new(catalog));

        let (west, south, east, north) = (24.5, 60.0, 25.0, 60.5);
        let alone = engine
            .query_bbox(west, south, east, north, Some((t0, t0)), None)
            .unwrap();
        let both = engine
            .query_bbox(west, south, east, north, None, None)
            .expect("an unreadable file does not fail the query");
        let (expected, values) = (area_values(&alone), area_values(&both));
        assert_eq!(values.len(), 2 * expected.len());
        assert_eq!(&values[..expected.len()], expected);
        assert!(values[expected.len()..].iter().all(Option::is_none));
    }

    /// Only a file failure becomes nulls; request errors fail the query.
    #[test]
    fn only_file_failures_are_unreadable() {
        for e in [
            DataServerError::Engine("decode".into()),
            DataServerError::Storage("fetch".into()),
            DataServerError::Io(std::io::Error::other("read")),
        ] {
            assert!(is_unreadable_file(&e), "{e:?}");
        }
        for e in [
            DataServerError::QueryTooLarge("budget".into()),
            DataServerError::InvalidParameter("window".into()),
            DataServerError::InvalidBbox("bbox".into()),
            DataServerError::ResourceExhausted,
            DataServerError::DeadlineExceeded,
        ] {
            assert!(!is_unreadable_file(&e), "{e:?}");
        }
    }
}
