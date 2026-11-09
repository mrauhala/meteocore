//! The period collection: config validation, the poll loop that reads the
//! source's frames, the snapshot requests read, and the `MapEngine` and
//! `EdrEngine` over the windows' grids (#1379).
//!
//! # Time
//!
//! Each window ends at the source's latest frame and holds only that end:
//! one grid per window, no per-frame grids, so a window's earlier ends are
//! not kept. A window is served at its end while its coverage is at least
//! `min_coverage`, and not at all below it. Each window (its product and
//! its coverage parameter) has its own time axis, `[end]` or empty
//! (`MapEngine::parameter_times`); `resolve_time`, `resolve_parameter_time`
//! and `get_raster_tile` all select through [`select_time`] over it (#507).
//!
//! # Versions
//!
//! A window's grids change in place at a fixed end when a late frame
//! arrives or a rebuild replaces them, so every change moves the window's
//! version, which keys the no-TTL map caches through
//! `MapEngine::content_version_for` (`content_version` is the highest
//! handed out). A re-sum that only clears rounding keeps the version.
//! While a poll folds strips into a window, the window serves a fresh
//! in-flux version, and after the poll publishes another fresh one
//! (`rolling::Versions`): a render part way through is never keyed on the
//! window's last consistent version.
//!
//! # Runtime
//!
//! `poll_loop` runs on the background poll runtime (Critical Rule 6), each
//! cycle inside `block_in_place`: the source's `get_raster_tile` may block
//! on storage I/O there and only there (Critical Rule 7). Requests read the
//! grids under each window's read lock and never touch the source.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use arc_swap::ArcSwap;
use chrono::{DateTime, Duration, Utc};
use ds_core::config::PeriodConfig;
use ds_core::datetime::parse_iso8601_duration;
use ds_core::edr_engine::{select_parameters, EdrEngine};
use ds_core::error::DataServerError;
use ds_core::feature::{parse_area_coords, parse_point_coords};
use ds_core::map_engine::{
    select_common_time, MapEngine, OutputCrs, ParameterInfo, RasterInfo, RasterTile, RasterValues,
};
use ds_core::model::{
    CoverageResponse, DomainDescription, LocationSnapshot, NdArray, ParameterDescription,
    QueryResult, ValuePrecision,
};
use ds_core::native_grid::NativeGrid;
use ds_poll::{FirstTick, Shutdown};
use tracing::{info, warn};

use crate::accum::{Frame, Grids, Statistic};
use crate::geometry::{self, GridGeom, MAX_CELLS};
use crate::rolling::{human, Built, Roller, RollerCfg, Strips, Versions, WindowStatus};
use crate::zr::ZR;

/// Windows one collection may have.
pub const MAX_WINDOWS: usize = 16;
/// The longest window.
pub const MAX_WINDOW: Duration = Duration::days(366);
/// The fewest cells a working grid is configured with.
pub const MIN_MAX_PIXELS: usize = 65_536;

/// The time a window serves for a requested `time`, among its served
/// `times` (`[end]` or none): latest-not-after, the earliest when `time`
/// precedes them, the latest for `None`. `get_raster_tile`,
/// `resolve_time` and `resolve_parameter_time` all use it (#507).
pub fn select_time(times: &[DateTime<Utc>], time: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    select_common_time(&[times], time)
}

/// One configured window.
#[derive(Debug, Clone)]
struct WindowCfg {
    /// As configured, upper case: `PT1H`. Part of the parameter names.
    label: String,
    length: Duration,
}

/// One served parameter: a window's statistic or its coverage.
#[derive(Debug, Clone)]
struct Param {
    name: String,
    title: String,
    unit: String,
    window: usize,
    coverage: bool,
}

/// What a window serves now.
#[derive(Debug, Clone)]
struct Served {
    built: Option<Built>,
    /// The served times, `[end]` when the coverage reaches `min_coverage`.
    times: Arc<[DateTime<Utc>]>,
}

/// What requests read: published after every poll.
struct Snapshot {
    geom: Option<GridGeom>,
    info: Arc<RasterInfo>,
    windows: Vec<Served>,
    /// The union of the windows' served times.
    times: Arc<[DateTime<Utc>]>,
    /// Why some window is not served, for `/health`.
    waiting: Option<String>,
}

/// The poll's own state, behind one lock the poll alone takes.
struct PollState {
    geom: Option<GridGeom>,
    roller: Option<Roller>,
    source_version: Option<u64>,
}

/// A derived collection of rolling-window statistics over a source
/// collection's frames.
pub struct PeriodEngine {
    collection_id: String,
    source_id: String,
    source: Arc<dyn MapEngine>,
    parameter: String,
    statistic: Statistic,
    zr: Option<ZR>,
    /// The statistic's inputs are never negative (rain rates).
    non_negative: bool,
    windows: Vec<WindowCfg>,
    min_coverage: f64,
    cadence: Option<Duration>,
    poll_interval: std::time::Duration,
    max_pixels: usize,
    max_reads: usize,
    read_strip_cells: usize,
    params: Vec<Param>,
    descriptions: HashMap<String, ParameterDescription>,
    /// Each window's live grids, read by requests under the read lock.
    slots: Vec<RwLock<Option<Grids>>>,
    /// Each window's served version, which keys the map caches: moved to a
    /// fresh in-flux value before the poll changes the window's grids, and
    /// to the next value after it publishes the poll's snapshot.
    versions: Arc<Versions>,
    snapshot: ArcSwap<Snapshot>,
    poll: Mutex<PollState>,
    shutdown: Shutdown,
    polls_total: AtomicU64,
    reads_total: AtomicU64,
    last_poll_ms: AtomicU64,
    grid_bytes: AtomicU64,
    /// The most bytes of source frames one poll has held at once.
    peak_frame_bytes: AtomicU64,
}

impl PeriodEngine {
    /// Validate `config` against the source as it is now and build the
    /// engine, nothing read yet. An unknown parameter, a bad window, a
    /// statistic the source's unit cannot carry or a forecast source is an
    /// error, which fails the collection at load.
    pub fn new(
        collection_id: &str,
        source_id: &str,
        source: Arc<dyn MapEngine>,
        config: &PeriodConfig,
    ) -> Result<Self, DataServerError> {
        let err = |msg: String| DataServerError::Config(format!("[period] {msg}"));
        let statistic = match (config.statistic.as_str(), config.threshold) {
            ("sum", None) => Statistic::Sum,
            ("mean", None) => Statistic::Mean,
            ("count_above", Some(t)) if t.is_finite() => Statistic::CountAbove(t as f32),
            ("count_above", _) => {
                return Err(err(
                    "statistic 'count_above' needs a finite threshold".into()
                ))
            }
            ("sum" | "mean", Some(_)) => {
                return Err(err(format!(
                    "threshold is for statistic 'count_above', not '{}'",
                    config.statistic
                )))
            }
            ("max" | "min", _) => {
                return Err(err(format!(
                    "statistic '{}' is not supported yet: a rolling maximum or minimum cannot \
                     take a leaving frame out of one running grid; use sum, mean or count_above",
                    config.statistic
                )))
            }
            (other, _) => {
                return Err(err(format!(
                    "unknown statistic '{other}'; expected sum, mean or count_above"
                )))
            }
        };

        if config.windows.is_empty() {
            return Err(err(
                "windows must list at least one ISO 8601 duration".into()
            ));
        }
        if config.windows.len() > MAX_WINDOWS {
            return Err(err(format!("at most {MAX_WINDOWS} windows")));
        }
        let mut windows: Vec<WindowCfg> = Vec::new();
        for raw in &config.windows {
            let label = raw.trim().to_ascii_uppercase();
            let length = parse_iso8601_duration(&label)
                .map_err(|e| err(format!("window '{raw}' is not an ISO 8601 duration: {e}")))?;
            if length <= Duration::zero() || length > MAX_WINDOW {
                return Err(err(format!(
                    "window '{raw}' must be positive and at most {}",
                    human(MAX_WINDOW)
                )));
            }
            if length.num_seconds() < 1 || length.subsec_nanos() != 0 {
                return Err(err(format!("window '{raw}' must be whole seconds")));
            }
            if let Some(same) = windows.iter().find(|w| w.length == length) {
                return Err(err(format!(
                    "windows '{}' and '{raw}' are the same length",
                    same.label
                )));
            }
            windows.push(WindowCfg { label, length });
        }

        if !(config.min_coverage.is_finite()
            && config.min_coverage > 0.0
            && config.min_coverage <= 1.0)
        {
            return Err(err(format!(
                "min_coverage must be above 0 and at most 1, got {}",
                config.min_coverage
            )));
        }
        let cadence = match &config.cadence {
            None => None,
            Some(raw) => {
                let d = parse_iso8601_duration(raw)
                    .map_err(|e| err(format!("cadence '{raw}': {e}")))?;
                if d < Duration::seconds(1) {
                    return Err(err(format!("cadence '{raw}' must be at least one second")));
                }
                Some(d)
            }
        };
        if !(MIN_MAX_PIXELS..=MAX_CELLS).contains(&config.max_pixels) {
            return Err(err(format!(
                "max_pixels must be within {MIN_MAX_PIXELS}..={MAX_CELLS}, got {}",
                config.max_pixels
            )));
        }
        if !(1..=MAX_CELLS).contains(&config.read_strip_cells) {
            return Err(err(format!(
                "read_strip_cells must be within 1..={MAX_CELLS}, got {}",
                config.read_strip_cells
            )));
        }
        // A window's steady poll reads its leaving frame, and every window
        // shares the frame that just got its successor.
        let steady = windows.len() + 1;
        if config.max_reads_per_cycle < steady || config.max_reads_per_cycle > 1000 {
            return Err(err(format!(
                "max_reads_per_cycle must be within {steady}..=1000 for {} windows (one leaving \
                 frame each and the new one), got {}",
                windows.len(),
                config.max_reads_per_cycle
            )));
        }

        // The source as it is at load.
        let info = source.raster_info();
        if !info.reference_times.is_empty() {
            return Err(err(format!(
                "source '{source_id}' has model runs; windows over a forecast run's valid times \
                 are not supported yet"
            )));
        }
        let parameter = match &config.parameter {
            Some(p) => p.trim().to_string(),
            None => info.parameter.clone(),
        };
        let known: Vec<&str> = std::iter::once(info.parameter.as_str())
            .chain(info.parameters.iter().map(|p| p.name.as_str()))
            .filter(|p| !p.is_empty())
            .collect();
        if parameter.is_empty() || !known.contains(&parameter.as_str()) {
            return Err(err(format!(
                "source '{source_id}' has no parameter '{parameter}'; it has {}",
                if known.is_empty() {
                    "none yet".to_string()
                } else {
                    known.join(", ")
                }
            )));
        }
        let source_unit = info
            .parameter_unit(Some(&parameter))
            .unwrap_or("")
            .to_string();
        let source_title = info
            .parameters
            .iter()
            .find(|p| p.name == parameter)
            .map(|p| p.title.clone())
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| parameter.clone());
        let zr = match &config.rain_rate {
            None => None,
            Some(rr) => {
                if !source_unit.trim().eq_ignore_ascii_case("dbz") {
                    return Err(err(format!(
                        "rain_rate converts reflectivity in dBZ, but '{parameter}' of \
                         '{source_id}' is in '{source_unit}'"
                    )));
                }
                Some(ZR::new(rr.a, rr.b, rr.min_dbz).map_err(err)?)
            }
        };
        let input_unit = if zr.is_some() {
            "mm/h".to_string()
        } else {
            source_unit.clone()
        };
        let summed_unit = match (statistic, &zr) {
            (Statistic::Sum, Some(_)) => Some("mm".to_string()),
            (Statistic::Sum, None) => match per_hour(&source_unit) {
                Some(unit) => Some(unit),
                None => {
                    return Err(err(format!(
                        "statistic 'sum' adds a rate times the hours each frame counts for, but \
                         '{parameter}' is in '{source_unit}', not a rate per hour; set rain_rate \
                         for reflectivity"
                    )))
                }
            },
            _ => None,
        };

        let params = parameters(
            &windows,
            statistic,
            zr.as_ref(),
            &parameter,
            &source_title,
            &input_unit,
            summed_unit.as_deref(),
            config.threshold,
        );
        let descriptions = params
            .iter()
            .map(|p| {
                (
                    p.name.clone(),
                    ParameterDescription {
                        label: p.title.clone(),
                        unit: p.unit.clone(),
                        observed_property: p.name.clone(),
                        standard_name: None,
                    },
                )
            })
            .collect();
        let geom = info
            .spatial_extent
            .zip(info.grid_size)
            .and_then(|(extent, size)| GridGeom::over(extent, size, config.max_pixels));
        let engine = Self {
            collection_id: collection_id.to_string(),
            source_id: source_id.to_string(),
            source,
            parameter,
            statistic,
            non_negative: zr.is_some(),
            zr,
            min_coverage: config.min_coverage,
            cadence,
            poll_interval: std::time::Duration::from_secs(config.poll_interval_secs.max(5)),
            max_pixels: config.max_pixels,
            max_reads: config.max_reads_per_cycle,
            read_strip_cells: config.read_strip_cells,
            slots: windows.iter().map(|_| RwLock::new(None)).collect(),
            versions: Arc::new(Versions::new(windows.len())),
            windows,
            params,
            descriptions,
            snapshot: ArcSwap::from_pointee(Snapshot {
                geom,
                info: Arc::new(RasterInfo {
                    native_crs: "CRS:84".into(),
                    spatial_extent: info.spatial_extent,
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
                windows: Vec::new(),
                times: Arc::from([]),
                waiting: Some("waiting for the first poll".into()),
            }),
            poll: Mutex::new(PollState {
                geom: None,
                roller: None,
                source_version: None,
            }),
            shutdown: Shutdown::new(),
            polls_total: AtomicU64::new(0),
            reads_total: AtomicU64::new(0),
            last_poll_ms: AtomicU64::new(0),
            grid_bytes: AtomicU64::new(0),
            peak_frame_bytes: AtomicU64::new(0),
        };
        let statuses: Vec<WindowStatus> = engine
            .windows
            .iter()
            .map(|_| WindowStatus {
                built: None,
                version: 1,
                waiting: Some("waiting for the first poll".into()),
            })
            .collect();
        engine.publish(geom, &statuses);
        match geom {
            Some(geom) => engine.log_grid(&geom, info.grid_size),
            None => info!(
                "[{collection_id}] period grid chosen at the first poll: source '{source_id}' \
                 reports no grid yet"
            ),
        }
        Ok(engine)
    }

    /// Log the working grid and what its windows hold: at load, and when
    /// the source's grid changes.
    fn log_grid(&self, geom: &GridGeom, source: Option<[u32; 2]>) {
        let per_window = 6 * geom.cells() as u64;
        let mb = |bytes: u64| bytes as f64 / 1e6;
        let how = match source {
            Some([sx, sy]) if [sx, sy] == [geom.nx, geom.ny] => {
                "the source's native grid".to_string()
            }
            Some([sx, sy]) => format!(
                "the source's {sx}×{sy} halved to fit max_pixels {}; frames are read as the \
                 source renders that size (a GeoTIFF's overview)",
                self.max_pixels
            ),
            None => "the source's grid".to_string(),
        };
        let strips = Strips::new(geom.nx, geom.ny, self.read_strip_cells);
        info!(
            "[{}] period grid {}×{} ({} cells, {how}): {:.1} MB per window, {:.1} MB for {} \
             windows, {:.1} MB while a re-sum runs; frames read in strips of {} rows ({} \
             cells)",
            self.collection_id,
            geom.nx,
            geom.ny,
            geom.cells(),
            mb(per_window),
            mb(per_window * self.windows.len() as u64),
            self.windows.len(),
            mb(per_window * (self.windows.len() as u64 + 1)),
            strips.rows,
            strips.rows as usize * geom.nx as usize,
        );
    }

    pub fn collection_id(&self) -> &str {
        &self.collection_id
    }

    pub fn source_id(&self) -> &str {
        &self.source_id
    }

    /// Whether any window is served.
    pub fn has_data(&self) -> bool {
        !self.snapshot.load().times.is_empty()
    }

    /// Why a window is not served, for `/health`; `None` when all are.
    pub fn waiting_for(&self) -> Option<String> {
        self.snapshot.load().waiting.clone()
    }

    /// `(polls, frame reads, last poll ms)` since this engine was built.
    pub fn metrics(&self) -> (u64, u64, u64) {
        (
            self.polls_total.load(Ordering::Relaxed),
            self.reads_total.load(Ordering::Relaxed),
            self.last_poll_ms.load(Ordering::Relaxed),
        )
    }

    /// Bytes of the windows' grids, a rebuild's included (6 per cell
    /// each), set after every poll.
    pub fn grid_bytes(&self) -> u64 {
        self.grid_bytes.load(Ordering::Relaxed)
    }

    /// The most bytes of frame values one poll has held at once: one strip
    /// of one frame, as the statistic's input (the source's own tile for the
    /// strip, which it may box at 16 bytes a cell, comes and goes with it).
    pub fn peak_frame_bytes(&self) -> u64 {
        self.peak_frame_bytes.load(Ordering::Relaxed)
    }

    /// Stop the poll loop.
    pub fn shutdown(&self) {
        self.shutdown.shutdown();
    }

    /// The background loop. Spawn it on the background poll runtime (a
    /// multi-thread runtime: each cycle runs in `block_in_place`).
    pub async fn poll_loop(&self) {
        let mut ticker = self
            .shutdown
            .ticker(self.poll_interval, FirstTick::Immediate);
        while ticker.tick().await {
            tokio::task::block_in_place(|| self.poll_once());
        }
        info!("[{}] period poll loop shutting down", self.collection_id);
    }

    /// One poll, synchronously: the source's geometry and frame times, then
    /// every window brought to its latest end (`rolling`), then a new
    /// snapshot. Public so tests drive polls without the loop.
    pub fn poll_once(&self) {
        if self.shutdown.is_shutdown() {
            return;
        }
        let started = Instant::now();
        let info = self.source.raster_info_shared();
        let mut poll = self.poll.lock().unwrap_or_else(|e| e.into_inner());
        if !info.reference_times.is_empty() {
            self.publish_waiting(
                poll.geom,
                "the source has model runs; windows over a forecast run are not supported yet",
            );
            return;
        }
        let geom = info
            .spatial_extent
            .zip(info.grid_size)
            .and_then(|(extent, size)| GridGeom::over(extent, size, self.max_pixels));
        if geom != poll.geom {
            if poll.geom.is_some() {
                warn!(
                    "[{}] source '{}' changed its grid; rebuilding every window",
                    self.collection_id, self.source_id
                );
            }
            for (w, slot) in self.slots.iter().enumerate() {
                self.versions.flux(w);
                *slot.write().unwrap_or_else(|e| e.into_inner()) = None;
            }
            // Logged at load already when the source had its grid then.
            if let Some(g) = geom
                .as_ref()
                .filter(|g| self.snapshot.load().geom != Some(**g))
            {
                self.log_grid(g, info.grid_size);
            }
            poll.geom = geom;
            poll.roller = geom.map(|g| {
                Roller::new(
                    RollerCfg {
                        statistic: self.statistic,
                        lengths: self.windows.iter().map(|w| w.length).collect(),
                        strips: Strips::new(g.nx, g.ny, self.read_strip_cells),
                        max_reads: self.max_reads,
                        cadence: self.cadence,
                    },
                    self.versions.clone(),
                )
            });
        }
        let Some(geom) = poll.geom else {
            self.publish_waiting(None, "the source reports no grid yet");
            return;
        };
        let version = self.source.content_version();
        let revised = poll.source_version.is_some_and(|v| v != version);
        poll.source_version = Some(version);
        let times: Vec<DateTime<Utc>> = match self.source.parameter_times(&self.parameter) {
            Some(times) => times.to_vec(),
            None => info.times.clone(),
        };
        let roller = poll.roller.as_mut().expect("a grid has a roller");
        if revised {
            info!(
                "[{}] source '{}' revised its frames; re-summing every window",
                self.collection_id, self.source_id
            );
            roller.mark_revised();
        }
        let (statuses, stats) = roller.poll(&times, &self.slots, &mut |t, rows| {
            self.read(&geom, t, rows)
        });
        let rebuild_bytes = roller.rebuild_bytes();
        drop(poll);
        self.publish(Some(geom), &statuses);

        let live_bytes: u64 = self
            .slots
            .iter()
            .filter_map(|s| {
                s.read()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .map(Grids::bytes)
            })
            .sum();
        self.grid_bytes
            .store(live_bytes + rebuild_bytes, Ordering::Relaxed);
        self.polls_total.fetch_add(1, Ordering::Relaxed);
        self.reads_total
            .fetch_add(stats.reads as u64, Ordering::Relaxed);
        self.peak_frame_bytes
            .fetch_max(stats.peak_frame_bytes, Ordering::Relaxed);
        self.last_poll_ms
            .store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    /// Read one strip of one source frame, `rows` of the working grid, as
    /// the statistic's input. Fails unless the source still selects exactly
    /// `t`, before and after the read: a source snaps a time it no longer
    /// has to another frame, whose values must never be added or taken out
    /// as `t`'s.
    fn read(
        &self,
        geom: &GridGeom,
        t: DateTime<Utc>,
        rows: std::ops::Range<u32>,
    ) -> Result<Frame, DataServerError> {
        let parameter = Some(self.parameter.as_str());
        let exact = || self.source.resolve_parameter_time(parameter, Some(t), None) == Some(t);
        if !exact() {
            return Err(DataServerError::Engine(format!(
                "frame {t} is no longer in the source"
            )));
        }
        let height = rows.end - rows.start;
        let tile = self.source.get_raster_tile(
            geom.strip_bbox(rows),
            geom.nx,
            height,
            Some(t),
            &OutputCrs::Wgs84,
            parameter,
            None,
            None,
        )?;
        if (tile.width, tile.height) != (geom.nx, height) {
            return Err(DataServerError::Engine(format!(
                "source returned {}x{} for a strip of frame {t}, asked for {}x{height}",
                tile.width, tile.height, geom.nx
            )));
        }
        if !exact() {
            return Err(DataServerError::Engine(format!(
                "frame {t} left the source while it was read"
            )));
        }
        Ok(to_frame(tile.values, self.zr.as_ref()))
    }

    fn publish_waiting(&self, geom: Option<GridGeom>, reason: &str) {
        let old = self.snapshot.load();
        let statuses: Vec<WindowStatus> = old
            .windows
            .iter()
            .enumerate()
            .map(|(w, served)| WindowStatus {
                built: served.built,
                version: self.versions.get(w),
                waiting: Some(reason.to_string()),
            })
            .collect();
        self.publish(geom.or(old.geom), &statuses);
    }

    /// Publish a poll's snapshot, THEN serve each window's new version: a
    /// request that reads the new version reads the new snapshot too, and
    /// one rendering between the two is keyed on the in-flux version the
    /// poll set, which nothing asks for afterwards.
    fn publish(&self, geom: Option<GridGeom>, statuses: &[WindowStatus]) {
        let windows: Vec<Served> = statuses
            .iter()
            .map(|s| {
                let served = s
                    .built
                    .filter(|b| b.coverage + 1e-9 >= self.min_coverage)
                    .map(|b| b.end);
                Served {
                    built: s.built,
                    times: served.into_iter().collect::<Vec<_>>().into(),
                }
            })
            .collect();
        let times: BTreeSet<DateTime<Utc>> = windows
            .iter()
            .flat_map(|w| w.times.iter().copied())
            .collect();
        let mut waiting: Vec<String> = Vec::new();
        for (w, (status, served)) in statuses.iter().zip(&windows).enumerate() {
            let label = &self.windows[w].label;
            if let Some(reason) = &status.waiting {
                waiting.push(format!("{label}: {reason}"));
            } else if let (Some(built), true) = (status.built, served.times.is_empty()) {
                waiting.push(format!(
                    "{label}: coverage {:.3} is below min_coverage {}",
                    built.coverage, self.min_coverage
                ));
            }
        }
        let first = &self.params[0];
        let info = RasterInfo {
            native_crs: "CRS:84".into(),
            spatial_extent: geom
                .map(|g| g.extent())
                .or(self.snapshot.load().info.spatial_extent),
            times: times.iter().copied().collect(),
            parameter: first.name.clone(),
            unit: first.unit.clone(),
            parameters: self
                .params
                .iter()
                .map(|p| ParameterInfo {
                    name: p.name.clone(),
                    title: p.title.clone(),
                    unit: p.unit.clone(),
                })
                .collect(),
            vertical: None,
            grid_size: geom.map(|g| [g.nx, g.ny]),
            layer_subtitle: None,
            reference_times: Vec::new(),
            native_grid: geom.and_then(|g| {
                NativeGrid::new(
                    ds_core::geo::Crs::Wgs84,
                    None,
                    [g.nx, g.ny],
                    [g.west, g.south],
                    [g.dx(), g.dy()],
                )
            }),
        };
        self.snapshot.store(Arc::new(Snapshot {
            geom,
            info: Arc::new(info),
            windows,
            times: times.into_iter().collect::<Vec<_>>().into(),
            waiting: (!waiting.is_empty()).then(|| waiting.join("; ")),
        }));
        for (w, status) in statuses.iter().enumerate() {
            self.versions.set(w, status.version.max(1));
        }
    }

    fn param_index(&self, parameter: Option<&str>) -> Result<usize, DataServerError> {
        let Some(name) = parameter else {
            return Ok(0);
        };
        self.params
            .iter()
            .position(|p| p.name == name)
            .ok_or_else(|| {
                DataServerError::InvalidParameter(format!(
                    "unknown parameter '{name}'; expected one of {}",
                    self.params
                        .iter()
                        .map(|p| p.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })
    }

    /// The value parameter `p` serves at `cell` of window grids `grids`.
    #[inline]
    fn value(&self, p: &Param, grids: &Grids, built: &Built, cell: usize) -> f32 {
        if p.coverage {
            grids.coverage(cell, built.coverage as f32, built.frames)
        } else {
            grids.value(cell, self.statistic, self.non_negative)
        }
    }

    /// Render parameter `p` at exactly `time`: an empty tile unless its
    /// window is served then.
    fn render(
        &self,
        p: usize,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        output_crs: &OutputCrs,
    ) -> RasterTile {
        let param = &self.params[p];
        let slot = self.slots[param.window]
            .read()
            .unwrap_or_else(|e| e.into_inner());
        let snapshot = self.snapshot.load();
        let window = snapshot.windows.get(param.window);
        let data = match (snapshot.geom, slot.as_ref(), window) {
            (Some(geom), Some(grids), Some(window))
                if time.is_some() && window.times.first() == time.as_ref() =>
            {
                let built = window.built.expect("a served window is built");
                geometry::sample(&geom, bbox, width, height, output_crs, |cell| {
                    self.value(param, grids, &built, cell)
                })
            }
            _ => vec![f32::NAN; width as usize * height as usize],
        };
        RasterTile {
            width,
            height,
            values: RasterValues::F32 { data, nodata: None },
        }
    }

    fn param_times(&self, p: usize) -> Arc<[DateTime<Utc>]> {
        self.snapshot
            .load()
            .windows
            .get(self.params[p].window)
            .map(|w| w.times.clone())
            .unwrap_or_else(|| Arc::from([]))
    }

    /// The parameters an EDR `parameter-name` selects.
    fn selected(&self, parameters: Option<&[String]>) -> Result<Vec<usize>, DataServerError> {
        let names: Vec<&str> = self.params.iter().map(|p| p.name.as_str()).collect();
        Ok(select_parameters(parameters, &names)?
            .into_iter()
            .filter_map(|name| self.params.iter().position(|p| p.name == name))
            .collect())
    }

    /// The times an EDR `datetime` selects among the selected parameters'
    /// served times: the latest without one, every time inside an interval
    /// (an instant is one).
    fn query_times(
        &self,
        params: &[usize],
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
    ) -> Result<Vec<DateTime<Utc>>, DataServerError> {
        let all: BTreeSet<DateTime<Utc>> = params
            .iter()
            .flat_map(|&p| self.param_times(p).to_vec())
            .collect();
        let selected: Vec<DateTime<Utc>> = match datetime {
            None => all.last().copied().into_iter().collect(),
            Some((start, end)) => all.range(start..=end).copied().collect(),
        };
        if selected.is_empty() {
            return Err(DataServerError::LocationNotFound(match datetime {
                None => format!(
                    "'{}' serves no window yet{}",
                    self.collection_id,
                    self.waiting_for()
                        .map(|w| format!(" ({w})"))
                        .unwrap_or_default()
                ),
                Some(_) => format!(
                    "'{}' serves no window ending in the requested datetime; its windows end \
                     at the source's latest frame",
                    self.collection_id
                ),
            }));
        }
        Ok(selected)
    }

    /// Every selected parameter at every cell of `cells` and every time.
    fn ranges(
        &self,
        params: &[usize],
        times: &[DateTime<Utc>],
        cells: &[Option<usize>],
        shape: Vec<usize>,
        axis_names: &[&str],
    ) -> HashMap<String, NdArray> {
        let snapshot = self.snapshot.load();
        let mut ranges = HashMap::new();
        for &p in params {
            let param = &self.params[p];
            let slot = self.slots[param.window]
                .read()
                .unwrap_or_else(|e| e.into_inner());
            let window = &snapshot.windows[param.window];
            let mut values = Vec::with_capacity(times.len() * cells.len());
            for t in times {
                let grids = slot
                    .as_ref()
                    .filter(|_| window.times.first() == Some(t))
                    .zip(window.built);
                for cell in cells {
                    values.push(match (grids, cell) {
                        (Some((grids, built)), Some(cell)) => {
                            shortest(self.value(param, grids, &built, *cell))
                        }
                        _ => None,
                    });
                }
            }
            ranges.insert(
                param.name.clone(),
                NdArray {
                    shape: shape.clone(),
                    axis_names: axis_names.iter().map(|s| s.to_string()).collect(),
                    values,
                    precision: ValuePrecision::F32,
                },
            );
        }
        ranges
    }

    fn descriptions_of(&self, params: &[usize]) -> HashMap<String, ParameterDescription> {
        params
            .iter()
            .filter_map(|&p| {
                let name = &self.params[p].name;
                Some((name.clone(), self.descriptions.get(name)?.clone()))
            })
            .collect()
    }

    fn geom(&self) -> Result<GridGeom, DataServerError> {
        self.snapshot.load().geom.ok_or_else(|| {
            DataServerError::LocationNotFound(format!(
                "'{}' has no grid yet: its source reports none",
                self.collection_id
            ))
        })
    }
}

/// A value's `f32` as the `f64` with its shortest decimal spelling, the
/// number the map draws without digits `f32` never had; `None` for nodata.
fn shortest(v: f32) -> Option<f64> {
    v.is_finite()
        .then(|| v.to_string().parse().unwrap_or(f64::from(v)))
}

/// A tile's values as the statistic's input: the rain rate with a Z–R
/// relation, else the value; NaN for nodata.
fn to_frame(values: RasterValues, zr: Option<&ZR>) -> Frame {
    let input = |v: f64| -> f32 {
        match zr {
            Some(zr) => zr.rate(v) as f32,
            None => v as f32,
        }
    };
    match values {
        RasterValues::U8 {
            data,
            nodata,
            gain,
            offset,
        } => {
            let mut table = Box::new([f32::NAN; 256]);
            for (raw, entry) in table.iter_mut().enumerate() {
                if nodata != Some(raw as u8) {
                    *entry = input(raw as f64 * gain + offset);
                }
            }
            Frame::U8 { data, table }
        }
        RasterValues::F32 { mut data, nodata } => {
            for v in &mut data {
                *v = RasterValues::decode_f32(*v, nodata).map_or(f32::NAN, input);
            }
            Frame::F32(data)
        }
        RasterValues::F64(values) => Frame::F32(
            values
                .into_iter()
                .map(|v| v.filter(|v| v.is_finite()).map_or(f32::NAN, input))
                .collect(),
        ),
    }
}

/// The amount unit of a rate per hour (`mm/h`, `mm h-1` → `mm`), from the
/// unit's canonical spelling; `None` for any other unit.
fn per_hour(unit: &str) -> Option<String> {
    let canonical = ds_core::units::canonical_unit(unit);
    [" h-1", " hr-1"]
        .iter()
        .find_map(|suffix| canonical.strip_suffix(suffix))
        .filter(|amount| !amount.trim().is_empty())
        .map(str::to_string)
}

/// The served parameters: each window's statistic, then each window's
/// coverage.
#[allow(clippy::too_many_arguments)] // each is one part of a name, title or unit
fn parameters(
    windows: &[WindowCfg],
    statistic: Statistic,
    zr: Option<&ZR>,
    parameter: &str,
    source_title: &str,
    input_unit: &str,
    summed_unit: Option<&str>,
    threshold: Option<f64>,
) -> Vec<Param> {
    let relation = zr.map(|zr| format!("Z = {}·R^{}", zr.a(), zr.b()));
    let (base, unit) = match (statistic, &relation) {
        (Statistic::Sum, Some(_)) => ("precipitation_amount".to_string(), "mm".to_string()),
        (Statistic::Sum, None) => (
            format!("{parameter}_sum"),
            summed_unit.unwrap_or_default().to_string(),
        ),
        (Statistic::Mean, Some(_)) => ("rain_rate_mean".to_string(), "mm/h".to_string()),
        (Statistic::Mean, None) => (format!("{parameter}_mean"), input_unit.to_string()),
        (Statistic::CountAbove(_), Some(_)) => {
            ("rain_rate_count_above".to_string(), "1".to_string())
        }
        (Statistic::CountAbove(_), None) => (format!("{parameter}_count_above"), "1".to_string()),
    };
    let mut params: Vec<Param> = windows
        .iter()
        .enumerate()
        .map(|(w, window)| {
            let over = human(window.length);
            let title = match (statistic, &relation) {
                (Statistic::Sum, Some(rel)) => {
                    format!("Radar precipitation accumulation over {over} ({rel})")
                }
                (Statistic::Sum, None) => format!("{source_title} summed over {over}"),
                (Statistic::Mean, Some(rel)) => format!("Mean radar rain rate over {over} ({rel})"),
                (Statistic::Mean, None) => format!("Mean {source_title} over {over}"),
                (Statistic::CountAbove(_), _) => format!(
                    "Frames at or above {} {input_unit} over {over}",
                    threshold.unwrap_or_default()
                ),
            };
            Param {
                name: format!("{base}_{}", window.label),
                title,
                unit: unit.clone(),
                window: w,
                coverage: false,
            }
        })
        .collect();
    params.extend(windows.iter().enumerate().map(|(w, window)| Param {
        name: format!("coverage_{}", window.label),
        title: format!(
            "Coverage of the {} window: the share of it with data",
            human(window.length)
        ),
        unit: "1".into(),
        window: w,
        coverage: true,
    }));
    params
}

impl MapEngine for PeriodEngine {
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
        let time = select_time(&self.param_times(p), time);
        Ok(self.render(p, bbox, width, height, time, output_crs))
    }

    /// Every band at exactly `time`: the windows have their own axes, so a
    /// band without a product then is an error, never another time.
    fn get_raster_tiles(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        output_crs: &OutputCrs,
        parameters: &[&str],
        _z: Option<f64>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<Vec<RasterTile>, DataServerError> {
        let time = match time {
            Some(t) => Some(t),
            None => self.resolve_parameters_time(parameters, None, None),
        };
        parameters
            .iter()
            .map(|parameter| {
                let p = self.param_index(Some(parameter))?;
                if time.is_none_or(|t| !self.param_times(p).contains(&t)) {
                    return Err(DataServerError::InvalidParameter(format!(
                        "'{parameter}' has no window ending at the requested time"
                    )));
                }
                Ok(self.render(p, bbox, width, height, time, output_crs))
            })
            .collect()
    }

    fn raster_info(&self) -> RasterInfo {
        (*self.snapshot.load().info).clone()
    }

    fn raster_info_shared(&self) -> Arc<RasterInfo> {
        self.snapshot.load().info.clone()
    }

    fn resolve_time(
        &self,
        time: Option<DateTime<Utc>>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        select_time(&self.param_times(0), time)
    }

    fn parameter_times(&self, parameter: &str) -> Option<Arc<[DateTime<Utc>]>> {
        let p = self.params.iter().position(|p| p.name == parameter)?;
        Some(self.param_times(p))
    }

    fn resolve_parameter_time(
        &self,
        parameter: Option<&str>,
        time: Option<DateTime<Utc>>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        let p = self.param_index(parameter).ok()?;
        select_time(&self.param_times(p), time)
    }

    /// The highest version handed out: moves whenever any window's does.
    fn content_version(&self) -> u64 {
        self.versions.top().max(1)
    }

    /// The served version of the parameter's window, one atomic load: it
    /// moves to an in-flux value before a poll changes the window's grids
    /// and to the next value after the poll publishes, so a render part
    /// way through never lands under the key of the window's last
    /// consistent state (late frames, rebuilds, breaks; `rolling`).
    fn content_version_for(
        &self,
        parameter: Option<&str>,
        _time: Option<DateTime<Utc>>,
        _reference_time: Option<DateTime<Utc>>,
        _z: Option<f64>,
    ) -> u64 {
        match self.param_index(parameter) {
            Ok(p) => self.versions.get(self.params[p].window).max(1),
            Err(_) => self.content_version(),
        }
    }
}

/// EDR over the windows' grids, at the cell under each point: a position
/// query and GetFeatureInfo answer the value the map shows. Without
/// `datetime` a query reads the windows' latest end; an instant or interval
/// must contain it.
impl EdrEngine for PeriodEngine {
    fn coarsens_areas(&self) -> bool {
        true
    }

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
            self.collection_id
        )))
    }

    fn get_parameters(&self) -> Vec<String> {
        self.params.iter().map(|p| p.name.clone()).collect()
    }

    fn get_parameter_descriptions(&self) -> HashMap<String, ParameterDescription> {
        self.descriptions.clone()
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        let times = self.snapshot.load().times.clone();
        Some((*times.first()?, *times.last()?))
    }

    fn get_available_times(&self) -> Option<Vec<DateTime<Utc>>> {
        Some(self.snapshot.load().times.to_vec())
    }

    fn get_parameter_available_times(&self, parameter: &str) -> Option<Vec<DateTime<Utc>>> {
        self.parameter_times(parameter).map(|t| t.to_vec())
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        let snapshot = self.snapshot.load();
        snapshot
            .geom
            .map(|g| g.extent())
            .or(snapshot.info.spatial_extent)
    }

    fn supported_query_types(&self) -> Vec<String> {
        ["position", "area", "radius"]
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
        let geom = self.geom()?;
        let cell = geom.cell(lon, lat).ok_or_else(|| {
            DataServerError::LocationNotFound(format!(
                "POINT({lon} {lat}) is outside the grid of '{}'",
                self.collection_id
            ))
        })?;
        let times = self.query_times(&params, datetime)?;
        let ranges = self.ranges(&params, &times, &[Some(cell)], vec![times.len()], &["t"]);
        Ok(CoverageResponse::Single(QueryResult {
            domain: DomainDescription::PointSeries {
                x: lon,
                y: lat,
                t: times,
                z: None,
            },
            parameters: self.descriptions_of(&params),
            ranges,
        }))
    }

    /// A CRS84 grid over the polygon's bbox at the working grid's
    /// resolution while it fits the value budget, evenly coarser past it
    /// (#1436), each cell the grid cell under its centre; cells outside
    /// the polygon are null.
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
        let geom = self.geom()?;
        if !polygon.bbox.intersects_bbox(&geom.extent()) {
            return Err(DataServerError::LocationNotFound(format!(
                "The polygon lies outside the grid of '{}'",
                self.collection_id
            )));
        }
        let times = self.query_times(&params, datetime)?;
        let axes = polygon.fit_grid(geom.dx(), geom.dy(), times.len(), params.len())?;
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
                "No cell of the area lies on the grid of '{}'",
                self.collection_id
            )));
        }
        let shape = vec![times.len(), axes.y.len(), axes.x.len()];
        let ranges = self.ranges(&params, &times, &cells, shape, &["t", "y", "x"]);
        Ok(CoverageResponse::Single(QueryResult {
            domain: DomainDescription::Grid {
                x: axes.x,
                y: axes.y,
                t: Some(times),
                z: None,
                coarsened: axes.coarsened,
            },
            parameters: self.descriptions_of(&params),
            ranges,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rate_per_hour_sums_to_its_amount() {
        assert_eq!(per_hour("mm/h").as_deref(), Some("mm"));
        assert_eq!(per_hour("mm h-1").as_deref(), Some("mm"));
        assert_eq!(per_hour("mm/hr").as_deref(), Some("mm"));
        assert_eq!(per_hour("kg m-2 h-1").as_deref(), Some("kg m-2"));
        assert_eq!(per_hour("dBZ"), None);
        assert_eq!(per_hour("mm"), None);
        assert_eq!(per_hour("h-1"), None);
    }
}
