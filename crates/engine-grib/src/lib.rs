pub mod cache;
pub mod catalog;
pub mod index;
pub mod params;
pub mod reader;

use std::collections::BTreeMap;
use std::sync::Arc;

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
}

impl GribEngine {
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

    /// Perform one scan cycle: list index files, parse, update catalog.
    fn scan_once(&self) -> Result<(), DataServerError> {
        let now = Utc::now();
        let prefix = expand_prefix(&self.prefix_pattern, now);

        let index_suffix = self.config.index_suffix.as_deref().unwrap_or(".index");
        let data_suffix = self.config.data_suffix.as_deref().unwrap_or(".grib2");

        // List objects in prefix
        let obj_prefix = ds_storage::object_store::path::Path::from(prefix.as_str());
        let objects = self.store.list(&obj_prefix).map_err(|e| {
            DataServerError::Storage(format!("Failed to list GRIB prefix '{prefix}': {e}"))
        })?;

        // Filter for index files
        let index_files: Vec<_> = objects
            .iter()
            .filter(|o| o.location.as_ref().ends_with(index_suffix))
            .collect();

        if index_files.is_empty() {
            tracing::debug!(
                "Collection '{}': no index files found in '{}'",
                self.collection_id,
                prefix
            );
            return Ok(());
        }

        tracing::info!(
            "Collection '{}': found {} index files in '{}'",
            self.collection_id,
            index_files.len(),
            prefix
        );

        let mut new_catalog = Catalog::new();

        for obj in &index_files {
            // Read index file
            let bytes = match self.store.get(&obj.location) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(
                        "Collection '{}': failed to read index file {}: {}",
                        self.collection_id,
                        obj.location,
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
                    obj.location
                );
                continue;
            };

            // Derive GRIB file URL from index file path
            let grib_url = obj.location.as_ref().replace(index_suffix, data_suffix);

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
        }

        // Apply max_runs eviction
        if let Some(max_runs) = self.config.max_runs {
            new_catalog.evict(max_runs);
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
            return Err(DataServerError::Grib(
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
        let params = self.catalog.load().all_params();
        let mut map = std::collections::HashMap::new();
        for p in params {
            let (label, unit) = params::ecmwf_param_info(&p);
            map.insert(
                p.clone(),
                ParameterDescription {
                    label: label.to_string(),
                    unit: unit.to_string(),
                    observed_property: p,
                },
            );
        }
        map
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        self.catalog.load().temporal_extent()
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
        // Parse "POINT(lon lat)" or "lon,lat"
        let (lon, lat) = parse_coords(coords)?;
        let (_step, step_file) = self.resolve_time(datetime)?;

        // Determine which parameters to query
        let _all_params = step_file.param_names();
        let query_params: Vec<&str> = match parameters {
            Some(p) => p.iter().map(|s| s.as_str()).collect(),
            None => {
                // Default to surface parameters only
                step_file
                    .messages
                    .iter()
                    .filter(|m| m.levtype == "sfc")
                    .map(|m| m.param.as_str())
                    .collect::<std::collections::HashSet<_>>()
                    .into_iter()
                    .collect()
            }
        };

        let mut param_descs = std::collections::HashMap::new();
        let mut ranges = std::collections::HashMap::new();

        for &param_name in &query_params {
            // Find the message (surface params have level=None)
            let level = step_file
                .messages
                .iter()
                .find(|m| m.param == param_name)
                .and_then(|m| m.level);

            let grid = match self.fetch_grid(&step_file, param_name, level) {
                Ok(g) => g,
                Err(e) => {
                    tracing::warn!("Failed to fetch grid for {param_name}: {e}");
                    continue;
                }
            };

            let value = grid.nearest_value(lon, lat);

            let (label, unit) = params::ecmwf_param_info(param_name);
            param_descs.insert(
                param_name.to_string(),
                ParameterDescription {
                    label: label.to_string(),
                    unit: unit.to_string(),
                    observed_property: param_name.to_string(),
                },
            );

            ranges.insert(
                param_name.to_string(),
                NdArray {
                    shape: vec![1],
                    axis_names: vec!["t".to_string()],
                    values: vec![value],
                },
            );
        }

        let valid_time = datetime.map_or_else(Utc::now, |(start, _)| start);

        Ok(QueryResult {
            domain: DomainDescription::PointSeries {
                x: lon,
                y: lat,
                t: vec![valid_time],
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

            let (label, unit) = params::ecmwf_param_info(pname);
            param_descs.insert(
                pname.to_string(),
                ParameterDescription {
                    label: label.to_string(),
                    unit: unit.to_string(),
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
                let (label, _unit) = params::ecmwf_param_info(&p);
                (p, label.to_string())
            })
            .collect();

        let default_param = params
            .first()
            .map(|(name, _)| name.clone())
            .unwrap_or_else(|| "2t".to_string());

        let default_unit = {
            let (_, unit) = params::ecmwf_param_info(&default_param);
            unit.to_string()
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

/// Expand a strftime prefix pattern to today's date.
fn expand_prefix(pattern: &str, now: DateTime<Utc>) -> String {
    now.format(pattern).to_string()
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
    fn test_expand_prefix() {
        use chrono::TimeZone;
        let dt = Utc.with_ymd_and_hms(2026, 4, 5, 0, 0, 0).unwrap();
        assert_eq!(
            expand_prefix("%Y%m%d/00z/ifs/0p25/oper/", dt),
            "20260405/00z/ifs/0p25/oper/"
        );
    }
}
