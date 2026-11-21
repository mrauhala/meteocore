//! Coarse-grid output→source pixel mapping for raster resampling.
//!
//! Rendering a WMS/Maps/Tiles tile resamples a source raster onto the output
//! grid. The naive approach projects every output pixel through the source
//! CRS forward transform, which for a projected source (e.g. EPSG:3067
//! Transverse Mercator, LAEA, LCC, Stereographic) costs roughly a dozen
//! transcendental ops *per pixel* — the dominant per-render CPU cost.
//!
//! The output→source pixel mapping is smooth, so [`ProjectionGrid`] evaluates
//! the exact mapping only at the nodes of a coarse grid and bilinearly
//! interpolates it for every interior pixel. For a fullscreen render this
//! turns ~2 million projection calls into a few thousand.
//!
//! Bilinear interpolation error depends on how much the mapping *curves*
//! within a cell, which is a function of the geographic span per cell, not the
//! pixel count — a zoomed-out tile spanning tens of degrees curves far more per
//! cell than a zoomed-in one. So the grid density is chosen **adaptively**:
//! [`ProjectionGrid::build`] starts from a pixel-based density and refines
//! until the measured interpolation error over the on-raster region is below
//! [`MAX_INTERP_ERROR_PX`] — well under the 0.5 px that nearest-neighbour
//! resampling can resolve. See the `grid_matches_exact_*` tests. A global
//! source's grid ([`ProjectionGrid::build_2d_periodic`]) refines the cells
//! near a pole on their own, splitting each in four until it meets a
//! tighter estimate, rather than refining the whole grid for them (#1195).
//!
//! The grid is engine-agnostic: it interpolates only the output→source
//! *coordinate* map, never the data values (so nearest-neighbour callers stay
//! nearest-neighbour and nodata/undetect edges are not smeared). The caller
//! supplies the source mapping as a `world_to_src_px` closure, so this is
//! shared by every raster engine — `engine-geotiff` (`GeoTransform`),
//! `engine-odim` (composite grid), and any future projected-grid engine.
//! Keeping it here makes the CLAUDE.md "never project per output pixel" rule
//! reusable rather than re-derived per engine (#203, #236).
//!
//! [`SourceGrid`] builds on it: the shared `f32` sampler a gridded engine
//! renders a decoded field with (see [`sampler`]), over a regular grid or a
//! curvilinear one placed by an inverse lookup ([`curvilinear`], #1174).

pub mod curvilinear;
pub mod sampler;

pub use curvilinear::{CurvilinearGrid, CurvilinearNodes};
pub use sampler::{CellWindow, OutputMap, SourceGrid};

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

/// Node spacing, in output pixels, inside a cell on a domain boundary —
/// some of its corner nodes map to finite source pixels, some don't (the
/// limb of a geostationary disk, the edge of a projected output CRS's
/// domain). Such a cell cannot be interpolated as a whole, so it gets its
/// own sub-grid at this spacing, which bounds the under-fill along the
/// boundary to this many pixels.
const BOUNDARY_STEP_PX: f64 = 2.0;

/// Interpolation-error budget, in source pixels (output pixels on a periodic
/// column axis, see [`ProjectionGrid::build_2d_periodic`]). Refinement stops
/// once the estimated error over the on-raster region drops below this. Kept
/// well under the 0.5 px resolution of nearest-neighbour resampling.
const MAX_INTERP_ERROR_PX: f64 = 0.2;

/// The estimated error a cell split near a pole is split down to
/// ([`ProjectionGrid::refine_locally`]), in output pixels: two thirds of
/// [`MAX_INTERP_ERROR_PX`] (#1195). Beside a pole the error grows as the
/// inverse of the distance to it, so it peaks on a cell's side nearest the
/// pole, past the outermost probes ([`PROBE_FRACS`]): there a cell meeting
/// the estimate was measured up to 1.34 times it. Refined as a whole, every
/// cell near a pole was split so far that this never showed; split on its
/// own, each would stop at the estimate. A third in hand keeps the true
/// error within the budget.
const NEAR_POLE_ERROR_PX: f64 = MAX_INTERP_ERROR_PX * 2.0 / 3.0;

/// The smallest cell [`ProjectionGrid::refine_locally`] splits, in output
/// pixels. A cell a pixel wide holds one pixel centre along that axis, so
/// splitting it further would cost about a projection per pixel. A small
/// output's cells at [`MAX_CELLS`] are finer still, and it keeps those.
const MIN_LOCAL_CELL_PX: f64 = 1.0;

/// How far from a cell bracketing a pole, as a share of the image's
/// shorter side, a periodic grid's cells over budget are split locally
/// ([`ProjectionGrid::refine_locally`]) instead of refining the whole grid
/// (#1195): 64 px on a 256 px tile. Beside a pole a cell `s` pixels wide is
/// over budget out to about `s²` pixels, so only once `s²` is within this
/// reach are the cells over budget because of the pole all inside it; the
/// whole grid refines until then, at 8 px cells on a 256 px tile, and
/// every cell farther from the pole is refined as it would be without
/// one. A quarter of the image keeps the pole's reach clear of a singular
/// feature across the view, such as the rim of EPSG:3035's domain on a
/// view of the globe.
const POLE_REACH: f64 = 0.25;

/// Projections one local split costs at most: five new nodes (the four
/// side midpoints and the centre) and the error probes of its four
/// children, none for a child bracketing a pole.
const SPLIT_COST: usize = 5 + 4 * PROBE_FRACS.len() * PROBE_FRACS.len();

/// [`ProjectionGrid::local`] of a cell not refined locally.
const NO_QUAD: u32 = u32::MAX;

/// Per-axis fractions of the 2-D grid of interior probe points `(tx, ty)` that
/// [`ProjectionGrid::cell_error`] samples within each cell (the cartesian
/// product gives `PROBE_FRACS.len()²` points).
///
/// A **dense, regular** grid is load-bearing for correctness, not just nicety.
/// The bilinear residual is zero at the cell corners and peaks somewhere in the
/// interior, but for a real projection over a wide cell that peak is *not* at
/// the centre — its location depends on the projection's higher-order terms and
/// shifts toward the edges. The original 5-point pattern (centre + four quarter
/// points, all clustered near the middle) systematically *missed* the peak and
/// under-reported the true error by up to ~20× for low-zoom viewports, so
/// [`ProjectionGrid::build`] stopped refining far too early and left several
/// source-pixels of misregistration — visible as dotted / "curtain" artifacts
/// along edges where the projection curves most (e.g. the eastern edge of an
/// EPSG:3067 radar composite). The regular grid below, spanning the cell from
/// near one edge (0.125) to the other (0.875), brackets the peak for every
/// projection family (#203 follow-up).
///
/// Four fractions per axis (16 probes) is the tuned balance: it reliably catches
/// the residual peak — verified to hold the source-pixel budget at production
/// resolution, see `grid_holds_budget_at_production_resolution` — while keeping
/// the estimator's cost down. The estimator runs only at grid-build time (never
/// per output pixel), and the probe count scales its cost, which matters for the
/// zoomed-out viewports that refine to many cells; 16 roughly halves that cost
/// versus a 25-probe (5×5) grid with no loss of accuracy on the regression test.
const PROBE_FRACS: [f64; 4] = [0.125, 0.375, 0.625, 0.875];

/// The `PROBE_FRACS × PROBE_FRACS` interior probe grid as `(tx, ty)` pairs.
fn error_probes() -> impl Iterator<Item = (f64, f64)> {
    PROBE_FRACS
        .iter()
        .flat_map(|&ty| PROBE_FRACS.iter().map(move |&tx| (tx, ty)))
}

/// Coarse grid of exact output→source pixel correspondences, with bilinear
/// interpolation for interior pixels.
pub struct ProjectionGrid {
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
    /// Sub-grids of the cells on a domain boundary, keyed and sorted by the
    /// index of the cell's top-left node.
    boundary: Vec<(usize, SubGrid)>,
    /// Source columns per 360° when the column axis is a whole turn of
    /// longitude (see [`ProjectionGrid::build_2d_periodic`]).
    col_period: Option<f64>,
    /// Per cell, row-major, the index in `quads` of its root when the cell
    /// is refined locally ([`Self::refine_locally`]), else [`NO_QUAD`].
    /// Empty when no cell is.
    local: Vec<u32>,
    /// The quadtrees of the cells refined locally.
    quads: Vec<Quad>,
    /// The estimated interpolation error, in the budget's units, when
    /// refinement stopped above [`MAX_INTERP_ERROR_PX`], and what stopped
    /// it.
    capped: Option<(f64, Limits)>,
}

/// A node of a locally refined cell's quadtree ([`ProjectionGrid::refine_locally`]).
/// Its children at depth `d` below a cell of the grid are cells of the grid
/// refined `d` times as a whole, node for node and probe for probe.
enum Quad {
    /// Interpolated over its corners' exact source `(col, row)`, in the
    /// order `[c00, c10, c01, c11]` of a grid cell.
    Leaf([(f64, f64); 4]),
    /// Split in four: the index in [`ProjectionGrid::quads`] of the first
    /// child, followed by the others, top-left, top-right, bottom-left,
    /// bottom-right.
    Split(u32),
}

/// A boundary cell's nodes at [`BOUNDARY_STEP_PX`] spacing, row-major.
struct SubGrid {
    cells_x: usize,
    cells_y: usize,
    nodes: Vec<(f64, f64)>,
}

impl SubGrid {
    /// Interpolated source `(col, row)` at fractional position `(tx, ty)`
    /// within the cell; non-finite where the enclosing sub-cell touches
    /// the boundary.
    fn sample(&self, tx: f64, ty: f64, col_period: Option<f64>) -> (f64, f64) {
        let gx = tx * self.cells_x as f64;
        let gy = ty * self.cells_y as f64;
        let cx = (gx.floor() as usize).min(self.cells_x - 1);
        let cy = (gy.floor() as usize).min(self.cells_y - 1);
        let (tx, ty) = (gx - cx as f64, gy - cy as f64);
        let stride = self.cells_x + 1;
        let n = cy * stride + cx;
        let [c00, c10, c01, c11] = unwrap_cols(
            [
                self.nodes[n],
                self.nodes[n + 1],
                self.nodes[n + stride],
                self.nodes[n + stride + 1],
            ],
            col_period,
        );
        (
            bilerp(c00.0, c10.0, c01.0, c11.0, tx, ty),
            bilerp(c00.1, c10.1, c01.1, c11.1, tx, ty),
        )
    }
}

/// `d` reduced to within half a turn of zero, for a period of `p`.
#[inline]
fn wrap_half(d: f64, p: f64) -> f64 {
    d - p * (d / p).round()
}

/// A cell's corners `[c00, c10, c01, c11]` with their columns unwrapped to
/// within half a turn of the first finite one. On a periodic column axis two
/// adjacent nodes either side of the source's seam (or of the output
/// projection's longitude cut) are a column apart, not a turn apart;
/// interpolating the raw columns would sweep the whole grid in between. The
/// caller's sampler wraps the interpolated column back into the grid.
#[inline]
fn unwrap_cols(mut corners: [(f64, f64); 4], col_period: Option<f64>) -> [(f64, f64); 4] {
    if let Some(p) = col_period {
        if let Some(r) = corners.iter().map(|c| c.0).find(|c| c.is_finite()) {
            for c in &mut corners {
                c.0 = r + wrap_half(c.0 - r, p);
            }
        }
    }
    corners
}

/// Whether column `col`, a fraction `t` of the way along a side whose
/// column runs from `from` and turns `turn`, strays from the line between
/// its ends by more than a 64th of the turn, modulo the period `p`: beside a
/// pole it strays by at least a 22nd ([`ProjectionGrid::brackets_pole`]).
/// A non-finite column strays.
fn strays(col: f64, from: f64, turn: f64, t: f64, p: f64) -> bool {
    let off = wrap_half(col - (from + t * turn), p).abs();
    off.is_nan() || off > turn.abs() / 64.0
}

/// What stopped a grid's refinement over budget ([`ProjectionGrid::capped_error`]):
/// every limit that left a cell over it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Limits {
    /// The whole grid reached [`MAX_CELLS`].
    cells: bool,
    /// A cell near a pole could not be split further: it is
    /// [`MIN_LOCAL_CELL_PX`] wide, or beside a domain edge.
    smallest: bool,
    /// The local splits near a pole spent their projections.
    splits: bool,
}

/// One of [`Limits`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Limit {
    Cells,
    Smallest,
    Splits,
}

/// A cell [`ProjectionGrid::refine_locally`] has yet to split, ordered so
/// the worst comes first and a cell bracketing a pole before any other.
struct Pending {
    /// The estimated error, `None` bracketing a pole.
    error: Option<f64>,
    /// Its leaf in [`ProjectionGrid::quads`].
    quad: u32,
    /// Its column, row and depth.
    cell: (usize, usize, u32),
    corners: [(f64, f64); 4],
}

impl Pending {
    fn key(&self) -> f64 {
        self.error.unwrap_or(f64::INFINITY)
    }
}

impl Ord for Pending {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key()
            .total_cmp(&other.key())
            .then_with(|| other.quad.cmp(&self.quad))
    }
}

impl PartialOrd for Pending {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Pending {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for Pending {}

impl ProjectionGrid {
    /// Build the grid for an output image of `out_width`×`out_height` pixels
    /// over a source raster of `src_cols`×`src_rows` pixels.
    ///
    /// `lon_at` maps a fractional x position in `[0, 1]` to longitude (deg) and
    /// `lat_at` maps a fractional y position in `[0, 1]` to latitude (deg).
    /// Passing closures (rather than a bbox) lets the caller capture a
    /// non-linear output-axis parameterisation, e.g. the equal-Y-meters
    /// spacing of Web Mercator.
    ///
    /// `world_to_src_px` maps geographic `(lon, lat)` (degrees) to fractional
    /// source pixel `(col, row)` — this is the expensive per-engine mapping
    /// (e.g. `GeoTransform::world_to_pixel_f64`, or a composite's
    /// `crs.forward` followed by the grid offset/spacing). It is evaluated only
    /// at grid nodes and error probes, never per output pixel.
    ///
    /// `src_cols`/`src_rows` bound the "on-raster" window used to decide which
    /// cells matter for the error estimate; off-raster cells resolve to nodata
    /// regardless of interpolation error, so refining for them is wasted work.
    ///
    /// The grid density is refined until the estimated bilinear-interpolation
    /// error over the on-raster region is below [`MAX_INTERP_ERROR_PX`], or the
    /// [`MAX_CELLS`] cap is reached.
    pub fn build(
        out_width: u32,
        out_height: u32,
        src_cols: u32,
        src_rows: u32,
        lon_at: impl Fn(f64) -> f64,
        lat_at: impl Fn(f64) -> f64,
        world_to_src_px: impl Fn(f64, f64) -> (f64, f64),
    ) -> Self {
        // Separable axes (linear lon / linear-or-Mercator lat) are the common
        // case; express them through the general 2-D mapping.
        Self::build_2d(
            out_width,
            out_height,
            src_cols,
            src_rows,
            |fx, fy| (lon_at(fx), lat_at(fy)),
            world_to_src_px,
        )
    }

    /// Like [`build`](Self::build) but with a fully 2-D output→world
    /// parameterisation: `out_to_world(fx, fy)` maps a fractional output
    /// position `(fx, fy)` in `[0, 1]²` to geographic `(lon, lat)` degrees.
    ///
    /// Required for a **projected output CRS** (e.g. EPSG:3067), where lon and
    /// lat each depend on *both* output axes because the inverse projection
    /// mixes easting and northing — so the separable `lon_at(fx)` / `lat_at(fy)`
    /// decomposition of [`build`](Self::build) does not hold (#160). Engines
    /// pass `|fx, fy| output_crs.project_node(bbox, fx, fy)`.
    pub fn build_2d(
        out_width: u32,
        out_height: u32,
        src_cols: u32,
        src_rows: u32,
        out_to_world: impl Fn(f64, f64) -> (f64, f64),
        world_to_src_px: impl Fn(f64, f64) -> (f64, f64),
    ) -> Self {
        Self::build_2d_periodic(
            out_width,
            out_height,
            src_cols,
            src_rows,
            None,
            out_to_world,
            world_to_src_px,
        )
    }

    /// Like [`build_2d`](Self::build_2d) for a source whose column axis may
    /// be a whole turn of longitude: `col_period = Some(p)` for a global
    /// lat/lon grid with `p` columns per 360° (`None` is exactly
    /// `build_2d`). `world_to_src_px` must then be continuous in longitude
    /// and the caller's sampler must wrap the column it is given modulo `p`.
    ///
    /// A global source in a projected output CRS meets two longitude
    /// discontinuities `build_2d` cannot interpolate across (#323): the
    /// output inverse's cut (EPSG:3035 returns longitudes within ±180° of
    /// 10°E, so they jump a turn along 170°W) and the poles, where every
    /// longitude meets. With a period, each cell's columns are unwrapped
    /// before interpolating, so the cut costs nothing. The error budget
    /// becomes a ground distance in output pixels, the column error
    /// measured along the parallel (scaled by cos lat): a source-pixel
    /// budget cannot converge on a column axis that shrinks to a point at
    /// the pole, and is too loose where one source pixel spans several
    /// output pixels.
    ///
    /// Beside a pole the column map changes fastest, and its bilinear error
    /// only halves when a cell's size does, about an eighth of a cell a
    /// cell's width out. Refined as a whole, a tile holding a pole reached
    /// [`MAX_CELLS`], a million and a half projections, to resolve a few
    /// pixels around it (#1195). So the whole grid refines only until every
    /// cell farther than [`POLE_REACH`] from a pole meets the budget; the
    /// cells nearer it over [`NEAR_POLE_ERROR_PX`], and the cells bracketing
    /// it, which are left out of the estimate, are split on their own
    /// ([`Self::refine_locally`]), and domain edges near it get sub-grids
    /// as fine. A pole is found by a cell bracketing it
    /// ([`Self::brackets_pole`]) or by the row or column of nodes it lies
    /// beside ([`Self::node_pole_sites`]); one just past a corner of the
    /// image is not, and refines the whole grid as before. The budget holds
    /// everywhere, measured per pixel, but in the pixel-sized cells that
    /// bracket the pole. A grid without a pole is refined as a whole, cell
    /// for cell as before.
    pub fn build_2d_periodic(
        out_width: u32,
        out_height: u32,
        src_cols: u32,
        src_rows: u32,
        col_period: Option<f64>,
        out_to_world: impl Fn(f64, f64) -> (f64, f64),
        world_to_src_px: impl Fn(f64, f64) -> (f64, f64),
    ) -> Self {
        Self::refined(
            out_width,
            out_height,
            src_cols,
            src_rows,
            col_period,
            None,
            out_to_world,
            world_to_src_px,
        )
        .warn_if_capped()
    }

    /// Like [`build_2d`](Self::build_2d) for a caller that samples only the
    /// output pixels of `read`, an inclusive window `(px_lo, px_hi, py_lo,
    /// py_hi)` such as [`OutputCrs::footprint_pixel_window`] gives. Only the
    /// cells holding one of its pixels count toward the error estimate and
    /// get boundary sub-grids, so a pixel outside it is interpolated without
    /// the error budget and must not be read.
    ///
    /// Outside a source's footprint the mapping need not be smooth, and
    /// refining for it then never converges (#1110). A conic source's
    /// forward transform jumps a cone's gap where longitude wraps, half a
    /// turn from the source. Over the pole that drove a 256 px meta-tile to
    /// [`MAX_CELLS`]: a million and a half projections for pixels the
    /// footprint guard then left empty.
    ///
    /// [`OutputCrs::footprint_pixel_window`]: crate::map_engine::OutputCrs::footprint_pixel_window
    pub fn build_2d_within(
        out_width: u32,
        out_height: u32,
        read: (u32, u32, u32, u32),
        src_cols: u32,
        src_rows: u32,
        out_to_world: impl Fn(f64, f64) -> (f64, f64),
        world_to_src_px: impl Fn(f64, f64) -> (f64, f64),
    ) -> Self {
        Self::build_2d_within_quiet(
            out_width,
            out_height,
            read,
            src_cols,
            src_rows,
            out_to_world,
            world_to_src_px,
        )
        .warn_if_capped()
    }

    /// [`Self::build_2d_within`] for a caller that reports a capped grid
    /// itself: nothing goes to stderr, and [`Self::capped_error`] says
    /// whether refinement stopped at [`MAX_CELLS`] over budget. ds-core has
    /// no `tracing`, so an engine logs it with what only it knows, the
    /// collection and the request (#1129).
    pub fn build_2d_within_quiet(
        out_width: u32,
        out_height: u32,
        read: (u32, u32, u32, u32),
        src_cols: u32,
        src_rows: u32,
        out_to_world: impl Fn(f64, f64) -> (f64, f64),
        world_to_src_px: impl Fn(f64, f64) -> (f64, f64),
    ) -> Self {
        Self::refined(
            out_width,
            out_height,
            src_cols,
            src_rows,
            None,
            Some(read),
            out_to_world,
            world_to_src_px,
        )
    }

    /// The estimated interpolation error when refinement stopped at
    /// [`MAX_CELLS`] above the [`MAX_INTERP_ERROR_PX`] budget, `None` when
    /// the grid met it. In source pixels, or output pixels on a periodic
    /// column axis ([`Self::build_2d_periodic`]).
    pub fn capped_error(&self) -> Option<f64> {
        self.capped.map(|(error, _)| error)
    }

    /// Report a capped grid on stderr, for every constructor but
    /// [`Self::build_2d_within_quiet`]. ds-core is framework-free (no
    /// `tracing`). It's a rare edge (only hit when a single render spans
    /// tens of degrees) and the consequence is sub-pixel-ish
    /// misregistration, not a failure.
    fn warn_if_capped(self) -> Self {
        if let Some((error, limits)) = self.capped {
            let limit = [
                (limits.cells, "hit the MAX_CELLS cap"),
                (limits.smallest, "split cells near a pole as far as they go"),
                (limits.splits, "spent its split budget near a pole"),
            ]
            .into_iter()
            .filter_map(|(hit, what)| hit.then_some(what))
            .collect::<Vec<_>>()
            .join(" and ");
            eprintln!(
                "WARN ds_core::resample: projection grid {limit} with \
                 interpolation error {error:.3} px > budget \
                 {MAX_INTERP_ERROR_PX} px; rendered output may be slightly \
                 misregistered for this viewport"
            );
        }
        self
    }

    /// The adaptive build behind every constructor: `col_period` as
    /// [`Self::build_2d_periodic`] takes it, `read` as
    /// [`Self::build_2d_within`] does (`None` reads every pixel). A grid
    /// that stops at the cap records its error ([`Self::capped_error`]);
    /// so does a periodic one whose local refinement stops over budget.
    #[allow(clippy::too_many_arguments)] // output dims + source dims + period + window + 2 mapping closures are all genuine inputs
    fn refined(
        out_width: u32,
        out_height: u32,
        src_cols: u32,
        src_rows: u32,
        col_period: Option<f64>,
        read: Option<(u32, u32, u32, u32)>,
        out_to_world: impl Fn(f64, f64) -> (f64, f64),
        world_to_src_px: impl Fn(f64, f64) -> (f64, f64),
    ) -> Self {
        let col_period = col_period.filter(|p| p.is_finite() && *p > 0.0);
        let mut cells_x = out_width.div_ceil(GRID_STEP_PX).clamp(MIN_CELLS, MAX_CELLS);
        let mut cells_y = out_height
            .div_ceil(GRID_STEP_PX)
            .clamp(MIN_CELLS, MAX_CELLS);
        let reach = f64::from(out_width.min(out_height)) * POLE_REACH;
        let (mut grid, errors, poles) = loop {
            let grid = Self::with_cells(
                out_width,
                out_height,
                cells_x,
                cells_y,
                col_period,
                &out_to_world,
                &world_to_src_px,
            );
            let errors =
                grid.cell_errors((src_cols, src_rows), read, &out_to_world, &world_to_src_px);
            // Where the cells bracketing a pole lie, in output pixels, and
            // the nodes a pole lies beside.
            let mut poles: Vec<_> = (0..errors.len())
                .filter(|&k| errors[k].is_none())
                .map(|k| grid.cell_site(k % grid.cells_x, k / grid.cells_x))
                .collect();
            poles.extend(grid.node_pole_sites(&out_to_world, &world_to_src_px));
            // Once its cells are small enough, a cell near a pole is left to
            // `refine_locally` (#1195): there the error falls only as fast
            // as a cell's size, and refining the whole grid pays for every
            // cell to resolve a few.
            let exempt = grid.cell_w.max(grid.cell_h).powi(2) <= reach;
            let over = errors
                .iter()
                .enumerate()
                .filter(|&(k, e)| {
                    e.is_some_and(|e| e > MAX_INTERP_ERROR_PX)
                        && !(exempt
                            && grid.near_pole(k % grid.cells_x, k / grid.cells_x, &poles, reach))
                })
                .count();
            if over == 0 {
                break (grid, errors, poles);
            }
            if cells_x >= MAX_CELLS && cells_y >= MAX_CELLS {
                // Extreme viewport: even the densest grid cannot meet the
                // budget. Accept it, but record the error, so the
                // degradation is observable rather than passing silently.
                break (grid, errors, poles);
            }
            // Halving the cell size quarters the bilinear error.
            cells_x = (cells_x * 2).min(MAX_CELLS);
            cells_y = (cells_y * 2).min(MAX_CELLS);
        };
        let error = grid.refine_locally(
            (out_width, out_height),
            (src_cols, src_rows),
            &errors,
            (&poles, reach),
            &out_to_world,
            &world_to_src_px,
        );
        grid.capped = (error.0 > MAX_INTERP_ERROR_PX).then_some(error);
        grid.refine_boundary(
            (out_width, out_height),
            read,
            (&poles, reach),
            &out_to_world,
            &world_to_src_px,
        );
        grid
    }

    /// Cell `(cx, cy)`'s centre and reach, half its diagonal, in output
    /// pixels.
    fn cell_site(&self, cx: usize, cy: usize) -> (f64, f64, f64) {
        (
            (cx as f64 + 0.5) * self.cell_w,
            (cy as f64 + 0.5) * self.cell_h,
            self.cell_w.hypot(self.cell_h) / 2.0,
        )
    }

    /// The sites of the nodes a pole lies beside, with no cell bracketing
    /// it: a pole just past a node on the image's edge, or on the edge of
    /// the output CRS's domain, as EPSG:3067's pole is on a tile centred on
    /// its central meridian. The two sides of a row or a column meeting at
    /// such a node turn more than a quarter turn between them, though
    /// neither does alone, and their column does not run linearly from end
    /// to end (see [`Self::brackets_pole`]): two projections, at the sides'
    /// midpoints, for each such pair, none for any other. A site's reach is
    /// half a cell's diagonal.
    fn node_pole_sites(
        &self,
        out_to_world: impl Fn(f64, f64) -> (f64, f64),
        world_to_src_px: impl Fn(f64, f64) -> (f64, f64),
    ) -> Vec<(f64, f64, f64)> {
        let Some(p) = self.col_period else {
            return Vec::new();
        };
        let reach = self.cell_w.hypot(self.cell_h) / 2.0;
        let (nx, ny) = (self.cells_x as f64, self.cells_y as f64);
        let column = |i: usize, j: usize| self.nodes[j * self.stride + i].0;
        let mut sites = Vec::new();
        for j in 0..=self.cells_y {
            for i in 0..=self.cells_x {
                let mut pairs = Vec::with_capacity(2);
                if i > 0 && i < self.cells_x {
                    pairs.push(((i - 1, j), (i + 1, j)));
                }
                if j > 0 && j < self.cells_y {
                    pairs.push(((i, j - 1), (i, j + 1)));
                }
                let beside = pairs.into_iter().any(|((i0, j0), (i1, j1))| {
                    let (a, m, b) = (column(i0, j0), column(i, j), column(i1, j1));
                    let turn = wrap_half(m - a, p) + wrap_half(b - m, p);
                    turn.abs() > p / 4.0
                        && [0.25, 0.75].iter().any(|&t| {
                            let (x, y) = (
                                i0 as f64 + 2.0 * t * (i1 as f64 - i0 as f64) / 2.0,
                                j0 as f64 + 2.0 * t * (j1 as f64 - j0 as f64) / 2.0,
                            );
                            let (lon, lat) = out_to_world(x / nx, y / ny);
                            let col = world_to_src_px(lon, lat).0;
                            strays(col, a, turn, t, p)
                        })
                });
                if beside {
                    sites.push((i as f64 * self.cell_w, j as f64 * self.cell_h, reach));
                }
            }
        }
        sites
    }

    /// Whether some pixel of cell `(cx, cy)` lies within `reach` output
    /// pixels ([`POLE_REACH`]) of a cell bracketing a pole, `poles` their
    /// [`Self::cell_site`]s.
    fn near_pole(&self, cx: usize, cy: usize, poles: &[(f64, f64, f64)], reach: f64) -> bool {
        let (x, y, own) = self.cell_site(cx, cy);
        poles
            .iter()
            .any(|&(px, py, r)| (x - px).hypot(y - py) <= reach + r + own)
    }

    /// Split the cells bracketing a pole of a periodic column axis, and the
    /// cells near one ([`Self::near_pole`]) over [`NEAR_POLE_ERROR_PX`], in
    /// four, then their children over it, until every cell meets it or is
    /// [`MIN_LOCAL_CELL_PX`] wide (#1195). The worst cell is split first,
    /// a cell bracketing a pole before any, so a spent budget leaves the
    /// least error it can and never a pole's cells coarse. `errors` are the grid's
    /// [`Self::cell_errors`], `poles` the [`Self::cell_site`]s of the cells
    /// bracketing a pole, and `reach` [`POLE_REACH`] in output pixels. A
    /// child is the cell of the grid refined as a whole that it covers, with
    /// its nodes and its probes, so a cell is split exactly when refining the
    /// whole grid would have left it over budget. A grid without a pole is
    /// left as it is.
    ///
    /// Beside a pole the error falls as a cell's size squared over its
    /// distance to the pole, so the cells over budget shrink to a disc
    /// about a cell's size squared wide, a handful of cells at each depth.
    /// Every split costs at most [`SPLIT_COST`] projections, and the splits are
    /// capped at a quarter of the per-pixel cost, or of the nodes of a grid
    /// of [`MAX_CELLS`], whichever is more, so this stays a coarse mapping
    /// (Critical Rule 5). A cell whose new nodes are not all finite stays
    /// whole. Returns the largest estimated error of the cells not split
    /// and of the leaves, but for those bracketing a pole, and every limit
    /// that left one over budget.
    fn refine_locally(
        &mut self,
        (out_width, out_height): (u32, u32),
        src: (u32, u32),
        errors: &[Option<f64>],
        (poles, reach): (&[(f64, f64, f64)], f64),
        out_to_world: impl Fn(f64, f64) -> (f64, f64),
        world_to_src_px: impl Fn(f64, f64) -> (f64, f64),
    ) -> (f64, Limits) {
        let finite = |(c, r): (f64, f64)| c.is_finite() && r.is_finite();
        let floor = |out: u32| (f64::from(out) / f64::from(MAX_CELLS)).min(MIN_LOCAL_CELL_PX);
        let (floor_w, floor_h) = (floor(out_width), floor(out_height));
        let cap = MAX_CELLS as usize * MAX_CELLS as usize;
        let mut budget = (out_width as usize * out_height as usize).max(cap) / 4;
        // The largest error left, and what left a cell over budget.
        let mut worst = (0.0_f64, Limits::default());
        let mut leave = |error: Option<f64>, limit: Limit| {
            let error = error.unwrap_or(0.0);
            worst.0 = worst.0.max(error);
            if error > MAX_INTERP_ERROR_PX {
                let limits = &mut worst.1;
                match limit {
                    Limit::Cells => limits.cells = true,
                    Limit::Smallest => limits.smallest = true,
                    Limit::Splits => limits.splits = true,
                }
            }
        };
        let mut roots = Vec::new();
        for (k, &error) in errors.iter().enumerate() {
            let (cx, cy) = (k % self.cells_x, k / self.cells_x);
            let corners = self.corners(cx, cy);
            let split = match error {
                None => true,
                Some(e) => e > NEAR_POLE_ERROR_PX && self.near_pole(cx, cy, poles, reach),
            };
            if split && corners.into_iter().all(finite) {
                roots.push((k, corners, error));
            } else {
                leave(error, Limit::Cells);
            }
        }
        if roots.is_empty() {
            return worst;
        }
        let mut local = vec![NO_QUAD; self.cells_x * self.cells_y];
        let mut work = std::collections::BinaryHeap::with_capacity(roots.len());
        for (k, corners, error) in roots {
            local[k] = self.quads.len() as u32;
            self.quads.push(Quad::Leaf(corners));
            work.push(Pending {
                error,
                quad: local[k],
                cell: (k % self.cells_x, k / self.cells_x, 0),
                corners,
            });
        }
        while let Some(Pending {
            error,
            quad,
            cell: (cx, cy, depth),
            corners: [c00, c10, c01, c11],
        }) = work.pop()
        {
            let scale = f64::from(1_u32 << depth);
            if self.cell_w / scale <= floor_w && self.cell_h / scale <= floor_h {
                leave(error, Limit::Smallest);
                continue;
            }
            if budget < SPLIT_COST {
                leave(error, Limit::Splits);
                continue;
            }
            budget -= SPLIT_COST;
            let (nx, ny) = (
                (self.cells_x << (depth + 1)) as f64,
                (self.cells_y << (depth + 1)) as f64,
            );
            let node = |i: usize, j: usize| {
                let (lon, lat) = out_to_world((2 * cx + i) as f64 / nx, (2 * cy + j) as f64 / ny);
                world_to_src_px(lon, lat)
            };
            let (top, left, centre, right, bottom) =
                (node(1, 0), node(0, 1), node(1, 1), node(2, 1), node(1, 2));
            if ![top, left, centre, right, bottom].into_iter().all(finite) {
                leave(error, Limit::Smallest);
                continue;
            }
            self.quads[quad as usize] = Quad::Split(self.quads.len() as u32);
            for (i, j, corners) in [
                (0, 0, [c00, top, left, centre]),
                (1, 0, [top, c10, centre, right]),
                (0, 1, [left, centre, c01, bottom]),
                (1, 1, [centre, right, bottom, c11]),
            ] {
                let cell = (2 * cx + i, 2 * cy + j, depth + 1);
                let error = self.cell_error(src, cell, corners, &out_to_world, &world_to_src_px);
                let quad = self.quads.len() as u32;
                self.quads.push(Quad::Leaf(corners));
                match error {
                    Some(e) if e <= NEAR_POLE_ERROR_PX => leave(error, Limit::Smallest),
                    _ => work.push(Pending {
                        error,
                        quad,
                        cell,
                        corners,
                    }),
                }
            }
        }
        self.local = local;
        worst
    }

    /// The corner nodes `[c00, c10, c01, c11]` of cell `(cx, cy)`.
    fn corners(&self, cx: usize, cy: usize) -> [(f64, f64); 4] {
        let n = cy * self.stride + cx;
        [
            self.nodes[n],
            self.nodes[n + 1],
            self.nodes[n + self.stride],
            self.nodes[n + self.stride + 1],
        ]
    }

    /// Whether cell `(cx, cy)` holds a pixel of `read`, the inclusive output
    /// window `(px_lo, px_hi, py_lo, py_hi)` the caller samples; every cell
    /// does without one.
    fn reads_cell(&self, read: Option<(u32, u32, u32, u32)>, cx: usize, cy: usize) -> bool {
        let Some((x_lo, x_hi, y_lo, y_hi)) = read else {
            return true;
        };
        let overlaps = |c: usize, size: f64, lo: u32, hi: u32| {
            (c as f64 * size) < f64::from(hi) + 1.0 && ((c + 1) as f64 * size) > f64::from(lo)
        };
        overlaps(cx, self.cell_w, x_lo, x_hi) && overlaps(cy, self.cell_h, y_lo, y_hi)
    }

    /// Give every cell on a domain boundary a sub-grid at
    /// [`BOUNDARY_STEP_PX`] spacing, so the boundary is resolved to that
    /// many pixels instead of blanking the whole cell. A cell is on it when
    /// some corner nodes are finite and some not, or when the domain
    /// reaches in between corners that all miss it. The domain's edge does
    /// that where it has a corner, such as a wedge's tip (#1113): low in a
    /// 32 px cell, the tip misses every corner. The domain then reaches the cell across
    /// a side, from a neighbour whose sub-grid has a finite node along it,
    /// or from past the grid or the cells `read` holds, where the cell's
    /// own side is probed at the sub-grid spacing. A cell bracketing a
    /// pole is not on a boundary: its corners are finite, and it is
    /// refined locally ([`Self::refine_locally`]). A boundary cell near a
    /// pole ([`Self::near_pole`]) gets a sub-grid at the smallest local
    /// cell's spacing instead, as fine as the cells around it: EPSG:3067's
    /// domain ends on the line through its pole.
    ///
    /// This stays a coarse-grid mapping (Critical Rule 5): a boundary curve
    /// crosses O(perimeter / cell) cells, each costing
    /// `(cell / BOUNDARY_STEP_PX)²` projections, so a limb-sized curve costs
    /// on the order of its perimeter in pixels times four. The probes cost
    /// at most half the perimeter of the grid, or of the cells `read`
    /// holds, in pixels. Total sub-grid nodes and probes are capped at a
    /// quarter of the per-pixel cost; a mapping with scattered non-finite
    /// nodes stops refining there, and its remaining boundary cells stay
    /// unfilled. A cell holding no pixel of `read` is never sampled and
    /// gets none.
    fn refine_boundary(
        &mut self,
        (out_width, out_height): (u32, u32),
        read: Option<(u32, u32, u32, u32)>,
        (poles, reach): (&[(f64, f64, f64)], f64),
        out_to_world: impl Fn(f64, f64) -> (f64, f64),
        world_to_src_px: impl Fn(f64, f64) -> (f64, f64),
    ) {
        let finite = |(c, r): (f64, f64)| c.is_finite() && r.is_finite();
        let (cells_x, cells_y, stride) = (self.cells_x, self.cells_y, self.stride);
        // A cell's sub-grid size: steps of `BOUNDARY_STEP_PX`, or, near a
        // pole, of the smallest local cell, as fine as the cells around it.
        let floor = |out: u32| (f64::from(out) / f64::from(MAX_CELLS)).min(MIN_LOCAL_CELL_PX);
        let sub = |cx: usize, cy: usize| {
            let (step_x, step_y) = if self.near_pole(cx, cy, poles, reach) {
                (floor(out_width), floor(out_height))
            } else {
                (BOUNDARY_STEP_PX, BOUNDARY_STEP_PX)
            };
            (
                (self.cell_w / step_x).ceil().max(1.0) as usize,
                (self.cell_h / step_y).ceil().max(1.0) as usize,
            )
        };
        let mut budget = (out_width as usize * out_height as usize / 4).max(4096);
        let corners = |cx: usize, cy: usize| self.corners(cx, cy);
        // Sub-grid node (i, j) of cell (cx, cy).
        let node = |cx: usize, cy: usize, i: usize, j: usize| {
            let (sub_x, sub_y) = sub(cx, cy);
            let fx = (cx as f64 + i as f64 / sub_x as f64) / cells_x as f64;
            let fy = (cy as f64 + j as f64 / sub_y as f64) / cells_y as f64;
            let (lon, lat) = out_to_world(fx, fy);
            world_to_src_px(lon, lat)
        };
        // A cell's sides, top, bottom, left and right: the sub-grid nodes
        // between its corners, and the read cell across, if any.
        let side = |(sub_x, sub_y): (usize, usize), s: usize| -> Vec<(usize, usize)> {
            match s {
                0 => (1..sub_x).map(|i| (i, 0)).collect(),
                1 => (1..sub_x).map(|i| (i, sub_y)).collect(),
                2 => (1..sub_y).map(|j| (0, j)).collect(),
                _ => (1..sub_y).map(|j| (sub_x, j)).collect(),
            }
        };
        let across = |cx: usize, cy: usize, s: usize| {
            let (x, y) = match s {
                0 => (Some(cx), cy.checked_sub(1)),
                1 => (Some(cx), Some(cy + 1)),
                2 => (cx.checked_sub(1), Some(cy)),
                _ => (Some(cx + 1), Some(cy)),
            };
            let (x, y) = (x?, y?);
            (x < cells_x && y < cells_y && self.reads_cell(read, x, y)).then_some((x, y))
        };
        let mut queued = vec![false; cells_x * cells_y];
        let mut work = Vec::new();
        for cy in 0..cells_y {
            for cx in 0..cells_x {
                if !self.reads_cell(read, cx, cy) {
                    continue;
                }
                let valid = corners(cx, cy).iter().filter(|&&c| finite(c)).count();
                let on_boundary = match valid {
                    // The domain can reach in only across a side no other
                    // cell's sub-grid runs along.
                    0 => (0..4).filter(|&s| across(cx, cy, s).is_none()).any(|s| {
                        let probes = side(sub(cx, cy), s);
                        if probes.len() > budget {
                            return false;
                        }
                        budget -= probes.len();
                        probes.into_iter().any(|(i, j)| finite(node(cx, cy, i, j)))
                    }),
                    4 => false,
                    _ => true,
                };
                if on_boundary {
                    queued[cy * cells_x + cx] = true;
                    work.push((cx, cy));
                }
            }
        }
        let mut boundary = Vec::with_capacity(work.len());
        let mut next = 0;
        while let Some(&(cx, cy)) = work.get(next) {
            next += 1;
            let (sub_x, sub_y) = sub(cx, cy);
            let per_cell = (sub_x + 1) * (sub_y + 1);
            if per_cell > budget {
                break;
            }
            budget -= per_cell;
            let mut nodes = Vec::with_capacity(per_cell);
            for j in 0..=sub_y {
                for i in 0..=sub_x {
                    nodes.push(node(cx, cy, i, j));
                }
            }
            // The domain reaching on across a side into a cell whose
            // corners all miss it.
            for s in 0..4 {
                let Some((x, y)) = across(cx, cy, s) else {
                    continue;
                };
                if queued[y * cells_x + x] || corners(x, y).into_iter().any(finite) {
                    continue;
                }
                if side((sub_x, sub_y), s)
                    .into_iter()
                    .any(|(i, j)| finite(nodes[j * (sub_x + 1) + i]))
                {
                    queued[y * cells_x + x] = true;
                    work.push((x, y));
                }
            }
            boundary.push((
                cy * stride + cx,
                SubGrid {
                    cells_x: sub_x,
                    cells_y: sub_y,
                    nodes,
                },
            ));
        }
        boundary.sort_unstable_by_key(|&(n, _)| n);
        self.boundary = boundary;
    }

    /// Build a grid with an explicit cell count (no refinement).
    #[allow(clippy::too_many_arguments)] // output dims + cell counts + period + 2 mapping closures are all genuine inputs
    fn with_cells(
        out_width: u32,
        out_height: u32,
        cells_x: u32,
        cells_y: u32,
        col_period: Option<f64>,
        out_to_world: impl Fn(f64, f64) -> (f64, f64),
        world_to_src_px: impl Fn(f64, f64) -> (f64, f64),
    ) -> Self {
        let cells_x = cells_x as usize;
        let cells_y = cells_y as usize;
        let stride = cells_x + 1;

        let mut nodes = Vec::with_capacity(stride * (cells_y + 1));
        for j in 0..=cells_y {
            let fy = j as f64 / cells_y as f64;
            for i in 0..=cells_x {
                let (lon, lat) = out_to_world(i as f64 / cells_x as f64, fy);
                nodes.push(world_to_src_px(lon, lat));
            }
        }

        ProjectionGrid {
            cells_x,
            cells_y,
            cell_w: out_width as f64 / cells_x as f64,
            cell_h: out_height as f64 / cells_y as f64,
            stride,
            nodes,
            boundary: Vec::new(),
            col_period,
            local: Vec::new(),
            quads: Vec::new(),
            capped: None,
        }
    }

    /// Estimate the worst-case bilinear-interpolation error of every cell,
    /// row-major, in source pixels: [`Self::cell_error`], `None` for a cell
    /// bracketing a pole.
    ///
    /// A cell holding no pixel of `read`, the output window the caller
    /// samples, is skipped whole, `Some(0.0)`: no error there is ever seen
    /// (see [`Self::build_2d_within`]).
    fn cell_errors(
        &self,
        src: (u32, u32),
        read: Option<(u32, u32, u32, u32)>,
        out_to_world: impl Fn(f64, f64) -> (f64, f64),
        world_to_src_px: impl Fn(f64, f64) -> (f64, f64),
    ) -> Vec<Option<f64>> {
        let mut errors = Vec::with_capacity(self.cells_x * self.cells_y);
        for j in 0..self.cells_y {
            for i in 0..self.cells_x {
                errors.push(if self.reads_cell(read, i, j) {
                    self.cell_error(
                        src,
                        (i, j, 0),
                        self.corners(i, j),
                        &out_to_world,
                        &world_to_src_px,
                    )
                } else {
                    Some(0.0)
                });
            }
        }
        errors
    }

    /// Whether cell `(i, j)` of this grid refined `depth` times as a whole
    /// brackets a pole of a periodic column axis. Around a pole every
    /// longitude meets, so the column is singular there and has no
    /// bilinear interpolant: a cell enclosing one winds a whole turn, and
    /// one beside it, with the pole on or just past an edge, turns more than
    /// a quarter turn along that edge. Refining never resolves such a cell
    /// (the geometry is self-similar around the pole), so it is left out of
    /// the error estimate and split down to the smallest local cell
    /// ([`Self::refine_locally`]), confining the singularity to a pixel.
    ///
    /// A cell wide in longitude turns as far with no pole near it, as in a
    /// geographic view a few cells across a turn, but its column runs
    /// linearly along the edge. So an edge turning more than a quarter turn
    /// is the pole's only when its column, two projections at a quarter and
    /// three quarters of the way, strays from the line between its corners
    /// by more than a 64th of the turn; beside a pole it strays by at least a
    /// 22nd. `corners` are unwrapped as [`Self::sample`] interpolates them.
    /// Always `false` without a period or with a non-finite corner.
    fn brackets_pole(
        &self,
        (i, j, depth): (usize, usize, u32),
        corners: &[(f64, f64); 4],
        out_to_world: impl Fn(f64, f64) -> (f64, f64),
        world_to_src_px: impl Fn(f64, f64) -> (f64, f64),
    ) -> bool {
        let Some(p) = self.col_period else {
            return false;
        };
        let [c00, c10, c01, c11] = corners.map(|c| c.0);
        let (x, y) = (i as f64, j as f64);
        // The sides in order around the cell, from corner to corner.
        let sides = [
            (c00, c10, (x, y), (x + 1.0, y)),
            (c10, c11, (x + 1.0, y), (x + 1.0, y + 1.0)),
            (c11, c01, (x + 1.0, y + 1.0), (x, y + 1.0)),
            (c01, c00, (x, y + 1.0), (x, y)),
        ];
        let winding: f64 = sides.iter().map(|&(a, b, ..)| wrap_half(b - a, p)).sum();
        if winding.abs() > p / 2.0 {
            return true;
        }
        let (cells_x, cells_y) = (
            (self.cells_x << depth) as f64,
            (self.cells_y << depth) as f64,
        );
        sides.iter().any(|&(a, b, (x0, y0), (x1, y1))| {
            let turn = b - a;
            turn.abs() > p / 4.0
                && [0.25, 0.75].iter().any(|&t| {
                    let (lon, lat) = out_to_world(
                        (x0 + t * (x1 - x0)) / cells_x,
                        (y0 + t * (y1 - y0)) / cells_y,
                    );
                    let col = world_to_src_px(lon, lat).0;
                    strays(col, a, turn, t, p)
                })
        })
    }

    /// Estimate the worst-case bilinear-interpolation error, in source
    /// pixels, of cell `(i, j)` of this grid refined `depth` times as a
    /// whole, `cells_x << depth` by `cells_y << depth` cells, whose corner
    /// nodes are `corners`; `None` when it brackets a pole of a periodic
    /// column axis ([`Self::brackets_pole`]).
    ///
    /// It compares the bilinearly-interpolated value against the exact
    /// mapping at a dense regular grid of interior probe points (the
    /// [`PROBE_FRACS`]²) and keeps the largest deviation. The dense grid is what
    /// makes this a faithful proxy for the true per-pixel maximum: a sparse,
    /// centre-clustered probe set misses the residual peak (which sits near the
    /// cell edges for a real projection over a wide cell) and under-reports the
    /// error, defeating the refinement loop. Probe points whose exact mapping
    /// falls far outside the raster are skipped — those output pixels resolve to
    /// nodata regardless of interpolation error, so refining for them would be
    /// wasted work (and could needlessly hit the [`MAX_CELLS`] cap on a viewport
    /// that mostly misses the raster).
    ///
    /// On a periodic column axis every column is on-raster, the column error
    /// is taken modulo the period and scaled by cos lat (a distance along
    /// the parallel), and a cell bracketing a pole has no estimate: it is
    /// split down to the smallest local cell instead (see
    /// [`Self::build_2d_periodic`]). That ground error
    /// is reported in output pixels, using the cell's own footprint: a
    /// global grid's pixel is tens of kilometres, several output pixels of a
    /// regional view, so a fifth of a source pixel would still be visibly
    /// misregistered there.
    fn cell_error(
        &self,
        (src_cols, src_rows): (u32, u32),
        (i, j, depth): (usize, usize, u32),
        corners: [(f64, f64); 4],
        out_to_world: impl Fn(f64, f64) -> (f64, f64),
        world_to_src_px: impl Fn(f64, f64) -> (f64, f64),
    ) -> Option<f64> {
        // Generous on-raster window: within one raster-size of the data.
        let (w, h) = (src_cols as f64, src_rows as f64);
        let periodic = self.col_period.is_some();
        let in_window =
            |c: f64, r: f64| (periodic || (c > -w && c < 2.0 * w)) && r > -h && r < 2.0 * h;
        let corners = unwrap_cols(corners, self.col_period);
        if self.brackets_pole((i, j, depth), &corners, &out_to_world, &world_to_src_px) {
            return None;
        }
        // The cells at this depth: halving a size is exact, so these are
        // the sizes `with_cells` gives a grid of that many cells.
        let scale = f64::from(1_u32 << depth);
        let (cells_x, cells_y) = (
            (self.cells_x << depth) as f64,
            (self.cells_y << depth) as f64,
        );
        let (cell_w, cell_h) = (self.cell_w / scale, self.cell_h / scale);
        let [c00, c10, c01, c11] = corners;
        let mut max = 0.0_f64;
        for (tx, ty) in error_probes() {
            let (lon, lat) = out_to_world((i as f64 + tx) / cells_x, (j as f64 + ty) / cells_y);
            let (ec, er) = world_to_src_px(lon, lat);
            if !in_window(ec, er) {
                continue;
            }
            let ic = bilerp(c00.0, c10.0, c01.0, c11.0, tx, ty);
            let ir = bilerp(c00.1, c10.1, c01.1, c11.1, tx, ty);
            let Some(p) = self.col_period else {
                max = max.max((ic - ec).abs()).max((ir - er).abs());
                continue;
            };
            let k = lat.to_radians().cos().abs();
            let err = (wrap_half(ic - ec, p).abs() * k).max((ir - er).abs());
            // One output pixel's footprint, in the same units: the
            // cell's mean edge per output pixel along each axis.
            let edge = |a: (f64, f64), b: (f64, f64)| ((b.0 - a.0) * k).hypot(b.1 - a.1);
            let px = ((edge(c00, c10) + edge(c01, c11)) / cell_w)
                .max((edge(c00, c01) + edge(c10, c11)) / cell_h)
                / 2.0;
            max = max.max(if px > 0.0 { err / px } else { err });
        }
        Some(max)
    }

    /// Exact projections the grid holds: its nodes, its sub-grids' and its
    /// local splits' (five new nodes each).
    #[cfg(test)]
    pub(crate) fn node_count(&self) -> usize {
        self.nodes.len()
            + self
                .boundary
                .iter()
                .map(|(_, s)| s.nodes.len())
                .sum::<usize>()
            + 5 * self
                .quads
                .iter()
                .filter(|q| matches!(q, Quad::Split(_)))
                .count()
    }

    /// Interpolated source `(col, row)` for the centre of output pixel
    /// `(ox, oy)`. Coordinates are fractional and unclamped — the caller floors
    /// and bounds-checks them, exactly as `GeoTransform::world_to_pixel` does.
    ///
    /// The result is non-finite where the mapping is undefined; the caller
    /// must finite-check before use (a non-finite result resolves to
    /// nodata/transparent). Non-finite nodes arise when `out_to_world` or
    /// `world_to_src_px` returns NaN: a degenerate source mapping (zero pixel
    /// size), a source that is only partly defined (a geostationary disk,
    /// whose far side has no projection), or a projected output CRS past its
    /// valid area. A cell with some finite and some non-finite corners is on
    /// such a domain boundary and is interpolated from its
    /// [`BOUNDARY_STEP_PX`] sub-grid, so the defined region is under-filled
    /// by at most that many pixels along the boundary.
    ///
    /// On a periodic column axis ([`Self::build_2d_periodic`]) the column is
    /// continuous across the source's seam but not reduced into the grid:
    /// the caller wraps it modulo the period.
    pub fn sample(&self, ox: u32, oy: u32) -> (f64, f64) {
        // Position of the pixel centre in grid-cell units.
        let gx = (ox as f64 + 0.5) / self.cell_w;
        let gy = (oy as f64 + 0.5) / self.cell_h;
        // Enclosing cell, clamped so the +1 node lookups stay in bounds.
        let cx = (gx.floor() as usize).min(self.cells_x - 1);
        let cy = (gy.floor() as usize).min(self.cells_y - 1);
        let tx = gx - cx as f64;
        let ty = gy - cy as f64;
        if let Some(&quad) = self.local.get(cy * self.cells_x + cx) {
            if quad != NO_QUAD {
                return self.sample_quad(quad, tx, ty);
            }
        }

        let i00 = cy * self.stride + cx;
        let [c00, c10, c01, c11] = unwrap_cols(
            [
                self.nodes[i00],
                self.nodes[i00 + 1],
                self.nodes[i00 + self.stride],
                self.nodes[i00 + self.stride + 1],
            ],
            self.col_period,
        );

        let col = bilerp(c00.0, c10.0, c01.0, c11.0, tx, ty);
        let row = bilerp(c00.1, c10.1, c01.1, c11.1, tx, ty);
        if col.is_finite() && row.is_finite() {
            return (col, row);
        }
        match self.boundary.binary_search_by_key(&i00, |(cell, _)| *cell) {
            Ok(k) => self.boundary[k].1.sample(tx, ty, self.col_period),
            Err(_) => (col, row),
        }
    }

    /// [`Self::sample`] at fractional position `(tx, ty)` within a locally
    /// refined cell, from the leaf of its quadtree `quad` that holds it. A
    /// child's position is its parent's doubled less a whole one, exact in
    /// floating point, so a leaf samples as the cell of the grid refined as
    /// a whole that it is.
    fn sample_quad(&self, mut quad: u32, mut tx: f64, mut ty: f64) -> (f64, f64) {
        loop {
            match self.quads[quad as usize] {
                Quad::Split(first) => {
                    let (right, below) = (tx >= 0.5, ty >= 0.5);
                    tx = 2.0 * tx - f64::from(u8::from(right));
                    ty = 2.0 * ty - f64::from(u8::from(below));
                    quad = first + u32::from(right) + 2 * u32::from(below);
                }
                Quad::Leaf(corners) => {
                    let [c00, c10, c01, c11] = unwrap_cols(corners, self.col_period);
                    return (
                        bilerp(c00.0, c10.0, c01.0, c11.0, tx, ty),
                        bilerp(c00.1, c10.1, c01.1, c11.1, tx, ty),
                    );
                }
            }
        }
    }

    /// How many times the cell [`Self::sample`] reads output pixel
    /// `(ox, oy)` from was split locally: 0 for a cell of the grid.
    #[cfg(test)]
    fn local_depth(&self, ox: u32, oy: u32) -> u32 {
        let gx = (ox as f64 + 0.5) / self.cell_w;
        let gy = (oy as f64 + 0.5) / self.cell_h;
        let cx = (gx.floor() as usize).min(self.cells_x - 1);
        let cy = (gy.floor() as usize).min(self.cells_y - 1);
        let Some(&root) = self
            .local
            .get(cy * self.cells_x + cx)
            .filter(|&&q| q != NO_QUAD)
        else {
            return 0;
        };
        let (mut quad, mut tx, mut ty, mut depth) = (root, gx - cx as f64, gy - cy as f64, 0);
        while let Quad::Split(first) = self.quads[quad as usize] {
            let (right, below) = (tx >= 0.5, ty >= 0.5);
            tx = 2.0 * tx - f64::from(u8::from(right));
            ty = 2.0 * ty - f64::from(u8::from(below));
            quad = first + u32::from(right) + 2 * u32::from(below);
            depth += 1;
        }
        depth
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
    use crate::geo::{Crs, GeoTransform};

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
                figure: crate::geo::Ellipsoid::WGS84,
            },
        }
    }

    /// TM35FIN at the *production* FMI composite resolution (4963×7316, 250 m
    /// pixels). The interpolation-error budget is in **source pixels**, so the
    /// same geographic misregistration is ~10× more pixels here than on the
    /// 480×360 fixture — which is exactly why the under-refinement bug was
    /// invisible at fixture resolution but produced visible "curtain" artifacts
    /// in production. The error-estimator probe density must be dense enough to
    /// hold the budget at this resolution.
    fn tm35fin_full_res_transform() -> GeoTransform {
        GeoTransform {
            origin_x: -196_593.004,
            origin_y: 8_084_432.005,
            pixel_width: 250.004,
            pixel_height: 250.014,
            width: 4963,
            height: 7316,
            crs: Crs::TransverseMercator {
                lat0: 0.0,
                lon0: 27.0_f64.to_radians(),
                k0: 0.9996,
                false_e: 500_000.0,
                false_n: 0.0,
                figure: crate::geo::Ellipsoid::WGS84,
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

    /// Oblique-stereographic GeoTransform shaped like an FMI/DMI ODIM radar
    /// composite (the projection family `engine-odim` renders through — see the
    /// `geo.rs` "Oblique Stereographic — used by FMI/DMI ODIM radar composites"
    /// note). Centred on Finland so projected coordinates stay near the origin;
    /// ±480 km easting × ±360 km northing at 2 km pixels.
    fn stereographic_transform() -> GeoTransform {
        GeoTransform {
            origin_x: -480_000.0,
            origin_y: 360_000.0,
            pixel_width: 2_000.0,
            pixel_height: 2_000.0,
            width: 480,
            height: 360,
            crs: Crs::Stereographic {
                lat0: 64.0_f64.to_radians(),
                lon0: 26.0_f64.to_radians(),
                k0: 1.0,
                false_e: 0.0,
                false_n: 0.0,
                figure: crate::geo::Ellipsoid::WGS84,
            },
        }
    }

    /// Largest deviation between the coarse grid and the exact per-pixel
    /// mapping, in source pixels, over every pixel of a `width`×`height` output
    /// image — but only counting pixels that land on the raster, since
    /// off-raster pixels resolve to nodata regardless of interpolation error.
    ///
    /// This is the `ProjectionGrid`-vs-per-pixel-reference correctness check:
    /// the grid is built with the coarse-node mapping, then compared against
    /// `gt.world_to_pixel_f64` evaluated at *every* output pixel.
    fn max_grid_error(
        gt: &GeoTransform,
        width: u32,
        height: u32,
        lon_at: impl Fn(f64) -> f64 + Copy,
        lat_at: impl Fn(f64) -> f64 + Copy,
    ) -> f64 {
        let grid = ProjectionGrid::build(
            width,
            height,
            gt.width,
            gt.height,
            lon_at,
            lat_at,
            |lon, lat| gt.world_to_pixel_f64(lon, lat),
        );
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
    fn grid_matches_exact_stereographic() {
        // ODIM radar composites are oblique-stereographic; the grid must hold
        // budget for that projection family too, on both output-axis modes.
        let gt = stereographic_transform();
        // Linear (WGS84) output axes over a Finland viewport covering the grid.
        let err = max_grid_error(&gt, 1024, 768, |fx| 19.0 + fx * 15.0, |fy| 70.0 - fy * 11.0);
        assert!(
            err < 0.5,
            "stereographic grid error {err} px exceeds budget"
        );
        // Non-linear (Web Mercator) output Y axis — the other ODIM render mode.
        let err = max_grid_error(
            &gt,
            1024,
            768,
            |fx| 19.0 + fx * 15.0,
            |fy| merc_lat(&(59.0, 70.0), fy),
        );
        assert!(
            err < 0.5,
            "stereographic Web Mercator grid error {err} px exceeds budget"
        );
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
    fn grid_holds_budget_at_production_resolution() {
        // Regression for the reported "east curtains" artifact: at the real FMI
        // composite resolution (4963×7316), a zoomed-out Web Mercator viewport
        // like the bug report's must still meet the source-pixel budget. With
        // the old sparse, centre-clustered probe set the estimator under-reported
        // the error (~0.08 px) and stopped refining while the TRUE error was
        // ~1.6 source px — visible as dotted/curtain misregistration. The dense
        // probe grid catches the residual peak so refinement actually converges.
        let gt = tm35fin_full_res_transform();
        // The user's "URL1" viewport: lon −13..73.8°E, lat 42.7..74.3°N, WebMerc.
        let err = max_grid_error(
            &gt,
            700,
            535,
            |fx| -13.06 + fx * 86.86,
            |fy| merc_lat(&(42.67, 74.33), fy),
        );
        assert!(
            err < 0.5,
            "production-resolution wide-viewport grid error {err} px exceeds \
             budget (the sparse-probe estimator left ~1.6 px → curtain artifacts)"
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
        let grid = ProjectionGrid::build(
            800,
            600,
            gt.width,
            gt.height,
            |fx| 19.0 + fx * 13.0,
            |fy| 70.0 - fy * 11.0,
            |lon, lat| gt.world_to_pixel_f64(lon, lat),
        );
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
        let grid = ProjectionGrid::build(
            64,
            64,
            gt.width,
            gt.height,
            |fx| fx,
            |fy| fy,
            |lon, lat| gt.world_to_pixel_f64(lon, lat),
        );
        let (c, r) = grid.sample(10, 10);
        assert!(!c.is_finite() || !r.is_finite());
    }

    /// A source defined only inside a circle — the shape of a geostationary
    /// disk. Cells the circle crosses are refined, so the defined region is
    /// filled to within the sub-grid step of the boundary rather than a
    /// whole cell short of it, and nothing outside the circle is sampled.
    #[test]
    fn boundary_cells_resolve_a_partial_domain() {
        let radius = 0.7;
        let map = |x: f64, y: f64| {
            if x * x + y * y <= radius * radius {
                (x * 100.0, y * 100.0)
            } else {
                (f64::NAN, f64::NAN)
            }
        };
        let size = 256;
        let grid = ProjectionGrid::build(
            size,
            size,
            200,
            200,
            |fx| -1.0 + 2.0 * fx,
            |fy| 1.0 - 2.0 * fy,
            map,
        );
        assert!(!grid.boundary.is_empty());
        let px = 2.0 / size as f64;
        let margin = 2.0 * BOUNDARY_STEP_PX * px;
        for oy in 0..size {
            for ox in 0..size {
                let x = -1.0 + (ox as f64 + 0.5) * px;
                let y = 1.0 - (oy as f64 + 0.5) * px;
                let d = x.hypot(y);
                let (c, r) = grid.sample(ox, oy);
                if d < radius - margin {
                    assert!(
                        (c - x * 100.0).abs() < 0.5 && (r - y * 100.0).abs() < 0.5,
                        "({ox}, {oy}) inside the domain sampled ({c}, {r})"
                    );
                } else if d > radius {
                    assert!(!c.is_finite() || !r.is_finite(), "({ox}, {oy}) outside");
                }
            }
        }
    }

    /// Scattered non-finite nodes make every cell a boundary cell; the
    /// refinement stops at its node budget instead of approaching a
    /// per-pixel projection.
    #[test]
    fn boundary_refinement_is_budgeted() {
        let map = |x: f64, y: f64| {
            if ((x * 97.0).floor() + (y * 89.0).floor()) as i64 % 2 == 0 {
                (x, y)
            } else {
                (f64::NAN, f64::NAN)
            }
        };
        let size = 512;
        let grid = ProjectionGrid::build(size, size, 100, 100, |fx| fx, |fy| fy, map);
        let nodes: usize = grid.boundary.iter().map(|(_, sub)| sub.nodes.len()).sum();
        assert!(
            nodes <= (size * size / 4) as usize,
            "{nodes} sub-grid nodes"
        );
    }

    /// #1110: a caller that reads only part of the output gets a grid
    /// refined for that part. Past the read window this mapping jumps 37
    /// source columns, then is undefined: without the window the jump
    /// alone refines to the cap, and the undefined part's edge would get
    /// sub-grids. With it the grid stays at its starting 8 × 8 cells, exact
    /// where it is read. A cell holding a single read pixel counts whole.
    #[test]
    fn a_read_window_refines_only_what_is_read() {
        let map = |x: f64, y: f64| {
            if x < 0.6 {
                (x * 100.0, y * 100.0)
            } else if x < 0.8 {
                (x * 100.0 + 37.0, y * 100.0)
            } else {
                (f64::NAN, f64::NAN)
            }
        };
        let size = 256;
        let build =
            |read| ProjectionGrid::build_2d_within(size, size, read, 100, 100, |x, y| (x, y), map);
        let grid = build((0, 127, 0, 255));
        assert_eq!((grid.cells_x, grid.cells_y, grid.node_count()), (8, 8, 81));
        for oy in 0..size {
            for ox in 0..=127 {
                let (c, r) = grid.sample(ox, oy);
                let (x, y) = ((ox as f64 + 0.5) / 256.0, (oy as f64 + 0.5) / 256.0);
                assert!((c - x * 100.0).abs() < 1e-9 && (r - y * 100.0).abs() < 1e-9);
            }
        }
        // Pixel 128 shares the 8 × 8 cell [128, 160) with the jump at 153.6.
        let grid = build((0, 128, 0, 255));
        assert_eq!(
            (grid.cells_x, grid.cells_y, grid.node_count()),
            (16, 16, 289)
        );
    }

    /// #1129: a grid that stops at `MAX_CELLS` over budget records its
    /// error, so a caller of the quiet constructor can report it with what
    /// it knows of the request. Every other constructor caps the same way
    /// and records the same error.
    #[test]
    fn a_capped_grid_records_its_error() {
        // A jump 37 source columns wide at x = 0.6: no cell size
        // interpolates across it.
        let map = |x: f64, y: f64| {
            let jump = if x < 0.6 { 0.0 } else { 37.0 };
            (x * 100.0 + jump, y * 100.0)
        };
        let size = 256;
        let node = |x: f64, y: f64| (x, y);
        let all = (0, size - 1, 0, size - 1);
        let quiet = ProjectionGrid::build_2d_within_quiet(size, size, all, 100, 100, node, map);
        let cap = MAX_CELLS as usize;
        assert_eq!((quiet.cells_x, quiet.cells_y), (cap, cap));
        let error = quiet.capped_error().expect("the jump caps the grid");
        assert!(error > MAX_INTERP_ERROR_PX, "capped at {error}");
        let within = ProjectionGrid::build_2d_within(size, size, all, 100, 100, node, map);
        let whole = ProjectionGrid::build_2d(size, size, 100, 100, node, map);
        assert_eq!(within.capped_error(), Some(error));
        assert_eq!(whole.capped_error(), Some(error));
        assert_eq!(
            quiet.capped.map(|(_, limits)| limits),
            Some(Limits {
                cells: true,
                ..Limits::default()
            })
        );
        // Read short of the jump, the grid meets the budget.
        let short = ProjectionGrid::build_2d_within_quiet(
            size,
            size,
            (0, 127, 0, size - 1),
            100,
            100,
            node,
            map,
        );
        assert_eq!(short.capped_error(), None);
    }

    /// Great-circle distance in km (sphere) — the ground misregistration
    /// between a sampled and a reference position.
    fn ground_km((lon1, lat1): (f64, f64), (lon2, lat2): (f64, f64)) -> f64 {
        let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
        let dl = (lon2 - lon1).to_radians();
        let h = ((p2 - p1) / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
        2.0 * 6371.0 * h.sqrt().min(1.0).asin()
    }

    /// #323: a global lat/lon source (GFS-shaped: 0.25°, 1440×721, first
    /// column at Greenwich) on the EPSG:3035 WMS meta-tile (level 10, 4 km
    /// texels) holding the North Pole and the 170°W longitude cut of the
    /// 3035 inverse. Without a period the cut cells interpolated through the
    /// whole grid (samples ~180° away), and the pole drove every such tile
    /// to `MAX_CELLS` — tens of seconds per Arctic view.
    ///
    /// #1195: refined as a whole, the grid met its budget only at
    /// `MAX_CELLS`, 1 485 934 projections (0.45 s in release) to resolve the
    /// pixels beside the pole. It stops at 32 × 32 cells and splits the
    /// cells around the pole down to pixel-sized cells: 36 644 projections.
    /// Each pixel reads the cell of the grid refined as a whole that its
    /// leaf is, bit for bit, so the pixels beside the pole are the 256-cell
    /// grid's. Its per-pixel error is pinned with the other polar tiles'
    /// (`polar_tiles_meet_the_budget_at_every_pixel`).
    #[test]
    fn periodic_grid_registers_a_global_source_over_the_pole() {
        use crate::map_engine::OutputCrs;
        use std::cell::Cell;
        let bbox = [4_096_000.0, 7_168_000.0, 5_120_000.0, 8_192_000.0];
        let out = OutputCrs::Projected {
            crs: crate::geo::projected_output_crs("EPSG:3035").unwrap(),
            bbox,
        };
        let size = 256;
        let node = |fx: f64, fy: f64| out.project_node([0.0; 4], fx, fy);
        let projections = Cell::new(0_usize);
        let to_src = |lon: f64, lat: f64| {
            projections.set(projections.get() + 1);
            (lon / 0.25, (90.0 - lat) / 0.25)
        };
        let grid =
            ProjectionGrid::build_2d_periodic(size, size, 1440, 721, Some(1440.0), node, to_src);
        assert!(
            projections.get() < 40_000,
            "{} projections",
            projections.get()
        );
        assert_eq!((grid.cells_x, grid.cells_y), (32, 32));
        // Converged within budget: no MAX_CELLS warning for the pole tile.
        assert_eq!(grid.capped_error(), None);
        // The pixel at the pole reads a cell split down to a pixel.
        assert_eq!(grid.local_depth(56, 205), 3);
        let uniform: Vec<_> = (0..=3)
            .map(|depth| {
                ProjectionGrid::with_cells(
                    size,
                    size,
                    32 << depth,
                    32 << depth,
                    Some(1440.0),
                    node,
                    to_src,
                )
            })
            .collect();
        for oy in 0..size {
            for ox in 0..size {
                let depth = grid.local_depth(ox, oy) as usize;
                let (c, r) = grid.sample(ox, oy);
                let (uc, ur) = uniform[depth].sample(ox, oy);
                assert!(
                    c.to_bits() == uc.to_bits() && r.to_bits() == ur.to_bits(),
                    "pixel ({ox}, {oy}) at depth {depth}: ({c}, {r}), refined as a whole \
                     ({uc}, {ur})"
                );
            }
        }
        let at = |ox: u32, oy: u32| {
            let (c, r) = grid.sample(ox, oy);
            (c * 0.25, 90.0 - r * 0.25)
        };
        // Pixel centres pinned to `cs2cs +proj=laea +lat_0=52 +lon_0=10
        // +x_0=4321000 +y_0=3210000 +datum=WGS84 +units=m +to
        // +proj=longlat +datum=WGS84` (PROJ 9.8): either side of the cut,
        // around the pole (at pixel 56.25, 205.57), and the corners.
        let pins = [
            (55, 100, -169.641582578, 85.991893101),
            (56, 100, -170.119473941, 85.991965549),
            (55, 20, -169.799393084, 82.903521887),
            (56, 20, -170.066869241, 82.903563322),
            (60, 200, 153.238680311, 89.759875091),
            (50, 210, -36.208945280, 89.730344939),
            (52, 205, -81.248427083, 89.873093569),
            (62, 208, 72.354877183, 89.761205443),
            (0, 0, -156.855754147, 81.896455619),
            (255, 0, 150.865239665, 79.563619678),
            (0, 255, -35.403882861, 87.335741867),
            (255, 255, 86.139274750, 83.015023026),
            (128, 128, 150.727354516, 86.172095052),
        ];
        for (ox, oy, lon, lat) in pins {
            let km = ground_km(at(ox, oy), (lon, lat));
            assert!(
                km < 2.0,
                "pixel ({ox}, {oy}) sampled {:?}, PROJ ({lon}, {lat}): {km:.1} km off",
                at(ox, oy)
            );
        }
    }

    /// #1195: on 256 px tiles of 4 km texels around the North Pole, a global
    /// 0.25° lat/lon source's grid places every pixel within the 0.2 px
    /// budget of the exact per-pixel mapping, measured, not estimated, and
    /// costs a fortieth of refining the whole grid, as it did, to 256 cells
    /// (1.49 M projections each). Within a pixel of a pole the tile holds,
    /// its cells are interpolated across the singularity: there within a
    /// pixel, as refined as a whole (0.72 at most). The tiles: #323's; the
    /// pole on a node, where four cells meet; the pole 1.5 px past the
    /// image's left edge, where no cell holds it; and EPSG:3067's tile
    /// centred on its central meridian, the pole on the edge of the
    /// projection's domain and on a node column at every density, where
    /// only the row of nodes below it shows it (`node_pole_sites`).
    #[test]
    fn polar_tiles_meet_the_budget_at_every_pixel() {
        use crate::map_engine::OutputCrs;
        use std::cell::Cell;
        let size = 256_u32;
        let texel = 4000.0;
        let unit = |(lon, lat): (f64, f64)| {
            let (l, p) = (lon.to_radians(), lat.to_radians());
            [p.cos() * l.cos(), p.cos() * l.sin(), p.sin()]
        };
        for (epsg, pole, ceiling) in [
            ("EPSG:3035", (56.25, 205.57), 40_000),
            ("EPSG:3035", (64.0, 192.0), 40_000),
            ("EPSG:3035", (-1.5, 128.3), 32_000),
            ("EPSG:3067", (128.0, 100.6), 35_000),
        ] {
            let crs = crate::geo::projected_output_crs(epsg).unwrap();
            // The tile whose pixel (`pole`) is where the pole projects.
            let (e, n) = crs.forward(0.0, 90.0);
            let (west, north) = (e - pole.0 * texel, n + pole.1 * texel);
            let side = f64::from(size) * texel;
            let out = OutputCrs::Projected {
                crs,
                bbox: [west, north - side, west + side, north],
            };
            let node = |fx: f64, fy: f64| out.project_node([0.0; 4], fx, fy);
            let projections = Cell::new(0_usize);
            let to_src = |lon: f64, lat: f64| {
                projections.set(projections.get() + 1);
                (lon / 0.25, (90.0 - lat) / 0.25)
            };
            let grid = ProjectionGrid::build_2d_periodic(
                size,
                size,
                1440,
                721,
                Some(1440.0),
                node,
                to_src,
            );
            let label = format!("{epsg} pole at {pole:?}");
            assert!(
                projections.get() < ceiling,
                "{label}: {} projections",
                projections.get()
            );
            assert_eq!(grid.capped_error(), None, "{label}");
            let mut drawn = 0;
            for oy in 0..size {
                for ox in 0..size {
                    let (fx, fy) = (f64::from(ox) + 0.5, f64::from(oy) + 0.5);
                    let exact = node(fx / f64::from(size), fy / f64::from(size));
                    let (col, row) = grid.sample(ox, oy);
                    if !exact.0.is_finite() {
                        // Past EPSG:3067's domain, which ends on the line
                        // through its pole.
                        assert!(
                            !(col.is_finite() && row.is_finite()),
                            "{label}: ({ox}, {oy})"
                        );
                        continue;
                    }
                    if !(col.is_finite() && row.is_finite()) {
                        // Within a boundary sub-grid's step of that edge.
                        assert!(
                            fy - pole.1 < BOUNDARY_STEP_PX,
                            "{label}: ({ox}, {oy}) empty"
                        );
                        continue;
                    }
                    let (a, b) = (unit((col * 0.25, 90.0 - row * 0.25)), unit(exact));
                    let px =
                        ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2))
                            .sqrt()
                            * 6_371_000.0
                            / texel;
                    let limit = if (fx - pole.0).hypot(fy - pole.1) < 1.0 {
                        1.0
                    } else {
                        MAX_INTERP_ERROR_PX
                    };
                    assert!(px <= limit, "{label}: ({ox}, {oy}) {px:.3} px off");
                    drawn += 1;
                }
            }
            assert!(drawn > 20_000, "{label}: {drawn} pixels drawn");
        }
    }

    /// A geographic view a few cells across a turn of longitude holds no
    /// pole, though each cell's top and bottom turn more than a quarter
    /// turn: 4 × 4 cells over 400°. Its column runs linearly along them, so
    /// each cell is interpolated whole, exactly as the mapping is affine,
    /// with no split and no sub-grid (#1195).
    #[test]
    fn a_wide_geographic_view_holds_no_pole() {
        let to_src = |lon: f64, lat: f64| (lon / 0.25, (90.0 - lat) / 0.25);
        let world = |fx: f64, fy: f64| (-200.0 + 400.0 * fx, 90.0 - 180.0 * fy);
        let grid =
            ProjectionGrid::build_2d_periodic(100, 50, 1440, 721, Some(1440.0), world, to_src);
        assert_eq!((grid.cells_x, grid.cells_y), (4, 4));
        assert!(grid.local.is_empty() && grid.boundary.is_empty());
        for oy in 0..50 {
            for ox in 0..100 {
                let (c, r) = grid.sample(ox, oy);
                let (ec, er) = to_src(
                    -200.0 + 4.0 * (ox as f64 + 0.5),
                    90.0 - 3.6 * (oy as f64 + 0.5),
                );
                assert!(
                    (c - ec).abs() < 1e-9 && (r - er).abs() < 1e-9,
                    "({ox}, {oy})"
                );
            }
        }
    }

    /// #1195: a periodic grid splits only the cells near a pole, within a
    /// budget. A synthetic polar view, the pole at the image's centre and
    /// 80° of latitude to its corners, its rows jumping 37 on a
    /// checkerboard within 51 px of the pole, which no cell size
    /// interpolates across: the splits, worst first and the pole's before
    /// any, take the pole's cells down to a pixel, then follow the jumps
    /// until they have cost a quarter of the projections of a grid of
    /// `MAX_CELLS`, and the grid records its error and why it stopped.
    /// Cells far from the pole are not split. The same jump on a periodic
    /// grid without a pole is refined as a whole, as on any other grid, to
    /// `MAX_CELLS`.
    #[test]
    fn local_refinement_is_budgeted() {
        let size = 256_u32;
        let world = |fx: f64, fy: f64| {
            let (dx, dy) = (fx - 0.5, fy - 0.5);
            (dy.atan2(dx).to_degrees(), 90.0 - 80.0 * dx.hypot(dy))
        };
        let to_src = |lon: f64, lat: f64| {
            let r = (90.0 - lat) / 80.0;
            let (dx, dy) = (r * lon.to_radians().cos(), r * lon.to_radians().sin());
            let checker = r < 0.2 && ((dx * 97.0).floor() + (dy * 89.0).floor()) as i64 % 2 != 0;
            let jump = if checker { 37.0 } else { 0.0 };
            (lon / 0.25, (90.0 - lat) / 0.25 + jump)
        };
        let grid =
            ProjectionGrid::build_2d_periodic(size, size, 1440, 721, Some(1440.0), world, to_src);
        assert_eq!((grid.cells_x, grid.cells_y), (32, 32));
        let error = grid.capped_error().expect("the jumps cap the grid");
        assert!(error > MAX_INTERP_ERROR_PX, "capped at {error}");
        assert!(grid.capped.is_some_and(|(_, limits)| limits.splits));
        let splits = grid
            .quads
            .iter()
            .filter(|q| matches!(q, Quad::Split(_)))
            .count();
        let budget = (MAX_CELLS * MAX_CELLS) as usize / 4;
        assert!(
            splits * SPLIT_COST <= budget && (splits + 1) * SPLIT_COST > budget,
            "{splits} splits"
        );
        // The pole is on the node at the centre: the cells that bracket it
        // were split first, from 8 px down to a pixel, before the jumps
        // spent the budget.
        let around = [(127, 127), (128, 127), (127, 128), (128, 128)];
        let depths = around.map(|(ox, oy)| grid.local_depth(ox, oy));
        assert!(depths.contains(&3), "{depths:?}");
        for (ox, oy) in [(0, 0), (255, 0), (0, 255), (255, 255), (128, 0), (0, 128)] {
            assert_eq!(grid.local_depth(ox, oy), 0, "({ox}, {oy})");
        }

        let jump = |x: f64, y: f64| {
            let jump = if x < 0.6 { 0.0 } else { 37.0 };
            (x * 100.0, y * 100.0 + jump)
        };
        let grid = ProjectionGrid::build_2d_periodic(
            size,
            size,
            100,
            100,
            Some(1000.0),
            |x, y| (x, y),
            jump,
        );
        let cap = MAX_CELLS as usize;
        assert_eq!((grid.cells_x, grid.cells_y), (cap, cap));
        assert!(grid.capped_error().is_some());
        assert!(grid.quads.is_empty() && grid.local.is_empty());
    }

    /// A domain whose edge has a corner reaches into a cell between its
    /// corner nodes: here a wedge 100° wide below its tip, the shape of
    /// EPSG:3067's area at the North Pole under the meridians 50° either
    /// side of 27°E that bounded it (#1113). In a 256 px tile, a tip 6 px
    /// above a 32 px cell's bottom side lies 16 px from that side's
    /// corners, and misses all four. Once with the cell below in the tile,
    /// whose sub-grid reaches the tip along their shared side, and once with
    /// the cell's bottom side the tile's own, where no node is in the domain
    /// and only the side's probes find the tip. The map is the identity, so
    /// the grid stays at its 32 px cells. Every pixel whose 2 px
    /// surroundings, a sub-grid step, lie in the domain is drawn where it
    /// lies, and no pixel past the domain.
    #[test]
    fn the_tip_of_a_domain_between_cell_corners_is_drawn() {
        let size = 256_u32;
        let half_angle = 50.0_f64.to_radians().tan();
        for (col, row) in [(80.0, 26.0), (80.0, 250.0)] {
            let place = |x: f64, y: f64| y >= row && (x - col).abs() <= (y - row) * half_angle;
            let grid = ProjectionGrid::build_2d(
                size,
                size,
                size,
                size,
                |fx, fy| {
                    let (x, y) = (fx * f64::from(size), fy * f64::from(size));
                    if place(x, y) {
                        (x, y)
                    } else {
                        (f64::NAN, f64::NAN)
                    }
                },
                |x, y| (x, y),
            );
            assert_eq!(
                (grid.cells_x, grid.cells_y),
                (8, 8),
                "tip at ({col}, {row})"
            );
            // The tip's cell, from 64 px across and its row down.
            let (cx, cy) = (2, (row / 32.0) as usize);
            let n = cy * grid.stride + cx;
            let corners = [n, n + 1, n + grid.stride, n + grid.stride + 1];
            assert!(
                corners.iter().all(|&k| !grid.nodes[k].0.is_finite()),
                "tip at ({col}, {row}): a corner of its cell is in the domain"
            );
            let mut drawn = 0;
            for oy in 0..size {
                for ox in 0..size {
                    let (x, y) = (f64::from(ox) + 0.5, f64::from(oy) + 0.5);
                    let (c, r) = grid.sample(ox, oy);
                    if !place(x, y) {
                        assert!(c.is_nan() || r.is_nan(), "({ox}, {oy}) past the domain");
                    } else if [(-2.0, -2.0), (2.0, -2.0), (-2.0, 2.0), (2.0, 2.0)]
                        .iter()
                        .all(|&(dx, dy)| place(x + dx, y + dy))
                    {
                        assert!(
                            (c - x).abs() < 0.5 && (r - y).abs() < 0.5,
                            "tip at ({col}, {row}): ({ox}, {oy}) drawn at ({c}, {r})"
                        );
                        drawn += usize::from(oy < 32 * (cy as u32 + 1));
                    }
                }
            }
            // Inside the tip's own cell row, 4 px below the tip.
            let (c, r) = grid.sample(80, row as u32 + 4);
            assert!(
                c.is_finite() && r.is_finite(),
                "tip at ({col}, {row}): the tip is empty"
            );
            assert!(drawn > 0, "tip at ({col}, {row})");
        }
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
