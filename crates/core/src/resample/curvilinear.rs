//! Curvilinear source grids (#1174): a latitude and longitude for every
//! cell centre and no CRS, as ocean, sea-ice and regional products carry
//! them in 2-D `latitude(y, x)` and `longitude(y, x)` auxiliaries.
//!
//! No formula takes a point to its cell, so a point is placed by an
//! inverse lookup over the nodes (the cell centres), made only where a
//! projection would be: at a map's coarse [`ProjectionGrid`] nodes, and
//! once per EDR point (Critical Rule 5). [`CurvilinearGrid::locate`]:
//!
//! 1. **Nearest node,** from a k-d tree over the nodes' unit vectors, built
//!    once per grid ([`CurvilinearGrid::new`]). Chord distance grows with
//!    great-circle distance, so the antimeridian and the poles need no
//!    special case.
//! 2. **Cell inversion:** the corners of a cell around that node are
//!    projected gnomonically onto the plane tangent to the Earth at the
//!    point, where great circles are straight lines, and the cell's
//!    bilinear map is inverted for the point (Newton's method). The result
//!    is fractional, so positions between two map nodes interpolate
//!    smoothly. A point outside the cell steps to the neighbouring cell it
//!    lies towards, at most [`MAX_WALK`] cells.
//! 3. **Footprint:** a point is off the grid (NaN) when its nearest node
//!    lies farther than [`REACH`] times that node's spacing to its
//!    neighbours, or when the inversion at the edge cell it walked to
//!    lands more than [`EDGE_MARGIN`] past the outer nodes. Within that
//!    margin the edge cell's bilinear map extends past the edge, so the
//!    shared sampler can draw the half cell past the outer cell edges every
//!    grid draws, and cut it per pixel; never an edge cell everywhere.
//!
//! Positions are measured as [`super::SourceGrid::position`] measures
//! them: from the first node, whole numbers on the nodes, `(column, row)`
//! along the second and first dimension, rows as stored. A node without a
//! valid latitude and longitude (a fill value) leaves the cells touching it
//! undrawn.
//!
//! Memory: 12 bytes per node for its unit vector (`f32`, ~0.4 m on the
//! Earth) and 5 for the tree, 17 MB per million nodes, held as long as the
//! grid. The 2-D arrays themselves are not kept.
//!
//! [`ProjectionGrid`]: super::ProjectionGrid

use std::sync::Arc;

mod kdtree;

use kdtree::{dist2, vector, KdTree};

/// The most nodes a grid may have: 16.8 million (4096²), 285 MB of
/// vectors and tree.
pub const MAX_NODES: usize = 1 << 24;

/// How far a point's nearest node may lie, in that node's spacing to its
/// neighbours, before the point is off the grid. A point [`EDGE_MARGIN`]
/// past a corner node is 2√2 spacings from it.
pub const REACH: f64 = 3.0;

/// How far past the outer nodes, in nodes, a position extends the edge
/// cells' bilinear map; farther is off the grid. The shared sampler draws
/// up to 1.0 past them (the half cell past the outer cell edges); the rest
/// keeps a map's coarse grid finite, and so interpolating, across that cut.
pub const EDGE_MARGIN: f64 = 2.0;

/// The most cells a lookup walks from its nearest node's cell.
pub const MAX_WALK: usize = 8;

/// Newton iterations of one cell inversion.
const NEWTON_STEPS: usize = 24;

/// How far outside `[0, 1]` a cell's inversion may land and still be in
/// the cell: a point on the edge two cells share lands on either side by
/// a rounding error.
const IN_CELL: f64 = 1e-9;

/// Corners farther than this from the point (cosine of 60°) are not
/// projected: the gnomonic projection runs to infinity at 90°.
const MIN_COS: f64 = 0.5;

/// The least tolerance [`CurvilinearNodes::same`] allows a node, in
/// radians of chord: about 6 m, past the `f32` vectors' rounding.
const SAME_FLOOR: f64 = 1e-6;

/// A curvilinear grid's nodes: each cell centre's unit vector, from its
/// latitude and longitude. Cheap to build and compare (O(n)): a file's grid
/// identity. [`CurvilinearGrid`] indexes them.
pub struct CurvilinearNodes {
    nx: usize,
    ny: usize,
    /// Each node's unit vector, row-major as stored; NaN for a node
    /// without a valid latitude and longitude.
    xyz: Vec<[f32; 3]>,
}

impl std::fmt::Debug for CurvilinearNodes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CurvilinearNodes")
            .field("nx", &self.nx)
            .field("ny", &self.ny)
            .finish_non_exhaustive()
    }
}

impl CurvilinearNodes {
    /// The nodes of an `nx` × `ny` grid from its row-major `lon` and `lat`
    /// in degrees, row `j` column `i` at `j * nx + i`. A node whose
    /// longitude or latitude is not finite, or whose latitude lies past
    /// ±90°, has no position (a fill value). An `Err` names why the arrays
    /// make no grid: fewer than 2 × 2 nodes, more than [`MAX_NODES`],
    /// lengths other than `nx * ny`, or no cell whose four corners all have
    /// a position.
    pub fn new(nx: usize, ny: usize, lon: &[f64], lat: &[f64]) -> Result<Self, String> {
        let n = Self::check_size(nx, ny)?;
        if lon.len() != n || lat.len() != n {
            return Err(format!(
                "{} longitudes and {} latitudes for {nx} x {ny} nodes",
                lon.len(),
                lat.len()
            ));
        }
        let xyz: Vec<[f32; 3]> = lon
            .iter()
            .zip(lat)
            .map(|(&lon, &lat)| {
                if lon.is_finite() && lat.is_finite() && lat.abs() <= 90.0 {
                    unit(lon, lat).map(|c| c as f32)
                } else {
                    [f32::NAN; 3]
                }
            })
            .collect();
        let nodes = Self { nx, ny, xyz };
        let any_cell = (0..ny - 1).any(|j| (0..nx - 1).any(|i| nodes.cell(i, j).is_some()));
        if !any_cell {
            return Err("no cell has a valid latitude and longitude at all four corners".into());
        }
        Ok(nodes)
    }

    /// The node count of an `nx` × `ny` grid, or why it makes no grid:
    /// fewer than 2 × 2 nodes, or more than [`MAX_NODES`]. A reader checks
    /// it from the dimensions before reading an array, so an oversized grid
    /// is refused unread.
    pub fn check_size(nx: usize, ny: usize) -> Result<usize, String> {
        if nx < 2 || ny < 2 {
            return Err(format!("a {nx} x {ny} grid has no cell"));
        }
        nx.checked_mul(ny)
            .filter(|&n| n <= MAX_NODES)
            .ok_or_else(|| format!("{nx} x {ny} nodes are more than {MAX_NODES}"))
    }

    /// `(columns, rows)`: `(nx, ny)`.
    pub fn size(&self) -> (usize, usize) {
        (self.nx, self.ny)
    }

    /// Whether `other` places every node where this places it: the same
    /// size, the same nodes without a position, and every other node within
    /// a thousandth of its spacing to its neighbours (at least
    /// [`SAME_FLOOR`]), as a regular grid is the same to a thousandth of a
    /// cell.
    pub fn same(&self, other: &Self) -> bool {
        if (self.nx, self.ny) != (other.nx, other.ny) {
            return false;
        }
        (0..self.ny).all(|j| {
            (0..self.nx).all(|i| match (self.at(i, j), other.at(i, j)) {
                (None, None) => true,
                (Some(a), Some(b)) => {
                    let tolerance = (1e-3 * self.spacing(i, j)).max(SAME_FLOOR);
                    dist2(a, b) <= tolerance * tolerance
                }
                _ => false,
            })
        })
    }

    /// Node `(i, j)`'s unit vector, `None` without a position or off the
    /// grid.
    #[inline]
    fn at(&self, i: usize, j: usize) -> Option<[f64; 3]> {
        if i >= self.nx || j >= self.ny {
            return None;
        }
        let v = self.xyz[j * self.nx + i];
        v[0].is_finite().then(|| vector(v))
    }

    /// The corners `[(i, j), (i + 1, j), (i, j + 1), (i + 1, j + 1)]` of
    /// the cell whose first corner is node `(i, j)`, when all four have a
    /// position.
    #[inline]
    fn cell(&self, i: usize, j: usize) -> Option<[[f64; 3]; 4]> {
        Some([
            self.at(i, j)?,
            self.at(i + 1, j)?,
            self.at(i, j + 1)?,
            self.at(i + 1, j + 1)?,
        ])
    }

    /// The chord distance from node `(i, j)` to its farthest neighbour
    /// along a row or column that has a position; 0 for none.
    fn spacing(&self, i: usize, j: usize) -> f64 {
        let Some(p) = self.at(i, j) else {
            return 0.0;
        };
        let neighbours = [
            i.checked_sub(1).map(|i| (i, j)),
            Some((i + 1, j)),
            j.checked_sub(1).map(|j| (i, j)),
            Some((i, j + 1)),
        ];
        neighbours
            .into_iter()
            .flatten()
            .filter_map(|(i, j)| self.at(i, j))
            .map(|q| dist2(p, q).sqrt())
            .fold(0.0, f64::max)
    }
}

/// A curvilinear grid indexed for lookups ([`Self::locate`]): its nodes, a
/// k-d tree over them, and its extent. Built once per grid and kept; every
/// lookup only reads it.
pub struct CurvilinearGrid {
    nodes: Arc<CurvilinearNodes>,
    tree: KdTree,
    /// [`REACH`] times the largest node spacing, squared: the tree's search
    /// radius, so a point far off the grid ends its search early.
    search_d2: f64,
    extent: Option<[f64; 4]>,
}

impl std::fmt::Debug for CurvilinearGrid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CurvilinearGrid")
            .field("nodes", &self.nodes)
            .field("extent", &self.extent)
            .finish_non_exhaustive()
    }
}

impl CurvilinearGrid {
    /// Index `nodes`: the k-d tree in O(n log n), and the extent.
    pub fn new(nodes: Arc<CurvilinearNodes>) -> Self {
        let tree = KdTree::new(&nodes.xyz);
        let largest = (0..nodes.ny)
            .flat_map(|j| (0..nodes.nx).map(move |i| (i, j)))
            .map(|(i, j)| nodes.spacing(i, j))
            .fold(0.0, f64::max);
        let mut grid = Self {
            nodes,
            tree,
            search_d2: (REACH * largest).powi(2),
            extent: None,
        };
        grid.extent = grid.compute_extent();
        grid
    }

    /// The nodes indexed.
    pub fn nodes(&self) -> &Arc<CurvilinearNodes> {
        &self.nodes
    }

    /// `(columns, rows)`: `(nx, ny)`.
    pub fn size(&self) -> (usize, usize) {
        self.nodes.size()
    }

    /// The grid's WGS84 extent in the CRS84 domain, through
    /// [`crate::geo::ring_extent`] and [`crate::geo::crs84_extent`]: its
    /// outer cell edges, half a node spacing past the outer nodes, walked
    /// as one ring. `west > east` across the antimeridian, and reaching
    /// every pole the grid holds at every longitude (−180…180). Computed
    /// once, by [`Self::new`]; `None` when no edge node has a position.
    pub fn extent(&self) -> Option<[f64; 4]> {
        self.extent
    }

    /// The extent as a map's footprint guard walks it, west to east:
    /// `west <= east`, with `east` past 180° when the grid crosses the
    /// antimeridian (as [`crate::geo::GeoTransform::footprint_envelope`]).
    pub fn footprint_envelope(&self) -> Option<[f64; 4]> {
        let [w, s, mut e, n] = self.extent?;
        if w > e {
            e += 360.0;
        }
        Some([w, s, e, n])
    }

    /// The fractional `(column, row)` of WGS84 `(lon, lat)` on the grid,
    /// whole numbers on the nodes (see the module docs), or NaN off its
    /// footprint. Any longitude, wrapped or not: `190` and `-170` are the
    /// same point.
    pub fn locate(&self, lon: f64, lat: f64) -> (f64, f64) {
        self.position(lon, lat).unwrap_or((f64::NAN, f64::NAN))
    }

    /// [`Self::locate`], `None` off the footprint.
    fn position(&self, lon: f64, lat: f64) -> Option<(f64, f64)> {
        if !lon.is_finite() || !lat.is_finite() || lat.abs() > 90.0 {
            return None;
        }
        let nodes = &*self.nodes;
        let (nx, ny) = (nodes.nx, nodes.ny);
        let frame = Frame::at(lon, lat);
        let (k, d2) = self.tree.nearest(&nodes.xyz, frame.p, self.search_d2)?;
        let (i, j) = (k % nx, k / nx);
        let reach = REACH * nodes.spacing(i, j);
        if d2 > reach * reach {
            return None;
        }
        let (mut ci, mut cj) = self.start_cell(i, j, frame.p)?;
        for _ in 0..MAX_WALK {
            let (s, t) = frame.invert(nodes.cell(ci, cj)?)?;
            let step = |f: f64| {
                if f < -IN_CELL {
                    -1
                } else if f > 1.0 + IN_CELL {
                    1
                } else {
                    0
                }
            };
            let next = |c: usize, f: f64, n: usize| (c as isize + step(f)).clamp(0, n as isize - 2);
            let (ni, nj) = (next(ci, s, nx) as usize, next(cj, t, ny) as usize);
            if (ni, nj) == (ci, cj) {
                // In this cell, or past the grid's edge beside it.
                let (col, row) = (ci as f64 + s, cj as f64 + t);
                let within =
                    |f: f64, n: usize| (-EDGE_MARGIN..=(n - 1) as f64 + EDGE_MARGIN).contains(&f);
                return (within(col, nx) && within(row, ny)).then_some((col, row));
            }
            (ci, cj) = (ni, nj);
        }
        None
    }

    /// The cell a lookup from node `(i, j)` starts in, by its first corner:
    /// of the up to four cells around the node, the one on the point's
    /// side of it along each axis, else another with all four corners, so
    /// a point beside a node without a position still finds its own cell.
    fn start_cell(&self, i: usize, j: usize, p: [f64; 3]) -> Option<(usize, usize)> {
        let nodes = &*self.nodes;
        let (nx, ny) = (nodes.nx, nodes.ny);
        let n = nodes.at(i, j)?;
        let offset = [p[0] - n[0], p[1] - n[1], p[2] - n[2]];
        // The grid's axis through the node: from its neighbour before to
        // its neighbour after, or from or to the node at an edge or a gap.
        let axis = |before: Option<[f64; 3]>, after: Option<[f64; 3]>| {
            let (from, to) = (before.unwrap_or(n), after.unwrap_or(n));
            [to[0] - from[0], to[1] - from[1], to[2] - from[2]]
        };
        let along_row = axis(
            i.checked_sub(1).and_then(|i| nodes.at(i, j)),
            nodes.at(i + 1, j),
        );
        let along_col = axis(
            j.checked_sub(1).and_then(|j| nodes.at(i, j)),
            nodes.at(i, j + 1),
        );
        // The first corner of the cell before the node along an axis, or
        // of the one after it.
        let first = |before: bool, c: usize, n: usize| {
            if before { c.saturating_sub(1) } else { c }.min(n - 2)
        };
        let before_i = dot(offset, along_row) < 0.0;
        let before_j = dot(offset, along_col) < 0.0;
        [
            (before_i, before_j),
            (!before_i, before_j),
            (before_i, !before_j),
            (!before_i, !before_j),
        ]
        .into_iter()
        .map(|(bi, bj)| (first(bi, i, nx), first(bj, j, ny)))
        .find(|&(ci, cj)| nodes.cell(ci, cj).is_some())
    }

    /// The extent of [`Self::extent`]: each outer node moved half a node
    /// spacing outwards (away from its inner neighbour, both ways at a
    /// corner), walked as one ring, and the poles the grid holds: a pole
    /// is in the grid when it locates inside its outer cell edges.
    fn compute_extent(&self) -> Option<[f64; 4]> {
        let nodes = &*self.nodes;
        let (nx, ny) = (nodes.nx, nodes.ny);
        let ring = (0..nx)
            .map(|i| (i, 0))
            .chain((1..ny).map(|j| (nx - 1, j)))
            .chain((0..nx - 1).rev().map(|i| (i, ny - 1)))
            .chain((1..ny - 1).rev().map(|j| (0, j)));
        let edge = |(i, j): (usize, usize)| -> Option<(f64, f64)> {
            let p = nodes.at(i, j)?;
            let mut q = p;
            let mut outwards = |inner: Option<[f64; 3]>| {
                if let Some(inner) = inner {
                    for a in 0..3 {
                        q[a] += 0.5 * (p[a] - inner[a]);
                    }
                }
            };
            if i == 0 {
                outwards(nodes.at(1, j));
            }
            if i == nx - 1 {
                outwards(nodes.at(nx - 2, j));
            }
            if j == 0 {
                outwards(nodes.at(i, 1));
            }
            if j == ny - 1 {
                outwards(nodes.at(i, ny - 2));
            }
            Some(lon_lat(q))
        };
        let inside = |(col, row): (f64, f64)| {
            (-0.5..=nx as f64 - 0.5).contains(&col) && (-0.5..=ny as f64 - 0.5).contains(&row)
        };
        let poles = [90.0, -90.0].map(|pole| self.position(0.0, pole).is_some_and(inside));
        crate::geo::crs84_extent(crate::geo::ring_extent(ring.filter_map(edge), poles))
    }
}

/// The unit vector of WGS84 `(lon, lat)` in degrees.
#[inline]
fn unit(lon: f64, lat: f64) -> [f64; 3] {
    let (lon, lat) = (lon.to_radians(), lat.to_radians());
    [lat.cos() * lon.cos(), lat.cos() * lon.sin(), lat.sin()]
}

/// `(lon, lat)` in degrees of a vector of any length.
#[inline]
fn lon_lat(v: [f64; 3]) -> (f64, f64) {
    let [x, y, z] = v;
    (y.atan2(x).to_degrees(), z.atan2(x.hypot(y)).to_degrees())
}

#[inline]
fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// The plane tangent to the Earth at a point: its unit vector and the
/// unit vectors east and north of it there (east from the longitude, so
/// at a pole too).
struct Frame {
    p: [f64; 3],
    east: [f64; 3],
    north: [f64; 3],
}

impl Frame {
    fn at(lon: f64, lat: f64) -> Self {
        let (lon, lat) = (lon.to_radians(), lat.to_radians());
        let (sin_lon, cos_lon) = lon.sin_cos();
        let (sin_lat, cos_lat) = lat.sin_cos();
        Self {
            p: [cos_lat * cos_lon, cos_lat * sin_lon, sin_lat],
            east: [-sin_lon, cos_lon, 0.0],
            north: [-sin_lat * cos_lon, -sin_lat * sin_lon, cos_lat],
        }
    }

    /// `q`'s gnomonic projection onto the plane: east and north, in units
    /// of the Earth's radius. `None` 60° or more away.
    #[inline]
    fn project(&self, q: [f64; 3]) -> Option<[f64; 2]> {
        let d = dot(q, self.p);
        (d >= MIN_COS).then(|| [dot(q, self.east) / d, dot(q, self.north) / d])
    }

    /// Where the point lies in the cell with `corners` (as
    /// [`CurvilinearNodes::cell`] lists them): the `(s, t)` its bilinear
    /// map takes to the point, `(0, 0)` at the first corner, `s` along the
    /// row and `t` down the column; outside `[0, 1]²` for a point outside
    /// the cell. `None` where the corners do not project, or Newton's
    /// method finds no solution (a degenerate cell).
    fn invert(&self, corners: [[f64; 3]; 4]) -> Option<(f64, f64)> {
        let [a, b, c, d] = [
            self.project(corners[0])?,
            self.project(corners[1])?,
            self.project(corners[2])?,
            self.project(corners[3])?,
        ];
        // F(s, t) = a + s·e1 + t·e2 + s·t·e3, solved for F = 0 (the
        // point is the plane's origin).
        let e1 = [b[0] - a[0], b[1] - a[1]];
        let e2 = [c[0] - a[0], c[1] - a[1]];
        let e3 = [a[0] - b[0] - c[0] + d[0], a[1] - b[1] - c[1] + d[1]];
        let size = (e1[0].hypot(e1[1])).max(e2[0].hypot(e2[1]));
        if size.is_nan() || size <= 0.0 {
            return None;
        }
        let f = |s: f64, t: f64| {
            [
                a[0] + s * e1[0] + t * e2[0] + s * t * e3[0],
                a[1] + s * e1[1] + t * e2[1] + s * t * e3[1],
            ]
        };
        let (mut s, mut t) = (0.5, 0.5);
        for _ in 0..NEWTON_STEPS {
            let r = f(s, t);
            // Columns of the Jacobian: dF/ds and dF/dt.
            let js = [e1[0] + t * e3[0], e1[1] + t * e3[1]];
            let jt = [e2[0] + s * e3[0], e2[1] + s * e3[1]];
            let det = js[0] * jt[1] - js[1] * jt[0];
            if det == 0.0 || !det.is_finite() {
                return None;
            }
            let ds = (r[0] * jt[1] - r[1] * jt[0]) / det;
            let dt = (js[0] * r[1] - js[1] * r[0]) / det;
            s -= ds;
            t -= dt;
            if !(s.is_finite() && t.is_finite()) {
                return None;
            }
            if ds.abs() + dt.abs() < 1e-13 {
                break;
            }
        }
        let r = f(s, t);
        (r[0].hypot(r[1]) <= 1e-9 * size).then_some((s, t))
    }
}

#[cfg(test)]
mod tests;
