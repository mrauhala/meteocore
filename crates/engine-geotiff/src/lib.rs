mod cache;
mod catalog;
mod parse;
mod reader;
pub mod stac;
mod time_window;

/// Re-exports for fuzz testing. Not part of the public API.
#[cfg(feature = "fuzz")]
#[doc(hidden)]
pub mod fuzz_exports {
    pub use crate::reader::{DataSource, TiffMetadata};
    pub use ds_core::geo::{Crs, GeoTransform};
}

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use regex::Regex;
use std::sync::Arc;
use tokio::sync::watch;

use ds_core::config::GeoTiffConfig;
use ds_core::engine::Engine;
use ds_core::error::DataServerError;
use ds_core::model::*;

use crate::catalog::{scan_directory, scan_remote_with_limit, Catalog, PendingFile};

/// RAII guard that removes a path from the `loading_in_flight` set on drop.
/// Prevents paths from getting stuck if a thread panics during metadata loading.
struct InFlightGuard<'a> {
    set: &'a Mutex<std::collections::HashSet<PathBuf>>,
    path: PathBuf,
}

impl<'a> InFlightGuard<'a> {
    fn new(set: &'a Mutex<std::collections::HashSet<PathBuf>>, path: PathBuf) -> Self {
        Self { set, path }
    }
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.set.lock() {
            guard.remove(&self.path);
        }
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
        time_window: Option<time_window::TimeWindow>,
    },
    /// STAC API catalog: items discovered on-demand via STAC, assets fetched as remote COGs.
    RemoteStac { client: stac::StacClient },
}

pub struct GeoTiffEngine {
    collection_id: String,
    catalog: ArcSwap<Catalog>,
    tile_cache: cache::TileCache,
    store_mode: StoreMode,
    filename_pattern: Regex,
    timestamp_format: String,
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
    /// Shutdown signal for the polling loop.
    shutdown_tx: watch::Sender<()>,
    /// Consecutive poll failures/empty results (for escalating warnings).
    consecutive_poll_failures: AtomicU32,
    /// Tracks STAC entries currently being loaded to prevent concurrent loads.
    loading_in_flight: Mutex<std::collections::HashSet<PathBuf>>,
    /// Circuit breaker: consecutive STAC API failures.
    stac_consecutive_failures: AtomicU32,
    /// Circuit breaker: last STAC API attempt time.
    stac_last_attempt: Mutex<Option<std::time::Instant>>,
    /// Tracks when the catalog was last successfully updated.
    catalog_updated_at: Mutex<Option<DateTime<Utc>>>,
}

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
                    return Err(DataServerError::GeoTiff(format!(
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

    /// How long ago the catalog was last successfully updated.
    /// Returns `None` if the catalog has never been updated after initial load.
    pub fn catalog_age(&self) -> Option<chrono::Duration> {
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

        // Derive filename_pattern and timestamp_format from template or explicit fields
        let (pattern_str, timestamp_format) = resolve_filename_config(config)?;

        let filename_pattern = Regex::new(&pattern_str).map_err(|e| {
            DataServerError::GeoTiff(format!("Invalid filename pattern '{}': {e}", pattern_str))
        })?;

        // Determine store mode from config
        // Parse time_window if configured
        let parsed_time_window = config
            .time_window
            .as_deref()
            .map(time_window::TimeWindow::parse)
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
            // When time_window is set, scan_dates() computes exact dates at scan time.
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
            let is_remote = data_path.starts_with("s3://")
                || data_path.starts_with("http://")
                || data_path.starts_with("https://");

            if is_remote {
                let (store, prefix) = ds_storage::build_store(data_path)?;
                (StoreMode::Remote { store, prefix }, data_path.to_string())
            } else {
                let directory = PathBuf::from(data_path);
                if !directory.is_dir() {
                    return Err(DataServerError::GeoTiff(format!(
                        "{data_path} is not a directory"
                    )));
                }
                let directory = directory.canonicalize().map_err(|e| {
                    DataServerError::GeoTiff(format!("Cannot resolve directory {data_path}: {e}"))
                })?;
                (
                    StoreMode::Local {
                        directory,
                        pending: Mutex::new(BTreeMap::new()),
                    },
                    data_path.to_string(),
                )
            }
        } else {
            return Err(DataServerError::GeoTiff(
                "Either data_path, endpoint+bucket, or stac_url must be configured".into(),
            ));
        };

        let cache_bytes = config.tile_cache_mb * 1024 * 1024;
        let tile_cache = cache::TileCache::new(cache_bytes);

        let band_index = (config.band.max(1) - 1) as usize; // 1-based config → 0-based index

        let (shutdown_tx, _) = watch::channel(());

        let engine = GeoTiffEngine {
            collection_id: collection_id.to_string(),
            catalog: ArcSwap::from_pointee(Catalog::empty()),
            tile_cache,
            store_mode,
            filename_pattern,
            timestamp_format,
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
            shutdown_tx,
            consecutive_poll_failures: AtomicU32::new(0),
            loading_in_flight: Mutex::new(std::collections::HashSet::new()),
            stac_consecutive_failures: AtomicU32::new(0),
            stac_last_attempt: Mutex::new(None),
            catalog_updated_at: Mutex::new(None),
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

        Ok(engine)
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
                    &self.filename_pattern,
                    &self.timestamp_format,
                    &self.exclude_patterns,
                    &mut pending,
                    &path_index,
                )?
            }
            StoreMode::Remote { store, prefix } => scan_remote_with_limit(
                store,
                prefix,
                &self.filename_pattern,
                &self.timestamp_format,
                &path_index,
                self.max_files,
                None,
                &self.collection_id,
            )?,
            StoreMode::RemoteDynamic {
                store,
                prefix_pattern,
                scan_days,
                time_window,
            } => {
                let now = Utc::now();
                let (prefixes, time_filter) = if let Some(tw) = time_window {
                    let dates = tw.scan_dates(now);
                    (
                        expand_prefix_for_dates(prefix_pattern, &dates),
                        Some(tw.to_range(now)),
                    )
                } else {
                    (expand_prefix_pattern(prefix_pattern, *scan_days), None)
                };
                let mut merged = Catalog::empty();
                let mut scan_errors: Vec<(String, DataServerError)> = Vec::new();
                for prefix_str in &prefixes {
                    let prefix = ds_storage::object_store::path::Path::from(prefix_str.as_str());
                    match scan_remote_with_limit(
                        store,
                        &prefix,
                        &self.filename_pattern,
                        &self.timestamp_format,
                        &path_index,
                        None, // no per-prefix limit; apply max_files after merge
                        time_filter,
                        &self.collection_id,
                    ) {
                        Ok(partial) => {
                            merged.entries.extend(partial.entries);
                        }
                        Err(e) => {
                            scan_errors.push((prefix_str.clone(), e));
                        }
                    }
                }
                if !scan_errors.is_empty() {
                    tracing::warn!(
                        "[{}] {}/{} prefix scan(s) failed: {}",
                        self.collection_id,
                        scan_errors.len(),
                        prefixes.len(),
                        scan_errors
                            .iter()
                            .map(|(p, e)| format!("'{}': {}", p, e))
                            .collect::<Vec<_>>()
                            .join("; ")
                    );
                }
                merged.recompute_extents();
                merged
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

    /// Load GeoTIFF metadata for a STAC stub entry.
    ///
    /// For STAC mode, uses the StacClient's reqwest-based HTTP methods directly
    /// (bypassing object_store which URL-encodes path components and breaks
    /// servers like Ceph RGW that use colons in paths).
    ///
    /// Tries COG range read first (64KB header), falls back to full download.
    fn load_stac_entry_metadata(
        &self,
        stub: &catalog::StacStub,
        file_size: u64,
    ) -> Result<(reader::TiffMetadata, reader::DataSource), DataServerError> {
        let stac_client = match &self.store_mode {
            StoreMode::RemoteStac { client } => client,
            _ => {
                return Err(DataServerError::GeoTiff(
                    "load_stac_entry_metadata called on non-STAC engine".into(),
                ))
            }
        };

        // Get actual file size if not known from STAC
        let actual_size = if file_size == 0 {
            stac_client.head_asset(&stub.asset_url).unwrap_or(0)
        } else {
            file_size
        };

        if actual_size > catalog::MAX_REMOTE_FILE_SIZE as u64 {
            return Err(DataServerError::GeoTiff(format!(
                "File too large ({} > {} max)",
                format_bytes(actual_size),
                format_bytes(catalog::MAX_REMOTE_FILE_SIZE as u64)
            )));
        }

        // Try COG range read first (only 512KB header) — creates HttpDirect source
        // that fetches tiles on demand via byte-range reads.
        let http = stac_client.http_client();
        if actual_size > 0 {
            if let Some((metadata, tile_info)) =
                reader::TiffMetadata::from_http_header_read(&http, &stub.asset_url, actual_size)
            {
                tracing::debug!(
                    "[{}] STAC COG range read '{}' (header only, {} tiles)",
                    self.collection_id,
                    stub.asset_url,
                    tile_info.tile_offsets.len()
                );
                let source = reader::DataSource::HttpDirect {
                    url: stub.asset_url.clone(),
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
            stub.asset_url,
            format_bytes(actual_size)
        );

        let data = stac_client.get_asset(&stub.asset_url)?;
        let source = reader::DataSource::from_bytes(data);
        let metadata = reader::TiffMetadata::from_source(&source)?;

        Ok((metadata, source))
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

    /// Wait for another thread to finish loading metadata for the given path.
    /// Returns `true` if another thread successfully loaded it.
    fn wait_for_concurrent_load(&self, timestamp: &DateTime<Utc>, path: &Path) -> bool {
        for attempt in 0u64..20 {
            std::thread::sleep(Duration::from_millis(100 * (attempt + 1).min(5)));
            let catalog = self.catalog.load();
            if let Some(entry) = catalog.entries.get(timestamp) {
                if entry.is_loaded() {
                    return true;
                }
            }
            // Check if still in-flight (other thread may have errored out)
            let in_flight = self
                .loading_in_flight
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !in_flight.contains(path) {
                // Other thread finished (possibly with error), try loading ourselves
                return false;
            }
        }
        false
    }

    /// Load metadata for a stub entry and update the catalog.
    fn do_load_metadata(
        &self,
        timestamp: &DateTime<Utc>,
        asset_url: &str,
        file_size: u64,
        path: &Path,
    ) -> Result<(), DataServerError> {
        // RAII guard ensures path is removed from in-flight set even on panic
        let _guard = InFlightGuard::new(&self.loading_in_flight, path.to_path_buf());

        let stub = catalog::StacStub {
            bbox: None, // Not needed for loading
            asset_url: asset_url.to_string(),
        };
        let (mut metadata, source) = self.load_stac_entry_metadata(&stub, file_size)?;

        // Apply config overrides
        metadata.apply_overrides(
            self.override_nodata,
            self.override_scale,
            self.override_offset,
        );

        // Clone the catalog, update the entry, and swap
        let current = self.catalog.load();
        let mut new_catalog = (**current).clone();
        if let Some(entry) = new_catalog.entries.get_mut(timestamp) {
            entry.set_loaded(Arc::new(metadata), Arc::new(source));
        }
        new_catalog.recompute_extents();
        self.catalog.store(Arc::new(new_catalog));

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

        // Check/acquire in-flight lock
        {
            let in_flight = self
                .loading_in_flight
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if in_flight.contains(&path) {
                drop(in_flight);
                if self.wait_for_concurrent_load(timestamp, &path) {
                    return Ok(());
                }
                // Fall through to try loading ourselves
            }
        }
        {
            let mut in_flight = self
                .loading_in_flight
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            in_flight.insert(path.clone());
        }

        self.do_load_metadata(timestamp, &asset_url, file_size, &path)
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
                            self.catalog.store(Arc::new(updated));
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
                            self.catalog.store(Arc::new(updated));
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
    pub async fn poll_loop(&self) {
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        let mut interval = tokio::time::interval(self.poll_interval);
        interval.tick().await;

        loop {
            tokio::select! {
                _ = interval.tick() => self.poll_once(),
                _ = shutdown_rx.changed() => {
                    tracing::info!("[{}] Poll loop shutting down", self.collection_id);
                    break;
                }
            }
        }
    }

    /// Signal the polling loop to stop.
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(());
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
                // Track successful catalog update time
                *self
                    .catalog_updated_at
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(Utc::now());
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
        if let Some(params) = parameters {
            if !params.iter().any(|p| p == &self.parameter) {
                return Err(DataServerError::InvalidParameter(format!(
                    "Unknown parameter. Available: {}",
                    self.parameter
                )));
            }
        }

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
        };

        let mut param_descs = HashMap::new();
        param_descs.insert(
            self.parameter.clone(),
            ParameterDescription {
                label: self.parameter.replace('_', " "),
                unit: self.unit.clone(),
                observed_property: self.parameter.clone(),
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
        if let Some(params) = parameters {
            if !params.iter().any(|p| p == &self.parameter) {
                return Err(DataServerError::InvalidParameter(format!(
                    "Unknown parameter. Available: {}",
                    self.parameter
                )));
            }
        }

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
            .ok_or_else(|| DataServerError::GeoTiff("First entry has no metadata loaded".into()))?;
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

        // Build x and y axis values (pixel centers)
        let x_values: Vec<f64> = (col_start..col_end)
            .map(|c| first_metadata.geo_transform.pixel_to_world(c, 0).0)
            .collect();
        let y_values: Vec<f64> = (row_start..row_end)
            .map(|r| first_metadata.geo_transform.pixel_to_world(0, r).1)
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
                Err(e) => {
                    tracing::warn!("Failed to read bbox from {}: {e}", entry.path.display());
                    // Fill with None for this timestep
                    all_values.extend(std::iter::repeat_n(None, nx * ny));
                }
            }
        }

        let domain = if has_time {
            DomainDescription::Grid {
                x: x_values.clone(),
                y: y_values.clone(),
                t: Some(times),
            }
        } else {
            DomainDescription::Grid {
                x: x_values.clone(),
                y: y_values.clone(),
                t: None,
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

impl ds_core::map_engine::MapEngine for GeoTiffEngine {
    fn get_raster_tile(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        output_crs: &ds_core::map_engine::OutputCrs,
    ) -> Result<ds_core::map_engine::RasterTile, DataServerError> {
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
                        self.catalog.store(Arc::new(updated));
                    }
                    Err(e) => {
                        self.record_stac_failure();
                        return Err(e);
                    }
                }
            }
        }

        // Find the target timestamp first, then ensure it's loaded
        let target_timestamp = {
            let catalog = self.catalog.load();
            let entry = if let Some(t) = time {
                catalog
                    .entries
                    .range(..=t)
                    .next_back()
                    .or_else(|| catalog.entries.iter().next())
            } else {
                catalog.entries.iter().next_back()
            };
            let (ts, _) = entry.ok_or_else(|| {
                DataServerError::GeoTiff("No data available for the requested time".into())
            })?;
            *ts
        };

        // Lazily load STAC stub if needed
        self.ensure_metadata(&target_timestamp)?;

        let catalog = self.catalog.load();
        let (_timestamp, entry) = catalog
            .entries
            .get_key_value(&target_timestamp)
            .ok_or_else(|| DataServerError::GeoTiff("Entry disappeared after loading".into()))?;

        let metadata = entry
            .metadata()
            .ok_or_else(|| DataServerError::GeoTiff("Entry metadata not loaded".into()))?;
        let source = entry
            .source()
            .ok_or_else(|| DataServerError::GeoTiff("Entry source not loaded".into()))?;

        let [west, south, east, north] = bbox;
        let total_pixels = (width as usize) * (height as usize);
        let mut values = Vec::with_capacity(total_pixels);

        // Select the best overview level for the output resolution.
        // This avoids reading millions of full-resolution pixels for zoomed-out views.
        // If no overview matches but full res is too large, force the smallest overview.
        let overview = metadata
            .select_overview(west, south, east, north, width, height)
            .or_else(|| {
                // Check if full resolution would exceed the map pixel limit
                if let Some((c0, r0, c1, r1)) = metadata
                    .geo_transform
                    .bbox_to_pixels(west, south, east, north)
                {
                    let full_pixels = ((c1 - c0) as usize) * ((r1 - r0) as usize);
                    if full_pixels > 16_000_000 {
                        // Force smallest overview to avoid exceeding limits
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

        if let Some((col_start, row_start, col_end, row_end)) = source_range {
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

            // Read from overview or full resolution
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

            // Pre-compute Mercator Y bounds if needed
            let (merc_y_north, merc_y_south) =
                if *output_crs == ds_core::map_engine::OutputCrs::WebMercator {
                    (lat_to_merc_y(north), lat_to_merc_y(south))
                } else {
                    (0.0, 0.0) // unused
                };

            // Resample source grid to output dimensions using nearest-neighbor
            for oy in 0..height {
                for ox in 0..width {
                    let frac_x = (ox as f64 + 0.5) / width as f64;
                    let frac_y = (oy as f64 + 0.5) / height as f64;
                    let lon = west + frac_x * (east - west);
                    let lat = if *output_crs == ds_core::map_engine::OutputCrs::WebMercator {
                        // In Mercator, pixels have equal spacing in Y meters.
                        // Interpolate in Mercator Y, then convert back to lat.
                        let merc_y = merc_y_north - frac_y * (merc_y_north - merc_y_south);
                        merc_y_to_lat(merc_y)
                    } else {
                        // Linear interpolation in latitude
                        north - frac_y * (north - south)
                    };

                    match gt.world_to_pixel(lon, lat) {
                        Some((col, row))
                            if col >= col_start
                                && col < col_end
                                && row >= row_start
                                && row < row_end =>
                        {
                            let sc = (col - col_start) as usize;
                            let sr = (row - row_start) as usize;
                            let idx = sr * src_nx + sc;
                            values.push(pixels.get(idx).copied().unwrap_or(None));
                        }
                        _ => values.push(None),
                    }
                }
            }
        } else {
            // Bbox doesn't intersect raster at all — all nodata
            values.resize(total_pixels, None);
        }

        Ok(ds_core::map_engine::RasterTile {
            width,
            height,
            values,
        })
    }

    fn raster_info(&self) -> ds_core::map_engine::RasterInfo {
        // Try to load at least one entry's metadata for CRS detection
        {
            let catalog = self.catalog.load();
            let has_loaded = catalog.entries.values().any(|e| e.is_loaded());
            if !has_loaded {
                // No loaded entries — try each until one succeeds
                let timestamps: Vec<DateTime<Utc>> = catalog.entries.keys().copied().collect();
                drop(catalog);
                for ts in &timestamps {
                    match self.ensure_metadata(ts) {
                        Ok(()) => break,
                        Err(e) => {
                            tracing::warn!(
                                "[{}] Failed to load metadata for CRS detection: {}",
                                self.collection_id,
                                e
                            );
                        }
                    }
                }
            }
        }

        let catalog = self.catalog.load();
        let crs_name = catalog
            .entries
            .values()
            .find_map(|entry| {
                entry.metadata().map(|m| match &m.geo_transform.crs {
                    ds_core::geo::Crs::Wgs84 => "EPSG:4326".to_string(),
                    ds_core::geo::Crs::TransverseMercator { .. } => "EPSG:3067".to_string(),
                    ds_core::geo::Crs::LambertAzimuthalEqualArea { .. } => "EPSG:3035".to_string(),
                    ds_core::geo::Crs::LambertConformalConic { .. } => "projected".to_string(),
                    ds_core::geo::Crs::Stereographic { .. } => "projected".to_string(),
                    ds_core::geo::Crs::RotatedLatLon { .. } => "EPSG:4326".to_string(),
                })
            })
            .unwrap_or_else(|| "EPSG:4326".to_string());

        let times: Vec<DateTime<Utc>> = catalog.entries.keys().cloned().collect();

        ds_core::map_engine::RasterInfo {
            native_crs: crs_name,
            spatial_extent: catalog.spatial_extent,
            times,
            parameter: self.parameter.clone(),
            unit: self.unit.clone(),
        }
    }
}

impl Engine for GeoTiffEngine {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        Ok(vec![])
    }

    fn query_location(
        &self,
        _location_id: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
    ) -> Result<QueryResult, DataServerError> {
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
    ) -> Result<QueryResult, DataServerError> {
        let (lat, lon) = parse_coords(coords)?;
        self.query_point(lat, lon, datetime, parameters)
    }

    fn query_area(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
    ) -> Result<AreaQueryResult, DataServerError> {
        let polygon = ds_core::feature::parse_area_coords(coords)?;
        let mut result = self.query_bbox(
            polygon.bbox.west,
            polygon.bbox.south,
            polygon.bbox.east,
            polygon.bbox.north,
            datetime,
            parameters,
        )?;

        // Mask pixels outside the polygon
        if let DomainDescription::Grid {
            ref x,
            ref y,
            ref t,
        } = result.domain
        {
            let nt = t.as_ref().map_or(1, |tv| tv.len());
            let ny = y.len();
            let nx = x.len();
            let expected_len = nt * ny * nx;
            for (_name, ndarray) in result.ranges.iter_mut() {
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
                        if !polygon.contains(*x_val, *y_val) {
                            for it in 0..nt {
                                let idx = it * ny * nx + iy * nx + ix;
                                ndarray.values[idx] = None;
                            }
                        }
                    }
                }
            }
        }

        Ok(AreaQueryResult::Single(result))
    }

    fn supported_query_types(&self) -> Vec<String> {
        vec!["position".to_string(), "area".to_string()]
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
/// Convert latitude (degrees) to Web Mercator Y (meters).
fn lat_to_merc_y(lat_deg: f64) -> f64 {
    const R: f64 = 6_378_137.0;
    let lat_rad = lat_deg.to_radians();
    R * ((std::f64::consts::FRAC_PI_4 + lat_rad / 2.0).tan()).ln()
}

/// Convert Web Mercator Y (meters) to latitude (degrees).
fn merc_y_to_lat(y: f64) -> f64 {
    const R: f64 = 6_378_137.0;
    (std::f64::consts::FRAC_PI_2 - 2.0 * (-y / R).exp().atan()).to_degrees()
}

fn validate_config(
    collection_id: &str,
    data_path: Option<&str>,
    config: &GeoTiffConfig,
) -> Result<(), DataServerError> {
    // endpoint and bucket must both be set or both absent
    match (&config.endpoint, &config.bucket) {
        (Some(_), None) => {
            return Err(DataServerError::GeoTiff(format!(
                "[{collection_id}] 'endpoint' is set but 'bucket' is missing — both are required for S3 access"
            )));
        }
        (None, Some(_)) => {
            return Err(DataServerError::GeoTiff(format!(
                "[{collection_id}] 'bucket' is set but 'endpoint' is missing — both are required for S3 access"
            )));
        }
        _ => {}
    }

    // stac_url is mutually exclusive with data_path and endpoint+bucket
    if config.stac_url.is_some() {
        if data_path.is_some() {
            return Err(DataServerError::GeoTiff(format!(
                "[{collection_id}] 'stac_url' and 'data_path' are mutually exclusive"
            )));
        }
        if config.endpoint.is_some() {
            return Err(DataServerError::GeoTiff(format!(
                "[{collection_id}] 'stac_url' and 'endpoint+bucket' are mutually exclusive"
            )));
        }
        // stac_asset_allowlist is required for SSRF protection
        match &config.stac_asset_allowlist {
            None => {
                return Err(DataServerError::GeoTiff(format!(
                    "[{collection_id}] 'stac_asset_allowlist' is required when 'stac_url' is set (SSRF protection)"
                )));
            }
            Some(list) if list.is_empty() => {
                return Err(DataServerError::GeoTiff(format!(
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

    if config.poll_interval_secs == 0 {
        return Err(DataServerError::GeoTiff(format!(
            "[{collection_id}] poll_interval_secs must be > 0"
        )));
    }

    if config.band == 0 {
        return Err(DataServerError::GeoTiff(format!(
            "[{collection_id}] band must be >= 1 (1-based index)"
        )));
    }

    Ok(())
}

/// Resolve filename_template or filename_pattern + timestamp_format from config.
/// Returns (regex_pattern, timestamp_format).
///
/// In STAC mode, filename patterns are not used (timestamps come from STAC properties),
/// so dummy values are returned.
fn resolve_filename_config(config: &GeoTiffConfig) -> Result<(String, String), DataServerError> {
    // STAC mode: timestamps come from STAC item properties, not filenames
    if config.stac_url.is_some() {
        return Ok(("unused".to_string(), "unused".to_string()));
    }

    if let Some(template) = &config.filename_template {
        let (regex, format) = expand_filename_template(template)?;
        tracing::debug!(
            "Expanded filename_template '{}' → regex='{}', format='{}'",
            template,
            regex,
            format
        );
        Ok((regex, format))
    } else if let (Some(pattern), Some(format)) =
        (&config.filename_pattern, &config.timestamp_format)
    {
        if !pattern.contains("(?P<timestamp>") {
            return Err(DataServerError::GeoTiff(
                "filename_pattern must contain a named capture group (?P<timestamp>...)".into(),
            ));
        }
        Ok((pattern.clone(), format.clone()))
    } else {
        Err(DataServerError::GeoTiff(
            "Either filename_template or both filename_pattern + timestamp_format must be set"
                .into(),
        ))
    }
}

/// Expand a filename template with strftime placeholders into a regex + timestamp format.
///
/// E.g. `"OPERA@%Y%m%dT%H%M@0@ACRR.tiff"` produces:
/// - regex: `OPERA@(?P<timestamp>\d{8}T\d{4})@0@ACRR\.tiff`
/// - format: `%Y%m%dT%H%M`
fn expand_filename_template(template: &str) -> Result<(String, String), DataServerError> {
    // Known strftime codes and their regex equivalents
    let codes: &[(&str, &str)] = &[
        ("%Y", r"\d{4}"),
        ("%m", r"\d{2}"),
        ("%d", r"\d{2}"),
        ("%H", r"\d{2}"),
        ("%M", r"\d{2}"),
        ("%S", r"\d{2}"),
        ("%j", r"\d{3}"),
    ];

    // Find the contiguous region of strftime codes in the template
    // (the timestamp part) and build regex + format from it
    let mut i = 0;
    let bytes = template.as_bytes();
    let mut regex = String::new();
    let mut timestamp_format = String::new();
    let mut in_timestamp = false;
    let mut timestamp_started = false;

    while i < bytes.len() {
        if bytes[i] == b'%' && i + 1 < bytes.len() {
            // Check if this is a known strftime code
            let mut matched = false;
            for &(code, _regex_part) in codes {
                if template[i..].starts_with(code) {
                    if !in_timestamp {
                        in_timestamp = true;
                        regex.push_str("(?P<timestamp>");
                    }
                    timestamp_started = true;
                    timestamp_format.push_str(code);
                    regex.push_str(_regex_part);
                    i += code.len();
                    matched = true;
                    break;
                }
            }
            if !matched {
                return Err(DataServerError::GeoTiff(format!(
                    "Unknown strftime code '{}' in filename_template",
                    &template[i..i + 2]
                )));
            }
        } else {
            // Literal character
            if in_timestamp {
                // Check if this is a separator within the timestamp (e.g., T, -, :)
                // or the end of the timestamp region
                let ch = bytes[i] as char;
                let is_separator = matches!(ch, 'T' | '-' | ':' | '_' | 'Z');
                // Peek ahead: is there another % code coming?
                let next_has_code = (i + 1 < bytes.len()) && {
                    let rest = &template[i + 1..];
                    codes.iter().any(|&(code, _)| rest.starts_with(code))
                };

                if is_separator && next_has_code {
                    // Separator within timestamp (e.g., the T in %Y%m%dT%H%M)
                    timestamp_format.push(ch);
                    regex.push_str(&regex::escape(&ch.to_string()));
                    i += 1;
                } else if ch == 'Z' && !next_has_code {
                    // Trailing Z (UTC marker) is part of the timestamp
                    timestamp_format.push('Z');
                    regex.push('Z');
                    i += 1;
                    // Close timestamp group
                    regex.push(')');
                    in_timestamp = false;
                } else {
                    // End of timestamp region
                    regex.push(')');
                    in_timestamp = false;
                    regex.push_str(&regex::escape(&(ch).to_string()));
                    i += 1;
                }
            } else {
                // Not in timestamp — escape for regex
                regex.push_str(&regex::escape(&(bytes[i] as char).to_string()));
                i += 1;
            }
        }
    }

    // Close timestamp group if template ends with strftime codes
    if in_timestamp {
        regex.push(')');
    }

    if !timestamp_started {
        return Err(DataServerError::GeoTiff(format!(
            "filename_template '{}' contains no strftime codes (%%Y, %%m, etc.)",
            template
        )));
    }

    Ok((regex, timestamp_format))
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

/// Expand a prefix pattern for specific dates.
///
/// E.g. `"%Y/%m/%d/OPERA/COMP/"` with dates [2026-03-25, 2026-03-24] produces:
/// `["2026/03/25/OPERA/COMP", "2026/03/24/OPERA/COMP"]`
///
/// If the pattern contains no `%` characters, returns it as-is (single prefix).
fn expand_prefix_for_dates(pattern: &str, dates: &[chrono::NaiveDate]) -> Vec<String> {
    if !pattern.contains('%') {
        return vec![pattern.trim_end_matches('/').to_string()];
    }

    dates
        .iter()
        .map(|date| {
            date.format(pattern)
                .to_string()
                .trim_end_matches('/')
                .to_string()
        })
        .collect()
}

/// Expand a prefix pattern for the last `scan_days` days (fallback when no time_window).
fn expand_prefix_pattern(pattern: &str, scan_days: u32) -> Vec<String> {
    let today = Utc::now().date_naive();
    let dates: Vec<_> = (0..scan_days.max(1))
        .map(|offset| today - chrono::Duration::days(offset as i64))
        .collect();
    expand_prefix_for_dates(pattern, &dates)
}

use parse::parse_coords;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_prefix_static() {
        let result = expand_prefix_pattern("some/fixed/prefix", 2);
        assert_eq!(result, vec!["some/fixed/prefix"]);
    }

    #[test]
    fn expand_prefix_with_date() {
        let result = expand_prefix_pattern("%Y/%m/%d/OPERA/COMP/", 2);
        assert_eq!(result.len(), 2);
        // Both should be date-formatted paths
        for p in &result {
            assert!(p.ends_with("/OPERA/COMP"), "unexpected prefix: {}", p);
            assert_eq!(p.len(), "2026/03/25/OPERA/COMP".len());
        }
        // First should be today, second yesterday
        assert_ne!(result[0], result[1]);
    }

    #[test]
    fn expand_prefix_single_day() {
        let result = expand_prefix_pattern("%Y/%m/%d/data/", 1);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn expand_prefix_zero_days_defaults_to_one() {
        let result = expand_prefix_pattern("%Y/%m/%d/data/", 0);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn template_opera_acrr() {
        let (regex, fmt) = expand_filename_template("OPERA@%Y%m%dT%H%M@0@ACRR.tiff").unwrap();
        assert_eq!(fmt, "%Y%m%dT%H%M");
        // Verify the regex actually matches real filenames
        let re = Regex::new(&regex).unwrap();
        let caps = re.captures("OPERA@20260324T2040@0@ACRR.tiff").unwrap();
        assert_eq!(caps.name("timestamp").unwrap().as_str(), "20260324T2040");
    }

    #[test]
    fn template_radar_with_trailing_z() {
        let (regex, fmt) = expand_filename_template("radar_%Y%m%dT%H%MZ.tif").unwrap();
        assert_eq!(fmt, "%Y%m%dT%H%MZ");
        let re = Regex::new(&regex).unwrap();
        let caps = re.captures("radar_20260324T2315Z.tif").unwrap();
        assert_eq!(caps.name("timestamp").unwrap().as_str(), "20260324T2315Z");
    }

    #[test]
    fn template_fmi_leading_timestamp() {
        let (regex, fmt) =
            expand_filename_template("%Y%m%d%H%M_composite_cappi_600_dbzh_finrad_qc.tif").unwrap();
        assert_eq!(fmt, "%Y%m%d%H%M");
        let re = Regex::new(&regex).unwrap();
        let caps = re
            .captures("202603251955_composite_cappi_600_dbzh_finrad_qc.tif")
            .unwrap();
        assert_eq!(caps.name("timestamp").unwrap().as_str(), "202603251955");
    }

    #[test]
    fn template_with_dashes() {
        let (regex, fmt) = expand_filename_template("data_%Y-%m-%dT%H:%M:%S.tif").unwrap();
        assert_eq!(fmt, "%Y-%m-%dT%H:%M:%S");
        let re = Regex::new(&regex).unwrap();
        let caps = re.captures("data_2026-03-25T19:30:00.tif").unwrap();
        assert_eq!(
            caps.name("timestamp").unwrap().as_str(),
            "2026-03-25T19:30:00"
        );
    }

    #[test]
    fn template_no_codes_rejected() {
        assert!(expand_filename_template("radar_data.tif").is_err());
    }
}
