//! `MapEngine` impl backed by an ODIM_H5 polar-volume (PVOL) catalog.
//!
//! Where [`crate::engine::OdimEngine`] serves pre-projected 2-D `COMP`
//! composites, this engine serves **native polar volumes** — multi-
//! elevation, multi-moment radar data in spherical (range × azimuth)
//! coordinates — and resamples them into Cartesian raster tiles on the
//! fly.
//!
//! ## Layer model
//!
//! A PVOL collection covers a radar **network**: a local directory of
//! `.h5` polar-volume files spanning multiple sites and multiple
//! acquisition times. Each radar **site** is a layer group; each radar
//! **quantity** (`DBZH`, `TH`, `VRADH`, `ZDR`, …) is a sub-layer —
//! exactly like a multi-parameter GRIB collection. The advertised
//! parameter name is `<nod>:<quantity>` (e.g. `fianj:DBZH`).
//!
//! The `:` separator is deliberate. WMS layer names are
//! `collection-id/parameter` and the api-wms handler splits on the
//! *first* `/`, so the parameter token must not itself contain `/`.
//! `:` is safe in a WMS `LAYERS=` value and in a Maps/Tiles
//! `?parameter-name=` query parameter, and reads naturally as a
//! site-scoped quantity.
//!
//! ## Interim scope (Milestone 2)
//!
//! - Renders **only the lowest elevation sweep** (`sweeps[0]`) of each
//!   site. Higher sweeps are parsed but not exposed.
//! - **No mosaic / compositing** — each site is rendered independently.
//! - Treats the sweep range axis as **ground range**: a proper slant-
//!   range / 4⁄3-Earth ground-range correction is deferred. For a
//!   near-horizon lowest sweep the error is small; see [`polar_sample`].
//!
//! A background `poll_loop` re-scans `data_path` every
//! `poll_interval_secs` and atomically swaps the catalog so new volume
//! files appear without an admin reload — mirroring `OdimEngine`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use ds_core::error::DataServerError;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterInfo, RasterTile};
use tokio::sync::Notify;

use crate::engine::EngineError;
use crate::pvol::{read_polar_volume, PolarVolume};

/// Separator between the ODIM node id and the radar quantity in an
/// advertised parameter name (`fianj:DBZH`). See the module docs for
/// why `:` and not `/`.
pub const SITE_QUANTITY_SEP: char = ':';

/// Mean Earth radius (metres) used by the geodesic helper. A sphere is
/// the right model here: radar ground range is itself a spherical
/// approximation, and the per-pixel error of WGS84-vs-sphere at radar
/// ranges (≤ ~250 km) is far below one output pixel.
const EARTH_RADIUS_M: f64 = 6_371_000.0;

/// Web-Mercator sphere radius (metres) — EPSG:3857 uses 6378137.
const MERC_RADIUS_M: f64 = 6_378_137.0;

// ---------------------------------------------------------------------------
// Geodesic helper
// ---------------------------------------------------------------------------

/// Ground distance (metres) and initial bearing (degrees, 0° = north,
/// clockwise) from `(lon0, lat0)` to `(lon1, lat1)` on a sphere of
/// radius [`EARTH_RADIUS_M`].
///
/// Distance is the haversine great-circle distance; bearing is the
/// standard initial-bearing (forward azimuth) formula, normalised to
/// `[0, 360)`. All inputs are degrees.
///
/// Kept as a small standalone function so it can be unit-tested in
/// isolation — the polar→Cartesian resampler's correctness hinges on
/// it.
pub fn ground_distance_bearing(lon0: f64, lat0: f64, lon1: f64, lat1: f64) -> (f64, f64) {
    let lat0_r = lat0.to_radians();
    let lat1_r = lat1.to_radians();
    let dlat = (lat1 - lat0).to_radians();
    let dlon = (lon1 - lon0).to_radians();

    // Haversine great-circle distance.
    let a = (dlat / 2.0).sin().powi(2) + lat0_r.cos() * lat1_r.cos() * (dlon / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().clamp(0.0, 1.0).asin();
    let distance = EARTH_RADIUS_M * c;

    // Initial bearing (forward azimuth): atan2 of the east/north
    // components of the great-circle tangent at the start point.
    let y = dlon.sin() * lat1_r.cos();
    let x = lat0_r.cos() * lat1_r.sin() - lat0_r.sin() * lat1_r.cos() * dlon.cos();
    let bearing = y.atan2(x).to_degrees().rem_euclid(360.0);

    (distance, bearing)
}

// ---------------------------------------------------------------------------
// Web-Mercator row interpolation (mirrors engine.rs)
// ---------------------------------------------------------------------------

/// WGS84 latitude (degrees) → Web-Mercator Y (metres).
fn lat_to_merc_y(lat_deg: f64) -> f64 {
    let lat_rad = lat_deg.to_radians();
    MERC_RADIUS_M * ((std::f64::consts::FRAC_PI_4 + lat_rad / 2.0).tan()).ln()
}

/// Inverse of [`lat_to_merc_y`].
fn merc_y_to_lat(y: f64) -> f64 {
    (std::f64::consts::FRAC_PI_2 - 2.0 * (-y / MERC_RADIUS_M).exp().atan()).to_degrees()
}

// ---------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------

/// One parsed polar volume in the catalog. The volume is held behind
/// `Arc` so the catalog can be cheaply cloned out of the `ArcSwap`
/// snapshot without re-parsing HDF5.
#[derive(Clone)]
struct VolumeEntry {
    /// Source file path — the parse-cache key; used to evict cache
    /// entries the (`max_files`-capped) catalog no longer references.
    path: PathBuf,
    /// Parsed volume — `Arc` so repeated requests share one parse.
    volume: Arc<PolarVolume>,
}

/// The engine's catalog: per-site time-sorted volume lists, plus the
/// derived metadata `raster_info()` answers from.
struct Catalog {
    /// Volumes grouped by `site.nod`, each list sorted by `time`
    /// ascending.
    by_site: HashMap<String, Vec<VolumeEntry>>,
    /// `(parameter_name, title)` pairs — one per `<nod>:<quantity>`
    /// from each site's lowest sweep. Sorted for stable output.
    parameters: Vec<(String, String)>,
    /// All distinct volume times across every site, sorted ascending.
    times: Vec<DateTime<Utc>>,
    /// Union of per-site coverage bboxes `[w, s, e, n]` in WGS84.
    spatial_extent: Option<[f64; 4]>,
}

/// WGS84 bounding box `[w, s, e, n]` of the circular coverage area of
/// one site: a circle of radius `radius_m` metres centred on
/// `(lon, lat)`. Computed by projecting the four cardinal extremes on
/// a sphere — the max-latitude reach is due north/south, the
/// max-longitude reach is at the latitude-scaled east/west offset.
fn site_coverage_bbox(lon: f64, lat: f64, radius_m: f64) -> [f64; 4] {
    let dlat = (radius_m / EARTH_RADIUS_M).to_degrees();
    // Longitude span widens toward the poles: divide by cos(lat). At
    // the latitude extremes of the circle the parallel is shortest, so
    // use the larger of the centre and edge latitudes' cos to bound
    // the east/west reach conservatively.
    let lat_for_lon = (lat.abs() + dlat).min(89.9);
    let cos_lat = lat_for_lon.to_radians().cos().max(1e-6);
    let dlon = dlat / cos_lat;
    [
        lon - dlon,
        (lat - dlat).max(-90.0),
        lon + dlon,
        (lat + dlat).min(90.0),
    ]
}

/// Scan `data_dir` for `.h5` polar-volume files, reusing already-parsed
/// volumes from `cache` (keyed by path) so a poll cycle doesn't
/// re-parse unchanged files. Returns the freshly built [`Catalog`].
fn scan_directory(
    collection_id: &str,
    data_dir: &std::path::Path,
    cache: &Mutex<HashMap<PathBuf, Arc<PolarVolume>>>,
    max_files: Option<usize>,
) -> Result<Catalog, EngineError> {
    let read_dir = std::fs::read_dir(data_dir).map_err(|e| {
        EngineError::Storage(DataServerError::Engine(format!(
            "[{collection_id}] failed to read PVOL directory `{}`: {e}",
            data_dir.display()
        )))
    })?;

    let mut by_site: HashMap<String, Vec<VolumeEntry>> = HashMap::new();

    for entry in read_dir.flatten() {
        let path = entry.path();
        let is_h5 = path
            .extension()
            .map(|e| e.eq_ignore_ascii_case("h5"))
            .unwrap_or(false);
        if !is_h5 || !path.is_file() {
            continue;
        }

        // Cache hit — reuse the parsed volume.
        let cached = {
            let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
            guard.get(&path).cloned()
        };
        let volume = match cached {
            Some(v) => v,
            None => {
                let bytes = match std::fs::read(&path) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(
                            "[{collection_id}] skipping PVOL file `{}`: read failed: {e}",
                            path.display()
                        );
                        continue;
                    }
                };
                match read_polar_volume(&bytes) {
                    Ok(v) => {
                        let v = Arc::new(v);
                        cache
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(path.clone(), v.clone());
                        v
                    }
                    Err(e) => {
                        tracing::warn!(
                            "[{collection_id}] skipping PVOL file `{}`: parse failed: {e}",
                            path.display()
                        );
                        continue;
                    }
                }
            }
        };

        let Some(nod) = volume.site.nod.clone() else {
            tracing::warn!(
                "[{collection_id}] skipping PVOL file `{}`: no NOD identifier in /what/source",
                path.display()
            );
            continue;
        };

        by_site
            .entry(nod)
            .or_default()
            .push(VolumeEntry { path, volume });
    }

    // Sort each site's volumes by time ascending, then cap each site
    // to the most-recent `max_files` — an archive directory holding
    // years of data must not load and cache every file.
    for list in by_site.values_mut() {
        list.sort_by_key(|e| e.volume.time);
        if let Some(cap) = max_files {
            if list.len() > cap {
                list.drain(..list.len() - cap);
            }
        }
    }

    // Evict parse-cache entries the catalog no longer references —
    // files deleted from disk, plus volumes aged out of a site's
    // `max_files` window. `kept` is a set, so this is O(cache).
    {
        let kept: std::collections::HashSet<&PathBuf> =
            by_site.values().flatten().map(|e| &e.path).collect();
        let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        guard.retain(|k, _| kept.contains(k));
    }

    // Derive parameters from each site's lowest sweep.
    let mut parameters: Vec<(String, String)> = Vec::new();
    for (nod, list) in &by_site {
        // Use the most recent volume's lowest sweep for the quantity
        // list — quantity sets are stable across a site's volumes.
        let Some(latest) = list.last() else { continue };
        let Some(sweep0) = latest.volume.sweeps.first() else {
            continue;
        };
        let plc = latest.volume.site.plc.as_deref();
        for moment in &sweep0.moments {
            let name = format!("{nod}{SITE_QUANTITY_SEP}{}", moment.quantity);
            let title = match plc {
                Some(place) => format!("{place} — {}", moment.quantity),
                None => name.clone(),
            };
            parameters.push((name, title));
        }
    }
    parameters.sort_by(|a, b| a.0.cmp(&b.0));
    // A producer shipping a sweep with a duplicate quantity name would
    // otherwise advertise the same `<nod>:<quantity>` layer twice.
    parameters.dedup_by(|a, b| a.0 == b.0);

    // Distinct, sorted volume times across all sites.
    let mut times: Vec<DateTime<Utc>> = by_site
        .values()
        .flat_map(|l| l.iter().map(|e| e.volume.time))
        .collect();
    times.sort_unstable();
    times.dedup();

    // Union of per-site coverage bboxes.
    let mut spatial_extent: Option<[f64; 4]> = None;
    for list in by_site.values() {
        let Some(latest) = list.last() else { continue };
        let Some(sweep0) = latest.volume.sweeps.first() else {
            continue;
        };
        let site = &latest.volume.site;
        let radius_m = sweep0.nbins as f64 * sweep0.rscale + sweep0.rstart;
        let bb = site_coverage_bbox(site.lon, site.lat, radius_m);
        spatial_extent = Some(match spatial_extent {
            None => bb,
            Some([w, s, e, n]) => [w.min(bb[0]), s.min(bb[1]), e.max(bb[2]), n.max(bb[3])],
        });
    }

    Ok(Catalog {
        by_site,
        parameters,
        times,
        spatial_extent,
    })
}

// ---------------------------------------------------------------------------
// PolarVolumeEngine
// ---------------------------------------------------------------------------

/// `MapEngine` implementation backed by an ODIM_H5 polar-volume catalog.
///
/// Construct with [`PolarVolumeEngine::new`]; drive metadata freshness
/// with [`PolarVolumeEngine::poll_loop`] and stop it with
/// [`PolarVolumeEngine::shutdown`].
pub struct PolarVolumeEngine {
    collection_id: String,
    /// Directory of `.h5` polar-volume files, re-scanned by the poll loop.
    data_dir: PathBuf,
    /// Per-site cap on retained volumes (`OdimConfig.max_files`) — keeps
    /// an archive directory from loading every file into the catalog.
    max_files: Option<usize>,
    /// Lock-free catalog snapshot for `raster_info()` + `get_raster_tile`.
    catalog: Arc<ArcSwap<Catalog>>,
    /// Multi-entry parse cache keyed by file path — a PVOL network of
    /// ~10 sites at 5-min cadence keeps tens of volumes resident; HDF5
    /// parsing (not the few-MB read) dominates, so caching every parsed
    /// volume keeps both the poll loop and hot tile requests cheap.
    parse_cache: Mutex<HashMap<PathBuf, Arc<PolarVolume>>>,
    poll_interval: Duration,
    shutdown: AtomicBool,
    shutdown_notify: Notify,
}

impl PolarVolumeEngine {
    /// Build a polar-volume engine over the local directory `data_path`.
    /// Performs one synchronous scan so `raster_info()` answers with a
    /// populated parameter list, temporal extent, and spatial extent
    /// immediately. An empty / file-less directory is allowed (the poll
    /// loop will pick files up later) — only an unreadable directory or
    /// a missing `data_path` is a hard error.
    ///
    /// `config` is the shared [`ds_core::config::OdimConfig`]; the
    /// volume engine ignores its `parameter`/`unit` fields (a PVOL
    /// collection is inherently multi-parameter) and uses only
    /// `poll_interval_secs`. S3 fields are not yet supported for PVOL.
    pub fn new(
        collection_id: &str,
        data_path: Option<&str>,
        config: &ds_core::config::OdimConfig,
    ) -> Result<Self, EngineError> {
        if config.endpoint.is_some() || config.bucket.is_some() {
            tracing::warn!(
                "[{collection_id}] `endpoint`/`bucket` are set but the PVOL \
                 (`odim-volume`) engine only supports a local `data_path` \
                 source — the S3 settings are ignored"
            );
        }

        let data_path = data_path.ok_or(EngineError::NoSource)?;
        let data_dir = PathBuf::from(data_path);

        let parse_cache = Mutex::new(HashMap::new());
        let catalog = scan_directory(collection_id, &data_dir, &parse_cache, config.max_files)?;
        if catalog.by_site.is_empty() {
            tracing::warn!(
                "[{collection_id}] no PVOL `.h5` files found in `{}` yet — \
                 the catalog will populate on the next poll",
                data_dir.display()
            );
        } else {
            tracing::info!(
                "[{collection_id}] PVOL catalog: {} site(s), {} parameter(s), {} time(s)",
                catalog.by_site.len(),
                catalog.parameters.len(),
                catalog.times.len()
            );
        }

        Ok(Self {
            collection_id: collection_id.to_string(),
            data_dir,
            max_files: config.max_files,
            catalog: Arc::new(ArcSwap::from_pointee(catalog)),
            parse_cache,
            poll_interval: Duration::from_secs(config.poll_interval_secs.max(1)),
            shutdown: AtomicBool::new(false),
            shutdown_notify: Notify::new(),
        })
    }

    /// Re-scan the directory and atomically swap the catalog. Exits
    /// when [`shutdown`](Self::shutdown) is called. Mirrors
    /// `OdimEngine::poll_loop` — `AtomicBool` flag + `Notify` wake-up.
    pub async fn poll_loop(&self) {
        let mut interval = tokio::time::interval(self.poll_interval);
        interval.tick().await; // skip immediate first tick (scanned at boot)

        loop {
            if self.shutdown.load(Ordering::Acquire) {
                tracing::info!("[{}] PVOL poll loop shutting down", self.collection_id);
                break;
            }
            tokio::select! {
                _ = interval.tick() => {
                    if !self.shutdown.load(Ordering::Acquire) {
                        self.poll_once();
                    }
                }
                _ = self.shutdown_notify.notified() => {}
            }
        }
    }

    /// Signal the poll loop to stop. Idempotent; safe to call before
    /// `poll_loop` starts.
    pub fn shutdown(&self) {
        if !self.shutdown.swap(true, Ordering::Release) {
            self.shutdown_notify.notify_waiters();
        }
    }

    fn poll_once(&self) {
        match scan_directory(
            &self.collection_id,
            &self.data_dir,
            &self.parse_cache,
            self.max_files,
        ) {
            Ok(catalog) => {
                let prev = self.catalog.load();
                let changed = prev.by_site.len() != catalog.by_site.len()
                    || prev.times.last() != catalog.times.last()
                    || prev.parameters.len() != catalog.parameters.len();
                if changed {
                    tracing::info!(
                        "[{}] PVOL catalog refreshed: {} → {} site(s), {} → {} time(s)",
                        self.collection_id,
                        prev.by_site.len(),
                        catalog.by_site.len(),
                        prev.times.len(),
                        catalog.times.len(),
                    );
                }
                self.catalog.store(Arc::new(catalog));
            }
            Err(e) => {
                tracing::warn!("[{}] PVOL catalog refresh failed: {e}", self.collection_id);
            }
        }
    }
}

/// Resample one polar moment of a sweep into a Cartesian output grid.
///
/// `bbox` is `[west, south, east, north]` in WGS84 degrees. For
/// [`OutputCrs::WebMercator`], output row latitudes are interpolated in
/// Mercator-Y so pixels are square in the projection; for
/// [`OutputCrs::Wgs84`] they are linear in latitude. For each output
/// pixel centre the algorithm:
///
/// 1. computes ground distance + azimuth from the site via
///    [`ground_distance_bearing`];
/// 2. maps distance to a range bin (ground range — see below);
/// 3. maps azimuth to a stored ray index;
/// 4. samples the raw moment array and applies gain/offset/nodata.
///
/// **Ground-range interim.** The sweep range axis is treated as ground
/// range: `bin = floor((d - rstart) / rscale)`. A proper slant-range /
/// 4⁄3-Earth ground-range correction is deferred — for the lowest
/// elevation sweep this M2 interim, the near-horizon geometry keeps the
/// slant-vs-ground discrepancy small.
fn polar_sample(
    volume: &PolarVolume,
    quantity: &str,
    bbox: [f64; 4],
    width: u32,
    height: u32,
    output_crs: &OutputCrs,
) -> Result<RasterTile, DataServerError> {
    // Lowest elevation sweep — M1 sorts `sweeps` ascending by elangle.
    let sweep = volume.sweeps.first().ok_or_else(|| {
        DataServerError::Engine(format!(
            "PVOL site `{}` has no elevation sweeps",
            volume.site.nod.as_deref().unwrap_or("?")
        ))
    })?;

    let moment = sweep
        .moments
        .iter()
        .find(|m| m.quantity == quantity)
        .ok_or_else(|| {
            DataServerError::InvalidParameter(format!(
                "quantity `{quantity}` is not present in the lowest sweep of \
                 PVOL site `{}`",
                volume.site.nod.as_deref().unwrap_or("?")
            ))
        })?;

    let [west, south, east, north] = bbox;
    let (site_lon, site_lat) = (volume.site.lon, volume.site.lat);
    let nrays = sweep.nrays;
    let nbins = sweep.nbins;
    if nrays == 0 || nbins == 0 {
        return Err(DataServerError::Engine(
            "PVOL lowest sweep has zero rays or bins".into(),
        ));
    }
    let ray_step_deg = 360.0 / nrays as f64;

    let (merc_y_north, merc_y_south) = if *output_crs == OutputCrs::WebMercator {
        (lat_to_merc_y(north), lat_to_merc_y(south))
    } else {
        (0.0, 0.0)
    };

    let mut values: Vec<Option<f64>> = Vec::with_capacity((width as usize) * (height as usize));
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

            let (dist, az) = ground_distance_bearing(site_lon, site_lat, lon, lat);

            // Range bin. Ground-range interim — see the doc comment;
            // a slant-range / 4⁄3-Earth correction is deferred.
            let bin = ((dist - sweep.rstart) / sweep.rscale).floor() as i64;
            if bin < 0 || bin >= nbins as i64 {
                values.push(None);
                continue;
            }

            // Azimuth ray. ODIM rays are stored north-first, clockwise,
            // already re-sorted into geographic order — `a1gate`
            // records *acquisition* order only and must NOT offset the
            // stored-array index here.
            let ray = (az / ray_step_deg).floor() as usize % nrays;

            // `RawPixels` is indexed [ray, bin].
            values.push(moment.data.sample(
                ray,
                bin as usize,
                moment.gain,
                moment.offset,
                moment.nodata,
                Some(moment.undetect),
            ));
        }
    }

    Ok(RasterTile {
        width,
        height,
        values,
    })
}

/// Split an advertised `<nod>:<quantity>` parameter name into its
/// `(site, quantity)` parts. Returns `None` when the separator is
/// absent or either side is empty.
fn parse_parameter(parameter: &str) -> Option<(&str, &str)> {
    let (site, quantity) = parameter.split_once(SITE_QUANTITY_SEP)?;
    if site.is_empty() || quantity.is_empty() {
        return None;
    }
    Some((site, quantity))
}

impl MapEngine for PolarVolumeEngine {
    fn get_raster_tile(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        output_crs: &OutputCrs,
        parameter: Option<&str>,
    ) -> Result<RasterTile, DataServerError> {
        // A PVOL collection is inherently multi-parameter — the caller
        // must name a `<nod>:<quantity>` layer.
        let parameter = parameter.ok_or_else(|| {
            DataServerError::InvalidParameter(format!(
                "[{}] PVOL collection requires a `<site>{SITE_QUANTITY_SEP}<quantity>` \
                 parameter (e.g. `fianj{SITE_QUANTITY_SEP}DBZH`)",
                self.collection_id
            ))
        })?;
        let (site, quantity) = parse_parameter(parameter).ok_or_else(|| {
            DataServerError::InvalidParameter(format!(
                "[{}] unparseable PVOL parameter `{parameter}` — expected \
                 `<site>{SITE_QUANTITY_SEP}<quantity>`",
                self.collection_id
            ))
        })?;

        let catalog = self.catalog.load();
        let site_volumes = catalog.by_site.get(site).ok_or_else(|| {
            DataServerError::InvalidParameter(format!(
                "[{}] unknown PVOL site `{site}`",
                self.collection_id
            ))
        })?;

        // Select the volume nearest `time` (latest if `None`) — mirrors
        // `OdimEngine::select_entry`.
        let entry = match time {
            Some(target) => site_volumes
                .iter()
                .min_by_key(|e| (e.volume.time - target).num_seconds().abs()),
            None => site_volumes.last(),
        }
        .ok_or_else(|| {
            DataServerError::Engine(format!(
                "[{}] PVOL site `{site}` has no volumes",
                self.collection_id
            ))
        })?;

        polar_sample(&entry.volume, quantity, bbox, width, height, output_crs)
    }

    fn raster_info(&self) -> RasterInfo {
        let catalog = self.catalog.load();
        let parameters = catalog.parameters.clone();
        let parameter = parameters
            .first()
            .map(|(name, _)| name.clone())
            .unwrap_or_default();

        RasterInfo {
            native_crs: "CRS:84".to_string(),
            spatial_extent: catalog.spatial_extent,
            // Most-recent-first, matching the `RasterInfo::times` contract.
            times: catalog.times.iter().rev().copied().collect(),
            parameter,
            // PVOL quantities span multiple physical units (dBZ, m/s,
            // dB, …); the per-layer unit is not a single collection
            // constant, so leave it blank.
            unit: String::new(),
            parameters,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pvol::{PolarMoment, RadarSite, Sweep};
    use crate::reader::RawPixels;
    use ndarray::Array2;

    /// A pixel due north of the site has bearing ≈ 0°.
    #[test]
    fn bearing_due_north_is_zero() {
        let (_d, az) = ground_distance_bearing(25.0, 60.0, 25.0, 60.5);
        // `rem_euclid(360)` keeps `az` in [0, 360), so due-north is a
        // small positive angle — assert that directly.
        assert!(az < 0.01, "due-north bearing ≈ 0°, got {az}");
    }

    /// A pixel ~10 km due east of the site has bearing ≈ 90° and
    /// distance ≈ 10 km. 10 km east at 60°N is ~0.1796° of longitude
    /// (10000 / (EARTH_RADIUS·cos60°·π/180)).
    #[test]
    fn bearing_due_east_ten_km() {
        let lat = 60.0_f64;
        let dlon =
            10_000.0 / (EARTH_RADIUS_M * lat.to_radians().cos()) * 180.0 / std::f64::consts::PI;
        let (d, az) = ground_distance_bearing(25.0, lat, 25.0 + dlon, lat);
        assert!((d - 10_000.0).abs() < 5.0, "distance ≈ 10 km, got {d}");
        assert!((az - 90.0).abs() < 0.2, "due-east bearing ≈ 90°, got {az}");
    }

    /// Distance is symmetric and zero at the site itself.
    #[test]
    fn distance_zero_at_site() {
        let (d, _az) = ground_distance_bearing(25.0, 60.0, 25.0, 60.0);
        assert!(d < 1e-6, "distance at site ≈ 0, got {d}");
    }

    /// Build a synthetic single-site, single-sweep, single-moment
    /// volume whose raw value at `[ray, bin]` encodes the bin index,
    /// so a rendered pixel's sampled value reveals which bin it hit.
    fn synthetic_volume(lon: f64, lat: f64) -> PolarVolume {
        let nrays = 360usize;
        let nbins = 100usize;
        // raw[ray][bin] = bin  → physical = bin*1.0 + 0.0 = bin.
        let mut data = Array2::<u16>::zeros((nrays, nbins));
        for ray in 0..nrays {
            for bin in 0..nbins {
                data[(ray, bin)] = bin as u16;
            }
        }
        let moment = PolarMoment {
            quantity: "DBZH".to_string(),
            gain: 1.0,
            offset: 0.0,
            // 65535 is an unused raw value — nothing masks.
            nodata: 65_535.0,
            undetect: 65_534.0,
            data: RawPixels::U16(data),
        };
        let sweep = Sweep {
            elangle: 0.5,
            nbins,
            nrays,
            rscale: 1_000.0, // 1 km per bin
            rstart: 0.0,
            a1gate: 0,
            moments: vec![moment],
        };
        PolarVolume {
            site: RadarSite {
                lon,
                lat,
                height: 100.0,
                nod: Some("test".to_string()),
                plc: Some("Test Site".to_string()),
                wmo: None,
            },
            time: Utc::now(),
            object: "PVOL".to_string(),
            sweeps: vec![sweep],
        }
    }

    /// Polar→Cartesian: a pixel at a known bearing/range must sample
    /// the bin matching its ground distance. The output bbox is a
    /// small box just east of the site; the rightmost pixels are
    /// farther out, so they sample higher bin indices.
    #[test]
    fn polar_sample_maps_distance_to_bin() {
        let (site_lon, site_lat) = (25.0, 60.0);
        let vol = synthetic_volume(site_lon, site_lat);

        // A box from the site eastward ~50 km. With 1 km bins the
        // sampled value at each pixel equals its ground-distance / 1000.
        let dlon_50km = 50_000.0 / (EARTH_RADIUS_M * site_lat.to_radians().cos()) * 180.0
            / std::f64::consts::PI;
        let bbox = [
            site_lon,
            site_lat - 0.001,
            site_lon + dlon_50km,
            site_lat + 0.001,
        ];
        let tile = polar_sample(&vol, "DBZH", bbox, 50, 1, &OutputCrs::Wgs84).unwrap();

        assert_eq!(tile.values.len(), 50);
        // Leftmost pixel ≈ at the site → bin 0.
        let first = tile.values[0].expect("near-site pixel should sample");
        assert!(first < 2.0, "near-site pixel ≈ bin 0, got {first}");
        // Rightmost pixel ≈ 50 km out → bin ≈ 49.
        let last = tile.values[49].expect("far pixel should sample");
        assert!(
            (45.0..=50.0).contains(&last),
            "far pixel ≈ bin 49, got {last}"
        );
        // Values increase monotonically with eastward distance.
        for w in tile.values.iter().flatten().collect::<Vec<_>>().windows(2) {
            assert!(w[0] <= w[1], "bin index must rise with distance");
        }
    }

    /// A pixel beyond the sweep's maximum range samples `None`.
    #[test]
    fn polar_sample_beyond_range_is_none() {
        let (site_lon, site_lat) = (25.0, 60.0);
        let vol = synthetic_volume(site_lon, site_lat); // 100 bins × 1 km = 100 km

        // 300 km east — well past the 100 km sweep.
        let dlon = 300_000.0 / (EARTH_RADIUS_M * site_lat.to_radians().cos()) * 180.0
            / std::f64::consts::PI;
        let bbox = [
            site_lon + dlon - 0.01,
            site_lat - 0.001,
            site_lon + dlon + 0.01,
            site_lat + 0.001,
        ];
        let tile = polar_sample(&vol, "DBZH", bbox, 4, 1, &OutputCrs::Wgs84).unwrap();
        assert!(
            tile.values.iter().all(Option::is_none),
            "pixels past max range must be None"
        );
    }

    /// An absent quantity is an `InvalidParameter` error, not a panic.
    #[test]
    fn polar_sample_unknown_quantity_errors() {
        let vol = synthetic_volume(25.0, 60.0);
        // `RasterTile` has no `Debug`, so match rather than `unwrap_err`.
        match polar_sample(
            &vol,
            "VRADH",
            [24.0, 59.0, 26.0, 61.0],
            4,
            4,
            &OutputCrs::Wgs84,
        ) {
            Err(DataServerError::InvalidParameter(_)) => {}
            Err(other) => panic!("expected InvalidParameter, got {other:?}"),
            Ok(_) => panic!("expected an error for an absent quantity"),
        }
    }

    /// `parse_parameter` round-trips a `<nod>:<quantity>` name and
    /// rejects malformed inputs.
    #[test]
    fn parse_parameter_splits_and_rejects() {
        assert_eq!(parse_parameter("fianj:DBZH"), Some(("fianj", "DBZH")));
        assert_eq!(parse_parameter("nodbzh"), None);
        assert_eq!(parse_parameter(":DBZH"), None);
        assert_eq!(parse_parameter("fianj:"), None);
    }

    /// `site_coverage_bbox` brackets the centre and widens with radius.
    #[test]
    fn site_coverage_bbox_brackets_centre() {
        let [w, s, e, n] = site_coverage_bbox(25.0, 60.0, 250_000.0);
        assert!(w < 25.0 && e > 25.0 && s < 60.0 && n > 60.0);
        // ~250 km ≈ 2.25° of latitude.
        assert!((n - s - 4.5).abs() < 0.5, "lat span ≈ 4.5°, got {}", n - s);
    }
}
