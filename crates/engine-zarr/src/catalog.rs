//! The Zarr catalog: a parsed snapshot of a store's data variables and shared
//! geographic/temporal axes, plus the point-sampling read path.
//!
//! A catalog is built once at engine construction and rebuilt by the poll loop;
//! it is swapped atomically via `ArcSwap`, so EDR queries read a consistent
//! snapshot without locking.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use zarrs::array::{data_type, Array, ArraySubset};
use zarrs::filesystem::FilesystemStore;
use zarrs::group::Group;

use ds_core::error::DataServerError;

use crate::cf::{self, AxisRole};

/// The concrete store type used by the Phase-1 local backend. Phase 2 will
/// generalise this to a trait object over `ds-storage`.
type Store = FilesystemStore;

/// A single exposed data variable (one EDR/Map parameter).
pub struct Variable {
    pub name: String,
    /// Opened zarr array handle, used for on-demand chunk reads.
    array: Array<Store>,
    pub units: String,
    pub label: String,
    /// Axis index of the time dimension within this variable's dim order.
    pub time_axis: Option<usize>,
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
    /// and NaN to `None`.
    fn convert(&self, raw: f64) -> Option<f64> {
        if raw.is_nan() {
            return None;
        }
        // `raw` is guaranteed non-NaN here, so a NaN `_FillValue` is already
        // covered by the check above; a plain equality is enough.
        if self.fill_values.contains(&raw) {
            return None;
        }
        Some(raw * self.scale_factor + self.add_offset)
    }
}

/// A parsed Zarr store snapshot.
pub struct Catalog {
    /// Data variables in stable (sorted) order.
    pub vars: Vec<Variable>,
    /// Decoded time axis (ascending), shared across all variables.
    pub times: Vec<DateTime<Utc>>,
    /// Latitude axis values (degrees north; may be ascending or descending).
    lats: Vec<f64>,
    /// Longitude axis values (degrees east).
    lons: Vec<f64>,
    /// Spatial extent `[west, south, east, north]` in WGS84 degrees.
    pub extent: [f64; 4],
}

impl Catalog {
    /// Sample a variable's value at `(lon, lat)` for each requested time index,
    /// using bilinear interpolation over the surrounding grid cells (nearest
    /// fallback where a neighbour is nodata). Reads a single small hyperslab
    /// covering the 2×2 spatial neighbourhood across the requested time span.
    pub fn sample_series(
        &self,
        var: &Variable,
        lon: f64,
        lat: f64,
        time_idx: &[usize],
    ) -> Result<Vec<Option<f64>>, DataServerError> {
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
            } else {
                ranges.push(0..1);
            }
        }
        let subset = ArraySubset::new_with_ranges(&ranges);
        let raw = retrieve_raw_f64(&var.array, &subset)?;
        let lens: Vec<usize> = ranges.iter().map(|r| (r.end - r.start) as usize).collect();
        let conv: Vec<Option<f64>> = raw.iter().map(|&r| var.convert(r)).collect();

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
            conv.get(off).copied().flatten()
        };

        let mut out = Vec::with_capacity(time_idx.len());
        for &ti in time_idx {
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
) -> Result<Catalog, DataServerError> {
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

    // Find the reference lat/lon/time dims from the first geographic variable.
    let mut ref_dims: Option<(String, String, Option<String>)> = None;
    for name in &data_var_names {
        let a = &by_name[name];
        let Some(dims) = dim_names(a) else { continue };
        let (mut lat, mut lon, mut time) = (None, None, None);
        for dn in &dims {
            match role_of(&by_name, dn) {
                AxisRole::Lat => lat = Some(dn.clone()),
                AxisRole::Lon => lon = Some(dn.clone()),
                AxisRole::Time => time = Some(dn.clone()),
                AxisRole::Other => {}
            }
        }
        if let (Some(la), Some(lo)) = (lat, lon) {
            ref_dims = Some((la, lo, time));
            break;
        }
    }
    let (lat_dim, lon_dim, time_dim) = ref_dims.ok_or_else(|| {
        DataServerError::Engine(
            "no geographic (latitude/longitude) data variable found; non-geographic Zarr \
             is not supported in Phase 1"
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

    let time_dim = time_dim.ok_or_else(|| {
        DataServerError::Engine(
            "collection has no time dimension; Phase 1 requires a CF time axis".into(),
        )
    })?;
    let time_arr = by_name.get(&time_dim).ok_or_else(|| {
        DataServerError::Engine(format!("missing time coordinate variable '{time_dim}'"))
    })?;
    let time_units = str_attr(time_arr, "units").ok_or_else(|| {
        DataServerError::Engine(format!(
            "time coordinate '{time_dim}' has no 'units' attribute"
        ))
    })?;
    if !cf::is_standard_calendar(str_attr(time_arr, "calendar").as_deref()) {
        tracing::warn!(
            "collection '{collection_id}': non-standard CF calendar '{}' approximated as \
             proleptic Gregorian",
            str_attr(time_arr, "calendar").unwrap_or_default()
        );
    }
    let raw_times = read_coord_f64(time_arr)?;
    let times = cf::decode_times(&raw_times, &time_units).map_err(DataServerError::Engine)?;

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
            array,
            units,
            label,
            time_axis,
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

    Ok(Catalog {
        vars,
        times,
        lats,
        lons,
        extent: [west, south, east, north],
    })
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

/// Read an entire array as `Vec<f64>` (used for small coordinate arrays).
fn read_coord_f64(array: &Array<Store>) -> Result<Vec<f64>, DataServerError> {
    let subset = ArraySubset::new_with_shape(array.shape().to_vec());
    retrieve_raw_f64(array, &subset)
}

/// Retrieve an array subset as raw `f64` values (no CF scaling), branching on
/// the array's data type. Integer and float types are widened to `f64`.
fn retrieve_raw_f64(
    array: &Array<Store>,
    subset: &ArraySubset,
) -> Result<Vec<f64>, DataServerError> {
    let dt = array.data_type();
    macro_rules! read_as {
        ($t:ty) => {
            array
                .retrieve_array_subset::<Vec<$t>>(subset)
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
    result.map_err(|e| DataServerError::Engine(format!("Zarr chunk read failed: {e}")))
}
