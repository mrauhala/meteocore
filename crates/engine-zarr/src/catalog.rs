//! The Zarr catalog: a parsed snapshot of a store's data variables and shared
//! geographic/temporal axes, plus the point-sampling read path.
//!
//! A catalog is built once at engine construction and rebuilt by the poll loop;
//! it is swapped atomically via `ArcSwap`, so EDR queries read a consistent
//! snapshot without locking.

use std::collections::BTreeMap;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use zarrs::array::{
    data_type, Array, ArrayBytes, ArrayError, ArrayShardedExt, ArraySubset, CodecError,
    CodecOptions, FromArrayBytes,
};
use zarrs::group::Group;
use zarrs::storage::StorageError;

use ds_core::error::DataServerError;
use ds_core::instances;
use ds_core::map_engine::RasterInfo;

use crate::cf::{self, AxisRole};
use crate::decoded::{DecodedArray, DecodedCache};
use crate::read_budget::{self, Budget, Permit};
use crate::store::EngineStore;

/// The store type backing every Zarr collection. A backend-agnostic wrapper so
/// the catalog stays non-generic across the plain (`ds-storage`) and Icechunk
/// backends (#125 Phase 2, #335).
type Store = EngineStore;

/// Codec options that pin chunk retrieval to the **calling thread** by setting
/// both the concurrency target and minimum chunk concurrency to 1. This stops
/// zarrs from dispatching storage reads onto `rayon` workers, which lose the calling
/// thread's deadline and runtime context. Icechunk's outer fan-out explicitly
/// propagates deadlines and reserves each active workspace; every individual
/// chunk job still uses these serial codec options.
pub(crate) fn single_threaded_opts() -> CodecOptions {
    CodecOptions::default()
        .with_concurrent_target(1)
        .with_chunk_concurrent_minimum(1)
}

/// A single exposed data variable (one EDR/Map parameter).
pub struct Variable {
    pub name: String,
    /// Opened zarr array handle, used for on-demand chunk reads.
    array: Array<Store>,
    decoded: Option<DecodedArray>,
    pub units: String,
    pub label: String,
    /// Axis index of the time dimension within this variable's dim order. For a
    /// forecast (reference + lead), this is the **lead** axis.
    pub time_axis: Option<usize>,
    /// For a forecast, the axis index of the **reference time** (model run).
    /// Reads take the resolved run ([`Catalog::resolve_run`]) as a parameter.
    ref_axis: Option<usize>,
    /// Axis index of the latitude dimension.
    lat_axis: usize,
    /// Axis index of the longitude dimension.
    lon_axis: usize,
    ndim: usize,
    /// CF packing: physical = raw * scale_factor + add_offset.
    scale_factor: f64,
    add_offset: f64,
    /// CF missing-value sentinels (`_FillValue`, `missing_value`), compared
    /// against the *raw* (pre-scale) value.
    fill_values: Vec<f64>,
}

impl Variable {
    /// Convert a raw stored sample to a physical value, mapping fill sentinels
    /// and non-finite values to `None`.
    fn convert(&self, raw: f64) -> Option<f64> {
        convert_sample(raw, self.scale_factor, self.add_offset, &self.fill_values)
    }
}

/// Map a raw sample to a physical value: NaN / ±infinity and fill sentinels
/// become `None` (an infinite sample would otherwise scale to ±inf and break
/// CoverageJSON serialisation); everything else gets CF `scale`/`offset`.
fn convert_sample(raw: f64, scale: f64, offset: f64, fills: &[f64]) -> Option<f64> {
    if !raw.is_finite() {
        return None;
    }
    // `raw` is finite here, so a NaN/inf fill can't match it; a plain equality
    // against the (finite) sentinels is enough.
    if fills.contains(&raw) {
        return None;
    }
    let value = raw * scale + offset;
    value.is_finite().then_some(value)
}

/// A parsed Zarr store snapshot.
pub struct Catalog {
    read_budget: Arc<Budget>,
    /// Pinned Icechunk snapshot; absent for an unversioned plain Zarr store.
    pub revision: Option<String>,
    pub content_version: u64,
    /// Data variables in stable (sorted) order.
    pub vars: Vec<Variable>,
    /// Decoded time axis (ascending), shared across all variables. For a
    /// forecast this is the **latest run's** valid times (run + leads); other
    /// runs' valid times come from [`Self::valid_times`].
    pub times: Vec<DateTime<Utc>>,
    /// Forecast model runs: reference time → index on the reference axis
    /// (the `ds_core::instances` contract, #337). Empty for a non-forecast
    /// store.
    pub runs: BTreeMap<DateTime<Utc>, usize>,
    /// Decoded reference axis in axis order (forecast only).
    ref_times: Vec<DateTime<Utc>>,
    /// Each run's valid times (run + leads), by reference-axis index —
    /// built once here so [`Self::valid_times`] is an O(1) borrow per
    /// request (Critical Rule 10). Empty for a non-forecast store.
    run_valid_times: Vec<Vec<DateTime<Utc>>>,
    /// Latitude axis values (degrees north; may be ascending or descending).
    lats: Vec<f64>,
    /// Longitude axis values (degrees east).
    lons: Vec<f64>,
    /// Spatial extent `[west, south, east, north]` in WGS84 degrees.
    pub extent: [f64; 4],
    /// Map-capabilities snapshot, built once here so `raster_info()` is O(1) and
    /// swaps **atomically** with the data (one `ArcSwap<Catalog>`), with no
    /// window where the advertised metadata and the served data disagree.
    pub raster_info: RasterInfo,
}

impl Catalog {
    /// Resolve a requested model run to its reference-axis index: `None` ⇒
    /// the latest run, `Some(rt)` ⇒ that exact run or `ReferenceTimeNotFound`.
    /// A non-forecast store has no runs and answers `Ok(None)` for any
    /// request (the shared accept-and-ignore contract).
    pub fn resolve_run(
        &self,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<Option<usize>, DataServerError> {
        if self.runs.is_empty() {
            return Ok(None);
        }
        // The shared selection rule (`None` ⇒ latest, `Some` ⇒ exact) — the
        // same call GRIB and QueryData make, so "latest" cannot drift from
        // what `/instances` lists last.
        match instances::select_run(&self.runs, reference_time) {
            Some((_, &idx)) => Ok(Some(idx)),
            None => Err(DataServerError::ReferenceTimeNotFound(format!(
                "no model run for reference time {}",
                reference_time.expect("None resolves to the latest run")
            ))),
        }
    }

    /// The valid times of `run` (a reference-axis index from
    /// [`Self::resolve_run`]); the shared [`Self::times`] otherwise. A
    /// borrow of a per-run list built at catalog build — no per-call work.
    pub fn valid_times(&self, run: Option<usize>) -> &[DateTime<Utc>] {
        match run.and_then(|r| self.run_valid_times.get(r)) {
            Some(v) => v,
            None => &self.times,
        }
    }

    /// The reference time of `run`, for the run-axis cache key.
    pub fn run_time(&self, run: Option<usize>) -> Option<DateTime<Utc>> {
        run.and_then(|r| self.ref_times.get(r).copied())
    }

    /// The reference-axis range a read of `run` selects. Only reached for a
    /// variable with a reference axis, whose caller must have resolved the
    /// run: a `None` here is a programming error, reported rather than
    /// silently defaulted.
    fn ref_range(run: Option<usize>) -> Result<Range<u64>, DataServerError> {
        let r = run.ok_or_else(|| {
            DataServerError::Engine("forecast variable read without a resolved run".into())
        })? as u64;
        Ok(r..r + 1)
    }

    /// Read a 2-D spatial slab of `var` at `time_idx` covering the WGS84 render
    /// `bbox` (`[west, south, east, north]`), expanded by one cell so edge
    /// pixels can interpolate. Returns `None` when the bbox lies entirely off
    /// the grid. Used by the Map/Tiles/WMS render path.
    pub fn read_window(
        &self,
        var: &Variable,
        run: Option<usize>,
        time_idx: usize,
        bbox: [f64; 4],
    ) -> Result<Option<Window>, DataServerError> {
        Ok(self
            .read_window_span(var, run, time_idx..time_idx + 1, bbox)?
            .and_then(|mut w| w.pop()))
    }

    /// Native cells `(ncols, nrows)` a [`Self::read_window_span`] of `bbox`
    /// would fetch (including the one-cell margin), or `None` off-grid —
    /// so a caller can budget the *read*, not just its output.
    pub fn window_dims(&self, bbox: [f64; 4]) -> Option<(usize, usize)> {
        let [west, south, east, north] = bbox;
        let (i0, i1) = axis_window(&self.lons, west, east)?;
        let (j0, j1) = axis_window(&self.lats, south, north)?;
        Some((i1 - i0 + 1, j1 - j0 + 1))
    }

    /// One subset retrieval across a contiguous span of timesteps, returning
    /// one window per step. The subset can still require many storage requests
    /// for its chunks/subchunks; it is not a single network round trip.
    pub fn read_window_span(
        &self,
        var: &Variable,
        run: Option<usize>,
        time_span: Range<usize>,
        bbox: [f64; 4],
    ) -> Result<Option<Vec<Window>>, DataServerError> {
        ds_core::deadline::check()?;
        let [west, south, east, north] = bbox;
        let (Some((i0, i1)), Some((j0, j1))) = (
            axis_window(&self.lons, west, east),
            axis_window(&self.lats, south, north),
        ) else {
            return Ok(None); // bbox entirely outside the grid
        };
        let nt = time_span.len().max(1);

        let mut ranges: Vec<Range<u64>> = Vec::with_capacity(var.ndim);
        for a in 0..var.ndim {
            if Some(a) == var.time_axis {
                ranges.push(time_span.start as u64..time_span.start as u64 + nt as u64);
            } else if a == var.lat_axis {
                ranges.push(j0 as u64..(j1 as u64) + 1);
            } else if a == var.lon_axis {
                ranges.push(i0 as u64..(i1 as u64) + 1);
            } else if Some(a) == var.ref_axis {
                ranges.push(Self::ref_range(run)?);
            } else {
                ranges.push(0..1);
            }
        }
        let subset = ArraySubset::new_with_ranges(&ranges);
        let axes_bytes = (i1 - i0 + 1)
            .checked_add(j1 - j0 + 1)
            .and_then(|n| n.checked_mul(size_of::<f64>()))
            .and_then(|n| n.checked_add(size_of::<Window>()))
            .and_then(|n| n.checked_mul(nt))
            .map(|n| n as u64);
        let reservation =
            self.read_budget
                .reserve(&var.array, &subset, axes_bytes, var.decoded.is_some())?;
        let raw = {
            let _encoded = crate::encoded::enter(Some(self.read_budget.clone()));
            retrieve_raw_f64(
                &var.array,
                &subset,
                var.decoded.as_ref(),
                reservation.parallelism(),
            )?
        };
        let lens: Vec<usize> = ranges.iter().map(|r| (r.end - r.start) as usize).collect();

        let nrow = j1 - j0 + 1;
        let ncol = i1 - i0 + 1;
        let mut lons = self.lons[i0..=i1].to_vec();
        let mut lats = self.lats[j0..=j1].to_vec();
        let mut windows = Vec::with_capacity(nt);
        for t in 0..nt {
            // The last (for a render tile: the only) window takes the axes by
            // move — no clone on the per-tile hot path.
            let (wl, wla) = if t + 1 == nt {
                (std::mem::take(&mut lons), std::mem::take(&mut lats))
            } else {
                (lons.clone(), lats.clone())
            };
            // Compact physical samples: NaN represents nodata. Do not build
            // a second Option<f64> buffer just to reorder the same subset.
            let mut data = vec![f64::NAN; nrow * ncol];
            for r in 0..nrow {
                ds_core::deadline::check()?;
                for c in 0..ncol {
                    let mut off = 0usize;
                    for (a, &len) in lens.iter().enumerate() {
                        let idx = if a == var.lat_axis {
                            r
                        } else if a == var.lon_axis {
                            c
                        } else if Some(a) == var.time_axis {
                            t
                        } else {
                            0 // pinned dims
                        };
                        off = off * len + idx;
                    }
                    data[r * ncol + c] = var.convert(raw[off]).unwrap_or(f64::NAN);
                }
            }
            windows.push(Window {
                data,
                lons: wl,
                lats: wla,
                _reservation: reservation.clone(),
            });
        }
        Ok(Some(windows))
    }

    /// Sample a variable's value at `(lon, lat)` for each requested time index,
    /// using bilinear interpolation over the surrounding grid cells (nearest
    /// fallback where a neighbour is nodata). Reads a single small hyperslab
    /// covering the 2×2 spatial neighbourhood across the requested time span.
    pub fn sample_series(
        &self,
        var: &Variable,
        run: Option<usize>,
        lon: f64,
        lat: f64,
        time_idx: &[usize],
    ) -> Result<Vec<Option<f64>>, DataServerError> {
        ds_core::deadline::check()?;
        let (xb, yb) = match (cf::locate(&self.lons, lon), cf::locate(&self.lats, lat)) {
            (Some(x), Some(y)) => (x, y),
            _ => return Ok(vec![None; time_idx.len()]), // off-grid → all nodata
        };
        let (i0, i1, wx) = xb;
        let (j0, j1, wy) = yb;

        // Build the read window: only the contiguous span of requested time
        // steps (not the whole axis — a single-step query on an 8760-step store
        // must not decode every time chunk), the 2×2 spatial block, and other
        // dims pinned to index 0.
        let (t_start, t_end) = match (
            time_idx.iter().copied().min(),
            time_idx.iter().copied().max(),
        ) {
            (Some(a), Some(b)) => (a, b),
            _ => return Ok(Vec::new()), // empty request (caller guards against this)
        };
        let mut ranges: Vec<Range<u64>> = Vec::with_capacity(var.ndim);
        for a in 0..var.ndim {
            if Some(a) == var.time_axis {
                ranges.push(t_start as u64..(t_end as u64) + 1);
            } else if a == var.lat_axis {
                ranges.push(j0 as u64..(j1 as u64) + 1);
            } else if a == var.lon_axis {
                ranges.push(i0 as u64..(i1 as u64) + 1);
            } else if Some(a) == var.ref_axis {
                ranges.push(Self::ref_range(run)?);
            } else {
                ranges.push(0..1);
            }
        }
        let subset = ArraySubset::new_with_ranges(&ranges);
        let output_bytes = time_idx
            .len()
            .checked_mul(size_of::<Option<f64>>())
            .map(|n| n as u64);
        let reservation =
            self.read_budget
                .reserve(&var.array, &subset, output_bytes, var.decoded.is_some())?;
        let raw = {
            let _encoded = crate::encoded::enter(Some(self.read_budget.clone()));
            retrieve_raw_f64(
                &var.array,
                &subset,
                var.decoded.as_ref(),
                reservation.parallelism(),
            )?
        };
        let lens: Vec<usize> = ranges.iter().map(|r| (r.end - r.start) as usize).collect();

        // Local corner offsets within the read window.
        let jc1 = j1 - j0; // 1, or 0 for a single-cell axis
        let ic1 = i1 - i0;

        let sample_at = |t_local: usize, jc: usize, ic: usize| -> Option<f64> {
            let mut off = 0usize;
            for (a, &len) in lens.iter().enumerate() {
                let pos = if Some(a) == var.time_axis {
                    t_local
                } else if a == var.lat_axis {
                    jc
                } else if a == var.lon_axis {
                    ic
                } else {
                    0
                };
                off = off * len + pos;
            }
            raw.get(off).and_then(|&r| var.convert(r))
        };

        let mut out = Vec::with_capacity(time_idx.len());
        for &ti in time_idx {
            ds_core::deadline::check()?;
            // `ti` indexes the global time axis; the read window starts at
            // `t_start`, so shift into local coordinates.
            let t_local = if var.time_axis.is_some() {
                ti - t_start
            } else {
                0
            };
            let v00 = sample_at(t_local, 0, 0);
            let v01 = sample_at(t_local, 0, ic1);
            let v10 = sample_at(t_local, jc1, 0);
            let v11 = sample_at(t_local, jc1, ic1);
            out.push(bilinear(v00, v01, v10, v11, wx, wy));
        }
        Ok(out)
    }
}

/// An in-memory spatial slab of one variable at one time, covering a render
/// bbox, with the slab's own (windowed) coordinate axes. Row-major, row `r` ↔
/// `lats[r]`, column `c` ↔ `lons[c]`.
pub struct Window {
    data: Vec<f64>,
    lons: Vec<f64>,
    lats: Vec<f64>,
    // Shared by a timestep span; held until its last sampling window is gone.
    _reservation: Arc<Permit>,
}

impl Window {
    pub fn ncols(&self) -> usize {
        self.lons.len()
    }

    pub fn nrows(&self) -> usize {
        self.lats.len()
    }

    /// Fractional window pixel `(col_f, row_f)` for a WGS84 `(lon, lat)`. An
    /// off-window coordinate yields a non-finite component, which
    /// [`Window::bilinear_at`] rejects.
    pub fn frac_px(&self, lon: f64, lat: f64) -> (f64, f64) {
        let fx = cf::locate(&self.lons, lon)
            .map(|(lo, _, w)| lo as f64 + w)
            .unwrap_or(f64::NAN);
        let fy = cf::locate(&self.lats, lat)
            .map(|(lo, _, w)| lo as f64 + w)
            .unwrap_or(f64::NAN);
        (fx, fy)
    }

    /// Bilinearly sample at a fractional window pixel; `None` off-window or where
    /// every neighbour is nodata.
    pub fn bilinear_at(&self, col_f: f64, row_f: f64) -> Option<f64> {
        if !col_f.is_finite() || !row_f.is_finite() {
            return None;
        }
        let (ncol, nrow) = (self.lons.len() as isize, self.lats.len() as isize);
        if ncol == 0 || nrow == 0 {
            return None;
        }
        let c0 = col_f.floor() as isize;
        let r0 = row_f.floor() as isize;
        if c0 < -1 || c0 >= ncol || r0 < -1 || r0 >= nrow {
            return None;
        }
        let (wx, wy) = (col_f - c0 as f64, row_f - r0 as f64);
        let at = |r: isize, c: isize| -> Option<f64> {
            if r < 0 || c < 0 || r >= nrow || c >= ncol {
                None
            } else {
                let value = self.data[r as usize * self.lons.len() + c as usize];
                value.is_finite().then_some(value)
            }
        };
        bilinear(
            at(r0, c0),
            at(r0, c0 + 1),
            at(r0 + 1, c0),
            at(r0 + 1, c0 + 1),
            wx,
            wy,
        )
    }

    /// Sample directly at a WGS84 `(lon, lat)`.
    pub fn sample(&self, lon: f64, lat: f64) -> Option<f64> {
        let (fx, fy) = self.frac_px(lon, lat);
        self.bilinear_at(fx, fy)
    }
}

/// Bilinear blend of four corner values with weights `wx` (toward the east
/// corners) and `wy` (toward the south corners). Falls back to the nearest
/// available corner when any neighbour is nodata.
fn bilinear(
    v00: Option<f64>,
    v01: Option<f64>,
    v10: Option<f64>,
    v11: Option<f64>,
    wx: f64,
    wy: f64,
) -> Option<f64> {
    if let (Some(a), Some(b), Some(c), Some(d)) = (v00, v01, v10, v11) {
        let top = a + (b - a) * wx;
        let bot = c + (d - c) * wx;
        return Some(top + (bot - top) * wy);
    }
    // Nearest corner by the interpolation weights, then any available value.
    let nearest = match (wy < 0.5, wx < 0.5) {
        (true, true) => v00,
        (true, false) => v01,
        (false, true) => v10,
        (false, false) => v11,
    };
    nearest.or(v00).or(v01).or(v10).or(v11)
}

/// Build a catalog by reading the store's metadata and coordinate variables.
pub fn build(
    store: Arc<Store>,
    collection_id: &str,
    param_filter: Option<&[String]>,
    decoded_cache: Arc<DecodedCache>,
) -> Result<Catalog, DataServerError> {
    build_with_codec_setup(
        store,
        collection_id,
        param_filter,
        decoded_cache,
        crate::codec_limits::bounded_array,
    )
}

fn build_with_codec_setup(
    store: Arc<Store>,
    collection_id: &str,
    param_filter: Option<&[String]>,
    decoded_cache: Arc<DecodedCache>,
    configure_codecs: impl Fn(Array<Store>) -> Result<Array<Store>, DataServerError>,
) -> Result<Catalog, DataServerError> {
    let revision = store.revision.clone();
    let content_version = revision.as_ref().map_or_else(
        || {
            store
                .generation
                .as_ref()
                .map_or(0, |generation| generation.version)
        },
        |id| {
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            id.hash(&mut hash);
            hash.finish().max(1)
        },
    );
    let group = Group::open(store.clone(), "/")
        .map_err(|e| DataServerError::Engine(format!("open Zarr root group: {e}")))?;
    let arrays = group
        .child_arrays()
        .map_err(|e| DataServerError::Engine(format!("list Zarr arrays: {e}")))?;
    if arrays.is_empty() {
        return Err(DataServerError::Engine(
            "Zarr store contains no arrays".into(),
        ));
    }

    // Index arrays by leaf name.
    let mut by_name: HashMap<String, Array<Store>> = HashMap::new();
    for a in arrays {
        let leaf = a
            .path()
            .as_str()
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_string();
        if !leaf.is_empty() {
            by_name.insert(leaf, a);
        }
    }

    // Coordinate variables: 1-D arrays whose (only) dim names themselves, the
    // CF coordinate-variable convention. These supply axis values + CF attrs.
    let mut coord_names: HashSet<String> = HashSet::new();
    for (name, a) in &by_name {
        if a.shape().len() == 1 {
            match dim_names(a) {
                Some(dims) if dims.len() == 1 && (dims[0] == *name || dims[0].is_empty()) => {
                    coord_names.insert(name.clone());
                }
                None => {
                    tracing::debug!(
                        "collection '{collection_id}': 1-D array '{name}' has no dimension \
                         names; treating it as a coordinate variable"
                    );
                    coord_names.insert(name.clone());
                }
                _ => {}
            }
        }
    }

    // Candidate data variables (sorted for stable ordering), minus the filter.
    let mut data_var_names: Vec<String> = by_name
        .keys()
        .filter(|n| !coord_names.contains(*n))
        .cloned()
        .collect();
    data_var_names.sort();
    if let Some(filter) = param_filter {
        data_var_names.retain(|n| filter.iter().any(|f| f.eq_ignore_ascii_case(n)));
    }
    if data_var_names.is_empty() {
        return Err(DataServerError::Engine(
            "Zarr store has no data variables (after parameter filter)".into(),
        ));
    }

    // Find the lat/lon dims and the time-like dims (valid time, forecast
    // reference/run, lead) from the first geographic variable.
    #[allow(clippy::type_complexity)]
    let mut found: Option<(
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    )> = None;
    for name in &data_var_names {
        let a = &by_name[name];
        let Some(dims) = dim_names(a) else { continue };
        let (mut lat, mut lon, mut time, mut reference, mut lead) = (None, None, None, None, None);
        for dn in &dims {
            match role_of(&by_name, dn) {
                AxisRole::Lat => lat = Some(dn.clone()),
                AxisRole::Lon => lon = Some(dn.clone()),
                AxisRole::Time => time = Some(dn.clone()),
                AxisRole::Reference => reference = Some(dn.clone()),
                AxisRole::Lead => lead = Some(dn.clone()),
                AxisRole::Other => {}
            }
        }
        if let (Some(la), Some(lo)) = (lat, lon) {
            found = Some((la, lo, time, reference, lead));
            break;
        }
    }
    let (lat_dim, lon_dim, time_role_dim, ref_role_dim, lead_role_dim) =
        found.ok_or_else(|| {
            DataServerError::Engine(
                "no geographic (latitude/longitude) data variable found; non-geographic Zarr \
             is not supported"
                    .into(),
            )
        })?;

    // Read the shared coordinate axes.
    let lat_arr = by_name.get(&lat_dim).ok_or_else(|| {
        DataServerError::Engine(format!("missing latitude coordinate variable '{lat_dim}'"))
    })?;
    let lats = read_coord_f64(lat_arr)?;
    let lon_arr = by_name.get(&lon_dim).ok_or_else(|| {
        DataServerError::Engine(format!("missing longitude coordinate variable '{lon_dim}'"))
    })?;
    let lons = read_coord_f64(lon_arr)?;

    // `locate` (and bilinear sampling) assume monotonic axes; a non-monotonic
    // coordinate would yield silently wrong interpolation, so reject it here.
    if !cf::is_monotonic(&lats) {
        return Err(DataServerError::Engine(format!(
            "latitude coordinate '{lat_dim}' is not monotonic"
        )));
    }
    if !cf::is_monotonic(&lons) {
        return Err(DataServerError::Engine(format!(
            "longitude coordinate '{lon_dim}' is not monotonic"
        )));
    }

    // Pick the primary time axis (the one that varies → the engine's time
    // dimension). A forecast (reference run + lead) uses the **latest run** and
    // exposes valid = run + lead as the time axis, matching the GRIB convention;
    // the reference axis defaults to that latest run. Explicit instances /
    // WMS reference_time select another run through resolve_run.
    let forecast = ref_role_dim.is_some() && lead_role_dim.is_some();
    let primary_dim = if forecast {
        lead_role_dim.clone().unwrap()
    } else if let Some(t) = &time_role_dim {
        t.clone()
    } else if let Some(r) = &ref_role_dim {
        r.clone() // a bare sequence of runs (no lead axis)
    } else {
        return Err(DataServerError::Engine(
            "collection has no time axis (need a CF time, or forecast reference+lead)".into(),
        ));
    };

    // Resolve the latest run when a reference axis is pinned, and keep every
    // run: each is an EDR instance / WMS DIM_REFERENCE_TIME value (#337).
    let mut ref_pin: Option<(String, usize)> = None;
    let mut latest_ref_time: Option<DateTime<Utc>> = None;
    let mut ref_times: Vec<DateTime<Utc>> = Vec::new();
    let mut runs: BTreeMap<DateTime<Utc>, usize> = BTreeMap::new();
    if forecast {
        let rd = ref_role_dim.as_ref().unwrap();
        let ref_arr = by_name.get(rd).ok_or_else(|| {
            DataServerError::Engine(format!("missing reference-time coordinate variable '{rd}'"))
        })?;
        let units = str_attr(ref_arr, "units").ok_or_else(|| {
            DataServerError::Engine(format!("reference-time coordinate '{rd}' has no 'units'"))
        })?;
        if !cf::is_standard_calendar(str_attr(ref_arr, "calendar").as_deref()) {
            tracing::warn!(
                "collection '{collection_id}': non-standard CF calendar '{}' on reference-time \
                 axis '{rd}' approximated as proleptic Gregorian",
                str_attr(ref_arr, "calendar").unwrap_or_default()
            );
        }
        let decoded =
            cf::decode_times(&read_coord_f64(ref_arr)?, &units).map_err(DataServerError::Engine)?;
        for (i, rt) in decoded.iter().enumerate() {
            // A duplicated reference time keeps its LAST axis position.
            runs.insert(*rt, i);
        }
        // "Latest" comes from the same shared rule `resolve_run` applies.
        let (t, idx) = instances::select_run(&runs, None).ok_or_else(|| {
            DataServerError::Engine(format!("reference-time coordinate '{rd}' is empty"))
        })?;
        tracing::info!(
            "collection '{collection_id}': forecast — latest run {t} (of {} runs); lead axis \
             '{primary_dim}' is the time dimension",
            decoded.len()
        );
        latest_ref_time = Some(*t);
        ref_pin = Some((rd.clone(), *idx));
        ref_times = decoded;
    }

    // Build the valid-time axis.
    let primary_arr = by_name.get(&primary_dim).ok_or_else(|| {
        DataServerError::Engine(format!("missing coordinate variable '{primary_dim}'"))
    })?;
    let mut leads: Vec<chrono::Duration> = Vec::new();
    let times = if forecast {
        let units = str_attr(primary_arr, "units").ok_or_else(|| {
            DataServerError::Engine(format!("lead coordinate '{primary_dim}' has no 'units'"))
        })?;
        let secs = cf::parse_duration_seconds(&units).ok_or_else(|| {
            DataServerError::Engine(format!(
                "lead coordinate '{primary_dim}' has unrecognised duration units '{units}'"
            ))
        })?;
        let base = latest_ref_time.expect("forecast sets latest_ref_time");
        // Guard non-finite leads: `NaN as i64` is 0, which would silently yield
        // the run time (mirrors `cf::decode_times`'s finite check).
        leads = read_coord_f64(primary_arr)?
            .iter()
            .map(|&v| {
                if !v.is_finite() {
                    return Err(DataServerError::Engine(format!(
                        "non-finite value in lead axis '{primary_dim}': {v}"
                    )));
                }
                Ok(chrono::Duration::milliseconds(
                    (v * secs * 1000.0).round() as i64
                ))
            })
            .collect::<Result<Vec<_>, _>>()?;
        leads.iter().map(|d| base + *d).collect()
    } else {
        let units = str_attr(primary_arr, "units").ok_or_else(|| {
            DataServerError::Engine(format!("time coordinate '{primary_dim}' has no 'units'"))
        })?;
        if !cf::is_standard_calendar(str_attr(primary_arr, "calendar").as_deref()) {
            tracing::warn!(
                "collection '{collection_id}': non-standard CF calendar '{}' approximated as \
                 proleptic Gregorian",
                str_attr(primary_arr, "calendar").unwrap_or_default()
            );
        }
        cf::decode_times(&read_coord_f64(primary_arr)?, &units).map_err(DataServerError::Engine)?
    };
    let time_dim = primary_dim;

    // Build the exposed variables.
    let mut vars = Vec::new();
    for name in &data_var_names {
        let array = match by_name.remove(name) {
            Some(a) => a,
            None => continue,
        };
        let Some(dims) = dim_names(&array) else {
            tracing::warn!(
                "collection '{collection_id}': variable '{name}' has no dimension names; skipping"
            );
            continue;
        };
        let mut time_axis = None;
        let mut lat_axis = None;
        let mut lon_axis = None;
        for (axis, dn) in dims.iter().enumerate() {
            if *dn == lat_dim {
                lat_axis = Some(axis);
            } else if *dn == lon_dim {
                lon_axis = Some(axis);
            } else if *dn == time_dim {
                time_axis = Some(axis);
            }
        }
        let (Some(lat_axis), Some(lon_axis)) = (lat_axis, lon_axis) else {
            tracing::warn!(
                "collection '{collection_id}': variable '{name}' does not share the lat/lon grid; skipping"
            );
            continue;
        };

        // Forecast: this variable's reference (run) axis.
        let ref_axis = ref_pin
            .as_ref()
            .and_then(|(rd, _)| dims.iter().position(|d| d == rd));

        // Skip variables whose data type the read path can't widen to f64 (e.g.
        // float16/bfloat16, complex, raw bytes) at build time, with a clear
        // warning — rather than listing the parameter and failing the query.
        if !dtype_supported(array.data_type()) {
            tracing::warn!(
                "collection '{collection_id}': variable '{name}' has unsupported Zarr data \
                 type {}; skipping",
                array.data_type()
            );
            continue;
        }

        let array = match configure_codecs(array) {
            Ok(array) => array,
            Err(error) => {
                tracing::warn!(
                    "collection '{collection_id}': variable '{name}' codec setup failed: {error}; skipping"
                );
                continue;
            }
        };

        let shape = array.shape();
        if shape[lat_axis] as usize != lats.len() || shape[lon_axis] as usize != lons.len() {
            tracing::warn!(
                "collection '{collection_id}': variable '{name}' lat/lon length mismatch; skipping"
            );
            continue;
        }

        let attrs = array.attributes();
        let units = attrs
            .get("units")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let label = attrs
            .get("long_name")
            .and_then(|v| v.as_str())
            .or_else(|| attrs.get("standard_name").and_then(|v| v.as_str()))
            .unwrap_or(name)
            .to_string();
        let scale_factor = attrs
            .get("scale_factor")
            .and_then(|v| v.as_f64())
            .unwrap_or(1.0);
        let add_offset = attrs
            .get("add_offset")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let mut fill_values = Vec::new();
        for key in ["_FillValue", "missing_value"] {
            match attrs.get(key) {
                Some(serde_json::Value::Number(n)) => {
                    if let Some(f) = n.as_f64() {
                        fill_values.push(f);
                    }
                }
                Some(serde_json::Value::Array(arr)) => {
                    fill_values.extend(arr.iter().filter_map(|x| x.as_f64()));
                }
                _ => {}
            }
        }
        // Also honour the array's own Zarr fill value — zarrs substitutes it for
        // unwritten/out-of-bounds chunks, so an integer array carrying a
        // non-NaN Zarr fill but no CF `_FillValue` would otherwise emit scaled
        // garbage. NaN floats are already mapped to nodata in `convert`.
        if let Some(fv) = fill_value_as_f64(&array) {
            if fv.is_finite() && !fill_values.contains(&fv) {
                fill_values.push(fv);
            }
        }

        let ndim = shape.len();
        warn_bad_chunking(
            collection_id,
            name,
            &array,
            time_axis,
            lat_axis,
            lon_axis,
            lats.len() as u64,
            lons.len() as u64,
            times.len() as u64,
        );

        vars.push(Variable {
            name: name.clone(),
            decoded: DecodedArray::new(&array, revision.as_deref(), name, decoded_cache.clone()),
            array,
            units,
            label,
            time_axis,
            ref_axis,
            lat_axis,
            lon_axis,
            ndim,
            scale_factor,
            add_offset,
            fill_values,
        });
    }

    if vars.is_empty() {
        return Err(DataServerError::Engine(
            "Zarr store has no usable geographic data variables".into(),
        ));
    }

    let (west, east) = axis_extent(&lons);
    let (south, north) = axis_extent(&lats);
    let extent = [west, south, east, north];

    let reference_times: Vec<DateTime<Utc>> = runs.keys().copied().collect();
    let run_valid_times: Vec<Vec<DateTime<Utc>>> = ref_times
        .iter()
        .map(|base| leads.iter().map(|d| *base + *d).collect())
        .collect();
    let raster_info = build_raster_info(
        &vars,
        &times,
        &reference_times,
        extent,
        [lons.len() as u32, lats.len() as u32],
    );

    Ok(Catalog {
        read_budget: read_budget::BUDGET.clone(),
        revision,
        content_version,
        vars,
        times,
        runs,
        ref_times,
        run_valid_times,
        lats,
        lons,
        extent,
        raster_info,
    })
}

/// Build the map-capabilities snapshot (one layer per variable). Stored on the
/// `Catalog` so `raster_info()` is O(1) and swaps atomically with the data.
fn build_raster_info(
    vars: &[Variable],
    times: &[DateTime<Utc>],
    reference_times: &[DateTime<Utc>],
    extent: [f64; 4],
    grid_size: [u32; 2],
) -> RasterInfo {
    let parameters: Vec<ds_core::map_engine::ParameterInfo> = vars
        .iter()
        .map(|v| ds_core::map_engine::ParameterInfo {
            name: v.name.clone(),
            title: v.label.clone(),
            unit: v.units.clone(),
        })
        .collect();
    let (parameter, unit) = vars
        .first()
        .map(|v| (v.name.clone(), v.units.clone()))
        .unwrap_or_default();
    RasterInfo {
        // Lon-first geographic grid → CRS:84 (not lat-first EPSG:4326), matching
        // the other gridded engines' storageCrs.
        native_crs: "CRS:84".to_string(),
        spatial_extent: Some(extent),
        times: times.to_vec(),
        reference_times: reference_times.to_vec(),
        parameter,
        unit,
        parameters,
        vertical: None,
        grid_size: Some(grid_size),
        layer_subtitle: None,
    }
}

/// Classify a dimension by its coordinate variable's CF attributes (preferred)
/// or its name.
fn role_of(by_name: &HashMap<String, Array<Store>>, dim: &str) -> AxisRole {
    match by_name.get(dim) {
        Some(a) => cf::classify_axis(
            dim,
            str_attr(a, "standard_name").as_deref(),
            str_attr(a, "units").as_deref(),
        ),
        None => cf::classify_axis(dim, None, None),
    }
}

/// Read a string attribute.
fn str_attr(array: &Array<Store>, key: &str) -> Option<String> {
    array
        .attributes()
        .get(key)
        .and_then(|v| v.as_str())
        .map(String::from)
}

/// Get a variable's dimension names, preferring V3 `dimension_names` and
/// falling back to the V2 `_ARRAY_DIMENSIONS` attribute. Returns one entry per
/// axis (empty string for an unnamed axis), or `None` if neither is present.
fn dim_names(array: &Array<Store>) -> Option<Vec<String>> {
    if let Some(names) = array.dimension_names() {
        let out: Vec<String> = names
            .iter()
            .map(|d| d.clone().unwrap_or_default())
            .collect();
        if out.iter().any(|s| !s.is_empty()) {
            return Some(out);
        }
    }
    if let Some(arr) = array
        .attributes()
        .get("_ARRAY_DIMENSIONS")
        .and_then(|v| v.as_array())
    {
        let out: Vec<String> = arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        if out.len() == array.shape().len() {
            return Some(out);
        }
    }
    None
}

/// Emit a startup warning when a variable's chunk shape is pathological for
/// point/time-series access (#125).
#[allow(clippy::too_many_arguments)]
fn warn_bad_chunking(
    collection_id: &str,
    name: &str,
    array: &Array<Store>,
    time_axis: Option<usize>,
    lat_axis: usize,
    lon_axis: usize,
    ny: u64,
    nx: u64,
    n_times: u64,
) {
    let ndim = array.shape().len();
    let Ok(chunk) = array.chunk_shape(&vec![0u64; ndim]) else {
        return;
    };
    let inner = array.effective_subchunk_shape();
    let effective = inner.as_ref().unwrap_or(&chunk);
    let native_bytes = array.data_type().fixed_size().and_then(|size| {
        effective
            .iter()
            .try_fold(size as u64, |n, dim| n.checked_mul(dim.get()))
    });
    tracing::info!(
        collection = collection_id, parameter = name,
        outer_chunk_shape = ?chunk, effective_inner_shape = ?inner,
        native_chunk_bytes = ?native_bytes,
        time_steps_per_chunk = time_axis.map(|a| effective[a].get()).unwrap_or(1),
        "Zarr chunk layout (uncompressed size estimate)"
    );
    let chunk = effective;
    let time_chunk = time_axis.map(|a| chunk[a].get()).unwrap_or(n_times);
    let lat_chunk = chunk[lat_axis].get();
    let lon_chunk = chunk[lon_axis].get();
    if cf::is_bad_timeseries_chunking(time_chunk, lat_chunk, lon_chunk, ny, nx, n_times) {
        tracing::warn!(
            "collection '{collection_id}': variable '{name}' chunk shape (time={time_chunk}, \
             lat={lat_chunk}, lon={lon_chunk}) over {ny}×{nx} is pathological for point/\
             time-series queries — each timestep is a single full-domain chunk, so a point \
             query decodes the entire field for every timestep"
        );
    }
}

/// Index range `(lo, hi)` (inclusive) of the cells of a monotonic axis needed to
/// render `[min, max]`: every cell whose half-cell footprint overlaps the
/// interval, expanded by one cell each side so bilinear has its bracketing
/// neighbours at the edge. `None` if the interval lies entirely off the axis.
///
/// Crucially this includes the *bracketing* cells even when no cell **centre**
/// falls inside `[min, max]` — a zoomed-in tile sitting between two grid centres
/// must still interpolate, not render transparent. Works for ascending or
/// descending axes (it compares values, not index order).
fn axis_window(axis: &[f64], min: f64, max: f64) -> Option<(usize, usize)> {
    let n = axis.len();
    if n == 0 || !min.is_finite() || !max.is_finite() || min > max {
        return None;
    }
    if n == 1 {
        // Single (collapsed) cell — `Window`/`locate` snap any target to it.
        return Some((0, 0));
    }
    // Keep the existing advertised edge footprint, but choose the interior
    // window by ACTUAL coordinate brackets. Mean spacing cannot describe an
    // irregular interior gap (e.g. [0, 1, 100] around longitude 50).
    let (edge_min, edge_max) = axis_extent(axis);
    if max < edge_min || min > edge_max {
        return None;
    }
    let start = axis[0].min(axis[n - 1]);
    let end = axis[0].max(axis[n - 1]);
    let (a0, a1, _) = cf::locate(axis, min.clamp(start, end))?;
    let (b0, b1, _) = cf::locate(axis, max.clamp(start, end))?;
    Some((a0.min(b0).saturating_sub(1), (a1.max(b1) + 1).min(n - 1)))
}

/// Edge extent `(min, max)` of a centred coordinate axis, expanded by half a
/// cell. Returns the bare min/max for a single-element axis.
fn axis_extent(vals: &[f64]) -> (f64, f64) {
    let min = vals.iter().copied().fold(f64::INFINITY, f64::min);
    let max = vals.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let half = if vals.len() > 1 {
        (max - min) / ((vals.len() - 1) as f64) / 2.0
    } else {
        0.0
    };
    (min - half, max + half)
}

/// Whether [`retrieve_raw_f64`] can widen this data type to `f64`. Keep the
/// arms in sync with `retrieve_raw_f64`. Unsupported types (float16/bfloat16,
/// complex, raw bytes, strings) are skipped at build time so a query never
/// fails at read time with a confusing error.
fn dtype_supported(dt: &zarrs::array::DataType) -> bool {
    *dt == data_type::float32()
        || *dt == data_type::float64()
        || *dt == data_type::int8()
        || *dt == data_type::int16()
        || *dt == data_type::int32()
        || *dt == data_type::int64()
        || *dt == data_type::uint8()
        || *dt == data_type::uint16()
        || *dt == data_type::uint32()
        || *dt == data_type::uint64()
}

/// Widen an array's Zarr-native fill value to `f64`, interpreting its raw
/// native-endian bytes per the array's data type. Returns `None` for an
/// unsupported dtype or a byte-length mismatch.
fn fill_value_as_f64(array: &Array<Store>) -> Option<f64> {
    let bytes = array.fill_value().as_ne_bytes();
    let dt = array.data_type();
    macro_rules! decode {
        ($t:ty, $n:literal) => {{
            let arr: [u8; $n] = bytes.try_into().ok()?;
            Some(<$t>::from_ne_bytes(arr) as f64)
        }};
    }
    if *dt == data_type::float32() {
        decode!(f32, 4)
    } else if *dt == data_type::float64() {
        decode!(f64, 8)
    } else if *dt == data_type::int8() {
        decode!(i8, 1)
    } else if *dt == data_type::int16() {
        decode!(i16, 2)
    } else if *dt == data_type::int32() {
        decode!(i32, 4)
    } else if *dt == data_type::int64() {
        decode!(i64, 8)
    } else if *dt == data_type::uint8() {
        decode!(u8, 1)
    } else if *dt == data_type::uint16() {
        decode!(u16, 2)
    } else if *dt == data_type::uint32() {
        decode!(u32, 4)
    } else if *dt == data_type::uint64() {
        decode!(u64, 8)
    } else {
        None
    }
}

/// Read an entire array as `Vec<f64>` (used for small coordinate arrays).
fn read_coord_f64(array: &Array<Store>) -> Result<Vec<f64>, DataServerError> {
    let subset = ArraySubset::new_with_shape(array.shape().to_vec());
    retrieve_raw_f64(array, &subset, None, 1)
}

pub(crate) fn chunk_read_error(error: ArrayError) -> DataServerError {
    // Chunk reads and partial/sharded decoding wrap storage errors differently.
    // Inspect the typed IO payload: text matching or checking the catalog's
    // current retirement flag could misclassify an unrelated codec failure.
    let io = match &error {
        ArrayError::StorageError(StorageError::IOError(io))
        | ArrayError::CodecError(CodecError::StorageError(StorageError::IOError(io)))
        | ArrayError::CodecError(CodecError::IOError(io)) => Some(io),
        _ => None,
    };
    match io
        .and_then(|io| io.get_ref())
        .and_then(|error| error.downcast_ref::<DataServerError>())
    {
        Some(DataServerError::ResourceExhausted) => return DataServerError::ResourceExhausted,
        Some(DataServerError::DeadlineExceeded) => return DataServerError::DeadlineExceeded,
        _ => {}
    }
    DataServerError::Engine(format!("Zarr chunk read failed: {error}"))
}

/// Retrieve an array subset as raw `f64` values (no CF scaling), branching on
/// the array's data type. Integer and float types are widened to `f64`.
fn retrieve_raw_f64(
    array: &Array<Store>,
    subset: &ArraySubset,
    decoded: Option<&DecodedArray>,
    parallelism: usize,
) -> Result<Vec<f64>, DataServerError> {
    ds_core::deadline::check()?;
    let dt = array.data_type();
    let opts = single_threaded_opts();
    if !dtype_supported(dt) {
        return Err(DataServerError::Engine(format!(
            "unsupported Zarr data type: {dt}"
        )));
    }
    let bytes = match decoded {
        Some(decoded) => decoded.read(array, subset, &opts, parallelism)?,
        None => {
            let result = array.retrieve_array_subset_opt::<ArrayBytes<'static>>(subset, &opts);
            ds_core::deadline::check()?;
            result.map_err(chunk_read_error)?
        }
    };
    macro_rules! read_as {
        ($t:ty) => {
            Vec::<$t>::from_array_bytes(bytes, subset.shape(), dt)
                .map(|v| v.into_iter().map(|x| x as f64).collect::<Vec<f64>>())
        };
    }
    let result = if *dt == data_type::float32() {
        read_as!(f32)
    } else if *dt == data_type::float64() {
        read_as!(f64)
    } else if *dt == data_type::int8() {
        read_as!(i8)
    } else if *dt == data_type::int16() {
        read_as!(i16)
    } else if *dt == data_type::int32() {
        read_as!(i32)
    } else if *dt == data_type::int64() {
        read_as!(i64)
    } else if *dt == data_type::uint8() {
        read_as!(u8)
    } else if *dt == data_type::uint16() {
        read_as!(u16)
    } else if *dt == data_type::uint32() {
        read_as!(u32)
    } else if *dt == data_type::uint64() {
        read_as!(u64)
    } else {
        return Err(DataServerError::Engine(format!(
            "unsupported Zarr data type: {dt}"
        )));
    };
    // Recover the typed request deadline before mapping other codec failures.
    ds_core::deadline::check()?;
    result.map_err(chunk_read_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_setup_failure_skips_only_the_affected_variable() {
        let config = ds_core::config::ZarrConfig::auto_local(
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/zarr-era5-t2m").into(),
        );
        // Inject at codec setup: malformed metadata would fail earlier during
        // child-array discovery and would not exercise this failure boundary.
        for reject_all in [false, true] {
            let result = build_with_codec_setup(
                Arc::new(EngineStore::plain(
                    crate::build_store("codec-setup-test", &config).unwrap(),
                )),
                "codec-setup-test",
                None,
                Arc::new(DecodedCache::new(0)),
                |array| {
                    if reject_all || array.path().as_str() == "/t2m" {
                        Err(DataServerError::Engine(
                            "unsupported shard configuration".into(),
                        ))
                    } else {
                        crate::codec_limits::bounded_array(array)
                    }
                },
            );
            if reject_all {
                assert!(matches!(result, Err(DataServerError::Engine(message))
                    if message == "Zarr store has no usable geographic data variables"));
            } else {
                let catalog = result.unwrap();
                assert_eq!(catalog.vars.len(), 1);
                assert_eq!(catalog.vars[0].name, "t2m_packed");
                assert_eq!(catalog.raster_info.parameters.len(), 1);
                assert_eq!(catalog.raster_info.parameter, "t2m_packed");
                let window = catalog
                    .read_window(&catalog.vars[0], None, 0, catalog.extent)
                    .unwrap()
                    .unwrap();
                let expected = 273.15 + 0.1 * 54.5 + 0.01 * 5.5;
                assert!((window.sample(5.5, 54.5).unwrap() - expected).abs() < 0.01);
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retirement_remains_retryable_through_full_and_partial_shard_reads() {
        use zarrs::array::{codec::ShardingCodecBuilder, ArrayBuilder};
        for sharded in [false, true] {
            let store = Arc::new(EngineStore::plain(crate::store::DsStore::new(
                ds_storage::DataStore::new(Arc::new(
                    ds_storage::object_store::memory::InMemory::new(),
                )),
                "",
                0,
            )));
            let mut builder = ArrayBuilder::new(vec![4, 4], vec![4, 4], data_type::float32(), 0f32);
            if sharded {
                builder.array_to_bytes_codec(Arc::new(
                    ShardingCodecBuilder::new(
                        vec![2.try_into().unwrap(); 2],
                        &data_type::float32(),
                    )
                    .build(),
                ));
            }
            let array = builder.build(store.clone(), "/temp").unwrap();
            let full = ArraySubset::new_with_shape(vec![4, 4]);
            assert_eq!(
                retrieve_raw_f64(&array, &full, None, 1).unwrap(),
                vec![0.; 16],
                "an active store still returns fill values for missing objects"
            );
            store.generation.as_ref().unwrap().retire();
            for subset in [full, ArraySubset::new_with_ranges(&[1..3, 1..3])] {
                assert!(matches!(
                    retrieve_raw_f64(&array, &subset, None, 1),
                    Err(DataServerError::ResourceExhausted)
                ));
            }
        }
    }

    fn fixture(budget: Arc<Budget>) -> Catalog {
        let config = ds_core::config::ZarrConfig::auto_local(
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/zarr-era5-t2m").into(),
        );
        let mut catalog = build(
            Arc::new(EngineStore::plain(
                crate::build_store("budget-test", &config).unwrap(),
            )),
            "budget-test",
            None,
            Arc::new(DecodedCache::new(0)),
        )
        .unwrap();
        catalog.read_budget = budget;
        catalog
    }

    #[test]
    fn window_span_keeps_reservation_until_last_window_is_dropped() {
        let budget = Arc::new(Budget::new(ds_cache::MIB));
        let catalog = fixture(budget.clone());
        let mut windows = catalog
            .read_window_span(&catalog.vars[0], None, 0..3, catalog.extent)
            .unwrap()
            .unwrap();
        let used = budget.metrics().0;
        assert!(used > 0);
        let last = windows.pop().unwrap();
        drop(windows);
        assert_eq!(budget.metrics().0, used);
        assert!(last.sample(5.5, 54.5).is_some());
        drop(last);
        assert_eq!(budget.metrics().0, 0);
        catalog
            .sample_series(&catalog.vars[0], None, 5.5, 54.5, &[0, 1])
            .unwrap();
        assert_eq!(
            budget.metrics().0,
            0,
            "position read releases its working set"
        );
    }

    #[test]
    fn map_admission_precedes_payload_io_and_failed_reads_release_memory() {
        use ds_core::map_engine::{MapEngine, OutputCrs};
        fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
            std::fs::create_dir_all(to).unwrap();
            for item in std::fs::read_dir(from).unwrap() {
                let item = item.unwrap();
                let dest = to.join(item.file_name());
                if item.file_type().unwrap().is_dir() {
                    copy_tree(&item.path(), &dest);
                } else {
                    std::fs::copy(item.path(), dest).unwrap();
                }
            }
        }
        let dir = tempfile::tempdir().unwrap();
        copy_tree(
            std::path::Path::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../testdata/zarr-era5-t2m"
            )),
            dir.path(),
        );
        let payload_path = dir.path().join("t2m/c/0/0/0");
        let valid_payload = std::fs::read(&payload_path).unwrap();
        use std::io::Write;
        let mut bomb = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        bomb.write_all(&vec![42; 2 * ds_cache::MIB as usize])
            .unwrap();
        let bomb = bomb.finish().unwrap();
        assert!(bomb.len() < 16 * 1024);
        std::fs::write(&payload_path, b"invalid gzip").unwrap();
        let config = ds_core::config::ZarrConfig::auto_local(dir.path().to_string_lossy().into());
        let engine = crate::ZarrEngine::new("budget-test", &config).unwrap();
        let render = || {
            engine.get_raster_tile(
                [0.0, 49.0, 15.0, 60.0],
                1,
                1,
                None,
                &OutputCrs::Wgs84,
                Some("t2m"),
                None,
                None,
            )
        };
        for (capacity, payload, exhausted) in [
            (0, "corrupt", true),
            (ds_cache::MIB, "corrupt", false),
            (ds_cache::MIB, "oversized", true),
            (ds_cache::MIB, "bomb", false),
            (ds_cache::MIB, "valid", false),
        ] {
            if payload == "oversized" {
                // The native window fits, but this encoded object does not.
                // Reject from its metadata before reading/decoding the corrupt body.
                std::fs::File::create(&payload_path)
                    .unwrap()
                    .set_len(2 * ds_cache::MIB)
                    .unwrap();
            } else if payload == "bomb" {
                // The encoded object fits, but the decoded body exceeds the
                // declared native chunk. Fail before allocating that body.
                std::fs::write(&payload_path, &bomb).unwrap();
            } else if payload == "valid" {
                std::fs::write(&payload_path, &valid_payload).unwrap();
            }
            let budget = Arc::new(Budget::new(capacity));
            let mut catalog = build(
                Arc::new(EngineStore::plain(
                    crate::build_store("budget-test", &config).unwrap(),
                )),
                "budget-test",
                None,
                Arc::new(DecodedCache::new(0)),
            )
            .unwrap();
            catalog.read_budget = budget.clone();
            engine.catalog.store(Arc::new(catalog));
            if payload == "valid" {
                assert!(
                    render().is_ok(),
                    "a fresh catalog can retry after corruption"
                );
                assert_eq!(budget.metrics().0, 0);
                continue;
            }
            let error = render().err().expect("render must fail");
            if exhausted {
                assert!(
                    matches!(error, DataServerError::ResourceExhausted),
                    "{error:?}"
                );
                assert_eq!(budget.metrics().2, 1);
            } else {
                assert!(
                    matches!(error, DataServerError::Engine(_)),
                    "the corrupt payload must be read once admitted"
                );
            }
            assert_eq!(budget.metrics().0, 0);
        }
    }

    #[test]
    fn off_grid_and_expired_reads_do_not_consume_source_budget() {
        let budget = Arc::new(Budget::new(0));
        let catalog = fixture(budget.clone());
        assert!(catalog
            .read_window(&catalog.vars[0], None, 0, [150.0, -50.0, 160.0, -40.0])
            .unwrap()
            .is_none());
        let _scope = ds_core::deadline::enter(Some(std::time::Instant::now()));
        assert!(matches!(
            catalog.read_window(&catalog.vars[0], None, 0, catalog.extent),
            Err(DataServerError::DeadlineExceeded)
        ));
        assert_eq!(budget.metrics(), (0, 0, 0));
    }

    #[test]
    fn irregular_axis_windows_include_real_brackets_in_both_directions() {
        for axis in [[0.0, 1.0, 100.0], [100.0, 1.0, 0.0]] {
            assert_eq!(axis_window(&axis, 49.0, 51.0), Some((0, 2)));
            assert!(axis_window(&axis, 150.0, 160.0).is_none());
            assert!(axis_window(&axis, 51.0, 49.0).is_none());
        }
    }

    #[test]
    fn convert_sample_scales_and_maps_nodata() {
        // Plain scale/offset.
        assert_eq!(convert_sample(5.0, 2.0, 1.0, &[]), Some(11.0));
        // CF packing with a fill sentinel.
        assert_eq!(
            convert_sample(550.0, 0.01, 273.15, &[-9999.0]),
            Some(278.65)
        );
        assert_eq!(convert_sample(-9999.0, 0.01, 273.15, &[-9999.0]), None);
        // Non-finite samples are nodata (an inf would otherwise scale to inf
        // and break CoverageJSON).
        assert_eq!(convert_sample(f64::NAN, 1.0, 0.0, &[]), None);
        assert_eq!(convert_sample(f64::INFINITY, 1.0, 0.0, &[]), None);
        assert_eq!(convert_sample(f64::NEG_INFINITY, 1.0, 0.0, &[]), None);
        assert_eq!(convert_sample(f64::MAX, 2.0, 0.0, &[]), None);
    }
}
