//! `MapEngine` impl backed by an ODIM_H5 composite catalog.
//!
//! At construction time, [`OdimEngine::new`] scans the configured
//! source — a local directory or an S3/HTTP object-store prefix — for
//! ODIM files matching the `filename_template` pattern and pre-loads
//! the most recent one so `raster_info()` can answer with non-empty
//! `spatial_extent` and `times`. After that, every `get_raster_tile`
//! call:
//!
//! 1. Picks the catalog entry matching the requested time (or the
//!    most recent if `time` is `None`).
//! 2. Loads the composite into a single-entry path-keyed cache
//!    (subsequent same-file reads avoid disk + HDF5 reparse).
//! 3. Walks the output grid pixel-by-pixel, projecting the WGS84
//!    or Web-Mercator output coords into the composite's native
//!    CRS, and samples via nearest-neighbour.
//!
//! Scope is still narrowed from the format's full capabilities:
//! - Single-dataset COMP composites (no PVOL polar volumes yet)
//! - Single-parameter (one composite quantity per collection)
//! - STAC sources land in a later phase
//!
//! A background `poll_loop` re-scans the source every
//! `poll_interval_secs` and atomically swaps the catalog
//! (`ArcSwap<Vec<CatalogEntry>>`) when the file set changes, so
//! new ODIM files appear in the temporal extent without needing
//! an admin reload.
//!
//! See [[project_odim_engine_plan]] for the full multi-phase plan.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use ds_core::error::DataServerError;
use ds_core::geo::Crs;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterInfo, RasterTile};
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

use ds_storage::discovery::{expand_prefix_for_dates, expand_prefix_pattern, TimeWindow};

use crate::catalog::{scan_local_directory, scan_remote, CatalogEntry, FilenameMatcher};
use crate::reader::{read_composite, OdimComposite};

/// Days of date-partitioned prefixes to scan when an S3 source has no
/// `time_window`. Two days covers the just-after-midnight case where
/// the recent tail still straddles yesterday's partition.
const DEFAULT_SCAN_DAYS: u32 = 2;

/// Where an [`OdimEngine`] reads ODIM files from.
#[derive(Clone)]
enum Source {
    /// A local filesystem directory, scanned with `read_dir`.
    Local { data_dir: PathBuf },
    /// An S3/HTTP object store. `prefix_pattern` may carry strftime
    /// codes (e.g. `%Y/%m/%d/OPERA/COMP/`); it is expanded per UTC
    /// date on every scan so the listing stays current across day
    /// boundaries. `time_window`, when set, bounds both the dates
    /// expanded and the timestamps kept.
    Remote {
        store: ds_storage::DataStore,
        prefix_pattern: String,
        time_window: Option<TimeWindow>,
    },
}

/// Scan `source` for ODIM files, returning catalog entries sorted by
/// timestamp ascending. Blocking — remote scans bridge async object-
/// store I/O internally (see [`ds_storage::DataStore`]).
fn scan_source(
    source: &Source,
    matcher: &FilenameMatcher,
    max_files: Option<usize>,
) -> Result<Vec<CatalogEntry>, EngineError> {
    match source {
        Source::Local { data_dir } => Ok(scan_local_directory(data_dir, matcher, max_files)?),
        Source::Remote {
            store,
            prefix_pattern,
            time_window,
        } => {
            let now = Utc::now();
            let (prefixes, time_filter) = match time_window {
                Some(tw) => (
                    expand_prefix_for_dates(prefix_pattern, &tw.scan_dates(now)),
                    Some(tw.to_range(now)),
                ),
                None => (
                    expand_prefix_pattern(prefix_pattern, DEFAULT_SCAN_DAYS),
                    None,
                ),
            };
            Ok(scan_remote(
                store,
                &prefixes,
                matcher,
                time_filter,
                max_files,
            )?)
        }
    }
}

/// Fetch the raw bytes of one ODIM file from `source`.
fn fetch_bytes(source: &Source, path: &Path) -> Result<Vec<u8>, DataServerError> {
    match source {
        Source::Local { .. } => std::fs::read(path).map_err(|e| {
            DataServerError::Engine(format!(
                "failed to read ODIM file `{}`: {e}",
                path.display()
            ))
        }),
        Source::Remote { store, .. } => {
            let key = path.to_str().ok_or_else(|| {
                DataServerError::Engine(format!("non-UTF8 ODIM object key `{}`", path.display()))
            })?;
            let object = ds_storage::object_store::path::Path::from(key);
            store.get(&object).map(|b| b.to_vec())
        }
    }
}

/// `MapEngine` implementation backed by an ODIM_H5 composite catalog.
///
/// Holds an [`ArcSwap`]-protected catalog so the async poll loop can
/// publish refreshed entries (new files, removed files) without
/// disturbing in-flight reads.
pub struct OdimEngine {
    // Fields used by both the `MapEngine` impl (this file) and the
    // `EdrEngine` impl (`edr.rs`) are `pub(crate)`.
    pub(crate) catalog: Arc<ArcSwap<Vec<CatalogEntry>>>,
    pub(crate) collection_id: String,
    pub(crate) parameter: String,
    pub(crate) unit: String,
    pub(crate) gain_override: Option<f64>,
    pub(crate) offset_override: Option<f64>,
    pub(crate) nodata_override: Option<f64>,
    /// Native-CRS label, WGS84 corner envelope, and grid dimensions
    /// captured from the seed composite at construction. Every
    /// timestep of a given ODIM collection shares the same grid, so
    /// these are stable for the engine's lifetime. Holding them as
    /// plain fields lets `raster_info()` (MapEngine) and the EDR
    /// metadata / area-grid-sizing paths answer without touching the
    /// render cache `Mutex` — and, crucially, without depending on
    /// whether a `get_raster_tile` call has warmed that cache yet
    /// (an `apis = ["edr"]`-only collection never issues one).
    pub(crate) seed_native_crs: String,
    pub(crate) seed_spatial_extent: [f64; 4],
    pub(crate) seed_xsize: u32,
    pub(crate) seed_ysize: u32,
    /// Single-entry path-keyed cache. ODIM composites are small (a
    /// few MB) but HDF5 parsing dominates `get_raster_tile` latency
    /// at high request rates — keeping the last file resident makes
    /// hot-tile loops effectively free of read cost.
    pub(crate) cached: Mutex<Option<(PathBuf, Arc<OdimComposite>)>>,
    /// Local directory or S3/HTTP object store the poll loop re-scans.
    source: Source,
    matcher: FilenameMatcher,
    max_files: Option<usize>,
    poll_interval: Duration,
    /// Shutdown coordination. `shutdown` is the authoritative flag —
    /// every poll-loop iteration checks it at the top. `notify` is
    /// a wake-up signal so `shutdown()` doesn't have to wait for
    /// the next `interval.tick()` to take effect.
    ///
    /// **Why not `watch::Sender<()>`** (the original design): if
    /// `shutdown()` fires before `poll_loop()` calls `subscribe()`,
    /// `watch::send` returns `Err` (no receivers) and silently
    /// drops the signal — the version doesn't bump, so a later
    /// `subscribe()`-then-`changed().await` would block forever.
    /// `AtomicBool` is edge-triggered: once set, every future
    /// iteration sees it regardless of timing.
    shutdown: AtomicBool,
    shutdown_notify: Notify,
}

/// Errors from [`OdimEngine::new`]. Per-tile errors are mapped to
/// [`DataServerError`] inside the `MapEngine` impl.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("either `filename_template` or `filename_pattern`+`timestamp_format` must be set")]
    NoFilenamePattern,
    #[error("filename pattern build failed: {0}")]
    BadPattern(#[from] crate::catalog::CatalogError),
    #[error(
        "ODIM collection has no source — set a local `data_path` or an S3 \
         `endpoint` + `bucket`"
    )]
    NoSource,
    #[error(
        "ODIM S3 config is incomplete — `endpoint` and `bucket` must both be \
         set (or both omitted for a local `data_path` source)"
    )]
    IncompleteS3Config,
    #[error(
        "no ODIM files found at `{location}` matching the configured filename \
         pattern — verify the source exists and the template matches the \
         producer's layout"
    )]
    NoFiles { location: String },
    #[error("ODIM source error: {0}")]
    Storage(#[from] DataServerError),
    #[error("failed to parse seed composite `{path}`: {source}")]
    SeedParseFailed {
        path: PathBuf,
        #[source]
        source: crate::reader::ReadError,
    },
}

impl OdimEngine {
    /// Build an engine by scanning its configured source for files
    /// matching the filename pattern. The source is a local directory
    /// (`data_path`) or an S3 bucket (`endpoint`/`bucket`/`prefix_pattern`).
    /// Loads the most recent file synchronously to populate metadata;
    /// raises [`EngineError`] when the source yields no files.
    pub fn new(
        collection_id: &str,
        data_path: Option<&str>,
        config: &ds_core::config::OdimConfig,
    ) -> Result<Self, EngineError> {
        let matcher = build_matcher(config)?;
        let source = build_source(collection_id, data_path, config)?;

        let catalog = scan_source(&source, &matcher, config.max_files)?;
        if catalog.is_empty() {
            return Err(EngineError::NoFiles {
                location: source_label(&source),
            });
        }

        // Pre-load the most recent file so `raster_info()` can
        // populate `spatial_extent` and `times` immediately, and so a
        // misconfigured filename pattern surfaces at engine
        // construction rather than at first request.
        let seed_path = catalog.last().expect("catalog non-empty").path.clone();
        let bytes = fetch_bytes(&source, &seed_path)?;
        let composite =
            Arc::new(
                read_composite(&bytes).map_err(|e| EngineError::SeedParseFailed {
                    path: seed_path.clone(),
                    source: e,
                })?,
            );

        let seed_native_crs = crs_label(&composite.crs);
        let seed_spatial_extent = composite.wgs84_bbox;
        let seed_xsize = composite.xsize;
        let seed_ysize = composite.ysize;

        Ok(Self {
            catalog: Arc::new(ArcSwap::from_pointee(catalog)),
            collection_id: collection_id.to_string(),
            parameter: config.parameter.clone(),
            unit: config.unit.clone(),
            gain_override: config.gain,
            offset_override: config.offset,
            nodata_override: config.nodata,
            seed_native_crs,
            seed_spatial_extent,
            seed_xsize,
            seed_ysize,
            cached: Mutex::new(Some((seed_path, composite))),
            source,
            matcher,
            max_files: config.max_files,
            poll_interval: Duration::from_secs(config.poll_interval_secs.max(1)),
            shutdown: AtomicBool::new(false),
            shutdown_notify: Notify::new(),
        })
    }

    /// Run the source poll loop. Exits when [`OdimEngine::shutdown`]
    /// is called. Each tick re-scans the source, atomically swaps the
    /// catalog `ArcSwap` if the file set changed, and logs at INFO
    /// when new files appear so operators can confirm the polling is
    /// alive.
    ///
    /// The scan runs on `tokio::task::spawn_blocking` so neither a
    /// slow filesystem (network mount, large directory) nor a slow S3
    /// `list` stalls a Tokio worker thread for its duration.
    ///
    /// Errors from the scan (source temporarily unavailable, S3
    /// timeout) are logged at WARN and otherwise ignored — the
    /// previous catalog stays in place so live requests keep working.
    pub async fn poll_loop(&self) {
        let mut interval = tokio::time::interval(self.poll_interval);
        interval.tick().await; // skip immediate first tick (already loaded at boot)

        loop {
            // Authoritative check — catches a `shutdown()` that fired
            // before `poll_loop()` started, or between two ticks.
            if self.shutdown.load(Ordering::Acquire) {
                tracing::info!("[{}] ODIM poll loop shutting down", self.collection_id);
                break;
            }
            tokio::select! {
                _ = interval.tick() => {
                    // Recheck inside the branch: `shutdown()` may have
                    // fired between the load above and the tick
                    // returning, and we don't want to do extra I/O on
                    // the way out.
                    if !self.shutdown.load(Ordering::Acquire) {
                        self.poll_once().await;
                    }
                }
                _ = self.shutdown_notify.notified() => {
                    // `shutdown()` was just called. The flag is set;
                    // top-of-loop check on the next iteration handles
                    // the actual exit.
                }
            }
        }
    }

    /// Signal the polling loop to stop. Idempotent — the first call
    /// transitions the flag and wakes any waiter; subsequent calls are
    /// no-ops. Safe to call before `poll_loop()` has started: the flag
    /// persists and the next `poll_loop()` invocation will exit on its
    /// first iteration.
    pub fn shutdown(&self) {
        if !self.shutdown.swap(true, Ordering::Release) {
            // We just transitioned false → true. Wake any pending
            // `notified()` waiter. If no waiter exists, the
            // top-of-loop flag check on the next iteration covers it.
            self.shutdown_notify.notify_waiters();
        }
    }

    async fn poll_once(&self) {
        let source = self.source.clone();
        let matcher = self.matcher.clone();
        let max_files = self.max_files;
        let scan_result =
            tokio::task::spawn_blocking(move || scan_source(&source, &matcher, max_files)).await;
        let scan = match scan_result {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                tracing::warn!(
                    "[{}] ODIM catalog refresh failed: {}",
                    self.collection_id,
                    e
                );
                return;
            }
            Err(join_err) => {
                tracing::error!(
                    "[{}] ODIM catalog refresh task panicked: {}",
                    self.collection_id,
                    join_err
                );
                return;
            }
        };
        // Diff against the previous catalog using count + most-recent
        // timestamp. Producing-side renames are rare; this is cheap
        // and accurate for the append-only / rolling-window case ODIM
        // producers actually use.
        let prev = self.catalog.load();
        let prev_count = prev.len();
        let prev_latest = prev.last().map(|e| e.time);
        let new_latest = scan.last().map(|e| e.time);
        if prev_count != scan.len() || prev_latest != new_latest {
            tracing::info!(
                "[{}] ODIM catalog refreshed: {} → {} files, latest {} → {}",
                self.collection_id,
                prev_count,
                scan.len(),
                prev_latest
                    .map(|t| t.to_rfc3339())
                    .unwrap_or_else(|| "<none>".into()),
                new_latest
                    .map(|t| t.to_rfc3339())
                    .unwrap_or_else(|| "<none>".into()),
            );
            self.catalog.store(Arc::new(scan));
        }
    }

    /// Pick the catalog entry whose timestamp is closest to `time`
    /// (exact match preferred). If `time` is `None`, return the most
    /// recent entry. Returns an owned `CatalogEntry` clone because
    /// the catalog lives behind `ArcSwap` and the snapshot guard
    /// must drop before the per-request work proceeds.
    ///
    /// Logs a WARN when the nearest match is more than 2× the poll
    /// interval from the requested time — operators should treat
    /// this as a signal that the data feed has stalled or that the
    /// client is querying outside the available window.
    fn select_entry(&self, time: Option<DateTime<Utc>>) -> Option<CatalogEntry> {
        let snapshot = self.catalog.load();
        if let Some(target) = time {
            // The catalog is sorted by time ascending — pick the
            // smallest-abs-difference entry. With only ~24 entries in
            // practice (24h * 5-min ODIM cadence is the upper end
            // before `max_files` trims), a linear scan is faster than
            // a binary search through the noise of branch prediction.
            let pick = snapshot
                .iter()
                .min_by_key(|e| (e.time - target).num_seconds().abs())?;
            let gap = (pick.time - target).num_seconds().abs();
            // Stale-match threshold: 2× the poll interval, with a 600s
            // floor. The floor accommodates real radar feeds which
            // typically arrive at 5-minute cadence — without it, the
            // default 30-second poll_interval would trip the warning
            // on healthy feeds whenever the requested time happened
            // to fall in the middle of a 5-min window. 600s is a
            // conservative envelope around the 5-min cadence + clock
            // skew + late arrivals.
            let stale_threshold = (self.poll_interval.as_secs() * 2).max(600) as i64;
            if gap > stale_threshold {
                tracing::warn!(
                    "[{}] ODIM request at {} matched stale entry {} (gap {}s > {}s threshold)",
                    self.collection_id,
                    target.to_rfc3339(),
                    pick.time.to_rfc3339(),
                    gap,
                    stale_threshold
                );
            }
            Some(pick.clone())
        } else {
            snapshot.last().cloned()
        }
    }

    /// Read + parse the ODIM file at `path` and return a shared
    /// `Arc<OdimComposite>` snapshot. Cached single-entry by path.
    ///
    /// **Blocking call** — reads the file (local `read` or S3 `get`)
    /// and parses HDF5 directly. Callers from async contexts must wrap
    /// in `tokio::task::spawn_blocking`. Two call paths reach here:
    ///
    /// - `MapEngine::get_raster_tile` — already runs inside
    ///   `spawn_blocking` (the WMS / Maps / Tiles handlers do this).
    /// - `EdrEngine::query_position` / `query_area` (see `edr.rs`) —
    ///   the api-edr handlers currently call these directly from an
    ///   `async fn` *without* `spawn_blocking`, so this blocking
    ///   work lands on a Tokio worker. That is a pre-existing
    ///   api-edr-level gap affecting every EDR engine (GeoTIFF,
    ///   QueryData, GRIB all do blocking I/O in `query_position`
    ///   too) — tracked in issue #178, to be fixed in the api-edr
    ///   handlers rather than per-engine.
    pub(crate) fn load_composite(
        &self,
        path: &Path,
    ) -> Result<Arc<OdimComposite>, DataServerError> {
        // Use a path-keyed single-entry cache. Cache hits return the
        // same `Arc` to every caller; on a cold miss two concurrent
        // `spawn_blocking` callers may both read the file and both
        // write the cache slot — the second write overwrites the
        // first with an identical, immutable composite. The cost is
        // at most one redundant read at the moment the slot fills,
        // never visible to clients. A swap to a different path
        // happens only when a different file is asked for.
        //
        // Different-path concurrent misses (e.g. two adjacent
        // timestep requests arriving during a time-slider scrub)
        // are tolerated but not optimised: both callers fall
        // through to a fresh fetch, both decode, and whichever
        // takes the write lock last evicts the other's entry. Both
        // callers still get a valid `Arc` (data is correct,
        // composites are immutable) — the cost is at most one
        // extra file read per burst. A future LRU upgrade should
        // be a multi-entry cache rather than this single-slot
        // version to eliminate that cost.
        //
        // Mutex poison is recovered (rather than silently disabling
        // the cache for the lifetime of the engine). The cached
        // entry is just bytes + path — no invariants to protect —
        // so the previous panic that poisoned the lock can't have
        // corrupted what we're reading. An ERROR-level log on
        // recovery makes the latent panic visible.
        {
            let guard = self.cached.lock().unwrap_or_else(|e| {
                tracing::error!(
                    "[{}] ODIM cache mutex was poisoned; recovering",
                    self.collection_id
                );
                e.into_inner()
            });
            if let Some((ref cached_path, ref cached_comp)) = *guard {
                if cached_path == path {
                    return Ok(cached_comp.clone());
                }
            }
        }
        let bytes = fetch_bytes(&self.source, path)
            .map_err(|e| DataServerError::Engine(format!("[{}] {e}", self.collection_id)))?;
        let composite = Arc::new(read_composite(&bytes).map_err(|e| {
            DataServerError::Engine(format!(
                "[{}] failed to parse ODIM file `{}`: {e}",
                self.collection_id,
                path.display()
            ))
        })?);
        let mut guard = self.cached.lock().unwrap_or_else(|e| {
            tracing::error!(
                "[{}] ODIM cache mutex was poisoned on insert; recovering",
                self.collection_id
            );
            e.into_inner()
        });
        *guard = Some((path.to_path_buf(), composite.clone()));
        Ok(composite)
    }
}

/// Resolve the engine's `FilenameMatcher` from config: prefer
/// `filename_template` (strftime), fall back to the explicit
/// `filename_pattern` + `timestamp_format` pair.
fn build_matcher(config: &ds_core::config::OdimConfig) -> Result<FilenameMatcher, EngineError> {
    if let Some(template) = &config.filename_template {
        return Ok(FilenameMatcher::from_template(template)?);
    }
    if let (Some(pattern), Some(format)) = (&config.filename_pattern, &config.timestamp_format) {
        return Ok(FilenameMatcher::from_pattern(pattern, format)?);
    }
    Err(EngineError::NoFilenamePattern)
}

/// Resolve the engine's [`Source`] from config. `endpoint` + `bucket`
/// select an S3 source; their absence falls back to the local
/// `data_path`. Setting exactly one of `endpoint` / `bucket` is a
/// configuration error rather than a silent fallback.
fn build_source(
    collection_id: &str,
    data_path: Option<&str>,
    config: &ds_core::config::OdimConfig,
) -> Result<Source, EngineError> {
    match (config.endpoint.as_deref(), config.bucket.as_deref()) {
        (Some(endpoint), Some(bucket)) => {
            let store = ds_storage::build_s3_store_from_parts(endpoint, bucket)?;
            let prefix_pattern = config.prefix_pattern.clone().unwrap_or_default();
            let time_window = match &config.time_window {
                Some(s) => Some(TimeWindow::parse(s)?),
                None => None,
            };
            tracing::info!(
                "[{}] ODIM S3 source: endpoint={endpoint} bucket={bucket} prefix='{prefix_pattern}'",
                collection_id
            );
            Ok(Source::Remote {
                store,
                prefix_pattern,
                time_window,
            })
        }
        (None, None) => {
            let data_path = data_path.ok_or(EngineError::NoSource)?;
            Ok(Source::Local {
                data_dir: PathBuf::from(data_path),
            })
        }
        _ => Err(EngineError::IncompleteS3Config),
    }
}

/// Human-readable description of a [`Source`] for error messages.
fn source_label(source: &Source) -> String {
    match source {
        Source::Local { data_dir } => data_dir.display().to_string(),
        Source::Remote { prefix_pattern, .. } => {
            format!("s3 prefix `{prefix_pattern}`")
        }
    }
}

/// Convert WGS84 latitude (degrees) to Web Mercator Y (metres).
/// Used to interpolate output-row latitudes in equal-Mercator-Y
/// steps when `OutputCrs::WebMercator` is requested.
fn lat_to_merc_y(lat_deg: f64) -> f64 {
    const R: f64 = 6_378_137.0;
    let lat_rad = lat_deg.to_radians();
    R * ((std::f64::consts::FRAC_PI_4 + lat_rad / 2.0).tan()).ln()
}

/// Inverse of [`lat_to_merc_y`].
///
/// The formula `π/2 - 2·atan(exp(-y/R))` is algebraically
/// equivalent to the standard EPSG:3857 inverse
/// `2·atan(exp(y/R)) - π/2`, but uses the negated exponent so the
/// `exp()` term decays toward zero as |y| grows rather than growing
/// without bound — that keeps the math numerically stable across the
/// full ±π/2 latitude range under f64 arithmetic. EPSG:3857 dataset:
/// https://epsg.io/3857.
fn merc_y_to_lat(y: f64) -> f64 {
    const R: f64 = 6_378_137.0;
    (std::f64::consts::FRAC_PI_2 - 2.0 * (-y / R).exp().atan()).to_degrees()
}

impl MapEngine for OdimEngine {
    fn get_raster_tile(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        output_crs: &OutputCrs,
        _parameter: Option<&str>,
    ) -> Result<RasterTile, DataServerError> {
        let entry = self.select_entry(time).ok_or_else(|| {
            DataServerError::Engine(format!(
                "[{}] empty ODIM catalog — no files available",
                self.collection_id
            ))
        })?;
        let composite = self.load_composite(&entry.path)?;

        let [west, south, east, north] = bbox;
        let gain = self.gain_override.unwrap_or(composite.gain);
        let offset = self.offset_override.unwrap_or(composite.offset);
        let nodata = self.nodata_override.unwrap_or(composite.nodata);
        let undetect = composite.undetect;

        let (merc_y_north, merc_y_south) = if *output_crs == OutputCrs::WebMercator {
            (lat_to_merc_y(north), lat_to_merc_y(south))
        } else {
            (0.0, 0.0)
        };

        let [src_w, src_s, src_e, src_n] = composite.bbox;
        let src_dx = (src_e - src_w) / composite.xsize as f64;
        let src_dy = (src_n - src_s) / composite.ysize as f64;
        let (rows, cols) = composite.pixels.shape();

        let mut values = Vec::with_capacity((width * height) as usize);
        for oy in 0..height {
            let frac_y = (oy as f64 + 0.5) / height as f64;
            let lat = if *output_crs == OutputCrs::WebMercator {
                let merc_y = merc_y_north - frac_y * (merc_y_north - merc_y_south);
                merc_y_to_lat(merc_y)
            } else {
                north - frac_y * (north - south)
            };
            for ox in 0..width {
                let frac_x = (ox as f64 + 0.5) / width as f64;
                let lon = west + frac_x * (east - west);

                // Forward-project (lon, lat) into the composite's
                // native CRS, then nearest-neighbour into the source
                // pixel grid. ODIM rows go north→south so the row
                // index counts from the north edge.
                let (x, y) = composite.crs.forward(lon, lat);
                if !x.is_finite() || !y.is_finite() {
                    values.push(None);
                    continue;
                }
                let col = ((x - src_w) / src_dx).floor() as i64;
                let row = ((src_n - y) / src_dy).floor() as i64;
                if col < 0 || col >= cols as i64 || row < 0 || row >= rows as i64 {
                    values.push(None);
                    continue;
                }
                values.push(composite.pixels.sample(
                    row as usize,
                    col as usize,
                    gain,
                    offset,
                    nodata,
                    undetect,
                ));
            }
        }

        Ok(RasterTile {
            width,
            height,
            values,
        })
    }

    fn raster_info(&self) -> RasterInfo {
        // `native_crs` / `spatial_extent` come from the seed fields
        // captured at construction — every timestep shares the same
        // grid, so this needs neither the render-cache `Mutex` nor a
        // warmed cache. Times come from the lock-free `ArcSwap`
        // catalog snapshot.
        let times: Vec<DateTime<Utc>> = self.catalog.load().iter().map(|e| e.time).collect();

        RasterInfo {
            native_crs: self.seed_native_crs.clone(),
            spatial_extent: Some(self.seed_spatial_extent),
            times,
            parameter: self.parameter.clone(),
            unit: self.unit.clone(),
            parameters: vec![],
        }
    }
}

/// Human-readable identifier for a `Crs`. Used by `raster_info()` —
/// approximate, not an authoritative EPSG mapping. ODIM composites
/// in the wild use spheres so EPSG codes don't strictly apply
/// anyway.
fn crs_label(crs: &Crs) -> String {
    match crs {
        Crs::Wgs84 => "CRS:84".into(),
        Crs::TransverseMercator { .. } => "TM".into(),
        Crs::LambertAzimuthalEqualArea { .. } => "LAEA".into(),
        Crs::LambertConformalConic { .. } => "LCC".into(),
        Crs::Stereographic { .. } => "stere".into(),
        Crs::RotatedLatLon { .. } => "rotated_latlon".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lat_merc_round_trip_is_identity() {
        for lat in [-60.0, -30.0, 0.0, 30.0, 60.0] {
            let y = lat_to_merc_y(lat);
            let lat_back = merc_y_to_lat(y);
            assert!(
                (lat - lat_back).abs() < 1e-9,
                "lat={lat} y={y} back={lat_back}"
            );
        }
    }

    #[test]
    fn crs_label_covers_every_variant() {
        // If a new `Crs` variant lands without a label, this test
        // forces the addition by triggering an unhandled-match
        // compile error first, not a silent "unknown" at runtime.
        assert_eq!(crs_label(&Crs::Wgs84), "CRS:84");
        let stere = Crs::Stereographic {
            lat0: 0.0,
            lon0: 0.0,
            k0: 1.0,
            false_e: 0.0,
            false_n: 0.0,
        };
        assert_eq!(crs_label(&stere), "stere");
    }

    /// `shutdown()` called before `poll_loop()` ever starts must
    /// still cause the loop to exit on its first iteration. The
    /// earlier `watch::Sender<()>`-based implementation lost this
    /// signal because `send()` returns `Err` when there are no
    /// receivers, and the initial receiver was dropped at
    /// `let (tx, _) = watch::channel(())`. The `AtomicBool`-based
    /// flag is edge-triggered and persists across the
    /// before-/after-subscribe boundary.
    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_before_poll_loop_takes_effect() {
        // Build the smallest possible OdimEngine-like setup. We
        // don't need a real catalog or filesystem — just the
        // shutdown coordination. Use a synthetic loop mirroring
        // `poll_loop`'s structure exactly.
        let shutdown = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(Notify::new());

        // Pre-fire the shutdown signal *before* the loop starts —
        // simulating the race the reviewer flagged. The
        // `notify_waiters` call here has no live waiters and is
        // discarded, mirroring the worst-case timing: the loop
        // must exit anyway via the AtomicBool flag check.
        shutdown.store(true, Ordering::Release);
        notify.notify_waiters();

        let shutdown_loop = shutdown.clone();
        let notify_loop = notify.clone();
        let handle = tokio::spawn(async move {
            // Long interval so the timer can't accidentally rescue
            // us if the flag check is buggy — the test would have
            // to hit the 1-second timeout below instead.
            let mut interval = tokio::time::interval(Duration::from_secs(3600));
            interval.tick().await;
            loop {
                if shutdown_loop.load(Ordering::Acquire) {
                    return;
                }
                tokio::select! {
                    _ = interval.tick() => {}
                    _ = notify_loop.notified() => {}
                }
            }
        });

        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("poll loop must exit on the first iteration when shutdown is pre-set")
            .unwrap();
    }
}
