//! The station-grid collection: its time axis, content version, poll loop,
//! and the `MapEngine` and `EdrEngine` over the analysis grids.
//!
//! # Time
//!
//! The analysis times are every `cadence` (aligned to 00:00 UTC) across the
//! source's temporal extent, or, without one, the source's own observation
//! instants (`EdrEngine::observation_instants`: CSV and BUFR list them from
//! their in-memory stores, #1206; a source that lists none, PostGIS, fails
//! the engine at load asking for a cadence). A map request snaps to the
//! latest analysis time not after it ([`select_time`]), and `resolve_time`
//! uses the same function (#507).
//!
//! # Revisions
//!
//! Observations arrive late and are corrected, so the grid of a time can
//! change after it was drawn. Every computation fingerprints its input
//! slice; when the fingerprint of a (parameter, time) differs from the one
//! recorded before, that time's revision moves, which re-keys its grid in
//! the grid cache, and so does the engine's content version, which re-keys
//! the no-TTL map caches (`MapEngine::content_version`). A grid is kept
//! under the revision of the input it was computed from, even when its own
//! computation found the revision (`compute::store`). The poll loop
//! re-reads the latest `precompute` times of every parameter on each cycle,
//! so late observations for them are found within one poll interval, and a
//! bounded round of the older times whose grid is cached (`recheck`), so a
//! cached older grid is found revised within a few cycles; an older time
//! without a cached grid is read afresh when it is next asked for. No
//! request reads the source to find a revision. The content version is one
//! value for the collection (the trait has no other): a revision re-keys
//! every time's map tiles, but only the revised time's grid is computed
//! again; the other tiles re-render from cached grids.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use arc_swap::ArcSwap;
use chrono::{DateTime, Duration, Utc};
use ds_cache::CacheEntry;
use ds_core::config::StationGridConfig;
use ds_core::cube::{axis_positions, check_cube_budget, CubeResolution};
use ds_core::datetime::parse_iso8601_duration;
use ds_core::edr_engine::{select_parameters, EdrEngine};
use ds_core::error::DataServerError;
use ds_core::feature::{
    check_area_budget, check_mask_budget, parse_area_coords, parse_point_coords, Bbox, MAX_AREA_DIM,
};
use ds_core::map_engine::{
    select_common_time, MapEngine, OutputCrs, ParameterInfo, RasterInfo, RasterTile, RasterValues,
};
use ds_core::model::{
    CoverageResponse, DomainDescription, LocationSnapshot, NdArray, ParameterDescription,
    QueryResult, ValuePrecision,
};
use ds_core::native_grid::NativeGrid;
use ds_core::resample::ProjectionGrid;
use ds_poll::{FirstTick, Shutdown};
use tracing::{debug, info, warn};

use crate::compute::{self, AnalysisGrid, GridKey, CACHE};
use crate::correction::{Auxiliary, Corrections};
use crate::geometry::{wrap_lon, GridGeom};
use crate::interpolate::{interpolate_grid, Method, Settings, Stations};
use crate::residuals::{self, Summary};
use crate::source::{self, Slice};

mod recheck;
#[cfg(test)]
mod revision_tests;

pub use recheck::RECHECK_PER_CYCLE;

/// Analysis times kept on the axis, the latest ones.
pub const MAX_TIMES: usize = 10_000;
/// Analysis grids one EDR query may read (times × parameters).
pub const MAX_QUERY_GRIDS: usize = 96;
/// Neighbours a cell may be estimated from at most.
pub const MAX_NEIGHBOURS: usize = 64;
/// Latest times per parameter the poll may compute ahead.
pub const MAX_PRECOMPUTE: usize = 24;
/// Largest search radius.
pub const MAX_RADIUS_KM: f64 = 10_000.0;
/// How long a caller without a request deadline (the poll, a test) waits
/// for a grid.
const NO_DEADLINE_WAIT: std::time::Duration = std::time::Duration::from_secs(600);
/// How often the poll retries terrain and mask not sampled yet.
const CORRECTIONS_RETRY: std::time::Duration = std::time::Duration::from_secs(5);

/// Distinct engine instances: a grid cache key names the instance, whose
/// source and configuration are fixed for its life.
static EPOCH: AtomicU64 = AtomicU64::new(1);

/// The analysis time a map draws for a requested `time`: the latest at or
/// before it, the earliest when it precedes them all, the latest for
/// `None`; `None` with no times. `get_raster_tile` and `resolve_time` both
/// use it (#507).
pub fn select_time(times: &[DateTime<Utc>], time: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    select_common_time(&[times], time)
}

/// The latest analysis computed of a parameter: its time, station count and
/// leave-one-out residual summary.
type Latest = (DateTime<Utc>, usize, Summary);

/// What one (parameter, time) was last computed from.
#[derive(Debug, Clone, Copy)]
struct Input {
    fingerprint: u64,
    /// How many times its input changed: part of its grid's cache key.
    revision: u64,
}

/// One gridded parameter.
struct Param {
    /// The source's spelling.
    name: String,
    description: ParameterDescription,
}

/// The axis and the metadata built from it, swapped whole.
struct Snapshot {
    times: Arc<[DateTime<Utc>]>,
    info: Arc<RasterInfo>,
}

/// A grid being fetched for a request.
enum Pending {
    Ready(Arc<AnalysisGrid>),
    Job(mpsc::Receiver<Result<Arc<AnalysisGrid>, DataServerError>>),
    /// Another caller is computing it.
    Elsewhere(GridKey),
}

struct Inner {
    collection_id: String,
    source_id: String,
    source: Arc<dyn EdrEngine>,
    params: Vec<Param>,
    settings: Settings,
    geom: GridGeom,
    /// The box the source is asked for: the grid widened by the radius.
    query_bbox: [f64; 4],
    /// Terrain, mask and exclusions (#1206).
    corrections: Corrections,
    max_age: Duration,
    cadence: Option<Duration>,
    poll_interval: std::time::Duration,
    precompute: usize,
    /// Bytes one computation reserves.
    job_bytes: u64,
    epoch: u64,
    version: AtomicU64,
    snapshot: ArcSwap<Snapshot>,
    /// The input last seen per (parameter, time).
    inputs: Mutex<HashMap<(usize, DateTime<Utc>), Input>>,
    /// Where the next round of older re-checks starts (`recheck`).
    recheck_from: Mutex<Option<recheck::Position>>,
    /// Per parameter, the latest analysis time computed and its station
    /// count.
    latest: Mutex<Vec<Option<Latest>>>,
    analyses: AtomicU64,
    failures: AtomicU64,
    last_analysis_ms: AtomicU64,
    stopped: AtomicBool,
    provenance: String,
}

/// A derived collection gridding a station collection; see the module
/// docs.
pub struct StationGridEngine {
    inner: Arc<Inner>,
    shutdown: Shutdown,
    config: StationGridConfig,
}

impl StationGridEngine {
    /// Build the engine over the station collection `source`. Checks every
    /// setting; reads only the source's metadata snapshots, never its data.
    /// A config naming an `elevation_source` or `land_mask_source` needs
    /// [`Self::new_with`].
    pub fn new(
        collection_id: &str,
        source_id: &str,
        source: Arc<dyn EdrEngine>,
        config: &StationGridConfig,
    ) -> Result<Self, DataServerError> {
        Self::new_with(
            collection_id,
            source_id,
            source,
            config,
            Auxiliary::default(),
        )
    }

    /// [`Self::new`] with the collections the corrections read (#1206): the
    /// terrain, the mask and the source's station inventory. Their data is
    /// read by the poll loop, never here.
    pub fn new_with(
        collection_id: &str,
        source_id: &str,
        source: Arc<dyn EdrEngine>,
        config: &StationGridConfig,
        aux: Auxiliary,
    ) -> Result<Self, DataServerError> {
        let bad = |message: String| DataServerError::Config(format!("station_grid: {message}"));
        if !source.serves_station_series() {
            return Err(bad(format!(
                "source '{source_id}' is not a station collection (csv, bufr or postgis stations)"
            )));
        }
        if !source.supported_query_types().iter().any(|q| q == "area") {
            return Err(bad(format!(
                "source '{source_id}' does not answer area queries"
            )));
        }
        if config.parameters.is_empty() {
            return Err(bad(
                "parameters must name at least one source parameter".into()
            ));
        }
        let available = source.get_parameters();
        let available: Vec<&str> = available.iter().map(String::as_str).collect();
        let wanted: Vec<String> = config
            .parameters
            .iter()
            .map(|p| p.name().to_string())
            .collect();
        let names = select_parameters(Some(&wanted), &available)
            .map_err(|e| bad(format!("source '{source_id}': {e}")))?;
        if names.len() != config.parameters.len() {
            return Err(bad("parameters must not repeat".into()));
        }
        let descriptions = source.get_parameter_descriptions();
        let params: Vec<Param> = names
            .iter()
            .map(|&name| Param {
                name: name.to_string(),
                description: descriptions.get(name).cloned().unwrap_or_else(|| {
                    ParameterDescription {
                        label: name.replace('_', " "),
                        unit: String::new(),
                        observed_property: name.to_string(),
                        standard_name: None,
                    }
                }),
            })
            .collect();

        if !(1..=MAX_NEIGHBOURS).contains(&config.max_neighbours) {
            return Err(bad(format!(
                "max_neighbours must be 1..={MAX_NEIGHBOURS}, not {}",
                config.max_neighbours
            )));
        }
        let radius_km = config.search_radius_km;
        if !(radius_km.is_finite() && radius_km > 0.0 && radius_km <= MAX_RADIUS_KM) {
            return Err(bad(format!(
                "search_radius_km must be in (0, {MAX_RADIUS_KM}], not {radius_km}"
            )));
        }
        let positive = |name: &str, v: f64| -> Result<f64, DataServerError> {
            if v.is_finite() && v > 0.0 {
                Ok(v)
            } else {
                Err(bad(format!("{name} must be a positive number, not {v}")))
            }
        };
        let method = match config.method.as_str() {
            "idw" => {
                if config.correlation_length_km.is_some() {
                    return Err(bad(
                        "correlation_length_km applies to method \"adw\" only".into()
                    ));
                }
                Method::Idw {
                    power: positive("power", config.power.unwrap_or(2.0))?,
                }
            }
            "adw" => Method::Adw {
                decay_m: 1000.0
                    * positive(
                        "correlation_length_km",
                        config.correlation_length_km.unwrap_or(radius_km),
                    )?,
                exponent: positive("power", config.power.unwrap_or(4.0))?,
            },
            "nearest" => {
                if config.power.is_some() || config.correlation_length_km.is_some() {
                    return Err(bad(
                        "power and correlation_length_km do not apply to method \"nearest\"".into(),
                    ));
                }
                Method::Nearest
            }
            other => {
                return Err(bad(format!(
                    "unknown method '{other}'; expected \"idw\", \"adw\" or \"nearest\""
                )))
            }
        };
        let settings = Settings {
            method,
            max_neighbours: config.max_neighbours,
            radius_m: radius_km * 1000.0,
        };

        let crs = config.grid.crs.trim();
        if ![
            "EPSG:4326",
            "CRS:84",
            "OGC:CRS84",
            "http://www.opengis.net/def/crs/OGC/1.3/CRS84",
        ]
        .iter()
        .any(|c| c.eq_ignore_ascii_case(crs))
        {
            return Err(bad(format!(
                "grid crs '{crs}' is not supported; the analysis grid is CRS84 (\"EPSG:4326\")"
            )));
        }
        let geom = GridGeom::new(config.grid.bbox, config.grid.resolution).map_err(bad)?;
        let described: Vec<(String, ParameterDescription)> = params
            .iter()
            .map(|p| (p.name.clone(), p.description.clone()))
            .collect();
        let corrections = Corrections::new(
            collection_id,
            config,
            aux,
            &described,
            &geom,
            settings.radius_m,
        )
        .map_err(bad)?;
        let grid_bytes = compute::grid_bytes(geom.cells());
        // The terrain and mask grids live as long as the engine: they count
        // with one grid against the cache.
        let resident = grid_bytes + corrections.resident_bytes();
        if resident > compute::cache_capacity() {
            return Err(bad(format!(
                "one grid of {} cells is {} MiB with its terrain and mask, more than the {} MiB \
                 grid cache (MC_STATION_GRID_CACHE_MB) holds, so every map tile would recompute \
                 it; raise the cache or coarsen the grid",
                geom.cells(),
                resident.div_ceil(ds_cache::MIB),
                compute::cache_capacity() / ds_cache::MIB
            )));
        }
        let job_bytes = grid_bytes + compute::COMPUTE_SLACK_BYTES;
        let (_, budget, _) = compute::budget_metrics();
        if job_bytes > budget / 2 {
            return Err(bad(format!(
                "computing one grid of {} cells needs {} MiB, more than half the {} MiB \
                 computation budget (MC_STATION_GRID_MEMORY_MB); raise it or coarsen the grid",
                geom.cells(),
                job_bytes.div_ceil(ds_cache::MIB),
                budget / ds_cache::MIB
            )));
        }

        let duration = |name: &str, value: &str| -> Result<Duration, DataServerError> {
            let d = parse_iso8601_duration(value).map_err(|e| bad(format!("{name}: {e}")))?;
            if d <= Duration::zero() {
                return Err(bad(format!("{name} must be positive")));
            }
            Ok(d)
        };
        let max_age = duration("max_age", &config.max_age)?;
        let cadence = config
            .cadence
            .as_deref()
            .map(|c| duration("cadence", c))
            .transpose()?;
        if cadence.is_none() && source.observation_instants().is_none() {
            return Err(bad(format!(
                "source '{source_id}' does not list its observation instants (a CSV or BUFR \
                 source does); set `cadence` (e.g. \"PT1H\") for the analysis times"
            )));
        }
        if config.poll_interval_secs == 0 {
            return Err(bad("poll_interval_secs must be > 0".into()));
        }
        if config.precompute > MAX_PRECOMPUTE {
            return Err(bad(format!(
                "precompute must be at most {MAX_PRECOMPUTE}, not {}",
                config.precompute
            )));
        }

        let provenance = [
            provenance(source_id, &params, &settings, &geom, &config.max_age),
            corrections.describe(),
        ]
        .join(" ")
        .trim_end()
        .to_string();
        let inner = Arc::new(Inner {
            collection_id: collection_id.to_string(),
            source_id: source_id.to_string(),
            source,
            query_bbox: geom.query_bbox(settings.radius_m),
            corrections,
            settings,
            geom,
            max_age,
            cadence,
            poll_interval: std::time::Duration::from_secs(config.poll_interval_secs.max(5)),
            precompute: config.precompute,
            job_bytes,
            epoch: EPOCH.fetch_add(1, Ordering::Relaxed),
            version: AtomicU64::new(1),
            snapshot: ArcSwap::from_pointee(Snapshot {
                times: Arc::from([]),
                info: Arc::new(RasterInfo {
                    native_crs: String::new(),
                    spatial_extent: None,
                    times: Vec::new(),
                    parameter: String::new(),
                    unit: String::new(),
                    parameters: Vec::new(),
                    vertical: None,
                    grid_size: None,
                    layer_subtitle: None,
                    reference_times: Vec::new(),
                    native_grid: None,
                }),
            }),
            inputs: Mutex::new(HashMap::new()),
            recheck_from: Mutex::new(None),
            latest: Mutex::new(vec![None; params.len()]),
            params,
            analyses: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            last_analysis_ms: AtomicU64::new(0),
            stopped: AtomicBool::new(false),
            provenance,
        });
        inner.publish(inner.build_times());
        Ok(Self {
            inner,
            shutdown: Shutdown::new(),
            config: config.clone(),
        })
    }

    pub fn collection_id(&self) -> &str {
        &self.inner.collection_id
    }

    pub fn source_id(&self) -> &str {
        &self.inner.source_id
    }

    /// The configuration the engine was built from.
    pub fn config(&self) -> &StationGridConfig {
        &self.config
    }

    /// One sentence on how the grids are made: the source, the parameters,
    /// the method and the grid. The server adds it to the collection's
    /// description.
    pub fn provenance(&self) -> &str {
        &self.inner.provenance
    }

    /// Whether there is any analysis time yet.
    pub fn has_data(&self) -> bool {
        !self.inner.snapshot.load().times.is_empty()
    }

    /// Why there is no analysis time yet, when it is the terrain, the mask
    /// or the station inventory the engine waits for.
    pub fn waiting_for(&self) -> Option<String> {
        self.inner.corrections.waiting_for()
    }

    /// Per parameter, the stations of the latest analysis computed (`None`
    /// before one is), for the `station_grid_stations` gauge.
    pub fn station_counts(&self) -> Vec<(String, Option<usize>)> {
        let latest = self.inner.latest.lock().unwrap_or_else(|e| e.into_inner());
        self.inner
            .params
            .iter()
            .zip(latest.iter())
            .map(|(p, l)| (p.name.clone(), l.map(|(_, n, _)| n)))
            .collect()
    }

    /// Per parameter, the leave-one-out residuals of the latest analysis
    /// computed (`None` before one is), for the `station_grid_loo_*` gauges.
    pub fn residual_summaries(&self) -> Vec<(String, Option<Summary>)> {
        let latest = self.inner.latest.lock().unwrap_or_else(|e| e.into_inner());
        self.inner
            .params
            .iter()
            .zip(latest.iter())
            .map(|(p, l)| (p.name.clone(), l.map(|(_, _, summary)| summary)))
            .collect()
    }

    /// The analysis times, oldest first.
    pub(crate) fn analysis_times(&self) -> Arc<[DateTime<Utc>]> {
        self.inner.snapshot.load().times.clone()
    }

    /// The box the source is read over: the grid widened by the search
    /// radius, where every station of an analysis lies.
    pub(crate) fn search_bbox(&self) -> [f64; 4] {
        self.inner.query_bbox
    }

    /// `(analyses computed, analyses failed, last analysis in ms)`.
    pub fn metrics(&self) -> (u64, u64, u64) {
        (
            self.inner.analyses.load(Ordering::Relaxed),
            self.inner.failures.load(Ordering::Relaxed),
            self.inner.last_analysis_ms.load(Ordering::Relaxed),
        )
    }

    /// The background loop: refresh the axis, compute the latest times
    /// ahead, and re-check them and a bounded round of the older cached
    /// ones for late observations. Spawn it on the background poll runtime
    /// (a multi-thread runtime: the cycle runs in `block_in_place`).
    pub async fn poll_loop(&self) {
        let mut ticker = self
            .shutdown
            .ticker(self.inner.poll_interval, FirstTick::Immediate);
        while ticker.tick().await {
            tokio::task::block_in_place(|| self.poll_once());
            // Until the terrain and mask are sampled there is no analysis:
            // retry them sooner than the poll interval.
            while !self.inner.corrections.ready() {
                if !self.shutdown.sleep(CORRECTIONS_RETRY).await {
                    break;
                }
                tokio::task::block_in_place(|| self.poll_once());
            }
        }
        info!(
            "[{}] station-grid poll loop shutting down",
            self.collection_id()
        );
    }

    /// One poll cycle, synchronously: the axis from the source's current
    /// extent, then the latest `precompute` times of every parameter read
    /// again and computed when new or revised, then at most
    /// [`RECHECK_PER_CYCLE`] older times whose grid is cached read again
    /// and computed when revised.
    pub fn poll_once(&self) {
        let inner = &self.inner;
        inner.corrections.refresh();
        inner.publish(inner.build_times());
        let times = inner.snapshot.load().times.clone();
        for p in 0..inner.params.len() {
            for &time in times.iter().rev().take(inner.precompute) {
                if inner.stopped.load(Ordering::Relaxed) {
                    return;
                }
                inner.precompute_one(p, time);
            }
        }
        inner.recheck_older(&times);
    }

    /// Stop the poll loop and drop this instance's grids.
    pub fn shutdown(&self) {
        self.shutdown.shutdown();
        self.inner.stopped.store(true, Ordering::Relaxed);
        compute::evict_epoch(self.inner.epoch);
    }

    fn param_index(&self, parameter: Option<&str>) -> Result<usize, DataServerError> {
        let Some(name) = parameter else {
            return Ok(0);
        };
        self.inner
            .params
            .iter()
            .position(|p| p.name == name)
            .or_else(|| {
                self.inner
                    .params
                    .iter()
                    .position(|p| p.name.eq_ignore_ascii_case(name))
            })
            .ok_or_else(|| {
                DataServerError::InvalidParameter(format!(
                    "Unknown parameter '{name}'; available: {}",
                    self.parameter_names().join(", ")
                ))
            })
    }

    pub(crate) fn parameter_names(&self) -> Vec<String> {
        self.inner.params.iter().map(|p| p.name.clone()).collect()
    }

    /// The parameter indices an EDR `parameter-name` selects.
    fn selected(&self, parameters: Option<&[String]>) -> Result<Vec<usize>, DataServerError> {
        let names = self.parameter_names();
        let available: Vec<&str> = names.iter().map(String::as_str).collect();
        Ok(select_parameters(parameters, &available)?
            .into_iter()
            .filter_map(|name| self.inner.params.iter().position(|p| p.name == name))
            .collect())
    }

    /// The analysis times an EDR `datetime` selects: the latest without one,
    /// exactly the instant given, or every time inside an interval.
    fn query_times(
        &self,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
    ) -> Result<Vec<DateTime<Utc>>, DataServerError> {
        let times = self.inner.snapshot.load().times.clone();
        let selected: Vec<DateTime<Utc>> = match datetime {
            None => times.last().copied().into_iter().collect(),
            Some((start, end)) => times
                .iter()
                .copied()
                .filter(|t| start <= *t && *t <= end)
                .collect(),
        };
        if selected.is_empty() {
            return Err(DataServerError::LocationNotFound(match datetime {
                None => format!("'{}' has no analysis time yet", self.collection_id()),
                Some(_) => format!(
                    "'{}' has no analysis time in the requested datetime",
                    self.collection_id()
                ),
            }));
        }
        Ok(selected)
    }

    /// The grids of every (parameter, time), all queued before any is
    /// waited for.
    pub(crate) fn grids_for(
        &self,
        params: &[usize],
        times: &[DateTime<Utc>],
    ) -> Result<Vec<Vec<Arc<AnalysisGrid>>>, DataServerError> {
        let count = params.len() * times.len();
        if count > MAX_QUERY_GRIDS {
            return Err(DataServerError::QueryTooLarge(format!(
                "The query reads {count} analyses ({} times × {} parameters); the limit is \
                 {MAX_QUERY_GRIDS} — narrow the datetime window or the parameters",
                times.len(),
                params.len()
            )));
        }
        let wanted: Vec<(usize, DateTime<Utc>)> = params
            .iter()
            .flat_map(|&p| times.iter().map(move |&t| (p, t)))
            .collect();
        let mut grids = self.inner.grids(&wanted)?.into_iter();
        Ok(params
            .iter()
            .map(|_| grids.by_ref().take(times.len()).collect())
            .collect())
    }

    fn description_map(&self, params: &[usize]) -> HashMap<String, ParameterDescription> {
        params
            .iter()
            .map(|&p| {
                let param = &self.inner.params[p];
                (param.name.clone(), param.description.clone())
            })
            .collect()
    }

    /// A gridded response sampling `cells` (row-major over `ny × nx`, `None`
    /// off the grid or outside the area) from each grid.
    fn grid_response(
        &self,
        params: &[usize],
        times: Vec<DateTime<Utc>>,
        x: Vec<f64>,
        y: Vec<f64>,
        cells: &[Option<usize>],
    ) -> Result<CoverageResponse, DataServerError> {
        let grids = self.grids_for(params, &times)?;
        let (nx, ny) = (x.len(), y.len());
        let mut ranges = HashMap::new();
        for (&p, grids) in params.iter().zip(&grids) {
            let mut values = Vec::with_capacity(times.len() * cells.len());
            for grid in grids {
                values.extend(cells.iter().map(|cell| cell.and_then(|i| value(grid, i))));
            }
            ranges.insert(
                self.inner.params[p].name.clone(),
                NdArray {
                    shape: vec![times.len(), ny, nx],
                    axis_names: vec!["t".into(), "y".into(), "x".into()],
                    values,
                    precision: VALUE_PRECISION,
                },
            );
        }
        Ok(CoverageResponse::Single(QueryResult {
            domain: DomainDescription::Grid {
                x,
                y,
                t: Some(times),
                z: None,
            },
            parameters: self.description_map(params),
            ranges,
        }))
    }
}

/// The precision of every EDR range (#1341): the analysis grids are `f32`
/// ([`value`]).
const VALUE_PRECISION: ValuePrecision = ValuePrecision::F32;

/// A cell's value: `None` for nodata. The `f64` with the stored `f32`'s
/// shortest decimal spelling, `-5.8676414` rather than the widened
/// `-5.867641448974609`: the same number the map draws, without the
/// digits `f32` never had.
fn value(grid: &AnalysisGrid, cell: usize) -> Option<f64> {
    let v = grid.values.get(cell).copied().filter(|v| v.is_finite())?;
    Some(v.to_string().parse().unwrap_or(f64::from(v)))
}

fn provenance(
    source_id: &str,
    params: &[Param],
    settings: &Settings,
    geom: &GridGeom,
    max_age: &str,
) -> String {
    let names: Vec<&str> = params.iter().map(|p| p.name.as_str()).collect();
    let reach = format!(
        "up to {} stations within {} km",
        settings.max_neighbours,
        settings.radius_m / 1000.0
    );
    let method = match settings.method {
        Method::Idw { power } => format!("inverse-distance weighting (power {power}, {reach})"),
        Method::Adw { decay_m, exponent } => format!(
            "angular distance weighting (correlation length {} km, exponent {exponent}, {reach})",
            decay_m / 1000.0
        ),
        Method::Nearest => format!(
            "the nearest station within {} km",
            settings.radius_m / 1000.0
        ),
    };
    let [w, s, e, n] = geom.bbox();
    format!(
        "Gridded on the fly from station collection '{source_id}' ({}) by {method}, onto a \
         CRS84 grid of {} × {} cells over [{w}, {s}, {e}, {n}]. A station's value at an \
         analysis time is its latest observation not after it and at most {max_age} old. EDR \
         queries sample the same grid the maps draw.",
        names.join(", "),
        geom.nx,
        geom.ny
    )
}

impl Inner {
    fn key(&self, parameter: usize, time: DateTime<Utc>) -> GridKey {
        GridKey {
            epoch: self.epoch,
            parameter,
            time: time.timestamp(),
            revision: self
                .inputs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&(parameter, time))
                .map_or(0, |input| input.revision),
        }
    }

    /// The key a grid computed from an input with `fingerprint` belongs
    /// under: the time's recorded revision, while that is still this input.
    fn key_of_input(
        &self,
        parameter: usize,
        time: DateTime<Utc>,
        fingerprint: u64,
    ) -> Option<GridKey> {
        let inputs = self.inputs.lock().unwrap_or_else(|e| e.into_inner());
        let input = inputs
            .get(&(parameter, time))
            .filter(|input| input.fingerprint == fingerprint)?;
        Some(GridKey {
            epoch: self.epoch,
            parameter,
            time: time.timestamp(),
            revision: input.revision,
        })
    }

    /// The analysis times from the source's current metadata; none until
    /// the corrections are ready, so nothing uncorrected is ever computed.
    fn build_times(&self) -> Vec<DateTime<Utc>> {
        if !self.corrections.ready() {
            return Vec::new();
        }
        let mut times = match self.cadence {
            Some(step) => {
                let Some((start, end)) = self.source.get_temporal_extent() else {
                    return Vec::new();
                };
                let s = step.num_seconds().max(1);
                let floor = |t: i64| t - t.rem_euclid(s);
                let last = floor(end.timestamp());
                let mut first = start.timestamp();
                if first.rem_euclid(s) != 0 {
                    first = floor(first) + s;
                }
                if first > last {
                    return Vec::new();
                }
                let count = ((last - first) / s + 1) as usize;
                if count > MAX_TIMES {
                    first = last - (MAX_TIMES as i64 - 1) * s;
                }
                (0..)
                    .map(|i| first + i * s)
                    .take_while(|&t| t <= last)
                    .filter_map(|t| DateTime::from_timestamp(t, 0))
                    .collect()
            }
            None => {
                let instants = self.source.observation_instants().unwrap_or_default();
                instants[instants.len().saturating_sub(MAX_TIMES)..].to_vec()
            }
        };
        times.sort_unstable();
        times.dedup();
        if times.len() > MAX_TIMES {
            times.drain(..times.len() - MAX_TIMES);
        }
        times
    }

    /// Swap in a new axis, and forget the inputs of times no longer
    /// on it.
    fn publish(&self, times: Vec<DateTime<Utc>>) {
        // The first call publishes the parameters even without a time, so
        // the layers' styles resolve at load (#1206: no time until the
        // terrain and mask are sampled).
        let current = self.snapshot.load();
        if *current.times == *times && !current.info.parameters.is_empty() {
            return;
        }
        drop(current);
        let first = self.params.first();
        let info = RasterInfo {
            native_crs: "EPSG:4326".into(),
            spatial_extent: Some(self.geom.bbox()),
            times: times.clone(),
            parameter: first.map(|p| p.name.clone()).unwrap_or_default(),
            unit: first
                .map(|p| p.description.unit.clone())
                .unwrap_or_default(),
            parameters: self
                .params
                .iter()
                .map(|p| ParameterInfo {
                    name: p.name.clone(),
                    title: format!(
                        "{} ({} analysis)",
                        p.description.label,
                        self.settings.method.name().to_uppercase()
                    ),
                    unit: p.description.unit.clone(),
                })
                .collect(),
            vertical: None,
            grid_size: Some([self.geom.nx, self.geom.ny]),
            layer_subtitle: None,
            reference_times: Vec::new(),
            // The analysis grid MeteoCore defines, in lon/lat (#1237). A
            // lat/lon grid gets no `meteocore:nativeGrid` member.
            native_grid: NativeGrid::new(
                ds_core::geo::Crs::Wgs84,
                None,
                [self.geom.nx, self.geom.ny],
                [self.geom.west, self.geom.south],
                [self.geom.dx, self.geom.dy],
            ),
        };
        let on_axis: std::collections::HashSet<DateTime<Utc>> = times.iter().copied().collect();
        self.snapshot.store(Arc::new(Snapshot {
            times: times.into(),
            info: Arc::new(info),
        }));
        self.inputs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(_, t), _| on_axis.contains(t));
    }

    /// Record the fingerprint of a (parameter, time) input; a different one
    /// than before is a revision: it moves the time's revision and the
    /// content version.
    fn observe(&self, parameter: usize, time: DateTime<Utc>, fingerprint: u64) {
        let mut inputs = self.inputs.lock().unwrap_or_else(|e| e.into_inner());
        let input = inputs.entry((parameter, time)).or_insert(Input {
            fingerprint,
            revision: 0,
        });
        if input.fingerprint == fingerprint {
            return;
        }
        input.fingerprint = fingerprint;
        input.revision += 1;
        let version = self.version.fetch_add(1, Ordering::AcqRel) + 1;
        info!(
            "[{}] observations for {} at {time} were revised; content version {version}",
            self.collection_id, self.params[parameter].name
        );
    }

    /// One slice from the source, as `parameter` grids it: exclusions left
    /// out, a corrected parameter's values reduced to sea level.
    fn read(&self, parameter: usize, time: DateTime<Utc>) -> Result<Slice, DataServerError> {
        let slice = source::fetch(
            self.source.as_ref(),
            &self.params[parameter].name,
            self.query_bbox,
            time,
            self.max_age,
        )?;
        self.corrections.prepare(parameter, slice)
    }

    /// Read (unless given) and grid one slice.
    fn compute(
        &self,
        parameter: usize,
        time: DateTime<Utc>,
        slice: Option<Slice>,
    ) -> Result<AnalysisGrid, DataServerError> {
        let started = Instant::now();
        let slice = match slice {
            Some(slice) => slice,
            None => self.read(parameter, time)?,
        };
        // The grid interpolates the values reduced to sea level (#1206);
        // the residuals put each estimate back at its station's height.
        let stations = Stations::new(&slice.interpolated());
        let mut values = interpolate_grid(&stations, &self.settings, &self.geom);
        self.corrections.finish(parameter, &mut values)?;
        let residuals = residuals::leave_one_out(&stations, &slice.stations, &self.settings);
        let summary = residuals::summarize(&residuals);
        self.observe(parameter, time, slice.fingerprint);
        let elapsed = started.elapsed().as_millis() as u64;
        self.analyses.fetch_add(1, Ordering::Relaxed);
        self.last_analysis_ms.store(elapsed, Ordering::Relaxed);
        {
            let mut latest = self.latest.lock().unwrap_or_else(|e| e.into_inner());
            if latest[parameter].is_none_or(|(t, _, _)| t <= time) {
                latest[parameter] = Some((time, stations.len(), summary));
            }
        }
        debug!(
            "[{}] analysed {} at {time} from {} stations in {elapsed} ms",
            self.collection_id,
            self.params[parameter].name,
            stations.len()
        );
        Ok(AnalysisGrid {
            values,
            stations: stations.len(),
            fingerprint: slice.fingerprint,
            residuals,
        })
    }

    /// Queue a computation on the pool; its result also fills `guard`,
    /// claimed under `key`, and is kept under the revision of the input it
    /// was computed from.
    fn submit(
        self: &Arc<Self>,
        parameter: usize,
        time: DateTime<Utc>,
        slice: Option<Slice>,
        key: GridKey,
        guard: ds_cache::CacheFillGuard<'static, GridKey, Arc<AnalysisGrid>>,
        permit: ds_executor::budget::BytePermit,
    ) -> mpsc::Receiver<Result<Arc<AnalysisGrid>, DataServerError>> {
        let (tx, rx) = mpsc::sync_channel(1);
        let inner = self.clone();
        compute::spawn(move || {
            let _permit = permit;
            let result = inner.compute(parameter, time, slice).map(Arc::new);
            match &result {
                Ok(grid) => {
                    let current = inner.key_of_input(parameter, time, grid.fingerprint);
                    compute::store(guard, key, current, grid.clone());
                }
                Err(e) => {
                    inner.failures.fetch_add(1, Ordering::Relaxed);
                    warn!(
                        "[{}] analysis of {} at {time} failed: {e}",
                        inner.collection_id, inner.params[parameter].name
                    );
                }
            }
            let _ = tx.send(result);
        });
        rx
    }

    /// Start fetching a grid for a request: a cache hit, a computation
    /// queued now (its bytes reserved first), or one already under way.
    fn start(
        self: &Arc<Self>,
        parameter: usize,
        time: DateTime<Utc>,
    ) -> Result<Pending, DataServerError> {
        let key = self.key(parameter, time);
        match CACHE.get_value_or_guard_untracked(&key, std::time::Duration::ZERO) {
            CacheEntry::Value(grid) => {
                CACHE.record_hit();
                Ok(Pending::Ready(grid))
            }
            CacheEntry::Vacant(guard) => {
                CACHE.record_miss();
                let permit = compute::reserve(self.job_bytes)?;
                Ok(Pending::Job(
                    self.submit(parameter, time, None, key, guard, permit),
                ))
            }
            CacheEntry::Timeout => Ok(Pending::Elsewhere(key)),
        }
    }

    /// Wait for a started grid until the request deadline. A grid not ready
    /// by then is `ResourceExhausted` (HTTP 503); its computation goes on
    /// and fills the cache for the retry. Never a partial grid.
    fn finish(
        self: &Arc<Self>,
        parameter: usize,
        time: DateTime<Utc>,
        pending: Pending,
    ) -> Result<Arc<AnalysisGrid>, DataServerError> {
        match pending {
            Pending::Ready(grid) => Ok(grid),
            Pending::Job(rx) => wait(rx),
            Pending::Elsewhere(key) => {
                match CACHE.get_value_or_guard_untracked(&key, remaining()) {
                    CacheEntry::Value(grid) => {
                        CACHE.record_hit();
                        Ok(grid)
                    }
                    CacheEntry::Timeout => Err(DataServerError::ResourceExhausted),
                    // The other computation failed: try once more ourselves.
                    CacheEntry::Vacant(guard) => {
                        CACHE.record_miss();
                        let permit = compute::reserve(self.job_bytes)?;
                        wait(self.submit(parameter, time, None, key, guard, permit))
                    }
                }
            }
        }
    }

    /// The grids of `wanted`, every miss queued before the first wait.
    fn grids(
        self: &Arc<Self>,
        wanted: &[(usize, DateTime<Utc>)],
    ) -> Result<Vec<Arc<AnalysisGrid>>, DataServerError> {
        let pending: Vec<Pending> = wanted
            .iter()
            .map(|&(p, t)| self.start(p, t))
            .collect::<Result<_, _>>()?;
        wanted
            .iter()
            .zip(pending)
            .map(|(&(p, t), pending)| self.finish(p, t, pending))
            .collect()
    }

    fn grid(
        self: &Arc<Self>,
        parameter: usize,
        time: DateTime<Utc>,
    ) -> Result<Arc<AnalysisGrid>, DataServerError> {
        let pending = self.start(parameter, time)?;
        self.finish(parameter, time, pending)
    }

    /// Re-read one of the latest times and compute it when it is new, was
    /// revised, or fell out of the cache.
    fn precompute_one(self: &Arc<Self>, parameter: usize, time: DateTime<Utc>) {
        let Some(slice) = self.read_ahead(parameter, time) else {
            return;
        };
        self.observe(parameter, time, slice.fingerprint);
        self.compute_ahead(parameter, time, slice);
    }

    /// Read a slice for the poll; a failure is counted and logged.
    fn read_ahead(&self, parameter: usize, time: DateTime<Utc>) -> Option<Slice> {
        let name = &self.params[parameter].name;
        // Through the corrections, as a request's read: exclusions and
        // heights move a pre-computed or re-checked time's fingerprint.
        match self.read(parameter, time) {
            Ok(slice) => Some(slice),
            Err(e) => {
                self.failures.fetch_add(1, Ordering::Relaxed);
                warn!(
                    "[{}] reading {name} at {time} from '{}' failed: {e}",
                    self.collection_id, self.source_id
                );
                None
            }
        }
    }

    /// Compute the grid of an observed `slice` unless the cache holds it,
    /// and wait for it. Background admission only: it never takes the half
    /// of the budget requests keep.
    fn compute_ahead(self: &Arc<Self>, parameter: usize, time: DateTime<Utc>, slice: Slice) {
        let key = self.key(parameter, time);
        let CacheEntry::Vacant(guard) =
            CACHE.get_value_or_guard_untracked(&key, std::time::Duration::ZERO)
        else {
            return;
        };
        let Some(permit) = compute::reserve_background(self.job_bytes) else {
            debug!(
                "[{}] pre-compute of {} at {time} deferred: requests hold the budget",
                self.collection_id, self.params[parameter].name
            );
            return;
        };
        let _ = self
            .submit(parameter, time, Some(slice), key, guard, permit)
            .recv();
    }
}

/// Time left before the request deadline, or a long wait without one.
fn remaining() -> std::time::Duration {
    ds_core::deadline::current()
        .map(|deadline| deadline.saturating_duration_since(Instant::now()))
        .unwrap_or(NO_DEADLINE_WAIT)
}

fn wait(
    rx: mpsc::Receiver<Result<Arc<AnalysisGrid>, DataServerError>>,
) -> Result<Arc<AnalysisGrid>, DataServerError> {
    match rx.recv_timeout(remaining()) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(DataServerError::ResourceExhausted),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(DataServerError::Engine(
            "station grid computation failed".into(),
        )),
    }
}

/// Sample a grid for an output image, nearest cell: output pixel → cell
/// index through the output CRS's axes (separable for CRS84 and Web
/// Mercator, one projection per row and column) or a coarse
/// `ProjectionGrid` (projected output), never a projection per pixel.
fn sample_tile(
    geom: &GridGeom,
    grid: &[f32],
    bbox: [f64; 4],
    width: u32,
    height: u32,
    output_crs: &OutputCrs,
) -> Vec<f32> {
    let mut out = vec![f32::NAN; width as usize * height as usize];
    let nx = geom.nx as usize;
    match output_crs {
        OutputCrs::Wgs84 | OutputCrs::WebMercator => {
            // A box given across the antimeridian is the same box unwrapped.
            let mut unwrapped = bbox;
            if unwrapped[0] > unwrapped[2] {
                unwrapped[2] += 360.0;
            }
            let cols: Vec<Option<u32>> = (0..width)
                .map(|ox| {
                    let fx = (ox as f64 + 0.5) / width as f64;
                    geom.col_at(geom.col_frac(output_crs.project_node(unwrapped, fx, 0.5).0))
                })
                .collect();
            for oy in 0..height {
                let fy = (oy as f64 + 0.5) / height as f64;
                let lat = output_crs.project_node(unwrapped, 0.5, fy).1;
                let Some(row) = geom.row_at(geom.row_frac(lat)) else {
                    continue;
                };
                let src = &grid[row as usize * nx..][..nx];
                let dst = &mut out[oy as usize * width as usize..][..width as usize];
                for (d, col) in dst.iter_mut().zip(&cols) {
                    if let Some(col) = col {
                        *d = src[*col as usize];
                    }
                }
            }
        }
        OutputCrs::Projected { .. } => {
            let to_world = |fx: f64, fy: f64| output_crs.project_node(bbox, fx, fy);
            let to_cell = |lon: f64, lat: f64| (geom.col_frac(lon), geom.row_frac(lat));
            let (projection, window) = if geom.is_global() {
                let window = (0, width.saturating_sub(1), 0, height.saturating_sub(1));
                let projection = ProjectionGrid::build_2d_periodic(
                    width,
                    height,
                    geom.nx,
                    geom.ny,
                    Some(geom.nx as f64),
                    to_world,
                    to_cell,
                );
                (projection, window)
            } else {
                let [w, s, _, n] = geom.bbox();
                let envelope = [w, s, w + geom.lon_span(), n];
                let Some(window) =
                    output_crs.footprint_window_in_image(bbox, envelope, width, height)
                else {
                    return out;
                };
                let projection = ProjectionGrid::build_2d_within(
                    width, height, window, geom.nx, geom.ny, to_world, to_cell,
                );
                (projection, window)
            };
            let (px_lo, px_hi, py_lo, py_hi) = window;
            for oy in py_lo..=py_hi.min(height.saturating_sub(1)) {
                for ox in px_lo..=px_hi.min(width.saturating_sub(1)) {
                    let (c, r) = projection.sample(ox, oy);
                    if let (Some(col), Some(row)) = (geom.col_at(c), geom.row_at(r)) {
                        out[oy as usize * width as usize + ox as usize] =
                            grid[row as usize * nx + col as usize];
                    }
                }
            }
        }
    }
    out
}

impl MapEngine for StationGridEngine {
    fn get_raster_tile(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        output_crs: &OutputCrs,
        parameter: Option<&str>,
        _z: Option<f64>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        let p = self.param_index(parameter)?;
        let times = self.inner.snapshot.load().times.clone();
        // No analysis time yet: an empty tile, and `resolve_time` is `None`
        // so no cache keeps it under a requested time.
        let data = match select_time(&times, time) {
            Some(t) => {
                let grid = self.inner.grid(p, t)?;
                sample_tile(
                    &self.inner.geom,
                    &grid.values,
                    bbox,
                    width,
                    height,
                    output_crs,
                )
            }
            None => vec![f32::NAN; width as usize * height as usize],
        };
        Ok(RasterTile {
            width,
            height,
            values: RasterValues::F32 { data, nodata: None },
        })
    }

    fn raster_info(&self) -> RasterInfo {
        (*self.inner.snapshot.load().info).clone()
    }

    fn raster_info_shared(&self) -> Arc<RasterInfo> {
        self.inner.snapshot.load().info.clone()
    }

    fn resolve_time(
        &self,
        time: Option<DateTime<Utc>>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        select_time(&self.inner.snapshot.load().times, time)
    }

    /// Moves when a revision of the observations is found (module docs).
    fn content_version(&self) -> u64 {
        self.inner.version.load(Ordering::Acquire).max(1)
    }
}

/// EDR over the analysis grids. Every query samples the cached grid the
/// maps draw, at the cell holding each point, so a position query and
/// GetFeatureInfo answer the value the map shows. Without `datetime` a
/// query reads the latest analysis time; an instant must be an analysis
/// time; an interval reads every analysis time inside it.
impl EdrEngine for StationGridEngine {
    fn get_locations(&self) -> Result<Arc<LocationSnapshot>, DataServerError> {
        Ok(LocationSnapshot::empty())
    }

    fn query_location(
        &self,
        location_id: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Err(DataServerError::LocationNotFound(format!(
            "'{}' has no named locations (requested '{location_id}')",
            self.collection_id()
        )))
    }

    fn get_parameters(&self) -> Vec<String> {
        self.parameter_names()
    }

    fn get_parameter_descriptions(&self) -> HashMap<String, ParameterDescription> {
        self.description_map(&(0..self.inner.params.len()).collect::<Vec<_>>())
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        let times = self.inner.snapshot.load().times.clone();
        Some((*times.first()?, *times.last()?))
    }

    fn get_available_times(&self) -> Option<Vec<DateTime<Utc>>> {
        Some(self.inner.snapshot.load().times.to_vec())
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some(self.inner.geom.bbox())
    }

    fn supported_query_types(&self) -> Vec<String> {
        ["position", "area", "radius", "cube"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    fn query_position(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        ds_core::deadline::check()?;
        let (lat, lon) = parse_point_coords(coords)?;
        let params = self.selected(parameters)?;
        let cell = self.inner.geom.cell(lon, lat).ok_or_else(|| {
            DataServerError::LocationNotFound(format!(
                "POINT({lon} {lat}) is outside the analysis grid of '{}'",
                self.collection_id()
            ))
        })?;
        let times = self.query_times(datetime)?;
        let grids = self.grids_for(&params, &times)?;
        let mut ranges = HashMap::new();
        for (&p, grids) in params.iter().zip(&grids) {
            ranges.insert(
                self.inner.params[p].name.clone(),
                NdArray {
                    shape: vec![times.len()],
                    axis_names: vec!["t".into()],
                    values: grids.iter().map(|g| value(g, cell)).collect(),
                    precision: VALUE_PRECISION,
                },
            );
        }
        Ok(CoverageResponse::Single(QueryResult {
            domain: DomainDescription::PointSeries {
                x: lon,
                y: lat,
                t: times,
                z: None,
            },
            parameters: self.description_map(&params),
            ranges,
        }))
    }

    /// A CRS84 grid over the polygon's bbox at the analysis grid's
    /// resolution (at most 256 cells per axis), each cell the analysis cell
    /// under its centre; cells outside the polygon are null.
    fn query_area(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        ds_core::deadline::check()?;
        let polygon = parse_area_coords(coords)?;
        let params = self.selected(parameters)?;
        let geom = self.inner.geom;
        if !polygon.bbox.intersects_bbox(&geom.bbox()) {
            return Err(DataServerError::LocationNotFound(format!(
                "The polygon lies outside the analysis grid of '{}'",
                self.collection_id()
            )));
        }
        let times = self.query_times(datetime)?;
        let axes = polygon.sample_grid(geom.dx, geom.dy, MAX_AREA_DIM);
        let (nx, ny) = axes.dims();
        check_area_budget(times.len(), ny, nx, params.len())?;
        check_mask_budget(nx * ny, &polygon)?;
        let mask = polygon.cell_mask(&axes);
        let cells: Vec<Option<usize>> = axes
            .y
            .iter()
            .flat_map(|&y| axes.x.iter().map(move |&x| (x, y)))
            .zip(&mask)
            .map(|((x, y), &inside)| if inside { geom.cell(x, y) } else { None })
            .collect();
        if cells.iter().all(Option::is_none) {
            return Err(DataServerError::LocationNotFound(format!(
                "No cell of the area lies on the analysis grid of '{}'",
                self.collection_id()
            )));
        }
        self.grid_response(&params, times, axes.x, axes.y, &cells)
    }

    /// The analysis cells whose centres lie in the bbox, or `resolution`
    /// positions per axis from edge to edge taking the cell under each.
    /// The grid has no vertical axis, so `z` is ignored.
    fn query_cube(
        &self,
        bbox: &Bbox,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        resolution: CubeResolution,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        ds_core::deadline::check()?;
        let params = self.selected(parameters)?;
        let geom = self.inner.geom;
        let times = self.query_times(datetime)?;
        check_cube_budget(
            times.len(),
            1,
            resolution.y.unwrap_or(1),
            resolution.x.unwrap_or(1),
            params.len(),
        )?;
        let span = if bbox.crosses_antimeridian() {
            bbox.east + 360.0 - bbox.west
        } else {
            bbox.east - bbox.west
        };
        // From the bbox's west edge eastwards, across the antimeridian when
        // the bbox crosses it.
        let x: Vec<f64> = match resolution.x {
            Some(n) => axis_positions(bbox.west, bbox.west + span, n),
            None => {
                let mut cols: Vec<(f64, f64)> = (0..geom.nx)
                    .map(|c| geom.lon(c))
                    .filter(|&lon| bbox.contains(lon, bbox.south))
                    .map(|lon| ((lon - bbox.west).rem_euclid(360.0), lon))
                    .collect();
                cols.sort_by(|a, b| a.0.total_cmp(&b.0));
                cols.into_iter().map(|(_, lon)| lon).collect()
            }
        };
        let y: Vec<f64> = match resolution.y {
            Some(n) => axis_positions(bbox.north, bbox.south, n),
            None => {
                let (lo, hi) = (
                    (geom.row_frac(bbox.north) - 0.5).ceil().max(0.0) as i64,
                    (geom.row_frac(bbox.south) - 0.5)
                        .floor()
                        .min(geom.ny as f64 - 1.0) as i64,
                );
                (lo..=hi).map(|r| geom.lat(r as u32)).collect()
            }
        };
        let x: Vec<f64> = x.into_iter().map(wrap_lon).collect();
        check_cube_budget(times.len(), 1, y.len(), x.len(), params.len())?;
        let mut cells = Vec::with_capacity(x.len() * y.len());
        for &lat in &y {
            for &lon in &x {
                cells.push(geom.cell(lon, lat));
            }
        }
        if cells.iter().all(Option::is_none) {
            return Err(DataServerError::LocationNotFound(format!(
                "The bbox lies outside the analysis grid of '{}'",
                self.collection_id()
            )));
        }
        self.grid_response(&params, times, x, y, &cells)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, h, 0, 0).unwrap()
    }

    #[test]
    fn select_time_is_latest_not_after() {
        let times = [at(0), at(1), at(2)];
        assert_eq!(select_time(&times, None), Some(at(2)));
        assert_eq!(
            select_time(&times, Some(at(1) + Duration::minutes(30))),
            Some(at(1))
        );
        assert_eq!(select_time(&times, Some(at(1))), Some(at(1)));
        assert_eq!(
            select_time(&times, Some(at(0) - Duration::hours(3))),
            Some(at(0))
        );
        assert_eq!(select_time(&[], None), None);
    }
}
