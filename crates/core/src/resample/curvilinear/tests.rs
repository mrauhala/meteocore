//! [`CurvilinearGrid`] over grids whose exact inverse is known: a polar
//! stereographic grid on a sphere, given to it only as each node's
//! latitude and longitude, around the North Pole and across the
//! antimeridian. A lookup is checked against the projection's own
//! `(column, row)` of the point.

use std::sync::Arc;

use super::*;
use crate::map_engine::OutputCrs;
use crate::resample::SourceGrid;

const R: f64 = 6_371_000.0;

/// A north polar stereographic projection on a sphere, true scale at 70°N,
/// `lon0` the meridian along its negative y axis.
struct Polar {
    lon0: f64,
    k: f64,
}

impl Polar {
    fn new(lon0: f64) -> Self {
        Self {
            lon0,
            k: (1.0 + 70.0_f64.to_radians().sin()) / 2.0,
        }
    }

    fn forward(&self, lon: f64, lat: f64) -> (f64, f64) {
        let rho = 2.0 * R * self.k * (std::f64::consts::FRAC_PI_4 - lat.to_radians() / 2.0).tan();
        let dl = (lon - self.lon0).to_radians();
        (rho * dl.sin(), -rho * dl.cos())
    }

    fn inverse(&self, x: f64, y: f64) -> (f64, f64) {
        let rho = x.hypot(y);
        let lat = 90.0 - 2.0 * (rho / (2.0 * R * self.k)).atan().to_degrees();
        let lon = self.lon0 + x.atan2(-y).to_degrees();
        let lon = (lon + 180.0).rem_euclid(360.0) - 180.0;
        (lon, lat)
    }
}

/// A regular grid of `nx` × `ny` nodes in a polar stereographic plane, the
/// first at `(x0, y0)`, `step` metres apart, rows running to -y.
struct Plane {
    polar: Polar,
    x0: f64,
    y0: f64,
    step: f64,
    nx: usize,
    ny: usize,
}

impl Plane {
    /// 19 × 28 nodes 100 km apart around the North Pole, Svalbard on the
    /// negative y axis (`lon0` 15°E): the antimeridian runs from the pole
    /// towards +y, across row 0 at about 79.5°N. The
    /// testdata/netcdf-curvilinear fixture's grid.
    fn svalbard() -> Self {
        Self {
            polar: Polar::new(15.0),
            x0: -900_000.0,
            y0: 1_100_000.0,
            step: 100_000.0,
            nx: 19,
            ny: 28,
        }
    }

    fn nodes(&self) -> CurvilinearNodes {
        let (mut lon, mut lat) = (Vec::new(), Vec::new());
        for j in 0..self.ny {
            for i in 0..self.nx {
                let (x, y) = self.xy(i as f64, j as f64);
                let (a, b) = self.polar.inverse(x, y);
                lon.push(a);
                lat.push(b);
            }
        }
        CurvilinearNodes::new(self.nx, self.ny, &lon, &lat).unwrap()
    }

    fn grid(&self) -> CurvilinearGrid {
        CurvilinearGrid::new(Arc::new(self.nodes()))
    }

    fn xy(&self, col: f64, row: f64) -> (f64, f64) {
        (self.x0 + col * self.step, self.y0 - row * self.step)
    }

    /// The exact `(column, row)` of `(lon, lat)`.
    fn position(&self, lon: f64, lat: f64) -> (f64, f64) {
        let (x, y) = self.polar.forward(lon, lat);
        ((x - self.x0) / self.step, (self.y0 - y) / self.step)
    }

    /// `(lon, lat)` of the fractional position `(col, row)`.
    fn lon_lat(&self, col: f64, row: f64) -> (f64, f64) {
        let (x, y) = self.xy(col, row);
        self.polar.inverse(x, y)
    }
}

/// A deterministic sequence in `[0, 1)`.
fn uniform() -> impl FnMut() -> f64 {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Every point inside the grid locates where the projection puts it: the
/// cell inversion is exact to second order in the cell, here within a
/// thousandth of a cell, around the pole and across the antimeridian alike.
#[test]
fn a_lookup_lands_where_the_projection_puts_the_point() {
    let plane = Plane::svalbard();
    let grid = plane.grid();
    let mut next = uniform();
    let mut worst = 0.0_f64;
    for _ in 0..4000 {
        let col = next() * (plane.nx - 1) as f64;
        let row = next() * (plane.ny - 1) as f64;
        let (lon, lat) = plane.lon_lat(col, row);
        let (c, r) = grid.locate(lon, lat);
        worst = worst.max((c - col).abs()).max((r - row).abs());
    }
    assert!(worst < 1e-3, "worst {worst} cells");
    // Nodes land on themselves.
    for (i, j) in [(0, 0), (18, 27), (9, 7), (3, 20)] {
        let (lon, lat) = plane.lon_lat(i as f64, j as f64);
        let (c, r) = grid.locate(lon, lat);
        assert!(
            (c - i as f64).abs() < 1e-5 && (r - j as f64).abs() < 1e-5,
            "node ({i}, {j}) at ({c}, {r})"
        );
    }
}

/// The antimeridian and the pole are ordinary places: a point is the same
/// point at 179.99° and -180.01°, and at 190°; the pole is one point from
/// every meridian.
#[test]
fn the_antimeridian_and_the_pole_need_no_special_case() {
    let plane = Plane::svalbard();
    let grid = plane.grid();
    let close =
        |a: (f64, f64), b: (f64, f64), d: f64| (a.0 - b.0).abs() < d && (a.1 - b.1).abs() < d;
    for lat in [81.0, 85.0, 88.0] {
        let (a, b) = (grid.locate(179.999, lat), grid.locate(-179.999, lat));
        assert!(a.0.is_finite() && b.0.is_finite(), "{lat}: {a:?} {b:?}");
        assert!(close(a, b, 0.01), "{a:?} {b:?}");
        let found = grid.locate(-170.0, lat);
        assert!(close(found, grid.locate(190.0, lat), 1e-9), "{lat}");
        assert!(
            close(found, plane.position(-170.0, lat), 1e-3),
            "{lat}: {found:?}"
        );
        assert!(found.1 > 0.0, "{lat}: {found:?} not inside");
    }
    let pole = plane.position(0.0, 90.0);
    for lon in [-180.0, -45.0, 0.0, 15.0, 123.0, 180.0] {
        let (c, r) = grid.locate(lon, 90.0);
        assert!(
            (c - pole.0).abs() < 1e-6 && (r - pole.1).abs() < 1e-6,
            "pole from {lon}: ({c}, {r}), exact {pole:?}"
        );
    }
}

/// Off the footprint is NaN, never the edge cell: a point half a node past
/// the outer nodes is still on the grid (the sampler draws it from the
/// edge cell), one [`EDGE_MARGIN`] past them is the last extrapolated, and
/// farther, or across the Earth, is off it.
#[test]
fn a_point_off_the_footprint_is_nan() {
    let plane = Plane::svalbard();
    let grid = plane.grid();
    for (lon, lat) in [(15.0, 60.0), (-160.0, 50.0), (100.0, -80.0), (15.0, 70.0)] {
        let (c, r) = grid.locate(lon, lat);
        assert!(c.is_nan() && r.is_nan(), "({lon}, {lat}) at ({c}, {r})");
    }
    // Past the first row (-y edge is the last row; +y is row 0), the
    // middle column.
    for (past, on) in [(0.5, true), (1.9, true), (2.2, false), (3.5, false)] {
        let (lon, lat) = plane.lon_lat(9.0, -past);
        let (c, r) = grid.locate(lon, lat);
        assert_eq!(c.is_finite(), on, "{past} past row 0: ({c}, {r})");
        if on {
            // The edge cell's map, extended: second order in the distance.
            let tolerance = 0.01 * past * past;
            assert!(
                (r + past).abs() < tolerance && (c - 9.0).abs() < tolerance,
                "({c}, {r})"
            );
        }
        // Past the last column, by a corner too.
        let (lon, lat) = plane.lon_lat(18.0 + past, 27.0 + past);
        let (c, _) = grid.locate(lon, lat);
        assert_eq!(c.is_finite(), on, "{past} past the last corner");
    }
    // The sampler draws the half cell past the outer cell edges, no more.
    let source = SourceGrid::from_curvilinear(Arc::new(grid));
    let covers = |col: f64, row: f64| {
        let (lon, lat) = plane.lon_lat(col, row);
        source.covers(source.position(lon, lat))
    };
    assert!(covers(9.0, -0.9) && covers(-0.9, 5.0) && covers(18.9, 27.9));
    assert!(!covers(9.0, -1.1) && !covers(-1.1, 5.0) && !covers(19.1, 5.0));
}

/// The extent holds the North Pole, at every longitude, and reaches south
/// to the outer cell edges' southernmost point: the corners half a node
/// past (0, 27) and (18, 27).
#[test]
fn the_extent_reaches_the_pole_and_the_outer_cell_edges() {
    let plane = Plane::svalbard();
    let grid = plane.grid();
    let [w, s, e, n] = grid.extent().unwrap();
    assert_eq!((w, e, n), (-180.0, 180.0, 90.0));
    let south = (0..=400)
        .flat_map(|k| {
            let f = k as f64 / 400.0;
            let (cols, rows) = ((plane.nx - 1) as f64 + 1.0, (plane.ny - 1) as f64 + 1.0);
            [
                (-0.5 + f * cols, -0.5),
                (-0.5 + f * cols, rows - 0.5),
                (-0.5, -0.5 + f * rows),
                (cols - 0.5, -0.5 + f * rows),
            ]
        })
        .map(|(c, r)| plane.lon_lat(c, r).1)
        .fold(90.0, f64::min);
    assert!((s - south).abs() < 0.02, "south {s}, edges {south}");
    assert_eq!(grid.footprint_envelope(), Some([-180.0, s, 180.0, 90.0]));
}

/// A regional grid across the antimeridian, away from the pole: its extent
/// is `west > east`, and its footprint envelope runs past 180°.
#[test]
fn a_grid_across_the_antimeridian_has_a_west_greater_than_east_extent() {
    let plane = Plane {
        polar: Polar::new(180.0),
        x0: -400_000.0,
        y0: -2_000_000.0,
        step: 50_000.0,
        nx: 17,
        ny: 9,
    };
    let grid = plane.grid();
    let [w, s, e, n] = grid.extent().unwrap();
    assert!(w > e, "{w} {e}");
    assert!(
        (160.0..175.0).contains(&w) && (-175.0..-160.0).contains(&e),
        "{w} {e}"
    );
    assert!(s < n && n < 80.0, "{s} {n}");
    let [w2, _, e2, _] = grid.footprint_envelope().unwrap();
    assert_eq!((w2, e2), (w, e + 360.0));
}

/// Two node sets are the same grid to a thousandth of a node's spacing;
/// a node moved by a hundredth, or one without a position, is another
/// grid.
#[test]
fn the_same_grid_compares_node_by_node() {
    let plane = Plane::svalbard();
    let a = plane.nodes();
    assert!(a.same(&plane.nodes()));
    let lon_lat = |nudge: Option<(usize, f64)>, missing: Option<usize>| {
        let (mut lon, mut lat) = (Vec::new(), Vec::new());
        for j in 0..plane.ny {
            for i in 0..plane.nx {
                let k = j * plane.nx + i;
                let shift = nudge.filter(|&(at, _)| at == k).map_or(0.0, |(_, d)| d);
                let (x, y) = plane.lon_lat(i as f64 + shift, j as f64);
                lon.push(if missing == Some(k) { f64::NAN } else { x });
                lat.push(y);
            }
        }
        CurvilinearNodes::new(plane.nx, plane.ny, &lon, &lat).unwrap()
    };
    assert!(a.same(&lon_lat(Some((200, 1e-5)), None)));
    assert!(!a.same(&lon_lat(Some((200, 1e-2)), None)));
    assert!(!a.same(&lon_lat(None, Some(5))));
    assert!(!lon_lat(None, Some(5)).same(&a));
}

/// A node without a position (a fill value) leaves the cells touching it
/// undrawn and every other cell as it was.
#[test]
fn a_node_without_a_position_leaves_its_cells_undrawn() {
    let plane = Plane::svalbard();
    let (mut lon, mut lat) = (Vec::new(), Vec::new());
    for j in 0..plane.ny {
        for i in 0..plane.nx {
            let (x, y) = plane.lon_lat(i as f64, j as f64);
            lon.push(x);
            lat.push(if (i, j) == (5, 5) { 1e36 } else { y });
        }
    }
    let grid = CurvilinearGrid::new(Arc::new(
        CurvilinearNodes::new(plane.nx, plane.ny, &lon, &lat).unwrap(),
    ));
    for (col, row, drawn) in [
        (4.5, 4.5, false),
        (5.5, 5.5, false),
        (5.2, 4.7, false),
        // Nearest node (4, 5), a corner of the undrawn cell (4, 5) too.
        (3.9, 4.6, true),
        (3.5, 3.5, true),
        (7.5, 5.5, true),
    ] {
        let (lon, lat) = plane.lon_lat(col, row);
        let (c, r) = grid.locate(lon, lat);
        assert_eq!(c.is_finite(), drawn, "({col}, {row}) at ({c}, {r})");
        if drawn {
            assert!((c - col).abs() < 1e-3 && (r - row).abs() < 1e-3);
        }
    }
    assert!(CurvilinearNodes::new(2, 2, &[0.0; 4], &[0.0, 0.0, f64::NAN, 0.0]).is_err());
    assert!(CurvilinearNodes::new(1, 5, &[0.0; 5], &[0.0; 5]).is_err());
    assert!(CurvilinearNodes::new(3, 3, &[0.0; 9], &[0.0; 8]).is_err());
}

/// A map over the grid places every pixel within the coarse grid's
/// interpolation budget of its own exact lookup, a Web Mercator view across
/// the antimeridian next to the pole included, and draws no pixel whose
/// lookup is off the footprint (Critical Rule 5: the lookups are made only
/// at the coarse grid's nodes).
#[test]
fn a_map_places_every_pixel_where_its_own_lookup_does() {
    let plane = Plane::svalbard();
    let source = SourceGrid::from_curvilinear(Arc::new(plane.grid()));
    let grid = source.curvilinear().unwrap();
    for (bbox, crs) in [
        ([150.0, 75.0, 210.0, 85.0], OutputCrs::WebMercator),
        ([-30.0, 60.0, 60.0, 84.0], OutputCrs::WebMercator),
        ([170.0, 80.0, -170.0, 89.0], OutputCrs::Wgs84),
        ([-180.0, 70.0, 180.0, 90.0], OutputCrs::Wgs84),
    ] {
        let (width, height) = (160, 120);
        let map = source.output_map(bbox, width, height, &crs);
        let [west, south, mut east, north] = bbox;
        if west > east {
            east += 360.0;
        }
        let (mut drawn, mut worst) = (0, 0.0_f64);
        for oy in 0..height {
            for ox in 0..width {
                let fx = (f64::from(ox) + 0.5) / f64::from(width);
                let fy = (f64::from(oy) + 0.5) / f64::from(height);
                let (lon, lat) = crs.project_node([west, south, east, north], fx, fy);
                let exact = grid.locate(lon, lat);
                let on = |f: f64, n: usize| (-1.0..n as f64).contains(&f);
                let mapped = map.position(ox, oy);
                if !exact.0.is_finite() {
                    assert_eq!(mapped, None, "{bbox:?} ({ox}, {oy})");
                    continue;
                }
                let inside = on(exact.0, 19) && on(exact.1, 28);
                // Within the budget of the cut, either side may hold it.
                let near_cut = [exact.0 + 1.0, 19.0 - exact.0, exact.1 + 1.0, 28.0 - exact.1]
                    .iter()
                    .any(|d| d.abs() < 0.25);
                match mapped {
                    Some((c, r)) => {
                        assert!(inside || near_cut, "{bbox:?} ({ox}, {oy}) off the grid");
                        worst = worst.max((c - exact.0).abs()).max((r - exact.1).abs());
                        drawn += 1;
                    }
                    None => assert!(!inside || near_cut, "{bbox:?} ({ox}, {oy}) undrawn"),
                }
            }
        }
        assert!(drawn > 100, "{bbox:?}: {drawn} drawn");
        assert!(worst < 0.25, "{bbox:?}: worst {worst} cells");
    }
}

/// A field sampled at a point is the bilinear value at the point's
/// position, as on every grid: here the column index itself.
#[test]
fn a_point_samples_the_field_at_its_position() {
    let plane = Plane::svalbard();
    let source = SourceGrid::from_curvilinear(Arc::new(plane.grid()));
    assert_eq!(source.size(), (19, 28));
    assert!(source.transform().is_none() && source.global_period().is_none());
    let columns: Vec<f32> = (0..28).flat_map(|_| (0..19).map(|i| i as f32)).collect();
    let rows: Vec<f32> = (0..28)
        .flat_map(|j| (0..19).map(move |_| j as f32))
        .collect();
    for (col, row) in [(3.25, 7.5), (17.9, 0.1), (9.0, 12.0), (0.6, 22.4)] {
        let (lon, lat) = plane.lon_lat(col, row);
        let position = source.position(lon, lat);
        let c = source.value_at(&columns, None, position).unwrap();
        let r = source.value_at(&rows, None, position).unwrap();
        assert!(
            (c - col).abs() < 1e-3 && (r - row).abs() < 1e-3,
            "({c}, {r})"
        );
    }
    assert_eq!(
        source.value_at(&columns, None, source.position(0.0, 40.0)),
        None
    );
}
