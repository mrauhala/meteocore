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
use ds_core::feature::{parse_area_coords, parse_point_coords};
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

/// Separator between the ODIM node id and the radar quantity in an
/// advertised parameter name (`fianj:DBZH`). See the module docs for
/// why `:` and not `/`.
pub const SITE_QUANTITY_SEP: char = ':';

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

/// The engine's catalog: per-site time-sorted volume lists, plus the
/// derived metadata `raster_info()` answers from.
struct Catalog {
    /// Volumes grouped by `site.nod`, each list sorted by `time`
    /// ascending.
    by_site: HashMap<String, Vec<VolumeEntry>>,
    /// `(parameter_name, title)` pairs — one per `<nod>:<quantity>`
    /// from each site's lowest sweep. Sorted for stable output. This is
    /// the Map/WMS layer list (site-scoped).
    parameters: Vec<(String, String)>,
    /// Distinct ODIM quantity names across every site's lowest sweep,
    /// sorted — the EDR parameter list. Unlike `parameters` these carry
    /// no `<site>:` prefix: in EDR the site is the *location* and the
    /// quantity is the *parameter*.
    quantities: Vec<String>,
    /// All distinct volume times across every site, sorted ascending.
    times: Vec<DateTime<Utc>>,
    /// Union of per-site coverage bboxes `[w, s, e, n]` in WGS84.
    spatial_extent: Option<[f64; 4]>,
    /// The collection's vertical axis — elevation angle (degrees), the
    /// union of every site's sweep angles. `None` until the catalog has
    /// at least one volume.
    vertical: Option<VerticalDimension>,
}

/// Round an elevation angle to 0.1° so near-identical sweep angles from
/// different sites or volumes collapse to one catalogue level.
fn round_elevation(deg: f64) -> f64 {
    (deg * 10.0).round() / 10.0
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

/// Finalise the grouped volume map into a [`Catalog`]: sort and cap
/// each site's list, evict stale parse-cache entries, and derive the
/// parameter list, temporal extent, and spatial extent.
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

    // Derive parameters (Map layers) and quantities (EDR parameters)
    // from each site's lowest sweep.
    let mut parameters: Vec<(String, String)> = Vec::new();
    let mut quantities: Vec<String> = Vec::new();
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
            quantities.push(moment.quantity.clone());
        }
    }
    parameters.sort_by(|a, b| a.0.cmp(&b.0));
    // A producer shipping a sweep with a duplicate quantity name would
    // otherwise advertise the same `<nod>:<quantity>` layer twice.
    parameters.dedup_by(|a, b| a.0 == b.0);
    // Quantities recur across sites — collapse to the distinct set.
    quantities.sort();
    quantities.dedup();

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

    // Vertical axis: the union of every site's sweep elevation angles
    // (from each site's most-recent volume), rounded and sorted ascending.
    let mut levels: Vec<f64> = by_site
        .values()
        .filter_map(|list| list.last())
        .flat_map(|e| e.volume.sweeps.iter().map(|s| round_elevation(s.elangle)))
        .collect();
    levels.sort_by(f64::total_cmp);
    levels.dedup();
    let vertical =
        (!levels.is_empty()).then(|| VerticalDimension::new(VerticalKind::ElevationAngle, levels));

    Catalog {
        by_site,
        parameters,
        quantities,
        times,
        spatial_extent,
        vertical,
    }
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
                "[{collection_id}] PVOL catalog: {} site(s), {} parameter(s), {} time(s)",
                catalog.by_site.len(),
                catalog.parameters.len(),
                catalog.times.len()
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
/// 2. maps distance to a range bin (ground range — see below);
/// 3. maps azimuth to a stored ray index;
/// 4. samples the raw moment array and applies gain/offset/nodata.
///
/// **Ground-range interim.** The sweep range axis is treated as ground
/// range: `bin = floor((d - rstart) / rscale)`. A proper slant-range /
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

            values.push(sample_sweep_moment(
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
        z: Option<f64>,
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

        polar_sample(&entry.volume, quantity, bbox, width, height, output_crs, z)
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
            times: catalog.times.clone(),
            parameter,
            // PVOL quantities span multiple physical units (dBZ, m/s,
            // dB, …); the per-layer unit is not a single collection
            // constant, so leave it blank.
            unit: String::new(),
            parameters,
            vertical: catalog.vertical.clone(),
            // Polar volume — no regular CRS84 cell grid, so the spatial grid
            // resolution is not meaningful here.
            grid_size: None,
        }
    }
}

// ---------------------------------------------------------------------------
// EdrEngine
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

/// NOD code of the radar site nearest to `(lon, lat)` by ground
/// distance. `None` only when the catalog holds no sites.
fn nearest_site(catalog: &Catalog, lon: f64, lat: f64) -> Option<&str> {
    catalog
        .by_site
        .iter()
        .filter_map(|(nod, list)| {
            let site = &list.last()?.volume.site;
            let (dist, _) = ground_distance_bearing(site.lon, site.lat, lon, lat);
            Some((nod.as_str(), dist))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(nod, _)| nod)
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
fn volume_profile(entry: &VolumeEntry, lon: f64, lat: f64, quantities: &[String]) -> QueryResult {
    let levels: Vec<f64> = entry
        .volume
        .sweeps
        .iter()
        .map(|s| round_elevation(s.elangle))
        .collect();
    let (site_lon, site_lat) = (entry.volume.site.lon, entry.volume.site.lat);

    let mut ranges = HashMap::new();
    let mut param_descs = HashMap::new();
    for quantity in quantities {
        let values: Vec<Option<f64>> = entry
            .volume
            .sweeps
            .iter()
            .map(|sweep| {
                let moment = sweep.moments.iter().find(|m| &m.quantity == quantity)?;
                sample_sweep_moment(sweep, moment, site_lon, site_lat, lon, lat)
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

    QueryResult {
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
    }
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
            .map(|e| volume_profile(e, lon, lat, &quantities))
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

/// Resolve a requested EDR `z` selector against the catalog's vertical
/// axis. `None` (or an empty selector) means "every level — a profile".
fn resolve_levels(
    catalog: &Catalog,
    z: Option<&[f64]>,
) -> Result<Option<Vec<f64>>, DataServerError> {
    let Some(zs) = z.filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let canonical = catalog
        .vertical
        .as_ref()
        .map(|v| v.levels.as_slice())
        .ok_or_else(|| {
            DataServerError::InvalidParameter(
                "this PVOL collection has no elevation sweeps to select with `z`".into(),
            )
        })?;
    let snapped = snap_levels(zs, canonical);
    Ok((!snapped.is_empty()).then_some(snapped))
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

impl EdrEngine for PolarVolumeEngine {
    /// One EDR location per radar site — `id` is the ODIM NOD code and
    /// the point geometry is the antenna position.
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        let catalog = self.catalog.load();
        let mut locations: Vec<Location> = catalog
            .by_site
            .iter()
            .filter_map(|(nod, list)| {
                let site = &list.last()?.volume.site;
                Some(Location {
                    id: nod.clone(),
                    label: site.plc.clone().unwrap_or_else(|| nod.clone()),
                    latitude: site.lat,
                    longitude: site.lon,
                })
            })
            .collect();
        locations.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(locations)
    }

    /// Query one radar site by NOD code. With no `z` the result is a
    /// `CoverageCollection` of `VerticalProfile`s (one per timestep);
    /// with `z` it is a `PointSeries` per selected elevation sweep.
    fn query_location(
        &self,
        location_id: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
    ) -> Result<CoverageResponse, DataServerError> {
        let catalog = self.catalog.load();
        let volumes = catalog
            .by_site
            .get(location_id)
            .ok_or_else(|| DataServerError::LocationNotFound(location_id.to_string()))?;
        // The location *is* the radar site, so the coverage point is
        // the antenna itself.
        let site = &volumes
            .last()
            .ok_or_else(|| DataServerError::LocationNotFound(location_id.to_string()))?
            .volume
            .site;
        let levels = resolve_levels(&catalog, z)?;
        let covs = site_coverages(
            volumes,
            site.lon,
            site.lat,
            datetime,
            parameters,
            levels.as_deref(),
        )?;
        finalize_single_site(covs, &levels)
    }

    fn get_parameters(&self) -> Vec<String> {
        self.catalog.load().quantities.clone()
    }

    fn get_vertical_extent(&self) -> Option<VerticalDimension> {
        self.catalog.load().vertical.clone()
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        let catalog = self.catalog.load();
        Some((*catalog.times.first()?, *catalog.times.last()?))
    }

    /// PVOL volumes arrive at discrete (typically 5-min) steps, so
    /// advertise the exact timestamps rather than just the interval —
    /// same rationale as the COMP engine.
    fn get_available_times(&self) -> Option<Vec<DateTime<Utc>>> {
        let times = self.catalog.load().times.clone();
        (!times.is_empty()).then_some(times)
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        self.catalog.load().spatial_extent
    }

    fn supported_query_types(&self) -> Vec<String> {
        vec![
            "locations".to_string(),
            "position".to_string(),
            "area".to_string(),
        ]
    }

    /// Position query against the radar site nearest the requested
    /// point — same `z`-driven shape as [`query_location`].
    fn query_position(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
    ) -> Result<CoverageResponse, DataServerError> {
        let (lat, lon) = parse_point_coords(coords)?;
        let catalog = self.catalog.load();
        let nod = nearest_site(&catalog, lon, lat).ok_or_else(|| {
            DataServerError::LocationNotFound("PVOL catalog has no radar sites".into())
        })?;
        // `nearest_site` returns a key it read from `by_site`, so the
        // lookup cannot miss — but a graceful error beats a panicking
        // request thread should a refactor ever break that invariant.
        let volumes = catalog.by_site.get(nod).ok_or_else(|| {
            DataServerError::Engine("internal: nearest_site returned a missing by_site key".into())
        })?;
        let levels = resolve_levels(&catalog, z)?;
        let covs = site_coverages(volumes, lon, lat, datetime, parameters, levels.as_deref())?;
        finalize_single_site(covs, &levels)
    }

    /// Area query — a `CoverageCollection` flattening every in-polygon
    /// radar site's coverages (a `VerticalProfile` per timestep with no
    /// `z`, a `PointSeries` per level with `z`).
    fn query_area(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
    ) -> Result<CoverageResponse, DataServerError> {
        let polygon = parse_area_coords(coords)?;
        let catalog = self.catalog.load();
        let levels = resolve_levels(&catalog, z)?;

        // Stable, sorted iteration so the collection order is
        // deterministic across requests.
        let mut nods: Vec<&String> = catalog.by_site.keys().collect();
        nods.sort();

        let mut coverages = Vec::new();
        for nod in nods {
            let volumes = &catalog.by_site[nod];
            let Some(site) = volumes.last().map(|e| &e.volume.site) else {
                continue;
            };
            if !polygon.contains(site.lon, site.lat) {
                continue;
            }
            match site_coverages(
                volumes,
                site.lon,
                site.lat,
                datetime,
                parameters,
                levels.as_deref(),
            ) {
                Ok(covs) => coverages.extend(covs),
                // A site inside the polygon is skipped — not fatal to
                // the whole area query — when it has no data in the
                // requested window (`LocationNotFound`) or none of the
                // requested quantities (`InvalidParameter`): a
                // heterogeneous network mixes single- and dual-pol
                // radars, so a `ZDR` filter legitimately misses some
                // sites while matching others.
                Err(
                    DataServerError::LocationNotFound(_) | DataServerError::InvalidParameter(_),
                ) => continue,
                Err(e) => return Err(e),
            }
        }

        if coverages.is_empty() {
            return Err(DataServerError::LocationNotFound(
                "No radar sites with data found within the requested area".into(),
            ));
        }
        Ok(CoverageResponse::Collection(coverages))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pvol::RadarSite;
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

    /// `nearest_site` picks the site with the smallest ground distance.
    #[test]
    fn nearest_site_picks_closest() {
        let mut by_site: HashMap<String, Vec<VolumeEntry>> = HashMap::new();
        by_site.insert(
            "west".to_string(),
            vec![entry(synthetic_volume(20.0, 60.0), "w")],
        );
        by_site.insert(
            "east".to_string(),
            vec![entry(synthetic_volume(30.0, 60.0), "e")],
        );
        let catalog = Catalog {
            by_site,
            parameters: vec![],
            quantities: vec![],
            times: vec![],
            spatial_extent: None,
            vertical: None,
        };
        assert_eq!(nearest_site(&catalog, 29.0, 60.0), Some("east"));
        assert_eq!(nearest_site(&catalog, 21.0, 60.0), Some("west"));
    }

    /// An empty catalog has no nearest site.
    #[test]
    fn nearest_site_empty_catalog_is_none() {
        let catalog = Catalog {
            by_site: HashMap::new(),
            parameters: vec![],
            quantities: vec![],
            times: vec![],
            spatial_extent: None,
            vertical: None,
        };
        assert_eq!(nearest_site(&catalog, 25.0, 60.0), None);
    }
}
