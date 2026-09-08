//! LRU cache for decoded GRIB grid data.
//!
//! Caches decoded f64 arrays keyed by (grib_url, offset) to avoid
//! repeated byte-range fetches and GRIB decoding.

use std::sync::Arc;

use ds_cache::ByteBoundedCache;
use ds_core::map_engine::OutputCrs;

/// Cache key: identifies a specific GRIB message.
/// Uses `Arc<str>` instead of `String` for a smaller allocation (no capacity field).
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct GridKey {
    /// URL or path to the GRIB file.
    url: Arc<str>,
    /// Byte offset of the message within the file.
    offset: u64,
}

/// Weight function: count the byte size of each cached grid.
fn weigh_grid(_key: &GridKey, val: &Arc<DecodedGrid>) -> u64 {
    val.size_bytes() as u64
}

/// A decoded grid with its metadata.
#[derive(Debug, Clone)]
pub struct DecodedGrid {
    /// Number of points along the longitude axis.
    pub ni: usize,
    /// Number of points along the latitude axis.
    pub nj: usize,
    /// First longitude (westernmost).
    pub lon_first: f64,
    /// First latitude (northernmost, typically 90.0).
    pub lat_first: f64,
    /// Longitude increment (positive eastward).
    pub lon_inc: f64,
    /// Latitude increment (positive northward, but stored negative for N→S grids).
    pub lat_inc: f64,
    /// Decoded values in row-major order (north to south, west to east).
    pub values: Arc<Vec<f64>>,
    /// WMO GRIB2 parameter identification triple `(discipline, category, number)`
    /// extracted from the decoded message. Used for unit resolution.
    pub triple: (u8, u8, u8),
    /// Originating centre ID from GRIB2 Section 1 (Common Code Table C-11).
    /// Used to resolve local parameter extensions (numbers 192-254).
    pub centre: u16,
    /// Type of first fixed surface from GRIB2 Code Table 4.5, e.g.
    /// 1 = ground or water surface, 100 = isobaric level,
    /// 101 = mean sea level, 103 = specified height above ground.
    /// 255 = missing / not set.
    pub first_surface_type: u8,
    /// Numeric value of the first fixed surface after scale_factor has been
    /// applied. `None` if not set (scale_factor == -127 or equivalent).
    /// Units depend on `first_surface_type` (see WMO Code Table 4.5):
    /// 100 → Pa, 103 → m, 107 → K, etc.
    pub first_surface_value: Option<f64>,
}

impl DecodedGrid {
    /// Memory size estimate in bytes.
    fn size_bytes(&self) -> usize {
        self.values.len() * 8 + 64 // values + struct overhead
    }

    /// Get the value at the grid point nearest to (lon, lat).
    /// Returns None if the point is outside the grid (or non-finite).
    pub fn nearest_value(&self, lon: f64, lat: f64) -> Option<f64> {
        let (col_f, row_f) = self.lonlat_to_src_px(lon, lat);
        if !col_f.is_finite() || !row_f.is_finite() {
            return None;
        }
        let col = self.wrap_col(col_f).floor() as isize;
        let row = row_f.floor() as isize;
        if col < 0 || col >= self.ni as isize || row < 0 || row >= self.nj as isize {
            return None;
        }
        Some(self.values[row as usize * self.ni + col as usize])
    }

    /// Wrap a fractional column into `[0, ni)` for (near-)global grids — a
    /// column outside the grid on a 360°-spanning grid names the same meridian
    /// one wrap away. Regional grids (span < 360°) don't wrap, so an
    /// out-of-range column stays out of range (genuine nodata).
    ///
    /// The wrap lives here (and is idempotent via `rem_euclid`) rather than in
    /// [`Self::lonlat_to_src_px`], so the lon→col mapping stays continuous for
    /// [`ProjectionGrid::build_2d`]'s node interpolation while [`Self::nearest_value`]
    /// and [`Self::bilinear_at`] still resolve wrapped meridians.
    fn wrap_col(&self, col_f: f64) -> f64 {
        let cols_per_360 = 360.0 / self.lon_inc;
        if (self.ni as f64) >= cols_per_360 - 0.5 {
            col_f.rem_euclid(cols_per_360)
        } else {
            col_f
        }
    }

    /// Bilinear interpolation at (lon, lat).
    /// Interpolates between the 4 surrounding grid points.
    /// Returns None if the point is outside the grid.
    pub fn bilinear_value(&self, lon: f64, lat: f64) -> Option<f64> {
        let (col_f, row_f) = self.lonlat_to_src_px(lon, lat);
        self.bilinear_at(col_f, row_f)
    }

    /// Map (lon, lat) to fractional source-grid pixel `(col_f, row_f)` — the
    /// cheap affine inverse of the grid's regular lat/lon spacing.
    ///
    /// **No longitude wrap here.** The wrap is applied in [`Self::bilinear_at`]
    /// instead, so this mapping stays *continuous* in longitude. That matters
    /// because [`ProjectionGrid::build_2d`] bilinearly interpolates the `col_f`
    /// values between coarse nodes: if this wrapped, two adjacent nodes
    /// straddling `lon_first` (e.g. a projected viewport crossing Greenwich on a
    /// 0–360° global grid) would get `col_f` like 1428 and 12, and the midpoint
    /// would interpolate to ~720 — sampling ~180° away. No bounds/finiteness
    /// check: that is [`Self::bilinear_at`]'s job.
    fn lonlat_to_src_px(&self, lon: f64, lat: f64) -> (f64, f64) {
        (
            (lon - self.lon_first) / self.lon_inc,
            (lat - self.lat_first) / self.lat_inc,
        )
    }

    /// Bilinearly sample the grid at fractional source pixel `(col_f, row_f)`.
    ///
    /// Returns `None` (transparent) for non-finite inputs or points outside the
    /// grid. Non-finite is the out-of-domain projected pixel case (`project_node`
    /// → NaN): it must be rejected up front because `NaN` comparisons are false
    /// and `NaN as isize` saturates to 0, so the bounds guard would otherwise
    /// pass and return grid-origin data rendered as a colour.
    fn bilinear_at(&self, col_f: f64, row_f: f64) -> Option<f64> {
        if !col_f.is_finite() || !row_f.is_finite() {
            return None;
        }
        // Apply the deferred ±360° longitude wrap here (once, idempotently —
        // replacing the old sequential-`if` that could double-adjust).
        let col_f = self.wrap_col(col_f);
        let col = col_f.floor() as isize;
        let row = row_f.floor() as isize;
        if col < 0 || col >= self.ni as isize || row < 0 || row >= self.nj as isize {
            return None;
        }
        let (col, row) = (col as usize, row as usize);
        let dx = col_f - col as f64;
        let dy = row_f - row as f64;

        // Right and bottom neighbors (clamp to grid edge)
        let col1 = (col + 1).min(self.ni - 1);
        let row1 = (row + 1).min(self.nj - 1);

        let v00 = self.values[row * self.ni + col];
        let v10 = self.values[row * self.ni + col1];
        let v01 = self.values[row1 * self.ni + col];
        let v11 = self.values[row1 * self.ni + col1];

        // Skip interpolation if any neighbor is NaN
        if v00.is_nan() || v10.is_nan() || v01.is_nan() || v11.is_nan() {
            // Fall back to the nearest non-NaN of the four neighbours (not just
            // v00 — otherwise a NaN at v00 with valid v10/v01/v11 would widen the
            // nodata hole by one source pixel).
            return [v00, v10, v01, v11].into_iter().find(|v| !v.is_nan());
        }

        let val = v00 * (1.0 - dx) * (1.0 - dy)
            + v10 * dx * (1.0 - dy)
            + v01 * (1.0 - dx) * dy
            + v11 * dx * dy;
        Some(val)
    }

    /// Extract a grid subset for the given bbox [west, south, east, north].
    /// Returns (x_coords, y_coords, values) for a Grid domain.
    #[allow(clippy::type_complexity)]
    pub fn extract_bbox(&self, bbox: [f64; 4]) -> Option<(Vec<f64>, Vec<f64>, Vec<Option<f64>>)> {
        let [west, south, east, north] = bbox;
        if self.ni == 0 || self.nj == 0 || self.lon_inc <= 0.0 || self.lat_inc == 0.0 {
            return None;
        }

        // Column range in grid units, relative to `lon_first`. On a
        // 360°-spanning grid the bbox may sit a whole turn away from
        // `lon_first` (ECMWF open data starts at 180°, a Finnish bbox at
        // 24° is at column −624): shift the range by whole turns so it
        // starts inside the grid, then walk columns modulo `ni` — the same
        // wrap `wrap_col` applies on the sampling path (#663). Every cast
        // happens AFTER clamping; a negative `isize as usize` once turned a
        // Finnish bbox into a ~1.8e19-element allocation and a 502.
        let cols_per_360 = 360.0 / self.lon_inc;
        let global = (self.ni as f64) >= cols_per_360 - 0.5;
        let mut c0 = ((west - self.lon_first) / self.lon_inc).floor();
        let mut c1 = ((east - self.lon_first) / self.lon_inc).ceil();
        if !(c0.is_finite() && c1.is_finite()) {
            return None;
        }
        // Both ends inclusive: a bbox edge exactly on a node keeps that
        // node, and a bbox narrower than one cell still returns the cell it
        // sits in.
        if c1 < c0 {
            c1 = c0;
        }
        // `cols` are storage columns (wrapped on a global grid); `positions`
        // are the same columns as unwrapped offsets from `lon_first`, which
        // is what the longitude axis is computed from — on a global grid a
        // wrapped column has lost its turn count, on a regional grid a
        // clamped column is not the unclamped `c0`.
        let (cols, positions): (Vec<usize>, Vec<f64>) = if global {
            let shift = c0.rem_euclid(cols_per_360) - c0;
            c0 += shift;
            c1 += shift;
            // Cap at one full turn so a ±180° bbox is the whole grid once.
            let n = ((c1 - c0) as usize + 1).min(self.ni);
            let start = c0 as usize;
            (0..n)
                .map(|k| ((start + k) % self.ni, c0 + k as f64))
                .unzip()
        } else {
            let start = c0.max(0.0).min(self.ni as f64) as usize;
            let end = (c1 + 1.0).max(0.0).min(self.ni as f64) as usize;
            (start..end).map(|c| (c, c as f64)).unzip()
        };

        // Row range (lat_inc is negative for N→S grids), clamped before cast.
        let (r0, r1) = if self.lat_inc < 0.0 {
            (
                ((north - self.lat_first) / self.lat_inc).floor(),
                ((south - self.lat_first) / self.lat_inc).ceil(),
            )
        } else {
            (
                ((south - self.lat_first) / self.lat_inc).floor(),
                ((north - self.lat_first) / self.lat_inc).ceil(),
            )
        };
        if !(r0.is_finite() && r1.is_finite()) {
            return None;
        }
        let row_start = r0.max(0.0).min(self.nj as f64) as usize;
        let row_end = (r1.max(r0) + 1.0).max(0.0).min(self.nj as f64) as usize;

        if cols.is_empty() || row_start >= row_end {
            return None;
        }

        // Longitudes in the requester's frame: ascending from the bbox
        // west edge, so a range that crosses the grid seam reads
        // …, 359.75, 360.0 → −0.25, 0.0 … as a monotonic axis.
        let x_coords: Vec<f64> = positions
            .iter()
            .map(|&pos| {
                let raw = self.lon_first + pos * self.lon_inc;
                if global {
                    raw - ((raw - west) / 360.0).floor() * 360.0
                } else {
                    raw
                }
            })
            .collect();

        let ny = row_end - row_start;
        let mut y_coords = Vec::with_capacity(ny);
        for r in row_start..row_end {
            y_coords.push(self.lat_first + r as f64 * self.lat_inc);
        }
        // y_coords should be in ascending order for CoverageJSON
        if self.lat_inc < 0.0 {
            y_coords.reverse();
        }

        let mut values = Vec::with_capacity(cols.len() * ny);
        // Output in y-ascending order (south to north)
        let row_iter: Box<dyn Iterator<Item = usize>> = if self.lat_inc < 0.0 {
            Box::new((row_start..row_end).rev())
        } else {
            Box::new(row_start..row_end)
        };
        for r in row_iter {
            for &c in &cols {
                values.push(Some(self.values[r * self.ni + c]));
            }
        }

        Some((x_coords, y_coords, values))
    }

    /// Resample grid to output dimensions for map rendering.
    ///
    /// `bbox` is the WGS84 bounding box `[west, south, east, north]`. Each output
    /// pixel's lon/lat comes from the shared [`OutputCrs::project_node`], so the
    /// output axes follow the requested CRS.
    ///
    /// - `Wgs84` / `WebMercator`: `project_node` is cheap (no projection), so we
    ///   sample per pixel. This path also keeps the ±360° longitude wrap that a
    ///   global grid needs for viewports crossing the antimeridian.
    /// - `Projected` (EPSG:3067/3035): `project_node` runs `Crs::inverse` per
    ///   node — so map output→source through [`ProjectionGrid::build_2d`] (coarse
    ///   grid + bilinear) rather than per pixel, per the CLAUDE.md "never project
    ///   per output pixel" rule (matches engine-geotiff/odim-COMP). Projected
    ///   output is regional, so no cell crosses the antimeridian wrap.
    pub fn resample(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        output_crs: &OutputCrs,
    ) -> Vec<Option<f64>> {
        let w = width as usize;
        let h = height as usize;
        let mut out = Vec::with_capacity(w * h);

        match output_crs {
            OutputCrs::Projected { .. } => {
                let grid = ds_core::resample::ProjectionGrid::build_2d(
                    width,
                    height,
                    self.ni as u32,
                    self.nj as u32,
                    |fx, fy| output_crs.project_node(bbox, fx, fy),
                    |lon, lat| self.lonlat_to_src_px(lon, lat),
                );
                for oy in 0..height {
                    for ox in 0..width {
                        let (col_f, row_f) = grid.sample(ox, oy);
                        out.push(self.bilinear_at(col_f, row_f));
                    }
                }
            }
            OutputCrs::Wgs84 | OutputCrs::WebMercator => {
                for row in 0..h {
                    let fy = (row as f64 + 0.5) / h as f64;
                    for col in 0..w {
                        let fx = (col as f64 + 0.5) / w as f64;
                        let (lon, lat) = output_crs.project_node(bbox, fx, fy);
                        out.push(self.bilinear_value(lon, lat));
                    }
                }
            }
        }

        out
    }
}

/// LRU cache for decoded grids, weighted by byte size.
pub struct GridCache {
    cache: ByteBoundedCache<GridKey, Arc<DecodedGrid>>,
}

impl GridCache {
    /// Create a new grid cache with the given size limit in MB.
    /// Returns None if size_mb is 0 (cache disabled).
    pub fn new(size_mb: u64) -> Option<Self> {
        if size_mb == 0 {
            return None;
        }
        // Estimate: each grid is ~8 MB (1440*721*8 bytes).
        Some(Self {
            cache: ByteBoundedCache::new(
                size_mb.saturating_mul(ds_cache::MIB),
                8 * ds_cache::MIB,
                weigh_grid,
            ),
        })
    }

    /// Get a cached grid, or None if not cached.
    pub fn get(&self, url: &str, offset: u64) -> Option<Arc<DecodedGrid>> {
        let key = GridKey {
            url: Arc::from(url),
            offset,
        };
        self.cache.get(&key)
    }

    /// Insert a decoded grid into the cache.
    pub fn insert(&self, url: &str, offset: u64, grid: Arc<DecodedGrid>) {
        let key = GridKey {
            url: Arc::from(url),
            offset,
        };
        self.cache.insert(key, grid);
    }

    /// Return (hits, misses) counters.
    pub fn stats(&self) -> (u64, u64) {
        self.cache.stats()
    }

    /// Current weight (bytes used) of the cache.
    pub fn weight(&self) -> u64 {
        self.cache.weight()
    }

    /// Maximum weight (bytes) the cache will hold.
    pub fn capacity(&self) -> u64 {
        self.cache.capacity_bytes()
    }

    /// Number of entries currently in the cache.
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// Whether the cache is currently empty.
    pub fn is_empty(&self) -> bool {
        self.cache.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2×2 regular lat/lon grid (10–11°E, 59–60°N) for sampling tests.
    fn grid_2x2() -> DecodedGrid {
        DecodedGrid {
            ni: 2,
            nj: 2,
            lon_first: 10.0,
            lat_first: 60.0,
            lon_inc: 1.0,
            lat_inc: -1.0,
            values: Arc::new(vec![1.0, 2.0, 3.0, 4.0]),
            triple: (0, 0, 0),
            centre: 0,
            first_surface_type: 1,
            first_surface_value: None,
        }
    }

    #[test]
    fn bilinear_value_rejects_nan_lonlat() {
        // Regression for the OutputCrs::Projected NaN path: an out-of-domain
        // pixel arrives as NaN and must resolve to None (transparent), not
        // grid-origin data — NaN bypasses the bounds guard otherwise.
        let g = grid_2x2();
        assert_eq!(g.bilinear_value(f64::NAN, 59.5), None);
        assert_eq!(g.bilinear_value(10.5, f64::NAN), None);
        assert_eq!(g.bilinear_value(f64::NAN, f64::NAN), None);
        assert_eq!(g.bilinear_value(f64::INFINITY, 59.5), None);
        // A normal in-grid sample still returns a value.
        assert!(g.bilinear_value(10.5, 59.5).is_some());
    }

    #[test]
    fn resample_projected_in_domain_has_data_via_build_2d() {
        // The OutputCrs::Projected path goes through ProjectionGrid::build_2d
        // (no per-pixel inverse). A projected bbox covering the 10–11°E/59–60°N
        // grid must resample to real values, not all-None.
        let g = grid_2x2();
        let crs = ds_core::geo::projected_output_crs("EPSG:3035").unwrap();
        // Forward the grid's WGS84 footprint into EPSG:3035 metres.
        let proj = ds_core::geo::projected_envelope(&crs, [10.0, 59.0, 11.0, 60.0]);
        let out = g.resample(
            ds_core::geo::wgs84_envelope(&crs, proj).unwrap(),
            16,
            16,
            &OutputCrs::Projected { crs, bbox: proj },
        );
        assert!(
            out.iter().any(|v| v.is_some()),
            "projected render over the grid footprint must place data"
        );
        // Every produced value is finite (no NaN leaked through build_2d).
        assert!(out.iter().flatten().all(|v| v.is_finite()));
    }

    #[test]
    fn build_2d_projected_handles_lon_first_seam() {
        // Regression for the #267 round-4 finding: a projected viewport
        // straddling the grid's lon_first (Greenwich, on a 0–360° global grid)
        // must not let build_2d interpolate col_f across the wrap seam. The grid
        // value is cos(lon): ~1 near lon 0, but ~ -1 at lon 180. A seam bug
        // interpolates adjacent nodes (col ~359 and ~1) to col ~180 and samples
        // ~ -1; the fix (raw affine in lonlat_to_src_px, wrap in bilinear_at)
        // keeps col_f continuous so every sample stays near the true meridian.
        let (ni, nj) = (360usize, 21usize);
        let mut values = vec![0.0f64; ni * nj];
        for r in 0..nj {
            for c in 0..ni {
                values[r * ni + c] = (c as f64).to_radians().cos(); // c == lon°
            }
        }
        let g = DecodedGrid {
            ni,
            nj,
            lon_first: 0.0,
            lat_first: 60.0,
            lon_inc: 1.0,
            lat_inc: -1.0,
            values: Arc::new(values),
            triple: (0, 0, 0),
            centre: 0,
            first_surface_type: 1,
            first_surface_value: None,
        };
        let crs = ds_core::geo::projected_output_crs("EPSG:3035").unwrap();
        // Western Europe, straddling the Greenwich meridian.
        let proj = ds_core::geo::projected_envelope(&crs, [-5.0, 45.0, 10.0, 55.0]);
        let read = ds_core::geo::wgs84_envelope(&crs, proj).unwrap();
        let out = g.resample(read, 64, 64, &OutputCrs::Projected { crs, bbox: proj });

        assert!(
            out.iter().all(|v| v.is_some()),
            "the whole viewport is inside the grid; no holes expected"
        );
        for v in out.iter().flatten() {
            assert!(
                *v > 0.5,
                "lon_first seam mis-sample: cos={v} (≈ -1 means it sampled ~180° away)"
            );
        }
    }

    #[test]
    fn resample_projected_out_of_domain_is_all_none() {
        // A projected bbox whose inverse-projected lon/lat fall far outside the
        // 10–11°E / 59–60°N grid must render fully transparent — every sample
        // resolves off-grid (or NaN) → None, never a grid-origin colour.
        let g = grid_2x2();
        let crs = ds_core::geo::projected_output_crs("EPSG:3035").unwrap();
        // Projected metres nowhere near the 10–11°E / 59–60°N grid.
        let out = g.resample(
            [-5_000_000.0, -5_000_000.0, -4_900_000.0, -4_900_000.0],
            8,
            8,
            &OutputCrs::Projected {
                crs,
                bbox: [-5_000_000.0, -5_000_000.0, -4_900_000.0, -4_900_000.0],
            },
        );
        assert!(
            out.iter().all(|v| v.is_none()),
            "out-of-domain projected render must be all-None, got {out:?}"
        );
    }

    /// #663: ECMWF open data is a 0.25° global grid whose first longitude
    /// is 180°. A Finnish bbox is a whole turn west of `lon_first`; the old
    /// code cast the negative column index to `usize` and panicked with a
    /// capacity overflow — every EDR area query on the collection 502'd.
    fn global_grid(lon_first: f64) -> DecodedGrid {
        let (ni, nj) = (1440usize, 721usize);
        // value = column index, so the extracted values name the columns.
        let values: Vec<f64> = (0..ni * nj).map(|i| (i % ni) as f64).collect();
        DecodedGrid {
            ni,
            nj,
            lon_first,
            lat_first: 90.0,
            lon_inc: 0.25,
            lat_inc: -0.25,
            values: Arc::new(values),
            triple: (0, 0, 0),
            centre: 0,
            first_surface_type: 1,
            first_surface_value: None,
        }
    }

    #[test]
    fn extract_bbox_wraps_a_180_first_global_grid() {
        let g = global_grid(180.0);
        let (x, y, v) = g
            .extract_bbox([24.0, 60.0, 26.0, 61.0])
            .expect("intersects");
        // 24°E is column (24 − 180)/0.25 = −624 ≡ 816 (mod 1440).
        assert_eq!(x.first().copied(), Some(24.0));
        assert_eq!(x.last().copied(), Some(26.0));
        assert!(x.windows(2).all(|w| w[1] > w[0]));
        assert_eq!(x.len(), 9);
        assert_eq!(y, vec![60.0, 60.25, 60.5, 60.75, 61.0]);
        assert_eq!(v.len(), 9 * 5);
        assert_eq!(v[0], Some(816.0));
        assert_eq!(v[8], Some(824.0));
    }

    #[test]
    fn extract_bbox_crosses_the_seam_of_a_greenwich_first_grid() {
        let g = global_grid(0.0);
        let (x, _y, v) = g.extract_bbox([-1.0, 50.0, 1.0, 50.5]).expect("intersects");
        // −1° is column 1436; the axis must read −1.0 … 1.0 ascending and
        // the values must come from columns 1436..1439 then 0..4.
        assert_eq!(x, vec![-1.0, -0.75, -0.5, -0.25, 0.0, 0.25, 0.5, 0.75, 1.0]);
        let first_row: Vec<f64> = v[..9].iter().map(|c| c.unwrap()).collect();
        assert_eq!(
            first_row,
            vec![1436.0, 1437.0, 1438.0, 1439.0, 0.0, 1.0, 2.0, 3.0, 4.0]
        );
    }

    #[test]
    fn extract_bbox_regional_grid_clamps_instead_of_wrapping() {
        // 10–11°E grid: a bbox entirely west of it does not wrap around
        // the planet; it simply misses.
        let g = grid_2x2();
        assert!(g.extract_bbox([-5.0, 59.0, -4.0, 60.0]).is_none());
        // A bbox straddling the grid's west edge clamps to the grid, and
        // the longitude axis names the columns actually returned.
        let (x, _, v) = g.extract_bbox([9.5, 59.0, 10.5, 60.0]).expect("overlaps");
        assert_eq!(x, vec![10.0, 11.0]);
        assert_eq!(v.len(), 4);
        // Sub-cell bbox on a node still returns the cell it sits in.
        let (x, _, _) = g.extract_bbox([10.0, 59.5, 10.0, 59.6]).expect("one cell");
        assert_eq!(x, vec![10.0]);
        // A whole-planet bbox on a global grid is exactly one turn.
        let g = global_grid(180.0);
        let (x, _, _) = g
            .extract_bbox([-180.0, -90.0, 180.0, 90.0])
            .expect("global");
        assert_eq!(x.len(), 1440);
    }
}
