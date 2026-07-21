//! The nowcast derived-collection engine (#522, epic #519).
//!
//! Wraps another collection's `MapEngine` (its "source"): a poll loop on the
//! background runtime watches the source's timesteps, and each new source
//! frame triggers a *generation* — fetch the last few frames on a regular
//! WGS84 grid, estimate a motion field ([`crate::motion`]), and extrapolate
//! the latest frame `horizon`-far into the future ([`crate::advect`]). The
//! stored frames then serve through the normal `MapEngine` raster path, so
//! WMS/Maps/Tiles animation and TIME dimensions work unchanged — with TIME
//! values in the future.
//!
//! Contracts honoured here:
//! - **#507**: `resolve_time` and `get_raster_tile` share the same
//!   generation + timestep selection helpers, so the API-layer cache keys
//!   can't drift from what is actually rendered.
//! - **#521**: every generation is a model run (`reference_time` = the source
//!   frame the extrapolation anchors on); `RasterInfo.reference_times` is
//!   populated so the API layers key their no-TTL caches on the concrete
//!   generation.
//! - **Critical Rule 5**: output→source mapping goes through
//!   `OutputCrs::project_node` / `ProjectionGrid` — the internal grid is a
//!   regular WGS84 grid, so the source-pixel map is affine.
//! - **Critical Rule 6/7**: `poll_loop` runs on the dedicated background
//!   runtime; source fetches (which may do blocking storage I/O internally)
//!   happen only there, never on a request worker.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use chrono::{DateTime, Duration, Utc};
use tokio::sync::watch;

use ds_core::config::NowcastConfig;
use ds_core::datetime::parse_iso8601_duration;
use ds_core::error::DataServerError;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterInfo, RasterTile, RasterValues};
use ds_core::resample::ProjectionGrid;

use crate::advect::advect_u8;
use crate::motion::{estimate_motion, MotionOptions};
use crate::Grid;

/// Fastest cell motion the search window must cover (m/s). 40 m/s ≈ 144 km/h
/// matches the cell-tracker gate in `ds_core::cells`.
const MAX_SPEED_MS: f64 = 40.0;
/// Target search radius (px) on the motion-estimation grid; frames are
/// coarsened until the physical search window fits.
const TARGET_SEARCH_PX: i32 = 24;
/// Trajectory integration substeps per frame interval.
const SUBSTEPS: usize = 4;
/// Hard cap on extrapolated frames per generation.
const MAX_LEADS: usize = 96;

/// Parsed, validated engine configuration.
struct EngineCfg {
    horizon: Duration,
    /// `None` ⇒ use the source cadence.
    step: Option<Duration>,
    history_frames: usize,
    poll_interval: std::time::Duration,
    max_generations: usize,
    max_pixels: usize,
    min_echo: f32,
}

/// The regular WGS84 grid every stored frame lives on.
#[derive(Debug, Clone, Copy)]
struct GridGeom {
    west: f64,
    south: f64,
    east: f64,
    north: f64,
    width: u32,
    height: u32,
}

impl GridGeom {
    /// Unclamped fractional source pixel for a WGS84 position — may fall
    /// outside `[0,w)×[0,h)` or be non-finite; callers bounds-check at sample
    /// time (the `ProjectionGrid` contract). Affine — the grid is linear in
    /// lon/lat by construction.
    #[inline]
    fn frac_px_unclamped(&self, lon: f64, lat: f64) -> (f64, f64) {
        (
            (lon - self.west) / (self.east - self.west) * self.width as f64,
            (self.north - lat) / (self.north - self.south) * self.height as f64,
        )
    }

    /// Bounds-checked source pixel for a WGS84 position; `None` outside.
    #[inline]
    fn frac_px(&self, lon: f64, lat: f64) -> Option<(f64, f64)> {
        if !lon.is_finite() || !lat.is_finite() {
            return None;
        }
        let (fx, fy) = self.frac_px_unclamped(lon, lat);
        if fx < 0.0 || fy < 0.0 || fx >= self.width as f64 || fy >= self.height as f64 {
            return None;
        }
        Some((fx, fy))
    }
}

/// One stored frame: either raw bytes with decode parameters (preferred — 1
/// byte/px end to end) or decoded f32 (sources that return boxed `F64`).
enum FrameData {
    U8 {
        data: Vec<u8>,
        nodata: u8,
        gain: f64,
        offset: f64,
    },
    F32(Vec<f32>),
}

/// One generation: the analysis frame plus extrapolated leads, anchored on a
/// source frame (`reference_time`).
struct Generation {
    reference_time: DateTime<Utc>,
    /// Valid times, ascending: `reference_time`, then the leads.
    times: Vec<DateTime<Utc>>,
    frames: Vec<FrameData>,
    geom: GridGeom,
}

/// Atomically swapped engine state.
struct NowcastState {
    /// Retained generations keyed by reference time (the instances contract).
    generations: BTreeMap<DateTime<Utc>, Arc<Generation>>,
    /// Pre-built snapshot for the O(1) `raster_info()` contract.
    info: RasterInfo,
}

pub struct NowcastEngine {
    collection_id: String,
    source_id: String,
    source: Arc<dyn MapEngine>,
    cfg: EngineCfg,
    state: ArcSwap<NowcastState>,
    shutdown_tx: watch::Sender<()>,
    // Metrics (read by the server's /metrics handler).
    generations_total: AtomicU64,
    generation_failures_total: AtomicU64,
    last_generation_ms: AtomicU64,
    /// Seconds between the source anchor frame and the wall clock at the end
    /// of the last generation (how far the nowcast lags reality).
    source_lag_secs: AtomicU64,
}

impl NowcastEngine {
    /// Build the engine around an already-constructed source engine. The
    /// server wires this in a second pass after all base engines exist.
    pub fn new(
        collection_id: &str,
        source_id: &str,
        source: Arc<dyn MapEngine>,
        config: &NowcastConfig,
    ) -> Result<Self, DataServerError> {
        let horizon = parse_iso8601_duration(&config.horizon)?;
        if horizon <= Duration::zero() {
            return Err(DataServerError::Config(
                "nowcast horizon must be positive".into(),
            ));
        }
        let step = match &config.step {
            Some(s) => {
                let d = parse_iso8601_duration(s)?;
                if d <= Duration::zero() {
                    return Err(DataServerError::Config(
                        "nowcast step must be positive".into(),
                    ));
                }
                Some(d)
            }
            None => None,
        };
        if config.history_frames < 2 {
            return Err(DataServerError::Config(
                "nowcast history_frames must be at least 2 (motion needs a frame pair)".into(),
            ));
        }

        let (shutdown_tx, _) = watch::channel(());
        let source_info = source.raster_info();
        Ok(Self {
            collection_id: collection_id.to_string(),
            source_id: source_id.to_string(),
            source,
            cfg: EngineCfg {
                horizon,
                step,
                history_frames: config.history_frames,
                poll_interval: std::time::Duration::from_secs(config.poll_interval_secs.max(5)),
                max_generations: config.max_generations.max(1),
                max_pixels: config.max_pixels.clamp(65_536, 16_000_000),
                min_echo: config.min_echo as f32,
            },
            state: ArcSwap::from_pointee(NowcastState {
                generations: BTreeMap::new(),
                info: empty_info(&source_info),
            }),
            shutdown_tx,
            generations_total: AtomicU64::new(0),
            generation_failures_total: AtomicU64::new(0),
            last_generation_ms: AtomicU64::new(0),
            source_lag_secs: AtomicU64::new(0),
        })
    }

    pub fn collection_id(&self) -> &str {
        &self.collection_id
    }

    pub fn source_id(&self) -> &str {
        &self.source_id
    }

    /// True once at least one generation has been produced (health gate).
    pub fn has_data(&self) -> bool {
        !self.state.load().generations.is_empty()
    }

    /// Age of the latest generation's anchor frame (health `data_age_secs`).
    pub fn catalog_age(&self) -> Option<chrono::Duration> {
        let state = self.state.load();
        let (&latest, _) = state.generations.iter().next_back()?;
        Some(Utc::now() - latest)
    }

    /// (generations_total, failures_total, last_generation_ms, source_lag_secs,
    /// retained_generations, frames_in_latest) — one snapshot for /metrics.
    pub fn metrics(&self) -> (u64, u64, u64, u64, usize, usize) {
        let state = self.state.load();
        let frames = state
            .generations
            .iter()
            .next_back()
            .map(|(_, g)| g.frames.len())
            .unwrap_or(0);
        (
            self.generations_total.load(Ordering::Relaxed),
            self.generation_failures_total.load(Ordering::Relaxed),
            self.last_generation_ms.load(Ordering::Relaxed),
            self.source_lag_secs.load(Ordering::Relaxed),
            state.generations.len(),
            frames,
        )
    }

    /// Signal the poll loop to exit (server reload/shutdown).
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(());
    }

    /// Poll the source for a new frame; spawn on the BACKGROUND runtime only.
    pub async fn poll_loop(&self) {
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        let mut ticker = tokio::time::interval(self.cfg.poll_interval);
        // A generation can outlast the interval on big grids; don't replay
        // missed ticks as a burst afterwards (#443 pattern).
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.changed() => {
                    tracing::info!("[{}] nowcast poll loop shutting down", self.collection_id);
                    break;
                }
                _ = ticker.tick() => self.poll_once(),
            }
        }
    }

    /// One poll cycle: regenerate iff the source's latest frame moved.
    /// Public so tests (and pre-warm paths) can drive generations without the
    /// loop.
    pub fn poll_once(&self) {
        let source_info = self.source.raster_info();
        let Some(&anchor) = source_info.times.last() else {
            return; // source has no data yet
        };
        {
            let state = self.state.load();
            if state.generations.contains_key(&anchor) {
                return; // already generated for this source frame
            }
        }
        let started = std::time::Instant::now();
        match self.generate(&source_info, anchor) {
            Ok(generation) => {
                let elapsed_ms = started.elapsed().as_millis() as u64;
                let old = self.state.load();
                let mut generations = old.generations.clone();
                generations.insert(anchor, Arc::new(generation));
                while generations.len() > self.cfg.max_generations {
                    let oldest = *generations.keys().next().unwrap();
                    generations.remove(&oldest);
                }
                let info = build_info(&source_info, &generations);
                self.state
                    .store(Arc::new(NowcastState { generations, info }));
                self.generations_total.fetch_add(1, Ordering::Relaxed);
                self.last_generation_ms.store(elapsed_ms, Ordering::Relaxed);
                let lag = (Utc::now() - anchor).num_seconds().max(0) as u64;
                self.source_lag_secs.store(lag, Ordering::Relaxed);
                tracing::info!(
                    "[{}] nowcast generation for {} in {}ms",
                    self.collection_id,
                    anchor,
                    elapsed_ms
                );
            }
            Err(e) => {
                self.generation_failures_total
                    .fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    "[{}] nowcast generation for {} failed: {}",
                    self.collection_id,
                    anchor,
                    e
                );
            }
        }
    }

    /// Produce one generation anchored on `anchor` (the source's latest
    /// frame). Runs on the background runtime.
    fn generate(
        &self,
        source_info: &RasterInfo,
        anchor: DateTime<Utc>,
    ) -> Result<Generation, DataServerError> {
        let extent = source_info.spatial_extent.ok_or_else(|| {
            DataServerError::Engine("nowcast source reports no spatial extent".into())
        })?;
        let n = source_info.times.len();
        if n < 2 {
            return Err(DataServerError::Engine(
                "nowcast needs at least 2 source frames for motion".into(),
            ));
        }
        let history: Vec<DateTime<Utc>> =
            source_info.times[n.saturating_sub(self.cfg.history_frames)..].to_vec();
        let prev_time = history[history.len() - 2];
        let interval = anchor - prev_time;
        if interval <= Duration::zero() {
            return Err(DataServerError::Engine(
                "nowcast source times are not strictly ascending".into(),
            ));
        }

        // Working grid: the source's native cell counts, halved until the
        // pixel budget fits (FMI's 250 m composite lands at ~1 km here).
        let [mut w, mut h] = source_info.grid_size.unwrap_or([1024, 1024]);
        while (w as usize) * (h as usize) > self.cfg.max_pixels && w > 64 && h > 64 {
            w /= 2;
            h /= 2;
        }
        let geom = GridGeom {
            west: extent[0],
            south: extent[1],
            east: extent[2],
            north: extent[3],
            width: w,
            height: h,
        };

        // Fetch the anchor + previous frame on the working grid.
        let prev = self.fetch_frame(&geom, prev_time)?;
        let analysis = self.fetch_frame(&geom, anchor)?;
        let prev_f32 = frame_to_grid(&prev, w as usize, h as usize);
        let analysis_f32 = frame_to_grid(&analysis, w as usize, h as usize);

        // Deliberate scale handling (not the phase-0 accident): estimate
        // motion on a grid coarse enough that the physical search window
        // (MAX_SPEED × interval) fits in TARGET_SEARCH_PX, then scale the
        // field back to working-grid units.
        let mid_lat = ((geom.south + geom.north) / 2.0).to_radians();
        let px_meters =
            ((geom.east - geom.west) * 111_320.0 * mid_lat.cos().abs().max(0.05)) / w as f64;
        let max_shift_px = MAX_SPEED_MS * interval.num_seconds() as f64 / px_meters.max(1.0);
        let mut factor = 1u32;
        while max_shift_px / factor as f64 > TARGET_SEARCH_PX as f64
            && (w / (factor * 2)) >= 128
            && (h / (factor * 2)) >= 128
        {
            factor *= 2;
        }
        let search_radius =
            ((max_shift_px / factor as f64).ceil() as i32).clamp(4, TARGET_SEARCH_PX * 2);
        let opts = MotionOptions {
            search_radius,
            min_echo: self.cfg.min_echo,
            ..MotionOptions::default()
        };
        // Vectors come out in pixels per source interval; the leads below are
        // expressed in the same interval unit, so no time scaling is needed.
        let field = if factor > 1 {
            let prev_coarse = downsample(&prev_f32, factor as usize);
            let analysis_coarse = downsample(&analysis_f32, factor as usize);
            let mut f = estimate_motion(&prev_coarse, &analysis_coarse, &opts);
            // Coarse-grid vectors/blocks → working-grid units.
            f.block *= factor as usize;
            for v in f.u.iter_mut().chain(f.v.iter_mut()) {
                *v *= factor as f32;
            }
            f
        } else {
            estimate_motion(&prev_f32, &analysis_f32, &opts)
        };

        // Lead schedule.
        let step = self.cfg.step.unwrap_or(interval);
        let k = ((self.cfg.horizon.num_seconds() as f64 / step.num_seconds().max(1) as f64).ceil()
            as usize)
            .clamp(1, MAX_LEADS);

        let mut times = Vec::with_capacity(k + 1);
        let mut frames = Vec::with_capacity(k + 1);
        times.push(anchor);
        frames.push(analysis);
        for i in 1..=k {
            let lead_time = anchor + step * (i as i32);
            let lead_intervals =
                (step.num_seconds() as f64 * i as f64 / interval.num_seconds() as f64) as f32;
            let frame = match &frames[0] {
                FrameData::U8 {
                    data,
                    nodata,
                    gain,
                    offset,
                } => FrameData::U8 {
                    data: advect_u8(
                        data,
                        w as usize,
                        h as usize,
                        *nodata,
                        &field,
                        lead_intervals,
                        SUBSTEPS,
                    ),
                    nodata: *nodata,
                    gain: *gain,
                    offset: *offset,
                },
                FrameData::F32(_) => {
                    let advected =
                        crate::advect::advect(&analysis_f32, &field, lead_intervals, SUBSTEPS);
                    FrameData::F32(advected.data)
                }
            };
            times.push(lead_time);
            frames.push(frame);
        }

        Ok(Generation {
            reference_time: anchor,
            times,
            frames,
            geom,
        })
    }

    /// Fetch one source frame on the working WGS84 grid.
    fn fetch_frame(
        &self,
        geom: &GridGeom,
        time: DateTime<Utc>,
    ) -> Result<FrameData, DataServerError> {
        let tile = self.source.get_raster_tile(
            [geom.west, geom.south, geom.east, geom.north],
            geom.width,
            geom.height,
            Some(time),
            &OutputCrs::Wgs84,
            None,
            None,
            None,
        )?;
        if (tile.width, tile.height) != (geom.width, geom.height) {
            return Err(DataServerError::Engine(format!(
                "nowcast source returned {}x{}, requested {}x{}",
                tile.width, tile.height, geom.width, geom.height
            )));
        }
        Ok(match tile.values {
            // Keep the raw-byte form only when a nodata byte exists — the
            // advected inflow boundary needs one to stay transparent.
            RasterValues::U8 {
                data,
                nodata: Some(nodata),
                gain,
                offset,
            } => FrameData::U8 {
                data,
                nodata,
                gain,
                offset,
            },
            RasterValues::U8 {
                data,
                nodata: None,
                gain,
                offset,
            } => FrameData::F32(
                data.into_iter()
                    .map(|raw| (raw as f64 * gain + offset) as f32)
                    .collect(),
            ),
            RasterValues::F64(values) => FrameData::F32(
                values
                    .into_iter()
                    .map(|v| v.map(|x| x as f32).unwrap_or(f32::NAN))
                    .collect(),
            ),
        })
    }

    /// Generation selection shared by `get_raster_tile` and `resolve_time`
    /// (#521): `None` ⇒ latest, `Some` ⇒ exact.
    fn select_generation(
        state: &NowcastState,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<Arc<Generation>> {
        match reference_time {
            None => state.generations.iter().next_back().map(|(_, g)| g.clone()),
            Some(rt) => state.generations.get(&rt).cloned(),
        }
    }

    /// Timestep selection shared by `get_raster_tile` and `resolve_time`
    /// (#507): latest-not-after, clamped to the first frame.
    fn select_time(generation: &Generation, time: Option<DateTime<Utc>>) -> DateTime<Utc> {
        match time {
            None => *generation.times.last().expect("generation has frames"),
            Some(t) => generation
                .times
                .iter()
                .rev()
                .find(|&&ts| ts <= t)
                .copied()
                .unwrap_or(generation.times[0]),
        }
    }
}

/// `RasterInfo` before the first generation: source geometry, no times.
fn empty_info(source_info: &RasterInfo) -> RasterInfo {
    RasterInfo {
        native_crs: "CRS:84".into(),
        spatial_extent: source_info.spatial_extent,
        times: Vec::new(),
        parameter: source_info.parameter.clone(),
        unit: source_info.unit.clone(),
        parameters: Vec::new(),
        vertical: None,
        grid_size: None,
        layer_subtitle: None,
        reference_times: Vec::new(),
    }
}

/// Snapshot `RasterInfo` from the retained generations (O(1) to serve).
fn build_info(
    source_info: &RasterInfo,
    generations: &BTreeMap<DateTime<Utc>, Arc<Generation>>,
) -> RasterInfo {
    let latest = generations.iter().next_back().map(|(_, g)| g);
    RasterInfo {
        native_crs: "CRS:84".into(),
        spatial_extent: source_info.spatial_extent,
        times: latest.map(|g| g.times.clone()).unwrap_or_default(),
        parameter: source_info.parameter.clone(),
        unit: source_info.unit.clone(),
        parameters: Vec::new(),
        vertical: None,
        grid_size: latest.map(|g| [g.geom.width, g.geom.height]),
        layer_subtitle: None,
        // Ascending by BTreeMap order — the #521 cache-pinning contract.
        reference_times: generations.keys().copied().collect(),
    }
}

/// NaN-for-nodata f32 view of a stored frame (motion estimation input).
fn frame_to_grid(frame: &FrameData, width: usize, height: usize) -> Grid {
    let data: Vec<f32> = match frame {
        FrameData::U8 {
            data,
            nodata,
            gain,
            offset,
        } => data
            .iter()
            .map(|&raw| {
                if raw == *nodata {
                    f32::NAN
                } else {
                    (raw as f64 * *gain + *offset) as f32
                }
            })
            .collect(),
        FrameData::F32(values) => values.clone(),
    };
    Grid::new(width, height, data)
}

/// Nearest-neighbour 1/f downsample (motion estimation only — stored frames
/// stay full resolution).
fn downsample(grid: &Grid, factor: usize) -> Grid {
    let w = (grid.width / factor).max(1);
    let h = (grid.height / factor).max(1);
    let mut data = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            data.push(grid.at(x * factor, y * factor));
        }
    }
    Grid::new(w, h, data)
}

impl MapEngine for NowcastEngine {
    fn get_raster_tile(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        output_crs: &OutputCrs,
        _parameter: Option<&str>,
        _z: Option<f64>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        let state = self.state.load();
        let generation =
            Self::select_generation(&state, reference_time).ok_or_else(
                || match reference_time {
                    Some(rt) => DataServerError::Engine(format!(
                        "nowcast generation {rt} is no longer retained"
                    )),
                    None => DataServerError::Engine("nowcast has no generations yet".into()),
                },
            )?;
        let resolved = Self::select_time(&generation, time);
        let idx = generation
            .times
            .iter()
            .position(|&t| t == resolved)
            .expect("select_time returns a member of times");
        let frame = &generation.frames[idx];
        let geom = generation.geom;
        let n = (width as usize) * (height as usize);

        // Output→source mapping. The internal grid is linear in lon/lat, so
        // `frac_px` is affine; Wgs84/WebMercator nodes are cheap to project
        // per pixel, while a projected CRS goes through the coarse
        // `ProjectionGrid` (Critical Rule 5), mirroring engine-zarr.
        let src_index = |lon: f64, lat: f64| -> Option<usize> {
            geom.frac_px(lon, lat)
                .map(|(fx, fy)| fy as usize * geom.width as usize + fx as usize)
        };
        let mut indices: Vec<Option<usize>> = Vec::with_capacity(n);
        match output_crs {
            OutputCrs::Wgs84 | OutputCrs::WebMercator => {
                for row in 0..height {
                    let fy = (row as f64 + 0.5) / height as f64;
                    for col in 0..width {
                        let fx = (col as f64 + 0.5) / width as f64;
                        let (lon, lat) = output_crs.project_node(bbox, fx, fy);
                        indices.push(src_index(lon, lat));
                    }
                }
            }
            OutputCrs::Projected { .. } => {
                let grid = ProjectionGrid::build_2d(
                    width,
                    height,
                    geom.width,
                    geom.height,
                    |fx, fy| output_crs.project_node(bbox, fx, fy),
                    |lon, lat| geom.frac_px_unclamped(lon, lat),
                );
                let env = [geom.west, geom.south, geom.east, geom.north];
                let (px_lo, px_hi, py_lo, py_hi) =
                    output_crs.footprint_pixel_window(bbox, env, width, height);
                for oy in 0..height {
                    let in_y = oy >= py_lo && oy <= py_hi;
                    for ox in 0..width {
                        if !in_y || ox < px_lo || ox > px_hi {
                            indices.push(None);
                            continue;
                        }
                        let (fx, fy) = grid.sample(ox, oy);
                        if fx < 0.0
                            || fy < 0.0
                            || fx >= geom.width as f64
                            || fy >= geom.height as f64
                            || !fx.is_finite()
                            || !fy.is_finite()
                        {
                            indices.push(None);
                        } else {
                            indices.push(Some(fy as usize * geom.width as usize + fx as usize));
                        }
                    }
                }
            }
        }

        let values = match frame {
            FrameData::U8 {
                data,
                nodata,
                gain,
                offset,
            } => RasterValues::U8 {
                data: indices
                    .iter()
                    .map(|src| src.map(|i| data[i]).unwrap_or(*nodata))
                    .collect(),
                nodata: Some(*nodata),
                gain: *gain,
                offset: *offset,
            },
            FrameData::F32(field) => RasterValues::F64(
                indices
                    .iter()
                    .map(|src| {
                        src.and_then(|i| {
                            let v = field[i];
                            v.is_finite().then_some(v as f64)
                        })
                    })
                    .collect(),
            ),
        };
        Ok(RasterTile {
            width,
            height,
            values,
        })
    }

    fn raster_info(&self) -> RasterInfo {
        self.state.load().info.clone()
    }

    fn resolve_time(
        &self,
        time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        let state = self.state.load();
        let generation = Self::select_generation(&state, reference_time)?;
        Some(Self::select_time(&generation, time))
    }

    fn resolve_reference_time(
        &self,
        _time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        // The run-axis cache-key authority (#521) — load-bearing here: every
        // generation rewrites ALL future valid times, so a `None`-keyed cache
        // entry would freeze the first generation's pixels within minutes.
        // Same `select_generation` the render path uses (`None` ⇒ latest
        // generation, `Some` ⇒ exact); an unretained pin echoes back (the
        // render errors and caches nothing).
        let state = self.state.load();
        Self::select_generation(&state, reference_time)
            .map(|g| Some(g.reference_time))
            .unwrap_or(reference_time)
    }
}
