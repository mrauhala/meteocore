use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use ds_poll::{FirstTick, Shutdown};

use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::feature::{check_area_budget, check_mask_budget, parse_area_coords, MAX_AREA_DIM};
use ds_core::instances::{self, RunInfo};
use ds_core::map_engine::{MapEngine, OutputCrs, RasterInfo, RasterTile};
use ds_core::model::{
    CoverageResponse, DomainDescription, Location, NdArray, ParameterDescription, QueryResult,
};
use ds_core::temp_files;
use ds_core::trajectory::{GridSpacing, TrajectoryAxes, TrajectoryPath, TrajectoryPlan};
use ds_core::wind::{GridAxes, ParameterFacts, VectorFrame, WindFacts, WindRole, WindSource};

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
    /// The latest run's wind components (#897), built with the set.
    wind: Option<Arc<WindFacts>>,
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
    /// Edge-triggered stop signal for `poll_loop` (shared lifecycle, #481).
    shutdown: Shutdown,
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

        Ok(Self {
            runs: ArcSwap::from_pointee(runset),
            data_dir: data_dir.to_path_buf(),
            wms_parameter: wms_parameter.map(String::from),
            collection_id: collection_id.to_string(),
            poll_interval: Duration::from_secs(poll_interval_secs.max(1)),
            max_runs,
            shutdown: Shutdown::new(),
            data_updated_at: Mutex::new(Some(Utc::now())),
        })
    }

    /// Run the directory poll loop. Exits when `shutdown()` is called.
    pub async fn poll_loop(&self) {
        let mut ticker = self.shutdown.ticker(self.poll_interval, FirstTick::Skip);
        while ticker.tick().await {
            self.poll_once();
        }
        tracing::info!("[{}] Poll loop shutting down", self.collection_id);
    }

    /// Signal the polling loop to stop.
    pub fn shutdown(&self) {
        self.shutdown.shutdown();
    }

    fn poll_once(&self) {
        // List the directory once; reuse for the staleness guard and the rebuild.
        let files = list_sqd_files(&self.data_dir);
        if files.is_empty() {
            return; // no files (or unreadable dir) — keep current data
        }

        let prev = self.runs.load();
        let new_set = build_runset(&files, self.max_runs, &prev, &self.collection_id);
        if new_set.runs.is_empty() {
            return; // nothing loadable (e.g. all files corrupt) — keep old data
                    // and do NOT stamp freshness; data_age keeps growing.
        }

        // We have a usable run set — stamp freshness (reflects loadable data, not
        // merely a readable directory).
        *self
            .data_updated_at
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(Utc::now());

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
                        standard_name: None,
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
        Some(data.grid.lonlat_extent())
    }

    fn supported_query_types(&self) -> Vec<String> {
        vec![
            "position".to_string(),
            "area".to_string(),
            "radius".to_string(),
            "trajectory".to_string(),
        ]
    }

    /// Trajectory query (#926): values along a WKT `LINESTRING` / `Z` / `M`
    /// / `ZM` path, densified to about one sample per grid cell crossed and
    /// bilinearly interpolated like position. The run's grid is memory
    /// mapped, so sampling does no I/O; each sample is projected once and
    /// reused by every parameter and timestep. No vertical axis: a Z
    /// coordinate is ignored.
    fn query_trajectory(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        let path = TrajectoryPath::parse(coords)?;
        let data = self.select_data(reference_time)?;
        // A 2-D path takes the position query's steps; an M path snaps to
        // any step of the run.
        let time_indices = find_time_range(&data, datetime.filter(|_| !path.has_m));
        let times: Vec<DateTime<Utc>> = time_indices.iter().map(|(_, t)| *t).collect();
        let param_indices = select_param_indices(&data, parameters)?;
        // The native cell size the area query samples at: exact for lat-lon
        // grids, a fair mean for projected ones.
        let extent = data.grid.lonlat_extent();
        let spacing =
            GridSpacing::from_extent(extent, [data.grid.nx as usize, data.grid.ny as usize])
                .ok_or_else(|| DataServerError::Engine("QueryData grid has no extent".into()))?;
        let plan = TrajectoryPlan::new(
            &path,
            spacing,
            TrajectoryAxes {
                times: &times,
                vertical: None,
                z: None,
            },
            param_indices.len(),
        )?;
        plan.require_extent(Some(extent))?;

        // Project each sample once (per vertex, never per output pixel).
        let gt = data.grid.geo_transform();
        let pixels: Vec<(f64, f64)> = plan
            .points()
            .iter()
            .map(|&(lon, lat)| world_to_grid_px(gt, lon, lat))
            .collect();
        let values: Vec<Vec<Vec<Option<f64>>>> = param_indices
            .iter()
            .map(|(pi, _)| {
                plan.fields()
                    .iter()
                    .map(|field| {
                        let ti = time_indices[field.time].0;
                        field
                            .points
                            .iter()
                            .map(|&p| {
                                let (col_f, row_f) = pixels[p];
                                sample_grid_bilinear(&data, col_f, row_f, *pi, 0, ti)
                            })
                            .collect()
                    })
                    .collect()
            })
            .collect();
        let descriptions: Vec<(String, ParameterDescription)> = param_indices
            .iter()
            .map(|(_, param)| {
                (
                    param.name.clone(),
                    ParameterDescription {
                        label: param.name.clone(),
                        unit: String::new(),
                        observed_property: param.name.clone(),
                        standard_name: None,
                    },
                )
            })
            .collect();
        plan.into_response(&descriptions, &values)
    }

    /// Area query: a CRS84 `Grid` over the polygon's bbox at the source's
    /// native resolution (each dimension ≤ `MAX_AREA_DIM`), every cell
    /// bilinearly interpolated from the run's grid and cells outside the
    /// polygon masked to null (#671). One `t` axis when the datetime window
    /// selects more than one step.
    fn query_area(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        let polygon = parse_area_coords(coords)?;
        let data = self.select_data(reference_time)?;

        let time_indices = find_time_range(&data, datetime);
        if time_indices.is_empty() {
            // No step in the window: no data (404), so a datetime list
            // skips the instant.
            return Err(DataServerError::LocationNotFound(
                "No data available for the requested time range".into(),
            ));
        }

        let param_indices = select_param_indices(&data, parameters)?;

        // A polygon entirely outside the run's coverage is a 404, not an
        // all-null 200 (GRIB answers the same way).
        let extent = data.grid.lonlat_extent();
        if !polygon.bbox.intersects_bbox(&extent) {
            return Err(DataServerError::LocationNotFound(
                "The polygon lies outside the collection's spatial extent".into(),
            ));
        }

        // Native resolution in degrees from the grid's corner coordinates —
        // exact for lat-lon grids, a fair mean cell size for projected ones.
        let res_lon = (extent[2] - extent[0]) / data.grid.nx.max(1) as f64;
        let res_lat = (extent[3] - extent[1]) / data.grid.ny.max(1) as f64;
        let axes = polygon.sample_grid(res_lon, res_lat, MAX_AREA_DIM);
        let (nx, ny) = axes.dims();
        check_area_budget(time_indices.len(), ny, nx, param_indices.len())?;
        check_mask_budget(nx * ny, &polygon)?;

        // Project each cell centre ONCE (the CRS forward transform is the
        // expensive part — Critical Rule 5); parameters and timesteps then
        // reuse the fractional grid pixel. `None` = masked.
        let mask = polygon.cell_mask(&axes);
        let gt = data.grid.geo_transform();
        let cell_px: Vec<Option<(f64, f64)>> = axes
            .y
            .iter()
            .enumerate()
            .flat_map(|(iy, &y)| {
                let (gt, axes, mask) = (gt, &axes, &mask);
                axes.x.iter().enumerate().map(move |(ix, &x)| {
                    mask[axes.index(ix, iy)].then(|| world_to_grid_px(gt, x, y))
                })
            })
            .collect();

        let has_time = time_indices.len() > 1;
        let times: Vec<DateTime<Utc>> = time_indices.iter().map(|(_, t)| *t).collect();
        let mut params_map = HashMap::new();
        let mut ranges = HashMap::new();

        for (pi, param) in &param_indices {
            let mut values: Vec<Option<f64>> = Vec::with_capacity(time_indices.len() * ny * nx);
            for (ti, _) in &time_indices {
                values.extend(cell_px.iter().map(|px| {
                    px.and_then(|(col_f, row_f)| {
                        sample_grid_bilinear(&data, col_f, row_f, *pi, 0, *ti)
                    })
                }));
            }
            params_map.insert(
                param.name.clone(),
                ParameterDescription {
                    label: param.name.clone(),
                    unit: String::new(),
                    observed_property: param.name.clone(),
                    standard_name: None,
                },
            );
            let (shape, axis_names) = if has_time {
                (
                    vec![times.len(), ny, nx],
                    vec!["t".to_string(), "y".to_string(), "x".to_string()],
                )
            } else {
                (vec![ny, nx], vec!["y".to_string(), "x".to_string()])
            };
            ranges.insert(
                param.name.clone(),
                NdArray {
                    shape,
                    axis_names,
                    values,
                },
            );
        }

        Ok(CoverageResponse::Single(QueryResult {
            domain: DomainDescription::Grid {
                x: axes.x,
                y: axes.y,
                t: has_time.then_some(times),
                z: None,
            },
            parameters: params_map,
            ranges,
        }))
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
            // A window without a run step has no data (404), as in
            // `query_area`; a datetime list skips such an instant.
            return Err(DataServerError::LocationNotFound(
                "No data available for the requested time range".into(),
            ));
        }

        let times: Vec<DateTime<Utc>> = time_indices.iter().map(|(_, t)| *t).collect();

        let param_indices = select_param_indices(&data, parameters)?;

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
                    standard_name: None,
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

/// The latest run's wind components for `ds_core::wind::DerivedWind`
/// (#897): an `Arc` clone of the snapshot built with the run set.
impl WindSource for QueryDataEngine {
    fn wind_facts(&self) -> Arc<WindFacts> {
        self.runs
            .load()
            .wind
            .clone()
            .unwrap_or_else(WindFacts::none)
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
        // (LCC, stereographic, rotated lat-lon) — is sampled by
        // `world_to_grid_px` (`Crs::forward` + affine) then bilinear.
        // A lat/lon source (`Crs::Wgs84`, forward = identity) under lat/lon
        // or Web Mercator output needs no projection at all: sample per
        // pixel. Every other pairing runs a projection per node — the
        // output's inverse, the source's forward (LCC `powf`/`atan2` for
        // MEPS), or both — so compose them into a coarse `ProjectionGrid` and
        // bilinearly interpolate the output→source pixel map rather than
        // projecting per output pixel (Critical Rule 5; #268, #807).
        // Projected sources are regional, so the grid stays accurate and
        // needs no antimeridian care.
        let gt = data.grid.geo_transform();
        let per_pixel = matches!(output_crs, OutputCrs::Wgs84 | OutputCrs::WebMercator)
            && matches!(gt.crs, ds_core::geo::Crs::Wgs84);
        if per_pixel {
            // The lat/lon grid is sampled in its own longitude frame, one
            // turn from a cell west of its first column (as far west as
            // `sample_grid_bilinear` reaches). A viewport may reach past
            // ±180° (OGC API Maps unwraps a bbox crossing the antimeridian,
            // #828), and such a pixel shows the meridian a turn away: 185°
            // is 175°W. A longitude already in the frame is used as is.
            let frame = (gt.pixel_width > 0.0).then(|| {
                let west = gt.origin_x - 0.5 * gt.pixel_width;
                west..west + 360.0
            });
            let in_frame = |lon: f64| match &frame {
                Some(frame) if !frame.contains(&lon) => {
                    frame.start + (lon - frame.start).rem_euclid(360.0)
                }
                _ => lon,
            };
            for row in 0..height {
                let fy = (row as f64 + 0.5) / height as f64;
                for col in 0..width {
                    let fx = (col as f64 + 0.5) / width as f64;
                    let (lon, lat) = output_crs.project_node(bbox, fx, fy);
                    values.push(interpolate(
                        &data,
                        in_frame(lon),
                        lat,
                        param_idx,
                        0,
                        time_idx,
                    ));
                }
            }
        } else {
            let grid = ds_core::resample::ProjectionGrid::build_2d(
                width,
                height,
                data.grid.nx,
                data.grid.ny,
                |fx, fy| output_crs.project_node(bbox, fx, fy),
                |lon, lat| world_to_grid_px(gt, lon, lat),
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

        Ok(RasterTile {
            width,
            height,
            values: values.into(),
        })
    }

    fn resolve_time(
        &self,
        time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        // The cache-key authority (#507): the exact timestep
        // `get_raster_tile` will render — same run selection (`select_data`)
        // and nearest-neighbour time selection (`find_time_idx`) as the
        // render path. A missing run falls back to the requested time: the
        // render will error and cache nothing, so the key value is moot.
        let Ok(data) = self.select_data(reference_time) else {
            return time;
        };
        find_time_idx(&data, time).map(|i| data.times[i]).or(time)
    }

    fn resolve_reference_time(
        &self,
        _time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        // The run-axis cache-key authority (#521): the exact run
        // `get_raster_tile` will render — the SAME `instances::select_run`
        // rule `select_data` applies (`None` ⇒ latest, `Some` ⇒ exact). A
        // pinned run that's no longer retained echoes back: the render will
        // error and cache nothing.
        let set = self.runs.load();
        ds_core::instances::select_run(&set.runs, reference_time)
            .map(|(rt, _)| Some(*rt))
            .unwrap_or(reference_time)
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
        let bbox = data.grid.bbox();

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

        // QueryData descriptors carry no trustworthy unit metadata. Keep units
        // unknown instead of inferring physical units from names.
        let parameters: Vec<ds_core::map_engine::ParameterInfo> = data
            .params
            .iter()
            .map(|p| {
                // Extract short name from parentheses, e.g., "2 Metre Temperature (2t)" → "2t"
                let short = map_name(&p.name).to_string();
                ds_core::map_engine::ParameterInfo {
                    name: short,
                    title: p.name.clone(),
                    unit: String::new(),
                }
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
///
/// A temporary name ([`temp_files::is_temporary`]) is never listed: a publisher
/// writes a run as a hidden `.name.sqd` and renames it once complete, and a
/// poll in between would parse a truncated file of up to a GB, logging a
/// spurious ERROR (#1009). The finished file is listed on the next poll.
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
                && p.file_name()
                    .is_some_and(|name| !temp_files::is_temporary(&name.to_string_lossy()))
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
///
/// The "most recent" window is taken by **filename sort**, which assumes the
/// standard `…YYYYMMDDHHMM.sqd` naming where lexical order matches origin-time
/// order. This keeps poll cheap — only the window's files are parsed (others are
/// reused from `prev` by path) rather than parsing every file in the directory
/// to read its origin time. A reissued run whose filename sorts out of origin
/// order could thus be windowed wrong; that's an accepted limitation of the
/// naming-convention assumption (the BTreeMap still keys by origin time, and a
/// same-origin collision is logged below).
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
    let wind = runs
        .values()
        .next_back()
        .map(|latest| Arc::new(wind_facts(&latest.data)));
    RunSet { runs, wind }
}

/// What a run states about its wind components (#897): each parameter's
/// FMI number and its map and EDR names, and the frame of its u/v.
///
/// The format has no u/v frame flag (neither the header, a parameter
/// descriptor nor an area class carries one), so the frame is FMI newbase's
/// convention: u and v are relative to the data's own grid. newbase's
/// `NFmiFastQueryInfo::DoWindComponentFix` (smartmet-library-newbase,
/// `newbase/NFmiFastQueryInfo.cpp`) relies on it when it reprojects
/// `kFmiWindUMS`/`kFmiWindVMS` onto another grid: it turns them by the
/// difference of the two areas' `NFmiArea::TrueNorthAzimuth`, which would be
/// wrong for components already along east and north. So the components are
/// grid-relative: on a lat/lon area that is east and north, giving speed and
/// direction; on a rotated lat/lon, stereographic or LCC area `ds_core::wind`
/// gives speed only until it can turn them to true north.
fn wind_facts(data: &QueryData) -> WindFacts {
    WindFacts {
        grid: GridAxes::of(&data.grid.area.crs),
        parameters: data
            .params
            .iter()
            .map(|p| {
                let name = map_name(&p.name);
                ParameterFacts {
                    edr_name: (name != p.name).then(|| p.name.clone()),
                    // Only a wind number asserts a role. Any other id says
                    // nothing about wind, so the name may still pair it by
                    // the source vocabulary (`10u`/`10v` from a converter
                    // that kept GRIB short names but not FMI's numbers).
                    fmi_param: WindRole::from_fmi_param(p.id).map(|_| p.id),
                    // newbase's convention, see above.
                    frame: VectorFrame::Grid,
                    ..ParameterFacts::new(name)
                }
            })
            .collect(),
    }
}

/// A parameter's map name: the short name in parentheses at the end of its
/// descriptor (`"2 Metre Temperature (2t)"` → `"2t"`), else the descriptor.
/// EDR uses the full descriptor.
fn map_name(name: &str) -> &str {
    name.rfind('(')
        .and_then(|start| name[start + 1..].strip_suffix(')'))
        .unwrap_or(name)
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
    let (col_f, row_f) = world_to_grid_px(gt, lon, lat);
    sample_grid_bilinear(data, col_f, row_f, param_idx, level_idx, time_idx)
}

/// The parameters an EDR query addresses, with their indices: every one
/// when `parameters` is absent, else exactly the named ones
/// (`select_parameters`: case-insensitive, an unknown name is a 400), in
/// request order. Shared by position and area so the two cannot drift.
fn select_param_indices<'a>(
    data: &'a QueryData,
    parameters: Option<&[String]>,
) -> Result<Vec<(usize, &'a crate::parse::ParamInfo)>, DataServerError> {
    let names: Vec<&str> = data.params.iter().map(|p| p.name.as_str()).collect();
    let selected = ds_core::edr_engine::select_parameters(parameters, &names)?;
    if selected.is_empty() {
        return Err(DataServerError::InvalidParameter(
            "No parameters available".into(),
        ));
    }
    Ok(selected
        .into_iter()
        .filter_map(|name| data.params.iter().enumerate().find(|(_, p)| p.name == name))
        .collect())
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

/// Parse EDR position query coordinates as `(lat, lon)`: the shared ds-core
/// parser, so a direct engine call gets the same finite and range checks
/// as the HTTP boundary (#534).
fn parse_coords(coords: &str) -> Result<(f64, f64), DataServerError> {
    ds_core::feature::parse_point_coords(coords)
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

    /// A direct `query_position` gets the HTTP boundary's coordinate checks
    /// (#534): non-finite and out-of-range points are a 400, never sampled.
    #[test]
    fn position_rejects_non_finite_and_out_of_range_coordinates() {
        assert!(test_file_exists(), "ecmwf-kenya fixture missing");
        let engine = QueryDataEngine::new(&test_dir(), "test", None, 30, 4).unwrap();
        for bad in [
            "POINT(NaN -1.3)",
            "POINT(36.8 inf)",
            "POINT(200 -1.3)",
            "36.8,-95",
        ] {
            assert!(
                matches!(
                    engine.query_position(bad, None, None, None, None),
                    Err(DataServerError::InvalidParameter(_))
                ),
                "{bad}"
            );
        }
    }

    fn meps_engine() -> QueryDataEngine {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/meps");
        assert!(
            dir.exists() && !list_sqd_files(&dir).is_empty(),
            "meps fixture missing"
        );
        QueryDataEngine::new(&dir, "test", None, 30, 4).unwrap()
    }

    #[test]
    fn engine_spatial_extent_lcc() {
        // The projected (LCC) rectangle's edges, not its stored corners'
        // lon/lat box: the northern edge bows out to 65.1°N and the north-east
        // corner reaches 19.95°E, past the stored corners' 64.96°N / 19.13°E.
        // Reference: the half-cell-padded rectangle's edges inverse-projected
        // with `cs2cs +proj=lcc +lat_1=63.3 +lat_2=63.3 +lat_0=63.3 +lon_0=15
        // +R=6371220 +to +proj=longlat +R=6371220` (PROJ 9.x) — the area's
        // own sphere.
        let bbox = meps_engine().get_spatial_extent().unwrap();
        for (got, want, edge) in [
            (bbox[0], 8.985862, "west"),
            (bbox[1], 59.963104, "south"),
            (bbox[2], 19.951358, "east"),
            (bbox[3], 65.097357, "north"),
        ] {
            assert!((got - want).abs() < 1e-3, "{edge} {got}, PROJ {want}");
        }
    }

    /// The exact per-pixel render of `bbox` — the path lat/lon and Web
    /// Mercator output of a projected source took before #807.
    fn per_pixel_reference(
        engine: &QueryDataEngine,
        bbox: [f64; 4],
        (width, height): (u32, u32),
        crs: &OutputCrs,
    ) -> Vec<Option<f64>> {
        let data = engine.latest_data().unwrap();
        let param = engine.resolve_map_param_idx(&data);
        let time = find_time_idx(&data, None).unwrap();
        let mut values = Vec::with_capacity((width * height) as usize);
        for row in 0..height {
            for col in 0..width {
                let (fx, fy) = (
                    (col as f64 + 0.5) / width as f64,
                    (row as f64 + 0.5) / height as f64,
                );
                let (lon, lat) = crs.project_node(bbox, fx, fy);
                values.push(interpolate(&data, lon, lat, param, 0, time));
            }
        }
        values
    }

    /// A projected (MEPS LCC) source under lat/lon or Web Mercator output
    /// renders through the coarse `ProjectionGrid`, not a source forward
    /// projection per output pixel (#807), and agrees with the exact
    /// per-pixel mapping within the grid's error budget.
    #[test]
    fn meps_grid_render_matches_the_per_pixel_mapping() {
        let engine = meps_engine();
        let bbox = [10.0, 60.5, 19.0, 64.5];
        let size = (256, 256);
        for crs in [OutputCrs::Wgs84, OutputCrs::WebMercator] {
            let tile = engine
                .get_raster_tile(bbox, size.0, size.1, None, &crs, None, None, None)
                .unwrap();
            let reference = per_pixel_reference(&engine, bbox, size, &crs);
            let (mut both, mut presence, mut max_diff) = (0, 0, 0.0_f64);
            for (i, exact) in reference.iter().enumerate() {
                match (tile.values.value_at(i), exact) {
                    (Some(got), Some(want)) => {
                        both += 1;
                        max_diff = max_diff.max((got - want).abs());
                    }
                    (None, None) => {}
                    _ => presence += 1,
                }
            }
            // Coverage differs only along the grid's edge; values within a
            // fraction of a source cell's gradient.
            assert!(both > 50_000, "{crs:?}: {both} pixels with data");
            assert!(
                presence <= 2 * (size.0 + size.1) as usize,
                "{crs:?}: {presence}"
            );
            assert!(max_diff < 0.25, "{crs:?}: max difference {max_diff}");
        }
    }

    /// Timing of a 256² MEPS tile, grid path against the per-pixel
    /// reference: `cargo test --release -p engine-querydata
    /// meps_render_timing -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn meps_render_timing() {
        let engine = meps_engine();
        let bbox = [10.0, 60.5, 19.0, 64.5];
        for crs in [OutputCrs::Wgs84, OutputCrs::WebMercator] {
            let runs = 50;
            let started = std::time::Instant::now();
            for _ in 0..runs {
                std::hint::black_box(per_pixel_reference(&engine, bbox, (256, 256), &crs));
            }
            let per_pixel = started.elapsed() / runs;
            let started = std::time::Instant::now();
            for _ in 0..runs {
                std::hint::black_box(
                    engine
                        .get_raster_tile(bbox, 256, 256, None, &crs, None, None, None)
                        .unwrap(),
                );
            }
            let grid = started.elapsed() / runs;
            println!("{crs:?}: per-pixel {per_pixel:?}, grid {grid:?}");
        }
    }

    #[test]
    fn meps_rows_run_south_to_north() {
        // The fixture stores its north-west corner first, but its data starts
        // in the south: read that way, the evening's mild Bothnian Sea lies
        // east of the Swedish coast and the colder Västerbotten interior
        // north of it. Read north-first, the two swap.
        let engine = meps_engine();
        let data = engine.latest_data().unwrap();
        let at = |lon, lat| interpolate(&data, lon, lat, 0, 0, 0).unwrap();
        let (sea, inland) = (at(18.8, 61.3), at(18.3, 64.3));
        assert!(sea - inland > 2.0, "Bothnian Sea {sea} vs inland {inland}");
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

    /// A `Trajectory` node `(t, lon, lat)` and one parameter's value there.
    type TrajectorySample = ((DateTime<Utc>, f64, f64), Option<f64>);

    /// The `(node, value)` pairs of one parameter of a `Trajectory` coverage.
    fn trajectory_samples(qr: &QueryResult, param: &str) -> Vec<TrajectorySample> {
        let DomainDescription::Trajectory { nodes, .. } = &qr.domain else {
            panic!("expected a Trajectory domain")
        };
        let range = &qr.ranges[param];
        assert_eq!(range.axis_names, ["composite"]);
        assert_eq!(range.shape, [nodes.len()]);
        nodes
            .iter()
            .copied()
            .zip(range.values.iter().copied())
            .collect()
    }

    /// Every trajectory sample equals a position query at its node and
    /// timestep: the same projection and bilinear interpolation (#926).
    fn assert_matches_position(engine: &QueryDataEngine, qr: &QueryResult, param: &str) {
        for ((t, lon, lat), value) in trajectory_samples(qr, param) {
            let CoverageResponse::Single(position) = engine
                .query_position(
                    &format!("POINT({lon} {lat})"),
                    Some((t, t)),
                    Some(&[param.to_string()]),
                    None,
                    None,
                )
                .unwrap()
            else {
                panic!("expected Single")
            };
            assert_eq!(
                value, position.ranges[param].values[0],
                "{param} at {lon},{lat} {t}"
            );
        }
    }

    #[test]
    fn trajectory_samples_the_run_along_the_path() {
        assert!(test_file_exists(), "ecmwf-kenya fixture missing");
        let engine = QueryDataEngine::new(&test_dir(), "test", None, 30, 4).unwrap();
        assert!(engine
            .supported_query_types()
            .contains(&"trajectory".to_string()));
        let param = "2 Metre Temperature (2t)";
        let (first, last) = engine.get_temporal_extent().unwrap();

        // A 2-D path at one step: a single coverage densified on the
        // fixture's grid, every sample a real value inside the extent.
        let CoverageResponse::Single(qr) = engine
            .query_trajectory(
                "LINESTRING(35 -2, 37 0, 39 1)",
                Some((first, first)),
                None,
                None,
                None,
            )
            .unwrap()
        else {
            panic!("one step → one coverage")
        };
        assert_eq!(qr.parameters.len(), 3);
        let samples = trajectory_samples(&qr, param);
        assert!(samples.len() > 3, "densified: {}", samples.len());
        assert!(samples.iter().all(|(n, v)| n.0 == first && v.is_some()));
        assert_matches_position(&engine, &qr, param);

        // An M path over the whole run: samples snap to the run's steps.
        let coords = format!(
            "LINESTRING M(35 -2 {}, 39 1 {})",
            first.timestamp(),
            last.timestamp()
        );
        let CoverageResponse::Single(qr) = engine
            .query_trajectory(&coords, None, Some(&[param.to_string()]), None, None)
            .unwrap()
        else {
            panic!("an M path is one coverage")
        };
        let samples = trajectory_samples(&qr, param);
        assert_eq!(samples.first().unwrap().0 .0, first);
        assert_eq!(samples.last().unwrap().0 .0, last);
        assert!(samples.windows(2).all(|w| w[0].0 .0 <= w[1].0 .0));
        assert_matches_position(&engine, &qr, param);

        // Outside the run's time range → 400; outside the grid → 404.
        let late = format!(
            "LINESTRING M(35 -2 {}, 39 1 {})",
            first.timestamp(),
            last.timestamp() + 86_400
        );
        assert!(matches!(
            engine.query_trajectory(&late, None, None, None, None),
            Err(DataServerError::InvalidParameter(_))
        ));
        assert!(matches!(
            engine.query_trajectory("LINESTRING(10 50, 11 51)", None, None, None, None),
            Err(DataServerError::LocationNotFound(_))
        ));
    }

    /// The projected (LCC) MEPS crop (9–20°E, 60–65°N): samples along a
    /// path across Sweden at its few-km cell size, projected per sample
    /// like position.
    #[test]
    fn trajectory_on_a_projected_grid_matches_position() {
        let engine = meps_engine();
        let (first, _) = engine.get_temporal_extent().unwrap();
        let param = engine.get_parameters()[0].clone();
        let CoverageResponse::Single(qr) = engine
            .query_trajectory(
                "LINESTRING(12 61, 17 63.5)",
                Some((first, first)),
                Some(std::slice::from_ref(&param)),
                None,
                None,
            )
            .unwrap()
        else {
            panic!("one step → one coverage")
        };
        let samples = trajectory_samples(&qr, &param);
        // ~380 km at a few-km mean cell size: densified well past the two
        // vertices, and inside the domain.
        assert!(samples.len() > 20, "{}", samples.len());
        assert!(samples.iter().any(|(_, v)| v.is_some()));
        assert_matches_position(&engine, &qr, &param);
    }

    #[test]
    fn engine_area_query_masks_outside_the_polygon() {
        assert!(test_file_exists(), "ecmwf-kenya fixture missing");
        let engine = QueryDataEngine::new(&test_dir(), "test", None, 30, 4).unwrap();
        let [w, s, e, n] = engine.get_spatial_extent().unwrap();
        // A triangle inside the extent: its bbox's north-east corner cell is
        // outside the shape, its centroid inside.
        let (x0, x1) = (w + 0.3 * (e - w), w + 0.6 * (e - w));
        let (y0, y1) = (s + 0.3 * (n - s), s + 0.6 * (n - s));
        let coords = format!("POLYGON(({x0} {y0}, {x1} {y0}, {x0} {y1}, {x0} {y0}))");
        let (start, _) = engine.get_temporal_extent().unwrap();
        // Pin the parameter: `ranges` is a HashMap, and the precipitation
        // field is legitimately nodata over part of the fixture, so picking
        // an arbitrary range made this test order-dependent (flaked on CI).
        let param = vec!["2 Metre Temperature (2t)".to_string()];
        let resp = engine
            .query_area(&coords, Some((start, start)), Some(&param), None, None)
            .unwrap();
        let CoverageResponse::Single(res) = resp else {
            panic!("expected a single Grid coverage");
        };
        let DomainDescription::Grid { x, y, t, .. } = &res.domain else {
            panic!("expected a Grid domain");
        };
        assert!(t.is_none(), "one timestep → no t axis");
        assert!(x.len() > 2 && y.len() > 2, "grid {}×{}", x.len(), y.len());
        assert!(x.windows(2).all(|p| p[0] < p[1]) && y.windows(2).all(|p| p[0] > p[1]));
        let arr = &res.ranges[&param[0]];
        assert_eq!(arr.shape, vec![y.len(), x.len()]);
        assert_eq!(arr.values.len(), y.len() * x.len());
        // North-east corner (row 0, last col) is outside the triangle.
        assert!(arr.values[x.len() - 1].is_none());
        // South-west corner cell (last row, col 0) is inside and has data.
        assert!(arr.values[(y.len() - 1) * x.len()].is_some());
        let inside = arr.values.iter().filter(|v| v.is_some()).count();
        let total = arr.values.len();
        assert!(
            inside * 3 > total && inside * 3 < total * 2,
            "a right triangle fills about half its bbox: {inside}/{total}"
        );
    }

    #[test]
    fn engine_area_query_outside_extent_is_not_found() {
        assert!(test_file_exists(), "ecmwf-kenya fixture missing");
        let engine = QueryDataEngine::new(&test_dir(), "test", None, 30, 4).unwrap();
        let err = engine
            .query_area(
                "POLYGON((100 10, 101 10, 101 11, 100 11, 100 10))",
                None,
                None,
                None,
                None,
            )
            .unwrap_err();
        assert!(matches!(err, DataServerError::LocationNotFound(_)), "{err}");
        // And a datetime window without a run step has no data (404) on
        // every path, so a datetime list skips that instant instead of
        // failing (EDR 1.2 /req/core/datetime-response A).
        let far: DateTime<Utc> = "2000-01-01T00:00:00Z".parse().unwrap();
        for r in [
            engine.query_area("36,-2,38,0", Some((far, far)), None, None, None),
            engine.query_position("POINT(36.8 -1.3)", Some((far, far)), None, None, None),
            engine.query_trajectory(
                "LINESTRING(36 -2, 38 0)",
                Some((far, far)),
                None,
                None,
                None,
            ),
        ] {
            assert!(
                matches!(r, Err(DataServerError::LocationNotFound(_))),
                "{r:?}"
            );
        }
    }

    #[test]
    fn engine_area_query_with_time_axis_and_budget() {
        assert!(test_file_exists(), "ecmwf-kenya fixture missing");
        let engine = QueryDataEngine::new(&test_dir(), "test", None, 30, 4).unwrap();
        let [w, s, e, n] = engine.get_spatial_extent().unwrap();
        let coords = format!(
            "POLYGON(({w} {s}, {e} {s}, {e} {n}, {w} {n}, {w} {s}))",
            w = w + 0.4 * (e - w),
            e = w + 0.5 * (e - w),
            s = s + 0.4 * (n - s),
            n = s + 0.5 * (n - s)
        );
        let params = vec![engine.get_parameters()[0].clone()];
        let resp = engine
            .query_area(&coords, None, Some(&params), None, None)
            .unwrap();
        let CoverageResponse::Single(res) = resp else {
            panic!("expected a single Grid coverage");
        };
        let DomainDescription::Grid { x, y, t, .. } = &res.domain else {
            panic!("expected a Grid domain");
        };
        let t = t.as_ref().expect("all timesteps → t axis");
        let arr = &res.ranges[&params[0]];
        assert_eq!(arr.shape, vec![t.len(), y.len(), x.len()]);
        assert_eq!(arr.axis_names, vec!["t", "y", "x"]);
        // Whole extent × every timestep × every parameter blows the budget.
        let whole = format!("POLYGON(({w} {s}, {e} {s}, {e} {n}, {w} {n}, {w} {s}))");
        let big = engine.query_area(&whole, None, None, None, None);
        match big {
            Err(DataServerError::QueryTooLarge(_)) => {}
            Ok(r) => {
                // Small fixtures may fit; then the response must still be sane.
                let CoverageResponse::Single(r) = r else {
                    panic!()
                };
                assert!(r
                    .ranges
                    .values()
                    .all(|a| a.values.len() == a.shape.iter().product::<usize>()));
            }
            Err(e) => panic!("unexpected error {e}"),
        }
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

    /// #828: a map viewport reaching past ±180°, as OGC API Maps sends a bbox
    /// crossing the antimeridian, samples a lat/lon grid at the meridian
    /// each pixel shows: the Kenya grid a turn east or west renders the
    /// same pixels as in place.
    #[test]
    fn map_viewport_a_turn_away_renders_the_same_lat_lon_grid() {
        assert!(test_file_exists(), "ecmwf-kenya fixture missing");
        let engine =
            QueryDataEngine::new(&test_dir(), "test", Some("2 Metre Temperature (2t)"), 30, 4)
                .unwrap();
        let render = |west: f64, east: f64| {
            let tile = engine
                .get_raster_tile(
                    [west, -5.0, east, 5.0],
                    16,
                    16,
                    None,
                    &OutputCrs::Wgs84,
                    None,
                    None,
                    None,
                )
                .unwrap();
            tile.values.iter_values().collect::<Vec<_>>()
        };
        let in_place = render(33.0, 42.0);
        let filled = in_place.iter().filter(|v| v.is_some()).count();
        assert!(
            filled > in_place.len() / 2,
            "{filled} of {}",
            in_place.len()
        );
        for (west, east) in [(393.0, 402.0), (-327.0, -318.0)] {
            let shifted = render(west, east);
            assert_eq!(shifted.len(), in_place.len());
            for (i, (a, b)) in in_place.iter().zip(&shifted).enumerate() {
                match (a, b) {
                    (Some(a), Some(b)) => {
                        assert!((a - b).abs() < 1e-9, "{west}..{east} #{i}: {a} vs {b}")
                    }
                    (None, None) => {}
                    _ => panic!("{west}..{east} #{i}: {a:?} vs {b:?}"),
                }
            }
        }
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
        let non_none = tile.values.iter_values().filter(|v| v.is_some()).count();
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
            tile.values.iter_values().filter(|v| v.is_some()).count() > 0,
            "projected build_2d tile should have data"
        );
        assert!(
            tile.values.iter_values().flatten().all(|v| v.is_finite()),
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

    const FIXTURE: &str = "202604042019_202604040600_ecmwf_kenya_surface.sqd";

    /// A publisher's in-progress run, `.name.sqd` holding the first bytes of
    /// the file, next to a finished one: only the finished run is listed
    /// (#1009). Temp-suffixed copies are skipped too.
    #[test]
    fn list_sqd_files_skips_in_progress_names() {
        assert!(test_file_exists(), "ecmwf-kenya fixture missing");
        let bytes = std::fs::read(test_dir().join(FIXTURE)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let finished = dir.path().join(FIXTURE);
        std::fs::write(&finished, &bytes).unwrap();
        let hidden = format!(".2026-10-09T00:00:00Z_{FIXTURE}");
        std::fs::write(dir.path().join(&hidden), &bytes[..bytes.len() / 10]).unwrap();
        std::fs::write(dir.path().join(format!("{FIXTURE}.tmp")), &bytes).unwrap();
        std::fs::write(dir.path().join(format!("{FIXTURE}.part")), &bytes).unwrap();

        assert_eq!(list_sqd_files(dir.path()), [finished]);
    }

    /// A hidden run is never parsed, even when all its bytes are there: the
    /// engine finds nothing to load in a directory holding only `.name.sqd`.
    #[test]
    fn engine_never_loads_a_hidden_run() {
        assert!(test_file_exists(), "ecmwf-kenya fixture missing");
        let dir = tempfile::tempdir().unwrap();
        std::fs::copy(
            test_dir().join(FIXTURE),
            dir.path().join(format!(".{FIXTURE}")),
        )
        .unwrap();
        let Err(err) = QueryDataEngine::new(dir.path(), "test", None, 30, 4) else {
            panic!("a hidden .sqd must not be loaded as a run");
        };
        assert!(err.to_string().contains("No loadable .sqd files"), "{err}");
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

#[cfg(test)]
mod wind_tests;
