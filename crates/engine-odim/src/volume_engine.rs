//! `MapEngine` impl backed by an ODIM_H5 polar-volume (PVOL) catalog.
//!
//! Where [`crate::engine::OdimEngine`] serves pre-projected 2-D `COMP`
//! composites, this engine serves **native polar volumes** — multi-
//! elevation, multi-moment radar data in spherical (range × azimuth)
//! coordinates — and resamples them into Cartesian raster tiles on the
//! fly.
//!
//! ## Collection model (model B — per-site collections)
//!
//! A PVOL **source** is a local directory of `.h5` polar-volume files (or
//! an S3/HTTP prefix) spanning multiple radar **sites** and multiple
//! acquisition times. [`PolarVolumeEngine`] owns one source: it scans,
//! parses, caches, and polls — but it is *not* itself an OGC collection.
//!
//! Instead, the loader expands one source into **N per-site collections**
//! (one per ODIM `nod`), each served by a cheap [`PolarVolumeSiteView`]
//! over the engine's shared catalog. A site collection's parameters are
//! **bare quantities** (`DBZH`, `TH`, `VRADH`, `ZDR`, …) — the site is the
//! collection (its EDR location, its spatial/vertical extent), so it has
//! no business in the parameter name. This matches EDR (where the
//! parameter list is the quantity, never `<nod>:<quantity>`) and lets WMS
//! styling key off the bare quantity. WMS layer names are
//! `collection-id/parameter`; with a bare-quantity parameter the token
//! never contains the api-wms-significant `/`.
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
//! A background `poll_loop` re-scans the source every
//! `poll_interval_secs` and atomically swaps the catalog so new volume
//! files appear without an admin reload — mirroring `OdimEngine`.
//!
//! ## Sources
//!
//! A PVOL collection reads its `.h5` files from either a local
//! filesystem directory (`data_path`) or an S3/HTTP object store
//! (`endpoint` + `bucket` + `prefix_pattern`) — the same two-source
//! model as the COMP [`crate::engine::OdimEngine`]. An S3 source streams
//! FMI polar volumes straight out of the open-data bucket; a
//! `time_window` bounds both the date-partitioned prefixes scanned and
//! the timestamps kept.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::feature::{parse_area_coords, parse_linestring_coords, parse_point_coords};
use ds_core::map_engine::{MapEngine, OutputCrs, RasterInfo, RasterTile};
use ds_core::model::{
    CoverageResponse, DomainDescription, Location, NdArray, ParameterDescription, QueryResult,
    VerticalCoord,
};
use ds_core::vertical::{VerticalDimension, VerticalKind};
use tokio::sync::Notify;

use ds_storage::discovery::{expand_prefix_for_dates, expand_prefix_pattern, TimeWindow};

use crate::catalog::MAX_REMOTE_FILE_SIZE;
use crate::engine::EngineError;
use crate::pvol::{read_polar_volume, PolarMoment, PolarVolume, Sweep};

/// Days of date-partitioned prefixes to scan when an S3 source has no
/// `time_window`. Two days covers the just-after-midnight case where
/// the recent tail still straddles yesterday's partition. Mirrors
/// `OdimEngine`'s constant of the same name.
const DEFAULT_SCAN_DAYS: u32 = 2;

/// Mean Earth radius (metres) used by the geodesic helper. A sphere is
/// the right model here: radar ground range is itself a spherical
/// approximation, and the per-pixel error of WGS84-vs-sphere at radar
/// ranges (≤ ~250 km) is far below one output pixel.
const EARTH_RADIUS_M: f64 = 6_371_000.0;

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

/// Forward great-circle destination point: starting at `(lon0, lat0)`
/// (degrees), travel `distance_m` along bearing `bearing_deg` and return
/// the destination `(lon, lat)` (degrees). Used for path resampling
/// along a LINESTRING segment.
pub fn destination_point(lon0: f64, lat0: f64, distance_m: f64, bearing_deg: f64) -> (f64, f64) {
    let ang = distance_m / EARTH_RADIUS_M;
    let lat0_r = lat0.to_radians();
    let lon0_r = lon0.to_radians();
    let brg = bearing_deg.to_radians();
    let sin_lat = lat0_r.sin() * ang.cos() + lat0_r.cos() * ang.sin() * brg.cos();
    let lat = sin_lat.asin();
    let y = brg.sin() * ang.sin() * lat0_r.cos();
    let x = ang.cos() - lat0_r.sin() * sin_lat;
    // Normalise longitude into (−180, 180]: a path starting near ±180°
    // can otherwise return a lon outside that range, which would leak
    // into the CoverageJSON `Section` nodes verbatim.
    let lon = lon0_r + y.atan2(x);
    let lon_deg = (lon.to_degrees() + 540.0).rem_euclid(360.0) - 180.0;
    (lon_deg, lat.to_degrees())
}

// ---------------------------------------------------------------------------
// 4/3-Earth beam geometry (cross-section path)
// ---------------------------------------------------------------------------
//
// The standard radar-meteorology "effective Earth" model: treat the
// atmosphere's average refractivity gradient as if the Earth's radius
// were 4/3 of its true value, then trace beams as straight lines through
// that fictitious sphere. Forward and inverse maps come from Doviak &
// Zrnić (1993), §2.2.3 — used by the cross-section sampler to translate
// between the polar sweep coordinate (slant range, elevation angle) and
// the human-meaningful display coordinate (ground distance from radar,
// height above antenna).
//
// This is *only* used by the new `query_trajectory` cross-section path.
// The legacy `sample_sweep_moment` (used by position/area queries) and
// `polar_sample` (used by Map/WMS) stay on their ground-range interim —
// migrating those is a separate ticket.

/// 4/3 of the mean Earth radius, in metres — the effective radius used
/// to model standard atmospheric refraction.
pub(crate) const FOUR_THIRDS_EARTH_M: f64 = 4.0 / 3.0 * EARTH_RADIUS_M;

/// Forward map: `(slant_range_m, elevation_angle_deg)` → `(ground_distance_m,
/// height_above_antenna_m)` under the 4/3-Earth model.
///
/// `h = sqrt(r² + R'² + 2·r·R'·sin(el)) − R'`
/// `s = R' · atan(r·cos(el) / (r·sin(el) + R'))`
///
/// where `R' = 4/3 · R_earth`. `r` is slant range in metres, `el` is in
/// degrees.
pub(crate) fn slant_to_ground_height(slant_range_m: f64, elangle_deg: f64) -> (f64, f64) {
    let r = slant_range_m;
    let el = elangle_deg.to_radians();
    let rp = FOUR_THIRDS_EARTH_M;
    let h = (r * r + rp * rp + 2.0 * r * rp * el.sin()).sqrt() - rp;
    let s = rp * (r * el.cos() / (r * el.sin() + rp)).atan();
    (s, h)
}

/// Inverse: `(ground_distance_m, height_above_antenna_m)` →
/// `(slant_range_m, elevation_angle_deg)`. Closed-form companion to
/// [`slant_to_ground_height`]; the algebra is in Doviak & Zrnić (1993)
/// §2.2.3 / Rinehart (2004) §3.5.
///
/// Given target point `(s, h)` on the effective-Earth sphere, parametrise
/// the antenna at radius `R'` and the target at radius `R' + h` separated
/// by the central angle `θ = s / R'`. Then by the law of cosines:
///
/// `r² = R'² + (R'+h)² − 2·R'·(R'+h)·cos(θ)`
///
/// and the elevation angle measured from the local horizontal at the
/// antenna is:
///
/// `el = atan2((R'+h)·cos(θ) − R', (R'+h)·sin(θ))`
pub(crate) fn ground_height_to_slant(
    ground_distance_m: f64,
    height_above_antenna_m: f64,
) -> (f64, f64) {
    let s = ground_distance_m;
    let h = height_above_antenna_m;
    let rp = FOUR_THIRDS_EARTH_M;
    let rh = rp + h;
    let theta = s / rp;
    let (sin_th, cos_th) = theta.sin_cos();
    let r = (rp * rp + rh * rh - 2.0 * rp * rh * cos_th).sqrt().max(0.0);
    let el = (rh * cos_th - rp).atan2(rh * sin_th);
    (r, el.to_degrees())
}

// ---------------------------------------------------------------------------
// Source
// ---------------------------------------------------------------------------

/// Where a [`PolarVolumeEngine`] reads PVOL `.h5` files from.
///
/// Mirrors [`crate::engine::OdimEngine`]'s `Source` exactly: a local
/// filesystem directory or an S3/HTTP object store.
#[derive(Clone)]
enum Source {
    /// A local filesystem directory, scanned with `read_dir`.
    Local { data_dir: PathBuf },
    /// An S3/HTTP object store. `prefix_pattern` may carry strftime
    /// codes (e.g. `%Y/%m/%d/fivih/`); it is expanded per UTC date on
    /// every scan so the listing stays current across day boundaries.
    /// `time_window`, when set, bounds both the dates expanded and the
    /// timestamps kept. `endpoint` / `bucket` are retained for
    /// diagnostics so log and error messages can name the store.
    Remote {
        store: ds_storage::DataStore,
        endpoint: String,
        bucket: String,
        prefix_pattern: String,
        time_window: Option<TimeWindow>,
    },
}

/// Stable file-identity for the parse cache.
///
/// The cache must **not** key on `PathBuf`: an S3/HTTP object key is
/// not a filesystem path — keys always use `/` regardless of host OS,
/// so round-tripping a key through `PathBuf` would corrupt it on a
/// platform with a different separator (the deliberate choice made in
/// `catalog::Location`). A plain `String` identity — the path string
/// for local files, the object key for remote ones — sidesteps that.
type FileId = String;

/// An inclusive `(start, end)` UTC time range — a parsed `time_window`
/// resolved against "now".
type TimeRange = (DateTime<Utc>, DateTime<Utc>);

/// Resolve the engine's [`Source`] from config. `endpoint` + `bucket`
/// select an S3 source; their absence falls back to the local
/// `data_path`. Setting exactly one of `endpoint` / `bucket` is a
/// configuration error rather than a silent fallback. Mirrors
/// `OdimEngine`'s `build_source`.
fn build_source(
    collection_id: &str,
    data_path: Option<&str>,
    config: &ds_core::config::OdimConfig,
) -> Result<Source, EngineError> {
    match (config.endpoint.as_deref(), config.bucket.as_deref()) {
        (Some(endpoint), Some(bucket)) => {
            // A missing `prefix_pattern` is almost always a config
            // mistake: it would list the whole bucket on every poll.
            // An explicit empty string is a deliberate flat-bucket
            // opt-in and is allowed.
            let prefix_pattern = config
                .prefix_pattern
                .clone()
                .ok_or(EngineError::MissingPrefixPattern)?;
            let store = ds_storage::build_s3_store_from_parts(endpoint, bucket)?;
            let time_window = match &config.time_window {
                Some(s) => Some(TimeWindow::parse(s)?),
                None => None,
            };
            tracing::info!(
                "[{collection_id}] PVOL S3 source: endpoint={endpoint} bucket={bucket} \
                 prefix='{prefix_pattern}'"
            );
            Ok(Source::Remote {
                store,
                endpoint: endpoint.to_string(),
                bucket: bucket.to_string(),
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

/// Human-readable description of a [`Source`] for log / error
/// messages.
fn source_label(source: &Source) -> String {
    match source {
        Source::Local { data_dir } => data_dir.display().to_string(),
        Source::Remote {
            endpoint,
            bucket,
            prefix_pattern,
            ..
        } => format!("s3 {endpoint}/{bucket}/{prefix_pattern}"),
    }
}

// ---------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------

/// One parsed polar volume in the catalog. The volume is held behind
/// `Arc` so the catalog can be cheaply cloned out of the `ArcSwap`
/// snapshot without re-parsing HDF5.
#[derive(Clone)]
struct VolumeEntry {
    /// Source file identity — the parse-cache key (local path string or
    /// S3 object key); used to evict cache entries the (`max_files`-
    /// capped) catalog no longer references.
    id: FileId,
    /// Parsed volume — `Arc` so repeated requests share one parse.
    volume: Arc<PolarVolume>,
}

/// Per-site derived metadata, computed once per scan in [`derive_catalog`].
///
/// A [`PolarVolumeSiteView`]'s capability accessors (`raster_info`,
/// `get_parameters`, extents) read from this snapshot so they stay O(1)
/// from an `ArcSwap` load (CLAUDE.md hot-path rule) instead of re-deriving
/// from sweeps on every request. Mirrors the union fields on [`Catalog`]
/// but scoped to one radar `nod`.
#[derive(Clone)]
struct SiteMeta {
    /// Antenna longitude (WGS84).
    lon: f64,
    /// Antenna latitude (WGS84).
    lat: f64,
    /// Human place name (ODIM `/what` PLC), if present.
    plc: Option<String>,
    /// Map/WMS layer list — `(bare_quantity, title)` from this site's
    /// lowest sweep. **No `<nod>:` prefix**: under model B the site *is*
    /// the collection, so the parameter is the bare quantity.
    parameters: Vec<(String, String)>,
    /// Bare EDR quantities (lowest sweep), sorted distinct.
    quantities: Vec<String>,
    /// This site's distinct volume times, ascending.
    times: Vec<DateTime<Utc>>,
    /// This site's circular coverage bbox `[w, s, e, n]` (WGS84).
    spatial_extent: Option<[f64; 4]>,
    /// This site's sweep elevation angles (degrees).
    vertical: Option<VerticalDimension>,
}

/// The engine's catalog: per-site time-sorted volume lists, plus the
/// per-site derived metadata each [`PolarVolumeSiteView`] answers from.
///
/// Under model B there is no network-level collection — each radar site is
/// its own collection — so the catalog carries only the per-site index and
/// no union/aggregate metadata.
struct Catalog {
    /// Volumes grouped by `site.nod`, each list sorted by `time`
    /// ascending.
    by_site: HashMap<String, Vec<VolumeEntry>>,
    /// Per-site derived metadata keyed by `nod` — the snapshot each
    /// [`PolarVolumeSiteView`] answers capability queries from. Keys
    /// match `by_site` (a site with no derivable metadata, e.g. every
    /// sweep malformed, is simply absent here).
    by_site_meta: HashMap<String, SiteMeta>,
}

/// Round an elevation angle to 0.1° so near-identical sweep angles from
/// different sites or volumes collapse to one catalogue level.
fn round_elevation(deg: f64) -> f64 {
    let r = (deg * 10.0).round() / 10.0;
    // Normalise -0.0 → +0.0 so downstream dedup/equality treats slightly-
    // negative grazing angles as a single zero level.
    if r == 0.0 {
        0.0
    } else {
        r
    }
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

/// One file the scan should ingest: a stable identity plus a thunk
/// that fetches its raw bytes on a cache miss. Decouples enumeration
/// (local `read_dir` vs S3 `list`) from the shared parse/cache/group
/// logic in [`build_catalog`].
struct PendingFile<'a> {
    /// Cache-key identity — local path string or S3 object key.
    id: FileId,
    /// Fetch the raw HDF5 bytes. Only called on a cache miss.
    fetch: Box<dyn FnOnce() -> Result<Vec<u8>, String> + 'a>,
}

/// Scan `source` for `.h5` polar-volume files and build the catalog,
/// reusing already-parsed volumes from `cache` (keyed by file identity)
/// so a poll cycle doesn't re-download / re-parse unchanged files.
///
/// Blocking — remote scans bridge async object-store I/O internally.
fn scan_source(
    collection_id: &str,
    source: &Source,
    cache: &Mutex<HashMap<FileId, Arc<PolarVolume>>>,
    max_files: Option<usize>,
) -> Result<Catalog, EngineError> {
    match source {
        Source::Local { data_dir } => {
            let pending = enumerate_local(collection_id, data_dir)?;
            let by_site = build_catalog(collection_id, pending, cache);
            Ok(derive_catalog(by_site, cache, max_files))
        }
        Source::Remote {
            store,
            prefix_pattern,
            time_window,
            ..
        } => {
            let (pending, time_filter) =
                enumerate_remote(collection_id, store, prefix_pattern, time_window)?;
            let mut by_site = build_catalog(collection_id, pending, cache);
            // A `time_window` also bounds the timestamps kept: the
            // object listing can include volumes just outside the
            // window (the prefix is a whole UTC day).
            if let Some((start, end)) = time_filter {
                for list in by_site.values_mut() {
                    list.retain(|e| e.volume.time >= start && e.volume.time <= end);
                }
            }
            Ok(derive_catalog(by_site, cache, max_files))
        }
    }
}

/// Enumerate `.h5` files directly in a local directory. Non-recursive.
fn enumerate_local<'a>(
    collection_id: &str,
    data_dir: &'a std::path::Path,
) -> Result<Vec<PendingFile<'a>>, EngineError> {
    let read_dir = std::fs::read_dir(data_dir).map_err(|e| {
        EngineError::Storage(DataServerError::Engine(format!(
            "[{collection_id}] failed to read PVOL directory `{}`: {e}",
            data_dir.display()
        )))
    })?;

    let mut pending = Vec::new();
    for entry in read_dir.flatten() {
        let path = entry.path();
        let is_h5 = path
            .extension()
            .map(|e| e.eq_ignore_ascii_case("h5"))
            .unwrap_or(false);
        if !is_h5 || !path.is_file() {
            continue;
        }
        let id = path.display().to_string();
        pending.push(PendingFile {
            id,
            fetch: Box::new(move || std::fs::read(&path).map_err(|e| format!("read failed: {e}"))),
        });
    }
    Ok(pending)
}

/// Extract an acquisition timestamp from an object key by finding the
/// first run of ≥ 12 consecutive ASCII digits in the basename and
/// parsing its leading 12 as `%Y%m%d%H%M` (UTC).
///
/// Handles both source layouts — FMI `202605150000_fivih_PVOL.h5`
/// (timestamp leads) and DMI `dkste_202512150405.vol.h5` (timestamp
/// follows the station code). Returns `None` when no such digit run
/// exists; the caller then keeps the file and relies on the
/// post-parse `time_window` filter.
fn parse_key_timestamp(key: &str) -> Option<DateTime<Utc>> {
    let basename = key.rsplit('/').next().unwrap_or(key);
    let bytes = basename.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i - start >= 12 {
                return chrono::NaiveDateTime::parse_from_str(
                    &basename[start..start + 12],
                    "%Y%m%d%H%M",
                )
                .ok()
                .map(|t| t.and_utc());
            }
        } else {
            i += 1;
        }
    }
    None
}

/// Enumerate `.h5` objects under an S3/HTTP store's date-expanded
/// prefixes. Returns the pending-file list plus the optional
/// `(start, end)` time filter the window implies.
///
/// A prefix that fails to `list` (e.g. a date partition that doesn't
/// exist yet) is logged and skipped. If *every* prefix fails the call
/// errors rather than silently returning an empty catalog. Mirrors
/// `catalog::scan_remote`'s error tolerance.
#[allow(clippy::type_complexity)]
fn enumerate_remote<'a>(
    collection_id: &str,
    store: &'a ds_storage::DataStore,
    prefix_pattern: &str,
    time_window: &Option<TimeWindow>,
) -> Result<(Vec<PendingFile<'a>>, Option<TimeRange>), EngineError> {
    use ds_storage::object_store::path::Path as ObjectPath;

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

    let mut pending: Vec<PendingFile<'a>> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for prefix in &prefixes {
        let listed = match store.list(&ObjectPath::from(prefix.as_str())) {
            Ok(objects) => objects,
            Err(e) => {
                errors.push(format!("'{prefix}': {e}"));
                continue;
            }
        };
        for obj in listed {
            let key = obj.location.to_string();
            if !key.to_ascii_lowercase().ends_with(".h5") {
                continue;
            }
            // Drop out-of-window objects *before* fetching. A per-day
            // prefix lists a whole day (~288 files at 5-min cadence);
            // without this pre-filter every one would be downloaded and
            // HDF5-parsed just to be trimmed by `filter_catalog_by_time`
            // afterwards — gigabytes of needless transfer. A key whose
            // name carries no parseable timestamp falls through and is
            // caught by the post-parse window filter instead.
            if let Some((start, end)) = time_filter {
                if let Some(ts) = parse_key_timestamp(&key) {
                    if ts < start || ts > end {
                        continue;
                    }
                }
            }
            if obj.size as u64 > MAX_REMOTE_FILE_SIZE {
                tracing::warn!(
                    "[{collection_id}] skipping oversized PVOL object `{key}` ({} bytes)",
                    obj.size
                );
                continue;
            }
            let store_clone = store.clone();
            let key_for_fetch = key.clone();
            pending.push(PendingFile {
                id: key,
                fetch: Box::new(move || {
                    let object = ObjectPath::from(key_for_fetch.as_str());
                    // Re-check size before `get`: `list` already
                    // filtered by size; this `head` closes the
                    // grow-between-list-and-get gap. Mirrors the COMP
                    // engine's `fetch_bytes` head-before-get.
                    let meta = store_clone
                        .head(&object)
                        .map_err(|e| format!("head failed: {e}"))?;
                    if meta.size as u64 > MAX_REMOTE_FILE_SIZE {
                        return Err(format!(
                            "object is {} bytes — exceeds the {MAX_REMOTE_FILE_SIZE}-byte limit",
                            meta.size
                        ));
                    }
                    store_clone
                        .get(&object)
                        .map(|b| b.to_vec())
                        .map_err(|e| format!("get failed: {e}"))
                }),
            });
        }
    }

    if pending.is_empty() && !errors.is_empty() {
        return Err(EngineError::Storage(DataServerError::Engine(format!(
            "[{collection_id}] all {} PVOL S3 prefix scan(s) failed: {}",
            errors.len(),
            errors.join("; ")
        ))));
    }
    if !errors.is_empty() {
        tracing::warn!(
            "[{collection_id}] {} PVOL S3 prefix scan(s) failed (kept {} object(s) from the \
             rest): {}",
            errors.len(),
            pending.len(),
            errors.join("; ")
        );
    }
    Ok((pending, time_filter))
}

/// Parse the enumerated pending files (reusing cached parses) and group
/// the volumes by `site.nod`. A file that fails to fetch or parse is
/// logged and skipped — it does not sink the whole scan. The grouped
/// map is finalised (sorted, capped, metadata-derived) by
/// [`derive_catalog`].
fn build_catalog(
    collection_id: &str,
    pending: Vec<PendingFile<'_>>,
    cache: &Mutex<HashMap<FileId, Arc<PolarVolume>>>,
) -> HashMap<String, Vec<VolumeEntry>> {
    let mut by_site: HashMap<String, Vec<VolumeEntry>> = HashMap::new();

    for file in pending {
        let PendingFile { id, fetch } = file;

        // Cache hit — reuse the parsed volume.
        let cached = {
            let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
            guard.get(&id).cloned()
        };
        let volume = match cached {
            Some(v) => v,
            None => {
                let bytes = match fetch() {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!("[{collection_id}] skipping PVOL file `{id}`: {e}");
                        continue;
                    }
                };
                match read_polar_volume(&bytes) {
                    Ok(v) => {
                        let v = Arc::new(v);
                        cache
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(id.clone(), v.clone());
                        v
                    }
                    Err(e) => {
                        tracing::warn!(
                            "[{collection_id}] skipping PVOL file `{id}`: parse failed: {e}"
                        );
                        continue;
                    }
                }
            }
        };

        let Some(nod) = volume.site.nod.clone() else {
            tracing::warn!(
                "[{collection_id}] skipping PVOL file `{id}`: no NOD identifier in /what/source"
            );
            continue;
        };

        by_site
            .entry(nod)
            .or_default()
            .push(VolumeEntry { id, volume });
    }

    by_site
}

/// Finalise the grouped volume map into a [`Catalog`]: sort and cap each
/// site's list, evict stale parse-cache entries, and derive per-site
/// metadata. Under model B there is no aggregate/union metadata — each
/// radar site is its own collection, served by a [`PolarVolumeSiteView`].
///
/// `max_files` caps each site to its most-recent N volumes — an archive
/// directory (or a wide S3 `time_window`) must not load and cache every
/// file.
fn derive_catalog(
    mut by_site: HashMap<String, Vec<VolumeEntry>>,
    cache: &Mutex<HashMap<FileId, Arc<PolarVolume>>>,
    max_files: Option<usize>,
) -> Catalog {
    // Sort each site's volumes by time ascending, then cap to the
    // most-recent `max_files`.
    for list in by_site.values_mut() {
        list.sort_by_key(|e| e.volume.time);
        if let Some(cap) = max_files {
            if list.len() > cap {
                list.drain(..list.len() - cap);
            }
        }
    }
    // An S3 `time_window` filter can empty a site's list entirely.
    by_site.retain(|_, list| !list.is_empty());

    // Evict parse-cache entries the catalog no longer references —
    // files dropped from the source, plus volumes aged out of a site's
    // `max_files` window. `kept` is a set, so this is O(cache).
    {
        let kept: std::collections::HashSet<&FileId> =
            by_site.values().flatten().map(|e| &e.id).collect();
        let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        guard.retain(|k, _| kept.contains(k));
    }

    // Per-site metadata snapshots (model B): one `SiteMeta` per site so
    // each `PolarVolumeSiteView` answers capability queries scoped to its
    // own radar without re-deriving from sweeps per request.
    let by_site_meta: HashMap<String, SiteMeta> = by_site
        .iter()
        .filter_map(|(nod, list)| Some((nod.clone(), derive_site_meta(list)?)))
        .collect();

    Catalog {
        by_site,
        by_site_meta,
    }
}

/// Derive one site's [`SiteMeta`] from its time-sorted volume list.
///
/// Returns `None` only for a site whose most-recent volume has no usable
/// lowest sweep (an entirely malformed file) — such a site is dropped
/// from `by_site_meta` and its view reports empty metadata.
fn derive_site_meta(list: &[VolumeEntry]) -> Option<SiteMeta> {
    let latest = list.last()?;
    let sweep0 = latest.volume.sweeps.first()?;
    let site = &latest.volume.site;

    // Map/WMS + EDR quantity set, from the lowest sweep — matching the
    // global derivation, but bare (no `<nod>:` prefix). Title is just the
    // quantity: the per-site collection already carries the place name.
    let mut quantities: Vec<String> = sweep0.moments.iter().map(|m| m.quantity.clone()).collect();
    quantities.sort();
    quantities.dedup();
    let parameters: Vec<(String, String)> =
        quantities.iter().map(|q| (q.clone(), q.clone())).collect();

    // Coverage radius from the lowest sweep's range gate geometry.
    let radius_m = sweep0.nbins as f64 * sweep0.rscale + sweep0.rstart;
    let spatial_extent = (radius_m.is_finite() && radius_m > 0.0)
        .then(|| site_coverage_bbox(site.lon, site.lat, radius_m));

    let mut times: Vec<DateTime<Utc>> = list.iter().map(|e| e.volume.time).collect();
    times.sort_unstable();
    times.dedup();

    // Elevation-angle axis from the most-recent volume's sweeps, rounded
    // and deduped — see the matching union derivation above.
    let mut levels: Vec<f64> = latest
        .volume
        .sweeps
        .iter()
        .map(|s| round_elevation(s.elangle))
        .collect();
    levels.retain(|v| v.is_finite());
    levels.sort_by(f64::total_cmp);
    levels.dedup();
    let vertical =
        (!levels.is_empty()).then(|| VerticalDimension::new(VerticalKind::ElevationAngle, levels));

    Some(SiteMeta {
        lon: site.lon,
        lat: site.lat,
        plc: site.plc.clone(),
        parameters,
        quantities,
        times,
        spatial_extent,
        vertical,
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
    /// Local directory or S3/HTTP object store, re-scanned by the poll loop.
    source: Source,
    /// Per-site cap on retained volumes (`OdimConfig.max_files`) — keeps
    /// an archive source from loading every file into the catalog.
    max_files: Option<usize>,
    /// Lock-free catalog snapshot for `raster_info()` + `get_raster_tile`.
    catalog: Arc<ArcSwap<Catalog>>,
    /// Multi-entry parse cache keyed by file identity (local path
    /// string or S3 object key — see [`FileId`]). A PVOL network of
    /// ~10 sites at 5-min cadence keeps tens of volumes resident; HDF5
    /// parsing (and, for S3, the multi-MB download) dominates, so
    /// caching every parsed volume keeps both the poll loop and hot
    /// tile requests cheap. The key is a `String`, not a `PathBuf`,
    /// because S3 object keys are not filesystem paths.
    ///
    /// Behind an `Arc` so a `poll_once` scan can be moved onto a
    /// `spawn_blocking` task without borrowing `self`.
    parse_cache: Arc<Mutex<HashMap<FileId, Arc<PolarVolume>>>>,
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
    /// The source is a local directory (`data_path`) or an S3/HTTP
    /// bucket (`endpoint` + `bucket` + `prefix_pattern`) — mirroring the
    /// COMP [`crate::engine::OdimEngine`].
    ///
    /// `config` is the shared [`ds_core::config::OdimConfig`]; the
    /// volume engine ignores its `parameter`/`unit` fields (a PVOL
    /// collection is inherently multi-parameter) and uses
    /// `poll_interval_secs` plus the S3 source fields.
    pub fn new(
        collection_id: &str,
        data_path: Option<&str>,
        config: &ds_core::config::OdimConfig,
    ) -> Result<Self, EngineError> {
        // `time_window` only constrains S3 prefix expansion + timestamp
        // filtering. A local `data_path` source ignores it — warn so a
        // misplaced setting doesn't silently do nothing.
        if config.time_window.is_some() && config.endpoint.is_none() {
            tracing::warn!(
                "[{collection_id}] `time_window` is set but has no effect on a \
                 local `data_path` PVOL source — it only applies to S3 sources"
            );
        }

        let source = build_source(collection_id, data_path, config)?;

        let parse_cache = Arc::new(Mutex::new(HashMap::new()));
        let catalog = scan_source(collection_id, &source, &parse_cache, config.max_files)?;
        if catalog.by_site.is_empty() {
            tracing::warn!(
                "[{collection_id}] no PVOL `.h5` files found at `{}` yet — \
                 the catalog will populate on the next poll",
                source_label(&source)
            );
        } else {
            tracing::info!(
                "[{collection_id}] PVOL catalog: {} site(s), {} volume(s)",
                catalog.by_site.len(),
                catalog.by_site.values().map(|l| l.len()).sum::<usize>(),
            );
        }

        Ok(Self {
            collection_id: collection_id.to_string(),
            source,
            max_files: config.max_files,
            catalog: Arc::new(ArcSwap::from_pointee(catalog)),
            parse_cache,
            poll_interval: Duration::from_secs(config.poll_interval_secs.max(1)),
            shutdown: AtomicBool::new(false),
            shutdown_notify: Notify::new(),
        })
    }

    /// Re-scan the source and atomically swap the catalog. Exits
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
                        self.poll_once().await;
                    }
                }
                _ = self.shutdown_notify.notified() => {}
            }
        }
    }

    /// Build an engine over a pre-constructed object store, bypassing
    /// `endpoint`/`bucket` parsing.
    ///
    /// **Test-only.** Exists so the integration suite can exercise the
    /// full remote scan path (`list` → `get` → parse → group-by-site)
    /// against a `DataStore` backed by `object_store`'s
    /// `LocalFileSystem` — the same trick PR #182 used for the COMP
    /// engine — without a live S3 endpoint. `prefix_pattern` is used
    /// verbatim (pass `""` to scan the store root); no `time_window`.
    #[doc(hidden)]
    pub fn new_remote_for_test(
        collection_id: &str,
        store: ds_storage::DataStore,
        prefix_pattern: &str,
    ) -> Result<Self, EngineError> {
        let source = Source::Remote {
            store,
            endpoint: "test://local".to_string(),
            bucket: "test".to_string(),
            prefix_pattern: prefix_pattern.to_string(),
            time_window: None,
        };
        let parse_cache = Arc::new(Mutex::new(HashMap::new()));
        let catalog = scan_source(collection_id, &source, &parse_cache, None)?;
        Ok(Self {
            collection_id: collection_id.to_string(),
            source,
            max_files: None,
            catalog: Arc::new(ArcSwap::from_pointee(catalog)),
            parse_cache,
            poll_interval: Duration::from_secs(30),
            shutdown: AtomicBool::new(false),
            shutdown_notify: Notify::new(),
        })
    }

    /// Signal the poll loop to stop. Idempotent; safe to call before
    /// `poll_loop` starts.
    pub fn shutdown(&self) {
        if !self.shutdown.swap(true, Ordering::Release) {
            self.shutdown_notify.notify_waiters();
        }
    }

    /// Re-scan the source and atomically swap the catalog.
    ///
    /// The scan — directory walk or S3 `list` + multi-MB `get`s plus
    /// HDF5 parsing — is blocking, so it runs on `spawn_blocking` rather
    /// than stalling a Tokio worker. Mirrors `OdimEngine::poll_once`.
    async fn poll_once(&self) {
        let collection_id = self.collection_id.clone();
        let source = self.source.clone();
        let cache = Arc::clone(&self.parse_cache);
        let max_files = self.max_files;
        let scan_result = tokio::task::spawn_blocking(move || {
            scan_source(&collection_id, &source, &cache, max_files)
        })
        .await;
        match scan_result {
            Ok(Ok(catalog)) => {
                let prev = self.catalog.load();
                // Aggregate signals across sites for change detection: site
                // count, newest volume time, and total volume count.
                let latest = |c: &Catalog| {
                    c.by_site_meta
                        .values()
                        .filter_map(|m| m.times.last().copied())
                        .max()
                };
                let total_volumes =
                    |c: &Catalog| c.by_site.values().map(|l| l.len()).sum::<usize>();
                let changed = prev.by_site.len() != catalog.by_site.len()
                    || latest(&prev) != latest(&catalog)
                    || total_volumes(&prev) != total_volumes(&catalog);
                if changed {
                    tracing::info!(
                        "[{}] PVOL catalog refreshed: {} → {} site(s), {} → {} volume(s)",
                        self.collection_id,
                        prev.by_site.len(),
                        catalog.by_site.len(),
                        total_volumes(&prev),
                        total_volumes(&catalog),
                    );
                }
                self.catalog.store(Arc::new(catalog));
            }
            Ok(Err(e)) => {
                tracing::warn!("[{}] PVOL catalog refresh failed: {e}", self.collection_id);
            }
            Err(join_err) => {
                tracing::error!(
                    "[{}] PVOL catalog refresh task panicked: {join_err}",
                    self.collection_id
                );
            }
        }
    }
}

/// Sample one moment of a sweep at a WGS84 `(lon, lat)`.
///
/// Computes ground distance + azimuth from the site, maps them to a
/// range bin and stored ray, and reads the raw array with gain/offset/
/// nodata applied. `None` when the point is out of range or the bin is
/// `nodata`/`undetect`. **Ground-range interim** — see [`polar_sample`].
fn sample_sweep_moment(
    sweep: &Sweep,
    moment: &PolarMoment,
    site_lon: f64,
    site_lat: f64,
    lon: f64,
    lat: f64,
) -> Option<f64> {
    if sweep.nrays == 0 || sweep.nbins == 0 {
        return None;
    }
    // Same malformed-`rscale` guard as `sample_polar_slant` /
    // `sample_sweep_moment_bilinear`: a `rscale = 0` / NaN makes the bin
    // `NaN`, and `NaN as i64 == 0` (saturating cast) would pass the bounds
    // check and return bin 0 for every requested point — silent fabricated
    // data on every EDR position/area/profile query against a corrupted
    // file. A negative `rscale` with `dist < rstart` likewise lands in
    // range and samples the wrong gate.
    if !sweep.rscale.is_finite() || sweep.rscale <= 0.0 {
        return None;
    }
    let (dist, az) = ground_distance_bearing(site_lon, site_lat, lon, lat);

    // Range bin. Ground-range interim — a slant-range / 4⁄3-Earth
    // correction is deferred.
    let bin = ((dist - sweep.rstart) / sweep.rscale).floor() as i64;
    if bin < 0 || bin >= sweep.nbins as i64 {
        return None;
    }

    // Azimuth ray. ODIM rays are stored north-first, clockwise, already
    // re-sorted into geographic order — `a1gate` records *acquisition*
    // order only and must NOT offset the stored-array index here.
    let ray = (az / (360.0 / sweep.nrays as f64)).floor() as usize % sweep.nrays;

    // `RawPixels` is indexed [ray, bin].
    moment.data.sample(
        ray,
        bin as usize,
        moment.gain,
        moment.offset,
        moment.nodata,
        Some(moment.undetect),
    )
}

/// Bilinearly read one moment at a *fractional* `(ray, bin)` position —
/// the shared core of the anti-spoke sampler. Blends the four
/// surrounding cells in the moment's stored physical units, wrapping the
/// azimuth axis at the 0°/360° ray seam (ray `nrays` ≡ ray 0) and
/// dropping any `nodata`/`undetect` or out-of-range neighbour while
/// renormalising the weights over the cells that remain — so a masked
/// cell never darkens valid output and valid data never bleeds more
/// than one cell into a gap. `None` when every contributing neighbour
/// is masked or out of range.
///
/// Interpolation is in the stored units (e.g. dBZ): the standard
/// cosmetic choice for a display product. It slightly under-weights
/// peaks versus a linear-reflectivity average, but avoids per-moment
/// unit-aware conversion for what is a smoothing pass.
fn bilinear_cell(
    moment: &PolarMoment,
    nrays: usize,
    nbins: usize,
    ray_f: f64,
    bin_f: f64,
) -> Option<f64> {
    let b0 = bin_f.floor() as i64;
    let r0 = ray_f.floor() as i64;
    let bf = bin_f - b0 as f64;
    let rf = ray_f - r0 as f64;

    let mut sum = 0.0;
    let mut wsum = 0.0;
    for (dr, wr) in [(0i64, 1.0 - rf), (1, rf)] {
        if wr <= 0.0 {
            continue;
        }
        // Azimuth wraps at the seam; `rem_euclid` keeps `nrays-1 → 0`.
        let ray = (r0 + dr).rem_euclid(nrays as i64) as usize;
        for (db, wb) in [(0i64, 1.0 - bf), (1, bf)] {
            let w = wr * wb;
            if w <= 0.0 {
                continue;
            }
            let bin = b0 + db;
            if bin < 0 || bin >= nbins as i64 {
                continue; // range edge — drop this neighbour
            }
            if let Some(v) = moment.data.sample(
                ray,
                bin as usize,
                moment.gain,
                moment.offset,
                moment.nodata,
                Some(moment.undetect),
            ) {
                sum += v * w;
                wsum += w;
            }
        }
    }
    (wsum > 0.0).then(|| sum / wsum)
}

/// Bilinear (anti-spoke) variant of [`sample_sweep_moment`] used by the
/// Cartesian render. Nearest-neighbour azimuth sampling leaves visible
/// radial spokes far from the radar, where adjacent output pixels
/// straddle a ray boundary (#186); blending between the straddling rays
/// closes them. Same **ground-range interim** as [`sample_sweep_moment`].
fn sample_sweep_moment_bilinear(
    sweep: &Sweep,
    moment: &PolarMoment,
    site_lon: f64,
    site_lat: f64,
    lon: f64,
    lat: f64,
) -> Option<f64> {
    if sweep.nrays == 0 || sweep.nbins == 0 {
        return None;
    }
    // Same defensive `rscale` guard as `sample_polar_slant`: a malformed
    // `rscale = 0` makes `bin_f` NaN, and a negative `rscale` with
    // `dist < rstart` yields a positive `bin_f` that can land in range and
    // sample the wrong gate. ODIM_H5 guarantees `rscale > 0`; this keeps a
    // corrupted file from surfacing fabricated values.
    if !sweep.rscale.is_finite() || sweep.rscale <= 0.0 {
        return None;
    }
    let (dist, az) = ground_distance_bearing(site_lon, site_lat, lon, lat);

    // Fractional bin (ground-range interim). Bin `i` starts at
    // `rstart + i*rscale`, matching the nearest-neighbour `floor` mapping;
    // the fractional part blends toward the next bin.
    let bin_f = (dist - sweep.rstart) / sweep.rscale;
    if bin_f < 0.0 || bin_f >= sweep.nbins as f64 {
        return None;
    }
    // Fractional ray. ODIM rays are stored north-first, clockwise (the
    // `a1gate` acquisition offset must NOT shift the stored index); ray
    // `i` starts at azimuth `i * 360/nrays`.
    let ray_f = az / (360.0 / sweep.nrays as f64);

    bilinear_cell(moment, sweep.nrays, sweep.nbins, ray_f, bin_f)
}

/// The sweep whose elevation angle is nearest `target` degrees. `None`
/// only when the volume has no sweeps.
fn nearest_sweep(volume: &PolarVolume, target: f64) -> Option<&Sweep> {
    volume.sweeps.iter().min_by(|a, b| {
        (a.elangle - target)
            .abs()
            .total_cmp(&(b.elangle - target).abs())
    })
}

/// Resample one polar moment of a sweep into a Cartesian output grid.
///
/// `bbox` is `[west, south, east, north]` in WGS84 degrees. Each output pixel's
/// WGS84 lon/lat comes from [`OutputCrs::project_node`], so the output axes
/// follow the requested CRS: linear lon/lat for `Wgs84`, equal-Mercator-Y rows
/// for `WebMercator`, or linear-in-projected-metres for a `Projected` CRS
/// (EPSG:3067/3035). For each output pixel centre the algorithm:
///
/// 1. computes ground distance + azimuth from the site via
///    [`ground_distance_bearing`];
/// 2. maps distance to a fractional range bin (ground range — see below);
/// 3. maps azimuth to a fractional ray index;
/// 4. **bilinearly** samples the four surrounding moment cells
///    ([`sample_sweep_moment_bilinear`]), blending across rays to close
///    the radial spoke gaps that nearest-neighbour sampling leaves far
///    from the radar (#186).
///
/// **Ground-range interim.** The sweep range axis is treated as ground
/// range: `bin = (d - rstart) / rscale`. A proper slant-range /
/// 4⁄3-Earth ground-range correction is deferred — for the lowest
/// elevation sweep this M2 interim, the near-horizon geometry keeps the
/// slant-vs-ground discrepancy small.
///
/// `z` selects the elevation sweep: `Some(angle)` renders the sweep
/// nearest that angle, `None` renders the lowest sweep.
fn polar_sample(
    volume: &PolarVolume,
    quantity: &str,
    bbox: [f64; 4],
    width: u32,
    height: u32,
    output_crs: &OutputCrs,
    z: Option<f64>,
) -> Result<RasterTile, DataServerError> {
    // M1 sorts `sweeps` ascending by elangle, so `first()` is the lowest.
    let sweep = match z {
        Some(target) => nearest_sweep(volume, target),
        None => volume.sweeps.first(),
    }
    .ok_or_else(|| {
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

    let (site_lon, site_lat) = (volume.site.lon, volume.site.lat);
    if sweep.nrays == 0 || sweep.nbins == 0 {
        return Err(DataServerError::Engine(
            "PVOL lowest sweep has zero rays or bins".into(),
        ));
    }

    // Polar sampling is inherently per-pixel (each output pixel resolves to a
    // ground distance + bearing from the site), so unlike the gridded engines
    // this loop maps each pixel's WGS84 lon/lat with the shared
    // `OutputCrs::project_node` directly — covering linear lon/lat, Mercator Y,
    // and projected output CRSs (EPSG:3067/3035) in one place (#160).
    //
    // For `OutputCrs::Projected` this adds one `Crs::inverse` per pixel on top of
    // the inherent per-pixel polar geometry. A coarse-grid map (as the gridded
    // engines use) doesn't drop in cleanly here because the polar sampler is not
    // a smooth source-pixel function; tracked in #268 if it proves to matter.
    let mut values: Vec<Option<f64>> = Vec::with_capacity((width as usize) * (height as usize));
    for oy in 0..height {
        let frac_y = (oy as f64 + 0.5) / height as f64;
        for ox in 0..width {
            let frac_x = (ox as f64 + 0.5) / width as f64;
            let (lon, lat) = output_crs.project_node(bbox, frac_x, frac_y);
            // An out-of-domain projected output pixel arrives as NaN (OutputCrs::
            // Projected inverse failure). NaN would propagate through
            // ground_distance_bearing and saturate to (ray=0, bin=0), returning
            // real radar data at the sweep origin instead of None (transparent).
            if !lon.is_finite() || !lat.is_finite() {
                values.push(None);
                continue;
            }

            values.push(sample_sweep_moment_bilinear(
                sweep, moment, site_lon, site_lat, lon, lat,
            ));
        }
    }

    Ok(RasterTile {
        width,
        height,
        values,
    })
}

// ---------------------------------------------------------------------------
// Per-site EDR/Map helpers (shared by PolarVolumeSiteView)
// ---------------------------------------------------------------------------

/// `ParameterDescription` for an ODIM quantity. ODIM moment groups
/// carry no unit attribute and quantities span several physical units,
/// so the unit is left blank (matching `raster_info`); the label
/// mirrors the `EdrEngine` default-description convention.
fn quantity_description(quantity: &str) -> ParameterDescription {
    ParameterDescription {
        label: quantity.replace('_', " "),
        unit: String::new(),
        observed_property: quantity.to_string(),
    }
}

/// Resolve the quantity set for a PVOL site query: the union of every
/// sweep's moments in the most recent selected volume — a profile
/// samples all sweeps, and a quantity may be present only on higher
/// elevations (e.g. dual-PRF `VRADH`/`WRADH`) — intersected with the
/// optional `parameters` filter.
fn resolve_quantities(
    selected: &[&VolumeEntry],
    parameters: Option<&[String]>,
) -> Result<Vec<String>, DataServerError> {
    let mut available: Vec<String> = selected
        .last()
        .map(|e| {
            e.volume
                .sweeps
                .iter()
                .flat_map(|s| s.moments.iter().map(|m| m.quantity.clone()))
                .collect()
        })
        .unwrap_or_default();
    available.sort();
    available.dedup();
    let quantities: Vec<String> = match parameters {
        Some(req) => available
            .into_iter()
            .filter(|q| req.iter().any(|p| p == q))
            .collect(),
        None => available,
    };
    if quantities.is_empty() {
        return Err(DataServerError::InvalidParameter(
            "No requested parameter is available at this PVOL site".into(),
        ));
    }
    Ok(quantities)
}

/// Snap requested elevation angles to the catalog's canonical sweep-angle
/// set, dropping duplicates while preserving request order.
fn snap_levels(requested: &[f64], canonical: &[f64]) -> Vec<f64> {
    let mut out: Vec<f64> = Vec::new();
    for &want in requested {
        if let Some(&lvl) = canonical
            .iter()
            .min_by(|a, b| (**a - want).abs().total_cmp(&(**b - want).abs()))
        {
            if !out.contains(&lvl) {
                out.push(lvl);
            }
        }
    }
    out
}

/// One `VerticalProfile` coverage: every elevation sweep of `entry`'s
/// volume sampled at WGS84 `(lon, lat)`, with the sweep angles as the
/// `z` axis.
fn volume_profile(
    entry: &VolumeEntry,
    lon: f64,
    lat: f64,
    quantities: &[String],
) -> Option<QueryResult> {
    // A radar may run split cuts — two sweeps at the same nominal
    // elevation angle (e.g. separate surveillance and Doppler scans),
    // so the raw per-sweep angles can repeat (FMI volumes carry two
    // sweeps at 2.0°). CoverageJSON axis values must be unique
    // (`uniqueItems`), so collapse to distinct angles — matching the
    // catalog's deduped vertical extent — and sample each quantity from
    // whichever sweep at that angle carries it.
    let mut levels: Vec<f64> = entry
        .volume
        .sweeps
        .iter()
        .map(|s| round_elevation(s.elangle))
        .collect();
    // Drop non-finite angles (a malformed file with a NaN elangle) before
    // they reach the z axis — a NaN there serialises to JSON `null` and
    // breaks the numeric axis. `round_elevation` normalises -0.0 to +0.0,
    // so plain `dedup` after `sort_by(total_cmp)` collapses every duplicate.
    levels.retain(|v| v.is_finite());
    levels.sort_by(f64::total_cmp);
    levels.dedup();
    // No usable sweep (a severely malformed volume where every elangle was
    // non-finite) — skip this coverage rather than emit a `VerticalProfile`
    // with an empty z axis, which the CoverageJSON schema rejects
    // (`numericValuesAxis.values` has `minItems: 1`).
    if levels.is_empty() {
        return None;
    }
    let (site_lon, site_lat) = (entry.volume.site.lon, entry.volume.site.lat);

    let mut ranges = HashMap::new();
    let mut param_descs = HashMap::new();
    for quantity in quantities {
        let values: Vec<Option<f64>> = levels
            .iter()
            .map(|&level| {
                // For a split cut (two sweeps at the same nominal angle, e.g.
                // a surveillance and a Doppler cut), the *first* sweep that
                // carries the quantity is authoritative — its sample stands
                // even when nodata, so a genuine no-echo is not silently
                // replaced by the sibling's measurement.
                entry
                    .volume
                    .sweeps
                    .iter()
                    .find(|s| {
                        round_elevation(s.elangle) == level
                            && s.moments.iter().any(|m| m.quantity == *quantity)
                    })
                    .and_then(|sweep| {
                        let moment = sweep.moments.iter().find(|m| m.quantity == *quantity)?;
                        sample_sweep_moment(sweep, moment, site_lon, site_lat, lon, lat)
                    })
            })
            .collect();
        ranges.insert(
            quantity.clone(),
            NdArray {
                shape: vec![levels.len()],
                axis_names: vec!["z".to_string()],
                values,
            },
        );
        param_descs.insert(quantity.clone(), quantity_description(quantity));
    }

    Some(QueryResult {
        domain: DomainDescription::VerticalProfile {
            x: lon,
            y: lat,
            t: Some(entry.volume.time),
            z: VerticalCoord {
                kind: VerticalKind::ElevationAngle,
                values: levels,
            },
        },
        parameters: param_descs,
        ranges,
    })
}

/// One `PointSeries` coverage pinned to elevation angle `level`: the
/// sweep nearest `level` in each selected volume, sampled at `(lon, lat)`.
fn level_series(
    selected: &[&VolumeEntry],
    lon: f64,
    lat: f64,
    level: f64,
    quantities: &[String],
    times: &[DateTime<Utc>],
) -> QueryResult {
    let mut ranges = HashMap::new();
    let mut param_descs = HashMap::new();
    for quantity in quantities {
        let values: Vec<Option<f64>> = selected
            .iter()
            .map(|e| {
                let sweep = nearest_sweep(&e.volume, level)?;
                let moment = sweep.moments.iter().find(|m| &m.quantity == quantity)?;
                sample_sweep_moment(
                    sweep,
                    moment,
                    e.volume.site.lon,
                    e.volume.site.lat,
                    lon,
                    lat,
                )
            })
            .collect();
        ranges.insert(
            quantity.clone(),
            NdArray {
                shape: vec![times.len()],
                axis_names: vec!["t".to_string()],
                values,
            },
        );
        param_descs.insert(quantity.clone(), quantity_description(quantity));
    }

    QueryResult {
        domain: DomainDescription::PointSeries {
            x: lon,
            y: lat,
            t: times.to_vec(),
            z: Some(VerticalCoord {
                kind: VerticalKind::ElevationAngle,
                values: vec![level],
            }),
        },
        parameters: param_descs,
        ranges,
    }
}

// ---------------------------------------------------------------------------
// Trajectory cross-section (`Section` domain)
// ---------------------------------------------------------------------------

/// Cross-section sampling parameters.
const TRAJECTORY_NODE_SPACING_M: f64 = 500.0;
/// Hard cap on along-path nodes — protects against a 1000-km path
/// allocating ~2000 columns × hundreds of z levels.
const TRAJECTORY_MAX_NODES: usize = 1024;
/// Default vertical step (metres) when `z` is absent.
const TRAJECTORY_DEFAULT_Z_STEP_M: f64 = 250.0;
/// Hard cap on z levels.
const TRAJECTORY_MAX_Z_LEVELS: usize = 100;

/// Resample a polyline `vertices` (WGS84 `(lon, lat)`) into evenly-spaced
/// nodes along the great-circle path, ~`TRAJECTORY_NODE_SPACING_M` apart,
/// with the endpoints preserved and a `TRAJECTORY_MAX_NODES` cap.
///
/// Returns an empty vec only when the polyline has < 2 vertices — the
/// caller has already validated that via `parse_linestring_coords`.
fn resample_path(vertices: &[(f64, f64)]) -> Vec<(f64, f64)> {
    if vertices.len() < 2 {
        return Vec::new();
    }
    // Per-segment ground length + bearing — cheap (one haversine call per
    // segment), cached so the sampling loop does not pay it per node.
    let mut seg_len = Vec::with_capacity(vertices.len() - 1);
    let mut seg_brg = Vec::with_capacity(vertices.len() - 1);
    let mut total = 0.0_f64;
    for w in vertices.windows(2) {
        let (lon0, lat0) = w[0];
        let (lon1, lat1) = w[1];
        let (d, b) = ground_distance_bearing(lon0, lat0, lon1, lat1);
        seg_len.push(d);
        seg_brg.push(b);
        total += d;
    }
    if total <= 0.0 {
        // Degenerate polyline (all vertices coincide). `parse_linestring_coords`
        // already rejects an all-identical LINESTRING with a 400, so this is
        // only reachable via a direct call — return a single node so the
        // caller's `path.len() < 2` guard turns it into an error rather than
        // emitting a Section with two identical (and schema-invalid)
        // composite-axis nodes.
        return vec![vertices[0]];
    }
    let n =
        ((total / TRAJECTORY_NODE_SPACING_M).ceil() as usize + 1).clamp(2, TRAJECTORY_MAX_NODES);
    let step = total / (n - 1) as f64;

    let mut out = Vec::with_capacity(n);
    out.push(vertices[0]);
    // Walk the path: track cumulative distance into the current segment.
    let mut seg = 0usize;
    let mut into = 0.0_f64;
    for i in 1..(n - 1) {
        let target = i as f64 * step;
        // Advance through segments until `target` falls inside the
        // current one. `total > 0` and `target < total` guarantees
        // termination without overflow.
        while seg + 1 < seg_len.len() && into + seg_len[seg] < target {
            into += seg_len[seg];
            seg += 1;
        }
        let remaining = target - into;
        let (start_lon, start_lat) = vertices[seg];
        out.push(destination_point(
            start_lon,
            start_lat,
            remaining,
            seg_brg[seg],
        ));
    }
    out.push(*vertices.last().unwrap());
    out
}

/// The elevation-angle window `[lo, hi]` (degrees) for a cross-section.
///
/// `z` carries the requested elevation angles (resolved by the API layer
/// against the collection's advertised extent — an interval `0.3/15`
/// arrives as the angles in range). `None`/empty selects the volume's
/// full sweep span. The window is clamped to the volume's actual sweep
/// range so a heterogeneous fleet (a site missing an advertised angle)
/// degrades to nodata rather than fabricating data.
///
/// Errors:
/// - `Engine` when the volume has no finite sweeps (a malformed file).
/// - `InvalidParameter` when an explicit `z` list falls **entirely**
///   outside the surveyed range, e.g. `z=40,50` on a 0.5–25° radar — the
///   clamp would otherwise invert to `(40, 25)` and silently produce an
///   all-nodata Section. (The interval form is already rejected earlier
///   by `resolve_z_levels`; this guards the comma-list form, which the
///   API layer passes through unvalidated.)
fn angle_window(z: Option<&[f64]>, volume: &PolarVolume) -> Result<(f64, f64), DataServerError> {
    let (smin, smax) = sweep_envelope(volume)
        .ok_or_else(|| DataServerError::Engine("PVOL volume has no finite sweep angles".into()))?;
    let Some(zs) = z.filter(|s| !s.is_empty()) else {
        return Ok((smin, smax));
    };
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for &v in zs.iter().filter(|v| v.is_finite()) {
        lo = lo.min(v);
        hi = hi.max(v);
    }
    if !lo.is_finite() {
        return Ok((smin, smax));
    }
    // Clamp into the site's surveyed range. An inverted clamp means the
    // whole request sits beyond the surveyed angles — surface that rather
    // than returning a silently empty Section.
    let (clo, chi) = (lo.max(smin), hi.min(smax));
    if clo > chi {
        return Err(DataServerError::InvalidParameter(format!(
            "requested elevation angle(s) {lo}–{hi}° are outside this radar's \
             surveyed range {smin}–{smax}°"
        )));
    }
    Ok((clo, chi))
}

/// Build the cross-section height axis (metres above antenna): a regular
/// `0..top` grid where `top` is the height of the `hi_angle` beam at the
/// path's farthest ground distance, clamped to `[step, 25 km]` and capped
/// at `TRAJECTORY_MAX_Z_LEVELS`. Selecting a lower top elevation angle (a
/// narrower `z`) therefore zooms the vertical axis onto that band.
fn height_axis(hi_angle_deg: f64, max_ground_dist_m: f64) -> Vec<f64> {
    // `slant_to_ground_height` wants a slant range, so convert the path's
    // farthest *ground* distance to slant via `ground / cos(el)` (exact
    // for a flat beam, and the dominant term at radar elevation angles).
    // Using the ground distance directly would under-size the ceiling by
    // ~cos(el) — ~3.5 % (≈ 420 m on a 100 km path) at 15°, clipping
    // real coverage near the top of the beam.
    let el = hi_angle_deg.max(0.0);
    let cos_el = el.to_radians().cos().max(1e-3);
    let r = (max_ground_dist_m / cos_el).max(1_000.0);
    let (_, h) = slant_to_ground_height(r, el);
    let top = h.clamp(TRAJECTORY_DEFAULT_Z_STEP_M, 25_000.0);
    let n =
        ((top / TRAJECTORY_DEFAULT_Z_STEP_M).ceil() as usize + 1).clamp(2, TRAJECTORY_MAX_Z_LEVELS);
    let step = top / (n - 1) as f64;
    (0..n).map(|i| i as f64 * step).collect()
}

/// Tolerance (degrees) for the sweep-envelope guard in
/// `sample_polar_slant`. ~Half a typical beam width — wide enough that
/// cells slightly outside the surveyed angle still snap to the nearest
/// sweep, narrow enough that surface cells far from the radar (whose
/// 4/3-Earth-inverted el drops below the horizon) and over-cone cells
/// fall out as `None` instead of fabricating data from the closest
/// sweep.
const SWEEP_ENVELOPE_TOL_DEG: f64 = 1.0;

/// Cached `(min_elangle, max_elangle)` over a volume's finite sweep
/// angles — passed into the per-cell sampler so the envelope check
/// stays O(1) on the hot path. `None` when the volume has no finite
/// sweep angles (a malformed file).
fn sweep_envelope(volume: &PolarVolume) -> Option<(f64, f64)> {
    let (lo, hi) = volume
        .sweeps
        .iter()
        .map(|s| s.elangle)
        .filter(|v| v.is_finite())
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
            (lo.min(v), hi.max(v))
        });
    lo.is_finite().then_some((lo, hi))
}

/// Sample one moment of a polar volume at *slant-range, azimuth* — the
/// cross-section variant of `sample_sweep_moment`. Picks the sweep
/// nearest `elangle_deg`, then maps `slant_range` to a range bin and
/// `azimuth_deg` to an azimuth ray. `None` when the point is outside
/// the sweep's range or the bin is nodata/undetect.
///
/// `envelope` is the volume's `(min_el, max_el)` precomputed by the
/// caller — required, not recomputed per call. With up to
/// `nodes × z_levels × quantities ≈ 5e5` calls per volume and ~20
/// sweeps each, an in-function fold would burn ~10 M iterations per
/// request on a serving worker, breaking the CLAUDE.md hot-path rule.
///
/// Distinct from `sample_sweep_moment` so the existing ground-range
/// interim (used by position/area/Map paths) is untouched: the slant
/// range here comes from the 4/3-Earth inversion in `volume_section`,
/// not from a ground-range fixup.
fn sample_polar_slant(
    volume: &PolarVolume,
    envelope: (f64, f64),
    quantity: &str,
    slant_range_m: f64,
    azimuth_deg: f64,
    elangle_deg: f64,
) -> Option<f64> {
    let sweep = nearest_sweep(volume, elangle_deg)?;
    if sweep.nrays == 0 || sweep.nbins == 0 {
        return None;
    }
    // Reject targets outside the sweep envelope (see the constant's doc
    // for the rationale). Pre-computed envelope keeps this O(1) per cell.
    let (min_el, max_el) = envelope;
    if !elangle_deg.is_finite()
        || elangle_deg < min_el - SWEEP_ENVELOPE_TOL_DEG
        || elangle_deg > max_el + SWEEP_ENVELOPE_TOL_DEG
    {
        return None;
    }
    // A malformed sweep with `rscale <= 0` would silently mis-sample:
    // `rscale = 0` makes the divisor zero (a NaN cast to `i64` becomes
    // 0, sampling the first bin with wrong-range data); `rscale < 0`
    // flips the bin direction and can land inside `[0, nbins)` for a
    // physically wrong gate. ODIM_H5 guarantees `rscale > 0`, but a
    // defensive guard here is cheap and keeps a corrupted file from
    // ever surfacing fabricated values.
    if !sweep.rscale.is_finite() || sweep.rscale <= 0.0 {
        return None;
    }
    let moment = sweep.moments.iter().find(|m| m.quantity == *quantity)?;
    let bin = ((slant_range_m - sweep.rstart) / sweep.rscale).floor() as i64;
    if bin < 0 || bin >= sweep.nbins as i64 {
        return None;
    }
    let ray = (azimuth_deg / (360.0 / sweep.nrays as f64)).floor() as usize % sweep.nrays;
    moment.data.sample(
        ray,
        bin as usize,
        moment.gain,
        moment.offset,
        moment.nodata,
        Some(moment.undetect),
    )
}

/// Build one `Section` coverage: the volume's polar field resampled on
/// `(along-path-distance, height-above-antenna)` and exposed via the
/// CoverageJSON composite axis `[t, lon, lat]` plus a numeric `z` axis
/// in metres. `window` is the caller's selected elevation-angle band
/// `[lo, hi]` (from `z`) — cells whose 4/3-Earth-inverted beam angle
/// falls outside it become nodata.
///
/// The guard actually applied is `window ∩ this volume's own sweep
/// envelope`: a multi-timestep request derives `window` once from the
/// newest volume, but an older entry whose scan strategy reached a
/// lower ceiling must not have a cell that inverts to (say) 20° matched
/// to its 10° top sweep — intersecting per entry keeps each timestep
/// honest about the angles it actually surveyed.
fn volume_section(
    entry: &VolumeEntry,
    path: &[(f64, f64)],
    heights_m: &[f64],
    quantities: &[String],
    window: (f64, f64),
) -> Option<QueryResult> {
    if path.len() < 2 || heights_m.is_empty() || quantities.is_empty() {
        return None;
    }
    // Per-entry effective envelope: the selected window clamped to this
    // volume's surveyed sweep range. A volume with no finite sweeps is
    // skipped; one that doesn't cover the window yields all-nodata cells
    // (an inverted envelope rejects every beam angle).
    let entry_env = sweep_envelope(&entry.volume)?;
    let envelope = (window.0.max(entry_env.0), window.1.min(entry_env.1));
    let site = &entry.volume.site;
    let t = entry.volume.time;
    let nodes: Vec<(DateTime<Utc>, f64, f64)> =
        path.iter().map(|&(lon, lat)| (t, lon, lat)).collect();

    // Per-node geometry from the radar antenna — computed once and shared
    // across every quantity and every z level.
    let geom: Vec<(f64, f64)> = path
        .iter()
        .map(|&(lon, lat)| ground_distance_bearing(site.lon, site.lat, lon, lat))
        .collect();

    let mut ranges = HashMap::new();
    let mut param_descs = HashMap::new();
    let nz = heights_m.len();
    let nn = nodes.len();

    for quantity in quantities {
        let mut values: Vec<Option<f64>> = Vec::with_capacity(nn * nz);
        for &(d, bearing) in &geom {
            for &h in heights_m {
                let (r, el) = ground_height_to_slant(d, h);
                values.push(sample_polar_slant(
                    &entry.volume,
                    envelope,
                    quantity,
                    r,
                    bearing,
                    el,
                ));
            }
        }
        ranges.insert(
            quantity.clone(),
            NdArray {
                shape: vec![nn, nz],
                axis_names: vec!["composite".to_string(), "z".to_string()],
                values,
            },
        );
        param_descs.insert(quantity.clone(), quantity_description(quantity));
    }

    Some(QueryResult {
        domain: DomainDescription::Section {
            nodes,
            z: VerticalCoord {
                kind: VerticalKind::HeightAboveAntenna,
                values: heights_m.to_vec(),
            },
        },
        parameters: param_descs,
        ranges,
    })
}

/// Build the EDR coverages for one radar site at WGS84 `(lon, lat)`.
///
/// `(lon, lat)` is both the coverage's domain point and the sample
/// point: for a location query it is the radar site itself; for a
/// position query it is the requested point against the nearest site.
///
/// With `levels = None` the result is one `VerticalProfile` per timestep
/// (reflectivity vs. elevation angle); with `levels = Some(..)` it is one
/// `PointSeries` per requested level (a time series at that sweep).
fn site_coverages(
    volumes: &[VolumeEntry],
    lon: f64,
    lat: f64,
    datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
    parameters: Option<&[String]>,
    levels: Option<&[f64]>,
) -> Result<Vec<QueryResult>, DataServerError> {
    let selected: Vec<&VolumeEntry> = match datetime {
        Some((start, end)) => volumes
            .iter()
            .filter(|e| e.volume.time >= start && e.volume.time <= end)
            .collect(),
        None => volumes.iter().collect(),
    };
    if selected.is_empty() {
        return Err(DataServerError::LocationNotFound(
            "No PVOL data available for the requested time range".into(),
        ));
    }

    let quantities = resolve_quantities(&selected, parameters)?;

    match levels {
        None => Ok(selected
            .iter()
            // `filter_map` drops volumes that produced no plottable profile
            // (every elangle non-finite — `volume_profile` returns `None`).
            .filter_map(|e| volume_profile(e, lon, lat, &quantities))
            .collect()),
        Some(lvls) => {
            let times: Vec<DateTime<Utc>> = selected.iter().map(|e| e.volume.time).collect();
            Ok(lvls
                .iter()
                .map(|&lvl| level_series(&selected, lon, lat, lvl, &quantities, &times))
                .collect())
        }
    }
}

/// Resolve a requested EDR `z` selector against a `canonical` set of
/// advertised sweep angles. `None` (or an empty selector) means "every
/// level — a profile". `canonical` is the collection's (or site's)
/// advertised vertical axis; `None` there means the collection exposes no
/// sweeps to select with `z` (a 400).
fn resolve_levels(
    canonical: Option<&[f64]>,
    z: Option<&[f64]>,
) -> Result<Option<Vec<f64>>, DataServerError> {
    let Some(zs) = z.filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let canonical = canonical.ok_or_else(|| {
        DataServerError::InvalidParameter(
            "this PVOL collection has no elevation sweeps to select with `z`".into(),
        )
    })?;
    let snapped = snap_levels(zs, canonical);
    Ok((!snapped.is_empty()).then_some(snapped))
}

/// Run a point-style EDR query (position / single-site location) against
/// one site's `volumes`, sampling at WGS84 `(lon, lat)`: resolve the `z`
/// selector against the site's `canonical` sweep angles, build the
/// coverages, and wrap them per [`finalize_single_site`]. Shared by the
/// network engine (after it picks a site) and each per-site view.
fn site_point_query(
    volumes: &[VolumeEntry],
    lon: f64,
    lat: f64,
    datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
    parameters: Option<&[String]>,
    z: Option<&[f64]>,
    canonical: Option<&[f64]>,
) -> Result<CoverageResponse, DataServerError> {
    let levels = resolve_levels(canonical, z)?;
    let covs = site_coverages(volumes, lon, lat, datetime, parameters, levels.as_deref())?;
    finalize_single_site(covs, &levels)
}

/// Parse + resample a WKT `LINESTRING` into the evenly-spaced node path a
/// cross-section samples, rejecting a degenerate (< 2-node) path. Shared
/// by the network engine's and the per-site view's `query_trajectory`.
fn resample_section_path(coords: &str) -> Result<Vec<(f64, f64)>, DataServerError> {
    let vertices = parse_linestring_coords(coords)?;
    let path = resample_path(&vertices);
    if path.len() < 2 {
        return Err(DataServerError::InvalidParameter(
            "LINESTRING must trace a non-degenerate path".into(),
        ));
    }
    Ok(path)
}

/// Build the trajectory cross-section coverage(s) for one site's
/// `volumes` along an already-resampled `path`. Time-filters the volumes,
/// resolves quantities + the `z` angle window, sizes the height axis from
/// the path's farthest reach, and emits one `Section` per timestep
/// (`Single` for one step, `Collection` otherwise). Shared by the network
/// engine (after it picks the nearest site) and each per-site view.
fn site_trajectory(
    volumes: &[VolumeEntry],
    path: &[(f64, f64)],
    datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
    parameters: Option<&[String]>,
    z: Option<&[f64]>,
) -> Result<CoverageResponse, DataServerError> {
    let selected: Vec<&VolumeEntry> = match datetime {
        Some((start, end)) => volumes
            .iter()
            .filter(|e| e.volume.time >= start && e.volume.time <= end)
            .collect(),
        None => volumes.iter().collect(),
    };
    if selected.is_empty() {
        return Err(DataServerError::LocationNotFound(
            "No PVOL data available for the requested time range".into(),
        ));
    }

    let quantities = resolve_quantities(&selected, parameters)?;

    // `z` carries the requested elevation angles (resolved by the API
    // layer against the advertised extent). Derive the angle window and
    // the matching height axis from the most recent volume. An
    // out-of-range `z` list surfaces as `InvalidParameter` (400) rather
    // than a silently empty Section.
    let ref_volume = &selected.last().unwrap().volume;
    let window = angle_window(z, ref_volume)?;
    // The path's farthest ground distance from the radar sizes the
    // height-axis ceiling for the selected top angle.
    let max_ground_dist = path
        .iter()
        .map(|&(lon, lat)| {
            ground_distance_bearing(ref_volume.site.lon, ref_volume.site.lat, lon, lat).0
        })
        .fold(0.0_f64, f64::max);
    let heights = height_axis(window.1, max_ground_dist);
    if heights.is_empty() {
        return Err(DataServerError::InvalidParameter(
            "trajectory `z` resolved to an empty axis".into(),
        ));
    }

    let coverages: Vec<QueryResult> = selected
        .iter()
        .filter_map(|e| volume_section(e, path, &heights, &quantities, window))
        .collect();
    if coverages.is_empty() {
        return Err(DataServerError::LocationNotFound(
            "No PVOL volumes produced a section for the requested path".into(),
        ));
    }
    if coverages.len() == 1 {
        // Single timestep — emit one Coverage rather than a one-item
        // collection, matching the `query_location` precedent.
        Ok(CoverageResponse::Single(
            coverages.into_iter().next().unwrap(),
        ))
    } else {
        Ok(CoverageResponse::Collection(coverages))
    }
}

/// Wrap one site's coverages: a request pinned to exactly one level is a
/// single `PointSeries` (`Single`); every other shape is a `Collection`.
fn finalize_single_site(
    covs: Vec<QueryResult>,
    levels: &Option<Vec<f64>>,
) -> Result<CoverageResponse, DataServerError> {
    if matches!(levels, Some(l) if l.len() == 1) {
        // `site_coverages` builds exactly one coverage per requested
        // level, so a single-level request yields exactly one. A missing
        // coverage would be an internal invariant violation — surface it
        // as a logged 500, not an opaque `spawn_blocking` panic.
        return covs
            .into_iter()
            .next()
            .map(CoverageResponse::Single)
            .ok_or_else(|| {
                DataServerError::Engine(
                    "internal: single-level query produced no coverage (invariant violated)".into(),
                )
            });
    }
    Ok(CoverageResponse::Collection(covs))
}

// ---------------------------------------------------------------------------
// Per-site collections (model B)
// ---------------------------------------------------------------------------

impl PolarVolumeEngine {
    /// NOD codes of every radar site currently in the catalog, sorted.
    ///
    /// The loader calls this after construction (one synchronous scan has
    /// already run) to expand this one source config into N per-site OGC
    /// collections — one [`site_view`](Self::site_view) each. The set is a
    /// snapshot: sites appearing or disappearing between polls are only
    /// reflected on the next admin reload, which re-runs the expansion.
    pub fn site_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.catalog.load().by_site.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Human place name (ODIM `/what` PLC) for site `nod`, if the catalog
    /// holds one — used to title the per-site collection. `None` when the
    /// site is unknown or carries no place name.
    pub fn site_label(&self, nod: &str) -> Option<String> {
        self.catalog.load().by_site_meta.get(nod)?.plc.clone()
    }

    /// Build a [`PolarVolumeSiteView`] scoped to radar `nod`, sharing this
    /// engine's live catalog (`ArcSwap`) so the view tracks poll-loop
    /// updates without re-parsing. `collection_id` is the per-site OGC
    /// collection id (`{base}-{nod}`) used in the view's error messages.
    pub fn site_view(&self, nod: &str, collection_id: &str) -> PolarVolumeSiteView {
        PolarVolumeSiteView {
            catalog: self.catalog.clone(),
            nod: nod.to_string(),
            collection_id: collection_id.to_string(),
        }
    }
}

/// A single radar site exposed as its own OGC collection (model B).
///
/// Where [`PolarVolumeEngine`] owns the source scan, parse cache, and poll
/// loop for a whole radar *network*, a `PolarVolumeSiteView` is a thin,
/// cheap handle scoped to one site `nod`: its `MapEngine`/`EdrEngine`
/// surface advertises **bare quantity** parameters (`DBZH`, `VRADH`, …),
/// a single location (the antenna), and the one site's spatial/vertical/
/// temporal extents. Many views share one engine's `Arc<ArcSwap<Catalog>>`,
/// so they all see poll-loop refreshes for free.
///
/// The site is not a sub-resource of a network collection: under model B
/// there is no network-level collection at all — each radar is registered
/// independently. The parse cache, poll loop, and shutdown all live on the
/// owning engine.
pub struct PolarVolumeSiteView {
    /// Live catalog shared with the owning [`PolarVolumeEngine`].
    catalog: Arc<ArcSwap<Catalog>>,
    /// ODIM NOD code this view is scoped to.
    nod: String,
    /// Per-site OGC collection id (`{base}-{nod}`), for error messages.
    collection_id: String,
}

impl MapEngine for PolarVolumeSiteView {
    fn get_raster_tile(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        output_crs: &OutputCrs,
        parameter: Option<&str>,
        z: Option<f64>,
    ) -> Result<RasterTile, DataServerError> {
        // A per-site PVOL collection is still multi-parameter — one layer
        // per radar quantity — but the layer name is now the bare quantity
        // (no `<site>:` prefix): the site *is* the collection.
        let quantity = parameter.ok_or_else(|| {
            DataServerError::InvalidParameter(format!(
                "[{}] PVOL collection requires a quantity parameter (e.g. `DBZH`)",
                self.collection_id
            ))
        })?;

        let catalog = self.catalog.load();
        let site_volumes = catalog.by_site.get(&self.nod).ok_or_else(|| {
            // The site aged out of the catalog since this view was
            // registered (source change / `max_files`). A reload would
            // drop the collection; until then report no data, not a 500.
            DataServerError::LocationNotFound(format!(
                "[{}] radar site `{}` has no current volumes",
                self.collection_id, self.nod
            ))
        })?;

        // Select the volume nearest `time` (latest if `None`).
        let entry = match time {
            Some(target) => site_volumes
                .iter()
                .min_by_key(|e| (e.volume.time - target).num_seconds().abs()),
            None => site_volumes.last(),
        }
        .ok_or_else(|| {
            DataServerError::LocationNotFound(format!(
                "[{}] radar site `{}` has no volumes",
                self.collection_id, self.nod
            ))
        })?;

        polar_sample(&entry.volume, quantity, bbox, width, height, output_crs, z)
    }

    fn raster_info(&self) -> RasterInfo {
        let catalog = self.catalog.load();
        let meta = catalog.by_site_meta.get(&self.nod);
        let parameters = meta.map(|m| m.parameters.clone()).unwrap_or_default();
        let parameter = parameters
            .first()
            .map(|(name, _)| name.clone())
            .unwrap_or_default();

        RasterInfo {
            native_crs: "CRS:84".to_string(),
            spatial_extent: meta.and_then(|m| m.spatial_extent),
            times: meta.map(|m| m.times.clone()).unwrap_or_default(),
            parameter,
            // PVOL quantities span multiple physical units; the per-layer
            // unit is not a single collection constant — leave it blank.
            unit: String::new(),
            parameters,
            vertical: meta.and_then(|m| m.vertical.clone()),
            grid_size: None,
        }
    }
}

impl EdrEngine for PolarVolumeSiteView {
    /// Exactly one EDR location — this radar site — `id` is the NOD code
    /// and the point geometry is the antenna position.
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        let catalog = self.catalog.load();
        Ok(catalog
            .by_site_meta
            .get(&self.nod)
            .map(|m| {
                vec![Location {
                    id: self.nod.clone(),
                    label: m.plc.clone().unwrap_or_else(|| self.nod.clone()),
                    latitude: m.lat,
                    longitude: m.lon,
                }]
            })
            .unwrap_or_default())
    }

    /// Query this radar site by NOD code. The only valid `location_id` is
    /// the view's own `nod`; any other is `LocationNotFound`.
    fn query_location(
        &self,
        location_id: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
    ) -> Result<CoverageResponse, DataServerError> {
        if location_id != self.nod {
            return Err(DataServerError::LocationNotFound(location_id.to_string()));
        }
        let catalog = self.catalog.load();
        let volumes = catalog
            .by_site
            .get(&self.nod)
            .ok_or_else(|| DataServerError::LocationNotFound(self.nod.clone()))?;
        let site = &volumes
            .last()
            .ok_or_else(|| DataServerError::LocationNotFound(self.nod.clone()))?
            .volume
            .site;
        let canonical = catalog
            .by_site_meta
            .get(&self.nod)
            .and_then(|m| m.vertical.as_ref())
            .map(|v| v.levels.as_slice());
        site_point_query(
            volumes, site.lon, site.lat, datetime, parameters, z, canonical,
        )
    }

    fn get_parameters(&self) -> Vec<String> {
        self.catalog
            .load()
            .by_site_meta
            .get(&self.nod)
            .map(|m| m.quantities.clone())
            .unwrap_or_default()
    }

    fn get_vertical_extent(&self) -> Option<VerticalDimension> {
        self.catalog
            .load()
            .by_site_meta
            .get(&self.nod)
            .and_then(|m| m.vertical.clone())
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        let catalog = self.catalog.load();
        let m = catalog.by_site_meta.get(&self.nod)?;
        Some((*m.times.first()?, *m.times.last()?))
    }

    /// Advertise the exact volume timestamps — same rationale as the
    /// network engine and the COMP engine.
    fn get_available_times(&self) -> Option<Vec<DateTime<Utc>>> {
        let catalog = self.catalog.load();
        let times = catalog
            .by_site_meta
            .get(&self.nod)
            .map(|m| m.times.clone())
            .unwrap_or_default();
        (!times.is_empty()).then_some(times)
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        self.catalog
            .load()
            .by_site_meta
            .get(&self.nod)
            .and_then(|m| m.spatial_extent)
    }

    fn supported_query_types(&self) -> Vec<String> {
        vec![
            "locations".to_string(),
            "position".to_string(),
            "area".to_string(),
            "trajectory".to_string(),
        ]
    }

    /// Position query — same `z`-driven shape as [`query_location`](Self::query_location),
    /// but always against this one site (no nearest-radar pick).
    fn query_position(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
    ) -> Result<CoverageResponse, DataServerError> {
        let (lat, lon) = parse_point_coords(coords)?;
        let catalog = self.catalog.load();
        let volumes = catalog.by_site.get(&self.nod).ok_or_else(|| {
            DataServerError::LocationNotFound(format!(
                "[{}] radar site `{}` has no current volumes",
                self.collection_id, self.nod
            ))
        })?;
        let canonical = catalog
            .by_site_meta
            .get(&self.nod)
            .and_then(|m| m.vertical.as_ref())
            .map(|v| v.levels.as_slice());
        site_point_query(volumes, lon, lat, datetime, parameters, z, canonical)
    }

    /// Area query — a `CoverageCollection` of this site's coverages, but
    /// only when the antenna falls inside the requested polygon (a per-site
    /// collection holds exactly one radar). Sampled at the antenna itself,
    /// matching the network engine's in-polygon-site semantics.
    fn query_area(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
    ) -> Result<CoverageResponse, DataServerError> {
        let polygon = parse_area_coords(coords)?;
        let catalog = self.catalog.load();
        let meta = catalog.by_site_meta.get(&self.nod).ok_or_else(|| {
            DataServerError::LocationNotFound(format!(
                "[{}] radar site `{}` has no current volumes",
                self.collection_id, self.nod
            ))
        })?;
        if !polygon.contains(meta.lon, meta.lat) {
            return Err(DataServerError::LocationNotFound(
                "the radar site is not within the requested area".into(),
            ));
        }
        let volumes = catalog.by_site.get(&self.nod).ok_or_else(|| {
            DataServerError::LocationNotFound(format!(
                "[{}] radar site `{}` has no current volumes",
                self.collection_id, self.nod
            ))
        })?;
        let canonical = meta.vertical.as_ref().map(|v| v.levels.as_slice());
        let levels = resolve_levels(canonical, z)?;
        let covs = site_coverages(
            volumes,
            meta.lon,
            meta.lat,
            datetime,
            parameters,
            levels.as_deref(),
        )?;
        Ok(CoverageResponse::Collection(covs))
    }

    /// Trajectory cross-section along a WKT `LINESTRING`, always against
    /// this one site (no nearest-radar pick). Same `Section` output shape
    /// as the network engine's [`query_trajectory`](PolarVolumeEngine).
    fn query_trajectory(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
    ) -> Result<CoverageResponse, DataServerError> {
        let path = resample_section_path(coords)?;
        let catalog = self.catalog.load();
        let volumes = catalog.by_site.get(&self.nod).ok_or_else(|| {
            DataServerError::LocationNotFound(format!(
                "[{}] radar site `{}` has no current volumes",
                self.collection_id, self.nod
            ))
        })?;
        site_trajectory(volumes, &path, datetime, parameters, z)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pvol::RadarSite;
    use crate::reader::RawPixels;
    use ndarray::Array2;

    /// `round_elevation` collapses a slightly-negative grazing angle to a
    /// canonical `+0.0` — otherwise `-0.0` and `+0.0` would survive dedup and
    /// the z axis would carry both as separate "0" levels.
    #[test]
    fn round_elevation_normalises_negative_zero() {
        let r = round_elevation(-0.04);
        assert_eq!(r, 0.0);
        assert!(
            r.is_sign_positive(),
            "expected +0.0 after normalisation, got {r}"
        );
        // NaN propagates (caller is responsible for filtering with retain).
        assert!(round_elevation(f64::NAN).is_nan());
    }

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

    /// `destination_point` plus `ground_distance_bearing` are inverses
    /// up to spherical-trig round-off — moving 50 km along bearing 45°
    /// from a starting point gives back distance 50 km and bearing 45°.
    #[test]
    fn destination_point_roundtrips_via_distance_bearing() {
        let (lon0, lat0) = (25.0, 60.0);
        let (lon1, lat1) = destination_point(lon0, lat0, 50_000.0, 45.0);
        let (d, az) = ground_distance_bearing(lon0, lat0, lon1, lat1);
        assert!((d - 50_000.0).abs() < 1.0, "round-trip distance, got {d}");
        assert!((az - 45.0).abs() < 1e-3, "round-trip bearing, got {az}");
    }

    /// Crossing the antimeridian eastbound keeps the longitude in
    /// (−180, 180]: starting at 179.9° and travelling east must wrap to a
    /// small negative longitude, not 180.x°, so the value is valid in a
    /// CoverageJSON node.
    #[test]
    fn destination_point_normalises_longitude_across_antimeridian() {
        let (lon, _lat) = destination_point(179.9, 0.0, 50_000.0, 90.0);
        assert!(
            (-180.0..=180.0).contains(&lon),
            "lon must be normalised into (−180, 180], got {lon}"
        );
        assert!(
            lon < 0.0,
            "eastbound past 180° wraps to negative, got {lon}"
        );
    }

    /// 4/3-Earth forward + inverse are mutual inverses to sub-metre /
    /// sub-mdeg residuals over the radar-relevant envelope
    /// (≤ 250 km range, 0°–30° elevation, up to 20 km height). This is
    /// the round-trip check; the next test pins absolute values against
    /// a Doviak reference.
    #[test]
    fn four_thirds_earth_roundtrips() {
        for &r in &[1_000.0_f64, 50_000.0, 100_000.0, 200_000.0, 250_000.0] {
            for &el in &[0.5_f64, 1.5, 5.0, 15.0, 30.0] {
                let (s, h) = slant_to_ground_height(r, el);
                let (r2, el2) = ground_height_to_slant(s, h);
                assert!(
                    (r - r2).abs() < 0.5,
                    "slant round-trip residual @ r={r} el={el}: r2={r2}"
                );
                assert!(
                    (el - el2).abs() < 1e-6,
                    "elangle round-trip residual @ r={r} el={el}: el2={el2}"
                );
            }
        }
    }

    /// Absolute pins (not round-trip) against the 4/3-Earth model: at
    /// elevation 0° the beam grazes the surface, so a 100 km slant
    /// range gives ground distance ≈ 99.95 km and height ≈ 587 m
    /// (R'/(2R'+r) curvature drop). At elevation 90° the beam goes
    /// straight up: ground distance ≈ 0, height ≈ r. Both follow from
    /// the formula given in `slant_to_ground_height`.
    #[test]
    fn four_thirds_earth_absolute_reference() {
        // Zero elevation, 100 km range: height = √(r² + R'²) − R'
        //                              = √(1e10 + R'²) − R' ≈ 587 m
        let (s, h) = slant_to_ground_height(100_000.0, 0.0);
        let expected_h =
            (100_000.0_f64.powi(2) + FOUR_THIRDS_EARTH_M.powi(2)).sqrt() - FOUR_THIRDS_EARTH_M;
        assert!(
            (h - expected_h).abs() < 0.01,
            "el=0 r=100km: h={h}, expected {expected_h}"
        );
        // Ground distance for el=0 ≈ R' · atan(r/R') ≈ r at radar range.
        // The drop from r is the curvature correction (~50 m at 100 km).
        assert!((s - 99_950.0).abs() < 60.0, "el=0 r=100km: s={s}");

        // 90° elevation: everything points straight up.
        let (s, h) = slant_to_ground_height(50_000.0, 90.0);
        assert!(s.abs() < 1e-3, "el=90 r=50km: s={s} should be ~0");
        assert!(
            (h - 50_000.0).abs() < 1.0,
            "el=90 r=50km: h={h} should be ~50000"
        );

        // 1° elevation, 50 km range. Under 4/3-Earth, h ≈ 1020 m
        // (≈ r·sin(el) ≈ 873 m plus the ~147 m curvature drop). The
        // ground distance is r·cos(el) less ~10 m of curvature
        // correction — `s ≈ 49982.6 m` — looser tolerance than the
        // straight-line approximation would suggest.
        let (s, h) = slant_to_ground_height(50_000.0, 1.0);
        assert!(
            (s - 49_983.0).abs() < 5.0,
            "el=1 r=50km: s={s} should be ~49983"
        );
        assert!(
            (h - 1_020.0).abs() < 10.0,
            "el=1 r=50km: h={h} should be ~1020"
        );
    }

    /// `sample_polar_slant` returns `None` for malformed sweep geometry
    /// (`rscale` zero, negative, or non-finite) rather than fabricating
    /// a bin-0 sample. ODIM_H5 guarantees `rscale > 0`, but a corrupted
    /// file would otherwise produce a `NaN as i64 == 0` cast and report
    /// the first bin's value at every requested range.
    #[test]
    fn sample_polar_slant_rejects_malformed_rscale() {
        let (site_lon, site_lat) = (25.0, 60.0);
        let mut vol = synthetic_volume(site_lon, site_lat);
        let env = sweep_envelope(&vol).expect("synthetic volume has finite sweep");
        let azimuth = 90.0;
        let elangle = 0.5;
        let range_m = 10_000.0;

        // Baseline: well-formed sweep returns a finite sample.
        let v = sample_polar_slant(&vol, env, "DBZH", range_m, azimuth, elangle);
        assert!(v.is_some(), "well-formed sweep must sample");

        for bad in [0.0_f64, -1_000.0, f64::NAN, f64::INFINITY] {
            vol.sweeps[0].rscale = bad;
            assert!(
                sample_polar_slant(&vol, env, "DBZH", range_m, azimuth, elangle).is_none(),
                "rscale={bad} must yield None, not fabricated data"
            );
        }
    }

    /// `sample_polar_slant` returns `None` for elevation targets outside
    /// the sweep envelope (above the highest beam or well below the
    /// lowest). This guards against the silent surface-row substitution
    /// the 4/3-Earth inversion otherwise causes: at h=0 far from the
    /// radar, the inverted target el goes slightly negative; without
    /// this guard `nearest_sweep` still picks the lowest sweep and
    /// fabricates "ground" reflectivity from a beam aimed well above.
    /// Found by claude-review on PR #275.
    #[test]
    fn sample_polar_slant_rejects_out_of_envelope_elevation() {
        let (site_lon, site_lat) = (25.0, 60.0);
        let vol = synthetic_volume(site_lon, site_lat);
        let env = sweep_envelope(&vol).unwrap();
        // Synthetic fixture has one sweep at 0.5°. Tolerance is 1°, so
        // targets in [-0.5°, 1.5°] are accepted; everything outside ⇒ None.
        let azimuth = 90.0;
        let range_m = 10_000.0;

        // Inside the window — sampled.
        assert!(sample_polar_slant(&vol, env, "DBZH", range_m, azimuth, 0.5).is_some());
        assert!(sample_polar_slant(&vol, env, "DBZH", range_m, azimuth, 1.4).is_some());
        assert!(sample_polar_slant(&vol, env, "DBZH", range_m, azimuth, -0.4).is_some());

        // Just outside the window — None.
        assert!(sample_polar_slant(&vol, env, "DBZH", range_m, azimuth, 1.6).is_none());
        assert!(sample_polar_slant(&vol, env, "DBZH", range_m, azimuth, -0.6).is_none());

        // Far outside — None (the 90° overhead case that bit the
        // h=large cells at the radar location).
        assert!(sample_polar_slant(&vol, env, "DBZH", range_m, azimuth, 90.0).is_none());

        // Non-finite el — None.
        assert!(sample_polar_slant(&vol, env, "DBZH", range_m, azimuth, f64::NAN).is_none());
    }

    /// `angle_window` selects the requested elevation-angle span, clamped
    /// to the volume's actual sweep range; `None` selects the full span;
    /// a request entirely above the top sweep is a 400, not silent nodata.
    #[test]
    fn angle_window_selects_and_clamps() {
        let (site_lon, site_lat) = (25.0, 60.0);
        let mut vol = synthetic_volume(site_lon, site_lat);
        // Give it a fan of sweeps 0.5°..15°.
        let base = vol.sweeps[0].clone();
        vol.sweeps = [0.5, 1.5, 5.0, 9.0, 15.0]
            .iter()
            .map(|&el| {
                let mut s = base.clone();
                s.elangle = el;
                s
            })
            .collect();

        // No z → full sweep span.
        assert_eq!(angle_window(None, &vol).unwrap(), (0.5, 15.0));
        // A sub-range → exactly that window.
        assert_eq!(
            angle_window(Some(&[1.5, 5.0, 9.0]), &vol).unwrap(),
            (1.5, 9.0)
        );
        // A request straddling the top sweep clamps to the surveyed range.
        assert_eq!(angle_window(Some(&[5.0, 45.0]), &vol).unwrap(), (5.0, 15.0));
        // A discrete list entirely above the top sweep would invert the
        // clamp to (40, 15) → would silently produce an all-nodata
        // Section. It must be a clear `InvalidParameter` instead.
        match angle_window(Some(&[40.0, 50.0]), &vol) {
            Err(DataServerError::InvalidParameter(_)) => {}
            other => panic!("expected InvalidParameter for out-of-range z, got {other:?}"),
        }
    }

    /// `height_axis` builds a monotonic 0..top grid whose ceiling tracks
    /// the selected top angle and is clamped to ≤ 25 km.
    #[test]
    fn height_axis_tracks_top_angle_and_caps() {
        // A 2° beam at 30 km ground distance reaches ~1 km up.
        let low = height_axis(2.0, 30_000.0);
        assert!(low.first() == Some(&0.0), "axis starts at 0");
        assert!(low.windows(2).all(|w| w[1] > w[0]), "monotonic ascending");
        assert!(*low.last().unwrap() <= 25_000.0);
        // A higher top angle reaches higher (taller axis ceiling).
        let high = height_axis(15.0, 30_000.0);
        assert!(*high.last().unwrap() > *low.last().unwrap());
        // The slant correction (ground/cos el) makes the 15° ceiling a
        // touch higher than using the ground distance as slant directly —
        // a few hundred metres on a 30 km path — so real coverage near the
        // top of the beam isn't clipped. (Both are well under the 25 km
        // cap at 30 km, so the correction is observable here.)
        let (_, h_ground_as_slant) = slant_to_ground_height(30_000.0, 15.0);
        assert!(*high.last().unwrap() <= 25_000.0, "below the cap at 30 km");
        assert!(
            *high.last().unwrap() > h_ground_as_slant,
            "cos-corrected ceiling {} must exceed the ground-as-slant value {}",
            high.last().unwrap(),
            h_ground_as_slant
        );
        // A near-vertical beam at long range is capped at 25 km, not
        // hundreds of km.
        let capped = height_axis(89.0, 200_000.0);
        assert!(*capped.last().unwrap() <= 25_000.0 + 1.0);
        assert!(capped.len() <= 100, "level count capped");
    }

    /// A zero-length polyline (all vertices coincident) resamples to a
    /// *single* node, so `query_trajectory`'s `path.len() < 2` guard turns
    /// it into a 400 rather than a Section with duplicate composite nodes.
    /// (`parse_linestring_coords` rejects this upstream too; this guards a
    /// direct call.)
    #[test]
    fn resample_path_degenerate_returns_single_node() {
        let coincident = vec![(25.0, 60.0), (25.0, 60.0)];
        assert_eq!(resample_path(&coincident).len(), 1);
        // A real path still resamples to ≥2 nodes.
        let real = vec![(25.0, 60.0), (25.5, 60.5)];
        assert!(resample_path(&real).len() >= 2);
    }

    /// `volume_section` clamps the caller's angle window to *this volume's*
    /// surveyed sweep range. A window wider than the volume's sweeps (as
    /// happens when the window is derived from a newer, deeper scan) must
    /// not fabricate data: a cell that inverts to an elevation the volume
    /// never scanned stays nodata rather than snapping to its top sweep.
    #[test]
    fn volume_section_clamps_window_to_per_volume_sweeps() {
        let (site_lon, site_lat) = (25.0, 60.0);
        // The synthetic volume has a single 0.5° sweep.
        let vol = synthetic_volume(site_lon, site_lat);
        let e = entry(vol, "v0");

        // A node ~10 km due east: at height 0 the beam grazes the surface
        // (el ≈ 0°, inside the 0.5° sweep envelope ±1°); at 1500 m it
        // climbs to el ≈ 8–9° — inside the *window* (0.5–25°) but outside
        // this volume's actual range, so it must be nodata.
        let dlon_10km = 10_000.0 / (EARTH_RADIUS_M * site_lat.to_radians().cos()) * 180.0
            / std::f64::consts::PI;
        let path = vec![(site_lon, site_lat), (site_lon + dlon_10km, site_lat)];
        let heights = vec![0.0, 1500.0];
        let window = (0.5, 25.0); // wider than the volume's 0.5° sweep

        let qr = volume_section(&e, &path, &heights, &["DBZH".to_string()], window)
            .expect("section produced");
        let nd = qr.ranges.get("DBZH").expect("DBZH range");
        // Layout is row-major [node][height]; the far node is index 1, so
        // its first cell starts at `heights.len()`.
        let nz = heights.len();
        let far_surface = nd.values[nz]; // (10 km, 0 m)
        let far_high = nd.values[nz + 1]; // (10 km, 1500 m)
        assert!(
            far_surface.is_some(),
            "the surface cell grazes the 0.5° sweep and must sample"
        );
        assert!(
            far_high.is_none(),
            "a cell at ~8° must stay nodata — the volume never scanned that \
             angle, even though it's inside the requested window"
        );
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
        let tile = polar_sample(&vol, "DBZH", bbox, 50, 1, &OutputCrs::Wgs84, None).unwrap();

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
        let tile = polar_sample(&vol, "DBZH", bbox, 4, 1, &OutputCrs::Wgs84, None).unwrap();
        assert!(
            tile.values.iter().all(Option::is_none),
            "pixels past max range must be None"
        );
    }

    /// Build a single moment whose raw `[ray, bin]` value is `set(ray, bin)`.
    fn moment_with(nrays: usize, nbins: usize, set: impl Fn(usize, usize) -> u16) -> PolarMoment {
        let mut data = Array2::<u16>::zeros((nrays, nbins));
        for r in 0..nrays {
            for b in 0..nbins {
                data[(r, b)] = set(r, b);
            }
        }
        PolarMoment {
            quantity: "DBZH".to_string(),
            gain: 1.0,
            offset: 0.0,
            nodata: 65_535.0,
            undetect: 65_534.0,
            data: RawPixels::U16(data),
        }
    }

    /// Bilinear blends across the two straddling rays (the anti-spoke
    /// fix, #186): a point halfway between ray 0 (20) and ray 1 (40) at
    /// bin 10 samples the mean, 30 — not one ray's value.
    #[test]
    fn bilinear_cell_blends_adjacent_rays() {
        let m = moment_with(360, 20, |r, b| match (r, b) {
            (0, 10) => 20,
            (1, 10) => 40,
            _ => 0,
        });
        let v = bilinear_cell(&m, 360, 20, 0.5, 10.0).unwrap();
        assert!((v - 30.0).abs() < 1e-9, "ray blend (20+40)/2, got {v}");
    }

    /// The azimuth axis wraps at the 0°/360° seam: ray 359 and ray 0
    /// blend for a point just inside the last ray.
    #[test]
    fn bilinear_cell_wraps_azimuth_seam() {
        let m = moment_with(360, 20, |r, b| match (r, b) {
            (359, 10) => 10,
            (0, 10) => 30,
            _ => 0,
        });
        let v = bilinear_cell(&m, 360, 20, 359.5, 10.0).unwrap();
        assert!((v - 20.0).abs() < 1e-9, "seam blend (10+30)/2, got {v}");
    }

    /// A `nodata`/`undetect` neighbour is dropped and the weights are
    /// renormalised over the valid cells — so a masked ray never darkens
    /// valid output (a 50/50 blend with a masked cell returns the valid
    /// value, not half of it).
    #[test]
    fn bilinear_cell_renormalises_over_masked_neighbours() {
        let m = moment_with(360, 20, |r, b| match (r, b) {
            (0, 10) => 20,
            (1, 10) => 65_535, // nodata
            _ => 0,
        });
        let v = bilinear_cell(&m, 360, 20, 0.5, 10.0).unwrap();
        assert!(
            (v - 20.0).abs() < 1e-9,
            "masked neighbour renormalised away, got {v}"
        );
    }

    /// Every contributing neighbour masked → `None` (transparent).
    #[test]
    fn bilinear_cell_all_masked_is_none() {
        let m = moment_with(360, 20, |_, b| if b == 10 { 65_535 } else { 0 });
        assert!(bilinear_cell(&m, 360, 20, 0.5, 10.0).is_none());
    }

    /// A range-edge neighbour (`bin + 1 == nbins`) is dropped, not read
    /// out of bounds; the in-range cell still samples.
    #[test]
    fn bilinear_cell_drops_range_edge_neighbour() {
        let m = moment_with(360, 20, |_, b| if b == 19 { 42 } else { 0 });
        let v = bilinear_cell(&m, 360, 20, 5.0, 19.5).unwrap();
        assert!((v - 42.0).abs() < 1e-9, "edge neighbour dropped, got {v}");
    }

    /// End-to-end: the bilinear sampler returns a *fractional* bin value
    /// where the nearest-neighbour sampler floors — confirming the
    /// interpolation is wired into the render path.
    #[test]
    fn sample_sweep_moment_bilinear_vs_nearest() {
        let (site_lon, site_lat) = (25.0, 60.0);
        let vol = synthetic_volume(site_lon, site_lat); // raw[ray][bin]=bin, 1 km bins
        let sweep = &vol.sweeps[0];
        let moment = &sweep.moments[0];
        // ~10.5 km due east → fractional bin ≈ 10.5.
        let dlon = 10_500.0 / (EARTH_RADIUS_M * site_lat.to_radians().cos()) * 180.0
            / std::f64::consts::PI;
        let (lon, lat) = (site_lon + dlon, site_lat);
        let bilinear = sample_sweep_moment_bilinear(sweep, moment, site_lon, site_lat, lon, lat)
            .expect("bilinear sample");
        let nearest =
            sample_sweep_moment(sweep, moment, site_lon, site_lat, lon, lat).expect("nn sample");
        assert!(
            (bilinear - 10.5).abs() < 0.1,
            "bilinear interpolates to ≈ 10.5, got {bilinear}"
        );
        assert_eq!(nearest, 10.0, "nearest floors to bin 10");
    }

    /// The bilinear sampler carries the same malformed-`rscale` guard as
    /// `sample_polar_slant`: a zero / negative / non-finite `rscale`
    /// yields `None`, not a sample from the wrong (or NaN) range gate.
    #[test]
    fn sample_sweep_moment_bilinear_rejects_malformed_rscale() {
        let (site_lon, site_lat) = (25.0, 60.0);
        let mut vol = synthetic_volume(site_lon, site_lat);
        let dlon = 10_000.0 / (EARTH_RADIUS_M * site_lat.to_radians().cos()) * 180.0
            / std::f64::consts::PI;
        let (lon, lat) = (site_lon + dlon, site_lat);
        // Baseline: well-formed sweep samples.
        assert!(sample_sweep_moment_bilinear(
            &vol.sweeps[0],
            &vol.sweeps[0].moments[0],
            site_lon,
            site_lat,
            lon,
            lat
        )
        .is_some());
        for bad in [0.0_f64, -1_000.0, f64::NAN, f64::INFINITY] {
            vol.sweeps[0].rscale = bad;
            assert!(
                sample_sweep_moment_bilinear(
                    &vol.sweeps[0],
                    &vol.sweeps[0].moments[0],
                    site_lon,
                    site_lat,
                    lon,
                    lat
                )
                .is_none(),
                "rscale={bad} must yield None"
            );
        }
    }

    /// The nearest-neighbour EDR sampler carries the malformed-`rscale`
    /// guard too: `rscale = 0`/NaN must not slip a `NaN as i64 == 0` cast
    /// past the bounds check and return bin-0 data for every point.
    #[test]
    fn sample_sweep_moment_rejects_malformed_rscale() {
        let (site_lon, site_lat) = (25.0, 60.0);
        let mut vol = synthetic_volume(site_lon, site_lat);
        let dlon = 10_000.0 / (EARTH_RADIUS_M * site_lat.to_radians().cos()) * 180.0
            / std::f64::consts::PI;
        let (lon, lat) = (site_lon + dlon, site_lat);
        assert!(sample_sweep_moment(
            &vol.sweeps[0],
            &vol.sweeps[0].moments[0],
            site_lon,
            site_lat,
            lon,
            lat
        )
        .is_some());
        for bad in [0.0_f64, -1_000.0, f64::NAN, f64::INFINITY] {
            vol.sweeps[0].rscale = bad;
            assert!(
                sample_sweep_moment(
                    &vol.sweeps[0],
                    &vol.sweeps[0].moments[0],
                    site_lon,
                    site_lat,
                    lon,
                    lat
                )
                .is_none(),
                "rscale={bad} must yield None"
            );
        }
    }

    /// End-to-end ray-blending guard (#186). The fixture's value depends
    /// only on the *ray* (`raw[ray][bin] = ray`), so nearest-neighbour
    /// sampling can only return integer ray indices — the bilinear render
    /// blends adjacent rays into fractional values. A regression of
    /// `polar_sample` back to `sample_sweep_moment` would make every
    /// output pixel integer-valued; this catches that.
    #[test]
    fn polar_sample_blends_across_rays_end_to_end() {
        let (site_lon, site_lat) = (25.0, 60.0);
        let (nrays, nbins) = (360usize, 100usize);
        let mut data = Array2::<u16>::zeros((nrays, nbins));
        for r in 0..nrays {
            for b in 0..nbins {
                data[(r, b)] = r as u16; // value = ray index, constant across bins
            }
        }
        let moment = PolarMoment {
            quantity: "DBZH".to_string(),
            gain: 1.0,
            offset: 0.0,
            nodata: 65_535.0,
            undetect: 65_534.0,
            data: RawPixels::U16(data),
        };
        let sweep = Sweep {
            elangle: 0.5,
            nbins,
            nrays,
            rscale: 1_000.0,
            rstart: 0.0,
            a1gate: 0,
            moments: vec![moment],
        };
        let vol = PolarVolume {
            site: RadarSite {
                lon: site_lon,
                lat: site_lat,
                height: 100.0,
                nod: Some("test".to_string()),
                plc: None,
                wmo: None,
            },
            time: Utc::now(),
            object: "PVOL".to_string(),
            sweeps: vec![sweep],
        };
        // A box NE of the site spanning many azimuths within range.
        let bbox = [
            site_lon + 0.02,
            site_lat + 0.02,
            site_lon + 0.25,
            site_lat + 0.25,
        ];
        let tile = polar_sample(&vol, "DBZH", bbox, 24, 24, &OutputCrs::Wgs84, None).unwrap();
        // Value varies only by ray, so any fractional pixel proves the
        // render blended across rays — impossible for nearest-neighbour.
        let fractional = tile
            .values
            .iter()
            .flatten()
            .any(|v| (v - v.round()).abs() > 1e-6);
        assert!(
            fractional,
            "bilinear render must blend adjacent rays into fractional values"
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
            None,
        ) {
            Err(DataServerError::InvalidParameter(_)) => {}
            Err(other) => panic!("expected InvalidParameter, got {other:?}"),
            Ok(_) => panic!("expected an error for an absent quantity"),
        }
    }

    /// `site_coverage_bbox` brackets the centre and widens with radius.
    #[test]
    fn site_coverage_bbox_brackets_centre() {
        let [w, s, e, n] = site_coverage_bbox(25.0, 60.0, 250_000.0);
        assert!(w < 25.0 && e > 25.0 && s < 60.0 && n > 60.0);
        // ~250 km ≈ 2.25° of latitude.
        assert!((n - s - 4.5).abs() < 0.5, "lat span ≈ 4.5°, got {}", n - s);
    }

    // -----------------------------------------------------------------
    // build_source — source selection + config validation
    // -----------------------------------------------------------------

    /// Minimal `OdimConfig` with every field defaulted; tests override
    /// only the fields they exercise.
    fn empty_config() -> ds_core::config::OdimConfig {
        ds_core::config::OdimConfig {
            filename_template: None,
            filename_pattern: None,
            timestamp_format: None,
            parameter: None,
            unit: None,
            nodata: None,
            gain: None,
            offset: None,
            poll_interval_secs: 30,
            max_files: None,
            endpoint: None,
            bucket: None,
            prefix_pattern: None,
            time_window: None,
        }
    }

    /// With no `endpoint`/`bucket`, `build_source` selects a local
    /// directory source from `data_path`.
    #[test]
    fn build_source_selects_local_when_no_s3_fields() {
        let config = empty_config();
        match build_source("c", Some("/some/dir"), &config).unwrap() {
            Source::Local { data_dir } => assert_eq!(data_dir, PathBuf::from("/some/dir")),
            Source::Remote { .. } => panic!("expected a Local source"),
        }
    }

    /// A missing `data_path` with no S3 fields is `NoSource`.
    #[test]
    fn build_source_no_path_no_s3_is_no_source() {
        let config = empty_config();
        assert!(matches!(
            build_source("c", None, &config),
            Err(EngineError::NoSource)
        ));
    }

    /// `endpoint` + `bucket` + `prefix_pattern` selects a remote source.
    #[test]
    fn build_source_selects_remote_with_full_s3_config() {
        let mut config = empty_config();
        config.endpoint = Some("https://s3-eu-west-1.amazonaws.com".into());
        config.bucket = Some("fmi-opendata-radar-volume-hdf5".into());
        config.prefix_pattern = Some("%Y/%m/%d/fivih/".into());
        config.time_window = Some("-PT3H".into());
        match build_source("c", None, &config).unwrap() {
            Source::Remote {
                bucket,
                prefix_pattern,
                time_window,
                ..
            } => {
                assert_eq!(bucket, "fmi-opendata-radar-volume-hdf5");
                assert_eq!(prefix_pattern, "%Y/%m/%d/fivih/");
                assert!(time_window.is_some());
            }
            Source::Local { .. } => panic!("expected a Remote source"),
        }
    }

    /// An S3 source without `prefix_pattern` is rejected — it would
    /// otherwise list the whole bucket on every poll.
    #[test]
    fn build_source_s3_without_prefix_pattern_errors() {
        let mut config = empty_config();
        config.endpoint = Some("https://s3-eu-west-1.amazonaws.com".into());
        config.bucket = Some("fmi-opendata-radar-volume-hdf5".into());
        assert!(matches!(
            build_source("c", None, &config),
            Err(EngineError::MissingPrefixPattern)
        ));
    }

    /// Setting exactly one of `endpoint` / `bucket` is a config error,
    /// not a silent fallback to the local source.
    #[test]
    fn build_source_endpoint_xor_bucket_errors() {
        let mut endpoint_only = empty_config();
        endpoint_only.endpoint = Some("https://s3.example.com".into());
        assert!(matches!(
            build_source("c", Some("/dir"), &endpoint_only),
            Err(EngineError::IncompleteS3Config)
        ));

        let mut bucket_only = empty_config();
        bucket_only.bucket = Some("some-bucket".into());
        assert!(matches!(
            build_source("c", Some("/dir"), &bucket_only),
            Err(EngineError::IncompleteS3Config)
        ));
    }

    /// The pre-fetch window filter hinges on reading a timestamp from
    /// the object key — for both the FMI (timestamp-leading) and DMI
    /// (timestamp-trailing) filename layouts.
    #[test]
    fn parse_key_timestamp_handles_fmi_and_dmi_layouts() {
        // FMI: timestamp leads the basename.
        let fmi = parse_key_timestamp("2026/05/15/fivih/202605150000_fivih_PVOL.h5")
            .expect("FMI key timestamp");
        assert_eq!(fmi.to_rfc3339(), "2026-05-15T00:00:00+00:00");

        // DMI: timestamp follows the station code.
        let dmi = parse_key_timestamp("dkste_202512150405.vol.h5").expect("DMI key timestamp");
        assert_eq!(dmi.to_rfc3339(), "2025-12-15T04:05:00+00:00");

        // No ≥12-digit run, or an unparseable stamp → None (the file
        // then falls through to the post-parse window filter).
        assert!(parse_key_timestamp("radar_volume.h5").is_none());
        assert!(parse_key_timestamp("999999999999_x.h5").is_none());
    }

    // --- EdrEngine helpers -------------------------------------------------

    /// Wrap a synthetic volume in a `VolumeEntry` for catalog tests.
    fn entry(volume: PolarVolume, id: &str) -> VolumeEntry {
        VolumeEntry {
            id: id.to_string(),
            volume: Arc::new(volume),
        }
    }

    /// With a pinned `z` level, `site_coverages` yields one `PointSeries`
    /// at the queried point, with one time-aligned range per quantity.
    #[test]
    fn site_coverages_level_yields_point_series() {
        let (site_lon, site_lat) = (25.0, 60.0);
        let volumes = vec![entry(synthetic_volume(site_lon, site_lat), "v0")];
        // 10.5 km due east — safely mid-bin 10 of the 1 km-spaced sweep,
        // so the sampled bin index is robust against rounding.
        let dlon = 10_500.0 / (EARTH_RADIUS_M * site_lat.to_radians().cos()) * 180.0
            / std::f64::consts::PI;
        let covs = site_coverages(
            &volumes,
            site_lon + dlon,
            site_lat,
            None,
            None,
            Some(&[0.5]),
        )
        .unwrap();
        assert_eq!(covs.len(), 1);
        match &covs[0].domain {
            DomainDescription::PointSeries { x, y, t, z } => {
                assert!((x - (site_lon + dlon)).abs() < 1e-9);
                assert!((y - site_lat).abs() < 1e-9);
                assert_eq!(t.len(), 1);
                assert_eq!(z.as_ref().expect("z axis").values, vec![0.5]);
            }
            _ => panic!("expected PointSeries"),
        }
        let dbzh = covs[0].ranges.get("DBZH").expect("DBZH range");
        assert_eq!(dbzh.shape, vec![1]);
        assert_eq!(dbzh.axis_names, vec!["t".to_string()]);
        // raw[ray][bin] = bin, gain 1 → the sampled value is the bin
        // index, i.e. ground distance / 1 km.
        assert_eq!(dbzh.values[0], Some(10.0));
        assert!(covs[0].parameters.contains_key("DBZH"));
    }

    /// With no `z`, `site_coverages` yields one `VerticalProfile` per
    /// timestep, with the sweep angles as the `z` axis.
    #[test]
    fn site_coverages_no_z_yields_vertical_profile() {
        let (site_lon, site_lat) = (25.0, 60.0);
        let volumes = vec![entry(synthetic_volume(site_lon, site_lat), "v0")];
        let covs = site_coverages(&volumes, site_lon, site_lat, None, None, None).unwrap();
        assert_eq!(covs.len(), 1);
        match &covs[0].domain {
            DomainDescription::VerticalProfile { z, .. } => {
                assert_eq!(z.values, vec![0.5]);
            }
            _ => panic!("expected VerticalProfile"),
        }
        assert_eq!(
            covs[0].ranges.get("DBZH").expect("DBZH range").shape,
            vec![1]
        );
    }

    /// A radar running split cuts has two sweeps at the same nominal
    /// elevation angle. The `VerticalProfile` `z` axis must collapse
    /// them to distinct values — CoverageJSON axis values are
    /// `uniqueItems` — and the per-quantity range must shrink to match,
    /// sampling each angle from whichever sweep carries the quantity.
    #[test]
    fn volume_profile_dedups_split_cut_elevation_angles() {
        let (site_lon, site_lat) = (25.0, 60.0);
        // A real FMI scan strategy: two sweeps at 2.0° (a surveillance
        // cut carrying DBZH, a Doppler cut carrying VRADH).
        let mut vol = synthetic_volume(site_lon, site_lat);
        let make_sweep = |elangle: f64, quantity: &str| {
            let mut s = vol.sweeps[0].clone();
            s.elangle = elangle;
            s.moments[0].quantity = quantity.to_string();
            s
        };
        vol.sweeps = vec![
            make_sweep(0.5, "DBZH"),
            make_sweep(2.0, "DBZH"),
            make_sweep(2.0, "VRADH"),
            make_sweep(5.0, "DBZH"),
        ];

        // Sample a point ~2.8 km from the site, well within every sweep's
        // range, querying both quantities at once.
        let profile = volume_profile(
            &entry(vol, "v0"),
            site_lon + 0.05,
            site_lat,
            &["DBZH".to_string(), "VRADH".to_string()],
        )
        .expect("finite sweeps must produce a coverage");
        match &profile.domain {
            DomainDescription::VerticalProfile { z, .. } => {
                assert_eq!(z.values, vec![0.5, 2.0, 5.0], "z axis must be deduped");
            }
            _ => panic!("expected VerticalProfile"),
        }

        // DBZH lives on a cut at every angle → a sample at each z level.
        let dbzh = profile.ranges.get("DBZH").expect("DBZH range");
        assert_eq!(
            dbzh.shape,
            vec![3],
            "range shape follows the deduped z axis"
        );
        assert!(
            dbzh.values.iter().all(Option::is_some),
            "DBZH sampled at every level, got {:?}",
            dbzh.values
        );

        // VRADH lives only on the Doppler cut at 2.0°. Even though a DBZH cut
        // shares that angle, the per-angle search must pick the VRADH-carrying
        // sibling — so only the z=2.0 entry is populated, proving split-cut
        // selection is per-quantity (not "first sweep at the angle wins").
        let vradh = profile.ranges.get("VRADH").expect("VRADH range");
        assert_eq!(vradh.shape, vec![3]);
        assert!(vradh.values[0].is_none(), "no VRADH cut at 0.5°");
        assert!(
            vradh.values[1].is_some(),
            "VRADH cut at 2.0° must be sampled"
        );
        assert!(vradh.values[2].is_none(), "no VRADH cut at 5.0°");
    }

    /// A malformed sweep with a NaN elevation must be dropped from the z
    /// axis — a NaN there serialises to JSON `null` and breaks CoverageJSON's
    /// numeric axis.
    #[test]
    fn volume_profile_drops_nan_elevation() {
        let (site_lon, site_lat) = (25.0, 60.0);
        let mut vol = synthetic_volume(site_lon, site_lat);
        let mut nan_sweep = vol.sweeps[0].clone();
        nan_sweep.elangle = f64::NAN;
        vol.sweeps.push(nan_sweep);

        let profile = volume_profile(&entry(vol, "v0"), site_lon, site_lat, &["DBZH".to_string()])
            .expect("one finite sweep remains, so a coverage is produced");
        match &profile.domain {
            DomainDescription::VerticalProfile { z, .. } => {
                assert!(
                    z.values.iter().all(|v| v.is_finite()),
                    "z axis must contain no NaN, got {:?}",
                    z.values
                );
                assert_eq!(z.values, vec![0.5], "only the finite sweep survives");
            }
            _ => panic!("expected VerticalProfile"),
        }
    }

    /// A volume where *every* sweep has a non-finite elevation angle
    /// (severely malformed) returns no coverage rather than emitting a
    /// `VerticalProfile` with an empty z axis (`numericValuesAxis.values`
    /// has `minItems: 1`).
    #[test]
    fn volume_profile_all_nan_returns_none() {
        let (site_lon, site_lat) = (25.0, 60.0);
        let mut vol = synthetic_volume(site_lon, site_lat);
        for s in &mut vol.sweeps {
            s.elangle = f64::NAN;
        }
        let result = volume_profile(&entry(vol, "v0"), site_lon, site_lat, &["DBZH".to_string()]);
        assert!(result.is_none(), "expected None, got {result:?}");
    }

    /// An out-of-window `datetime` range yields `LocationNotFound`.
    #[test]
    fn site_coverages_empty_window_errors() {
        let volumes = vec![entry(synthetic_volume(25.0, 60.0), "v0")];
        let past = DateTime::parse_from_rfc3339("2000-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        match site_coverages(&volumes, 25.0, 60.0, Some((past, past)), None, None) {
            Err(DataServerError::LocationNotFound(_)) => {}
            other => panic!("expected LocationNotFound, got {other:?}"),
        }
    }

    /// A `parameters` filter naming no real quantity is rejected.
    #[test]
    fn site_coverages_unknown_parameter_errors() {
        let volumes = vec![entry(synthetic_volume(25.0, 60.0), "v0")];
        let filter = ["NONESUCH".to_string()];
        match site_coverages(&volumes, 25.0, 60.0, None, Some(&filter), None) {
            Err(DataServerError::InvalidParameter(_)) => {}
            other => panic!("expected InvalidParameter, got {other:?}"),
        }
    }

    /// Build a [`PolarVolumeSiteView`] over a synthetic catalog scoped to
    /// `nod` — mirrors what `PolarVolumeEngine::site_view` produces, but
    /// without a real source scan.
    fn site_view_for(by_site: HashMap<String, Vec<VolumeEntry>>, nod: &str) -> PolarVolumeSiteView {
        let cache = Mutex::new(HashMap::new());
        let catalog = derive_catalog(by_site, &cache, None);
        PolarVolumeSiteView {
            catalog: Arc::new(ArcSwap::from_pointee(catalog)),
            nod: nod.to_string(),
            collection_id: format!("test-{nod}"),
        }
    }

    /// Model B: a per-site view advertises **bare** quantities (no
    /// `<nod>:` prefix), a single EDR location (the antenna), and the
    /// site's own coverage extent — even when the catalog holds other
    /// sites.
    #[test]
    fn site_view_advertises_bare_quantities_and_single_location() {
        let mut by_site: HashMap<String, Vec<VolumeEntry>> = HashMap::new();
        by_site.insert(
            "fivih".to_string(),
            vec![entry(synthetic_volume(25.0, 60.0), "v")],
        );
        by_site.insert(
            "fianj".to_string(),
            vec![entry(synthetic_volume(27.0, 60.9), "a")],
        );
        let view = site_view_for(by_site, "fivih");

        // Map/WMS parameters are the bare quantity — no `fivih:` prefix.
        let info = MapEngine::raster_info(&view);
        let names: Vec<&str> = info.parameters.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec!["DBZH"],
            "per-site layer must be the bare quantity, got {names:?}"
        );
        assert!(
            info.spatial_extent.is_some(),
            "a per-site view reports its own coverage bbox"
        );
        assert!(
            info.vertical.is_some(),
            "a per-site view reports its own elevation axis"
        );

        // EDR parameter list is bare too.
        assert_eq!(
            EdrEngine::get_parameters(&view),
            vec!["DBZH".to_string()],
            "EDR parameter list is the bare quantity"
        );

        // Exactly one EDR location — this radar.
        let locs = EdrEngine::get_locations(&view).expect("locations");
        assert_eq!(locs.len(), 1, "a per-site collection has one location");
        assert_eq!(locs[0].id, "fivih");
        assert_eq!(locs[0].label, "Test Site");
    }

    /// The per-site view renders with a bare-quantity parameter, and
    /// errors cleanly (not a panic) on a missing parameter or a site that
    /// has no current volumes.
    #[test]
    fn site_view_get_raster_tile_uses_bare_quantity() {
        let mut by_site: HashMap<String, Vec<VolumeEntry>> = HashMap::new();
        by_site.insert(
            "fivih".to_string(),
            vec![entry(synthetic_volume(25.0, 60.0), "v")],
        );
        let view = site_view_for(by_site, "fivih");

        let dlon =
            50_000.0 / (EARTH_RADIUS_M * 60f64.to_radians().cos()) * 180.0 / std::f64::consts::PI;
        let bbox = [25.0, 59.999, 25.0 + dlon, 60.001];
        let tile = MapEngine::get_raster_tile(
            &view,
            bbox,
            32,
            4,
            None,
            &OutputCrs::Wgs84,
            Some("DBZH"),
            None,
        )
        .expect("render with a bare quantity");
        assert_eq!(tile.values.len(), 32 * 4);
        assert!(
            tile.values.iter().any(Option::is_some),
            "a render over the radar's own coverage samples some echoes"
        );

        // Missing parameter is a clean 400. (`RasterTile` is not `Debug`,
        // so assert on the variant rather than `unwrap_err`.)
        assert!(matches!(
            MapEngine::get_raster_tile(&view, bbox, 4, 4, None, &OutputCrs::Wgs84, None, None),
            Err(DataServerError::InvalidParameter(_))
        ));

        // A view over a site absent from the catalog reports no data, not
        // a 500.
        let ghost = site_view_for(HashMap::new(), "ghost");
        assert!(matches!(
            MapEngine::get_raster_tile(
                &ghost,
                bbox,
                4,
                4,
                None,
                &OutputCrs::Wgs84,
                Some("DBZH"),
                None,
            ),
            Err(DataServerError::LocationNotFound(_))
        ));
    }
}
