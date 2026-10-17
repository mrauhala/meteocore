//! [`SourceGrid`]: the sampling rules on small grids, the antimeridian
//! seam, low-zoom views and an Arctic EPSG:3035 view. engine-querydata
//! pins the sampler to its own render of the MEPS fixture
//! (`engine/sampler_tests.rs`).

use super::*;
use crate::geo::projected_output_crs;
use crate::web_mercator::{lat_to_y, lon_to_x};

/// The canonical seam box in the three forms an engine can be handed it.
const SEAM: [[f64; 4]; 3] = [
    [170.0, 10.0, -170.0, 20.0],
    [170.0, 10.0, 190.0, 20.0],
    [-190.0, 10.0, -170.0, 20.0],
];

/// A field smooth on the sphere (linear in its 3-D coordinates), so
/// continuous across the antimeridian and at the poles: what a grid holds
/// at `(lon, lat)`, and what a registered pixel there shows.
fn geography(lon: f64, lat: f64) -> f64 {
    let (lon, lat) = (lon.to_radians(), lat.to_radians());
    20.0 * lat.sin() + lat.cos() * (3.0 * lon.cos() + 2.0 * lon.sin())
}

/// [`geography`] at each cell centre of `gt`, row-major north first; NaN
/// where a centre does not inverse-project.
fn field_of(gt: &GeoTransform) -> Vec<f32> {
    let mut values = Vec::with_capacity(gt.width as usize * gt.height as usize);
    for row in 0..gt.height {
        let y = gt.origin_y - (f64::from(row) + 0.5) * gt.pixel_height;
        for col in 0..gt.width {
            let x = gt.origin_x + (f64::from(col) + 0.5) * gt.pixel_width;
            let v = gt.crs.inverse(x, y).map(|(lon, lat)| geography(lon, lat));
            values.push(v.map_or(f32::NAN, |v| v as f32));
        }
    }
    values
}

fn pixels(tile: &RasterTile) -> Vec<Option<f64>> {
    tile.values.iter_values().collect()
}

/// The WGS84 centre of every output pixel, mapped exactly (per pixel: the
/// reference the coarse map is checked against).
fn centres(bbox: [f64; 4], width: u32, height: u32, crs: &OutputCrs) -> Vec<(f64, f64)> {
    let mut out = Vec::with_capacity(width as usize * height as usize);
    for oy in 0..height {
        for ox in 0..width {
            let fx = (f64::from(ox) + 0.5) / f64::from(width);
            let fy = (f64::from(oy) + 0.5) / f64::from(height);
            out.push(crs.project_node(bbox, fx, fy));
        }
    }
    out
}

fn lat_lon(x0: f64, dx: f64, y0: f64, dy: f64, width: u32, height: u32) -> GeoTransform {
    GeoTransform::from_cell_centres(x0, dx, y0, dy, width, height, Crs::Wgs84).unwrap()
}

/// A tangent-cone Lambert projection on a sphere, MEPS's form.
fn lambert(lat0_deg: f64, lon0_deg: f64) -> Crs {
    let lat0 = lat0_deg.to_radians();
    Crs::LambertConformalConic {
        lat1: lat0,
        lat2: lat0,
        lat0,
        lon0: lon0_deg.to_radians(),
        false_e: 0.0,
        false_n: 0.0,
        radius: Some(6_371_000.0),
    }
}

/// MEPS's grid (Lambert conformal conic at 63.3°N, 15°E) at 25 km instead
/// of 2.5 km: 95 × 107 cells over the same area.
fn meps_shaped() -> GeoTransform {
    let crs = lambert(63.3, 15.0);
    GeoTransform::from_cell_centres(-1_060_000.0, 25_000.0, 1_337_500.0, -25_000.0, 95, 107, crs)
        .unwrap()
}

/// A north polar stereographic grid, ±3000 km around the pole at 50 km.
fn polar() -> GeoTransform {
    let crs = Crs::Stereographic {
        lat0: 90f64.to_radians(),
        lon0: (-45f64).to_radians(),
        k0: 1.0,
        false_e: 0.0,
        false_n: 0.0,
    };
    GeoTransform::from_cell_centres(
        -2_975_000.0,
        50_000.0,
        2_975_000.0,
        -50_000.0,
        120,
        120,
        crs,
    )
    .unwrap()
}

/// `lon` within half a turn of `centre`.
fn near(lon: f64, centre: f64) -> f64 {
    centre + (lon - centre + 180.0).rem_euclid(360.0) - 180.0
}

/// Check every pixel of a render against the exact geography. `source`
/// maps a pixel's WGS84 centre to its exact source position, measured from
/// the first cell's centre. A filled pixel must lie on the grid, within
/// its half-cell margin plus `slack` cells for the coarse map: no ghosts.
/// A pixel a cell or more inside the grid must be filled, with the value
/// [`geography`] gives there within `tolerance`. Returns the number of
/// filled pixels.
fn assert_registered(
    gt: &GeoTransform,
    tile: &RasterTile,
    centres: &[(f64, f64)],
    source: impl Fn(f64, f64) -> (f64, f64),
    (slack, tolerance): (f64, f64),
    label: &str,
) -> usize {
    let (nx, ny) = (f64::from(gt.width), f64::from(gt.height));
    let within = |v: f64, lo: f64, hi: f64| v >= lo && v <= hi;
    let mut filled = 0;
    for (i, (value, &(lon, lat))) in pixels(tile).iter().zip(centres).enumerate() {
        let (col, row) = if lon.is_finite() && lat.is_finite() {
            source(lon, lat)
        } else {
            (f64::NAN, f64::NAN)
        };
        if value.is_some() {
            filled += 1;
            assert!(
                within(col, -1.0 - slack, nx + slack) && within(row, -1.0 - slack, ny + slack),
                "{label}: pixel {i} at ({lon:.3}, {lat:.3}) is filled, but its source \
                 position ({col:.2}, {row:.2}) is off the {nx}x{ny} grid"
            );
        }
        if within(col, 1.0, nx - 2.0) && within(row, 1.0, ny - 2.0) {
            let value = value.unwrap_or_else(|| {
                panic!("{label}: pixel {i} at ({lon:.3}, {lat:.3}) inside the grid is empty")
            });
            let want = geography(lon, lat);
            assert!(
                (value - want).abs() < tolerance,
                "{label}: pixel {i} at ({lon:.3}, {lat:.3}) is {value}, the geography {want}"
            );
        }
    }
    filled
}

/// A 3 × 3 grid with cells 1 km (or 1°) apart.
fn three_by_three(crs: Crs) -> SourceGrid {
    let d = if crs == Crs::Wgs84 { 1.0 } else { 1000.0 };
    SourceGrid::new(GeoTransform::from_cell_centres(0.0, d, 2.0 * d, -d, 3, 3, crs).unwrap())
}

#[test]
fn samples_bilinear_with_the_nearest_cell_as_fallback() {
    let grid = three_by_three(lambert(63.3, 15.0));
    // 0 1 2
    // 3 4 5
    // 6 7 8
    let values: Vec<f32> = (0..9).map(|v| v as f32).collect();
    let at = |values: &[f32], nodata, col, row| grid.sample(values, nodata, col, row);
    // Between cell centres: bilinear.
    assert_eq!(at(&values, None, 0.5, 0.5), Some(2.0));
    assert_eq!(at(&values, None, 0.25, 0.0), Some(0.25));
    assert_eq!(at(&values, None, 1.5, 1.25), Some(5.25));
    // Up to half a cell past the edge cells' outer edges: the nearest
    // edge cell. Past that, nothing.
    assert_eq!(at(&values, None, -1.0, 1.0), Some(3.0));
    assert_eq!(at(&values, None, 2.99, 2.99), Some(8.0));
    assert_eq!(at(&values, None, 1.0, -0.9), Some(1.0));
    assert_eq!(at(&values, None, -1.01, 1.0), None);
    assert_eq!(at(&values, None, 3.0, 1.0), None);
    assert_eq!(at(&values, None, 1.0, 3.0), None);
    assert_eq!(at(&values, None, f64::NAN, 1.0), None);
    assert_eq!(at(&values, None, 1.0, f64::INFINITY), None);
    // A nodata neighbour (the sentinel, NaN or ±∞): the nearest cell,
    // itself possibly nodata.
    for (raw, nodata) in [
        (-9999.0, Some(-9999.0)),
        (f32::NAN, None),
        (f32::INFINITY, Some(-9999.0)),
    ] {
        let mut holed = values.clone();
        holed[4] = raw;
        assert_eq!(at(&holed, nodata, 0.4, 0.4), Some(0.0), "{raw}");
        assert_eq!(at(&holed, nodata, 1.4, 0.6), None, "{raw}");
        assert_eq!(at(&holed, nodata, 1.6, 0.4), Some(2.0), "{raw}");
        assert_eq!(at(&holed, nodata, 0.6, 1.6), Some(7.0), "{raw}");
    }
}

#[test]
fn lat_lon_columns_repeat_every_turn() {
    // Regional: the same cells a turn either way, nothing far off.
    let grid = three_by_three(Crs::Wgs84);
    let values: Vec<f32> = (0..9).map(|v| v as f32).collect();
    for turn in [-720.0, -360.0, 0.0, 360.0] {
        assert_eq!(grid.sample(&values, None, 0.5 + turn, 0.5), Some(2.0));
        assert_eq!(grid.sample(&values, None, -1.0 + turn, 1.0), Some(3.0));
    }
    assert_eq!(grid.sample(&values, None, 100.0, 1.0), None);
    assert_eq!(grid.sample(&values, None, -100.0, 1.0), None);

    // Global, 1° from 0.5°E: the last column's neighbour is the first.
    let gt = lat_lon(0.5, 1.0, 89.5, -1.0, 360, 180);
    let values: Vec<f32> = (0..360 * 180).map(|i| (i % 360) as f32).collect();
    let grid = SourceGrid::new(gt);
    for col in [359.5, -0.5, 719.5, -360.5] {
        assert_eq!(grid.sample(&values, None, col, 10.0), Some(179.5), "{col}");
    }
    assert_eq!(grid.sample(&values, None, 359.75, 10.0), Some(89.75));
    let mut holed = values.clone();
    holed[10 * 360] = f32::NAN;
    assert_eq!(grid.sample(&holed, None, 359.6, 10.0), None);
    assert_eq!(grid.sample(&holed, None, 359.4, 10.0), Some(359.0));
}

#[test]
fn renders_f32_with_nan_for_nodata() {
    let gt = meps_shaped();
    let grid = SourceGrid::new(gt.clone());
    let bbox = [-30.0, 40.0, 60.0, 85.0];
    let tile = grid
        .render_f32(&field_of(&gt), None, bbox, 64, 32, &OutputCrs::Wgs84)
        .unwrap();
    let RasterValues::F32 { data, nodata } = &tile.values else {
        panic!("not F32");
    };
    assert_eq!(*nodata, None);
    assert_eq!((tile.width, tile.height, data.len()), (64, 32, 64 * 32));
    assert!(data.iter().any(|v| v.is_nan()) && data.iter().any(|v| v.is_finite()));
}

#[test]
fn a_field_of_the_wrong_length_is_an_error() {
    let grid = SourceGrid::new(meps_shaped());
    let bbox = [0.0, 55.0, 30.0, 70.0];
    let result = grid.render_f32(&[0.0; 10], None, bbox, 8, 8, &OutputCrs::Wgs84);
    assert!(matches!(result, Err(DataServerError::Engine(_))));
}

#[test]
fn a_passed_deadline_stops_the_render() {
    let gt = meps_shaped();
    let grid = SourceGrid::new(gt.clone());
    let values = field_of(&gt);
    let _deadline = crate::deadline::enter(Some(std::time::Instant::now()));
    let bbox = [0.0, 55.0, 30.0, 70.0];
    let result = grid.render_f32(&values, None, bbox, 8, 8, &OutputCrs::Wgs84);
    assert!(matches!(result, Err(DataServerError::DeadlineExceeded)));
}

/// The seam box 170,10,-170,20, in each of its three forms, on global
/// grids stored from 180°W, from Greenwich, with a duplicate seam column,
/// and on a Web Mercator grid: every pixel filled with the geography's
/// value, no gap at the seam, in CRS84 and Web Mercator output.
#[test]
fn the_seam_box_is_continuous_on_global_grids() {
    let dx = lon_to_x(1.0);
    let top = lat_to_y(80.0);
    let rows = (2.0 * top / dx) as u32 + 1;
    let mercator = GeoTransform::from_cell_centres(
        lon_to_x(-179.5),
        dx,
        top,
        -dx,
        360,
        rows,
        Crs::WebMercator,
    )
    .unwrap();
    for (label, gt) in [
        ("from 180°W", lat_lon(-179.5, 1.0, 89.5, -1.0, 360, 180)),
        ("from Greenwich", lat_lon(0.5, 1.0, 89.5, -1.0, 360, 180)),
        (
            "duplicate seam column",
            lat_lon(0.0, 1.0, 90.0, -1.0, 361, 181),
        ),
        ("Web Mercator", mercator),
    ] {
        let grid = SourceGrid::new(gt.clone());
        let values = field_of(&gt);
        for crs in [OutputCrs::Wgs84, OutputCrs::WebMercator] {
            let reference = centres(SEAM[1], 64, 32, &crs);
            for bbox in SEAM {
                let tile = grid.render_f32(&values, None, bbox, 64, 32, &crs).unwrap();
                for (i, (value, &(lon, lat))) in pixels(&tile).iter().zip(&reference).enumerate() {
                    let value = value.unwrap_or_else(|| {
                        panic!("{label} {crs:?} {bbox:?}: pixel {i} at ({lon}, {lat}) empty")
                    });
                    let want = geography(lon, lat);
                    assert!(
                        (value - want).abs() < 0.01,
                        "{label} {crs:?} {bbox:?}: pixel {i} at ({lon}, {lat}) is {value}, \
                         the geography {want}"
                    );
                }
            }
        }
    }
}

/// Regional grids across the antimeridian: a lat/lon grid stored either
/// side of it, and Lambert grids centred on it, whose forward transform
/// needs the longitude within half a turn of their central meridian. Every
/// form of the seam box draws them whole.
#[test]
fn the_seam_box_draws_regional_grids_across_the_antimeridian() {
    let lcc = |lon0| {
        let crs = lambert(15.0, lon0);
        GeoTransform::from_cell_centres(-1_490_000.0, 20_000.0, 790_000.0, -20_000.0, 150, 80, crs)
            .unwrap()
    };
    for (label, gt, centre) in [
        (
            "lat/lon 160°E..160°W",
            lat_lon(160.25, 0.5, 29.75, -0.5, 80, 60),
            180.0,
        ),
        (
            "lat/lon 200°W..160°W",
            lat_lon(-199.75, 0.5, 29.75, -0.5, 80, 60),
            -180.0,
        ),
        ("Lambert at 180°", lcc(180.0), 180.0),
        ("Lambert at 180°W", lcc(-180.0), -180.0),
    ] {
        let grid = SourceGrid::new(gt.clone());
        let values = field_of(&gt);
        for crs in [OutputCrs::Wgs84, OutputCrs::WebMercator] {
            let reference = centres(SEAM[1], 64, 32, &crs);
            for bbox in SEAM {
                let tile = grid.render_f32(&values, None, bbox, 64, 32, &crs).unwrap();
                let filled = assert_registered(
                    &gt,
                    &tile,
                    &reference,
                    |lon, lat| centred(gt.world_to_pixel_f64(near(lon, centre), lat)),
                    (0.5, 0.02),
                    &format!("{label} {crs:?} {bbox:?}"),
                );
                assert_eq!(filled, 64 * 32, "{label} {crs:?} {bbox:?}");
            }
        }
    }
}

/// Whole-world views of a MEPS-shaped Lambert grid, one turn wide and
/// three: the grid is drawn once, where it is, and nowhere else. In the
/// view three turns wide, the Lambert forward transform alone maps the
/// pixels at 393°W, which show 33°W, onto the grid: n · 408° ≈ 364°.
#[test]
fn low_zoom_views_draw_no_ghosts() {
    let gt = meps_shaped();
    let grid = SourceGrid::new(gt.clone());
    let values = field_of(&gt);
    // The grid's own turn, around its central meridian; a pixel in any
    // other is drawn nowhere.
    let source = |lon: f64, lat: f64| {
        if (-165.0..195.0).contains(&lon) {
            centred(gt.world_to_pixel_f64(lon, lat))
        } else {
            (f64::MAX, f64::MAX)
        }
    };
    for (bbox, width, height, crs) in [
        ([-180.0, -90.0, 180.0, 90.0], 720, 360, OutputCrs::Wgs84),
        (
            [-180.0, -85.0, 180.0, 85.0],
            512,
            512,
            OutputCrs::WebMercator,
        ),
        (
            [-540.0, -85.0, 540.0, 85.0],
            1536,
            512,
            OutputCrs::WebMercator,
        ),
    ] {
        let tile = grid
            .render_f32(&values, None, bbox, width, height, &crs)
            .unwrap();
        let reference = centres(bbox, width, height, &crs);
        let label = format!("{crs:?} {bbox:?}");
        let filled = assert_registered(&gt, &tile, &reference, source, (1.0, 0.1), &label);
        assert!(filled > 50, "{label}: {filled} filled");
    }
}

/// An Arctic EPSG:3035 view holding the North Pole and the inverse's
/// longitude cut along 170°W, over Scandinavia: a MEPS-shaped Lambert
/// grid, a lat/lon grid global in longitude up to 75°N, and a polar
/// stereographic grid. No pixel outside a grid's footprint has data, and
/// every pixel inside has the geography's value. A global lat/lon grid
/// fills the view, registered across the cut and around the pole.
#[test]
fn an_arctic_laea_view_has_no_data_outside_the_footprint() {
    let laea = projected_output_crs("EPSG:3035").unwrap();
    let view = [2_000_000.0, 3_500_000.0, 7_000_000.0, 8_500_000.0];
    let crs = OutputCrs::Projected {
        crs: laea.clone(),
        bbox: view,
    };
    let (width, height) = (256, 256);
    let read = crate::geo::wgs84_envelope(&laea, view).unwrap();
    let reference = centres(read, width, height, &crs);
    let (meps, to_75, global, polar) = (
        meps_shaped(),
        lat_lon(-179.75, 0.5, 74.75, -0.5, 720, 270),
        lat_lon(-179.75, 0.5, 89.75, -0.5, 720, 360),
        polar(),
    );
    type Source<'a> = &'a dyn Fn(f64, f64) -> (f64, f64);
    let sources: [(&str, &GeoTransform, Source, bool); 4] = [
        (
            "MEPS-shaped",
            &meps,
            &|lon, lat| centred(meps.world_to_pixel_f64(near(lon, 15.0), lat)),
            true,
        ),
        (
            "lat/lon to 75°N",
            &to_75,
            &|lon, lat| centred(to_75.world_to_pixel_f64(near(lon, 0.0), lat)),
            true,
        ),
        (
            "global lat/lon",
            &global,
            &|lon, lat| centred(global.world_to_pixel_f64(near(lon, 0.0), lat)),
            false,
        ),
        (
            "polar stereographic",
            &polar,
            &|lon, lat| centred(polar.world_to_pixel_f64(lon, lat)),
            true,
        ),
    ];
    for (label, gt, source, partial) in sources {
        let grid = SourceGrid::new(gt.clone());
        let tile = grid
            .render_f32(&field_of(gt), None, read, width, height, &crs)
            .unwrap();
        let filled = assert_registered(gt, &tile, &reference, source, (0.5, 0.05), label);
        let empty = width as usize * height as usize - filled;
        if partial {
            assert!(
                filled > 500 && empty > 500,
                "{label}: {filled} filled, {empty} empty"
            );
        } else {
            assert_eq!(empty, 0, "{label}");
        }
    }
}

/// A grid holding the pole reaches it at every longitude: geographic
/// output across the seam and over the whole cap draws it up to the pole,
/// though none of the grid's edges comes near it.
#[test]
fn a_grid_holding_the_pole_is_drawn_to_the_pole() {
    let gt = polar();
    let grid = SourceGrid::new(gt.clone());
    let values = field_of(&gt);
    for bbox in [
        [170.0, 60.0, -170.0, 85.0],
        [-180.0, 50.0, 180.0, 85.0],
        [0.0, 50.0, 360.0, 85.0],
    ] {
        for crs in [OutputCrs::Wgs84, OutputCrs::WebMercator] {
            let tile = grid.render_f32(&values, None, bbox, 128, 64, &crs).unwrap();
            let [west, south, mut east, north] = bbox;
            if west > east {
                east += 360.0;
            }
            let reference = centres([west, south, east, north], 128, 64, &crs);
            let label = format!("{crs:?} {bbox:?}");
            let source = |lon, lat| centred(gt.world_to_pixel_f64(lon, lat));
            let filled = assert_registered(&gt, &tile, &reference, source, (0.5, 0.05), &label);
            assert!(filled > 1000, "{label}: {filled} filled");
        }
    }
}

/// MEPS's grid as the engine-netcdf fixture samples it, every 100th
/// point: 10 × 11 cells 250 km apart.
fn meps_sample() -> GeoTransform {
    let crs = lambert(63.3, 15.0);
    GeoTransform::from_cell_centres(
        -1_060_084.0,
        250_000.0,
        1_167_482.1,
        -250_000.0,
        10,
        11,
        crs,
    )
    .unwrap()
}

/// MEPS's grid in full: 949 × 1069 cells at 2.5 km.
fn meps() -> GeoTransform {
    let crs = lambert(63.3, 15.0);
    GeoTransform::from_cell_centres(-1_060_084.0, 2_500.0, 1_337_482.1, -2_500.0, 949, 1069, crs)
        .unwrap()
}

/// #1110: the 256 px WMS meta-tiles of an EPSG:3067 and an EPSG:3035
/// request near the pole, on MEPS's grid sampled and in full. Past the
/// grid's footprint their output→source map jumps: EPSG:3067's inverse
/// breaks down in the first tile's north-east corner, and the Lambert
/// forward transform jumps where longitude wraps, half a turn from the
/// grid, in the second. Only the footprint is sampled, so only its error
/// counts: a few hundred nodes instead of the 256 × 256 cap's 66 049.
/// Every pixel with a place on the Earth is what the same map projected
/// per pixel draws, except where its own position error, under the half
/// pixel nearest-neighbour resolves, reaches across the sampler's edge.
#[test]
fn polar_meta_tiles_refine_only_the_footprint() {
    let tiles = [
        (
            "EPSG:3067",
            [
                1448154.6878700492,
                7240773.439350246,
                2896309.3757400983,
                8688928.127220295,
            ],
            [
                -651.893197248287,
                57.99572959596767,
                168.97855020194945,
                143.55146987202554,
            ],
        ),
        (
            "EPSG:3035",
            [
                2896309.3757400983,
                5792618.7514801975,
                5792618.751480197,
                8688928.127220295,
            ],
            [
                -165.5766516894288,
                70.8962238105685,
                189.14274294709074,
                77.91855602096956,
            ],
        ),
    ];
    let (width, height) = (256, 256);
    for (label, gt) in [("sample", meps_sample()), ("full", meps())] {
        let grid = SourceGrid::new(gt.clone());
        let Layout::Projected {
            envelope: Some([w, _, e, _]),
        } = grid.layout
        else {
            panic!("{label}: no envelope");
        };
        let values = field_of(&gt);
        let (nx, ny) = (f64::from(gt.width), f64::from(gt.height));
        for (epsg, projected, bbox) in tiles {
            let out = projected_output_crs(epsg).unwrap();
            let crs = OutputCrs::Projected {
                crs: out.clone(),
                bbox: projected,
            };
            let label = format!("{label} {epsg}");
            let map = grid.output_map(bbox, width, height, &crs);
            let nodes = map.grid.node_count();
            assert!(nodes <= 400, "{label}: {nodes} nodes");
            let tile = map.sample_f32(&values, None).unwrap();
            let (x_lo, x_hi, y_lo, y_hi) = map.window.unwrap();
            // Whether output position (fx, fy) has a reproducible place on
            // the Earth: a latitude within 90° and, its longitude taken
            // within ±180°, a forward transform that returns to the point.
            // Some 60° from its central meridian EPSG:3067's inverse breaks
            // down (#1113), into positions that land anywhere, differently
            // with each platform's libm. A pixel there has no per-pixel
            // truth, nor does a pixel of a cell with a corner there,
            // interpolated from it or from a sub-grid around it.
            let on_earth = |fx: f64, fy: f64| {
                let [min_e, min_n, max_e, max_n] = projected;
                let (x, y) = (min_e + fx * (max_e - min_e), max_n - fy * (max_n - min_n));
                let (lon, lat) = crs.project_node(bbox, fx, fy);
                let (bx, by) = out.forward((lon + 180.0).rem_euclid(360.0) - 180.0, lat);
                lat.abs() <= 90.0 && (bx - x).hypot(by - y) < 1.0
            };
            let g = &map.grid;
            let node_on_earth: Vec<bool> = (0..=g.cells_y)
                .flat_map(|j| (0..=g.cells_x).map(move |i| (i, j)))
                .map(|(i, j)| on_earth(i as f64 / g.cells_x as f64, j as f64 / g.cells_y as f64))
                .collect();
            let (mut filled, mut skipped) = (0, 0);
            for (i, got) in pixels(&tile).into_iter().enumerate() {
                let (ox, oy) = (i as u32 % width, i as u32 / width);
                let (fx, fy) = (
                    (f64::from(ox) + 0.5) / f64::from(width),
                    (f64::from(oy) + 0.5) / f64::from(height),
                );
                let read = (x_lo..=x_hi).contains(&ox) && (y_lo..=y_hi).contains(&oy);
                let cx = ((f64::from(ox) + 0.5) / g.cell_w) as usize;
                let cy = ((f64::from(oy) + 0.5) / g.cell_h) as usize;
                let n = cy.min(g.cells_y - 1) * g.stride + cx.min(g.cells_x - 1);
                let corners = [n, n + 1, n + g.stride, n + g.stride + 1];
                if !on_earth(fx, fy) || (read && !corners.iter().all(|&k| node_on_earth[k])) {
                    skipped += usize::from(read);
                    continue;
                }
                let (lon, lat) = crs.project_node(bbox, fx, fy);
                let exact =
                    centred(gt.world_to_pixel_f64(within_half_turn(lon, (w + e) / 2.0), lat));
                let want = grid.sample(&values, None, exact.0, exact.1);
                if !read {
                    // Outside the footprint: nothing drawn, nothing to draw.
                    assert_eq!((got, want), (None, None), "{label}: pixel ({ox}, {oy})");
                    continue;
                }
                // Where it can be seen, the coarse map holds the half
                // pixel nearest-neighbour resolves, as `grid_matches_exact_*`
                // pin it; the 0.2 px budget is the estimate's target.
                let (col, row) = g.sample(ox, oy);
                let err = (col - exact.0).abs().max((row - exact.1).abs());
                let seen = |c: f64, r: f64| c > -2.0 && c < nx + 1.0 && r > -2.0 && r < ny + 1.0;
                assert!(
                    !(seen(col, row) || seen(exact.0, exact.1)) || err < 0.5,
                    "{label}: pixel ({ox}, {oy}) at ({col}, {row}), per pixel {exact:?}"
                );
                match (got, want) {
                    (Some(got), Some(want)) => {
                        filled += 1;
                        assert!(
                            (got - want).abs() < 0.01,
                            "{label}: pixel ({ox}, {oy}) is {got}, per pixel {want}"
                        );
                    }
                    (None, None) => {}
                    // Drawn on one side of the grid's edge only: the
                    // pixel's own position error must reach across it. The
                    // sampler draws positions in [-1, n) along each axis.
                    _ => {
                        let edge = [exact.0 + 1.0, exact.0 - nx, exact.1 + 1.0, exact.1 - ny]
                            .iter()
                            .fold(f64::MAX, |d, x| d.min(x.abs()));
                        assert!(
                            edge <= err,
                            "{label}: pixel ({ox}, {oy}) is {got:?}, per pixel {want:?}, \
                             {edge:.3} px from the edge, {err:.3} px off"
                        );
                    }
                }
            }
            // EPSG:3067's tile reads one 16 px cell with a corner where the
            // inverse breaks down, and a few pixels there.
            assert!(skipped < 100, "{label}: {skipped} read pixels skipped");
            // The sample's 250 km cells reach past the full grid's east
            // edge, into the EPSG:3067 tile.
            if label != "full EPSG:3067" {
                assert!(filled > 300, "{label}: {filled} filled");
            }
        }
    }
}
