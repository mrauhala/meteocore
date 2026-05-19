//! Coarse-grid output→source pixel mapping for raster resampling.
//!
//! Rendering a WMS/Maps/Tiles tile resamples the source raster onto the output
//! grid. The naive approach projects every output pixel through the CRS forward
//! transform, which for a projected source (e.g. EPSG:3067 Transverse Mercator)
//! costs roughly a dozen transcendental ops *per pixel* — the dominant
//! per-render CPU cost (issue #203).
//!
//! The output→source pixel mapping is smooth, so [`ProjectionGrid`] evaluates
//! the exact projection only at the nodes of a coarse grid and bilinearly
//! interpolates it for every interior pixel. For a fullscreen render this
//! turns ~2 million projection calls into a few thousand.
//!
//! Bilinear interpolation error depends on how much the projection *curves*
//! within a cell, which is a function of the geographic span per cell, not the
//! pixel count — a zoomed-out tile spanning tens of degrees curves far more per
//! cell than a zoomed-in one. So the grid density is chosen **adaptively**:
//! [`ProjectionGrid::build`] starts from a pixel-based density and refines
//! until the measured interpolation error over the on-raster region is below
//! [`MAX_INTERP_ERROR_PX`] — well under the 0.5 px that nearest-neighbour
//! resampling can resolve. See the `grid_matches_exact_*` tests.

use ds_core::geo::GeoTransform;

/// Initial node spacing, in output pixels. The adaptive refinement in
/// [`ProjectionGrid::build`] only ever *increases* density from here, so this
/// is just a starting point that keeps the common (zoomed-in) case at one
/// build with no refinement.
const GRID_STEP_PX: u32 = 32;

/// Lower bound on grid cells per axis (the starting density floor).
const MIN_CELLS: u32 = 4;

/// Upper bound on grid cells per axis, capping the node count — and hence the
/// projection-call count — for extreme zoomed-out viewports.
const MAX_CELLS: u32 = 256;

/// Interpolation-error budget, in source pixels. Refinement stops once the
/// estimated error over the on-raster region drops below this. Kept well under
/// the 0.5 px resolution of nearest-neighbour resampling.
const MAX_INTERP_ERROR_PX: f64 = 0.2;

/// Coarse grid of exact output→source pixel correspondences, with bilinear
/// interpolation for interior pixels.
pub(crate) struct ProjectionGrid {
    cells_x: usize,
    cells_y: usize,
    /// Output pixels per grid cell along each axis.
    cell_w: f64,
    cell_h: f64,
    /// Nodes per row (`cells_x + 1`).
    stride: usize,
    /// Exact `(col, row)` source pixel coordinates at each node, row-major,
    /// `(cells_y + 1)` rows of `stride` nodes.
    nodes: Vec<(f64, f64)>,
}

impl ProjectionGrid {
    /// Build the grid for an output image of `width`×`height` pixels.
    ///
    /// `lon_at` maps a fractional x position in `[0, 1]` to longitude (deg) and
    /// `lat_at` maps a fractional y position in `[0, 1]` to latitude (deg).
    /// Passing closures (rather than a bbox) lets the caller capture a
    /// non-linear output-axis parameterisation, e.g. the equal-Y-meters
    /// spacing of Web Mercator. `gt` projects geographic coordinates to source
    /// pixels.
    ///
    /// The grid density is refined until the estimated bilinear-interpolation
    /// error over the on-raster region is below [`MAX_INTERP_ERROR_PX`], or the
    /// [`MAX_CELLS`] cap is reached.
    pub(crate) fn build(
        gt: &GeoTransform,
        width: u32,
        height: u32,
        lon_at: impl Fn(f64) -> f64,
        lat_at: impl Fn(f64) -> f64,
    ) -> Self {
        let mut cells_x = width.div_ceil(GRID_STEP_PX).clamp(MIN_CELLS, MAX_CELLS);
        let mut cells_y = height.div_ceil(GRID_STEP_PX).clamp(MIN_CELLS, MAX_CELLS);
        loop {
            let grid = Self::with_cells(gt, width, height, cells_x, cells_y, &lon_at, &lat_at);
            let at_cap = cells_x >= MAX_CELLS && cells_y >= MAX_CELLS;
            if at_cap || grid.estimate_error(gt, &lon_at, &lat_at) <= MAX_INTERP_ERROR_PX {
                return grid;
            }
            // Halving the cell size quarters the bilinear error.
            cells_x = (cells_x * 2).min(MAX_CELLS);
            cells_y = (cells_y * 2).min(MAX_CELLS);
        }
    }

    /// Build a grid with an explicit cell count (no refinement).
    fn with_cells(
        gt: &GeoTransform,
        width: u32,
        height: u32,
        cells_x: u32,
        cells_y: u32,
        lon_at: impl Fn(f64) -> f64,
        lat_at: impl Fn(f64) -> f64,
    ) -> Self {
        let cells_x = cells_x as usize;
        let cells_y = cells_y as usize;
        let stride = cells_x + 1;

        let mut nodes = Vec::with_capacity(stride * (cells_y + 1));
        for j in 0..=cells_y {
            let lat = lat_at(j as f64 / cells_y as f64);
            for i in 0..=cells_x {
                let lon = lon_at(i as f64 / cells_x as f64);
                nodes.push(gt.world_to_pixel_f64(lon, lat));
            }
        }

        ProjectionGrid {
            cells_x,
            cells_y,
            cell_w: width as f64 / cells_x as f64,
            cell_h: height as f64 / cells_y as f64,
            stride,
            nodes,
        }
    }

    /// Estimate the worst-case bilinear-interpolation error, in source pixels.
    ///
    /// For each cell it compares the interpolated value at the cell centre
    /// (where the bilinear residual of a smooth function peaks) against the
    /// exact projection. Cells whose centre projects far outside the raster are
    /// skipped — their pixels resolve to nodata regardless of interpolation
    /// error, so refining for them would be wasted work (and could needlessly
    /// hit the [`MAX_CELLS`] cap on a viewport that mostly misses the raster).
    fn estimate_error(
        &self,
        gt: &GeoTransform,
        lon_at: impl Fn(f64) -> f64,
        lat_at: impl Fn(f64) -> f64,
    ) -> f64 {
        // Generous on-raster window: within one raster-size of the data.
        let (w, h) = (gt.width as f64, gt.height as f64);
        let in_window = |c: f64, r: f64| c > -w && c < 2.0 * w && r > -h && r < 2.0 * h;

        let mut max = 0.0_f64;
        for j in 0..self.cells_y {
            let lat = lat_at((j as f64 + 0.5) / self.cells_y as f64);
            for i in 0..self.cells_x {
                let lon = lon_at((i as f64 + 0.5) / self.cells_x as f64);
                let (ec, er) = gt.world_to_pixel_f64(lon, lat);
                if !in_window(ec, er) {
                    continue;
                }
                // Bilinear value at the cell centre is the mean of its corners.
                let n = j * self.stride + i;
                let (c00, c10) = (self.nodes[n], self.nodes[n + 1]);
                let (c01, c11) = (self.nodes[n + self.stride], self.nodes[n + self.stride + 1]);
                let ic = (c00.0 + c10.0 + c01.0 + c11.0) / 4.0;
                let ir = (c00.1 + c10.1 + c01.1 + c11.1) / 4.0;
                max = max.max((ic - ec).abs()).max((ir - er).abs());
            }
        }
        max
    }

    /// Interpolated source `(col, row)` for the centre of output pixel
    /// `(ox, oy)`. Coordinates are fractional and unclamped — the caller floors
    /// and bounds-checks them, exactly as [`GeoTransform::world_to_pixel`] does.
    ///
    /// If a grid node is non-finite (only reachable via a degenerate
    /// `GeoTransform`, e.g. a zero pixel size) the result is non-finite; the
    /// caller must finite-check before use.
    pub(crate) fn sample(&self, ox: u32, oy: u32) -> (f64, f64) {
        // Position of the pixel centre in grid-cell units.
        let gx = (ox as f64 + 0.5) / self.cell_w;
        let gy = (oy as f64 + 0.5) / self.cell_h;
        // Enclosing cell, clamped so the +1 node lookups stay in bounds.
        let cx = (gx.floor() as usize).min(self.cells_x - 1);
        let cy = (gy.floor() as usize).min(self.cells_y - 1);
        let tx = gx - cx as f64;
        let ty = gy - cy as f64;

        let i00 = cy * self.stride + cx;
        let (c00, c10) = (self.nodes[i00], self.nodes[i00 + 1]);
        let (c01, c11) = (
            self.nodes[i00 + self.stride],
            self.nodes[i00 + self.stride + 1],
        );

        let col = bilerp(c00.0, c10.0, c01.0, c11.0, tx, ty);
        let row = bilerp(c00.1, c10.1, c01.1, c11.1, tx, ty);
        (col, row)
    }
}

#[inline]
fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

#[inline]
fn bilerp(v00: f64, v10: f64, v01: f64, v11: f64, tx: f64, ty: f64) -> f64 {
    lerp(lerp(v00, v10, tx), lerp(v01, v11, tx), ty)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_core::geo::{Crs, GeoTransform};

    /// TM35FIN (EPSG:3067) GeoTransform shaped like the 480×360 radar fixture.
    fn tm35fin_transform() -> GeoTransform {
        GeoTransform {
            origin_x: -196_593.004,
            origin_y: 8_084_432.005,
            pixel_width: 2_584.937,
            pixel_height: 5_080.840,
            width: 480,
            height: 360,
            crs: Crs::TransverseMercator {
                lat0: 0.0,
                lon0: 27.0_f64.to_radians(),
                k0: 0.9996,
                false_e: 500_000.0,
                false_n: 0.0,
            },
        }
    }

    fn wgs84_transform() -> GeoTransform {
        GeoTransform {
            origin_x: 19.0,
            origin_y: 70.0,
            pixel_width: 0.02,
            pixel_height: 0.02,
            width: 650,
            height: 550,
            crs: Crs::Wgs84,
        }
    }

    /// Largest deviation between the coarse grid and the exact per-pixel
    /// projection, in source pixels, over every pixel of a `width`×`height`
    /// output image — but only counting pixels that land on the raster, since
    /// off-raster pixels resolve to nodata regardless of interpolation error.
    fn max_grid_error(
        gt: &GeoTransform,
        width: u32,
        height: u32,
        lon_at: impl Fn(f64) -> f64 + Copy,
        lat_at: impl Fn(f64) -> f64 + Copy,
    ) -> f64 {
        let grid = ProjectionGrid::build(gt, width, height, lon_at, lat_at);
        let (w, h) = (gt.width as f64, gt.height as f64);
        let mut max = 0.0_f64;
        for oy in 0..height {
            for ox in 0..width {
                let (gc, gr) = grid.sample(ox, oy);
                let lon = lon_at((ox as f64 + 0.5) / width as f64);
                let lat = lat_at((oy as f64 + 0.5) / height as f64);
                let (ec, er) = gt.world_to_pixel_f64(lon, lat);
                // Only on-raster pixels are observable in the output.
                if ec >= 0.0 && ec < w && er >= 0.0 && er < h {
                    max = max.max((gc - ec).abs()).max((gr - er).abs());
                }
            }
        }
        max
    }

    #[test]
    fn grid_is_exact_for_affine_wgs84() {
        // For a WGS84 source with linear output axes the output→source mapping
        // is exactly affine, so bilinear interpolation must reproduce it to
        // floating-point precision.
        let gt = wgs84_transform();
        let err = max_grid_error(
            &gt,
            gt.width,
            gt.height,
            |fx| 20.0 + fx * 10.0,
            |fy| 69.0 - fy * 9.0,
        );
        assert!(err < 1e-9, "affine grid error {err} px should be ~0");
    }

    #[test]
    fn grid_matches_exact_tm35fin() {
        // Projected source, linear (WGS84) output axes over a Finland-sized
        // viewport.
        let gt = tm35fin_transform();
        let err = max_grid_error(&gt, 1280, 960, |fx| 19.0 + fx * 13.0, |fy| 70.0 - fy * 11.0);
        assert!(err < 0.5, "TM35FIN grid error {err} px exceeds budget");
    }

    #[test]
    fn grid_matches_exact_webmercator_output() {
        // Projected source with a non-linear (Web Mercator) output Y axis.
        let gt = tm35fin_transform();
        let err = max_grid_error(
            &gt,
            1280,
            960,
            |fx| 19.0 + fx * 13.0,
            |fy| merc_lat(&(59.0, 70.0), fy),
        );
        assert!(err < 0.5, "Web Mercator grid error {err} px exceeds budget");
    }

    #[test]
    fn grid_matches_exact_zoomed_out() {
        // Adversarial: low-zoom viewports that span tens of degrees per render.
        // A fixed pixel-based grid step badly under-samples these — the
        // adaptive refinement must still keep them under budget. Each viewport
        // overlaps the raster, so the grid resampler genuinely runs.
        let gt = tm35fin_transform();
        // z=2-style 256 px tile over a 90° longitude span.
        let err = max_grid_error(&gt, 256, 256, |fx| fx * 90.0, |fy| 78.0 - fy * 50.0);
        assert!(err < 0.5, "z2 tile grid error {err} px exceeds budget");
        // Wide WMS render — "zoom out to see the whole radar".
        let err = max_grid_error(
            &gt,
            1024,
            512,
            |fx| -30.0 + fx * 114.0,
            |fy| 78.0 - fy * 40.0,
        );
        assert!(err < 0.5, "wide WMS grid error {err} px exceeds budget");
        // Web Mercator output over a wide span.
        let err = max_grid_error(
            &gt,
            512,
            512,
            |fx| fx * 90.0,
            |fy| merc_lat(&(45.0, 78.0), fy),
        );
        assert!(
            err < 0.5,
            "wide Web Mercator grid error {err} px exceeds budget"
        );
    }

    #[test]
    fn grid_handles_small_output_over_wide_area() {
        // A physically tiny image spanning a large area: few output pixels but
        // high per-cell curvature. Adaptive refinement must still hold budget.
        let gt = tm35fin_transform();
        let err = max_grid_error(&gt, 24, 18, |fx| -10.0 + fx * 70.0, |fy| 78.0 - fy * 40.0);
        assert!(err < 0.5, "small-output grid error {err} px exceeds budget");
    }

    #[test]
    fn sample_stays_in_bounds_at_edges() {
        // The far-corner pixel must not index past the node array.
        let gt = tm35fin_transform();
        let grid =
            ProjectionGrid::build(&gt, 800, 600, |fx| 19.0 + fx * 13.0, |fy| 70.0 - fy * 11.0);
        let (c, r) = grid.sample(799, 599);
        assert!(c.is_finite() && r.is_finite());
    }

    #[test]
    fn sample_is_non_finite_for_degenerate_transform() {
        // A zero pixel size makes world_to_pixel_f64 divide by zero. The grid
        // must not panic; it propagates a non-finite value for the caller to
        // finite-check (mirroring the guard in get_raster_tile).
        let gt = GeoTransform {
            origin_x: 0.0,
            origin_y: 0.0,
            pixel_width: 0.0,
            pixel_height: 0.0,
            width: 100,
            height: 100,
            crs: Crs::Wgs84,
        };
        let grid = ProjectionGrid::build(&gt, 64, 64, |fx| fx, |fy| fy);
        let (c, r) = grid.sample(10, 10);
        assert!(!c.is_finite() || !r.is_finite());
    }

    /// Web Mercator latitude for fractional y in `[0, 1]` over `[south, north]`.
    fn merc_lat((south, north): &(f64, f64), fy: f64) -> f64 {
        const R: f64 = 6_378_137.0;
        let merc_y = |lat: f64| {
            R * ((std::f64::consts::FRAC_PI_4 + lat.to_radians() / 2.0)
                .tan()
                .ln())
        };
        let (my_s, my_n) = (merc_y(*south), merc_y(*north));
        let y = my_n - fy * (my_n - my_s);
        (std::f64::consts::FRAC_PI_2 - 2.0 * (-y / R).exp().atan()).to_degrees()
    }
}
