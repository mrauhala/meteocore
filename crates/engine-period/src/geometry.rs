//! The working grid: a regular CRS84 grid over the source's extent, row 0
//! at the north, which may cross the antimeridian or go round the Earth
//! (#1379). Frames are read onto it and maps sample it nearest-cell.

use ds_core::map_engine::OutputCrs;
use ds_core::resample::ProjectionGrid;

/// The most cells `max_pixels` may allow: 384 MB of grids per window at 6
/// bytes a cell, and the GeoTIFF engine's own cap on one full-resolution
/// read (`engine_geotiff::reader::MAX_MAP_PIXELS`).
pub const MAX_CELLS: usize = 64_000_000;

/// A regular CRS84 grid of `nx × ny` cells over `[west, south, east,
/// north]`, cell `(col, row)` at index `row * nx + col`. `east` may exceed
/// 180 for a grid across the antimeridian (the box unwrapped, as the map
/// APIs pass one to a source).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridGeom {
    pub west: f64,
    pub south: f64,
    pub east: f64,
    pub north: f64,
    pub nx: u32,
    pub ny: u32,
}

impl GridGeom {
    /// The grid over a source's WGS84 extent at its own cell counts, native
    /// while they fit `max_cells`, else halved along the longer axis until
    /// they do (the nowcast's working-grid rule). `west > east` crosses the
    /// antimeridian.
    pub fn over(extent: [f64; 4], [mut nx, mut ny]: [u32; 2], max_cells: usize) -> Option<Self> {
        let [west, south, mut east, north] = extent;
        if !extent.iter().all(|v| v.is_finite()) || south >= north || nx == 0 || ny == 0 {
            return None;
        }
        if west > east {
            east += 360.0;
        }
        if west == east {
            return None;
        }
        while (nx as usize) * (ny as usize) > max_cells.max(1) && nx.max(ny) > 1 {
            if nx >= ny {
                nx = (nx / 2).max(1);
            } else {
                ny = (ny / 2).max(1);
            }
        }
        Some(Self {
            west,
            south: south.max(-90.0),
            east,
            north: north.min(90.0),
            nx,
            ny,
        })
    }

    pub fn cells(&self) -> usize {
        self.nx as usize * self.ny as usize
    }

    /// The box frames are read over: `east` past 180 across the seam.
    pub fn bbox(&self) -> [f64; 4] {
        [self.west, self.south, self.east, self.north]
    }

    /// The extent advertised: longitudes back in `[-180, 180]`, so a box
    /// across the seam has `west > east`.
    pub fn extent(&self) -> [f64; 4] {
        let east = if self.east > 180.0 {
            self.east - 360.0
        } else {
            self.east
        };
        [self.west, self.south, east, self.north]
    }

    /// The box of rows `rows` of the grid, every column: whole cells, its
    /// edges the grid's own row edges (the grid's north and south exactly
    /// at its ends), so a strip read at `nx × rows.len()` samples each cell
    /// where the whole frame's read does.
    pub fn strip_bbox(&self, rows: std::ops::Range<u32>) -> [f64; 4] {
        let edge = |row: u32| match row {
            0 => self.north,
            r if r >= self.ny => self.south,
            r => self.north - f64::from(r) * self.dy(),
        };
        [self.west, edge(rows.end), self.east, edge(rows.start)]
    }

    pub fn dx(&self) -> f64 {
        (self.east - self.west) / f64::from(self.nx)
    }

    pub fn dy(&self) -> f64 {
        (self.north - self.south) / f64::from(self.ny)
    }

    /// Whether the columns go round the Earth.
    pub fn is_global(&self) -> bool {
        self.east - self.west >= 360.0 - 1e-9
    }

    /// Fractional column of a longitude, continuous in longitude except
    /// half a turn from the grid's centre.
    pub fn col_frac(&self, lon: f64) -> f64 {
        if self.is_global() {
            return (lon - self.west).rem_euclid(360.0) / self.dx();
        }
        let half = (self.east - self.west) / 2.0;
        let centre = self.west + half;
        let offset = (lon - centre + 180.0).rem_euclid(360.0) - 180.0;
        (offset + half) / self.dx()
    }

    /// Fractional row of a latitude.
    pub fn row_frac(&self, lat: f64) -> f64 {
        (self.north - lat) / self.dy()
    }

    /// The column holding fractional column `frac`, wrapped on a global
    /// grid; `None` off a regional grid or for a non-finite input.
    pub fn col_at(&self, frac: f64) -> Option<u32> {
        if !frac.is_finite() {
            return None;
        }
        let col = frac.floor();
        if self.is_global() {
            return Some(col.rem_euclid(f64::from(self.nx)) as u32);
        }
        (col >= 0.0 && col < f64::from(self.nx)).then_some(col as u32)
    }

    /// The row holding fractional row `frac`; `None` off the grid.
    pub fn row_at(&self, frac: f64) -> Option<u32> {
        let row = frac.floor();
        (frac.is_finite() && row >= 0.0 && row < f64::from(self.ny)).then_some(row as u32)
    }

    /// The cell holding `(lon, lat)`; `None` off the grid.
    pub fn cell(&self, lon: f64, lat: f64) -> Option<usize> {
        let col = self.col_at(self.col_frac(lon))?;
        let row = self.row_at(self.row_frac(lat))?;
        Some(row as usize * self.nx as usize + col as usize)
    }

    /// Longitude of column `col`'s centre, in `(-180, 180]`.
    pub fn lon(&self, col: u32) -> f64 {
        let lon = self.west + (f64::from(col) + 0.5) * self.dx();
        let wrapped = (lon + 180.0).rem_euclid(360.0) - 180.0;
        if wrapped == -180.0 {
            180.0
        } else {
            wrapped
        }
    }

    /// Latitude of row `row`'s centre.
    pub fn lat(&self, row: u32) -> f64 {
        self.north - (f64::from(row) + 0.5) * self.dy()
    }
}

/// An output image sampled from the grid, nearest cell, `value(cell)` per
/// pixel and NaN off the grid: output pixel → cell through the output CRS's
/// axes (separable for CRS84 and Web Mercator, one projection per row and
/// column) or a coarse `ProjectionGrid` (projected output), never a
/// projection per pixel (Critical Rule 5).
pub fn sample(
    geom: &GridGeom,
    bbox: [f64; 4],
    width: u32,
    height: u32,
    output_crs: &OutputCrs,
    value: impl Fn(usize) -> f32,
) -> Vec<f32> {
    let mut out = vec![f32::NAN; width as usize * height as usize];
    let nx = geom.nx as usize;
    match output_crs {
        OutputCrs::Wgs84 | OutputCrs::WebMercator => {
            let mut unwrapped = bbox;
            if unwrapped[0] > unwrapped[2] {
                unwrapped[2] += 360.0;
            }
            let cols: Vec<Option<u32>> = (0..width)
                .map(|ox| {
                    let fx = (f64::from(ox) + 0.5) / f64::from(width);
                    geom.col_at(geom.col_frac(output_crs.project_node(unwrapped, fx, 0.5).0))
                })
                .collect();
            for oy in 0..height {
                let fy = (f64::from(oy) + 0.5) / f64::from(height);
                let lat = output_crs.project_node(unwrapped, 0.5, fy).1;
                let Some(row) = geom.row_at(geom.row_frac(lat)) else {
                    continue;
                };
                let base = row as usize * nx;
                let dst = &mut out[oy as usize * width as usize..][..width as usize];
                for (d, col) in dst.iter_mut().zip(&cols) {
                    if let Some(col) = col {
                        *d = value(base + *col as usize);
                    }
                }
            }
        }
        OutputCrs::Projected { .. } => {
            let to_world = |fx: f64, fy: f64| output_crs.project_node(bbox, fx, fy);
            let to_cell = |lon: f64, lat: f64| (geom.col_frac(lon), geom.row_frac(lat));
            let (projection, window) = if geom.is_global() {
                let window = (0, width.saturating_sub(1), 0, height.saturating_sub(1));
                let projection = ProjectionGrid::build_2d_periodic(
                    width,
                    height,
                    geom.nx,
                    geom.ny,
                    Some(f64::from(geom.nx)),
                    to_world,
                    to_cell,
                );
                (projection, window)
            } else {
                let Some(window) =
                    output_crs.footprint_window_in_image(bbox, geom.bbox(), width, height)
                else {
                    return out;
                };
                let projection = ProjectionGrid::build_2d_within(
                    width, height, window, geom.nx, geom.ny, to_world, to_cell,
                );
                (projection, window)
            };
            let (px_lo, px_hi, py_lo, py_hi) = window;
            for oy in py_lo..=py_hi.min(height.saturating_sub(1)) {
                for ox in px_lo..=px_hi.min(width.saturating_sub(1)) {
                    let (c, r) = projection.sample(ox, oy);
                    if let (Some(col), Some(row)) = (geom.col_at(c), geom.row_at(r)) {
                        out[oy as usize * width as usize + ox as usize] =
                            value(row as usize * nx + col as usize);
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 36 M-cell composite stays native at the default `max_pixels`
    /// (40 M) and at the cap; below its size it is halved.
    #[test]
    fn a_large_composite_stays_native_by_default() {
        let extent = [18.0, 55.0, 33.0, 71.0];
        let native = GridGeom::over(extent, [4963, 7316], 40_000_000).unwrap();
        assert_eq!(
            (native.nx, native.ny, native.cells()),
            (4963, 7316, 36_309_308)
        );
        assert_eq!(6 * native.cells(), 217_855_848, "bytes per window");
        let capped = GridGeom::over(extent, [4963, 7316], MAX_CELLS).unwrap();
        assert_eq!((capped.nx, capped.ny), (4963, 7316));
        let halved = GridGeom::over(extent, [4963, 7316], 20_000_000).unwrap();
        assert_eq!((halved.nx, halved.ny), (4963, 3658));
    }

    #[test]
    fn the_source_grid_is_halved_to_fit() {
        let g = GridGeom::over([0.42, 54.57, 37.99, 74.81], [3249, 1750], 4_000_000).unwrap();
        assert_eq!((g.nx, g.ny), (1624, 1750));
        let g = GridGeom::over([0.42, 54.57, 37.99, 74.81], [3249, 1750], 1_500_000).unwrap();
        assert_eq!((g.nx, g.ny), (1624, 875));
        assert!(GridGeom::over([0.0, 10.0, 1.0, 5.0], [10, 10], 100).is_none());
    }

    #[test]
    fn cells_are_found_by_position_across_the_seam() {
        let g = GridGeom::over([170.0, 10.0, -170.0, 20.0], [20, 10], 1000).unwrap();
        assert_eq!(g.extent(), [170.0, 10.0, -170.0, 20.0]);
        assert_eq!(g.cell(170.5, 19.5), Some(0));
        assert_eq!(g.cell(-179.5, 19.5), Some(10));
        assert_eq!(g.cell(180.5, 19.5), Some(10));
        assert_eq!(g.cell(-169.5, 19.5), None);
        assert_eq!(g.lon(10), -179.5);
        // A CRS84 image across the seam samples the cells either side.
        let image = sample(
            &g,
            [175.0, 10.0, -175.0, 20.0],
            10,
            10,
            &OutputCrs::Wgs84,
            |cell| cell as f32,
        );
        assert_eq!(image[0], 5.0);
        assert_eq!(image[9], 14.0);
    }

    #[test]
    fn a_crs84_image_samples_the_cell_under_each_pixel() {
        let g = GridGeom::over([0.0, 0.0, 4.0, 2.0], [4, 2], 100).unwrap();
        let image = sample(&g, [0.0, 0.0, 4.0, 2.0], 4, 2, &OutputCrs::Wgs84, |cell| {
            cell as f32
        });
        assert_eq!(image, vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);
        let outside = sample(&g, [10.0, 0.0, 14.0, 2.0], 4, 2, &OutputCrs::Wgs84, |c| {
            c as f32
        });
        assert!(outside.iter().all(|v| v.is_nan()));
    }
}
