//! A global spherical-Mercator grid recognised from a file's 2-D latitude
//! and longitude arrays (#819, GMGSI).
//!
//! NOAA's GMGSI mosaic declares no `grid_mapping`: each pixel carries its
//! own latitude and longitude. The arrays are separable (every row holds the
//! same longitudes, every column the same latitudes), longitude is linear in
//! the column, and latitude is linear in the row in spherical Mercator
//! northing — a regular EPSG:3857 grid. It is recognised once per file, here,
//! so no render ever looks up a pixel's latitude and longitude (Critical
//! Rule 5); a file whose arrays are not such a grid is an error.
//!
//! Each array is one deflated chunk of 15 M floats in GMGSI, inflated whole
//! whatever is read from it. Only its first and last rows (longitude) and
//! columns (latitude) are read — the rows and columns a curvilinear or
//! rotated grid bends most between — which holds the transient to the
//! inflated chunk: ~120 MB and ~55 ms per array, against ~180 MB for the
//! whole array.
//!
//! The grid spans the globe: its columns run a little short of or past one
//! turn of longitude (GMGSI's 4999 columns reach 0.38 px short of 360°), so
//! callers wrap columns modulo [`MercatorGrid::col_period`].

use ds_core::geo::{Crs, GeoTransform};
use ds_core::web_mercator::{lat_to_y, lon_to_x};
use netcdf_reader::{NcFile, NcSliceInfo, NcSliceInfoElem, NcType, NcVariable};

/// How far a recognised grid may stray from exact, in pixels: separable to
/// this, and each row and column within this of its place on the regular
/// grid. GMGSI's float32 arrays are separable exactly and regular to
/// 0.0008 px in Mercator northing; fitting its latitudes on the WGS84
/// ellipsoid instead would miss by ~1 px.
const TOLERANCE_PX: f64 = 0.01;

/// A regular spherical-Mercator grid spanning the globe.
#[derive(Debug)]
pub(crate) struct MercatorGrid {
    /// In [`Crs::WebMercator`] metres: the first column's longitude
    /// unwrapped, later columns increasing past 180°.
    pub gt: GeoTransform,
    /// Columns per 360° of longitude.
    pub col_period: f64,
}

/// Recognise the grid of a `(…, y_name, x_name)` field from the 2-D arrays
/// its `coordinates` attribute names (CF: latitude by `standard_name` or
/// `units` of `degrees_north`, longitude of `degrees_east`).
pub(crate) fn from_lat_lon(
    nc: &NcFile,
    var: &NcVariable,
    y_name: &str,
    x_name: &str,
) -> Result<MercatorGrid, String> {
    let names = var
        .attribute("coordinates")
        .and_then(|a| a.value.as_string())
        .ok_or("it has neither a grid_mapping nor 2-D lat/lon coordinates")?;
    let (mut lat, mut lon) = (None, None);
    for name in names.split_whitespace() {
        let Ok(coord) = nc.variable(name) else {
            continue;
        };
        let text = |a: &str| {
            coord
                .attribute(a)
                .and_then(|v| v.value.as_string())
                .map(|s| s.trim().to_ascii_lowercase())
        };
        let (standard, units) = (text("standard_name"), text("units"));
        let dims: Vec<&str> = coord.dimensions().iter().map(|d| d.name.as_str()).collect();
        if dims != [y_name, x_name] {
            continue;
        }
        if standard.as_deref() == Some("latitude") || units.as_deref() == Some("degrees_north") {
            lat = Some(coord);
        } else if standard.as_deref() == Some("longitude")
            || units.as_deref() == Some("degrees_east")
        {
            lon = Some(coord);
        }
    }
    let (Some(lat), Some(lon)) = (lat, lon) else {
        return Err(format!(
            "its coordinates '{names}' name no 2-D latitude and longitude over ({y_name}, {x_name})"
        ));
    };
    let ny = to_usize(var_dim(lat, 0)?)?;
    let nx = to_usize(var_dim(lat, 1)?)?;
    if nx < 2 || ny < 2 {
        return Err("a Mercator grid needs at least 2 × 2 pixels".to_string());
    }
    // The first and last row of longitudes, the first and last column of
    // latitudes; one array at a time.
    let ends = |n: usize| NcSliceInfoElem::Slice {
        start: 0,
        end: n as u64,
        step: n as u64 - 1,
    };
    let all = |n: usize| NcSliceInfoElem::Slice {
        start: 0,
        end: n as u64,
        step: 1,
    };
    let rows = NcSliceInfo {
        selections: vec![ends(ny), all(nx)],
    };
    let longitudes = with_values(
        nc,
        lon,
        &rows,
        |v| longitudes(v, 2, nx),
        |v| longitudes(v, 2, nx),
    )?;
    let columns = NcSliceInfo {
        selections: vec![all(ny), ends(nx)],
    };
    let northings = with_values(
        nc,
        lat,
        &columns,
        |v| northings(v, ny, 2),
        |v| northings(v, ny, 2),
    )?;
    grid(&longitudes, &northings)
}

/// The grid from each column's unwrapped longitude and each row's Mercator
/// northing: both must be regular, rows north to south, and the columns
/// must span 360° to within a pixel.
fn grid(longitudes: &[f64], northings: &[f64]) -> Result<MercatorGrid, String> {
    let (nx, ny) = (longitudes.len(), northings.len());
    if nx < 2 || ny < 2 {
        return Err("a Mercator grid needs at least 2 × 2 pixels".to_string());
    }
    let dlon = regular_step(longitudes).ok_or("its longitudes are not regular")?;
    let dy = regular_step(northings).ok_or("its latitudes are not regular in Mercator northing")?;
    if dlon <= 0.0 || dy >= 0.0 {
        return Err(format!(
            "its columns must run west to east and rows north to south, \
             got {dlon}° per column and {dy} m per row"
        ));
    }
    let col_period = 360.0 / dlon;
    if (nx as f64) + 1.0 <= col_period {
        return Err(format!(
            "its {nx} columns of {dlon}° do not span the globe; only a global Mercator \
             mosaic is served"
        ));
    }
    let gt = GeoTransform::from_cell_centres(
        lon_to_x(longitudes[0]),
        lon_to_x(dlon),
        northings[0],
        dy,
        to_u32(nx)?,
        to_u32(ny)?,
        Crs::WebMercator,
    )?;
    Ok(MercatorGrid { gt, col_period })
}

/// The step of a regular sequence, from its ends, when every value is
/// within [`TOLERANCE_PX`] of a step of its place.
fn regular_step(values: &[f64]) -> Option<f64> {
    let n = values.len();
    let step = (values[n - 1] - values[0]) / (n - 1) as f64;
    let regular = step.is_finite()
        && step != 0.0
        && values
            .iter()
            .enumerate()
            .all(|(i, v)| (v - (values[0] + i as f64 * step)).abs() <= TOLERANCE_PX * step.abs());
    regular.then_some(step)
}

/// Each column's longitude from the first row, unwrapped to increase
/// across ±180°, after checking every row (of the `ny` given) repeats it.
fn longitudes<T: Copy + Into<f64>>(values: &[T], ny: usize, nx: usize) -> Result<Vec<f64>, String> {
    let at = |r: usize, c: usize| -> f64 { values[r * nx + c].into() };
    let mut unwrapped = Vec::with_capacity(nx);
    let mut previous = at(0, 0);
    let mut lon = previous;
    for c in 0..nx {
        let v = at(0, c);
        if !v.is_finite() || v.abs() > 360.0 {
            return Err(format!("longitude {v} at column {c}"));
        }
        lon += wrap_half_turn(v - previous);
        previous = v;
        unwrapped.push(lon);
    }
    let step = (unwrapped[nx - 1] - unwrapped[0]).abs() / (nx - 1) as f64;
    let tol = TOLERANCE_PX * step;
    for r in 1..ny {
        for c in 0..nx {
            let d = at(r, c) - at(0, c);
            // The same meridian may read 180° in one row and −180° in the next.
            if d.abs() > tol && wrap_half_turn(d).abs() > tol {
                return Err(format!(
                    "its longitudes are not separable: row {r} column {c} is {}°, row 0 {}°",
                    at(r, c),
                    at(0, c)
                ));
            }
        }
    }
    Ok(unwrapped)
}

/// Each row's Mercator northing (metres) from the first column's latitude,
/// after checking every column (of the `nx` given) repeats it.
fn northings<T: Copy + Into<f64>>(values: &[T], ny: usize, nx: usize) -> Result<Vec<f64>, String> {
    let at = |r: usize, c: usize| -> f64 { values[r * nx + c].into() };
    let mut northings = Vec::with_capacity(ny);
    for r in 0..ny {
        let lat = at(r, 0);
        if !(lat.is_finite() && lat.abs() < 90.0) {
            return Err(format!("latitude {lat} at row {r}"));
        }
        // The row's own spacing in degrees sets how far a column may stray.
        let neighbour = at(
            if r + 1 < ny {
                r + 1
            } else {
                r.saturating_sub(1)
            },
            0,
        );
        let tol = TOLERANCE_PX * (neighbour - lat).abs();
        if let Some(c) = (1..nx).find(|&c| (at(r, c) - lat).abs() > tol) {
            return Err(format!(
                "its latitudes are not separable: row {r} column {c} is {}°, column 0 {lat}°",
                at(r, c)
            ));
        }
        northings.push(lat_to_y(lat));
    }
    Ok(northings)
}

/// `d` degrees reduced to within half a turn of zero.
fn wrap_half_turn(d: f64) -> f64 {
    d - 360.0 * (d / 360.0).round()
}

/// Read `selection` of a 2-D `float` or `double` coordinate array and hand
/// its values, row-major, to the check for that type.
fn with_values<R>(
    nc: &NcFile,
    var: &NcVariable,
    selection: &NcSliceInfo,
    f32_check: impl FnOnce(&[f32]) -> Result<R, String>,
    f64_check: impl FnOnce(&[f64]) -> Result<R, String>,
) -> Result<R, String> {
    let name = var.name();
    let read = |e: netcdf_reader::Error| format!("coordinate '{name}': {e}");
    let contiguous = "a coordinate array read is not contiguous";
    match var.dtype() {
        NcType::Float => {
            let array = nc
                .read_variable_slice::<f32>(name, selection)
                .map_err(read)?;
            f32_check(array.as_slice().ok_or(contiguous)?)
        }
        NcType::Double => {
            let array = nc
                .read_variable_slice::<f64>(name, selection)
                .map_err(read)?;
            f64_check(array.as_slice().ok_or(contiguous)?)
        }
        other => Err(format!(
            "coordinate '{name}' is {other:?}, not float or double"
        )),
    }
}

fn var_dim(var: &NcVariable, index: usize) -> Result<u64, String> {
    var.dimensions()
        .get(index)
        .map(|d| d.size)
        .ok_or_else(|| format!("coordinate '{}' is not 2-D", var.name()))
}

fn to_usize(size: u64) -> Result<usize, String> {
    usize::try_from(size).map_err(|_| format!("dimension of {size} is too large"))
}

fn to_u32(size: usize) -> Result<u32, String> {
    u32::try_from(size).map_err(|_| format!("dimension of {size} is too large"))
}

#[cfg(test)]
mod tests {
    use super::{grid, longitudes, northings};
    use ds_core::geo::Crs;
    use ds_core::web_mercator::{lat_to_y, lon_to_x, y_to_lat};

    /// GMGSI's grid: 4999 columns and 3000 rows of 0.001256793 rad
    /// (8016 m on the EPSG:3857 sphere), from 179.99962°E and 72.71541°N.
    fn gmgsi_axes() -> (Vec<f64>, Vec<f64>) {
        let step = lon_to_x(0.072_008_957_2);
        let lon = (0..4999)
            .map(|c| 179.999_62 + c as f64 * 0.072_008_957_2)
            .collect();
        let y0 = lat_to_y(72.715_41);
        let rows = (0..3000).map(|r| y0 - r as f64 * step).collect();
        (lon, rows)
    }

    #[test]
    fn recognises_a_global_spherical_mercator_grid() {
        let (lon, rows) = gmgsi_axes();
        let grid = grid(&lon, &rows).unwrap();
        assert_eq!(grid.gt.crs, Crs::WebMercator);
        assert_eq!((grid.gt.width, grid.gt.height), (4999, 3000));
        assert!(
            (grid.col_period - 4999.378).abs() < 1e-3,
            "{}",
            grid.col_period
        );
        // A square pixel of ~8016 m, the first centre at 179.99962°E.
        assert!((grid.gt.pixel_width - 8016.0).abs() < 1.0);
        assert!((grid.gt.pixel_height - grid.gt.pixel_width).abs() < 1e-6);
        let x0 = grid.gt.origin_x + grid.gt.pixel_width / 2.0;
        assert!((x0 - lon_to_x(179.999_62)).abs() < 1e-6);
    }

    #[test]
    fn rejects_grids_that_are_not_a_global_mercator_mosaic() {
        let (lon, rows) = gmgsi_axes();
        // Latitude linear in degrees (a plain lat/lon grid), not northing.
        let degrees: Vec<f64> = (0..3000)
            .map(|r| lat_to_y(72.7 - r as f64 * 0.0485))
            .collect();
        assert!(grid(&lon, &degrees).unwrap_err().contains("not regular"));
        // Half the world.
        assert!(grid(&lon[..2500], &rows)
            .unwrap_err()
            .contains("do not span the globe"));
        // South-up rows, east-to-west columns.
        let south_up: Vec<f64> = rows.iter().rev().copied().collect();
        assert!(grid(&lon, &south_up)
            .unwrap_err()
            .contains("north to south"));
        let westward: Vec<f64> = lon.iter().rev().copied().collect();
        assert!(grid(&westward, &rows).unwrap_err().contains("west to east"));
        // One row a twentieth of a pixel off: past the 0.01 px tolerance;
        // a two-hundredth is within it.
        let step = rows[0] - rows[1];
        for (shift, ok) in [(0.05, false), (0.005, true)] {
            let mut bent = rows.clone();
            bent[1500] += shift * step;
            assert_eq!(grid(&lon, &bent).is_ok(), ok, "shift {shift} px");
        }
    }

    /// Longitudes unwrap across ±180° and must repeat down the rows (the
    /// same meridian may read 180° in one row and −180° in another);
    /// latitudes must repeat across the columns.
    #[test]
    fn arrays_must_be_separable() {
        let lon_row = [179.9f32, -179.9, -179.7, -179.5];
        let mut lon = [lon_row, lon_row].concat();
        lon[4] = -180.1; // 179.9 read the other way round
        let unwrapped = longitudes(&lon, 2, 4).unwrap();
        assert!((unwrapped[3] - 180.5).abs() < 1e-4, "{unwrapped:?}");
        lon[6] = -179.6;
        assert!(longitudes(&lon, 2, 4)
            .unwrap_err()
            .contains("not separable"));

        let y = |r: usize| y_to_lat(lat_to_y(60.0) - r as f64 * 8016.0);
        let lat: Vec<f64> = (0..3).flat_map(|r| [y(r), y(r)]).collect();
        let rows = northings(&lat, 3, 2).unwrap();
        assert!((rows[0] - rows[1] - 8016.0).abs() < 1e-6);
        let mut skewed = lat.clone();
        skewed[3] += 0.01;
        assert!(northings(&skewed, 3, 2)
            .unwrap_err()
            .contains("not separable"));
        assert!(northings(&[f64::NAN, 0.0], 1, 2).is_err());
    }
}
