//! CF-NetCDF gridded forecasts and analyses (#1067), served through WMS,
//! OGC API Maps and Tiles.
//!
//! One collection is one file family, such as MEPS's per-lead surface files:
//! every file carries the collection's variables on one grid, its valid
//! times in `time` and its model run in `forecast_reference_time`. The poll
//! loop lists the data directory, opens each new file once and keeps its
//! handle while the catalog keeps the file ([`catalog::OpenFile`]); a render
//! reads one 2-D field through the engine-wide field cache ([`field`]) and
//! resamples it with `ds_core::resample::SourceGrid`.

mod catalog;
pub mod decode_budget;
mod discover;
mod field;
mod ingest;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use ds_core::config::NetcdfConfig;
use ds_core::error::DataServerError;
use ds_core::geo::GeoTransform;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterInfo, RasterTile};
use ds_core::resample::SourceGrid;
use ds_poll::{FirstTick, Shutdown};
use ds_storage::discovery::FilenameMatcher;

pub use field::field_cache_metrics;

use catalog::{Catalog, Identity, Layout, OpenFile, Run, RunKey, Step};
use discover::{Listed, Names};
use ingest::{Inspected, Wanted};

/// Files one poll opens: new and replaced files, newest first, then
/// retained files to open again. A directory of many files is opened over
/// several polls instead of in one long sequential pass (Critical Rule 9).
const MAX_OPENS_PER_POLL: usize = 128;

pub struct NetcdfEngine {
    collection_id: Arc<str>,
    dir: PathBuf,
    names: Names,
    variables: Option<Vec<String>>,
    titles: BTreeMap<String, String>,
    member: u32,
    max_runs: usize,
    max_files: usize,
    poll_interval: Duration,
    catalog: ArcSwap<Catalog>,
    /// What the poll knows about the directory; only the poll takes it.
    state: Mutex<PollState>,
    /// Fields this engine decoded (cache fills), for tests and diagnostics.
    decodes: AtomicU64,
    shutdown: Shutdown,
}

/// What the poll remembers between polls.
struct PollState {
    known: HashMap<PathBuf, Known>,
    layout: Option<Arc<Layout>>,
    content_version: u64,
    /// Whether the mixed runs and run-less files WARN was logged.
    warned_mixed: bool,
}

/// A listed file as the poll last left it.
enum Known {
    /// Ingested, open, and catalogued if the retention keeps it.
    Open(Arc<OpenFile>),
    /// Ingested once, then closed: outside the runs or files kept. Opened
    /// again if it comes back into them.
    Closed {
        identity: Identity,
        run: Option<DateTime<Utc>>,
        newest: DateTime<Utc>,
    },
    /// Not servable; why was logged when it was found.
    Skipped { identity: Identity },
}

impl Known {
    fn identity(&self) -> Identity {
        match self {
            Known::Open(file) => file.identity,
            Known::Closed { identity, .. } | Known::Skipped { identity } => *identity,
        }
    }

    /// The run and newest valid time of an ingested file.
    fn place(&self) -> Option<(Option<DateTime<Utc>>, DateTime<Utc>)> {
        match self {
            Known::Open(file) => Some((file.run, *file.times.iter().max()?)),
            Known::Closed { run, newest, .. } => Some((*run, *newest)),
            Known::Skipped { .. } => None,
        }
    }
}

/// A new process-unique ingest id ([`OpenFile::id`]).
fn next_file_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

impl NetcdfEngine {
    /// Build the engine and scan the directory once, as engine-grib does, so
    /// the layers and their styles exist when the collection is registered.
    /// A failed scan logs a WARN and the poll retries it.
    pub fn new(collection_id: &str, config: &NetcdfConfig) -> Result<Self, DataServerError> {
        ds_core::config::validate_netcdf(collection_id, config)?;
        let names = match config.filename_template.as_deref() {
            Some(template) => {
                Names::Template(FilenameMatcher::from_template(template).map_err(|e| {
                    DataServerError::Config(format!(
                        "Collection '{collection_id}': [netcdf].filename_template: {e}"
                    ))
                })?)
            }
            None => Names::NetcdfSuffix,
        };
        // Nonzero and new on every start, so a browser's cached explicit-time
        // images revalidate after a restart that may have replaced files.
        let content_version = chrono::Utc::now().timestamp().max(1) as u64;
        let engine = Self {
            collection_id: collection_id.into(),
            dir: PathBuf::from(config.data_path.as_deref().unwrap_or_default()),
            names,
            variables: config.variables.clone(),
            titles: config.titles.clone(),
            member: config.member,
            max_runs: config.max_runs,
            max_files: config.max_files,
            poll_interval: Duration::from_secs(config.poll_interval_secs),
            catalog: ArcSwap::from_pointee(Catalog::empty(content_version)),
            state: Mutex::new(PollState {
                known: HashMap::new(),
                layout: None,
                content_version,
                warned_mixed: false,
            }),
            decodes: AtomicU64::new(0),
            shutdown: Shutdown::new(),
        };
        if let Err(e) = engine.scan() {
            tracing::warn!(
                "Collection '{collection_id}': initial NetCDF scan failed (will retry on poll): {e}"
            );
        }
        Ok(engine)
    }

    /// Poll until [`Self::shutdown`]. Run on the background poll runtime
    /// (Critical Rule 6): each tick lists the directory and opens files.
    /// [`Self::new`] has made the first scan.
    pub async fn poll_loop(&self) {
        let mut ticker = self.shutdown.ticker(self.poll_interval, FirstTick::Skip);
        while ticker.tick().await {
            self.poll_once();
        }
        tracing::info!("[{}] netcdf poll loop shutting down", self.collection_id);
    }

    pub fn shutdown(&self) {
        self.shutdown.shutdown();
    }

    pub fn collection_id(&self) -> &str {
        &self.collection_id
    }

    /// Live `/health` status: ready while the directory lists and the
    /// catalog has data.
    pub fn live_health(&self) -> Option<ds_core::health::LiveStatus> {
        use ds_core::health::LiveStatus;
        let catalog = self.catalog.load();
        let Some(polled_at) = catalog.polled_at else {
            return Some(LiveStatus::Degraded {
                reason: "waiting for the first poll",
            });
        };
        let stale_after = chrono::Duration::from_std(self.poll_interval * 5)
            .unwrap_or(chrono::Duration::MAX)
            .max(chrono::Duration::minutes(10));
        Some(if Utc::now() - polled_at > stale_after {
            LiveStatus::Degraded {
                reason: "listing the data directory has failed repeatedly",
            }
        } else if catalog.runs.is_empty() {
            LiveStatus::Degraded {
                reason: "no servable NetCDF files found",
            }
        } else {
            LiveStatus::Ready
        })
    }

    /// Age of the newest data, for `/health`'s `data_age_secs`: the newest
    /// run's reference time, or the newest valid time without runs.
    pub fn data_age(&self) -> Option<chrono::Duration> {
        let catalog = self.catalog.load();
        let newest = match catalog.runs.last_key_value()? {
            (Some(reference_time), _) => *reference_time,
            (None, run) => *run.steps.last_key_value()?.0,
        };
        Some(Utc::now() - newest)
    }

    /// Every served valid time, ascending.
    pub fn times(&self) -> Vec<DateTime<Utc>> {
        self.catalog.load().info.times.clone()
    }

    /// The grid fields are rendered from, north-up, once a file is ingested.
    pub fn grid(&self) -> Option<GeoTransform> {
        let catalog = self.catalog.load();
        Some(catalog.layout.as_ref()?.grid.transform.clone())
    }

    /// Fields this engine has decoded, every cache fill counted once.
    pub fn field_decodes(&self) -> u64 {
        self.decodes.load(Ordering::Relaxed)
    }

    /// List the directory, open new and replaced files, keep the newest
    /// runs (or files) and publish the catalog. A directory that cannot be
    /// listed keeps the catalog served.
    pub fn poll_once(&self) {
        if let Err(e) = self.scan() {
            tracing::warn!("[{}] NetCDF poll failed: {e}", self.collection_id);
        }
    }

    /// [`Self::poll_once`]; `Err` when the directory cannot be listed.
    fn scan(&self) -> Result<(), String> {
        let listed = discover::list(&self.dir, &self.names, &self.collection_id)
            .map_err(|e| format!("cannot list {}: {e}", self.dir.display()))?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let present: HashSet<&PathBuf> = listed.iter().map(|l| &l.path).collect();
        state.known.retain(|path, _| present.contains(path));

        let mut opens = MAX_OPENS_PER_POLL;
        let mut deferred = 0usize;
        for file in &listed {
            match state.known.get(&file.path) {
                Some(known) if known.identity() == file.identity => continue,
                // Replaced in place: its old fields may be cached and drawn.
                Some(Known::Open(_) | Known::Closed { .. }) => {
                    state.content_version += 1;
                    tracing::info!(
                        "[{}] {} changed on disk, reading it again",
                        self.collection_id,
                        file.path.display()
                    );
                }
                _ => {}
            }
            if opens == 0 {
                state.known.remove(&file.path);
                deferred += 1;
                continue;
            }
            opens -= 1;
            let known = self.ingest(file, &mut state);
            state.known.insert(file.path.clone(), known);
        }

        let keep = self.retained(&mut state);
        let paths: Vec<PathBuf> = state.known.keys().cloned().collect();
        for path in paths {
            let kept = keep.contains(&path);
            match state.known.get(&path) {
                Some(Known::Open(file)) if !kept => {
                    let closed = Known::Closed {
                        identity: file.identity,
                        run: file.run,
                        newest: file.times.iter().max().copied().unwrap_or_default(),
                    };
                    state.known.insert(path, closed);
                }
                Some(Known::Closed { identity, .. }) if kept => {
                    if opens == 0 {
                        deferred += 1;
                        continue;
                    }
                    opens -= 1;
                    let file = Listed {
                        path: path.clone(),
                        identity: *identity,
                    };
                    let known = self.ingest(&file, &mut state);
                    state.known.insert(path, known);
                }
                _ => {}
            }
        }
        if deferred > 0 {
            tracing::info!(
                "[{}] opened {MAX_OPENS_PER_POLL} files this poll, {deferred} left for the next",
                self.collection_id
            );
        }

        let catalog = self.build_catalog(&state, &keep);
        tracing::debug!(
            "[{}] poll: {} listed, {} runs, {} times",
            self.collection_id,
            listed.len(),
            catalog.runs.len(),
            catalog.info.times.len()
        );
        self.catalog.store(Arc::new(catalog));
        Ok(())
    }

    /// Open and inspect `file` against the collection's layout, setting the
    /// layout from it when there is none yet.
    fn ingest(&self, file: &Listed, state: &mut PollState) -> Known {
        let skipped = |reason: String| {
            tracing::warn!(
                "[{}] skipping {}: {reason}",
                self.collection_id,
                file.path.display()
            );
            Known::Skipped {
                identity: file.identity,
            }
        };
        let (nc, storage) = match ingest::open(&file.path) {
            Ok(opened) => opened,
            Err(e) => return skipped(e),
        };
        let layout_names: Vec<String>;
        let wanted = match (&self.variables, &state.layout) {
            (Some(names), _) => Wanted::Listed(names),
            (None, Some(layout)) => {
                layout_names = layout
                    .variables
                    .iter()
                    .map(|v| v.name.to_string())
                    .collect();
                Wanted::Listed(&layout_names)
            }
            (None, None) => Wanted::All,
        };
        let inspected = match ingest::inspect(&nc, &storage, &wanted, self.member) {
            Ok(inspected) => inspected,
            Err(e) => return skipped(e),
        };
        if inspected.times.is_empty() {
            return skipped("its time axis is empty".into());
        }
        if let Err(e) = self.fits_layout(&inspected, state) {
            return skipped(e);
        }
        // Warm what a request's read uses: the dataset lookup, and the chunk
        // index walked once without reading a chunk. A contiguous variable
        // or a classic file has no index.
        for field in &inspected.fields {
            if let Err(e) = nc.iter_variable_chunks(&field.read.name) {
                tracing::debug!(
                    "[{}] {}: no chunk index for '{}': {e}",
                    self.collection_id,
                    file.path.display(),
                    field.read.name
                );
            }
        }
        Known::Open(Arc::new(OpenFile {
            id: next_file_id(),
            path: file.path.clone(),
            identity: file.identity,
            nc,
            run: inspected.run,
            times: inspected.times,
            reads: inspected.fields.into_iter().map(|f| f.read).collect(),
        }))
    }

    /// Check `inspected` against the collection's layout, or make it the
    /// layout when there is none yet.
    fn fits_layout(&self, inspected: &Inspected, state: &mut PollState) -> Result<(), String> {
        let Some(layout) = &state.layout else {
            for (name, reason) in &inspected.skipped {
                tracing::warn!(
                    "[{}] not serving variable '{name}': it {reason}",
                    self.collection_id
                );
            }
            if inspected.fields.is_empty() {
                return Err("has no servable variable".into());
            }
            for field in &inspected.fields {
                let read = &field.read;
                if read.chunk_bytes > read.field_bytes {
                    tracing::warn!(
                        "[{}] variable '{}' is chunked across {} fields: each field read \
                         inflates them all",
                        self.collection_id,
                        read.name,
                        read.chunk_bytes / read.field_bytes.max(1)
                    );
                }
            }
            let t = &inspected.grid.transform;
            tracing::info!(
                "[{}] serving {} variables on a {} x {} {} grid{}",
                self.collection_id,
                inspected.fields.len(),
                t.width,
                t.height,
                match t.crs {
                    ds_core::geo::Crs::Wgs84 => "latitude/longitude",
                    ds_core::geo::Crs::LambertConformalConic { .. } => "Lambert conformal conic",
                    ds_core::geo::Crs::Geostationary { .. } => "geostationary",
                    _ => "projected",
                },
                if inspected.grid.flipped {
                    " (rows stored south to north)"
                } else {
                    ""
                }
            );
            state.layout = Some(Arc::new(Layout {
                variables: inspected
                    .fields
                    .iter()
                    .map(|f| f.variable.clone())
                    .collect(),
                source: SourceGrid::new(inspected.grid.transform.clone()),
                grid: inspected.grid.clone(),
            }));
            return Ok(());
        };
        if !layout.grid.same(&inspected.grid) {
            return Err("is on another grid than the collection's".into());
        }
        for (served, field) in layout.variables.iter().zip(&inspected.fields) {
            let found = &field.variable;
            if (&found.units, &found.levels) != (&served.units, &served.levels) {
                return Err(format!(
                    "variable '{}' has units '{}' at levels {:?}, the collection's has '{}' at {:?}",
                    served.name, found.units, found.levels, served.units, served.levels
                ));
            }
        }
        Ok(())
    }

    /// The ingested files the catalog keeps: the newest `max_runs` runs, as
    /// many of them, newest first, as `max_files` holds; without runs, the
    /// `max_files` files with the newest times. A directory mixing files
    /// with and without a run keeps the runs.
    fn retained(&self, state: &mut PollState) -> HashSet<PathBuf> {
        // Per run, each file's newest valid time and path.
        let mut groups: BTreeMap<RunKey, Vec<(DateTime<Utc>, &PathBuf)>> = BTreeMap::new();
        for (path, known) in &state.known {
            if let Some((run, newest)) = known.place() {
                groups.entry(run).or_default().push((newest, path));
            }
        }
        let has_runs = groups.keys().any(Option::is_some);
        if has_runs && groups.remove(&None).is_some() && !state.warned_mixed {
            state.warned_mixed = true;
            tracing::warn!(
                "[{}] the directory mixes files with and without forecast_reference_time; \
                 serving only those with one",
                self.collection_id
            );
        }
        let mut keep = HashSet::new();
        let runs = groups
            .into_values()
            .rev()
            .take(if has_runs { self.max_runs } else { 1 });
        for mut files in runs {
            let room = self.max_files - keep.len();
            if room == 0 {
                break;
            }
            if files.len() > room {
                if !keep.is_empty() {
                    break;
                }
                // The newest run alone: its earliest times, or without
                // runs the newest files.
                if has_runs {
                    files.sort();
                } else {
                    files.sort_by(|a, b| b.cmp(a));
                }
                tracing::warn!(
                    "[{}] keeping {room} of {} files: max_files",
                    self.collection_id,
                    files.len()
                );
                files.truncate(room);
            }
            keep.extend(files.into_iter().map(|(_, path)| path.clone()));
        }
        keep
    }

    /// The catalog of the retained open files.
    fn build_catalog(&self, state: &PollState, keep: &HashSet<PathBuf>) -> Catalog {
        let mut runs: BTreeMap<RunKey, Run> = BTreeMap::new();
        // Of two files with a step at one valid time in one run, the greater
        // path wins, whatever the listing order.
        let mut files: Vec<&Arc<OpenFile>> = state
            .known
            .iter()
            .filter(|(path, _)| keep.contains(*path))
            .filter_map(|(_, known)| match known {
                Known::Open(file) => Some(file),
                _ => None,
            })
            .collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        for file in files {
            let run = runs.entry(file.run).or_insert_with(|| Run {
                reference_time: file.run,
                steps: BTreeMap::new(),
            });
            for (index, &valid) in file.times.iter().enumerate() {
                run.steps.insert(
                    valid,
                    Step {
                        file: file.clone(),
                        index: index as u32,
                    },
                );
            }
        }
        let layout = state.layout.clone();
        let info = catalog::raster_info(layout.as_deref(), &runs, &self.titles);
        Catalog {
            layout,
            runs,
            info: Arc::new(info),
            content_version: state.content_version,
            polled_at: Some(Utc::now()),
        }
    }

    /// The layout index of `parameter`, the first variable for `None`.
    fn variable_index(layout: &Layout, parameter: Option<&str>) -> Result<usize, DataServerError> {
        match parameter {
            None => Ok(0),
            Some(name) => layout
                .variables
                .iter()
                .position(|v| &*v.name == name)
                .ok_or_else(|| {
                    DataServerError::InvalidParameter(format!(
                        "Parameter '{name}' is unavailable in this collection"
                    ))
                }),
        }
    }

    /// The decoded field of variable `index` at `step`.
    fn field(
        &self,
        layout: &Layout,
        step: &Step,
        index: usize,
    ) -> Result<field::Values, DataServerError> {
        let file = &step.file;
        field::read(
            &file.nc,
            file.id,
            &file.reads[index],
            step.index,
            layout.grid.flipped,
            &self.decodes,
        )
    }
}

impl MapEngine for NetcdfEngine {
    #[allow(clippy::too_many_arguments)] // bbox/size/time/crs/parameter/z/reference_time are all genuine selectors
    fn get_raster_tile(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        output_crs: &OutputCrs,
        parameter: Option<&str>,
        _z: Option<f64>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        let catalog = self.catalog.load_full();
        let selected = catalog.select(reference_time, time)?;
        let layout = catalog
            .layout
            .as_ref()
            .ok_or_else(|| DataServerError::Engine("No NetCDF data available yet".to_string()))?;
        let index = Self::variable_index(layout, parameter)?;
        let values = self.field(layout, selected.step, index)?;
        layout
            .source
            .render_f32(&values, None, bbox, width, height, output_crs)
    }

    /// Every band from the one step `select` picks, sampled from one
    /// output→source map: the variables share the grid.
    #[allow(clippy::too_many_arguments)] // mirrors get_raster_tile
    fn get_raster_tiles(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        output_crs: &OutputCrs,
        parameters: &[&str],
        _z: Option<f64>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<Vec<RasterTile>, DataServerError> {
        if parameters.is_empty() {
            return Ok(Vec::new());
        }
        let catalog = self.catalog.load_full();
        let selected = catalog.select(reference_time, time)?;
        let layout = catalog
            .layout
            .as_ref()
            .ok_or_else(|| DataServerError::Engine("No NetCDF data available yet".to_string()))?;
        let indices = parameters
            .iter()
            .map(|&p| Self::variable_index(layout, Some(p)))
            .collect::<Result<Vec<_>, _>>()?;
        let map = layout.source.output_map(bbox, width, height, output_crs);
        indices
            .into_iter()
            .map(|index| map.sample_f32(&self.field(layout, selected.step, index)?, None))
            .collect()
    }

    fn raster_info(&self) -> RasterInfo {
        (*self.raster_info_shared()).clone()
    }

    fn raster_info_shared(&self) -> Arc<RasterInfo> {
        self.catalog.load().info.clone()
    }

    /// The valid time `get_raster_tile` draws, through the same
    /// [`Catalog::select`]; a failed selection echoes the request, since
    /// the render then fails and caches nothing.
    fn resolve_time(
        &self,
        time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        match self.catalog.load().select(reference_time, time) {
            Ok(selected) => Some(selected.valid),
            Err(_) => time,
        }
    }

    /// The run `get_raster_tile` draws from, through the same
    /// [`Catalog::select`], an older run's included when the newest does
    /// not cover the time; a failed selection echoes the request.
    fn resolve_reference_time(
        &self,
        time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        match self.catalog.load().select(reference_time, time) {
            Ok(selected) => selected.reference_time,
            Err(_) => reference_time,
        }
    }

    /// Moves when a catalogued file is replaced on disk (#1067 decision 7):
    /// the rendered caches key on it.
    fn content_version(&self) -> u64 {
        self.catalog.load().content_version
    }
}

impl Drop for NetcdfEngine {
    fn drop(&mut self) {
        self.shutdown.shutdown();
    }
}

impl std::fmt::Debug for NetcdfEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetcdfEngine")
            .field("collection_id", &self.collection_id)
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}
