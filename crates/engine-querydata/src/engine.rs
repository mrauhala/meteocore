use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use tokio::sync::watch;

use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterInfo, RasterTile};
use ds_core::model::{
    CoverageResponse, DomainDescription, Location, NdArray, ParameterDescription, QueryResult,
};

use crate::parse::QueryData;

/// QueryData engine serving multi-parameter NWP/observation gridded data.
///
/// Polls a directory for `.sqd` files. The latest file (by filename sort)
/// is loaded and served. When a new file appears, it is atomically swapped
/// in via `ArcSwap`.
pub struct QueryDataEngine {
    /// Current loaded data. Swapped atomically on poll.
    data: ArcSwap<QueryData>,
    /// Directory to poll for .sqd files.
    data_dir: PathBuf,
    /// Currently loaded filename (for change detection).
    current_file: ArcSwap<Option<PathBuf>>,
    /// Parameter name to render for MapEngine (matched by name on each load).
    wms_parameter: Option<String>,
    /// Collection ID for logging.
    collection_id: String,
    /// Poll interval.
    poll_interval: Duration,
    /// Shutdown signal.
    shutdown_tx: watch::Sender<()>,
    /// Tracks when data was last successfully loaded/updated.
    data_updated_at: Mutex<Option<DateTime<Utc>>>,
}

impl QueryDataEngine {
    /// Create a new QueryDataEngine that polls a directory for .sqd files.
    ///
    /// Loads the latest file immediately. Returns an error if no files are found
    /// or the latest file cannot be parsed.
    pub fn new(
        data_dir: &Path,
        collection_id: &str,
        wms_parameter: Option<&str>,
        poll_interval_secs: u64,
    ) -> Result<Self, DataServerError> {
        let latest = find_latest_sqd(data_dir).ok_or_else(|| {
            DataServerError::Engine(format!(
                "[{collection_id}] No .sqd files found in {}",
                data_dir.display()
            ))
        })?;

        let data = load_file(&latest, collection_id)?;

        log_loaded(collection_id, &latest, &data);

        let (shutdown_tx, _) = watch::channel(());

        Ok(Self {
            data: ArcSwap::from_pointee(data),
            data_dir: data_dir.to_path_buf(),
            current_file: ArcSwap::from_pointee(Some(latest)),
            wms_parameter: wms_parameter.map(String::from),
            collection_id: collection_id.to_string(),
            poll_interval: Duration::from_secs(poll_interval_secs.max(1)),
            shutdown_tx,
            data_updated_at: Mutex::new(Some(Utc::now())),
        })
    }

    /// Run the directory poll loop. Exits when `shutdown()` is called.
    pub async fn poll_loop(&self) {
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        let mut interval = tokio::time::interval(self.poll_interval);
        interval.tick().await; // skip immediate first tick

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
        let latest = match find_latest_sqd(&self.data_dir) {
            Some(p) => p,
            None => return, // no files — keep current data
        };

        // Successful directory read — update staleness tracker
        *self
            .data_updated_at
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(Utc::now());

        // Check if file changed
        let current = self.current_file.load();
        if current.as_deref() == Some(&latest) {
            return;
        }

        match load_file(&latest, &self.collection_id) {
            Ok(new_data) => {
                log_loaded(&self.collection_id, &latest, &new_data);
                self.data.store(Arc::new(new_data));
                self.current_file.store(Arc::new(Some(latest)));
            }
            Err(e) => {
                tracing::error!(
                    "[{}] Failed to load {}: {e}",
                    self.collection_id,
                    latest.display()
                );
                // Keep old data on failure
            }
        }
    }

    /// Get a snapshot of the current data.
    fn load_data(&self) -> arc_swap::Guard<Arc<QueryData>> {
        self.data.load()
    }

    /// Resolve the map parameter index for the current data snapshot.
    fn resolve_map_param_idx(&self, data: &QueryData) -> usize {
        if let Some(ref name) = self.wms_parameter {
            data.param_index_by_name(name).unwrap_or(0)
        } else {
            0
        }
    }

    /// Check if this engine has data loaded.
    pub fn has_data(&self) -> bool {
        !self.data.load().times.is_empty()
    }

    /// The collection ID this engine serves.
    pub fn collection_id(&self) -> &str {
        &self.collection_id
    }

    /// How long ago the data was last successfully loaded/updated.
    pub fn data_age(&self) -> Option<chrono::Duration> {
        let updated_at = self
            .data_updated_at
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        updated_at.map(|t| Utc::now() - t)
    }
}

impl EdrEngine for QueryDataEngine {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        Ok(vec![])
    }

    fn query_location(
        &self,
        _location_id: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
    ) -> Result<CoverageResponse, DataServerError> {
        Err(DataServerError::InvalidParameter(
            "QueryData engine does not support location queries (use position query)".into(),
        ))
    }

    fn get_parameters(&self) -> Vec<String> {
        let data = self.load_data();
        data.params.iter().map(|p| p.name.clone()).collect()
    }

    fn get_parameter_descriptions(&self) -> HashMap<String, ParameterDescription> {
        let data = self.load_data();
        data.params
            .iter()
            .map(|p| {
                (
                    p.name.clone(),
                    ParameterDescription {
                        label: p.name.clone(),
                        unit: String::new(),
                        observed_property: p.name.clone(),
                    },
                )
            })
            .collect()
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        let data = self.load_data();
        let first = data.times.first()?;
        let last = data.times.last()?;
        Some((*first, *last))
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        let data = self.load_data();
        let bl = data.grid.area.bottom_left;
        let tr = data.grid.area.top_right;
        // Normalize to [west, south, east, north]. `bottom_left`/`top_right` are
        // the grid's first/last corners as stored, which for a north-to-south
        // (or cropped) grid can have bottom_left *north* of top_right — emitting
        // those raw would produce an invalid south>north bbox to WMS
        // `EX_GeographicBoundingBox`, EDR/Maps/Tiles extents.
        Some([
            bl.0.min(tr.0),
            bl.1.min(tr.1),
            bl.0.max(tr.0),
            bl.1.max(tr.1),
        ])
    }

    fn supported_query_types(&self) -> Vec<String> {
        vec!["position".to_string()]
    }

    fn query_position(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _z: Option<&[f64]>,
    ) -> Result<CoverageResponse, DataServerError> {
        let (lat, lon) = parse_coords(coords)?;
        let data = self.load_data();

        let time_indices = find_time_range(&data, datetime);
        if time_indices.is_empty() {
            return Err(DataServerError::Engine(
                "No data available for the requested time range".into(),
            ));
        }

        let times: Vec<DateTime<Utc>> = time_indices.iter().map(|(_, t)| *t).collect();

        let param_indices: Vec<(usize, &crate::parse::ParamInfo)> = data
            .params
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                parameters
                    .is_none_or(|filter| filter.iter().any(|f| f.eq_ignore_ascii_case(&p.name)))
            })
            .collect();

        if param_indices.is_empty() {
            return Err(DataServerError::InvalidParameter(
                "No matching parameters found".into(),
            ));
        }

        let domain = DomainDescription::PointSeries {
            x: lon,
            y: lat,
            t: times,
            z: None,
        };

        let mut params_map = HashMap::new();
        let mut ranges = HashMap::new();

        for (pi, param) in &param_indices {
            let values: Vec<Option<f64>> = time_indices
                .iter()
                .map(|(ti, _)| interpolate(&data, lon, lat, *pi, 0, *ti))
                .collect();

            params_map.insert(
                param.name.clone(),
                ParameterDescription {
                    label: param.name.clone(),
                    unit: String::new(),
                    observed_property: param.name.clone(),
                },
            );

            ranges.insert(
                param.name.clone(),
                NdArray {
                    shape: vec![values.len()],
                    axis_names: vec!["t".to_string()],
                    values,
                },
            );
        }

        Ok(CoverageResponse::Single(QueryResult {
            domain,
            parameters: params_map,
            ranges,
        }))
    }
}

impl MapEngine for QueryDataEngine {
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
        let _ = z; // QueryData collections expose no vertical dimension yet (#185)
        let data = self.load_data();
        let param_idx = if let Some(param_name) = parameter {
            data.param_index_by_name(param_name)
                .unwrap_or_else(|| self.resolve_map_param_idx(&data))
        } else {
            self.resolve_map_param_idx(&data)
        };

        let time_idx = find_time_idx(&data, time).ok_or_else(|| {
            DataServerError::Engine("No data available for the requested time".into())
        })?;

        let mut values = Vec::with_capacity((width * height) as usize);

        // Each output pixel's WGS84 lon/lat comes from the shared
        // `OutputCrs::project_node` (linear lon/lat, Mercator-Y, or a projected
        // output CRS; #160), and the source grid — itself possibly projected
        // (stereographic / rotated lat-lon) — is sampled by `world_to_grid_px`
        // (`Crs::forward` + affine) then bilinear.
        match output_crs {
            OutputCrs::Projected { .. } => {
                // Projected output runs `Crs::inverse` per node, and the source
                // mapping runs `Crs::forward` per node; compose both into a
                // coarse `ProjectionGrid` and bilinearly interpolate the
                // output→source pixel map, rather than projecting per output
                // pixel (CLAUDE.md "never project per output pixel"; #268). This
                // also removes the per-pixel *source* forward projection on this
                // path. Projected output is regional, so the grid stays accurate.
                let gt = data.grid.geo_transform();
                let grid = ds_core::resample::ProjectionGrid::build_2d(
                    width,
                    height,
                    data.grid.nx,
                    data.grid.ny,
                    |fx, fy| output_crs.project_node(bbox, fx, fy),
                    |lon, lat| world_to_grid_px(&gt, lon, lat),
                );
                for oy in 0..height {
                    for ox in 0..width {
                        let (col_f, row_f) = grid.sample(ox, oy);
                        values.push(sample_grid_bilinear(
                            &data, col_f, row_f, param_idx, 0, time_idx,
                        ));
                    }
                }
            }
            OutputCrs::Wgs84 | OutputCrs::WebMercator => {
                // `project_node` is cheap here (no projection); sample per pixel.
                for row in 0..height {
                    let fy = (row as f64 + 0.5) / height as f64;
                    for col in 0..width {
                        let fx = (col as f64 + 0.5) / width as f64;
                        let (lon, lat) = output_crs.project_node(bbox, fx, fy);
                        values.push(interpolate(&data, lon, lat, param_idx, 0, time_idx));
                    }
                }
            }
        }

        Ok(RasterTile {
            width,
            height,
            values,
        })
    }

    fn raster_info(&self) -> RasterInfo {
        let data = self.load_data();
        let param_idx = self.resolve_map_param_idx(&data);

        let param_name = data
            .params
            .get(param_idx)
            .map(|p| p.name.clone())
            .unwrap_or_default();

        let gt = data.grid.geo_transform();
        let bbox = gt.bbox();

        let native_crs = match data.grid.area.crs {
            // Internal grids are lon-first, so CRS:84 (not EPSG:4326, which is
            // lat-first) — this is the value surfaced as OGC `storageCrs`.
            // Generic labels match engine-geotiff/engine-odim so
            // ds_core::geo::native_crs_uri treats every engine consistently.
            ds_core::geo::Crs::Wgs84 => "CRS:84".to_string(),
            ds_core::geo::Crs::Stereographic { .. } => "stere".to_string(),
            ds_core::geo::Crs::RotatedLatLon { .. } => "rotated_ll".to_string(),
            _ => "projected".to_string(),
        };

        // Build parameter list: (short_name, full_title) for each parameter
        let parameters: Vec<(String, String)> = data
            .params
            .iter()
            .map(|p| {
                // Extract short name from parentheses, e.g., "2 Metre Temperature (2t)" → "2t"
                let short = p
                    .name
                    .rfind('(')
                    .and_then(|start| p.name[start + 1..].strip_suffix(')'))
                    .unwrap_or(&p.name)
                    .to_string();
                (short, p.name.clone())
            })
            .collect();

        RasterInfo {
            native_crs,
            spatial_extent: Some(bbox),
            times: data.times.clone(),
            parameter: param_name,
            unit: String::new(),
            parameters,
            vertical: None,
            grid_size: Some([gt.width, gt.height]),
            layer_subtitle: None,
        }
    }
}

// ============================================================================
// Free functions (operate on QueryData snapshots, not &self)
// ============================================================================

/// Find the latest .sqd file in a directory (by filename, lexicographic sort).
fn find_latest_sqd(dir: &Path) -> Option<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("sqd"))
        })
        .collect();

    entries.sort();
    entries.pop() // last = lexicographically latest
}

fn load_file(path: &Path, collection_id: &str) -> Result<QueryData, DataServerError> {
    QueryData::open(path).map_err(|e| {
        DataServerError::Engine(format!(
            "[{collection_id}] Failed to load {}: {e}",
            path.display()
        ))
    })
}

fn log_loaded(collection_id: &str, path: &Path, data: &QueryData) {
    tracing::info!(
        "[{}] Loaded {}: {} params, {}x{} grid, {} levels, {} times",
        collection_id,
        path.file_name().unwrap_or_default().to_string_lossy(),
        data.params.len(),
        data.grid.nx,
        data.grid.ny,
        data.levels.len(),
        data.times.len(),
    );
}

/// Bilinear interpolation at (lon, lat) for a given parameter and time.
///
/// Used by EDR position queries and the `Wgs84`/`WebMercator` map path. The
/// projected map path instead drives [`sample_grid_bilinear`] through a coarse
/// [`ProjectionGrid`] to avoid per-pixel projection (#268).
fn interpolate(
    data: &QueryData,
    lon: f64,
    lat: f64,
    param_idx: usize,
    level_idx: usize,
    time_idx: usize,
) -> Option<f64> {
    // An out-of-domain projected output pixel arrives as NaN; reject before the
    // forward transform (see `sample_grid_bilinear` for why NaN is dangerous).
    if !lon.is_finite() || !lat.is_finite() {
        return None;
    }
    let gt = data.grid.geo_transform();
    let (col_f, row_f) = world_to_grid_px(&gt, lon, lat);
    sample_grid_bilinear(data, col_f, row_f, param_idx, level_idx, time_idx)
}

/// Map WGS84 (lon, lat) to fractional source-grid pixel `(col_f, row_f)` — the
/// source `Crs::forward` plus the grid's affine, with the half-pixel centre
/// offset. This is the (possibly projected) per-node mapping fed to
/// [`ProjectionGrid::build_2d`] and the front half of [`interpolate`].
fn world_to_grid_px(gt: &ds_core::geo::GeoTransform, lon: f64, lat: f64) -> (f64, f64) {
    let (x, y) = gt.crs.forward(lon, lat);
    (
        (x - gt.origin_x) / gt.pixel_width - 0.5,
        (gt.origin_y - y) / gt.pixel_height - 0.5,
    )
}

/// Bilinearly sample the grid at fractional source pixel `(col_f, row_f)`,
/// falling back to nearest when a bilinear neighbour is nodata.
///
/// Returns `None` (transparent) for non-finite inputs or points off the grid.
/// Non-finite is the out-of-domain projected pixel case (`project_node` → NaN):
/// rejected up front because NaN comparisons are false and `NaN as i64/usize`
/// saturates to 0, so the bounds guards would otherwise pass and return
/// grid-origin data.
fn sample_grid_bilinear(
    data: &QueryData,
    col_f: f64,
    row_f: f64,
    param_idx: usize,
    level_idx: usize,
    time_idx: usize,
) -> Option<f64> {
    if !col_f.is_finite() || !row_f.is_finite() {
        return None;
    }

    let col0 = col_f.floor() as i64;
    let row0 = row_f.floor() as i64;

    let nx = data.grid.nx as i64;
    let ny = data.grid.ny as i64;

    if col0 < -1 || col0 >= nx || row0 < -1 || row0 >= ny {
        return None;
    }

    let dx = col_f - col0 as f64;
    let dy = row_f - row0 as f64;

    let mut vals = [None; 4];
    for (i, (dr, dc)) in [(0, 0), (0, 1), (1, 0), (1, 1)].iter().enumerate() {
        let c = col0 + dc;
        let r = row0 + dr;
        if c >= 0 && c < nx && r >= 0 && r < ny {
            let qd_row = (ny - 1 - r) as usize;
            let grid_idx = qd_row * nx as usize + c as usize;
            vals[i] = data.value(param_idx, grid_idx, level_idx, time_idx);
        }
    }

    match (vals[0], vals[1], vals[2], vals[3]) {
        (Some(tl), Some(tr), Some(bl), Some(br)) => {
            let top = tl + (tr - tl) * dx;
            let bot = bl + (br - bl) * dx;
            Some(top + (bot - top) * dy)
        }
        _ => {
            let nc = (col_f + 0.5).floor().clamp(0.0, (nx - 1) as f64) as usize;
            let nr = (row_f + 0.5).floor().clamp(0.0, (ny - 1) as f64) as usize;
            let qd_row = (ny as usize - 1) - nr;
            let grid_idx = qd_row * nx as usize + nc;
            data.value(param_idx, grid_idx, level_idx, time_idx)
        }
    }
}

/// Find the time index closest to the requested time.
fn find_time_idx(data: &QueryData, time: Option<DateTime<Utc>>) -> Option<usize> {
    if data.times.is_empty() {
        return None;
    }
    match time {
        None => Some(data.times.len() - 1),
        Some(t) => {
            let mut best_idx = 0;
            let mut best_diff = i64::MAX;
            for (i, dt) in data.times.iter().enumerate() {
                let diff = dt.signed_duration_since(t).num_seconds().abs();
                if diff < best_diff {
                    best_diff = diff;
                    best_idx = i;
                }
            }
            Some(best_idx)
        }
    }
}

/// Find time indices within a datetime range.
fn find_time_range(
    data: &QueryData,
    datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> Vec<(usize, DateTime<Utc>)> {
    match datetime {
        None => data
            .times
            .iter()
            .enumerate()
            .map(|(i, t)| (i, *t))
            .collect(),
        Some((start, end)) => data
            .times
            .iter()
            .enumerate()
            .filter(|(_, t)| **t >= start && **t <= end)
            .map(|(i, t)| (i, *t))
            .collect(),
    }
}

/// Parse EDR position query coordinates.
fn parse_coords(coords: &str) -> Result<(f64, f64), DataServerError> {
    let trimmed = coords.trim();

    if let Some(inner) = trimmed
        .strip_prefix("POINT(")
        .or_else(|| trimmed.strip_prefix("POINT ("))
        .and_then(|s| s.strip_suffix(')'))
    {
        let parts: Vec<&str> = inner.split_whitespace().collect();
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
        return Ok((lat, lon));
    }

    let parts: Vec<&str> = trimmed.split(',').collect();
    if parts.len() == 2 {
        let lon: f64 = parts[0].trim().parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("Invalid longitude: {}", parts[0]))
        })?;
        let lat: f64 = parts[1].trim().parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("Invalid latitude: {}", parts[1]))
        })?;
        return Ok((lat, lon));
    }

    Err(DataServerError::InvalidParameter(
        "Expected POINT(lon lat) or lon,lat format".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/ecmwf-kenya")
    }

    fn test_file_exists() -> bool {
        test_dir().exists() && find_latest_sqd(&test_dir()).is_some()
    }

    #[test]
    fn engine_from_directory() {
        assert!(test_file_exists(), "ecmwf-kenya fixture missing");
        let engine = QueryDataEngine::new(&test_dir(), "test", None, 30).unwrap();
        assert!(engine.has_data());
        let params = engine.get_parameters();
        assert_eq!(params.len(), 3);
    }

    #[test]
    fn engine_spatial_extent() {
        assert!(test_file_exists(), "ecmwf-kenya fixture missing");
        let engine = QueryDataEngine::new(&test_dir(), "test", None, 30).unwrap();
        // [west, south, east, north] — normalized, so south < north even though
        // this fixture's stored bottom_left lat (4.75) is north of top_right
        // (-5.25). Guards the get_spatial_extent min/max normalization.
        let bbox = engine.get_spatial_extent().unwrap();
        assert!((bbox[0] - 34.0).abs() < 0.01, "west {}", bbox[0]);
        assert!((bbox[1] - (-5.25)).abs() < 0.01, "south {}", bbox[1]);
        assert!((bbox[2] - 41.5).abs() < 0.01, "east {}", bbox[2]);
        assert!((bbox[3] - 4.75).abs() < 0.01, "north {}", bbox[3]);
    }

    #[test]
    fn engine_spatial_extent_lcc() {
        // Projected (LCC) fixture: confirms the get_spatial_extent min/max
        // normalization holds for non-WGS84 grids too. FMI grids store
        // north-to-south, so the raw bottom_left lat is north of top_right —
        // the result must still be a valid [west, south, east, north].
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/meps");
        assert!(
            dir.exists() && find_latest_sqd(&dir).is_some(),
            "meps fixture missing"
        );
        let engine = QueryDataEngine::new(&dir, "test", None, 30).unwrap();
        let bbox = engine.get_spatial_extent().unwrap();
        assert!(bbox[0] < bbox[2], "west {} < east {}", bbox[0], bbox[2]);
        assert!(bbox[1] < bbox[3], "south {} < north {}", bbox[1], bbox[3]);
        assert!((bbox[1] - 60.02).abs() < 0.1, "south {}", bbox[1]);
        assert!((bbox[3] - 64.96).abs() < 0.1, "north {}", bbox[3]);
    }

    #[test]
    fn engine_temporal_extent() {
        assert!(test_file_exists(), "ecmwf-kenya fixture missing");
        let engine = QueryDataEngine::new(&test_dir(), "test", None, 30).unwrap();
        let (first, last) = engine.get_temporal_extent().unwrap();
        assert_eq!(
            first.format("%Y-%m-%dT%H:%M").to_string(),
            "2026-04-04T06:00"
        );
        assert!(last > first);
    }

    #[test]
    fn engine_position_query() {
        assert!(test_file_exists(), "ecmwf-kenya fixture missing");
        let engine = QueryDataEngine::new(&test_dir(), "test", None, 30).unwrap();

        let response = engine
            .query_position("POINT(36.8 -1.3)", None, None, None)
            .unwrap();
        let result = match response {
            CoverageResponse::Single(qr) => qr,
            CoverageResponse::Collection(_) => panic!("expected Single"),
        };

        assert_eq!(result.parameters.len(), 3);
        assert_eq!(result.ranges.len(), 3);

        let temp = result.ranges.get("2 Metre Temperature (2t)").unwrap();
        let has_values = temp.values.iter().any(|v| v.is_some());
        assert!(has_values, "Temperature should have some values");
    }

    #[test]
    fn engine_position_query_filtered_params() {
        assert!(test_file_exists(), "ecmwf-kenya fixture missing");
        let engine = QueryDataEngine::new(&test_dir(), "test", None, 30).unwrap();

        let params = vec!["2 Metre Temperature (2t)".to_string()];
        let response = engine
            .query_position("POINT(36.8 -1.3)", None, Some(&params), None)
            .unwrap();
        let result = match response {
            CoverageResponse::Single(qr) => qr,
            CoverageResponse::Collection(_) => panic!("expected Single"),
        };

        assert_eq!(result.parameters.len(), 1);
        assert!(result.parameters.contains_key("2 Metre Temperature (2t)"));
    }

    #[test]
    fn map_engine_raster_tile() {
        assert!(test_file_exists(), "ecmwf-kenya fixture missing");
        let engine =
            QueryDataEngine::new(&test_dir(), "test", Some("2 Metre Temperature (2t)"), 30)
                .unwrap();

        let tile = engine
            .get_raster_tile(
                [33.0, -5.0, 42.0, 5.0],
                16,
                16,
                None,
                &OutputCrs::Wgs84,
                None,
                None,
            )
            .unwrap();

        assert_eq!(tile.width, 16);
        assert_eq!(tile.height, 16);
        assert_eq!(tile.values.len(), 256);
        let non_none = tile.values.iter().filter(|v| v.is_some()).count();
        assert!(non_none > 0, "Tile should have some data values");
    }

    #[test]
    fn map_engine_raster_tile_projected_via_build_2d() {
        // Exercises the OutputCrs::Projected coarse-grid path (#268). TM math is
        // globally valid, so projecting the fixture's own region into EPSG:3067
        // metres and back must still place data and never leak NaN — even though
        // the fixture is nowhere near the TM35FIN zone.
        assert!(test_file_exists(), "ecmwf-kenya fixture missing");
        let engine =
            QueryDataEngine::new(&test_dir(), "test", Some("2 Metre Temperature (2t)"), 30)
                .unwrap();
        let crs = ds_core::geo::projected_output_crs("EPSG:3067").unwrap();
        let proj = ds_core::geo::projected_envelope(&crs, [33.0, -5.0, 42.0, 5.0]);
        let read = ds_core::geo::wgs84_envelope(&crs, proj).expect("in-domain envelope");
        let tile = engine
            .get_raster_tile(
                read,
                16,
                16,
                None,
                &OutputCrs::Projected { crs, bbox: proj },
                None,
                None,
            )
            .unwrap();

        assert_eq!(tile.values.len(), 256);
        assert!(
            tile.values.iter().filter(|v| v.is_some()).count() > 0,
            "projected build_2d tile should have data"
        );
        assert!(
            tile.values.iter().flatten().all(|v| v.is_finite()),
            "no NaN may leak through the build_2d path"
        );
    }

    #[test]
    fn map_engine_raster_info() {
        assert!(test_file_exists(), "ecmwf-kenya fixture missing");
        let engine =
            QueryDataEngine::new(&test_dir(), "test", Some("2 Metre Temperature (2t)"), 30)
                .unwrap();
        let info = engine.raster_info();

        assert_eq!(info.parameter, "2 Metre Temperature (2t)");
        // Lon-first geographic grid -> CRS:84 (not lat-first EPSG:4326).
        assert_eq!(info.native_crs, "CRS:84");
        assert_eq!(info.times.len(), 4);
        assert!(info.spatial_extent.is_some());
    }

    #[test]
    fn find_latest_sqd_in_dir() {
        assert!(test_file_exists(), "ecmwf-kenya fixture missing");
        let latest = find_latest_sqd(&test_dir());
        assert!(latest.is_some());
        let name = latest.unwrap();
        assert!(name.to_string_lossy().ends_with(".sqd"));
    }
}
