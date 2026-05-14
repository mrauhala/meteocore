//! `MapEngine` impl backed by an ODIM_H5 composite catalog.
//!
//! At construction time, [`OdimEngine::new`] scans the configured
//! local directory for ODIM files matching the `filename_template`
//! pattern and pre-loads the most recent one so `raster_info()`
//! can answer with non-empty `spatial_extent` and `times`. After
//! that, every `get_raster_tile` call:
//!
//! 1. Picks the catalog entry matching the requested time (or the
//!    most recent if `time` is `None`).
//! 2. Loads the composite into a single-entry path-keyed cache
//!    (subsequent same-file reads avoid disk + HDF5 reparse).
//! 3. Walks the output grid pixel-by-pixel, projecting the WGS84
//!    or Web-Mercator output coords into the composite's native
//!    CRS, and samples via nearest-neighbour.
//!
//! Phase 1 narrows the scope from the format's full capabilities:
//! - Local directories only (no S3 / STAC — those land later)
//! - Single-parameter (one composite quantity per collection)
//!
//! A background `poll_loop` re-scans `data_dir` every
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
use tokio::sync::watch;

use crate::catalog::{scan_local_directory, CatalogEntry, FilenameMatcher};
use crate::reader::{read_composite, OdimComposite};

/// `MapEngine` implementation backed by an ODIM_H5 composite catalog.
///
/// Holds an [`ArcSwap`]-protected catalog so the async poll loop can
/// publish refreshed entries (new files, removed files) without
/// disturbing in-flight reads.
pub struct OdimEngine {
    catalog: Arc<ArcSwap<Vec<CatalogEntry>>>,
    collection_id: String,
    parameter: String,
    unit: String,
    gain_override: Option<f64>,
    offset_override: Option<f64>,
    nodata_override: Option<f64>,
    /// Single-entry path-keyed cache. ODIM composites are small (a
    /// few MB) but HDF5 parsing dominates `get_raster_tile` latency
    /// at high request rates — keeping the last file resident makes
    /// hot-tile loops effectively free of read cost.
    cached: Mutex<Option<(PathBuf, Arc<OdimComposite>)>>,
    /// Source state for the poll loop.
    data_dir: PathBuf,
    matcher: FilenameMatcher,
    max_files: Option<usize>,
    poll_interval: Duration,
    shutdown_tx: watch::Sender<()>,
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
        "no ODIM files found in `{dir}` matching the configured filename pattern — \
         verify the directory exists and the template matches the producer's layout"
    )]
    NoFiles { dir: PathBuf },
    #[error("failed to read seed composite `{path}`: {source}")]
    SeedReadFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse seed composite `{path}`: {source}")]
    SeedParseFailed {
        path: PathBuf,
        #[source]
        source: crate::reader::ReadError,
    },
}

impl OdimEngine {
    /// Build an engine by scanning `data_dir` for files matching the
    /// configured filename pattern. Loads the most recent file
    /// synchronously to populate metadata; raises [`EngineError`]
    /// when the directory is empty.
    pub fn new(
        data_dir: &Path,
        collection_id: &str,
        config: &ds_core::config::OdimConfig,
    ) -> Result<Self, EngineError> {
        // Phase 1 supports local directories only. S3-style config
        // fields (`endpoint`, `bucket`, `prefix_pattern`) are present
        // in `OdimConfig` as Phase 2 stubs — if an operator filled
        // them in expecting remote behaviour, warn loudly so the
        // misconfiguration is visible rather than silently producing
        // a "local data not found" error.
        if config.endpoint.is_some() || config.bucket.is_some() || config.prefix_pattern.is_some() {
            tracing::warn!(
                "[{}] ODIM config carries S3 fields (endpoint/bucket/prefix_pattern) but \
                 Phase 1 supports local directories only — these fields are ignored. \
                 Remote source support lands in Phase 2 (STAC) / Phase 3 (PVOL S3).",
                collection_id
            );
        }

        let matcher = build_matcher(config)?;
        let catalog = scan_local_directory(data_dir, &matcher, config.max_files)?;
        if catalog.is_empty() {
            return Err(EngineError::NoFiles {
                dir: data_dir.to_path_buf(),
            });
        }

        // Pre-load the most recent file so `raster_info()` can
        // populate `spatial_extent` and `times` immediately, and so a
        // misconfigured filename pattern surfaces at engine
        // construction rather than at first request.
        let seed_path = catalog.last().expect("catalog non-empty").path.clone();
        let bytes = std::fs::read(&seed_path).map_err(|e| EngineError::SeedReadFailed {
            path: seed_path.clone(),
            source: e,
        })?;
        let composite =
            Arc::new(
                read_composite(&bytes).map_err(|e| EngineError::SeedParseFailed {
                    path: seed_path.clone(),
                    source: e,
                })?,
            );

        let (shutdown_tx, _) = watch::channel(());

        Ok(Self {
            catalog: Arc::new(ArcSwap::from_pointee(catalog)),
            collection_id: collection_id.to_string(),
            parameter: config.parameter.clone(),
            unit: config.unit.clone(),
            gain_override: config.gain,
            offset_override: config.offset,
            nodata_override: config.nodata,
            cached: Mutex::new(Some((seed_path, composite))),
            data_dir: data_dir.to_path_buf(),
            matcher,
            max_files: config.max_files,
            poll_interval: Duration::from_secs(config.poll_interval_secs.max(1)),
            shutdown_tx,
        })
    }

    /// Run the directory poll loop. Exits when [`OdimEngine::shutdown`]
    /// is called. Each tick re-scans `data_dir`, atomically swaps the
    /// catalog `ArcSwap` if the file set changed, and logs at INFO
    /// when new files appear so operators can confirm the polling is
    /// alive.
    ///
    /// The actual `read_dir` runs on `tokio::task::spawn_blocking`
    /// so a slow filesystem (network mount, large directory) doesn't
    /// stall the Tokio worker thread for the duration of the scan.
    ///
    /// Errors from the scan (e.g. `data_dir` temporarily disappears)
    /// are logged at WARN and otherwise ignored — the previous
    /// catalog stays in place so live requests keep working.
    pub async fn poll_loop(&self) {
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        let mut interval = tokio::time::interval(self.poll_interval);
        interval.tick().await; // skip immediate first tick (already loaded at boot)

        loop {
            tokio::select! {
                _ = interval.tick() => self.poll_once().await,
                _ = shutdown_rx.changed() => {
                    tracing::info!("[{}] ODIM poll loop shutting down", self.collection_id);
                    break;
                }
            }
        }
    }

    /// Signal the polling loop to stop. Idempotent.
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(());
    }

    async fn poll_once(&self) {
        let data_dir = self.data_dir.clone();
        let matcher = self.matcher.clone();
        let max_files = self.max_files;
        let scan_result = tokio::task::spawn_blocking(move || {
            scan_local_directory(&data_dir, &matcher, max_files)
        })
        .await;
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
            let stale_threshold = (self.poll_interval.as_secs() * 2).max(60) as i64;
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

    fn load_composite(&self, path: &Path) -> Result<Arc<OdimComposite>, DataServerError> {
        // Use a path-keyed single-entry cache. Concurrent requests
        // for the same file get the same `Arc`; a swap happens only
        // when a different file is asked for.
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
        let bytes = std::fs::read(path).map_err(|e| {
            DataServerError::Engine(format!(
                "[{}] failed to read ODIM file `{}`: {e}",
                self.collection_id,
                path.display()
            ))
        })?;
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

/// Convert WGS84 latitude (degrees) to Web Mercator Y (metres).
/// Used to interpolate output-row latitudes in equal-Mercator-Y
/// steps when `OutputCrs::WebMercator` is requested.
fn lat_to_merc_y(lat_deg: f64) -> f64 {
    const R: f64 = 6_378_137.0;
    let lat_rad = lat_deg.to_radians();
    R * ((std::f64::consts::FRAC_PI_4 + lat_rad / 2.0).tan()).ln()
}

/// Inverse of [`lat_to_merc_y`].
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
        // Read native_crs and spatial_extent from the seed-loaded
        // composite under a single lock acquisition. Times come from
        // the catalog (separate lock-free ArcSwap snapshot). Recover
        // from a poisoned mutex (same rationale as `load_composite`).
        let cached_guard = self.cached.lock().unwrap_or_else(|e| {
            tracing::error!(
                "[{}] ODIM cache mutex was poisoned in raster_info; recovering",
                self.collection_id
            );
            e.into_inner()
        });
        let (native_crs, spatial_extent) = cached_guard
            .as_ref()
            .map(|(_, c)| (crs_label(&c.crs), Some(c.wgs84_corners)))
            .unwrap_or_else(|| ("unknown".into(), None));
        drop(cached_guard);

        let times: Vec<DateTime<Utc>> = self.catalog.load().iter().map(|e| e.time).collect();

        RasterInfo {
            native_crs,
            spatial_extent,
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
}
