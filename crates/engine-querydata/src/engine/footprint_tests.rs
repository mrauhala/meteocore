//! The projected map path's footprint (#1114): [`GridMap`] refines its
//! coarse grid for, and samples, only the output its grid's footprint can
//! occupy, and projects a pixel from the longitude it shows within half a
//! turn of the grid. On a full-size MEPS grid built here from its
//! definition (no fixture is that large), on a Lambert grid centred on the
//! antimeridian, and on the cropped MEPS fixture.

use std::cell::Cell;

use super::*;
use crate::parse::GridArea;
use ds_core::geo::{projected_envelope, projected_output_crs, wgs84_envelope, Crs};

/// A Lambert conformal conic grid tangent at `lat0`° on a sphere of
/// `radius` metres: `nx` × `ny` cells `step` metres apart, the north-west
/// cell's centre at `(x0, y0)`. Built as [`GridInfo::new`] builds a parsed
/// one, from the lon/lat of two corner cell centres.
fn lambert(
    (lat0, lon0, radius): (f64, f64, f64),
    (x0, y0): (f64, f64),
    step: f64,
    (nx, ny): (u32, u32),
) -> GridInfo {
    let lat0 = lat0.to_radians();
    let crs = Crs::LambertConformalConic {
        lat1: lat0,
        lat2: lat0,
        lat0,
        lon0: lon0.to_radians(),
        false_e: 0.0,
        false_n: 0.0,
        radius: Some(radius),
    };
    let corner = |x, y| crs.inverse(x, y).unwrap();
    let bottom_left = corner(x0, y0 - f64::from(ny - 1) * step);
    let top_right = corner(x0 + f64::from(nx - 1) * step, y0);
    GridInfo::new(
        nx,
        ny,
        GridArea {
            bottom_left,
            top_right,
            crs,
        },
    )
}

/// MEPS in full: 949 × 1069 cells at 2.5 km, tangent at 63.3°N, 15°E, on
/// QueryData's 6 371 220 m sphere.
fn meps() -> GridInfo {
    lambert(
        (63.3, 15.0, 6_371_220.0),
        (-1_060_084.0, 1_337_482.1),
        2_500.0,
        (949, 1069),
    )
}

/// The cropped MEPS fixture's engine.
fn meps_fixture() -> QueryDataEngine {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/meps");
    QueryDataEngine::new(&dir, "test", None, 30, 4).unwrap()
}

/// [`GridMap::new`] over `info`, with the number of grid projections its
/// build made: nodes, error probes and boundary sub-grids.
fn counted(
    info: &GridInfo,
    bbox: [f64; 4],
    (width, height): (u32, u32),
    crs: &OutputCrs,
) -> (Option<GridMap>, usize) {
    let gt = info.geo_transform();
    let calls = Cell::new(0);
    let map = GridMap::new(info, bbox, width, height, crs, |lon, lat| {
        calls.set(calls.get() + 1);
        world_to_grid_px(gt, lon, lat)
    });
    (map, calls.get())
}

/// Whether grid position `(col, row)` is where [`sample_grid_bilinear`]
/// finds a cell: within the half cell past the grid's edges.
fn on_grid(info: &GridInfo, (col, row): (f64, f64)) -> bool {
    (-1.0..f64::from(info.nx)).contains(&col) && (-1.0..f64::from(info.ny)).contains(&row)
}

/// The fractional position of output pixel `(ox, oy)`'s centre.
fn centre(ox: u32, oy: u32, (width, height): (u32, u32)) -> (f64, f64) {
    (
        (f64::from(ox) + 0.5) / f64::from(width),
        (f64::from(oy) + 0.5) / f64::from(height),
    )
}

/// #1110's polar WMS meta-tiles, the 256 px tiles of an EPSG:3067 and an
/// EPSG:3035 request near the pole, on the full MEPS grid. Past the grid's
/// footprint, the EPSG:3035 tile's map jumps where the Lambert forward
/// transform crosses the cone's gap, half a turn from the grid. Refined
/// for the whole tile, that never converged: the build ran to the 256 ×
/// 256 cell cap, 1 486 134 projections and a stderr warning per render.
/// Refined for the footprint it takes 1394. Since #1113 bounded the
/// EPSG:3067 inverse, that tile no longer reaches the cap: 3796
/// projections for the whole tile, 3060 for the footprint. No pixel
/// outside the footprint has a place on the grid, and inside it the coarse
/// map holds the half pixel nearest-neighbour resolves.
#[test]
fn polar_meta_tiles_refine_only_the_footprint() {
    let info = meps();
    let gt = info.geo_transform();
    let [w, _, e, _] = info.footprint().unwrap();
    let size = (256, 256);
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
            4_000,
            0,
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
            2_000,
            400,
        ),
    ];
    for (epsg, projected, bbox, budget, min_on_grid) in tiles {
        let crs = OutputCrs::Projected {
            crs: projected_output_crs(epsg).unwrap(),
            bbox: projected,
        };
        let (map, projections) = counted(&info, bbox, size, &crs);
        let map = map.expect("the footprint meets the tile");
        assert!(projections <= budget, "{epsg}: {projections} projections");
        let mut seen = 0;
        for oy in 0..size.1 {
            for ox in 0..size.0 {
                let (fx, fy) = centre(ox, oy, size);
                let (lon, lat) = crs.project_node(bbox, fx, fy);
                let exact = world_to_grid_px(gt, within_half_turn(lon, (w + e) / 2.0), lat);
                let Some(at) = map.position(ox, oy) else {
                    assert!(
                        !on_grid(&info, exact),
                        "{epsg}: ({ox}, {oy}) at {exact:?}, outside the footprint"
                    );
                    continue;
                };
                if on_grid(&info, at) || on_grid(&info, exact) {
                    seen += 1;
                    assert!(
                        (at.0 - exact.0).abs() < 0.5 && (at.1 - exact.1).abs() < 0.5,
                        "{epsg}: ({ox}, {oy}) mapped to {at:?}, exactly {exact:?}"
                    );
                }
            }
        }
        assert!(seen >= min_on_grid, "{epsg}: {seen} pixels on the grid");
    }
}

/// A tile inside the grid's footprint is mapped as before #1114, bit for
/// bit: its window is the whole tile, its longitudes lie within half a
/// turn of the grid's centre, and its grid is the whole-tile build's.
#[test]
fn an_in_footprint_tile_maps_as_the_whole_tile_build() {
    let info = meps();
    let gt = info.geo_transform();
    let size = (256, 256);
    let tm = projected_output_crs("EPSG:3067").unwrap();
    let finland = [200_000.0, 6_700_000.0, 700_000.0, 7_200_000.0];
    let tiles = [
        // Web Mercator z = 5, x = 17, y = 8.
        (
            [11.25, 61.60639637, 22.5, 66.51326044],
            OutputCrs::WebMercator,
        ),
        ([5.0, 57.0, 25.0, 68.0], OutputCrs::Wgs84),
        (
            wgs84_envelope(&tm, finland).unwrap(),
            OutputCrs::Projected {
                crs: tm,
                bbox: finland,
            },
        ),
    ];
    for (bbox, crs) in tiles {
        let map = counted(&info, bbox, size, &crs).0.unwrap();
        assert_eq!(map.window, (0, 255, 0, 255), "{crs:?}");
        let whole = ProjectionGrid::build_2d(
            size.0,
            size.1,
            info.nx,
            info.ny,
            |fx, fy| crs.project_node(bbox, fx, fy),
            |lon, lat| world_to_grid_px(gt, lon, lat),
        );
        for oy in 0..size.1 {
            for ox in 0..size.0 {
                assert_eq!(
                    map.position(ox, oy),
                    Some(whole.sample(ox, oy)),
                    "{crs:?}: ({ox}, {oy})"
                );
            }
        }
    }
}

/// The same on the fixture's data: an in-footprint tile draws exactly what
/// the whole-tile build drew, value for value.
#[test]
fn an_in_footprint_tile_draws_as_before() {
    let engine = meps_fixture();
    let data = engine.latest_data().unwrap();
    let gt = data.grid.geo_transform();
    let param = engine.resolve_map_param_idx(&data);
    let time = find_time_idx(&data, None).unwrap();
    let size = (256, 256);
    let (tm, laea) = (
        projected_output_crs("EPSG:3067").unwrap(),
        projected_output_crs("EPSG:3035").unwrap(),
    );
    let inside = [11.0, 61.0, 17.0, 64.0];
    let mut views = vec![(inside, OutputCrs::Wgs84), (inside, OutputCrs::WebMercator)];
    for crs in [tm, laea] {
        let bbox = projected_envelope(&crs, inside);
        let read = wgs84_envelope(&crs, bbox).unwrap();
        views.push((read, OutputCrs::Projected { crs, bbox }));
    }
    for (bbox, crs) in views {
        let tile = engine
            .get_raster_tile(bbox, size.0, size.1, None, &crs, None, None, None)
            .unwrap();
        let whole = ProjectionGrid::build_2d(
            size.0,
            size.1,
            data.grid.nx,
            data.grid.ny,
            |fx, fy| crs.project_node(bbox, fx, fy),
            |lon, lat| world_to_grid_px(gt, lon, lat),
        );
        let mut filled = 0;
        for oy in 0..size.1 {
            for ox in 0..size.0 {
                let (col, row) = whole.sample(ox, oy);
                let before = sample_grid_bilinear(&data, col, row, param, 0, time);
                let i = (oy * size.0 + ox) as usize;
                assert_eq!(tile.values.value_at(i), before, "{crs:?}: ({ox}, {oy})");
                filled += usize::from(before.is_some());
            }
        }
        assert!(filled > 50_000, "{crs:?}: {filled} filled");
    }
}

/// `Lcc::forward` is not periodic in longitude (θ = n·Δλ, n ≈ 0.89 for
/// MEPS), so a pixel a turn from the grid, projected as given, lands
/// elsewhere on the cone. In a second world copy of a Web Mercator view,
/// 58°E + 360° maps to n · 403° ≈ 360°, MEPS's own central meridian:
/// before #1114 a 256 px view of 405°..430°E drew 13 255 pixels of the
/// fixture's 15°E data there, and a view of 0°..30°E a turn east drew
/// none where the grid's copy lies. Projected from the longitude it shows,
/// a view a turn away draws what the view in place draws, and 405°..430°E
/// draws nothing. A view three turns wide draws the grid once, in the
/// turn around its centre, and every pixel where it lies.
#[test]
fn a_lambert_grid_is_drawn_where_it_lies_in_every_world_copy() {
    let engine = meps_fixture();
    let data = engine.latest_data().unwrap();
    let gt = data.grid.geo_transform();
    let [w, _, e, _] = data.grid.footprint().unwrap();
    let render = |bbox: [f64; 4], (width, height): (u32, u32)| {
        engine
            .get_raster_tile(
                bbox,
                width,
                height,
                None,
                &OutputCrs::WebMercator,
                None,
                None,
                None,
            )
            .unwrap()
            .values
            .iter_values()
            .collect::<Vec<_>>()
    };
    let in_place = render([0.0, 55.0, 30.0, 70.0], (256, 256));
    let filled = in_place.iter().flatten().count();
    assert!(filled > 5_000, "{filled} filled in place");
    for turns in [1.0, -1.0, 2.0] {
        let (west, east) = (360.0 * turns, 30.0 + 360.0 * turns);
        let shifted = render([west, 55.0, east, 70.0], (256, 256));
        for (i, (a, b)) in in_place.iter().zip(&shifted).enumerate() {
            match (a, b) {
                (Some(a), Some(b)) => {
                    assert!((a - b).abs() < 1e-6, "{west}..{east} #{i}: {a} vs {b}")
                }
                (None, None) => {}
                _ => panic!("{west}..{east} #{i}: {a:?} vs {b:?}"),
            }
        }
    }
    let ghost = render([405.0, 58.0, 430.0, 68.0], (256, 256));
    let drawn = ghost.iter().flatten().count();
    assert_eq!(drawn, 0, "405°..430°E: {drawn} pixels drawn");

    let (bbox, size) = ([-540.0, -85.0, 540.0, 85.0], (768, 256));
    let world = render(bbox, size);
    let mut drawn = 0;
    for (i, value) in world.iter().enumerate() {
        if value.is_none() {
            continue;
        }
        drawn += 1;
        let (ox, oy) = (i as u32 % size.0, i as u32 / size.0);
        let (fx, fy) = centre(ox, oy, size);
        let (lon, lat) = OutputCrs::WebMercator.project_node(bbox, fx, fy);
        let half = (w + e) / 2.0;
        assert!(
            (half - 180.0..half + 180.0).contains(&lon),
            "({ox}, {oy}) drawn at {lon}°, a turn from the grid"
        );
        let (col, row) = world_to_grid_px(gt, lon, lat);
        let near = |c: f64, n: u32| c > -2.0 && c < f64::from(n) + 1.0;
        assert!(
            near(col, data.grid.nx) && near(row, data.grid.ny),
            "({ox}, {oy}) at ({lon}, {lat}) drawn from grid ({col}, {row})"
        );
    }
    assert!(drawn > 20, "three turns: {drawn} drawn");
}

/// The antimeridian seam box in each form an engine can be handed it:
/// `west > east`, unwrapped east past 180°, and unwrapped west past 180°W.
const SEAM: [[f64; 4]; 3] = [
    [170.0, 10.0, -170.0, 20.0],
    [170.0, 10.0, 190.0, 20.0],
    [-190.0, 10.0, -170.0, 20.0],
];

/// The seam box on the MEPS fixture, far from it: nothing drawn, on
/// geographic or Web Mercator output.
#[test]
fn the_seam_box_draws_nothing_of_meps() {
    let engine = meps_fixture();
    for crs in [OutputCrs::Wgs84, OutputCrs::WebMercator] {
        for bbox in SEAM {
            let tile = engine
                .get_raster_tile(bbox, 64, 32, None, &crs, None, None, None)
                .unwrap();
            let drawn = tile.values.iter_values().flatten().count();
            assert_eq!(drawn, 0, "{crs:?} {bbox:?}");
        }
    }
}

/// A tile the footprint misses builds no grid and makes no projection
/// (#1129): on the full MEPS grid, a view a turn east of the grid's east
/// edge, one south of it, and the seam box in each form, geographic and
/// Web Mercator. The engine answers such a tile with every pixel empty.
#[test]
fn a_tile_the_footprint_misses_builds_no_grid() {
    let info = meps();
    let mut views = vec![
        ([430.0, 58.0, 460.0, 68.0], OutputCrs::WebMercator),
        ([0.0, 0.0, 30.0, 20.0], OutputCrs::Wgs84),
    ];
    for crs in [OutputCrs::Wgs84, OutputCrs::WebMercator] {
        views.extend(SEAM.map(|bbox| (bbox, crs.clone())));
    }
    for (bbox, crs) in views {
        let (map, projections) = counted(&info, bbox, (256, 256), &crs);
        assert!(
            map.is_none() && projections == 0,
            "{crs:?} {bbox:?}: {projections} projections"
        );
    }
    let tile = meps_fixture()
        .get_raster_tile(
            [405.0, 58.0, 430.0, 68.0],
            256,
            256,
            None,
            &OutputCrs::WebMercator,
            None,
            None,
            None,
        )
        .unwrap();
    assert_eq!(tile.values.len(), 256 * 256);
    assert_eq!(tile.values.iter_values().flatten().count(), 0);
}

/// A Lambert grid centred on the antimeridian (15°N, 180°; 150 × 80
/// cells at 20 km) under the seam box: every form maps every pixel onto
/// the grid, within half a pixel of where its longitude east of
/// Greenwich, 170°..190°, projects.
#[test]
fn a_grid_on_the_antimeridian_is_mapped_across_the_seam() {
    let info = lambert(
        (15.0, 180.0, 6_371_000.0),
        (-1_490_000.0, 790_000.0),
        20_000.0,
        (150, 80),
    );
    let gt = info.geo_transform();
    let size = (64, 32);
    for crs in [OutputCrs::Wgs84, OutputCrs::WebMercator] {
        for bbox in SEAM {
            let map = counted(&info, bbox, size, &crs).0.unwrap();
            let unwrapped = [170.0, 10.0, 190.0, 20.0];
            for oy in 0..size.1 {
                for ox in 0..size.0 {
                    let (fx, fy) = centre(ox, oy, size);
                    let (lon, lat) = crs.project_node(unwrapped, fx, fy);
                    let exact = world_to_grid_px(gt, lon, lat);
                    let at = map.position(ox, oy);
                    assert!(
                        at.is_some_and(|at| on_grid(&info, at)
                            && (at.0 - exact.0).abs() < 0.5
                            && (at.1 - exact.1).abs() < 0.5),
                        "{crs:?} {bbox:?}: ({ox}, {oy}) mapped to {at:?}, exactly {exact:?}"
                    );
                }
            }
        }
    }
}
