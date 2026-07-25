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
use ds_core::feature::{Feature, FeaturePage, FeatureQuery, Geometry, PropertyValue};
use ds_core::feature_engine::FeatureEngine;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterInfo, RasterTile, RasterValues};
use ds_core::resample::ProjectionGrid;

use crate::advect::TrajectoryIntegrator;
use crate::cells2d::{advance_tracks, CellTrack, CELL_MIN_AREA_PX, CELL_THRESHOLD_DBZ};
use crate::motion::{estimate_motion_multi, MotionField, MotionOptions};
use crate::objects::{segment_cells_labeled, PixelScale};
use crate::tendency::EFOLD_INTERVALS;
use crate::Grid;

/// Fastest cell motion the search window must cover (m/s). 40 m/s ≈ 144 km/h
/// matches the cell-tracker gate in `ds_core::cells`.
const MAX_SPEED_MS: f64 = 40.0;
/// Target search radius (px) on the motion-estimation grid; frames are
/// coarsened until the physical search window fits. 48 keeps the FMI
/// composite's ~500 m working grid uncoarsened (~16 km motion blocks
/// instead of ~25 km — small convective cells get a closer-fitting
/// vector), affordable since #529 made generation cost linear in leads.
const TARGET_SEARCH_PX: i32 = 48;
/// Temporal EMA weights for blending each generation's motion field with
/// the previous one (#524): the new field keeps this share, per block.
/// Measured blocks carry fresh information; filled blocks are inferred and
/// lean harder on history. 0.7 ≈ one-and-a-half generations of memory —
/// enough to damp single-pair convective noise without lagging a genuine
/// wind shift by more than a couple of cadence intervals.
const EMA_ALPHA_MEASURED: f32 = 0.7;
const EMA_ALPHA_FILLED: f32 = 0.4;
/// Trajectory integration substeps per frame interval.
const SUBSTEPS: usize = 4;
/// Hard cap on extrapolated frames per generation.
const MAX_LEADS: usize = 96;
/// Hard cap on `history_frames`: each is one sequential blocking source
/// fetch per generation, and pair-averaging saturates after a few pairs.
const MAX_HISTORY_FRAMES: usize = 8;

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
    growth_decay: bool,
}

/// The regular WGS84 grid every stored frame lives on.
#[derive(Debug, Clone, Copy, PartialEq)]
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
    /// The (blended) motion field this generation advected along — the
    /// EMA history for the NEXT generation (#524).
    field: MotionField,
    /// Per-band growth/decay profile (measured + EMA'd) — the history for
    /// the NEXT generation (#546).
    /// Tracked cells of this generation's analysis frame (#544/#546).
    cells: Arc<Vec<CellTrack>>,
}

/// Atomically swapped engine state.
struct NowcastState {
    /// Retained generations keyed by reference time (the instances contract).
    generations: BTreeMap<DateTime<Utc>, Arc<Generation>>,
    /// Pre-built snapshot for the O(1) `raster_info()` contract.
    info: RasterInfo,
    /// Tracked cells of the latest generation's analysis frame (#544).
    cells: Arc<Vec<CellTrack>>,
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
    /// One-shot latch for the lead-cap warning (source-cadence default step
    /// only; an explicit step is validated at construction).
    lead_cap_warned: std::sync::atomic::AtomicBool,
    /// Latest realized lead-1 skill (#542): CSI ×1000 of the previous
    /// generation's prediction for the newest analysis, and the persistence
    /// baseline. `u64::MAX` = not yet measured.
    lead_csi_permille: AtomicU64,
    lead_persistence_csi_permille: AtomicU64,
    /// Monotonic id source for cell tracks (#544).
    next_track_id: AtomicU64,
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
                // Fail fast on a horizon/step pair the per-generation lead
                // cap would silently truncate (root rule: no silent caps).
                // With `step = None` the cadence is unknown until data
                // arrives; that case warns at generation time instead.
                let step_secs = d.num_seconds().max(1);
                let leads = (horizon.num_seconds() + step_secs - 1) / step_secs;
                if leads > MAX_LEADS as i64 {
                    return Err(DataServerError::Config(format!(
                        "nowcast horizon/step = {leads} lead frames exceeds the cap of \
                         {MAX_LEADS}; increase step or shorten horizon"
                    )));
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
        // Each history frame is one sequential blocking source fetch per
        // generation (Critical Rule 9: never loop unbounded blocking I/O on
        // one thread) — and motion averaging saturates after a few pairs
        // anyway. Fail fast rather than silently clamp.
        if config.history_frames > MAX_HISTORY_FRAMES {
            return Err(DataServerError::Config(format!(
                "nowcast history_frames = {} exceeds the cap of {MAX_HISTORY_FRAMES} \
                 (each frame is one blocking source fetch per generation)",
                config.history_frames
            )));
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
                growth_decay: config.growth_decay,
            },
            state: ArcSwap::from_pointee(NowcastState {
                generations: BTreeMap::new(),
                info: empty_info(&source_info),
                cells: Arc::new(Vec::new()),
            }),
            shutdown_tx,
            generations_total: AtomicU64::new(0),
            generation_failures_total: AtomicU64::new(0),
            last_generation_ms: AtomicU64::new(0),
            source_lag_secs: AtomicU64::new(0),
            lead_cap_warned: std::sync::atomic::AtomicBool::new(false),
            lead_csi_permille: AtomicU64::new(u64::MAX),
            lead_persistence_csi_permille: AtomicU64::new(u64::MAX),
            next_track_id: AtomicU64::new(1),
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

    /// Latest realized lead-1 skill (#542): `(nowcast_csi, persistence_csi)`
    /// as CSI ×1000, `None` before the second generation has been scored.
    pub fn skill_permille(&self) -> Option<(u64, u64)> {
        let f = self.lead_csi_permille.load(Ordering::Relaxed);
        let p = self.lead_persistence_csi_permille.load(Ordering::Relaxed);
        (f != u64::MAX && p != u64::MAX).then_some((f, p))
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
                // V2.1 (#542): score the PREVIOUS generation's prediction for
                // this anchor against the fresh analysis, next to persistence
                // — continuous skill telemetry, so a quality regression shows
                // in ops instead of only at review time.
                if let Some((_, prev)) = old.generations.iter().next_back() {
                    self.score_previous_generation(prev, &generation, anchor);
                }
                let mut generations = old.generations.clone();
                generations.insert(anchor, Arc::new(generation));
                while generations.len() > self.cfg.max_generations {
                    let oldest = *generations.keys().next().unwrap();
                    generations.remove(&oldest);
                }
                let info = build_info(&source_info, &generations);
                let cells = generations
                    .get(&anchor)
                    .expect("just inserted")
                    .cells
                    .clone();
                self.state.store(Arc::new(NowcastState {
                    generations,
                    info,
                    cells,
                }));
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

    /// Score the previous generation's prediction for `anchor` (usually its
    /// lead-1 frame) against the new generation's analysis frame, plus the
    /// persistence baseline (previous analysis vs new analysis). Pixel CSI
    /// at the `min_echo` threshold, stored ×1000 in the skill gauges.
    /// Skipped when geometries differ (config change) or the previous
    /// generation never predicted this anchor.
    fn score_previous_generation(
        &self,
        prev: &Generation,
        current: &Generation,
        anchor: DateTime<Utc>,
    ) {
        // Full-geometry guard: same pixel dimensions over a SHIFTED extent
        // (source coverage change, config reload) would silently compare
        // frames that aren't co-located — bail on any geometry difference.
        if prev.geom != current.geom {
            return;
        }
        // STRICT lead-1 only: after a skipped generation (transient failure,
        // source cadence gap) the previous generation's match for `anchor`
        // sits at a deeper lead — reporting that under the lead1 gauge names
        // would silently mix leads. Better no measurement than a mislabeled
        // one.
        let idx = 1;
        if prev.times.get(idx) != Some(&anchor) {
            return;
        }
        let w = current.geom.width as usize;
        let h = current.geom.height as usize;
        let observed = frame_to_grid(&current.frames[0], w, h);
        let predicted = frame_to_grid(&prev.frames[idx], w, h);
        let persisted = frame_to_grid(&prev.frames[0], w, h);
        let threshold = self.cfg.min_echo;
        let forecast_csi = crate::skill::score(&predicted, &observed, threshold).csi();
        let persistence_csi = crate::skill::score(&persisted, &observed, threshold).csi();
        // The measurement is the PAIR: updating one gauge while the other
        // keeps a stale value from an earlier generation would fabricate a
        // skill collapse (e.g. a dry scene where only the extrapolation has
        // a few spurious echo pixels: forecast CSI Some(0), persistence
        // None). Either both update from the same frame pair, or neither.
        let to_permille = |c: Option<f64>| c.map(|v| (v * 1000.0).round() as u64);
        if let (Some(f), Some(p)) = (to_permille(forecast_csi), to_permille(persistence_csi)) {
            self.lead_csi_permille.store(f, Ordering::Relaxed);
            self.lead_persistence_csi_permille
                .store(p, Ordering::Relaxed);
        }
        tracing::info!(
            "[{}] nowcast skill vs {anchor}: CSI {} (persistence {}) at >= {threshold} (lead {} min)",
            self.collection_id,
            forecast_csi.map(|v| format!("{v:.3}")).unwrap_or_else(|| "n/a".into()),
            persistence_csi.map(|v| format!("{v:.3}")).unwrap_or_else(|| "n/a".into()),
            (anchor - prev.reference_time).num_minutes(),
        );
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
        // Reject sub-second cadence too, not just non-ascending times:
        // `num_seconds()` truncates, so a <1 s interval would evaluate to 0
        // in the lead-interval division below — Infinity leads would explode
        // `advect`'s substep count and wedge the poll runtime.
        if interval < Duration::seconds(1) {
            return Err(DataServerError::Engine(format!(
                "nowcast source cadence must be at least 1 second and ascending \
                 (got {} ms between {prev_time} and {anchor})",
                interval.num_milliseconds()
            )));
        }

        // Working grid: the source's native cell counts, halved until the
        // pixel budget fits (FMI's 250 m composite lands at ~1 km here).
        let [mut w, mut h] = source_info.grid_size.unwrap_or([1024, 1024]);
        // Halve the larger axis until the budget fits — guaranteed to
        // terminate and to hold for any aspect ratio (a per-axis floor
        // instead would let an elongated grid blow past the budget).
        while (w as usize) * (h as usize) > self.cfg.max_pixels && w.max(h) > 1 {
            if w >= h {
                w = (w / 2).max(1);
            } else {
                h = (h / 2).max(1);
            }
        }
        let geom = GridGeom {
            west: extent[0],
            south: extent[1],
            east: extent[2],
            north: extent[3],
            width: w,
            height: h,
        };

        // Fetch the motion history + anchor frame on the working grid,
        // every fetch pinned to ONE resolved source run (see fetch_frame).
        // `history` holds up to `history_frames` timestamps ending at the
        // anchor; a fetch failure on an OLDER frame degrades to fewer pairs
        // rather than failing the generation (the last pair is mandatory).
        let source_run = self.source.resolve_reference_time(Some(anchor), None);
        let mut motion_frames: Vec<(DateTime<Utc>, Grid)> = Vec::with_capacity(history.len());
        for &t in history.iter().rev().skip(1).rev() {
            // All but the anchor (fetched below as the stored analysis frame).
            match self.fetch_frame(&geom, t, source_run) {
                Ok(f) => motion_frames.push((t, frame_to_grid(&f, w as usize, h as usize))),
                Err(e) if t != prev_time => {
                    tracing::debug!(
                        "[{}] nowcast history frame {t} unavailable ({e}); \
                         continuing with fewer motion pairs",
                        self.collection_id
                    );
                }
                Err(e) => return Err(e),
            }
        }
        let analysis = self.fetch_frame(&geom, anchor, source_run)?;
        let analysis_f32 = frame_to_grid(&analysis, w as usize, h as usize);
        motion_frames.push((anchor, analysis_f32));
        let analysis_f32 = &motion_frames.last().expect("anchor pushed").1;

        // Deliberate scale handling (not the phase-0 accident): estimate
        // motion on a grid coarse enough that the physical search window
        // (MAX_SPEED × interval) fits in TARGET_SEARCH_PX, then scale the
        // field back to working-grid units.
        let (px_km_x, _) =
            crate::lonlat_grid_km_per_px([geom.west, geom.south, geom.east, geom.north], w, h);
        let px_meters = px_km_x * 1000.0;
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

        // Multi-pair estimation (#524): every consecutive history pair
        // contributes measurements, scaled to px-per-LAST-interval so a
        // mildly irregular cadence still averages correctly (a degenerate
        // pair interval skips that pair inside estimate_motion_multi).
        // Vectors come out in pixels per source interval; the leads below
        // are expressed in the same interval unit — no time scaling needed.
        let interval_secs = interval.num_seconds().max(1) as f32;
        let scales: Vec<f32> = motion_frames
            .windows(2)
            .map(|p| interval_secs / (p[1].0 - p[0].0).num_seconds().max(0) as f32)
            .collect();
        let mut field = if factor > 1 {
            let coarse: Vec<Grid> = motion_frames
                .iter()
                .map(|(_, g)| downsample(g, factor as usize))
                .collect();
            let coarse_refs: Vec<&Grid> = coarse.iter().collect();
            let mut f = estimate_motion_multi(&coarse_refs, &scales, &opts);
            // Coarse-grid vectors/blocks → working-grid units.
            f.block *= factor as usize;
            for v in f.u.iter_mut().chain(f.v.iter_mut()) {
                *v *= factor as f32;
            }
            f
        } else {
            let refs: Vec<&Grid> = motion_frames.iter().map(|(_, g)| g).collect();
            estimate_motion_multi(&refs, &scales, &opts)
        };

        // Temporal EMA with the previous generation's field (#524): stops
        // the per-generation motion-noise oscillation that reads as
        // rubber-banding in animations. No-op when the block grid changed
        // (blend_with_previous checks dims).
        {
            let state = self.state.load();
            if let Some((_, latest)) = state.generations.iter().next_back() {
                field.blend_with_previous(&latest.field, EMA_ALPHA_MEASURED, EMA_ALPHA_FILLED);
            }
        }

        // Cell tracking (#544) — now inside generate() so the growth/decay
        // measurement (#546 iteration 1) can condition on per-cell classes.
        let (blobs, labels) =
            segment_cells_labeled(analysis_f32, CELL_THRESHOLD_DBZ, CELL_MIN_AREA_PX);
        let (kx, ky) = crate::lonlat_grid_km_per_px(
            [geom.west, geom.south, geom.east, geom.north],
            geom.width,
            geom.height,
        );
        let scale = PixelScale {
            x: kx as f32,
            y: ky as f32,
        };
        let prev_state = self.state.load();
        let prev_latest = prev_state.generations.iter().next_back();
        // Displacement spans the previous generation's anchor → this one
        // (2× cadence after a skipped generation); field vectors span the
        // source interval. Track continuity requires an unchanged grid
        // (geometry change ⇒ reset, cells restart as newborns).
        let displacement_secs = prev_latest
            .map(|(&p, _)| (anchor - p).num_seconds() as f32)
            .unwrap_or_else(|| interval.num_seconds() as f32);
        let previous_cells: &[CellTrack] = match prev_latest {
            Some((_, prev)) if prev.geom == geom => &prev.cells,
            _ => &[],
        };
        let cells = Arc::new(advance_tracks(
            previous_cells,
            blobs,
            scale,
            &field,
            displacement_secs,
            interval.num_seconds() as f32,
            || self.next_track_id.fetch_add(1, Ordering::Relaxed),
        ));

        // Per-pixel LABEL map + per-cell tendency table (#546 iteration 1
        // pivot): each pixel of tracked cell L gets L's OWN EMA'd intensity
        // tendency (a tracker-level signal, robust to pixel misalignment);
        // background and newborns get 0 = pure advection. Labels are capped
        // at 255 to ride the u8 trajectory sampler — cells beyond that (rare;
        // FMI convective days run ~150) fall back to pure advection.
        let label_map: Vec<u8> = labels.iter().map(|&l| l.min(255) as u8).collect();
        let mut cell_tendency = [0f32; 256];
        for (i, t) in cells.iter().take(255).enumerate() {
            // Per-interval units to pair with lead_intervals below.
            cell_tendency[i + 1] = t.intensity_tendency * interval.num_seconds() as f32;
        }

        // Lead schedule. An explicit step was validated against MAX_LEADS at
        // construction; the source-cadence default can still exceed it (a
        // fast-cadence source under a long horizon), so make the truncation
        // visible instead of silent (root rule: no silent caps). Warned once
        // per engine — the same clamp would otherwise log every generation.
        let step = self.cfg.step.unwrap_or(interval);
        let wanted = ((self.cfg.horizon.num_seconds() as f64 / step.num_seconds().max(1) as f64)
            .ceil() as usize)
            .max(1);
        let k = wanted.min(MAX_LEADS);
        if wanted > MAX_LEADS && !self.lead_cap_warned.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                "[{}] nowcast horizon needs {wanted} lead frames at the source cadence but \
                 the cap is {MAX_LEADS}: serving only {} of the configured horizon — set an \
                 explicit larger `step` or shorten `horizon`",
                self.collection_id,
                format_args!("{}s", step.num_seconds() * MAX_LEADS as i64),
            );
        }

        let mut times = Vec::with_capacity(k + 1);
        let mut frames = Vec::with_capacity(k + 1);
        times.push(anchor);
        frames.push(analysis);
        // One trajectory set, extended lead by lead (#528): each iteration
        // integrates only the newest `delta` of every pixel's backward
        // trajectory, making the whole schedule O(leads) in field samples
        // instead of the O(leads²) of per-lead from-scratch integration
        // (67 s per prod generation at 2.27 Mpx × 24 leads). With the
        // default source-cadence step (`delta == 1.0`) this reproduces the
        // one-shot trajectories bit-for-bit; an explicit fractional `step`
        // integrates at least as finely (see `TrajectoryIntegrator` docs).
        //
        // `interval` ≥ 1 s is guaranteed by the cadence guard above;
        // `.max(1)` keeps the division safe against future edits.
        let delta = (step.num_seconds() as f64 / interval.num_seconds().max(1) as f64) as f32;
        let mut trajectories = TrajectoryIntegrator::new(w as usize, h as usize, &field);
        for i in 1..=k {
            let lead_time = anchor + step * (i as i32);
            trajectories.advance(delta, SUBSTEPS);
            let lead_intervals = delta * i as f32;
            let frame = match &frames[0] {
                FrameData::U8 {
                    data,
                    nodata,
                    gain,
                    offset,
                } => {
                    let mut sampled = trajectories.sample_u8(data, *nodata);
                    if self.cfg.growth_decay {
                        let moved = trajectories.sample_u8(&label_map, 0);
                        let damp =
                            EFOLD_INTERVALS * (1.0 - (-lead_intervals / EFOLD_INTERVALS).exp());
                        for (raw, k) in sampled.iter_mut().zip(&moved) {
                            if *raw == *nodata || *k == 0 {
                                continue;
                            }
                            let v = (f64::from(*raw) * *gain + *offset) as f32;
                            let adjusted = v + cell_tendency[*k as usize] * damp;
                            if adjusted != v {
                                let mut r = ((f64::from(adjusted) - *offset) / *gain)
                                    .round()
                                    .clamp(0.0, 255.0)
                                    as u8;
                                if r == *nodata {
                                    // Never collide with the nodata byte —
                                    // step AWAY from it in whichever
                                    // direction exists (nodata = 0 sources
                                    // can't step down).
                                    r = if r == 0 { 1 } else { r - 1 };
                                }
                                *raw = r;
                            }
                        }
                    }
                    FrameData::U8 {
                        data: sampled,
                        nodata: *nodata,
                        gain: *gain,
                        offset: *offset,
                    }
                }
                FrameData::F32(_) => {
                    let mut sampled = trajectories.sample(analysis_f32).data;
                    if self.cfg.growth_decay {
                        let moved = trajectories.sample_u8(&label_map, 0);
                        let damp =
                            EFOLD_INTERVALS * (1.0 - (-lead_intervals / EFOLD_INTERVALS).exp());
                        for (v, k) in sampled.iter_mut().zip(&moved) {
                            if v.is_finite() && *k > 0 {
                                *v += cell_tendency[*k as usize] * damp;
                            }
                        }
                    }
                    FrameData::F32(sampled)
                }
            };
            times.push(lead_time);
            frames.push(frame);
        }
        drop(trajectories); // release the borrow of `field` before moving it

        Ok(Generation {
            reference_time: anchor,
            times,
            frames,
            geom,
            field,
            cells,
        })
    }

    /// Fetch one source frame on the working WGS84 grid.
    fn fetch_frame(
        &self,
        geom: &GridGeom,
        time: DateTime<Utc>,
        source_run: Option<DateTime<Utc>>,
    ) -> Result<FrameData, DataServerError> {
        let tile = self.source.get_raster_tile(
            [geom.west, geom.south, geom.east, geom.north],
            geom.width,
            geom.height,
            Some(time),
            &OutputCrs::Wgs84,
            None,
            None,
            // Both frames of a generation pin the SAME source run: if the
            // source is itself a forecast engine, a run rollover landing
            // between the two fetches must fail the generation (retried next
            // poll) rather than silently mix runs into the motion estimate.
            // Non-forecast sources resolve this to `None` — no change.
            source_run,
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
                    // A syntactically valid run the engine no longer retains
                    // maps to HTTP 404 — the GRIB/QueryData convention.
                    Some(rt) => DataServerError::ReferenceTimeNotFound(format!(
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

/// Build one cell feature: lon/lat from the grid geometry plus the served
/// property set. Shared by `get_features` and `get_feature` so the two
/// paths cannot drift (and the by-id path needn't materialize every cell).
fn cell_feature(
    t: &CellTrack,
    g: GridGeom,
    kx: f64,
    ky: f64,
    anchor: DateTime<Utc>,
) -> (f64, f64, Feature) {
    let lon = g.west + (f64::from(t.blob.centroid.0) / f64::from(g.width)) * (g.east - g.west);
    let lat = g.north - (f64::from(t.blob.centroid.1) / f64::from(g.height)) * (g.north - g.south);
    let mut props = std::collections::HashMap::new();
    props.insert(
        "severity".into(),
        PropertyValue::String(t.severity.as_str().into()),
    );
    props.insert(
        "max_dbz".into(),
        PropertyValue::Float(f64::from(t.blob.max_value)),
    );
    props.insert(
        "area_km2".into(),
        PropertyValue::Float(t.blob.area as f64 * kx * ky),
    );
    props.insert("track_age".into(), PropertyValue::Integer(t.age as i64));
    props.insert("deviant_mover".into(), PropertyValue::Bool(t.deviant()));
    props.insert(
        "speed_ms".into(),
        t.speed_ms()
            .map(|v| PropertyValue::Float(f64::from(v)))
            .unwrap_or(PropertyValue::Null),
    );
    props.insert(
        "bearing_deg".into(),
        t.bearing_deg()
            .map(PropertyValue::Float)
            .unwrap_or(PropertyValue::Null),
    );
    props.insert(
        "observed".into(),
        PropertyValue::String(anchor.to_rfc3339()),
    );
    // Lifecycle as DATA, not field modification: three gate runs showed
    // tendency extrapolation loses to pure advection (#546), but the
    // measured trend is still valuable client-side ("intensifying" /
    // "weakening" badges).
    props.insert(
        "intensity_trend_dbz_min".into(),
        if t.age >= 2 {
            PropertyValue::Float(f64::from(t.intensity_tendency) * 60.0)
        } else {
            PropertyValue::Null
        },
    );
    (
        lon,
        lat,
        Feature {
            id: t.id.to_string(),
            geometry: Arc::new(Geometry::Point { x: lon, y: lat }),
            properties: Arc::new(props),
        },
    )
}

impl FeatureEngine for NowcastEngine {
    /// Tracked cells of the latest analysis frame as Point features (#544).
    fn get_features(&self, query: &FeatureQuery) -> Result<FeaturePage, DataServerError> {
        let state = self.state.load();
        let Some((&anchor, latest)) = state.generations.iter().next_back() else {
            return Ok(FeaturePage {
                features: Vec::new(),
                number_matched: 0,
                number_returned: 0,
                next_offset: None,
            });
        };
        // Cells exist only at the latest analysis instant: a datetime
        // filter that excludes the anchor matches nothing (engine-cap
        // precedent for honoring `?datetime=`).
        if let Some(dt) = &query.datetime {
            let after_start = dt.start.is_none_or(|s| anchor >= s);
            let before_end = dt.end.is_none_or(|e| anchor <= e);
            if !(after_start && before_end) {
                return Ok(FeaturePage {
                    features: Vec::new(),
                    number_matched: 0,
                    number_returned: 0,
                    next_offset: None,
                });
            }
        }
        let g = latest.geom;
        let (kx, ky) =
            crate::lonlat_grid_km_per_px([g.west, g.south, g.east, g.north], g.width, g.height);
        let matched: Vec<Feature> = state
            .cells
            .iter()
            .filter_map(|t| {
                let (lon, lat, feature) = cell_feature(t, g, kx, ky, anchor);
                if let Some(b) = &query.bbox {
                    if !b.contains(lon, lat) {
                        return None;
                    }
                }
                Some(feature)
            })
            .collect();
        let number_matched = matched.len();
        let page: Vec<Feature> = matched
            .into_iter()
            .skip(query.offset)
            .take(query.limit)
            .collect();
        let number_returned = page.len();
        let next_offset = (query.offset + number_returned < number_matched)
            .then(|| query.offset + number_returned);
        Ok(FeaturePage {
            features: page,
            number_matched,
            number_returned,
            next_offset,
        })
    }

    /// O(1) from the snapshot — the default would build every feature just
    /// to count them, on every collection-metadata request.
    fn feature_count(&self) -> usize {
        self.state.load().cells.len()
    }

    fn get_feature(&self, feature_id: &str) -> Result<Feature, DataServerError> {
        let state = self.state.load();
        let not_found = || DataServerError::FeatureNotFound(feature_id.to_string());
        let (&anchor, latest) = state.generations.iter().next_back().ok_or_else(not_found)?;
        let id: u64 = feature_id.parse().map_err(|_| not_found())?;
        let track = state
            .cells
            .iter()
            .find(|t| t.id == id)
            .ok_or_else(not_found)?;
        let g = latest.geom;
        let (kx, ky) =
            crate::lonlat_grid_km_per_px([g.west, g.south, g.east, g.north], g.width, g.height);
        Ok(cell_feature(track, g, kx, ky, anchor).2)
    }

    /// Bumps every generation, so any future consumer keying caches/ETags on
    /// the feature snapshot (e.g. MVT serving of tracked cells) invalidates
    /// correctly — cells are rebuilt per generation. Currently only the
    /// plain Features path is wired, which doesn't read this; overriding
    /// anyway removes the stale-tile trap before it can exist.
    fn data_version(&self) -> u64 {
        self.generations_total.load(Ordering::Relaxed)
    }

    fn spatial_extent(&self) -> Option<[f64; 4]> {
        self.state.load().info.spatial_extent
    }

    fn temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        let state = self.state.load();
        state.generations.iter().next_back().map(|(&a, _)| (a, a))
    }
}
