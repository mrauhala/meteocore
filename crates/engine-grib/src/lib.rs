pub mod cache;
pub mod catalog;
pub mod index;
pub mod params;
pub mod reader;
mod time_window;

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use tokio::sync::watch;

use ds_core::config::GribConfig;
use ds_core::engine::Engine;
use ds_core::error::DataServerError;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterInfo, RasterTile};
use ds_core::model::*;

use crate::cache::GridCache;
use crate::catalog::{Catalog, ForecastRun, StepFile};
use crate::time_window::TimeWindow;

/// Default model run hours for ECMWF IFS (4 runs per day).
const DEFAULT_RUN_HOURS: &[u32] = &[0, 6, 12, 18];

/// Number of days to scan back (today + yesterday handles overnight transitions).
const SCAN_DAYS: u32 = 2;

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

        // Generate all prefixes to scan: SCAN_DAYS dates × run_hours
        let prefixes = build_scan_prefixes(&self.prefix_pattern, now, run_hours);

        // Collect all index files across all prefixes
        let mut all_index_paths: Vec<ds_storage::object_store::path::Path> = Vec::new();
        for prefix in &prefixes {
            let obj_prefix = ds_storage::object_store::path::Path::from(prefix.as_str());
            match self.store.list(&obj_prefix) {
                Ok(objects) => {
                    for obj in objects {
                        if obj.location.as_ref().ends_with(index_suffix) {
                            all_index_paths.push(obj.location);
                        }
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
        }

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
            "Collection '{}': found {} new index files ({} total across {} prefixes)",
            self.collection_id,
            new_paths.len(),
            all_index_paths.len(),
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

            let Some(parsed) = index::parse_index(content) else {
                continue;
            };

            let Some(ref_time) = catalog::parse_reference_time(&parsed.date, &parsed.time) else {
                tracing::warn!(
                    "Collection '{}': cannot parse ref time from index {}",
                    self.collection_id,
                    path
                );
                continue;
            };

            // Derive GRIB file URL from index file path
            let grib_url = path.as_ref().replace(index_suffix, data_suffix);

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
        Ok(())
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

        // Check cache
        if let Some(cache) = &self.grid_cache {
            if let Some(grid) = cache.get(&step_file.grib_url, entry.offset) {
                return Ok(grid);
            }
        }

        // Fetch via byte-range
        let path = ds_storage::object_store::path::Path::from(step_file.grib_url.as_str());
        let grid = reader::read_message(&self.store, &path, entry)?;
        let grid = Arc::new(grid);

        // Cache it
        if let Some(cache) = &self.grid_cache {
            cache.insert(&step_file.grib_url, entry.offset, grid.clone());
        }

        Ok(grid)
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

impl Engine for GribEngine {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        // Gridded data has no discrete locations
        Ok(Vec::new())
    }

    fn query_location(
        &self,
        _location_id: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
    ) -> Result<QueryResult, DataServerError> {
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
            let info = params::ecmwf_param_info(&p);
            map.insert(
                p.clone(),
                ParameterDescription {
                    label: info.label.to_string(),
                    unit: info.unit.to_string(),
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
    ) -> Result<QueryResult, DataServerError> {
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
                // Default to surface parameters only
                let mut seen = std::collections::HashSet::new();
                first_step
                    .messages
                    .iter()
                    .filter(|m| m.levtype == "sfc")
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
            let info = params::ecmwf_param_info(param_name);

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
                        values.push(raw.map(|v| info.convert(v)));
                    }
                    Err(e) => {
                        tracing::warn!("Failed to fetch {param_name} for step: {e}");
                        values.push(None);
                    }
                }
            }

            param_descs.insert(
                param_name.to_string(),
                ParameterDescription {
                    label: info.label.to_string(),
                    unit: info.unit.to_string(),
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

        Ok(QueryResult {
            domain: DomainDescription::PointSeries {
                x: lon,
                y: lat,
                t: valid_times,
            },
            parameters: param_descs,
            ranges,
        })
    }

    fn query_area(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
    ) -> Result<AreaQueryResult, DataServerError> {
        let bbox = parse_bbox_from_wkt(coords)?;
        let (_step, step_file) = self.resolve_time(datetime)?;

        // Default to first surface parameter
        let query_params: Vec<&str> = match parameters {
            Some(p) => p.iter().map(|s| s.as_str()).collect(),
            None => {
                let surface: Vec<_> = step_file
                    .messages
                    .iter()
                    .filter(|m| m.levtype == "sfc")
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
            let info = params::ecmwf_param_info(pname);
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

            // Apply unit conversion
            let values: Vec<Option<f64>> = if info.has_conversion() {
                values
                    .into_iter()
                    .map(|v| v.map(|raw| info.convert(raw)))
                    .collect()
            } else {
                values
            };

            param_descs.insert(
                pname.to_string(),
                ParameterDescription {
                    label: info.label.to_string(),
                    unit: info.unit.to_string(),
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

        Ok(AreaQueryResult::Single(QueryResult {
            domain: DomainDescription::Grid {
                x: x_coords,
                y: y_coords,
                t: None,
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
    ) -> Result<RasterTile, DataServerError> {
        let datetime = time.map(|t| (t, t));
        let (_step, step_file) = self.resolve_time(datetime)?;

        // Determine parameter to render
        let param_name = parameter.unwrap_or_else(|| {
            // Default to first surface parameter
            step_file
                .messages
                .iter()
                .find(|m| m.levtype == "sfc")
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

        // Apply unit conversion so colormap ranges use display units
        let info = params::ecmwf_param_info(param_name);
        let values = if info.has_conversion() {
            values
                .into_iter()
                .map(|v| v.map(|raw| info.convert(raw)))
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

        // Build parameter list from catalog
        let params: Vec<(String, String)> = catalog
            .all_params()
            .into_iter()
            .map(|p| {
                let info = params::ecmwf_param_info(&p);
                (p, info.label.to_string())
            })
            .collect();

        let default_param = params
            .first()
            .map(|(name, _)| name.clone())
            .unwrap_or_else(|| "2t".to_string());

        let default_unit = {
            let info = params::ecmwf_param_info(&default_param);
            info.unit.to_string()
        };

        RasterInfo {
            native_crs: "EPSG:4326".to_string(),
            spatial_extent: Some([-180.0, -90.0, 180.0, 90.0]),
            times,
            parameter: default_param,
            unit: default_unit,
            parameters: params,
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

/// Build all S3 prefixes to scan for the given pattern, dates, and run hours.
///
/// The pattern supports strftime placeholders for the date part, plus `{run}`
/// which is expanded to each run hour (zero-padded, e.g., "00", "06", "12", "18").
///
/// If the pattern contains no `{run}` placeholder, it's expanded once per date
/// using strftime only (backward compatible with the original behavior).
fn build_scan_prefixes(pattern: &str, now: DateTime<Utc>, run_hours: &[u32]) -> Vec<String> {
    use chrono::Duration;

    let mut prefixes = Vec::new();

    for days_back in 0..SCAN_DAYS {
        let date = now - Duration::days(i64::from(days_back));

        if pattern.contains("{run}") {
            // Expand for each run hour
            for &hour in run_hours {
                let run_str = format!("{hour:02}");
                let with_run = pattern.replace("{run}", &run_str);
                prefixes.push(date.format(&with_run).to_string());
            }
        } else {
            // No {run} placeholder — just expand strftime
            prefixes.push(date.format(pattern).to_string());
        }
    }

    prefixes
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let dt = Utc.with_ymd_and_hms(2026, 4, 6, 15, 0, 0).unwrap();
        let prefixes = build_scan_prefixes("%Y%m%d/{run}z/ifs/0p25/oper/", dt, &[0, 6, 12, 18]);

        // 2 days × 4 run hours = 8 prefixes
        assert_eq!(prefixes.len(), 8);
        assert!(prefixes.contains(&"20260406/00z/ifs/0p25/oper/".to_string()));
        assert!(prefixes.contains(&"20260406/12z/ifs/0p25/oper/".to_string()));
        assert!(prefixes.contains(&"20260405/18z/ifs/0p25/oper/".to_string()));
    }

    #[test]
    fn test_build_scan_prefixes_no_run_placeholder() {
        use chrono::TimeZone;
        let dt = Utc.with_ymd_and_hms(2026, 4, 6, 15, 0, 0).unwrap();
        let prefixes = build_scan_prefixes("%Y%m%d/00z/ifs/0p25/oper/", dt, &[0, 6, 12, 18]);

        // No {run} placeholder — 2 dates, 1 prefix each
        assert_eq!(prefixes.len(), 2);
        assert_eq!(prefixes[0], "20260406/00z/ifs/0p25/oper/");
        assert_eq!(prefixes[1], "20260405/00z/ifs/0p25/oper/");
    }
}
