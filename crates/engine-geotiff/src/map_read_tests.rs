//! #1540: the map path in the raster's own longitude frame.
//!
//! A raster stored from 0° to 360° draws what one stored from 180°W draws,
//! in every view across either raster's seam, and each pixel is the value
//! the EDR position query, which WMS GetFeatureInfo asks, reads at its
//! centre. So does a Lambert cone centred on 180°, on both sides of it. A
//! raster stored from 180°W renders each view inside ±180° as it did
//! before, pinned by the FNV-1a hashes of its tiles as the map path drew
//! them before #1540.

use ds_core::geo::{projected_output_crs, wgs84_envelope, Crs, GeoTransform};
use ds_core::map_engine::{MapEngine, OutputCrs, RasterTile, RasterValues};
use ds_core::web_mercator::{x_to_lon, y_to_lat, EARTH_RADIUS};

use crate::area_tests::{geographic, lcc, Fixture, Level, Samples};
use crate::map_read::MapRead;
use crate::GeoTiffEngine;

/// The pixel of a global fixture's full resolution: 384 columns a turn.
const GLOBAL_PIXEL: f64 = 360.0 / 384.0;

/// A global geographic fixture of 384 × 192 pixels from `west`, 90°N, each
/// pixel holding a value of its place on the Earth, whatever the frame it
/// is stored in: float32 `row × 384 + c` with `c` its column counted from
/// 180°W, bytes `(7c + 3 row) mod 250`. With `overview`, an overview of
/// 192 × 128 too, its values by the same rule a million on (bytes: 50 on).
fn global(name: &str, west: f64, bytes: bool, overview: bool) -> Fixture {
    let level = |columns: u32, rows: u32, level: u32| {
        let pixel = 360.0 / f64::from(columns);
        let offset = ((west + 180.0) / pixel).round() as u32;
        let place = |i: u32| {
            let (row, col) = (i / columns, i % columns);
            (row, (col + offset) % columns)
        };
        let samples = if bytes {
            Samples::U8(
                (0..columns * rows)
                    .map(|i| {
                        let (row, col) = place(i);
                        ((7 * col + 3 * row + 50 * level) % 250) as u8
                    })
                    .collect(),
            )
        } else {
            Samples::F32(
                (0..columns * rows)
                    .map(|i| {
                        let (row, col) = place(i);
                        (row * columns + col + level * 1_000_000) as f32
                    })
                    .collect(),
            )
        };
        Level {
            dims: (columns, rows),
            samples,
        }
    };
    let mut levels = vec![level(384, 192, 0)];
    if overview {
        levels.push(level(192, 128, 1));
    }
    Fixture::levels(name, geographic(), (west, 90.0), GLOBAL_PIXEL, &levels)
}

/// A local engine over a committed fixture directory.
fn committed(id: &str, dir: &str, template: &str, parameter: &str) -> GeoTiffEngine {
    let config: ds_core::config::GeoTiffConfig = serde_json::from_value(serde_json::json!({
        "filename_template": template, "parameter": parameter, "unit": "1"
    }))
    .unwrap();
    GeoTiffEngine::new(id, Some(dir), &config).expect("engine over a committed fixture")
}

/// The WGS84 box of a WMS Web Mercator meta-tile: `level` of the
/// half-octave ladder, `2^(level/2)` tiles a world, longitudes unwrapped
/// past ±180° as `ds_render::metatile` passes them.
fn meta_tile(level: i32, col: i64, row: i64) -> [f64; 4] {
    let origin = std::f64::consts::PI * EARTH_RADIUS;
    let span = 2.0 * origin / 2f64.powf(f64::from(level) / 2.0);
    let (x0, top) = (-origin + col as f64 * span, origin - row as f64 * span);
    [
        x_to_lon(x0),
        y_to_lat(top - span),
        x_to_lon(x0 + span),
        y_to_lat(top),
    ]
}

/// The WGS84 box of WebMercatorQuad tile `z/x/y`.
fn quad_tile(z: u32, x: i64, y: i64) -> [f64; 4] {
    meta_tile(2 * z as i32, x, y)
}

/// A projected view: the read envelope WMS passes with its output CRS.
fn projected(code: &str, bbox: [f64; 4]) -> ([f64; 4], OutputCrs) {
    let crs = projected_output_crs(code).unwrap();
    let envelope = wgs84_envelope(&crs, bbox).expect("a read envelope");
    (envelope, OutputCrs::Projected { crs, bbox })
}

/// A view: what it is, its WGS84 box, its output CRS and its size.
type View = (&'static str, [f64; 4], OutputCrs, (u32, u32));

/// Views inside the CRS84 frame, in every output CRS the map APIs ask for.
fn in_frame_views() -> Vec<View> {
    let (laea, laea_crs) = projected(
        "EPSG:3035",
        [3_000_000.0, 2_000_000.0, 5_500_000.0, 4_500_000.0],
    );
    let (tm, tm_crs) = projected("EPSG:3067", [0.0, 6_600_000.0, 800_000.0, 7_800_000.0]);
    vec![
        (
            "CRS:84 world",
            [-180.0, -90.0, 180.0, 90.0],
            OutputCrs::Wgs84,
            (512, 256),
        ),
        (
            "CRS:84 world, small",
            [-180.0, -90.0, 180.0, 90.0],
            OutputCrs::Wgs84,
            (256, 128),
        ),
        (
            "CRS:84 Europe",
            [-10.0, 35.0, 40.0, 72.0],
            OutputCrs::Wgs84,
            (256, 256),
        ),
        (
            "CRS:84 Finland, retina",
            [19.0, 59.0, 32.0, 71.0],
            OutputCrs::Wgs84,
            (1300, 1200),
        ),
        (
            "CRS:84 Pacific",
            [120.0, -40.0, 179.0, 40.0],
            OutputCrs::Wgs84,
            (300, 400),
        ),
        (
            "CRS:84 Americas",
            [-170.0, -60.0, -30.0, 70.0],
            OutputCrs::Wgs84,
            (350, 330),
        ),
        (
            "meta-tile 0/0/0",
            meta_tile(0, 0, 0),
            OutputCrs::WebMercator,
            (256, 256),
        ),
        (
            "meta-tile 0/0/0, small",
            meta_tile(0, 0, 0),
            OutputCrs::WebMercator,
            (128, 128),
        ),
        (
            "meta-tile 3/1/0",
            meta_tile(3, 1, 0),
            OutputCrs::WebMercator,
            (256, 256),
        ),
        (
            "meta-tile 7/6/2",
            meta_tile(7, 6, 2),
            OutputCrs::WebMercator,
            (256, 256),
        ),
        (
            "meta-tile 8/9/3",
            meta_tile(8, 9, 3),
            OutputCrs::WebMercator,
            (256, 256),
        ),
        (
            "quad 1/0/0",
            quad_tile(1, 0, 0),
            OutputCrs::WebMercator,
            (256, 256),
        ),
        (
            "quad 2/3/1",
            quad_tile(2, 3, 1),
            OutputCrs::WebMercator,
            (256, 256),
        ),
        (
            "quad 4/9/4",
            quad_tile(4, 9, 4),
            OutputCrs::WebMercator,
            (512, 512),
        ),
        ("EPSG:3035 Europe", laea, laea_crs, (500, 500)),
        ("EPSG:3067 Finland", tm, tm_crs, (400, 600)),
    ]
}

/// The FNV-1a hash of a tile's representation and values.
fn tile_hash(tile: &RasterTile) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for &b in bytes {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    eat(&tile.width.to_le_bytes());
    eat(&tile.height.to_le_bytes());
    match &tile.values {
        RasterValues::F64(values) => {
            eat(b"f64");
            for value in values {
                match value {
                    Some(v) => eat(&v.to_bits().to_le_bytes()),
                    None => eat(b"-"),
                }
            }
        }
        RasterValues::U8 {
            data,
            nodata,
            gain,
            offset,
        } => {
            eat(b"u8");
            eat(data);
            eat(&nodata.map_or(256, u16::from).to_le_bytes());
            eat(&gain.to_bits().to_le_bytes());
            eat(&offset.to_bits().to_le_bytes());
        }
        RasterValues::F32 { data, nodata } => {
            eat(b"f32");
            for v in data {
                eat(&v.to_bits().to_le_bytes());
            }
            eat(&nodata.map_or(u32::MAX, f32::to_bits).to_le_bytes());
        }
    }
    hash
}

/// A render with the source projections its coarse grid made.
fn render_counted(
    engine: &GeoTiffEngine,
    bbox: [f64; 4],
    crs: &OutputCrs,
    size: (u32, u32),
) -> (RasterTile, usize) {
    crate::SOURCE_PROJECTIONS.with(|n| n.set(0));
    let tile = render(engine, bbox, crs, size);
    (tile, crate::SOURCE_PROJECTIONS.with(|n| n.get()))
}

fn render(
    engine: &GeoTiffEngine,
    bbox: [f64; 4],
    crs: &OutputCrs,
    (w, h): (u32, u32),
) -> RasterTile {
    engine
        .get_raster_tile(bbox, w, h, None, crs, None, None, None)
        .expect("render")
}

/// A raster stored from 180°W, or regional inside ±180°, renders each
/// in-frame view as it did before #1540, in every representation (boxed,
/// bytes) and on every level: the hashes are of the tiles the map path drew
/// before it learned the raster's frame.
#[test]
fn a_raster_inside_the_crs84_frame_renders_as_before() {
    let global_f32 = global("golden-f32", -180.0, false, true);
    let global_u8 = global("golden-u8", -180.0, true, false);
    let cone = Fixture::new(
        "golden-lcc",
        lcc(50.0, 60.0, 55.0, 25.0),
        [10.0, 50.0, 40.0, 70.0],
        20_000.0,
    );
    assert!(matches!(
        cone.geo_transform().crs,
        Crs::LambertConformalConic { .. }
    ));
    let europe = committed(
        "golden-europe",
        "../../testdata/radar",
        "radar_%Y%m%dT%H%MZ.tif",
        "dbz",
    );
    let tm35fin = committed(
        "golden-tm35fin",
        "../../testdata/radar-tm35fin",
        "radar_tm35_%Y%m%dT%H%MZ.tif",
        "dbz",
    );
    let engines: [(&str, &GeoTiffEngine); 5] = [
        ("global float32", &global_f32.engine),
        ("global bytes", &global_u8.engine),
        ("Lambert at 25°E", &cone.engine),
        ("committed Europe", &europe),
        ("committed TM35FIN", &tm35fin),
    ];
    let mut hashes = Vec::new();
    for (name, engine) in engines {
        for (view, bbox, crs, size) in in_frame_views() {
            hashes.push((name, view, tile_hash(&render(engine, bbox, &crs, size))));
        }
    }
    assert_eq!(hashes.len(), BEFORE.len());
    for (got, want) in hashes.iter().zip(BEFORE) {
        assert_eq!(got, want, "{} {} moved pixels", want.0, want.1);
    }
}

/// The tiles' hashes as the map path drew them before #1540.
const BEFORE: &[(&str, &str, u64)] = &[
    ("global float32", "CRS:84 world", 0x9922afe99607533c),
    ("global float32", "CRS:84 world, small", 0xd533211145769f1b),
    ("global float32", "CRS:84 Europe", 0x8004a14e4a1094ab),
    (
        "global float32",
        "CRS:84 Finland, retina",
        0xd1705c07339734cf,
    ),
    ("global float32", "CRS:84 Pacific", 0xe4274e827bca4697),
    ("global float32", "CRS:84 Americas", 0x26223c5a644f01c2),
    ("global float32", "meta-tile 0/0/0", 0x3c894f9d2ee6444e),
    (
        "global float32",
        "meta-tile 0/0/0, small",
        0x700e84b8c1541cb2,
    ),
    ("global float32", "meta-tile 3/1/0", 0x64f2bc9a3409bd2a),
    ("global float32", "meta-tile 7/6/2", 0x52cea4fabb1376d9),
    ("global float32", "meta-tile 8/9/3", 0xec7fe32c8c6cb393),
    ("global float32", "quad 1/0/0", 0x1cbed703f6b3c200),
    ("global float32", "quad 2/3/1", 0x056a17fda6eb3cdb),
    ("global float32", "quad 4/9/4", 0xea2a1f3bd4c78b33),
    ("global float32", "EPSG:3035 Europe", 0x0859c2c837dcbb2c),
    ("global float32", "EPSG:3067 Finland", 0xdd2809de07e507d4),
    ("global bytes", "CRS:84 world", 0x2ceb334ffe618795),
    ("global bytes", "CRS:84 world, small", 0xe98369544d7f6d2b),
    ("global bytes", "CRS:84 Europe", 0xcde9ec7b0d2fdb7c),
    ("global bytes", "CRS:84 Finland, retina", 0xf442f2a757644d99),
    ("global bytes", "CRS:84 Pacific", 0xa4f72d196505821c),
    ("global bytes", "CRS:84 Americas", 0x4bda8040b1a42a50),
    ("global bytes", "meta-tile 0/0/0", 0x59c42c297aa96a28),
    ("global bytes", "meta-tile 0/0/0, small", 0x36e5fe02997e3272),
    ("global bytes", "meta-tile 3/1/0", 0xe69af353deea6442),
    ("global bytes", "meta-tile 7/6/2", 0x35808ff421657f00),
    ("global bytes", "meta-tile 8/9/3", 0xe155311a0e8663be),
    ("global bytes", "quad 1/0/0", 0x631230489f009fee),
    ("global bytes", "quad 2/3/1", 0x5a97bcb8ea94f57a),
    ("global bytes", "quad 4/9/4", 0x45709dc875ed8444),
    ("global bytes", "EPSG:3035 Europe", 0xfea3bf58092c7192),
    ("global bytes", "EPSG:3067 Finland", 0xb94a39e076a9bc2e),
    ("Lambert at 25°E", "CRS:84 world", 0x94ff47ebba69a3c7),
    ("Lambert at 25°E", "CRS:84 world, small", 0x91e18d9d46991dd7),
    ("Lambert at 25°E", "CRS:84 Europe", 0xca623c48319f4899),
    (
        "Lambert at 25°E",
        "CRS:84 Finland, retina",
        0xe1c0f601612ed725,
    ),
    ("Lambert at 25°E", "CRS:84 Pacific", 0xc5a7971b485d669f),
    ("Lambert at 25°E", "CRS:84 Americas", 0x319b28778798a5bf),
    ("Lambert at 25°E", "meta-tile 0/0/0", 0xfd5a51f59b5c0482),
    (
        "Lambert at 25°E",
        "meta-tile 0/0/0, small",
        0x67411b1f4b1f94a5,
    ),
    ("Lambert at 25°E", "meta-tile 3/1/0", 0x16efe70b15851181),
    ("Lambert at 25°E", "meta-tile 7/6/2", 0xd172186a907175f1),
    ("Lambert at 25°E", "meta-tile 8/9/3", 0x67c287262c59b2ab),
    ("Lambert at 25°E", "quad 1/0/0", 0xaf9bc2b5c19721fa),
    ("Lambert at 25°E", "quad 2/3/1", 0x2903998c07ea31bb),
    ("Lambert at 25°E", "quad 4/9/4", 0x0598f16742de2ca7),
    ("Lambert at 25°E", "EPSG:3035 Europe", 0x49cec5204fb179e2),
    ("Lambert at 25°E", "EPSG:3067 Finland", 0xf670be646ac734f5),
    ("committed Europe", "CRS:84 world", 0xc121f7909e8a37ef),
    (
        "committed Europe",
        "CRS:84 world, small",
        0xd1dd1a954fa05452,
    ),
    ("committed Europe", "CRS:84 Europe", 0xd4d8c77779b65027),
    (
        "committed Europe",
        "CRS:84 Finland, retina",
        0xb71086bb4bd91841,
    ),
    ("committed Europe", "CRS:84 Pacific", 0xc5a7971b485d669f),
    ("committed Europe", "CRS:84 Americas", 0x319b28778798a5bf),
    ("committed Europe", "meta-tile 0/0/0", 0x288fb095123feedb),
    (
        "committed Europe",
        "meta-tile 0/0/0, small",
        0x360616e40b90b217,
    ),
    ("committed Europe", "meta-tile 3/1/0", 0x63faf0b56534d74e),
    ("committed Europe", "meta-tile 7/6/2", 0x5a1c2d204ce81265),
    ("committed Europe", "meta-tile 8/9/3", 0xedb0744dfc152cd9),
    ("committed Europe", "quad 1/0/0", 0x2903998c07ea31bb),
    ("committed Europe", "quad 2/3/1", 0x2903998c07ea31bb),
    ("committed Europe", "quad 4/9/4", 0x7bd36352bdc070eb),
    ("committed Europe", "EPSG:3035 Europe", 0xe5aa7392db91659a),
    ("committed Europe", "EPSG:3067 Finland", 0xf1a1f47f786330bc),
    ("committed TM35FIN", "CRS:84 world", 0x8e21ab35d09c96f3),
    (
        "committed TM35FIN",
        "CRS:84 world, small",
        0x07f5f3c1620f2316,
    ),
    ("committed TM35FIN", "CRS:84 Europe", 0xe21f5612fa4d2583),
    (
        "committed TM35FIN",
        "CRS:84 Finland, retina",
        0xcbc1f621b15b3b40,
    ),
    ("committed TM35FIN", "CRS:84 Pacific", 0xc5a7971b485d669f),
    ("committed TM35FIN", "CRS:84 Americas", 0x319b28778798a5bf),
    ("committed TM35FIN", "meta-tile 0/0/0", 0xe7f98737acfbf87d),
    (
        "committed TM35FIN",
        "meta-tile 0/0/0, small",
        0x5b2df6a4fe31861e,
    ),
    ("committed TM35FIN", "meta-tile 3/1/0", 0x95210c20ff9f593f),
    ("committed TM35FIN", "meta-tile 7/6/2", 0x4c74fff3e186fee3),
    ("committed TM35FIN", "meta-tile 8/9/3", 0x4d0ab23a42363f0f),
    ("committed TM35FIN", "quad 1/0/0", 0x2903998c07ea31bb),
    ("committed TM35FIN", "quad 2/3/1", 0x2903998c07ea31bb),
    ("committed TM35FIN", "quad 4/9/4", 0xaea62e96cd597801),
    ("committed TM35FIN", "EPSG:3035 Europe", 0xeddf8e000607920f),
    ("committed TM35FIN", "EPSG:3067 Finland", 0xd88f6e11e19c1bed),
];

/// The pixel-centre `(lon, lat)` of output pixel `(ox, oy)` of a `w` × `h`
/// map of `bbox` in `crs`, the box unwrapped as the engine takes it.
fn centre(bbox: [f64; 4], crs: &OutputCrs, (w, h): (u32, u32), ox: u32, oy: u32) -> (f64, f64) {
    let [west, south, east, north] = bbox;
    let east = if west > east { east + 360.0 } else { east };
    let (fx, fy) = (
        (f64::from(ox) + 0.5) / f64::from(w),
        (f64::from(oy) + 0.5) / f64::from(h),
    );
    crs.project_node([west, south, east, north], fx, fy)
}

/// Whether a pixel centred at `(lon, lat)` may take a source pixel of `gt`
/// other than the one its exact position lies in: when that position lies
/// on a pixel edge, where the floor of a rounding error crosses it, or
/// where the map is not affine, within the coarse grid's interpolation
/// budget, 0.2 pixels, plus the slack between its error probes ([`EDGE`]),
/// of one. A geographic source's columns are affine in a CRS84 or Web
/// Mercator map's, its rows in a CRS84 map's.
fn near_an_edge(gt: &GeoTransform, crs: &OutputCrs, lon: f64, lat: f64) -> bool {
    let (col, row) = gt.world_to_pixel_f64(crate::area::source_longitude(gt, lon), lat);
    let geographic = matches!(gt.crs, Crs::Wgs84);
    let (col_edge, row_edge) = match crs {
        OutputCrs::Wgs84 if geographic => (ON_EDGE, ON_EDGE),
        OutputCrs::WebMercator if geographic => (ON_EDGE, EDGE),
        _ => (EDGE, EDGE),
    };
    (col - col.round()).abs() < col_edge || (row - row.round()).abs() < row_edge
}

/// Within this of a pixel edge, a pixel of a map that is not affine in the
/// source's pixels may take the pixel across it (see [`near_an_edge`]).
const EDGE: f64 = 0.3;
/// On a pixel edge, but for rounding.
const ON_EDGE: f64 = 1e-9;

/// Every `step`-th pixel of `tile`, a `size` map of `bbox` in `crs` drawn
/// from full resolution, against the EDR position query at its centre: the
/// value WMS GetFeatureInfo answers there. Equal, or the centre lies
/// [`near_an_edge`]. Returns how many of them are drawn.
fn assert_draws_the_position(
    fixture: &Fixture,
    tile: &RasterTile,
    (what, bbox, crs, size): (&str, [f64; 4], &OutputCrs, (u32, u32)),
    step: u32,
) -> usize {
    let gt = fixture.geo_transform();
    let mut drawn = 0;
    for oy in (0..size.1).step_by(step as usize) {
        for ox in (0..size.0).step_by(step as usize) {
            let value = tile.values.value_at((oy * size.0 + ox) as usize);
            let (lon, lat) = centre(bbox, crs, size, ox, oy);
            drawn += usize::from(value.is_some());
            let position = fixture.position(lon, lat);
            assert!(
                value == position || near_an_edge(&gt, crs, lon, lat),
                "{what}: pixel ({ox}, {oy}) at ({lon}, {lat}) draws {value:?}, \
                 the position reads {position:?}"
            );
        }
    }
    drawn
}

/// Every pixel of `a` and `b`, two `size` maps of `bbox` in `crs`, equal,
/// or its centre [`near_an_edge`] of `gt`, the level both drew; and every
/// pixel drawn.
fn assert_same_and_full(
    a: &RasterTile,
    b: &RasterTile,
    gt: &GeoTransform,
    (what, bbox, crs, size): (&str, [f64; 4], &OutputCrs, (u32, u32)),
) {
    for i in 0..(size.0 * size.1) as usize {
        let (va, vb) = (a.values.value_at(i), b.values.value_at(i));
        assert!(va.is_some(), "{what}: pixel {i} empty on a global raster");
        let (ox, oy) = (i as u32 % size.0, i as u32 / size.0);
        let (lon, lat) = centre(bbox, crs, size, ox, oy);
        assert!(
            va == vb || near_an_edge(gt, crs, lon, lat),
            "{what}: pixel {i} at ({lon}, {lat}) draws {va:?} against {vb:?}"
        );
    }
}

/// An EPSG:3035 view across 180°, south of the Bering Strait: 600 km
/// square round (180°, 60°N), its read envelope from 170°E to 190°E in the
/// inverse's own frame.
fn bering() -> ([f64; 4], OutputCrs) {
    let laea = projected_output_crs("EPSG:3035").unwrap();
    let (x, y) = laea.forward(180.0, 60.0);
    let view = projected(
        "EPSG:3035",
        [x - 300_000.0, y - 300_000.0, x + 300_000.0, y + 300_000.0],
    );
    let [west, _, east, _] = view.0;
    assert!(west < 180.0 && east > 180.0 && east < 190.0, "{:?}", view.0);
    view
}

/// The views across a seam of a raster stored from 180°W or from 0°, in
/// each form and output CRS a map API hands the engine: CRS84 boxes, OGC
/// API Maps' unwrapped one and `west > east`, world copies, WMS Web
/// Mercator meta-tiles either side of ±180°, WebMercatorQuad tiles either
/// side of 0° and an EPSG:3035 view across 180°.
fn seam_views() -> Vec<View> {
    let (bering, bering_crs) = bering();
    let merc = OutputCrs::WebMercator;
    vec![
        (
            "CRS:84 western hemisphere",
            [-170.0, 10.0, -160.0, 20.0],
            OutputCrs::Wgs84,
            (64, 64),
        ),
        (
            "CRS:84 across 0°",
            [-10.0, -5.0, 10.0, 5.0],
            OutputCrs::Wgs84,
            (64, 32),
        ),
        (
            "CRS:84 seam, unwrapped",
            [170.0, 10.0, 190.0, 20.0],
            OutputCrs::Wgs84,
            (64, 32),
        ),
        (
            "CRS:84 seam, west > east",
            [170.0, 10.0, -170.0, 20.0],
            OutputCrs::Wgs84,
            (64, 32),
        ),
        (
            "CRS:84 a turn on",
            [190.0, 10.0, 200.0, 20.0],
            OutputCrs::Wgs84,
            (64, 64),
        ),
        (
            "CRS:84 three turns",
            [-540.0, -60.0, 540.0, 60.0],
            OutputCrs::Wgs84,
            (360, 40),
        ),
        (
            "meta-tile 4/-1/1",
            meta_tile(4, -1, 1),
            merc.clone(),
            (64, 64),
        ),
        (
            "meta-tile 4/0/1",
            meta_tile(4, 0, 1),
            merc.clone(),
            (64, 64),
        ),
        (
            "meta-tile 4/3/1",
            meta_tile(4, 3, 1),
            merc.clone(),
            (64, 64),
        ),
        (
            "meta-tile 4/4/1",
            meta_tile(4, 4, 1),
            merc.clone(),
            (64, 64),
        ),
        (
            "meta-tile 8/15/7",
            meta_tile(8, 15, 7),
            merc.clone(),
            (64, 64),
        ),
        (
            "meta-tile 8/16/7",
            meta_tile(8, 16, 7),
            merc.clone(),
            (64, 64),
        ),
        ("quad 1/0/0", quad_tile(1, 0, 0), merc.clone(), (64, 64)),
        ("quad 1/1/1", quad_tile(1, 1, 1), merc, (64, 64)),
        ("EPSG:3035 across 180°", bering, bering_crs, (64, 64)),
    ]
}

/// A raster stored from 0° to 360° draws every view across either seam as
/// one stored from 180°W draws it, every pixel, in floats and through the
/// byte path, and from the same overview: each level counts the pixels of
/// the windows it reads in its own frame. Before, the first drew nothing
/// west of 0°, and both drew nothing past their own east edge, such as
/// 180°–190°E of the unwrapped seam box on the second.
#[test]
fn a_raster_from_0_draws_what_one_from_180w_draws() {
    for bytes in [false, true] {
        let from_west = global(&format!("from-west-{bytes}"), -180.0, bytes, false);
        let from_0 = global(&format!("from-0-{bytes}"), 0.0, bytes, false);
        for (what, bbox, crs, size) in seam_views() {
            let a = render(&from_west.engine, bbox, &crs, size);
            let b = render(&from_0.engine, bbox, &crs, size);
            assert_eq!(
                matches!(a.values, RasterValues::U8 { .. }),
                bytes,
                "{what}: the byte path"
            );
            assert_eq!(matches!(b.values, RasterValues::U8 { .. }), bytes, "{what}");
            let gt = from_west.geo_transform();
            assert_same_and_full(&a, &b, &gt, (what, bbox, &crs, size));
        }
    }

    // Small enough for the overview: both rasters read it.
    let from_west = global("overview-from-west", -180.0, false, true);
    let from_0 = global("overview-from-0", 0.0, false, true);
    let overview = GeoTransform {
        pixel_width: 360.0 / 192.0,
        pixel_height: 180.0 / 128.0,
        width: 192,
        height: 128,
        ..from_west.geo_transform()
    };
    let views: [View; 3] = [
        (
            "CRS:84 world",
            [-180.0, -90.0, 180.0, 90.0],
            OutputCrs::Wgs84,
            (256, 128),
        ),
        (
            "CRS:84 three turns",
            [-540.0, -60.0, 540.0, 60.0],
            OutputCrs::Wgs84,
            (360, 40),
        ),
        (
            "meta-tile 0/0/0",
            meta_tile(0, 0, 0),
            OutputCrs::WebMercator,
            (128, 128),
        ),
    ];
    for (what, bbox, crs, size) in views {
        let a = render(&from_west.engine, bbox, &crs, size);
        let b = render(&from_0.engine, bbox, &crs, size);
        for tile in [&a, &b] {
            assert!(
                tile.values.iter_values().flatten().all(|v| v >= 1e6),
                "{what}: drawn from the overview"
            );
        }
        assert_same_and_full(&a, &b, &overview, (what, bbox, &crs, size));
    }
}

/// Each pixel of a map across a seam is the value the EDR position query,
/// which WMS GetFeatureInfo asks (#1064), reads at its centre: on a raster
/// stored from 0° and on one stored from 180°W, in floats and bytes.
#[test]
fn a_map_across_a_seam_draws_the_edr_position() {
    for (west, bytes) in [(0.0, false), (0.0, true), (-180.0, false)] {
        let fixture = global(&format!("position-{west}-{bytes}"), west, bytes, false);
        for (what, bbox, crs, size) in seam_views() {
            let tile = render(&fixture.engine, bbox, &crs, size);
            let step = if size.0 > 64 { 4 } else { 2 };
            let what = format!("from {west}°, bytes {bytes}, {what}");
            let drawn = assert_draws_the_position(&fixture, &tile, (&what, bbox, &crs, size), step);
            assert!(drawn > 0, "{what} draws nothing");
        }
    }
}

/// A view across a raster's seam reads two windows, its last columns then
/// its first, and keeps the byte path through both. A raster from 0° meets
/// its seam at the prime meridian, one from 180°W at the antimeridian.
#[test]
fn a_view_across_a_seam_reads_its_last_columns_then_its_first() {
    for (west, bbox) in [
        (0.0, [-10.0, -5.0, 10.0, 5.0]),
        (-180.0, [170.0, -5.0, 190.0, 5.0]),
    ] {
        let fixture = global(&format!("windows-{west}"), west, true, false);
        let read = MapRead::new(&fixture.geo_transform(), bbox);
        assert_eq!(
            read.windows(),
            [(373, 90, 384, 102), (0, 90, 11, 102)],
            "from {west}°"
        );
        assert_eq!(read.dims(), Some((22, 12)));
        assert_eq!(read.col_period(), Some(384.0));
        let tile = render(&fixture.engine, bbox, &OutputCrs::Wgs84, (64, 32));
        assert!(
            matches!(tile.values, RasterValues::U8 { .. }),
            "from {west}°"
        );
        assert!(
            tile.values.iter_values().all(|v| v.is_some()),
            "from {west}°"
        );
    }
    // Inside the frame: one window, as before.
    let fixture = global("windows-inside", 0.0, true, false);
    let read = MapRead::new(&fixture.geo_transform(), [10.0, -5.0, 30.0, 5.0]);
    assert_eq!(read.windows(), [(10, 90, 32, 102)]);
}

/// A Lambert cone centred on 180° draws its pixels on both sides of the
/// antimeridian from its own side, each the value EDR position reads there,
/// in CRS84 views either side, through it in both forms, in Web Mercator
/// meta-tiles and a world copy; a whole-world view draws it on both edges.
/// Before, a view west of 180° gave the cone longitudes a turn away, off
/// its side (#1114), and the footprint guard left it empty.
#[test]
fn a_lambert_cone_centred_on_180_draws_its_own_side() {
    let fixture = Fixture::new(
        "map-lcc-seam",
        lcc(40.0, 50.0, 45.0, 180.0),
        [170.0, 35.0, 190.0, 55.0],
        10_000.0,
    );
    let merc = OutputCrs::WebMercator;
    let views: [View; 7] = [
        (
            "CRS:84 west of 180°",
            [-180.0, 40.0, -175.0, 50.0],
            OutputCrs::Wgs84,
            (64, 64),
        ),
        (
            "CRS:84 east of 180°",
            [175.0, 40.0, 180.0, 50.0],
            OutputCrs::Wgs84,
            (64, 64),
        ),
        (
            "CRS:84 seam, unwrapped",
            [178.0, 42.0, 182.0, 48.0],
            OutputCrs::Wgs84,
            (64, 64),
        ),
        (
            "CRS:84 seam, west > east",
            [178.0, 42.0, -178.0, 48.0],
            OutputCrs::Wgs84,
            (64, 64),
        ),
        (
            "meta-tile 8/15/5",
            meta_tile(8, 15, 5),
            merc.clone(),
            (64, 64),
        ),
        (
            "meta-tile 8/16/5",
            meta_tile(8, 16, 5),
            merc.clone(),
            (64, 64),
        ),
        (
            "meta-tile 8/-1/5, a world copy",
            meta_tile(8, -1, 5),
            merc,
            (64, 64),
        ),
    ];
    for (what, bbox, crs, size) in views {
        let tile = render(&fixture.engine, bbox, &crs, size);
        let drawn = assert_draws_the_position(&fixture, &tile, (what, bbox, &crs, size), 1);
        assert!(drawn > 500, "{what} draws {drawn} pixels");
    }
    // The world copy draws its twin.
    let copy = render(
        &fixture.engine,
        meta_tile(8, -1, 5),
        &OutputCrs::WebMercator,
        (64, 64),
    );
    let twin = render(
        &fixture.engine,
        meta_tile(8, 15, 5),
        &OutputCrs::WebMercator,
        (64, 64),
    );
    let values = |tile: &RasterTile| tile.values.iter_values().collect::<Vec<_>>();
    assert!(
        values(&copy) == values(&twin),
        "the world copy moved pixels"
    );

    // Both edges of a world view, from 180°W and from 185°W: the cone
    // lies at both ends of the view, 170–175°E at its east end.
    for (what, bbox) in [
        ("CRS:84 world", [-180.0, 30.0, 180.0, 60.0]),
        ("CRS:84 world from 185°W", [-185.0, 30.0, 175.0, 60.0]),
    ] {
        let (crs, size) = (OutputCrs::Wgs84, (1440, 120));
        let tile = render(&fixture.engine, bbox, &crs, size);
        assert_draws_the_position(&fixture, &tile, (what, bbox, &crs, size), 2);
        let (mut west, mut east) = (0, 0);
        for (i, value) in tile.values.iter_values().enumerate() {
            if value.is_some() {
                let ox = i as u32 % size.0;
                west += usize::from(ox < 40);
                east += usize::from(ox >= size.0 - 40);
            }
        }
        assert!(
            west > 500 && east > 500,
            "{what}: {west} pixels drawn at its west end, {east} at its east end"
        );
    }
}

/// A regional geographic raster, 10°W–40°E and 22–72°N in pixels of
/// 0.15625°, bytes `(7c + 3 row) mod 250` with an overview of 192 × 192.
fn regional() -> Fixture {
    let level = |side: u32, level: u32| Level {
        dims: (side, side),
        samples: Samples::U8(
            (0..side * side)
                .map(|i| ((7 * (i % side) + 3 * (i / side) + 50 * level) % 250) as u8)
                .collect(),
        ),
    };
    Fixture::levels(
        "regional-copies",
        geographic(),
        (-10.0, 72.0),
        50.0 / 320.0,
        &[level(320, 0), level(192, 1)],
    )
}

/// A world copy of a geographic raster, the view a turn or two west or
/// east of its twin, draws its twin pixel for pixel from as many
/// projections, at every level of the WMS meta-tiling up to 4 and from both
/// of the raster's levels. Its grid nodes move by the view's whole turns
/// onto the raster, where the grid measures its error: left a turn off it,
/// no probe counted, the grid never refined, and every copy drew unlike its
/// twin, at level 3 col 1 row 0 8 300 of 65 536 pixels, from 337
/// projections, not 5 635.
#[test]
fn a_geographic_world_copy_draws_as_its_twin() {
    let fixture = regional();
    let [west, south, east, north] = [-10.0, 22.0, 40.0, 72.0];
    let merc = OutputCrs::WebMercator;
    let mut twins = 0;
    for level in 0..=4 {
        let per_turn = 2f64.powf(f64::from(level) / 2.0);
        for row in 0..per_turn.ceil() as i64 {
            for col in 0..per_turn.ceil() as i64 {
                let twin = meta_tile(level, col, row);
                let [w, s, e, n] = twin;
                if w >= east || e <= west || s >= north || n <= south || w >= 180.0 {
                    continue;
                }
                twins += 1;
                let (expected, projections) =
                    render_counted(&fixture.engine, twin, &merc, (256, 256));
                let drawn = expected.values.iter_values().flatten().count();
                assert!(drawn > 0, "level {level} col {col} row {row} draws nothing");
                for turns in [-720.0, -360.0, 360.0, 720.0] {
                    let what = format!("level {level} col {col} row {row}, {turns}°");
                    let copy = [w + turns, s, e + turns, n];
                    let (tile, copy_projections) =
                        render_counted(&fixture.engine, copy, &merc, (256, 256));
                    assert!(
                        tile.values.iter_values().eq(expected.values.iter_values()),
                        "{what} moved pixels"
                    );
                    assert_eq!(copy_projections, projections, "{what}: projections");
                }
            }
        }
    }
    assert_eq!(twins, 10, "tiles holding the raster");
}

/// A global raster two turns away, Web Mercator level 0 col ±2, draws the
/// world as the tile in it does, from as many projections, and each pixel
/// as EDR position reads it. Before, its unrefined grid drew 45 568 of
/// 65 536 pixels unlike the tile in the world, from 1 105 projections, not
/// 22 963.
#[test]
fn a_global_raster_two_turns_away_draws_the_edr_position() {
    let fixture = global("two-turns", 0.0, false, false);
    let merc = OutputCrs::WebMercator;
    let size = (256, 256);
    let world = meta_tile(0, 0, 0);
    let (expected, projections) = render_counted(&fixture.engine, world, &merc, size);
    for col in [-2, 2] {
        let what = format!("level 0 col {col}");
        let bbox = meta_tile(0, col, 0);
        let (tile, copy_projections) = render_counted(&fixture.engine, bbox, &merc, size);
        assert!(
            tile.values.iter_values().eq(expected.values.iter_values()),
            "{what} moved pixels"
        );
        assert_eq!(copy_projections, projections, "{what}: projections");
        let drawn = assert_draws_the_position(&fixture, &tile, (&what, bbox, &merc, size), 2);
        assert_eq!(drawn, 128 * 128, "{what}");
    }
}

/// A copy of the footprint a turn on joins the guard's window only where
/// its longitudes meet the view's. A raster at 147.5–179.5°E has a copy at
/// 212.5–180.5°W, just past a world view's west edge: by the guard's margin
/// it joined a window across the whole view, and the grid refined all of
/// it. Copies that do meet the view still join: a raster across the
/// antimeridian fills both edges of it.
#[test]
fn a_copy_past_the_view_edge_joins_no_window() {
    let pacific = [147.5, -40.0, 179.5, 40.0];
    let views: [View; 3] = [
        (
            "CRS:84 world",
            [-180.0, -90.0, 180.0, 90.0],
            OutputCrs::Wgs84,
            (512, 256),
        ),
        (
            "quad 0/0/0",
            quad_tile(0, 0, 0),
            OutputCrs::WebMercator,
            (256, 256),
        ),
        (
            "CRS:84 to 150°E",
            [-180.0, -60.0, 150.0, 60.0],
            OutputCrs::Wgs84,
            (330, 120),
        ),
    ];
    for (what, bbox, crs, (w, h)) in &views {
        let own = crs.footprint_window_in_image(*bbox, pacific, *w, *h);
        assert!(own.is_some(), "{what}");
        assert_eq!(
            crate::map_read::footprint_window(crs, *bbox, pacific, *w, *h),
            own,
            "{what}: a copy past the view joined"
        );
    }
    // Across the antimeridian, both copies meet a world view.
    let seam = [170.0, -40.0, 190.0, 40.0];
    let (_, bbox, crs, (w, h)) = &views[0];
    let joined = crate::map_read::footprint_window(crs, *bbox, seam, *w, *h).unwrap();
    assert_eq!((joined.0, joined.1), (0, w - 1), "both edges");
}
