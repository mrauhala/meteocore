mod cache;
mod catalog;
mod geo;
mod reader;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use regex::Regex;
use std::sync::Arc;

use ds_core::config::GeoTiffConfig;
use ds_core::engine::Engine;
use ds_core::error::DataServerError;
use ds_core::model::*;

use crate::catalog::{scan_directory, scan_remote_with_limit, Catalog, PendingFile};
use crate::reader::TiffMetadata;

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
    },
}

pub struct GeoTiffEngine {
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
    data_path_display: String,
}

impl GeoTiffEngine {
    /// Create a new GeoTIFF engine, performing an initial scan.
    ///
    /// Data source is determined by config:
    /// - `endpoint` + `bucket` (+ optional `prefix_pattern`): S3 with dynamic prefix
    /// - `data_path` starting with `s3://` or `http(s)://`: S3/HTTP with fixed prefix
    /// - `data_path` otherwise: local directory
    pub fn new(data_path: Option<&str>, config: &GeoTiffConfig) -> Result<Self, DataServerError> {
        let filename_pattern = Regex::new(&config.filename_pattern).map_err(|e| {
            DataServerError::GeoTiff(format!("Invalid filename_pattern: {e}"))
        })?;

        if !config.filename_pattern.contains("(?P<timestamp>") {
            return Err(DataServerError::GeoTiff(
                "filename_pattern must contain a named capture group (?P<timestamp>...)".into(),
            ));
        }

        // Determine store mode from config
        let (store_mode, display) = if let (Some(endpoint), Some(bucket)) = (&config.endpoint, &config.bucket) {
            let store = ds_storage::build_s3_store_from_parts(endpoint, bucket)?;
            let prefix_pattern = config.prefix_pattern.clone().unwrap_or_default();
            let display = format!("s3://{}/{}", bucket, prefix_pattern);
            (
                StoreMode::RemoteDynamic {
                    store,
                    prefix_pattern,
                    scan_days: config.scan_days,
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
                "Either data_path or endpoint+bucket must be configured".into(),
            ));
        };

        let cache_bytes = config.tile_cache_mb * 1024 * 1024;
        let tile_cache = cache::TileCache::new(cache_bytes);

        let engine = GeoTiffEngine {
            catalog: ArcSwap::from_pointee(Catalog::empty()),
            tile_cache,
            store_mode,
            filename_pattern,
            timestamp_format: config.timestamp_format.clone(),
            parameter: config.parameter.clone(),
            unit: config.unit.clone(),
            poll_interval: Duration::from_secs(config.poll_interval_secs),
            exclude_patterns: config.exclude_patterns.clone(),
            max_files: config.max_files,
            data_path_display: display,
        };

        // Initial scan
        let initial_catalog = engine.do_scan(&BTreeMap::new(), &BTreeMap::new())?;
        let file_count = initial_catalog.entries.len();
        engine.catalog.store(Arc::new(initial_catalog));

        if file_count == 0 {
            tracing::warn!("No matching GeoTIFF files found in {}", engine.data_path_display);
        } else {
            tracing::info!("Loaded {} GeoTIFF files from {}", file_count, engine.data_path_display);
        }

        Ok(engine)
    }

    /// Perform a scan appropriate to the store mode.
    /// Applies max_files limit if configured.
    fn do_scan(
        &self,
        local_existing: &BTreeMap<PathBuf, (u64, TiffMetadata)>,
        remote_existing: &BTreeMap<PathBuf, catalog::FileEntry>,
    ) -> Result<Catalog, DataServerError> {
        let mut catalog = match &self.store_mode {
            StoreMode::Local { directory, pending } => {
                let mut pending = pending.lock().unwrap();
                scan_directory(
                    directory,
                    &self.filename_pattern,
                    &self.timestamp_format,
                    &self.exclude_patterns,
                    &mut pending,
                    local_existing,
                )?
            }
            StoreMode::Remote { store, prefix } => {
                scan_remote_with_limit(
                    store,
                    prefix,
                    &self.filename_pattern,
                    &self.timestamp_format,
                    remote_existing,
                    self.max_files,
                )?
            }
            StoreMode::RemoteDynamic { store, prefix_pattern, scan_days } => {
                let prefixes = expand_prefix_pattern(prefix_pattern, *scan_days);
                let mut merged = Catalog::empty();
                for prefix_str in &prefixes {
                    let prefix = ds_storage::object_store::path::Path::from(prefix_str.as_str());
                    match scan_remote_with_limit(
                        store,
                        &prefix,
                        &self.filename_pattern,
                        &self.timestamp_format,
                        remote_existing,
                        None, // no per-prefix limit; apply max_files after merge
                    ) {
                        Ok(partial) => {
                            merged.entries.extend(partial.entries);
                        }
                        Err(e) => {
                            tracing::warn!("Scan failed for prefix '{}': {e}", prefix_str);
                        }
                    }
                }
                // Recompute extents from merged entries
                merged.temporal_extent = if merged.entries.is_empty() {
                    None
                } else {
                    let first = *merged.entries.keys().next().unwrap();
                    let last = *merged.entries.keys().next_back().unwrap();
                    Some((first, last))
                };
                merged.spatial_extent = merged.entries.values().next()
                    .map(|e| e.metadata.geo_transform.bbox());
                merged
            }
        };

        if let Some(max) = self.max_files {
            catalog.trim_to_latest(max);
        }

        Ok(catalog)
    }

    /// Run the polling loop. Call this from a tokio::spawn task.
    pub async fn poll_loop(&self) {
        let mut interval = tokio::time::interval(self.poll_interval);
        interval.tick().await;

        loop {
            interval.tick().await;
            self.poll_once();
        }
    }

    fn poll_once(&self) {
        let current = self.catalog.load();

        let result = match &self.store_mode {
            StoreMode::Local { .. } => {
                let existing: BTreeMap<PathBuf, (u64, TiffMetadata)> = current
                    .entries
                    .values()
                    .map(|e| (e.path.clone(), (e.file_size, e.metadata.clone())))
                    .collect();
                self.do_scan(&existing, &BTreeMap::new())
            }
            StoreMode::Remote { .. } | StoreMode::RemoteDynamic { .. } => {
                let existing: BTreeMap<PathBuf, catalog::FileEntry> = current
                    .entries
                    .values()
                    .map(|e| (e.path.clone(), e.clone()))
                    .collect();
                self.do_scan(&BTreeMap::new(), &existing)
            }
        };

        match result {
            Ok(new_catalog) => {
                let count = new_catalog.entries.len();
                self.catalog.store(Arc::new(new_catalog));
                let (hits, misses) = self.tile_cache.stats();
                tracing::debug!("Catalog updated: {} files, tile cache: {} hits / {} misses", count, hits, misses);
            }
            Err(e) => {
                tracing::warn!("Scan failed, keeping old catalog: {e}");
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

        let catalog = self.catalog.load();

        let entries: Vec<_> = if let Some((start, end)) = datetime {
            catalog.entries.range(start..=end).collect()
        } else {
            catalog.entries.iter().collect()
        };

        if entries.is_empty() {
            return Err(DataServerError::LocationNotFound(
                "No data available for the requested coordinates/time range".into(),
            ));
        }

        let mut times = Vec::with_capacity(entries.len());
        let mut values = Vec::with_capacity(entries.len());

        for (timestamp, entry) in &entries {
            times.push(**timestamp);

            let pixel = entry.metadata.geo_transform.world_to_pixel(lon, lat);
            let value = match pixel {
                Some((col, row)) => {
                    match reader::read_pixel(&entry.source, &entry.metadata, col, row, Some(&self.tile_cache), &entry.path) {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!("Failed to read pixel from {}: {e}", entry.path.display());
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

        let catalog = self.catalog.load();

        let entries: Vec<_> = if let Some((start, end)) = datetime {
            catalog.entries.range(start..=end).collect()
        } else {
            catalog.entries.iter().collect()
        };

        if entries.is_empty() {
            return Err(DataServerError::LocationNotFound(
                "No data available for the requested time range".into(),
            ));
        }

        // Use first entry's geo_transform to compute pixel range and axis values
        let first_entry = entries[0].1;
        let (col_start, row_start, col_end, row_end) = first_entry
            .metadata
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
            .map(|c| first_entry.metadata.geo_transform.pixel_to_world(c, 0).0)
            .collect();
        let y_values: Vec<f64> = (row_start..row_end)
            .map(|r| first_entry.metadata.geo_transform.pixel_to_world(0, r).1)
            .collect();

        let has_time = entries.len() > 1;
        let mut times = Vec::with_capacity(entries.len());
        let mut all_values = Vec::new();

        for (timestamp, entry) in &entries {
            times.push(**timestamp);

            match reader::read_bbox(&entry.source, &entry.metadata, col_start, row_start, col_end, row_end, Some(&self.tile_cache), &entry.path) {
                Ok(grid_values) => {
                    all_values.extend(grid_values);
                }
                Err(e) => {
                    tracing::warn!("Failed to read bbox from {}: {e}", entry.path.display());
                    // Fill with None for this timestep
                    all_values.extend(std::iter::repeat(None).take(nx * ny));
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
            (
                vec![ny, nx],
                vec!["y".to_string(), "x".to_string()],
            )
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

impl Engine for GeoTiffEngine {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        Ok(vec![])
    }

    fn query_location(
        &self,
        location_id: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
    ) -> Result<QueryResult, DataServerError> {
        // Fallback: parse location_id as "lat,lon" and delegate to query_point
        let (lat, lon) = parse_location_id(location_id)?;
        self.query_point(lat, lon, datetime, parameters)
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
    ) -> Result<QueryResult, DataServerError> {
        let (west, south, east, north) = parse_bbox_coords(coords)?;
        self.query_bbox(west, south, east, north, datetime, parameters)
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

/// Expand a prefix pattern with strftime templates for the last `scan_days` days.
///
/// E.g. `"%Y/%m/%d/OPERA/COMP/"` with `scan_days=2` on 2026-03-25 produces:
/// `["2026/03/25/OPERA/COMP/", "2026/03/24/OPERA/COMP/"]`
///
/// If the pattern contains no `%` characters, returns it as-is (single prefix).
fn expand_prefix_pattern(pattern: &str, scan_days: u32) -> Vec<String> {
    if !pattern.contains('%') {
        return vec![pattern.trim_end_matches('/').to_string()];
    }

    let today = Utc::now().date_naive();
    let days = scan_days.max(1);
    let mut prefixes = Vec::with_capacity(days as usize);

    for offset in 0..days {
        let date = today - chrono::Duration::days(offset as i64);
        let expanded = date.format(pattern).to_string();
        prefixes.push(expanded.trim_end_matches('/').to_string());
    }

    prefixes
}

/// Parse EDR position query coordinates.
/// Accepts `POINT(lon lat)` (WKT) or `lon,lat` format.
/// Returns (lat, lon).
fn parse_coords(coords: &str) -> Result<(f64, f64), DataServerError> {
    let trimmed = coords.trim();

    // Try WKT POINT format: POINT(lon lat)
    if let Some(inner) = trimmed
        .strip_prefix("POINT(")
        .or_else(|| trimmed.strip_prefix("POINT ("))
        .and_then(|s| s.strip_suffix(')'))
    {
        let parts: Vec<&str> = inner.trim().split_whitespace().collect();
        if parts.len() != 2 {
            return Err(DataServerError::InvalidParameter(
                "Expected POINT(lon lat) format".into(),
            ));
        }
        let lon: f64 = parts[0].parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("Invalid longitude: {}", parts[0]))
        })?;
        let lat: f64 = parts[1].parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("Invalid latitude: {}", parts[1]))
        })?;
        return validate_coords(lat, lon);
    }

    // Try simple "lon,lat" format
    let parts: Vec<&str> = trimmed.split(',').collect();
    if parts.len() == 2 {
        let lon: f64 = parts[0].trim().parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("Invalid longitude: {}", parts[0]))
        })?;
        let lat: f64 = parts[1].trim().parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("Invalid latitude: {}", parts[1]))
        })?;
        return validate_coords(lat, lon);
    }

    Err(DataServerError::InvalidParameter(
        "Expected coords as POINT(lon lat) or lon,lat".into(),
    ))
}

fn validate_coords(lat: f64, lon: f64) -> Result<(f64, f64), DataServerError> {
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return Err(DataServerError::InvalidParameter(format!(
            "Coordinates out of range: lat={lat}, lon={lon}"
        )));
    }
    Ok((lat, lon))
}

/// Parse EDR area query coordinates.
/// Accepts `POLYGON((lon1 lat1, lon2 lat2, ...))` (WKT) — extracts the bounding box.
/// Also accepts `bbox` format `west,south,east,north`.
/// Returns (west, south, east, north).
fn parse_bbox_coords(coords: &str) -> Result<(f64, f64, f64, f64), DataServerError> {
    let trimmed = coords.trim();

    // Try WKT POLYGON format
    if let Some(inner) = trimmed
        .strip_prefix("POLYGON((")
        .or_else(|| trimmed.strip_prefix("POLYGON (("))
        .and_then(|s| s.strip_suffix("))"))
    {
        let points: Vec<&str> = inner.split(',').collect();
        if points.len() < 3 {
            return Err(DataServerError::InvalidParameter(
                "POLYGON must have at least 3 coordinate pairs".into(),
            ));
        }

        let mut min_lon = f64::MAX;
        let mut max_lon = f64::MIN;
        let mut min_lat = f64::MAX;
        let mut max_lat = f64::MIN;

        for point in &points {
            let parts: Vec<&str> = point.trim().split_whitespace().collect();
            if parts.len() != 2 {
                return Err(DataServerError::InvalidParameter(format!(
                    "Invalid coordinate pair: '{}'", point.trim()
                )));
            }
            let lon: f64 = parts[0].parse().map_err(|_| {
                DataServerError::InvalidParameter(format!("Invalid longitude: {}", parts[0]))
            })?;
            let lat: f64 = parts[1].parse().map_err(|_| {
                DataServerError::InvalidParameter(format!("Invalid latitude: {}", parts[1]))
            })?;
            min_lon = min_lon.min(lon);
            max_lon = max_lon.max(lon);
            min_lat = min_lat.min(lat);
            max_lat = max_lat.max(lat);
        }

        return Ok((min_lon, min_lat, max_lon, max_lat));
    }

    // Try simple bbox format: west,south,east,north
    let parts: Vec<&str> = trimmed.split(',').collect();
    if parts.len() == 4 {
        let west: f64 = parts[0].trim().parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("Invalid west: {}", parts[0]))
        })?;
        let south: f64 = parts[1].trim().parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("Invalid south: {}", parts[1]))
        })?;
        let east: f64 = parts[2].trim().parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("Invalid east: {}", parts[2]))
        })?;
        let north: f64 = parts[3].trim().parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("Invalid north: {}", parts[3]))
        })?;
        return Ok((west, south, east, north));
    }

    Err(DataServerError::InvalidParameter(
        "Expected coords as POLYGON((lon1 lat1, lon2 lat2, ...)) or west,south,east,north".into(),
    ))
}

fn parse_location_id(id: &str) -> Result<(f64, f64), DataServerError> {
    let parts: Vec<&str> = id.split(',').collect();
    if parts.len() != 2 {
        return Err(DataServerError::LocationNotFound(format!(
            "Expected 'lat,lon' format, got: {id}"
        )));
    }
    let lat: f64 = parts[0].trim().parse().map_err(|_| {
        DataServerError::LocationNotFound(format!("Invalid latitude in: {id}"))
    })?;
    let lon: f64 = parts[1].trim().parse().map_err(|_| {
        DataServerError::LocationNotFound(format!("Invalid longitude in: {id}"))
    })?;

    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return Err(DataServerError::LocationNotFound(format!(
            "Coordinates out of range: lat={lat}, lon={lon}"
        )));
    }

    Ok((lat, lon))
}

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
}
