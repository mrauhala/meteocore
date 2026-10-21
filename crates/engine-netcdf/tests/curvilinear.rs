//! engine-netcdf over a curvilinear grid (#1174): testdata/netcdf-curvilinear,
//! whose fields are placed only by 2-D `lat(y, x)` and `lon(y, x)`
//! auxiliaries (no 1-D coordinates, no grid mapping), 19 × 28 nodes of a
//! polar stereographic plane around the North Pole, over Svalbard and
//! across the antimeridian. Its two fields are each node's own `column`
//! and `row` index, so a read anywhere returns the fractional position the
//! server put the point at.
//!
//! The expected positions come from `tests/curvilinear_reference.py`, a
//! stdlib brute force over the CDL's own arrays (every cell inverted in
//! closed form; the nearest node past the outer nodes); run it to print
//! the tables below.

use std::path::PathBuf;

use ds_core::config::NetcdfConfig;
use ds_core::cube::CubeResolution;
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::feature::Bbox;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterTile};
use ds_core::model::{CoverageResponse, DomainDescription, QueryResult};
use ds_core::web_mercator::{lat_to_y, x_to_lon, y_to_lat, EARTH_RADIUS};
use ds_core::wind::{GridAxes, WindSource};
use engine_netcdf::NetcdfEngine;

/// The `(column, row)` read at a point, `None` off the grid.
type Read = Option<(f64, f64)>;

fn engine() -> NetcdfEngine {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/netcdf-curvilinear");
    let config: NetcdfConfig =
        toml::from_str(&format!("data_path = {:?}", path.display().to_string())).unwrap();
    let engine = NetcdfEngine::new("netcdf-curvilinear", &config).unwrap();
    engine.poll_once();
    engine
}

fn single(response: CoverageResponse) -> QueryResult {
    match response {
        CoverageResponse::Single(result) => result,
        CoverageResponse::Collection(_) => panic!("expected a single coverage"),
    }
}

fn fields() -> Vec<String> {
    vec!["column".to_string(), "row".to_string()]
}

/// `(column, row)` an EDR position query reads at `(lon, lat)`; `None`
/// where both are null.
fn edr(engine: &NetcdfEngine, lon: f64, lat: f64) -> Read {
    let result = single(
        engine
            .query_position(
                &format!("POINT({lon} {lat})"),
                None,
                Some(&fields()),
                None,
                None,
            )
            .unwrap(),
    );
    let value = |field: &str| result.ranges[field].values[0];
    match (value("column"), value("row")) {
        (Some(c), Some(r)) => Some((c, r)),
        (None, None) => None,
        other => panic!("({lon}, {lat}): {other:?}"),
    }
}

/// Both fields drawn on one map.
fn draw(
    engine: &NetcdfEngine,
    bbox: [f64; 4],
    width: u32,
    height: u32,
    crs: &OutputCrs,
) -> [RasterTile; 2] {
    let tile = |field: &str| {
        engine
            .get_raster_tile(bbox, width, height, None, crs, Some(field), None, None)
            .unwrap()
    };
    [tile("column"), tile("row")]
}

/// Pixel `i`'s `(column, row)`, `None` where both are empty.
fn pixel([column, row]: &[RasterTile; 2], i: usize) -> Read {
    match (column.values.value_at(i), row.values.value_at(i)) {
        (Some(c), Some(r)) => Some((c, r)),
        (None, None) => None,
        other => panic!("pixel {i}: {other:?}"),
    }
}

fn close(a: Read, b: Read, tolerance: f64) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => (a.0 - b.0).abs() < tolerance && (a.1 - b.1).abs() < tolerance,
        (None, None) => true,
        _ => false,
    }
}

/// `curvilinear_reference.py`'s points: inside the nodes' hull the
/// fractional position; past the outer nodes, within half a node, the
/// nearest node; far off, null.
const POINTS: [(&str, f64, f64, Read); 11] = [
    ("Longyearbyen", 15.633, 78.217, Some((9.140874, 23.751279))),
    ("North Pole", 0.0, 90.0, Some((9.000000, 11.000000))),
    (
        "North Pole from 123E",
        123.0,
        90.0,
        Some((9.000000, 11.000000)),
    ),
    (
        "antimeridian west",
        179.5,
        84.0,
        Some((10.730761, 4.759118)),
    ),
    (
        "antimeridian east",
        -179.5,
        84.0,
        Some((10.621579, 4.729872)),
    ),
    ("Kvitoya", 33.0, 80.1, Some((12.307417, 21.179107))),
    ("near row 0", -150.0, 79.0, Some((6.000000, 0.000000))),
    (
        "a third of a node past row 27",
        15.0,
        75.0,
        Some((9.000000, 27.000000)),
    ),
    ("Hopen", 25.0, 76.5, Some((11.539850, 25.404181))),
    ("Tromso", 18.96, 69.65, None),
    ("off the grid", -100.0, 40.0, None),
];

/// The reference's map pixels: `(ox, oy, (column, row))`.
type Pins = [(usize, usize, Read)];

/// A CRS84 map across the antimeridian, 20 × 12 over 170…190°E, 80…88°N.
const CRS84_PINS: [(usize, usize, Read); 8] = [
    (0, 0, Some((10.043629, 8.709917))),
    (9, 5, Some((10.634457, 5.106453))),
    (10, 5, Some((10.531351, 5.078842))),
    (19, 11, Some((10.001527, 0.598425))),
    (5, 2, Some((10.320388, 7.271327))),
    (14, 9, Some((10.640629, 2.148097))),
    (0, 11, Some((13.333335, 1.491320))),
    (19, 0, Some((9.241204, 8.494914))),
];

/// An EPSG:3857 map over Svalbard, 32 × 24 over 0…40°E, 73…82°N.
const MERCATOR_PINS: [(usize, usize, Read); 8] = [
    (0, 0, Some((6.820426, 19.504419))),
    (12, 10, Some((9.131410, 23.046696))),
    (16, 12, Some((10.258010, 23.772833))),
    (30, 19, Some((15.290639, 25.730385))),
    (5, 20, Some((7.000000, 27.000000))),
    (25, 3, Some((11.802287, 20.237901))),
    (0, 23, None),
    (8, 16, Some((7.888814, 25.524345))),
];

/// The grid is recognised from its 2-D latitude/longitude, has no CRS to
/// advertise, and its extent holds the North Pole at every longitude and
/// reaches south past its southernmost node (73.1009°N, the southern
/// corners) by less than half a cell (~0.8° there). Its axes' directions
/// on the ground are unknown, so grid-relative wind would give nothing.
#[test]
fn the_grid_is_curvilinear_with_an_extent_holding_the_pole() {
    let engine = engine();
    let info = engine.raster_info();
    assert_eq!(info.native_crs, "curvilinear");
    assert_eq!(info.grid_size, Some([19, 28]));
    assert!(info.native_grid.is_none());
    assert!(engine.grid().is_none(), "no transform");
    assert_eq!(engine.wind_facts().grid, GridAxes::Skewed);
    let names: Vec<(&str, &str)> = info
        .parameters
        .iter()
        .map(|p| (p.name.as_str(), p.unit.as_str()))
        .collect();
    assert_eq!(names, [("column", "1"), ("row", "1")]);
    let [w, s, e, n] = info.spatial_extent.unwrap();
    assert_eq!((w, e, n), (-180.0, 180.0, 90.0));
    assert!(s < 73.1009 && s > 72.3, "south {s}");
}

/// EDR positions read where the reference places each point: around the
/// pole (from any meridian), either side of the antimeridian, over
/// Svalbard, the nearest node a third of a node past the outer row, and
/// null off the footprint, never an edge cell.
#[test]
fn edr_positions_read_where_the_reference_places_them() {
    let engine = engine();
    for (name, lon, lat, expected) in POINTS {
        let read = edr(&engine, lon, lat);
        assert!(
            close(read, expected, 1e-3),
            "{name} ({lon}, {lat}): {read:?}, reference {expected:?}"
        );
    }
}

/// A map pixel and an EDR value at the same point are the same number
/// (the crate's rule): a one-pixel map centred on each point draws what
/// the position query reads, narrowed to `f32`.
#[test]
fn a_map_pixel_is_what_edr_reads_there() {
    let engine = engine();
    let d = 1e-4;
    for (name, lon, lat, _) in POINTS {
        let lat = lat.min(90.0 - d);
        let drawn = pixel(
            &draw(
                &engine,
                [lon - d, lat - d, lon + d, lat + d],
                1,
                1,
                &OutputCrs::Wgs84,
            ),
            0,
        );
        let read = edr(&engine, lon, lat).map(|(c, r)| (c as f32 as f64, r as f32 as f64));
        assert!(
            close(drawn, read, 1e-4),
            "{name}: map {drawn:?}, EDR {read:?}"
        );
    }
}

/// Map pixels land where the reference places their centres, a CRS84 map
/// across the antimeridian (given either way round) and an EPSG:3857 map
/// over Svalbard alike, within a tenth of a cell: the lookups are made
/// only at the coarse grid's nodes (Critical Rule 5) and the pixels
/// between interpolated, within its 0.2-cell budget (here 0.003 on the
/// CRS84 map, 0.034 on the Mercator one, 10° of longitude per coarse
/// cell). Off the footprint is empty; the half cell past the outer edges
/// reads the nearest node.
#[test]
fn map_pixels_land_where_the_reference_places_them() {
    let engine = engine();
    let check = |tiles: &[RasterTile; 2], width: usize, pins: &Pins| {
        for &(ox, oy, expected) in pins {
            let drawn = pixel(tiles, oy * width + ox);
            assert!(
                close(drawn, expected, 0.1),
                "pixel ({ox}, {oy}): {drawn:?}, reference {expected:?}"
            );
        }
    };
    let seam = draw(
        &engine,
        [170.0, 80.0, -170.0, 88.0],
        20,
        12,
        &OutputCrs::Wgs84,
    );
    check(&seam, 20, &CRS84_PINS);
    let unwrapped = draw(
        &engine,
        [170.0, 80.0, 190.0, 88.0],
        20,
        12,
        &OutputCrs::Wgs84,
    );
    for i in 0..240 {
        assert_eq!(pixel(&seam, i), pixel(&unwrapped, i), "pixel {i}");
    }
    let mercator = draw(
        &engine,
        [0.0, 73.0, 40.0, 82.0],
        32,
        24,
        &OutputCrs::WebMercator,
    );
    check(&mercator, 32, &MERCATOR_PINS);
    // The whole world at 2.5°: only the cap north of 72.5°N is drawn, the
    // top row (88.75°N) whole; nothing south of the footprint is the edge
    // cell.
    let world = draw(
        &engine,
        [-180.0, -90.0, 180.0, 90.0],
        144,
        72,
        &OutputCrs::Wgs84,
    );
    for oy in 0..72 {
        let lat = 90.0 - (oy as f64 + 0.5) * 2.5;
        let drawn = (0..144)
            .filter(|&ox| pixel(&world, oy * 144 + ox).is_some())
            .count();
        match lat {
            l if l < 72.5 => assert_eq!(drawn, 0, "{lat}°N"),
            l if l > 87.0 => assert_eq!(drawn, 144, "{lat}°N"),
            _ => {}
        }
    }
}

/// The WMS meta-tile path draws an EPSG:3857 view from 256 × 256 tiles of
/// the fixed Web Mercator grid, each rendered by `get_raster_tile` over its
/// own bounds, and crops them; the direct path renders the view's bounds
/// at once. Both place every pixel at the same source position, to within
/// their coarse grids' interpolation (a tenth of a cell; 0.033 measured,
/// against a budget of 0.2 each): here the zoom 3 tile over Svalbard
/// (0…45°E, 66.5…79.2°N), against a direct render of its pixels 50…177
/// across and 10…137 down, which the grid's southern edge crosses: off the
/// tile's 32-pixel coarse grid, so the two interpolate between different
/// lookups.
#[test]
fn the_meta_tile_and_the_direct_path_place_data_alike() {
    let engine = engine();
    let span = 2.0 * std::f64::consts::PI * EARTH_RADIUS / 8.0;
    let origin = std::f64::consts::PI * EARTH_RADIUS;
    let (col, row) = (4.0, 1.0);
    let (x0, y0) = (-origin + col * span, origin - row * span);
    assert!(lat_to_y(78.2) < y0 && lat_to_y(78.2) > y0 - span);
    let px = span / 256.0;
    let bounds = |(x_lo, x_hi): (f64, f64), (y_lo, y_hi): (f64, f64)| {
        [
            x_to_lon(x0 + x_lo * px),
            y_to_lat(y0 - y_hi * px),
            x_to_lon(x0 + x_hi * px),
            y_to_lat(y0 - y_lo * px),
        ]
    };
    let full = (0.0, 256.0);
    let tile = draw(
        &engine,
        bounds(full, full),
        256,
        256,
        &OutputCrs::WebMercator,
    );
    let view = draw(
        &engine,
        bounds((50.0, 178.0), (10.0, 138.0)),
        128,
        128,
        &OutputCrs::WebMercator,
    );
    // Past the outer nodes the sampler reads the nearest node, a step
    // apart either side of its middle: only positions between nodes are
    // compared, and drawing at all along the edge.
    let edge = |v: f64, last: f64| v.abs() < 1e-6 || (v - last).abs() < 1e-6;
    let between = |p: (f64, f64)| p.0 > 0.1 && p.0 < 17.9 && p.1 > 0.1 && p.1 < 26.9;
    let (mut compared, mut worst) = (0, 0.0_f64);
    for vy in 0..128 {
        for vx in 0..128 {
            let a = pixel(&tile, (vy + 10) * 256 + vx + 50);
            let b = pixel(&view, vy * 128 + vx);
            match (a, b) {
                (Some(a), Some(b)) if between(a) && between(b) => {
                    worst = worst.max((a.0 - b.0).abs()).max((a.1 - b.1).abs());
                    compared += 1;
                }
                (Some(_), Some(_)) | (None, None) => {}
                // Either side of the footprint's cut, by interpolation.
                (Some(p), None) | (None, Some(p)) => {
                    assert!(
                        edge(p.0, 18.0) || edge(p.1, 27.0),
                        "({vx}, {vy}) drawn by one path only, at {p:?}"
                    );
                }
            }
        }
    }
    assert!(compared > 4000, "{compared} pixels compared");
    assert!(worst < 0.1, "worst {worst} cells apart");
}

/// An EDR area samples each cell centre as a position query does, a
/// circle around the pole included (every cell on the grid), and radius
/// goes through it; a cube's cells too, and a trajectory over the pole and
/// across the antimeridian reads each sample as a position.
#[test]
fn area_radius_cube_and_trajectory_sample_as_positions_do() {
    let engine = engine();
    let grid_cells = |result: &QueryResult| {
        let DomainDescription::Grid { x, y, .. } = &result.domain else {
            panic!("not a Grid: {:?}", result.domain);
        };
        let (x, y) = (x.clone(), y.clone());
        let column = result.ranges["column"].values.clone();
        let row = result.ranges["row"].values.clone();
        let mut cells = Vec::new();
        for (iy, &lat) in y.iter().enumerate() {
            for (ix, &lon) in x.iter().enumerate() {
                let k = iy * x.len() + ix;
                cells.push((lon, lat, column[k].zip(row[k])));
            }
        }
        cells
    };
    let check = |cells: &[(f64, f64, Read)], what: &str| {
        let mut on = 0;
        for &(lon, lat, value) in cells {
            if let Some(value) = value {
                assert_eq!(edr(&engine, lon, lat), Some(value), "{what} ({lon}, {lat})");
                on += 1;
            }
        }
        on
    };
    let svalbard = single(
        engine
            .query_area(
                "POLYGON((10 77,22 77,22 80,10 80,10 77))",
                None,
                Some(&fields()),
                None,
                None,
            )
            .unwrap(),
    );
    // At the extent's mean cell size in degrees, as every gridded
    // engine's area: around a pole that is 360° / 19 columns of longitude.
    assert!(check(&grid_cells(&svalbard), "area") >= 3);
    let cap = single(
        engine
            .query_area("-180,88,180,90", None, Some(&fields()), None, None)
            .unwrap(),
    );
    let cells = grid_cells(&cap);
    assert!(
        cells.iter().all(|(.., v)| v.is_some()),
        "the cap is on the grid"
    );
    assert_eq!(check(&cells, "polar cap"), cells.len());
    let radius = single(
        engine
            .query_radius(
                "POINT(15.633 78.217)",
                60_000.0,
                None,
                Some(&fields()),
                None,
                None,
            )
            .unwrap(),
    );
    assert!(check(&grid_cells(&radius), "radius") > 0);
    let cube = single(
        engine
            .query_cube(
                &Bbox::new(170.0, 82.0, -170.0, 86.0).unwrap(),
                None,
                Some(&fields()),
                None,
                CubeResolution::default(),
                None,
            )
            .unwrap(),
    );
    let DomainDescription::Grid { t, .. } = &cube.domain else {
        panic!("not a Grid: {:?}", cube.domain);
    };
    assert_eq!(t.as_ref().map(Vec::len), Some(1));
    assert!(check(&grid_cells(&cube), "cube") > 0);

    let path = single(
        engine
            .query_trajectory(
                "LINESTRING(15 78,15 89,-165 85,175 84)",
                None,
                Some(&fields()),
                None,
                None,
            )
            .unwrap(),
    );
    let DomainDescription::Trajectory { nodes, .. } = &path.domain else {
        panic!("not a Trajectory: {:?}", path.domain);
    };
    let (column, row) = (&path.ranges["column"].values, &path.ranges["row"].values);
    assert!(nodes.len() > 20, "{} samples", nodes.len());
    for (k, &(_, lon, lat)) in nodes.iter().enumerate() {
        let value = column[k].zip(row[k]).expect("the path lies on the grid");
        assert_eq!(
            edr(&engine, lon, lat),
            Some(value),
            "sample {k} ({lon}, {lat})"
        );
    }
    // A polygon outside the extent is not found.
    assert!(matches!(
        engine.query_area(
            "POLYGON((0 50,10 50,10 60,0 60,0 50))",
            None,
            Some(&fields()),
            None,
            None
        ),
        Err(DataServerError::LocationNotFound(_))
    ));
}
