//! The terrain and the land/sea mask a station grid corrects with (#1206):
//! other collections' raster `MapEngine`s, named by id
//! (`elevation_source`, `land_mask_source`) and sampled ONCE per engine
//! onto the analysis grid's cell centres through
//! `MapEngine::get_raster_tile`.
//!
//! - A request for a CRS84 box `[w, s, e, n]` at `width × height` draws
//!   pixel `(i, j)` at `(w + (i + ½)(e − w)/width, n − (j + ½)(n − s)/height)`
//!   (`OutputCrs::project_node` at the pixel centre, as every engine samples
//!   through `ProjectionGrid`). Asked for the grid's own box at `nx × ny`,
//!   each pixel is a cell centre. A box across the antimeridian is passed
//!   unwrapped, `east > 180`, as the map APIs pass it.
//! - The terrain covers more than the grid: the grid widened by whole cells
//!   to every station within the search radius ([`dem_grid`]), so a station
//!   whose source states no height takes the terrain at its position.
//! - Units come from the collection's own metadata: heights in `m`, `km`,
//!   `ft` or `gpm` (geopotential metres, read as metres: they differ by
//!   under 0.1 % at the surface), a mask as a fraction (`1`, `proportion`,
//!   no unit: inside from 0.5) or in `%` (inside from 50). Anything else is a
//!   load error, never a guess.

use std::sync::Arc;

use ds_core::geo::EARTH_RADIUS_M;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterInfo};
use ds_core::units::canonical_unit;

use crate::geometry::GridGeom;

/// What a raster collection is read as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Elevation,
    LandMask,
}

impl Kind {
    fn key(self) -> &'static str {
        match self {
            Kind::Elevation => "elevation_source",
            Kind::LandMask => "land_mask_source",
        }
    }

    fn parameter_key(self) -> &'static str {
        match self {
            Kind::Elevation => "elevation_parameter",
            Kind::LandMask => "land_mask_parameter",
        }
    }
}

/// How a raster's values read, from its metadata.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Reading {
    /// The parameter to ask for: `None` for a single-parameter engine.
    request: Option<String>,
    /// Metres per unit (elevation) or the threshold from which a cell is
    /// inside (mask).
    factor: f64,
}

/// A raster collection named in the config.
pub(crate) struct RasterSource {
    pub id: String,
    pub kind: Kind,
    engine: Arc<dyn MapEngine>,
    parameter: Option<String>,
}

impl RasterSource {
    pub fn new(id: &str, kind: Kind, engine: Arc<dyn MapEngine>, parameter: Option<&str>) -> Self {
        Self {
            id: id.to_string(),
            kind,
            engine,
            parameter: parameter.map(str::to_string),
        }
    }

    /// The parameter and unit rule from the collection's current metadata.
    /// `Ok(None)` while it reports no parameter yet (a remote collection
    /// before its first scan); `Err` for an ambiguous or unknown parameter
    /// or a unit this kind cannot read.
    pub fn check(&self) -> Result<Option<Reading>, String> {
        check(&self.engine.raster_info(), self)
    }

    /// The raster at `geom`'s cell centres, row-major from the north;
    /// `None` for nodata. Synchronous source I/O: the poll loop only.
    fn sample(&self, geom: &GridGeom) -> Result<(Vec<Option<f64>>, f64), String> {
        let id = &self.id;
        let key = self.kind.key();
        let reading = self
            .check()?
            .ok_or_else(|| format!("{key} '{id}' has no data yet"))?;
        let bbox = [
            geom.west,
            geom.south,
            geom.west + geom.lon_span(),
            geom.north,
        ];
        let tile = self
            .engine
            .get_raster_tile(
                bbox,
                geom.nx,
                geom.ny,
                None,
                &OutputCrs::Wgs84,
                reading.request.as_deref(),
                None,
                None,
            )
            .map_err(|e| format!("{key} '{id}' could not be read: {e}"))?;
        let cells = geom.cells();
        if (tile.width, tile.height) != (geom.nx, geom.ny) || tile.values.len() != cells {
            return Err(format!(
                "{key} '{id}' answered {} × {} values for a {} × {} grid",
                tile.width, tile.height, geom.nx, geom.ny
            ));
        }
        let values: Vec<Option<f64>> = (0..cells)
            .map(|i| tile.values.value_at(i).filter(|v| v.is_finite()))
            .collect();
        if values.iter().all(Option::is_none) {
            return Err(format!("{key} '{id}' has no value over the analysis grid"));
        }
        Ok((values, reading.factor))
    }

    /// Terrain heights in metres over `geom` (NaN for nodata).
    pub fn sample_metres(&self, geom: &GridGeom) -> Result<Vec<f32>, String> {
        debug_assert_eq!(self.kind, Kind::Elevation);
        let (values, metres_per_unit) = self.sample(geom)?;
        Ok(values
            .into_iter()
            .map(|v| v.map_or(f32::NAN, |v| (v * metres_per_unit) as f32))
            .collect())
    }

    /// Per cell of `geom`, whether it is inside the mask: a value at or
    /// above the threshold. Nodata is outside.
    pub fn sample_inside(&self, geom: &GridGeom) -> Result<Vec<bool>, String> {
        debug_assert_eq!(self.kind, Kind::LandMask);
        let (values, threshold) = self.sample(geom)?;
        Ok(values
            .into_iter()
            .map(|v| v.is_some_and(|v| v >= threshold))
            .collect())
    }
}

fn check(info: &RasterInfo, source: &RasterSource) -> Result<Option<Reading>, String> {
    let id = &source.id;
    let key = source.kind.key();
    // (name, unit) of every parameter the collection reports.
    let listed: Vec<(&str, &str)> = if info.parameters.is_empty() {
        if info.parameter.is_empty() {
            Vec::new()
        } else {
            vec![(info.parameter.as_str(), info.unit.as_str())]
        }
    } else {
        info.parameters
            .iter()
            .map(|p| (p.name.as_str(), p.unit.as_str()))
            .collect()
    };
    if listed.is_empty() {
        return Ok(None);
    }
    let (name, unit) = match source.parameter.as_deref() {
        Some(wanted) => *listed
            .iter()
            .find(|(name, _)| *name == wanted)
            .or_else(|| {
                listed
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(wanted))
            })
            .ok_or_else(|| {
                format!(
                    "{} '{wanted}' is not a parameter of {key} '{id}'; it has {}",
                    source.kind.parameter_key(),
                    names(&listed)
                )
            })?,
        None if listed.len() == 1 => listed[0],
        None => {
            return Err(format!(
                "{key} '{id}' has {} parameters ({}); name the one to read with {}",
                listed.len(),
                names(&listed),
                source.kind.parameter_key()
            ))
        }
    };
    let factor = match source.kind {
        Kind::Elevation => metres_per_unit(unit).ok_or_else(|| {
            format!(
                "{key} '{id}' states its heights ('{name}') in '{unit}'; they must be in m, km, \
                 ft or gpm"
            )
        })?,
        Kind::LandMask => mask_threshold(unit).ok_or_else(|| {
            format!(
                "{key} '{id}' states its mask ('{name}') in '{unit}'; it must be a fraction \
                 (unit '1', 'proportion' or none) or a percentage ('%')"
            )
        })?,
    };
    Ok(Some(Reading {
        request: (!info.parameters.is_empty()).then(|| name.to_string()),
        factor,
    }))
}

fn names(listed: &[(&str, &str)]) -> String {
    listed
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Metres per unit of a stated height unit; `None` for anything else.
pub(crate) fn metres_per_unit(unit: &str) -> Option<f64> {
    match canonical_unit(unit).as_ref() {
        "m" | "metre" | "metres" | "meter" | "meters" | "gpm" => Some(1.0),
        "km" => Some(1000.0),
        "ft" | "foot" | "feet" => Some(0.3048),
        _ => None,
    }
}

/// The value from which a mask cell is inside, by the mask's unit.
pub(crate) fn mask_threshold(unit: &str) -> Option<f64> {
    match canonical_unit(unit).as_ref() {
        "" | "1" | "proportion" | "Proportion" | "(0 - 1)" => Some(0.5),
        "%" => Some(50.0),
        _ => None,
    }
}

/// The terrain grid: the analysis grid widened by whole cells to every
/// point within `radius_m`, so the cells stay the analysis grid's
/// (`(col0, row0)` is analysis cell `(0, 0)`). Rows stop at the poles. A
/// global grid widens only in latitude; a regional one stays under a whole
/// turn of longitude.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct DemGrid {
    pub geom: GridGeom,
    pub col0: u32,
    pub row0: u32,
}

pub(crate) fn dem_grid(geom: &GridGeom, radius_m: f64) -> DemGrid {
    let delta = (radius_m / EARTH_RADIUS_M).min(std::f64::consts::PI);
    let delta_deg = delta.to_degrees();
    let rows = (delta_deg / geom.dy).ceil();
    let room = |extent: f64| (extent / geom.dy + 1e-9).floor();
    let north_rows = rows.min(room(90.0 - geom.north)).max(0.0) as u32;
    let south_rows = rows.min(room(geom.south + 90.0)).max(0.0) as u32;
    let north = (geom.north + north_rows as f64 * geom.dy).min(90.0);
    let south = (geom.south - south_rows as f64 * geom.dy).max(-90.0);
    let cols = if geom.is_global() {
        0
    } else {
        let poleward = south.abs().max(north.abs()).to_radians();
        let reach = delta.sin() / poleward.cos();
        let dlon = if reach.is_finite() && reach < 1.0 {
            reach.asin().to_degrees()
        } else {
            180.0
        };
        let wanted = (dlon / geom.dx).ceil() as u32;
        // Strictly under a whole turn, so the grid stays regional.
        let most = ((360.0 / geom.dx) - 1e-9).floor() as u32;
        wanted.min(most.saturating_sub(geom.nx) / 2)
    };
    let mut west = geom.west - cols as f64 * geom.dx;
    if west < -180.0 {
        west += 360.0;
    }
    let nx = geom.nx + 2 * cols;
    let span = nx as f64 * geom.dx;
    DemGrid {
        geom: GridGeom {
            west,
            south,
            east: crate::geometry::wrap_lon(west + span),
            north,
            nx,
            ny: geom.ny + north_rows + south_rows,
            dx: geom.dx,
            dy: geom.dy,
        },
        col0: cols,
        row0: north_rows,
    }
}

/// Terrain heights in metres on a [`DemGrid`].
pub(crate) struct Dem {
    pub grid: DemGrid,
    pub metres: Vec<f32>,
}

impl Dem {
    /// The height of analysis cell `(col, row)`; NaN where the terrain has
    /// none.
    pub fn at_cell(&self, col: u32, row: u32) -> f32 {
        let g = &self.grid;
        let i = (row + g.row0) as usize * g.geom.nx as usize + (col + g.col0) as usize;
        self.metres.get(i).copied().unwrap_or(f32::NAN)
    }

    /// The terrain height at a point, bilinear between the four cell
    /// centres around it (wrapping a global grid's columns, clamped within
    /// half a cell of a regional grid's edge); the cell under the point
    /// when one of them is nodata; `None` off the grid or over nodata.
    pub fn height_at(&self, lon: f64, lat: f64) -> Option<f64> {
        let g = &self.grid.geom;
        let (fc, fr) = (g.col_frac(lon), g.row_frac(lat));
        if !(fc.is_finite() && fr.is_finite()) {
            return None;
        }
        let (nx, ny) = (g.nx as i64, g.ny as i64);
        let global = g.is_global();
        if fr < 0.0 || fr > ny as f64 || (!global && (fc < 0.0 || fc > nx as f64)) {
            return None;
        }
        let value = |c: i64, r: i64| -> Option<f64> {
            let c = if global {
                c.rem_euclid(nx)
            } else {
                c.clamp(0, nx - 1)
            };
            let r = r.clamp(0, ny - 1);
            let v = self.metres[(r * nx + c) as usize];
            v.is_finite().then_some(f64::from(v))
        };
        let (x, y) = (fc - 0.5, fr - 0.5);
        let (c0, r0) = (x.floor() as i64, y.floor() as i64);
        let (tx, ty) = (x - c0 as f64, y - r0 as f64);
        let corners = (
            value(c0, r0),
            value(c0 + 1, r0),
            value(c0, r0 + 1),
            value(c0 + 1, r0 + 1),
        );
        if let (Some(a), Some(b), Some(c), Some(d)) = corners {
            let top = a + (b - a) * tx;
            let bottom = c + (d - c) * tx;
            return Some(top + (bottom - top) * ty);
        }
        value(fc.floor() as i64, fr.floor() as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The terrain grid shares the analysis cells and reaches every point
    /// within the radius, clamped at a pole.
    #[test]
    fn dem_grid_widens_by_whole_cells() {
        let g = GridGeom::new([24.0, 60.0, 25.0, 60.4], 0.1).unwrap();
        let d = dem_grid(&g, 100_000.0);
        // 100 km is 0.899° of arc: 9 rows each way.
        assert_eq!((d.row0, d.geom.ny), (9, 4 + 18));
        // At 61.3°N that arc spans asin(sin 0.899° / cos 61.3°) ≈ 1.88°.
        assert_eq!(d.col0, 19);
        assert_eq!(d.geom.nx, 10 + 38);
        // Analysis cell (0, 0)'s centre is terrain cell (col0, row0)'s.
        assert!((d.geom.west + (d.col0 as f64 + 0.5) * d.geom.dx - g.lon(0)).abs() < 1e-9);
        assert!((d.geom.north - (d.row0 as f64 + 0.5) * d.geom.dy - g.lat(0)).abs() < 1e-9);

        let polar = GridGeom::new([0.0, 80.0, 10.0, 89.5], 0.5).unwrap();
        let d = dem_grid(&polar, 200_000.0);
        assert_eq!(d.row0, 1, "one row of room below the pole");
        assert!((d.geom.north - 90.0).abs() < 1e-9);
        assert!(d.geom.lon_span() < 360.0 && !d.geom.is_global());

        let global = GridGeom::new([-180.0, -60.0, 180.0, 60.0], 1.0).unwrap();
        let d = dem_grid(&global, 300_000.0);
        assert_eq!((d.col0, d.geom.nx), (0, 360));
        assert!(d.geom.is_global());
    }

    /// Across the antimeridian the terrain grid starts west of 180° and
    /// runs on past it, unwrapped in the request box.
    #[test]
    fn dem_grid_across_the_antimeridian() {
        let g = GridGeom::new([170.0, 10.0, -170.0, 20.0], 0.5).unwrap();
        let d = dem_grid(&g, 100_000.0);
        assert!(
            d.geom.west > 160.0 && d.geom.west < 170.0,
            "{}",
            d.geom.west
        );
        assert!(d.geom.west + d.geom.lon_span() > 190.0);
        // The analysis cell either side of 180° maps to its terrain cell.
        let east = g.cell(-179.75, 15.25).unwrap();
        let (col, row) = ((east % 40) as u32, (east / 40) as u32);
        let lon = d.geom.west + (col + d.col0) as f64 * d.geom.dx + 0.5 * d.geom.dx;
        let lat = d.geom.north - (row + d.row0) as f64 * d.geom.dy - 0.5 * d.geom.dy;
        assert!((lon - 180.25).abs() < 1e-9 && (lat - 15.25).abs() < 1e-9);

        let near_seam = GridGeom::new([-179.0, 0.0, -170.0, 5.0], 0.5).unwrap();
        let d = dem_grid(&near_seam, 300_000.0);
        assert!(d.geom.west > 170.0, "wrapped west of 180°: {}", d.geom.west);
    }

    /// Bilinear between cell centres; the cell under the point next to
    /// nodata; nothing off the grid.
    #[test]
    fn height_at_a_point() {
        let g = GridGeom::new([0.0, 0.0, 2.0, 2.0], 1.0).unwrap();
        let grid = DemGrid {
            geom: g,
            col0: 0,
            row0: 0,
        };
        // Row 0 is the north row.
        let dem = Dem {
            grid,
            metres: vec![100.0, 200.0, 300.0, 400.0],
        };
        assert_eq!(dem.height_at(0.5, 1.5), Some(100.0));
        assert_eq!(dem.height_at(1.0, 1.5), Some(150.0));
        assert_eq!(dem.height_at(1.0, 1.0), Some(250.0));
        // Clamped within half a cell of the edge.
        assert_eq!(dem.height_at(0.1, 1.9), Some(100.0));
        assert_eq!(dem.height_at(2.5, 1.0), None);
        let holey = Dem {
            grid,
            metres: vec![100.0, f32::NAN, 300.0, 400.0],
        };
        assert_eq!(holey.height_at(0.9, 1.9), Some(100.0));
        assert_eq!(holey.height_at(1.1, 1.9), None);
        // The south row has no nodata: bilinear along it.
        let south = holey.height_at(1.1, 0.4).unwrap();
        assert!((south - 360.0).abs() < 1e-9, "{south}");
    }

    #[test]
    fn units_from_metadata() {
        assert_eq!(metres_per_unit("m"), Some(1.0));
        assert_eq!(metres_per_unit(" metres "), Some(1.0));
        assert_eq!(metres_per_unit("km"), Some(1000.0));
        assert_eq!(metres_per_unit("ft"), Some(0.3048));
        assert_eq!(metres_per_unit("gpm"), Some(1.0));
        assert_eq!(metres_per_unit(""), None);
        assert_eq!(metres_per_unit("m2 s-2"), None);
        assert_eq!(mask_threshold("1"), Some(0.5));
        assert_eq!(mask_threshold(""), Some(0.5));
        assert_eq!(mask_threshold("percent"), Some(50.0));
        assert_eq!(mask_threshold("(0 - 1)"), Some(0.5));
        assert_eq!(mask_threshold("m"), None);
    }
}
