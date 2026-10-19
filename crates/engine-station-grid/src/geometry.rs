//! The analysis grid: a regular CRS84 grid, row 0 north, which may cross the
//! antimeridian (`west > east`) or go round the whole Earth.

use ds_core::geo::EARTH_RADIUS_M;

/// The most cells an analysis grid may have: 64 MiB of `f32`.
pub const MAX_GRID_CELLS: usize = 16 * 1024 * 1024;

/// A regular CRS84 grid of `nx × ny` cells over `[west, south, east,
/// north]`, cell `(col, row)` at index `row * nx + col`, row 0 at the north.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridGeom {
    pub west: f64,
    pub south: f64,
    pub east: f64,
    pub north: f64,
    pub nx: u32,
    pub ny: u32,
    /// Cell width and height in degrees: the configured resolution, adjusted
    /// so whole cells cover the bbox exactly.
    pub dx: f64,
    pub dy: f64,
}

impl GridGeom {
    /// The grid over `bbox` at `resolution` degrees. `west > east` crosses
    /// the antimeridian; `[-180, s, 180, n]` goes round the Earth.
    pub fn new(bbox: [f64; 4], resolution: f64) -> Result<Self, String> {
        let [west, south, east, north] = bbox;
        if !bbox.iter().all(|v| v.is_finite()) {
            return Err("grid bbox must be finite numbers".into());
        }
        if !(-180.0..=180.0).contains(&west) || !(-180.0..=180.0).contains(&east) {
            return Err("grid bbox longitudes must be within -180..180".into());
        }
        if !(-90.0..=90.0).contains(&south) || !(-90.0..=90.0).contains(&north) || south >= north {
            return Err("grid bbox latitudes must be within -90..90, south below north".into());
        }
        if west == east {
            return Err("grid bbox west and east must differ".into());
        }
        if !(resolution.is_finite() && resolution > 0.0) {
            return Err("grid resolution must be a positive number of degrees".into());
        }
        let span = if west > east {
            east + 360.0 - west
        } else {
            east - west
        };
        let cells = |extent: f64| (extent / resolution).round().max(1.0);
        let (nx, ny) = (cells(span), cells(north - south));
        if nx * ny > MAX_GRID_CELLS as f64 {
            return Err(format!(
                "grid of {nx} × {ny} cells exceeds the {MAX_GRID_CELLS}-cell limit; \
                 coarsen the resolution or narrow the bbox"
            ));
        }
        Ok(Self {
            west,
            south,
            east,
            north,
            nx: nx as u32,
            ny: ny as u32,
            dx: span / nx,
            dy: (north - south) / ny,
        })
    }

    /// `[west, south, east, north]`.
    pub fn bbox(&self) -> [f64; 4] {
        [self.west, self.south, self.east, self.north]
    }

    /// Degrees of longitude the grid spans.
    pub fn lon_span(&self) -> f64 {
        self.dx * self.nx as f64
    }

    /// Whether the columns go round the Earth, so column `nx` is column 0.
    pub fn is_global(&self) -> bool {
        self.lon_span() >= 360.0 - 1e-9
    }

    pub fn cells(&self) -> usize {
        self.nx as usize * self.ny as usize
    }

    /// Longitude of column `col`'s centre, in `(-180, 180]`.
    pub fn lon(&self, col: u32) -> f64 {
        wrap_lon(self.west + (col as f64 + 0.5) * self.dx)
    }

    /// Latitude of row `row`'s centre.
    pub fn lat(&self, row: u32) -> f64 {
        self.north - (row as f64 + 0.5) * self.dy
    }

    /// Fractional column of a longitude, continuous in longitude everywhere
    /// but half a turn from the grid's centre (outside a regional grid). A
    /// global grid's column runs from 0 at `west` up to `nx`; callers wrap
    /// it modulo `nx`.
    pub fn col_frac(&self, lon: f64) -> f64 {
        if self.is_global() {
            return (lon - self.west).rem_euclid(360.0) / self.dx;
        }
        let half = self.lon_span() / 2.0;
        let centre = self.west + half;
        let offset = (lon - centre + 180.0).rem_euclid(360.0) - 180.0;
        (offset + half) / self.dx
    }

    /// Fractional row of a latitude.
    pub fn row_frac(&self, lat: f64) -> f64 {
        (self.north - lat) / self.dy
    }

    /// The column holding fractional column `frac`, wrapped on a global
    /// grid; `None` off a regional grid or for a non-finite input.
    pub fn col_at(&self, frac: f64) -> Option<u32> {
        if !frac.is_finite() {
            return None;
        }
        let col = frac.floor();
        if self.is_global() {
            return Some(col.rem_euclid(self.nx as f64) as u32);
        }
        (col >= 0.0 && col < self.nx as f64).then_some(col as u32)
    }

    /// The row holding fractional row `frac`; `None` off the grid.
    pub fn row_at(&self, frac: f64) -> Option<u32> {
        let row = frac.floor();
        (frac.is_finite() && row >= 0.0 && row < self.ny as f64).then_some(row as u32)
    }

    /// Index of the cell holding `(lon, lat)`; `None` off the grid.
    pub fn cell(&self, lon: f64, lat: f64) -> Option<usize> {
        let col = self.col_at(self.col_frac(lon))?;
        let row = self.row_at(self.row_frac(lat))?;
        Some(row as usize * self.nx as usize + col as usize)
    }

    /// The box a station query asks the source for: the grid widened by the
    /// search radius, so a station just outside still reaches the cells
    /// near the edge. Longitude widens by the most a point within the
    /// radius can differ at the box's poleward latitude; a box that reaches
    /// a pole or goes round the Earth covers every longitude. `west > east`
    /// crosses the antimeridian.
    pub fn query_bbox(&self, radius_m: f64) -> [f64; 4] {
        let delta = (radius_m / EARTH_RADIUS_M).min(std::f64::consts::PI);
        let delta_deg = delta.to_degrees();
        let south = (self.south - delta_deg).max(-90.0);
        let north = (self.north + delta_deg).min(90.0);
        let poleward = south.abs().max(north.abs()).to_radians();
        let reach = delta.sin() / poleward.cos();
        let whole = [-180.0, south, 180.0, north];
        if reach.is_nan() || reach >= 1.0 || south <= -90.0 || north >= 90.0 {
            return whole;
        }
        let dlon = reach.asin().to_degrees();
        if self.lon_span() + 2.0 * dlon >= 360.0 {
            return whole;
        }
        let mut west = self.west - dlon;
        let mut east = self.east + dlon;
        if west < -180.0 {
            west += 360.0;
        }
        if east > 180.0 {
            east -= 360.0;
        }
        [west, south, east, north]
    }
}

/// `lon` in `(-180, 180]`.
pub fn wrap_lon(lon: f64) -> f64 {
    let wrapped = (lon + 180.0).rem_euclid(360.0) - 180.0;
    if wrapped == -180.0 {
        180.0
    } else {
        wrapped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_cells_cover_the_bbox() {
        let g = GridGeom::new([-25.0, 22.0, 50.0, 72.0], 0.05).unwrap();
        assert_eq!((g.nx, g.ny), (1500, 1000));
        assert!((g.dx - 0.05).abs() < 1e-12 && (g.dy - 0.05).abs() < 1e-12);
        assert_eq!(g.cell(-25.0, 72.0), Some(0));
        assert_eq!(g.cell(49.999, 22.001), Some(g.cells() - 1));
        assert_eq!(g.cell(50.0, 40.0), None);
        assert_eq!(g.cell(0.0, 21.99), None);
        assert!(GridGeom::new([0.0, 0.0, 10.0, 10.0], 0.0).is_err());
        assert!(GridGeom::new([0.0, 10.0, 10.0, 0.0], 1.0).is_err());
        assert!(GridGeom::new([-180.0, -90.0, 180.0, 90.0], 0.01).is_err());
    }

    /// A grid across the antimeridian finds cells either side of it, from
    /// either spelling of a longitude past 180°.
    #[test]
    fn seam_grid_cells() {
        let g = GridGeom::new([170.0, 10.0, -170.0, 20.0], 1.0).unwrap();
        assert_eq!((g.nx, g.ny), (20, 10));
        assert!(!g.is_global());
        assert_eq!(g.cell(170.5, 19.5), Some(0));
        assert_eq!(g.cell(179.5, 19.5), Some(9));
        assert_eq!(g.cell(-179.5, 19.5), Some(10));
        assert_eq!(g.cell(180.5, 19.5), Some(10));
        assert_eq!(g.cell(-170.5, 19.5), Some(19));
        assert_eq!(g.cell(-169.5, 19.5), None);
        assert_eq!(g.cell(169.5, 19.5), None);
        assert_eq!(g.lon(10), -179.5);
        // Continuous across the seam: 179.9 and -179.9 are 0.2 columns apart.
        assert!((g.col_frac(-179.9) - g.col_frac(179.9) - 0.2).abs() < 1e-9);
    }

    #[test]
    fn global_grid_wraps_columns() {
        let g = GridGeom::new([-180.0, -90.0, 180.0, 90.0], 10.0).unwrap();
        assert!(g.is_global());
        assert_eq!(g.cell(-180.0, 0.0), g.cell(180.0, 0.0));
        assert_eq!(g.col_at(36.2), Some(0));
        assert_eq!(g.col_at(-0.5), Some(35));
        assert_eq!(g.cell(0.0, 90.0), Some(18));
        assert_eq!(g.query_bbox(100_000.0), [-180.0, -90.0, 180.0, 90.0]);
    }

    /// The source query box: widened by the radius, across the seam where
    /// the widening passes it, and every longitude at a pole.
    #[test]
    fn query_bbox_widens_by_the_radius() {
        let g = GridGeom::new([20.0, 60.0, 30.0, 70.0], 0.1).unwrap();
        let [w, s, e, n] = g.query_bbox(111_195.0);
        assert!((s - 59.0).abs() < 1e-3 && (n - 71.0).abs() < 1e-3);
        // At 71°N one degree of arc spans asin(sin 1° / cos 71°) ≈ 3.07°.
        assert!((w - (20.0 - 3.074)).abs() < 1e-2, "{w}");
        assert!((e - (30.0 + 3.074)).abs() < 1e-2, "{e}");

        let seam = GridGeom::new([175.0, 0.0, 179.0, 10.0], 0.1).unwrap();
        let [w, _, e, _] = seam.query_bbox(500_000.0);
        assert!(w > e, "crosses the antimeridian: {w} {e}");

        let polar = GridGeom::new([0.0, 80.0, 10.0, 89.0], 0.1).unwrap();
        assert_eq!(polar.query_bbox(200_000.0)[0], -180.0);
    }
}
