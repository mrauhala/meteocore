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
//! - No async polling — the engine is fully synchronous and
//!   refreshes the catalog only when `new` runs. A poll loop and
//!   `ArcSwap` swap on file changes are the next commit.
//! - Single-parameter (one composite quantity per collection).
//!
//! See [[project_odim_engine_plan]] for the full multi-phase plan.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use ds_core::error::DataServerError;
use ds_core::geo::Crs;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterInfo, RasterTile};

use crate::catalog::{scan_local_directory, CatalogEntry, FilenameMatcher};
use crate::reader::{read_composite, OdimComposite};

/// `MapEngine` implementation backed by an ODIM_H5 composite catalog.
pub struct OdimEngine {
    catalog: Vec<CatalogEntry>,
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

        Ok(Self {
            catalog,
            collection_id: collection_id.to_string(),
            parameter: config.parameter.clone(),
            unit: config.unit.clone(),
            gain_override: config.gain,
            offset_override: config.offset,
            nodata_override: config.nodata,
            cached: Mutex::new(Some((seed_path, composite))),
        })
    }

    /// Pick the catalog entry whose timestamp is closest to `time`
    /// (exact match preferred). If `time` is `None`, return the most
    /// recent entry. Empty-catalog and missing-time cases bubble up
    /// as `LocationNotFound` from the caller.
    fn select_entry(&self, time: Option<DateTime<Utc>>) -> Option<&CatalogEntry> {
        let Some(target) = time else {
            return self.catalog.last();
        };
        // The catalog is sorted by time ascending — pick the
        // smallest-abs-difference entry. With only ~24 entries in
        // practice (24h * 5-min ODIM cadence is the upper end before
        // `max_files` trims), a linear scan is faster than a binary
        // search through the noise of branch prediction.
        self.catalog
            .iter()
            .min_by_key(|e| (e.time - target).num_seconds().abs())
    }

    fn load_composite(&self, path: &Path) -> Result<Arc<OdimComposite>, DataServerError> {
        // Use a path-keyed single-entry cache. Concurrent requests
        // for the same file get the same `Arc`; a swap happens only
        // when a different file is asked for.
        if let Ok(guard) = self.cached.lock() {
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
        if let Ok(mut guard) = self.cached.lock() {
            *guard = Some((path.to_path_buf(), composite.clone()));
        }
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
        // composite. Times come from the catalog.
        let native_crs = if let Ok(guard) = self.cached.lock() {
            guard.as_ref().map(|(_, c)| crs_label(&c.crs))
        } else {
            None
        }
        .unwrap_or_else(|| "unknown".into());

        let spatial_extent = if let Ok(guard) = self.cached.lock() {
            guard.as_ref().map(|(_, c)| c.wgs84_corners)
        } else {
            None
        };

        let times: Vec<DateTime<Utc>> = self.catalog.iter().map(|e| e.time).collect();

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
