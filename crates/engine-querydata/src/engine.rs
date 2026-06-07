use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use tokio::sync::watch;

use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::instances::{self, RunInfo};
use ds_core::map_engine::{MapEngine, OutputCrs, RasterInfo, RasterTile};
use ds_core::model::{
    CoverageResponse, DomainDescription, Location, NdArray, ParameterDescription, QueryResult,
};

use crate::parse::QueryData;

/// One retained model run: a parsed `.sqd` file plus its source path (for
/// change detection on poll). Keyed in [`RunSet`] by the file's origin time.
struct RunEntry {
    data: Arc<QueryData>,
    path: PathBuf,
}

/// The retained model runs, keyed by origin (analysis / forecast reference)
/// time, ascending — so `values().next_back()` is the latest run. Swapped
/// atomically on poll. See [`ds_core::instances`].
#[derive(Default)]
struct RunSet {
    runs: BTreeMap<DateTime<Utc>, RunEntry>,
}

impl RunSet {
    /// The latest (most recent reference time) run, if any.
    fn latest(&self) -> Option<&RunEntry> {
        self.runs.values().next_back()
    }
}

/// QueryData engine serving multi-parameter NWP/observation gridded data.
///
/// Polls a directory for `.sqd` files and retains the most recent `max_runs`
/// as model runs (keyed by origin time), exposing each as an OGC EDR instance /
/// WMS `reference_time` (#337). The newest run is the default for un-pinned
/// queries. New/removed files are picked up on poll and the run set is swapped
/// atomically via `ArcSwap`; already-loaded files are reused (not re-parsed).
pub struct QueryDataEngine {
    /// Retained model runs. Swapped atomically on poll.
    runs: ArcSwap<RunSet>,
    /// Directory to poll for .sqd files.
    data_dir: PathBuf,
    /// Parameter name to render for MapEngine (matched by name on each load).
    wms_parameter: Option<String>,
    /// Collection ID for logging.
    collection_id: String,
    /// Poll interval.
    poll_interval: Duration,
    /// How many recent runs to retain (>= 1).
    max_runs: usize,
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
        max_runs: usize,
    ) -> Result<Self, DataServerError> {
        let max_runs = max_runs.max(1);
        let files = list_sqd_files(data_dir);
        let runset = build_runset(&files, max_runs, &RunSet::default(), collection_id);
        if runset.runs.is_empty() {
            return Err(DataServerError::Engine(format!(
                "[{collection_id}] No loadable .sqd files found in {}",
                data_dir.display()
            )));
        }

        let (shutdown_tx, _) = watch::channel(());

        Ok(Self {
            runs: ArcSwap::from_pointee(runset),
            data_dir: data_dir.to_path_buf(),
            wms_parameter: wms_parameter.map(String::from),
            collection_id: collection_id.to_string(),
            poll_interval: Duration::from_secs(poll_interval_secs.max(1)),
            max_runs,
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
        // List the directory once; reuse for the staleness guard and the rebuild.
        let files = list_sqd_files(&self.data_dir);
        if files.is_empty() {
            return; // no files (or unreadable dir) — keep current data
        }

        // Successful directory read — update staleness tracker
        *self
            .data_updated_at
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(Utc::now());

        let prev = self.runs.load();
        let new_set = build_runset(&files, self.max_runs, &prev, &self.collection_id);
        if new_set.runs.is_empty() {
            return; // nothing loadable — keep old data
        }

        // Swap only when the retained file set actually changed (add/remove).
        let prev_paths: BTreeSet<&Path> = prev.runs.values().map(|e| e.path.as_path()).collect();
        let new_paths: BTreeSet<&Path> = new_set.runs.values().map(|e| e.path.as_path()).collect();
        if prev_paths != new_paths {
            self.runs.store(Arc::new(new_set));
        }
    }

    /// The data for a requested model run: `None` ⇒ the latest run; `Some(rt)` ⇒
    /// the run with exactly that reference time (absent ⇒ error → 404). Shares
    /// the selection rule with every forecast engine via [`instances::select_run`].
    fn select_data(
        &self,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<Arc<QueryData>, DataServerError> {
        let set = self.runs.load();
        instances::select_run(&set.runs, reference_time)
            .map(|(_, e)| e.data.clone())
            .ok_or_else(|| match reference_time {
                Some(rt) => DataServerError::ReferenceTimeNotFound(format!(
                    "no model run for reference time {rt}"
                )),
                None => DataServerError::Engine("No data available".into()),
            })
    }

    /// A snapshot of the latest run's data (for run-agnostic metadata). The
    /// engine always retains at least one run after construction.
    fn latest_data(&self) -> Option<Arc<QueryData>> {
        self.runs.load().latest().map(|e| e.data.clone())
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
        self.latest_data().is_some_and(|d| !d.times.is_empty())
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

    /// Each retained `.sqd` file is a model run / EDR instance (latest last),
    /// with its own valid times.
    fn get_instances(&self) -> Vec<RunInfo> {
        let set = self.runs.load();
        instances::build_instances(&set.runs, |_, e| e.data.times.clone())
    }

    fn has_instances(&self) -> bool {
        !self.runs.load().runs.is_empty()
    }

    fn find_instance(&self, reference_time: DateTime<Utc>) -> Option<RunInfo> {
        let set = self.runs.load();
        set.runs.get(&reference_time).map(|e| RunInfo {
            reference_time,
            valid_times: e.data.times.clone(),
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
            "QueryData engine does not support location queries (use position query)".into(),
        ))
    }

    fn get_parameters(&self) -> Vec<String> {
        let Some(data) = self.latest_data() else {
            return Vec::new();
        };
        data.params.iter().map(|p| p.name.clone()).collect()
    }

    fn get_parameter_descriptions(&self) -> HashMap<String, ParameterDescription> {
        let Some(data) = self.latest_data() else {
            return HashMap::new();
        };
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
        let data = self.latest_data()?;
        let first = data.times.first()?;
        let last = data.times.last()?;
        Some((*first, *last))
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        let data = self.latest_data()?;
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
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        let (lat, lon) = parse_coords(coords)?;
        let data = self.select_data(reference_time)?;

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
        let _ = z; // QueryData collections expose no vertical dimension yet (#185)
        let data = self.select_data(reference_time)?;
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
        let set = self.runs.load();
        // Every retained run is a selectable reference time (WMS dimension /
        // EDR instance); ascending, latest last.
        let reference_times: Vec<DateTime<Utc>> = set.runs.keys().copied().collect();
        let data = match set.latest() {
            Some(e) => e.data.clone(),
            None => {
                // No runs retained (shouldn't happen post-construction).
                return RasterInfo {
                    native_crs: "CRS:84".to_string(),
                    spatial_extent: None,
                    times: Vec::new(),
                    parameter: String::new(),
                    unit: String::new(),
                    parameters: Vec::new(),
                    vertical: None,
                    grid_size: None,
                    layer_subtitle: None,
                    reference_times,
                };
            }
        };
        drop(set);
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
            reference_times,
        }
    }
}

// ============================================================================
// Free functions (operate on QueryData snapshots, not &self)
// ============================================================================

/// List `.sqd` files in a directory, sorted ascending by filename (lexicographic
/// ≈ chronological for the usual `…YYYYMMDDHHMM.sqd` naming, so the last entry is
/// the latest run). Returns empty on a directory read error.
fn list_sqd_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        // A read failure (e.g. a permissions regression) is indistinguishable
        // from "empty" to callers — log it so a silent stale-data situation has
        // a breadcrumb. Poll then keeps the current data.
        tracing::warn!(dir = %dir.display(), "cannot read .sqd directory; keeping current data");
        return Vec::new();
    };
    let mut entries: Vec<PathBuf> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("sqd"))
        })
        .collect();
    entries.sort();
    entries
}

/// Build a [`RunSet`] from the directory: load the most recent `max_runs` files
/// as model runs (keyed by origin time), reusing already-parsed entries from
/// `prev` whose path is unchanged (so poll never re-parses a stable run).
/// Unloadable files are logged and skipped.
///
/// `files` is the directory listing (ascending; see [`list_sqd_files`]) — the
/// caller lists once and passes it in so poll never reads the directory twice.
fn build_runset(files: &[PathBuf], max_runs: usize, prev: &RunSet, collection_id: &str) -> RunSet {
    // Keep only the most recent `max_runs` files (the listing is ascending).
    let window = &files[files.len().saturating_sub(max_runs)..];

    let by_path: HashMap<&Path, &RunEntry> =
        prev.runs.values().map(|e| (e.path.as_path(), e)).collect();

    let mut runs: BTreeMap<DateTime<Utc>, RunEntry> = BTreeMap::new();
    for path in window {
        let entry = if let Some(existing) = by_path.get(path.as_path()) {
            RunEntry {
                data: existing.data.clone(),
                path: path.clone(),
            }
        } else {
            match load_file(path, collection_id) {
                Ok(data) => {
                    let data = Arc::new(data);
                    log_loaded(collection_id, path, &data);
                    RunEntry {
                        data,
                        path: path.clone(),
                    }
                }
                Err(e) => {
                    // `e` already carries the `[collection_id]` prefix.
                    tracing::error!("{e}");
                    continue;
                }
            }
        };
        // Two files decoding to the same origin time (e.g. a reissued run) would
        // collide on the key; the later-sorted file wins. Surface the drop so it
        // isn't silent.
        if let Some(prev) = runs.insert(entry.data.origin_time, entry) {
            tracing::warn!(
                "[{collection_id}] two .sqd files share origin time {}; keeping the later one, dropping {}",
                prev.data.origin_time,
                prev.path.display()
            );
        }
    }
    RunSet { runs }
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
        test_dir().exists() && !list_sqd_files(&test_dir()).is_empty()
    }

    #[test]
    fn engine_from_directory() {
        assert!(test_file_exists(), "ecmwf-kenya fixture missing");
        let engine = QueryDataEngine::new(&test_dir(), "test", None, 30, 4).unwrap();
        assert!(engine.has_data());
        let params = engine.get_parameters();
        assert_eq!(params.len(), 3);
    }

    #[test]
    fn engine_spatial_extent() {
        assert!(test_file_exists(), "ecmwf-kenya fixture missing");
        let engine = QueryDataEngine::new(&test_dir(), "test", None, 30, 4).unwrap();
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
            dir.exists() && !list_sqd_files(&dir).is_empty(),
            "meps fixture missing"
        );
        let engine = QueryDataEngine::new(&dir, "test", None, 30, 4).unwrap();
        let bbox = engine.get_spatial_extent().unwrap();
        assert!(bbox[0] < bbox[2], "west {} < east {}", bbox[0], bbox[2]);
        assert!(bbox[1] < bbox[3], "south {} < north {}", bbox[1], bbox[3]);
        // Pin all four to the cropped LCC corners (in degrees) — a parse
        // regression returning projected metres would be ~10^5, not ~10-65.
        assert!((bbox[0] - 9.04).abs() < 0.1, "west {}", bbox[0]);
        assert!((bbox[1] - 60.02).abs() < 0.1, "south {}", bbox[1]);
        assert!((bbox[2] - 19.13).abs() < 0.1, "east {}", bbox[2]);
        assert!((bbox[3] - 64.96).abs() < 0.1, "north {}", bbox[3]);
    }

    #[test]
    fn engine_temporal_extent() {
        assert!(test_file_exists(), "ecmwf-kenya fixture missing");
        let engine = QueryDataEngine::new(&test_dir(), "test", None, 30, 4).unwrap();
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
        let engine = QueryDataEngine::new(&test_dir(), "test", None, 30, 4).unwrap();

        let response = engine
            .query_position("POINT(36.8 -1.3)", None, None, None, None)
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
        let engine = QueryDataEngine::new(&test_dir(), "test", None, 30, 4).unwrap();

        let params = vec!["2 Metre Temperature (2t)".to_string()];
        let response = engine
            .query_position("POINT(36.8 -1.3)", None, Some(&params), None, None)
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
            QueryDataEngine::new(&test_dir(), "test", Some("2 Metre Temperature (2t)"), 30, 4)
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
            QueryDataEngine::new(&test_dir(), "test", Some("2 Metre Temperature (2t)"), 30, 4)
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
            QueryDataEngine::new(&test_dir(), "test", Some("2 Metre Temperature (2t)"), 30, 4)
                .unwrap();
        let info = engine.raster_info();

        assert_eq!(info.parameter, "2 Metre Temperature (2t)");
        // Lon-first geographic grid -> CRS:84 (not lat-first EPSG:4326).
        assert_eq!(info.native_crs, "CRS:84");
        assert_eq!(info.times.len(), 4);
        assert!(info.spatial_extent.is_some());
    }

    #[test]
    fn list_sqd_files_in_dir() {
        assert!(test_file_exists(), "ecmwf-kenya fixture missing");
        let files = list_sqd_files(&test_dir());
        assert!(!files.is_empty());
        let latest = files.last().unwrap();
        assert!(latest.to_string_lossy().ends_with(".sqd"));
    }

    #[test]
    fn instances_expose_runs_latest_default() {
        assert!(test_file_exists(), "ecmwf-kenya fixture missing");
        let engine = QueryDataEngine::new(&test_dir(), "test", None, 30, 4).unwrap();
        let instances = engine.get_instances();
        // The fixture dir has at least one .sqd → at least one run/instance.
        assert!(!instances.is_empty());
        // raster_info advertises the same runs as reference times.
        assert_eq!(engine.raster_info().reference_times.len(), instances.len());
        // An un-pinned position query (reference_time = None) serves the latest
        // run; pinning the latest run's reference time returns the same series.
        let latest_rt = instances.last().unwrap().reference_time;
        let default = engine
            .query_position("POINT(36.8 -1.3)", None, None, None, None)
            .unwrap();
        let pinned = engine
            .query_position("POINT(36.8 -1.3)", None, None, None, Some(latest_rt))
            .unwrap();
        let (a, b) = match (default, pinned) {
            (CoverageResponse::Single(a), CoverageResponse::Single(b)) => (a, b),
            _ => panic!("expected Single coverages"),
        };
        assert_eq!(a.ranges.len(), b.ranges.len());
        // A bogus reference time is rejected (→ 404 at the API layer).
        let bogus = chrono::DateTime::parse_from_rfc3339("1990-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(engine
            .query_position("POINT(36.8 -1.3)", None, None, None, Some(bogus))
            .is_err());
    }
}
