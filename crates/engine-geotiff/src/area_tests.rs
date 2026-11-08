//! #1435: an EDR area query on a projected GeoTIFF answers a regular CRS84
//! grid whose every cell holds the value of the native pixel at its own
//! `(x[i], y[j])`, where a position query there reads. A geographic source
//! answers its native window, as before.
//!
//! The fixtures are written here: small tiled float32 GeoTIFFs whose every
//! pixel holds its own index `row × width + col`, so a value names the
//! pixel it came from.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::feature::{Bbox, MAX_AREA_VALUES};
use ds_core::geo::{Crs, Ellipsoid, GeoTransform};
use ds_core::model::{CoverageResponse, DomainDescription, QueryResult};

use crate::area::{lon_arc, AreaGrid, AreaPlan};
use crate::GeoTiffEngine;

/// Tile side of the written fixtures; their sizes are multiples of it.
const TILE: u32 = 64;
/// The interpolation budget of `ds_core::resample::ProjectionGrid`, in
/// source pixels: a cell centre nearer a pixel edge than this may take the
/// pixel across it.
const EDGE_PX: f64 = 0.2;

/// A temporary directory, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "meteocore_area_grid_{name}_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A TIFF field's values.
enum Field {
    Short(Vec<u16>),
    Long(Vec<u32>),
    Double(Vec<f64>),
    Ascii(&'static str),
}

impl Field {
    /// `(type, count, little-endian bytes)`.
    fn encode(&self) -> (u16, u32, Vec<u8>) {
        match self {
            Field::Short(v) => (
                3,
                v.len() as u32,
                v.iter().flat_map(|x| x.to_le_bytes()).collect(),
            ),
            Field::Long(v) => (
                4,
                v.len() as u32,
                v.iter().flat_map(|x| x.to_le_bytes()).collect(),
            ),
            Field::Double(v) => (
                12,
                v.len() as u32,
                v.iter().flat_map(|x| x.to_le_bytes()).collect(),
            ),
            Field::Ascii(s) => {
                let mut bytes = s.as_bytes().to_vec();
                bytes.push(0);
                (2, bytes.len() as u32, bytes)
            }
        }
    }
}

/// A projected CRS as GeoKeys, and as ds-core's `Crs` to place a fixture.
struct Projection {
    crs: Crs,
    geokeys: Vec<u16>,
    doubles: Vec<f64>,
}

/// A projected CRS named only by its EPSG code, as the reader knows it.
fn epsg(code: u16, crs: Crs) -> Projection {
    Projection {
        crs,
        geokeys: vec![1, 1, 0, 3, 1024, 0, 1, 1, 1025, 0, 1, 1, 3072, 0, 1, code],
        doubles: Vec::new(),
    }
}

/// EPSG:3067, ETRS89 / TM35FIN.
fn tm35fin() -> Projection {
    epsg(
        3067,
        Crs::TransverseMercator {
            lat0: 0.0,
            lon0: 27.0_f64.to_radians(),
            k0: 0.9996,
            false_e: 500_000.0,
            false_n: 0.0,
            figure: Ellipsoid::WGS84,
        },
    )
}

/// EPSG:3035, ETRS89-extended / LAEA Europe.
fn laea_europe() -> Projection {
    epsg(
        3035,
        Crs::LambertAzimuthalEqualArea {
            lat0: 52.0_f64.to_radians(),
            lon0: 10.0_f64.to_radians(),
            false_e: 4_321_000.0,
            false_n: 3_210_000.0,
            figure: Ellipsoid::WGS84,
        },
    )
}

/// A UTM-like Transverse Mercator on WGS84 with its central meridian at
/// `lon0` degrees, from its parameter GeoKeys.
fn tm_at(lon0: f64) -> Projection {
    Projection {
        crs: Crs::TransverseMercator {
            lat0: 0.0,
            lon0: lon0.to_radians(),
            k0: 0.9996,
            false_e: 500_000.0,
            false_n: 0.0,
            figure: Ellipsoid::WGS84,
        },
        geokeys: vec![
            1, 1, 0, 9, //
            1024, 0, 1, 1, // projected
            1025, 0, 1, 1, // pixel is area
            3072, 0, 1, 32767, // user-defined
            3075, 0, 1, 1, // Transverse Mercator
            3080, 34736, 1, 0, // natural origin longitude
            3081, 34736, 1, 1, // natural origin latitude
            3082, 34736, 1, 2, // false easting
            3083, 34736, 1, 3, // false northing
            3092, 34736, 1, 4, // scale at natural origin
        ],
        doubles: vec![lon0, 0.0, 500_000.0, 0.0, 0.9996],
    }
}

/// A Lambert conformal conic (2SP) on WGS84, from its parameter GeoKeys.
fn lcc(lat1: f64, lat2: f64, lat0: f64, lon0: f64) -> Projection {
    Projection {
        crs: Crs::LambertConformalConic {
            lat1: lat1.to_radians(),
            lat2: lat2.to_radians(),
            lat0: lat0.to_radians(),
            lon0: lon0.to_radians(),
            false_e: 0.0,
            false_n: 0.0,
            radius: None,
        },
        geokeys: vec![
            1, 1, 0, 8, //
            1024, 0, 1, 1, //
            1025, 0, 1, 1, //
            3072, 0, 1, 32767, //
            3075, 0, 1, 8, // Lambert conformal conic 2SP
            3078, 34736, 1, 0, // first standard parallel
            3079, 34736, 1, 1, // second standard parallel
            3084, 34736, 1, 2, // false origin longitude
            3085, 34736, 1, 3, // false origin latitude
        ],
        doubles: vec![lat1, lat2, lon0, lat0],
    }
}

/// Write a tiled, uncompressed float32 GeoTIFF of `width` × `height` pixels
/// of `pixel` metres, its top-left corner at `origin`, each pixel holding
/// `row × width + col`.
fn write_geotiff(
    path: &Path,
    projection: &Projection,
    origin: (f64, f64),
    pixel: f64,
    (width, height): (u32, u32),
) {
    assert!(width % TILE == 0 && height % TILE == 0);
    let mut tiff = b"II\x2a\x00\0\0\0\0".to_vec();
    let (mut offsets, mut counts) = (Vec::new(), Vec::new());
    for tile_row in 0..height / TILE {
        for tile_col in 0..width / TILE {
            offsets.push(tiff.len() as u32);
            counts.push(TILE * TILE * 4);
            for r in 0..TILE {
                for c in 0..TILE {
                    let (row, col) = (tile_row * TILE + r, tile_col * TILE + c);
                    let value = (row * width + col) as f32;
                    tiff.extend_from_slice(&value.to_le_bytes());
                }
            }
        }
    }
    let mut fields = vec![
        (256, Field::Long(vec![width])),
        (257, Field::Long(vec![height])),
        (258, Field::Short(vec![32])),
        (259, Field::Short(vec![1])),
        (262, Field::Short(vec![1])),
        (277, Field::Short(vec![1])),
        (284, Field::Short(vec![1])),
        (322, Field::Short(vec![TILE as u16])),
        (323, Field::Short(vec![TILE as u16])),
        (324, Field::Long(offsets)),
        (325, Field::Long(counts)),
        (339, Field::Short(vec![3])),
        (33550, Field::Double(vec![pixel, pixel, 0.0])),
        (
            33922,
            Field::Double(vec![0.0, 0.0, 0.0, origin.0, origin.1, 0.0]),
        ),
        (34735, Field::Short(projection.geokeys.clone())),
    ];
    if !projection.doubles.is_empty() {
        fields.push((34736, Field::Double(projection.doubles.clone())));
    }
    fields.push((42113, Field::Ascii("-9999")));

    // Out-of-line values, then the IFD.
    let mut entries = Vec::new();
    for (tag, field) in &fields {
        let (kind, count, bytes) = field.encode();
        let value = if bytes.len() <= 4 {
            let mut inline = bytes.clone();
            inline.resize(4, 0);
            inline
        } else {
            if tiff.len() % 2 == 1 {
                tiff.push(0);
            }
            let offset = tiff.len() as u32;
            tiff.extend_from_slice(&bytes);
            offset.to_le_bytes().to_vec()
        };
        entries.push((*tag, kind, count, value));
    }
    if tiff.len() % 2 == 1 {
        tiff.push(0);
    }
    let ifd = tiff.len() as u32;
    tiff[4..8].copy_from_slice(&ifd.to_le_bytes());
    tiff.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for (tag, kind, count, value) in entries {
        tiff.extend_from_slice(&u16::to_le_bytes(tag));
        tiff.extend_from_slice(&kind.to_le_bytes());
        tiff.extend_from_slice(&count.to_le_bytes());
        tiff.extend_from_slice(&value);
    }
    tiff.extend_from_slice(&0u32.to_le_bytes());
    std::fs::write(path, tiff).unwrap();
}

/// A one-file collection on a `pixel`-metre grid of `projection` reaching
/// a few pixels past the lon/lat box `cover` (its east may lie past 180°).
struct Fixture {
    _dir: TempDir,
    engine: GeoTiffEngine,
}

impl Fixture {
    fn new(name: &str, projection: Projection, cover: [f64; 4], pixel: f64) -> Self {
        let [w, s, e, n] = cover;
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for i in 0..=100 {
            for j in 0..=100 {
                let lon = w + (e - w) * f64::from(i) / 100.0;
                let lat = s + (n - s) * f64::from(j) / 100.0;
                let (x, y) = projection.crs.forward(lon, lat);
                (min_x, min_y) = (min_x.min(x), min_y.min(y));
                (max_x, max_y) = (max_x.max(x), max_y.max(y));
            }
        }
        let margin = 4.0 * pixel;
        let origin = (
            ((min_x - margin) / pixel).floor() * pixel,
            ((max_y + margin) / pixel).ceil() * pixel,
        );
        let size = |span: f64| ((span / pixel).ceil() as u32).div_ceil(TILE) * TILE;
        let dims = (
            size(max_x + margin - origin.0),
            size(origin.1 - (min_y - margin)),
        );
        let dir = TempDir::new(name);
        write_geotiff(
            &dir.0.join("area_20261108T1200Z.tif"),
            &projection,
            origin,
            pixel,
            dims,
        );
        let config: ds_core::config::GeoTiffConfig = serde_json::from_value(serde_json::json!({
            "filename_template": "area_%Y%m%dT%H%MZ.tif", "parameter": "value", "unit": "1"
        }))
        .unwrap();
        let engine = GeoTiffEngine::new(name, Some(dir.0.to_str().unwrap()), &config)
            .expect("the written fixture loads");
        let fixture = Self { _dir: dir, engine };
        let crs = fixture.geo_transform().crs;
        assert_eq!(
            std::mem::discriminant(&crs),
            std::mem::discriminant(&projection.crs),
            "the reader parses the fixture's CRS: {crs:?}"
        );
        fixture
    }

    fn geo_transform(&self) -> GeoTransform {
        let catalog = self.engine.catalog.load();
        let entry = catalog.entries.values().next().unwrap();
        entry.metadata().unwrap().geo_transform.clone()
    }

    fn area(&self, coords: &str) -> Result<QueryResult, DataServerError> {
        match self.engine.query_area(coords, None, None, None, None)? {
            CoverageResponse::Single(result) => Ok(result),
            CoverageResponse::Collection(_) => panic!("expected one coverage"),
        }
    }

    /// The EDR position query's value at `(lon, lat)`.
    fn position(&self, lon: f64, lat: f64) -> Option<f64> {
        let point = self.engine.query_point(lat, lon, None, None).unwrap();
        point.ranges["value"].values[0]
    }

    /// The values a cell centre at `(lon, lat)` may hold: its pixel's, or
    /// within [`EDGE_PX`] of a pixel edge also the pixel across it. Empty
    /// off the raster.
    fn pixel_values(&self, lon: f64, lat: f64) -> Vec<Option<f64>> {
        pixel_values(&self.geo_transform(), lon, lat)
    }
}

/// [`Fixture::pixel_values`] on the fixture's grid `gt`.
fn pixel_values(gt: &GeoTransform, lon: f64, lat: f64) -> Vec<Option<f64>> {
    let (col, row) = gt.world_to_pixel_f64(lon, lat);
    let near = |v: f64| {
        let base = v.floor();
        let mut out = vec![base];
        if v - base < EDGE_PX {
            out.push(base - 1.0);
        }
        if base + 1.0 - v < EDGE_PX {
            out.push(base + 1.0);
        }
        out
    };
    let mut values = Vec::new();
    for c in near(col) {
        for r in near(row) {
            if c >= 0.0 && r >= 0.0 && c < f64::from(gt.width) && r < f64::from(gt.height) {
                values.push(Some(r * f64::from(gt.width) + c));
            }
        }
    }
    values
}

fn axes(result: &QueryResult) -> (&[f64], &[f64]) {
    let DomainDescription::Grid { x, y, .. } = &result.domain else {
        panic!("expected a Grid, got {:?}", result.domain);
    };
    (x, y)
}

fn values(result: &QueryResult) -> &[Option<f64>] {
    &result.ranges["value"].values
}

/// The plan of an area query over `bbox` on `gt`, one timestep.
fn plan(gt: &GeoTransform, [west, south, east, north]: [f64; 4]) -> AreaPlan {
    let bbox = Bbox {
        west,
        south,
        east,
        north,
    };
    AreaGrid::new(gt, &bbox)
        .unwrap()
        .plan(None, 1, "test")
        .unwrap()
}

/// The share of the native pixels whose centre lies in `lons` × `lats` that
/// some cell of `result` holds: nearest sampling at a step coarser than a
/// pixel skips some.
fn reached_share(
    fixture: &Fixture,
    result: &QueryResult,
    lons: std::ops::RangeInclusive<f64>,
    lats: std::ops::RangeInclusive<f64>,
) -> f64 {
    let gt = fixture.geo_transform();
    let reached: std::collections::HashSet<u64> =
        values(result).iter().flatten().map(|&v| v as u64).collect();
    let (mut inside, mut hit) = (0usize, 0usize);
    for row in 0..gt.height {
        for col in 0..gt.width {
            let (lon, lat) = gt.pixel_to_world(col, row);
            if lons.contains(&lon) && lats.contains(&lat) {
                inside += 1;
                hit += usize::from(reached.contains(&u64::from(row * gt.width + col)));
            }
        }
    }
    assert!(inside > 0);
    hit as f64 / inside as f64
}

/// Whether `(lon, lat)` lies on the raster farther than [`EDGE_PX`] from its
/// edge, where a cell must hold its pixel.
fn surely_on_raster(gt: &GeoTransform, lon: f64, lat: f64) -> bool {
    let (col, row) = gt.world_to_pixel_f64(lon, lat);
    col >= EDGE_PX
        && row >= EDGE_PX
        && col < f64::from(gt.width) - EDGE_PX
        && row < f64::from(gt.height) - EDGE_PX
}

/// Every cell of `result` whose centre lies on the raster holds its own
/// pixel's value, and every other cell is null; within [`EDGE_PX`] of the
/// raster's edge, either.
fn assert_cells_on_the_raster_are_filled(fixture: &Fixture, result: &QueryResult, what: &str) {
    let (x, y) = axes(result);
    let values = values(result);
    let gt = fixture.geo_transform();
    let mut wrong = 0;
    for (j, &lat) in y.iter().enumerate() {
        for (i, &lon) in x.iter().enumerate() {
            let value = values[j * x.len() + i];
            let right = pixel_values(&gt, lon, lat).contains(&value)
                || (value.is_none() && !surely_on_raster(&gt, lon, lat));
            wrong += usize::from(!right);
        }
    }
    assert_eq!(wrong, 0, "{what}: cells that do not hold their own pixel");
}

/// `axis` is evenly spaced; its step.
fn assert_regular(axis: &[f64], what: &str) -> f64 {
    assert!(axis.len() > 2, "{what}: {} cells", axis.len());
    let step = (axis[axis.len() - 1] - axis[0]) / (axis.len() - 1) as f64;
    for (i, pair) in axis.windows(2).enumerate() {
        assert!(
            (pair[1] - pair[0] - step).abs() < 1e-9,
            "{what}: step {i} is {} where the axis steps {step}",
            pair[1] - pair[0]
        );
    }
    step
}

/// Every cell of `result` holds the value of the native pixel at its own
/// `(x[i], y[j])`; the corner cells and a few inside it also match the
/// position query there.
fn assert_cells_are_where_the_axes_say(fixture: &Fixture, result: &QueryResult, what: &str) {
    let (x, y) = axes(result);
    let (nx, ny) = (x.len(), y.len());
    let values = values(result);
    assert_eq!(values.len(), nx * ny, "{what}: one timestep");
    let gt = fixture.geo_transform();
    for (j, &lat) in y.iter().enumerate() {
        for (i, &lon) in x.iter().enumerate() {
            let value = values[j * nx + i];
            let expected = pixel_values(&gt, lon, lat);
            assert!(
                value.is_some(),
                "{what}: cell ({i}, {j}) lies on the raster"
            );
            assert!(
                expected.contains(&value),
                "{what}: cell ({i}, {j}) at ({lon}, {lat}) holds {value:?}, its pixel {expected:?}"
            );
        }
    }
    let cells = [
        (0, 0),
        (nx - 1, 0),
        (0, ny - 1),
        (nx - 1, ny - 1),
        (nx / 3, ny / 4),
        (nx / 2, ny / 2),
        (2 * nx / 3, 3 * ny / 4),
    ];
    for (i, j) in cells {
        let (lon, lat) = (x[i], y[j]);
        let area = values[j * nx + i];
        let position = fixture.position(lon, lat);
        assert!(position.is_some(), "{what}: ({lon}, {lat}) on the raster");
        // Equal, unless the centre lies within the interpolation budget of
        // a pixel edge, where the area may take the pixel across it.
        assert!(
            area == position || fixture.pixel_values(lon, lat).len() > 1,
            "{what}: cell ({i}, {j}) at ({lon}, {lat}): area {area:?}, position {position:?}"
        );
    }
}

/// The issue's TM35FIN case: a 4° × 1.3° box at 68°N, 3 to 7 degrees west
/// of the central meridian, on a 250 m grid. Axes along the window's middle
/// row and column put its corner cells kilometres from their data; the
/// answer is a regular CRS84 grid over the box instead, every cell its own
/// pixel's, at about the native resolution.
#[test]
fn a_tm35fin_area_is_a_regular_crs84_grid_of_its_own_pixels() {
    let fixture = Fixture::new("tm35fin", tm35fin(), [20.0, 67.5, 24.0, 68.8], 250.0);
    let result = fixture.area("20,67.5,24,68.8").unwrap();
    let (x, y) = axes(&result);
    let dx = assert_regular(x, "x");
    let dy = assert_regular(y, "y");
    assert!(dx > 0.0 && dy < 0.0, "x ascends, y descends");
    // Cells over the box, edge to edge.
    assert!((x[0] - dx / 2.0 - 20.0).abs() < 1e-9);
    assert!((x[x.len() - 1] + dx / 2.0 - 24.0).abs() < 1e-9);
    assert!((y[0] - dy / 2.0 - 68.8).abs() < 1e-9);
    assert!((y[y.len() - 1] + dy / 2.0 - 67.5).abs() < 1e-9);
    // A native pixel's ground size, whatever the grid's rotation: 250 m is
    // 0.00225° of latitude, and 0.00588° of longitude at 67.5°N, the box's
    // edge nearest the equator, so no cell is wider than a pixel.
    let metres_per_degree = ds_core::geo::EARTH_RADIUS_M * std::f64::consts::PI / 180.0;
    let native_dy = 250.0 / metres_per_degree;
    let native_dx = native_dy / 67.5_f64.to_radians().cos();
    assert!((-dy / native_dy - 1.0).abs() < 0.01, "y step {dy}");
    assert!((dx / native_dx - 1.0).abs() < 0.01, "x step {dx}");
    let plan = plan(&fixture.geo_transform(), [20.0, 67.5, 24.0, 68.8]);
    let (window_nx, window_ny) = plan.window_dims();
    eprintln!(
        "4° × 1.3° at 68°N on 250 m TM35FIN: {} × {} = {} cells, native window {window_nx} × \
         {window_ny} = {} pixels, steps {dx:.5}° × {:.5}°",
        x.len(),
        y.len(),
        x.len() * y.len(),
        window_nx * window_ny,
        -dy
    );
    assert!(x.len() * y.len() <= window_nx * window_ny);
    assert_cells_are_where_the_axes_say(&fixture, &result, "TM35FIN");

    // Nearest sampling reaches every native pixel whose centre lies in the
    // box, but for a few where the rotated pixel rows cross the cell rows.
    let share = reached_share(&fixture, &result, 20.0..=24.0, 67.5..=68.8);
    eprintln!(
        "{:.2} % of the native pixels in the box reached",
        100.0 * share
    );
    assert!(share > 0.995, "{share}");
}

/// Review of #1435: the longitude step is taken at the grid's latitude
/// nearest the equator, not at its centre's. Over 60–70°N a step at 65°N
/// was a fifth wider than a pixel at 60°N, and nearest sampling reached
/// only 85.6 % of the pixels there; every band now reaches all but a few.
#[test]
fn no_cell_is_wider_than_a_native_pixel_anywhere_on_a_tall_area() {
    let fixture = Fixture::new("tall", tm35fin(), [20.0, 60.0, 30.0, 70.0], 1_000.0);
    let result = fixture.area("20,60,30,70").unwrap();
    let (x, y) = axes(&result);
    let dx = assert_regular(x, "x");
    let metres_per_degree = ds_core::geo::EARTH_RADIUS_M * std::f64::consts::PI / 180.0;
    let native_dx = 1_000.0 / metres_per_degree / 60.0_f64.to_radians().cos();
    assert!((dx / native_dx - 1.0).abs() < 0.01, "x step {dx}");
    eprintln!(
        "20,60,30,70 on 1 km TM35FIN: {} × {} cells",
        x.len(),
        y.len()
    );
    for lats in [60.0..=61.0, 65.0..=66.0, 69.0..=70.0] {
        let share = reached_share(&fixture, &result, 20.0..=30.0, lats.clone());
        eprintln!("{lats:?}: {:.2} % reached", 100.0 * share);
        assert!(share > 0.99, "{lats:?}: {share}");
    }
}

/// Review of #1435: a polar cap takes the same equatorward longitude step,
/// not its latitude step, which made a cap 1 / cos(edge latitude) times the
/// cells: on a 3 km polar LAEA grid -180,85,180,90 was 2.5 M cells, a 400,
/// where the native window was 141 k. Each cap now answers, its cells their
/// own pixels'.
#[test]
fn a_polar_cap_answers_at_the_native_step_of_its_edge() {
    let polar = Projection {
        crs: Crs::LambertAzimuthalEqualArea {
            lat0: 90.0_f64.to_radians(),
            lon0: 0.0,
            false_e: 0.0,
            false_n: 0.0,
            figure: Ellipsoid::WGS84,
        },
        geokeys: vec![
            1, 1, 0, 7, //
            1024, 0, 1, 1, //
            1025, 0, 1, 1, //
            3072, 0, 1, 32767, //
            3075, 0, 1, 10, // Lambert azimuthal equal area
            3080, 34736, 1, 0, // natural origin longitude
            3081, 34736, 1, 1, // natural origin latitude
        ],
        doubles: vec![0.0, 90.0],
    };
    let fixture = Fixture::new("polar", polar, [-180.0, 79.5, 180.0, 90.0], 3_000.0);
    let metres_per_degree = ds_core::geo::EARTH_RADIUS_M * std::f64::consts::PI / 180.0;
    let dlat = 3_000.0 / metres_per_degree;
    for (coords, [west, south, east, north]) in [
        ("-180,85,180,90", [-180.0, 85.0, 180.0, 90.0]),
        ("-180,80,180,90", [-180.0, 80.0, 180.0, 90.0]),
        ("0,85,90,90", [0.0, 85.0, 90.0, 90.0]),
    ] {
        let result = fixture.area(coords).unwrap();
        let (x, y) = axes(&result);
        let dlon = dlat / f64::to_radians(south).cos();
        let expected = ((east - west) / dlon).ceil() * ((north - south) / dlat).ceil();
        let cells = (x.len() * y.len()) as f64;
        eprintln!(
            "{coords} on 3 km polar LAEA: {} × {} cells",
            x.len(),
            y.len()
        );
        assert!(
            (cells / expected - 1.0).abs() < 0.02,
            "{coords}: {cells} cells, {expected}"
        );
        if coords == "-180,85,180,90" {
            assert_cells_are_where_the_axes_say(&fixture, &result, coords);
        }
    }
}

/// Review of #1435: the answer and its read window come from the cells'
/// own source positions, not from points sampled along the bbox's edges.
/// Those miss the bulge of a parallel at the central meridian between
/// samples: over a raster at 25.5–28.5°E, -10,62,60,70 answered 720 cells
/// on the raster as nulls, and -180,62,180,70 2656, its grid starting at
/// 62.279°N. -40,62,94,70 had a sample on 27°E.
#[test]
fn a_wide_bbox_answers_every_cell_on_the_raster() {
    let fixture = Fixture::new("wide", tm35fin(), [25.5, 61.8, 28.5, 62.8], 250.0);
    for coords in ["-10,62,60,70", "-180,62,180,70", "-40,62,94,70"] {
        let result = fixture.area(coords).unwrap();
        let (x, y) = axes(&result);
        let dy = assert_regular(y, coords);
        assert!(
            (y[y.len() - 1] + dy / 2.0 - 62.0).abs() < 1e-9,
            "{coords}: the grid reaches the bbox's south edge, {}",
            y[y.len() - 1]
        );
        assert!(values(&result).iter().any(Option::is_some), "{coords}");
        assert_cells_on_the_raster_are_filled(&fixture, &result, coords);
        eprintln!("{coords}: {} × {} cells", x.len(), y.len());
    }
}

/// The issue's LAEA case, an OPERA-like EPSG:3035 grid over 10–14°E,
/// 50–53°N, and a Lambert conformal conic grid: every cell is its own
/// pixel's, on regular axes.
#[test]
fn laea_and_lcc_areas_are_regular_crs84_grids_of_their_own_pixels() {
    for (what, projection, cover, coords) in [
        (
            "LAEA",
            laea_europe(),
            [10.0, 50.0, 14.0, 53.0],
            "10,50,14,53",
        ),
        (
            "LCC",
            lcc(63.3, 63.3, 63.3, 15.0),
            [18.0, 61.0, 24.0, 64.0],
            "18,61,24,64",
        ),
    ] {
        let fixture = Fixture::new(what, projection, cover, 2_000.0);
        let result = fixture.area(coords).unwrap();
        let (x, y) = axes(&result);
        assert_regular(x, what);
        assert_regular(y, what);
        assert_cells_are_where_the_axes_say(&fixture, &result, what);
    }
}

/// A WKT polygon and a radius take the shared mask over the regular grid:
/// a cell is filled when its centre is inside, with its own pixel's value.
#[test]
fn a_projected_polygon_masks_the_regular_grid_by_cell_centre() {
    let fixture = Fixture::new("mask", tm35fin(), [24.0, 60.0, 26.0, 61.0], 1_000.0);
    let triangle = "POLYGON((24 60, 26 60, 25 61, 24 60))";
    let polygon = ds_core::feature::parse_area_coords(triangle).unwrap();
    let result = fixture.area(triangle).unwrap();
    let (x, y) = axes(&result);
    assert_regular(x, "x");
    assert_regular(y, "y");
    let cells = values(&result);
    let mut inside = 0;
    for (j, &lat) in y.iter().enumerate() {
        for (i, &lon) in x.iter().enumerate() {
            let value = cells[j * x.len() + i];
            if polygon.contains(lon, lat) {
                inside += 1;
                assert!(
                    fixture.pixel_values(lon, lat).contains(&value),
                    "({lon}, {lat}) holds {value:?}"
                );
            } else {
                assert_eq!(value, None, "({lon}, {lat}) is outside the triangle");
            }
        }
    }
    assert!(inside > x.len() * y.len() / 3, "{inside} cells inside");

    let CoverageResponse::Single(circle) = fixture
        .engine
        .query_radius("POINT(25 60.5)", 20_000.0, None, None, None, None)
        .unwrap()
    else {
        panic!("expected one coverage");
    };
    let (x, y) = axes(&circle);
    assert_regular(x, "radius x");
    assert_regular(y, "radius y");
    assert!(values(&circle).iter().any(Option::is_some));
    // The bbox's corners lie outside the circle.
    assert_eq!(values(&circle)[0], None);
}

/// A bbox reaching past the raster answers the part the raster's window
/// covers, as the native window did, not a grid of nulls over the rest.
#[test]
fn a_bbox_past_the_raster_answers_the_window_it_covers() {
    let fixture = Fixture::new("past", tm35fin(), [24.0, 60.0, 26.0, 61.0], 1_000.0);
    let inside = fixture.area("24,60,26,61").unwrap();
    let past = fixture.area("10,50,40,70").unwrap();
    let (x, y) = axes(&past);
    assert_regular(x, "x");
    assert_regular(y, "y");
    let gt = fixture.geo_transform();
    let [w, s, e, n] = gt.bbox_with_poles();
    assert!(x[0] >= w && x[x.len() - 1] <= e && y[0] <= n && y[y.len() - 1] >= s);
    // About the raster's own pixels, its envelope at its native step, not
    // the 3.6 M cells the whole bbox would take at that step.
    assert!(x.len() * y.len() < 2 * (gt.width * gt.height) as usize);
    let (ix, iy) = axes(&inside);
    assert!(x.len() >= ix.len() && y.len() >= iy.len());
}

/// The answer's budgets count its own cells: an area within the response
/// budget answers, and one over it is the 400, before any file is read.
/// A projected source's native window is held to the budget too, with a
/// message naming its pixels: a long thin box far from the central
/// meridian, its window many times its answer.
#[test]
fn the_budgets_count_the_answer_and_the_native_window() {
    let gt = GeoTransform {
        origin_x: -500_000.0,
        origin_y: 8_000_000.0,
        pixel_width: 100.0,
        pixel_height: 100.0,
        width: 20_000,
        height: 20_000,
        crs: tm35fin().crs,
    };
    let bbox = |west, south, east, north| Bbox {
        west,
        south,
        east,
        north,
    };
    let thin = AreaGrid::new(&gt, &bbox(10.0, 62.0, 20.0, 62.01)).unwrap();
    let (nx, ny) = thin.dims();
    assert!(nx * ny <= MAX_AREA_VALUES, "{nx} × {ny}");
    match thin.plan(None, 1, "test") {
        Err(DataServerError::QueryTooLarge(msg)) => {
            assert!(msg.contains("source pixels"), "{msg}");
            assert!(msg.contains(&MAX_AREA_VALUES.to_string()), "{msg}");
        }
        Err(e) => panic!("expected QueryTooLarge, got {e:?}"),
        Ok(_) => panic!("expected QueryTooLarge, got a plan"),
    }

    let small = || AreaGrid::new(&gt, &bbox(24.0, 62.0, 25.0, 62.5)).unwrap();
    let (nx, ny) = small().dims();
    assert!(small().plan(None, 1, "test").is_ok());
    let over = MAX_AREA_VALUES / (nx * ny) + 1;
    match small().plan(None, over, "test") {
        Err(DataServerError::QueryTooLarge(msg)) => {
            assert!(
                msg.contains(&format!("{over} timesteps × {ny} × {nx} cells")),
                "{msg}"
            );
        }
        Err(e) => panic!("expected QueryTooLarge, got {e:?}"),
        Ok(_) => panic!("expected QueryTooLarge, got a plan"),
    }
}

/// Antimeridian: a source across it answers the bbox 170,10,-170,20 as one
/// grid through the seam, its cells their own pixels', the position query
/// agreeing on both sides. A source far from it is the 400 that any bbox
/// missing the raster is, without a panic; before #1435 its edges were
/// sampled the long way round, through the source, and the whole raster
/// answered as nulls.
#[test]
fn a_bbox_across_the_antimeridian_answers_one_grid_through_the_seam() {
    let fixture = Fixture::new("seam", tm_at(180.0), [170.0, 10.0, 190.0, 20.0], 10_000.0);
    let result = fixture.area("170,10,-170,20").unwrap();
    let (x, y) = axes(&result);
    let unwrapped: Vec<f64> = x
        .iter()
        .map(|&lon| if lon < 0.0 { lon + 360.0 } else { lon })
        .collect();
    let dx = assert_regular(&unwrapped, "x through the seam");
    assert_regular(y, "y");
    assert!((x[0] - dx / 2.0 - 170.0).abs() < 1e-9, "{}", x[0]);
    assert!(
        (x[x.len() - 1] + dx / 2.0 + 170.0).abs() < 1e-9,
        "{}",
        x[x.len() - 1]
    );
    assert!(x.iter().all(|lon| (-180.0..=180.0).contains(lon)));
    assert!(x.iter().any(|&lon| lon > 175.0) && x.iter().any(|&lon| lon < -175.0));
    assert_cells_are_where_the_axes_say(&fixture, &result, "seam");
    // Either side of the seam.
    let values = values(&result);
    let east_of = x.iter().position(|&lon| lon < 0.0).unwrap();
    for i in [east_of - 1, east_of] {
        let (lon, lat) = (x[i], y[y.len() / 2]);
        let area = values[(y.len() / 2) * x.len() + i];
        assert!(
            area == fixture.position(lon, lat) || fixture.pixel_values(lon, lat).len() > 1,
            "({lon}, {lat})"
        );
    }

    let far = Fixture::new("seam-far", tm35fin(), [24.0, 60.0, 26.0, 61.0], 1_000.0);
    assert!(matches!(
        far.area("170,10,-170,20"),
        Err(DataServerError::InvalidParameter(_))
    ));
}

/// A geographic source answers as it always did: its native pixel window,
/// axes at the pixel centres, values as read, each masked by its centre.
#[test]
fn a_geographic_area_is_its_native_window_unchanged() {
    let config: ds_core::config::GeoTiffConfig = serde_json::from_value(serde_json::json!({
        "filename_template": "radar_%Y%m%dT%H%MZ.tif", "parameter": "value", "unit": "dBZ"
    }))
    .unwrap();
    let engine = GeoTiffEngine::new("radar", Some("../../testdata/radar"), &config).unwrap();
    let (t0, entry) = {
        let catalog = engine.catalog.load();
        let (t, e) = catalog.entries.iter().next().unwrap();
        (*t, e.clone())
    };
    let metadata = entry.metadata().unwrap();
    let gt = &metadata.geo_transform;
    assert!(matches!(gt.crs, Crs::Wgs84));
    let (west, south, east, north) = (24.5, 60.0, 25.0, 60.5);
    let (c0, r0, c1, r1) = gt.bbox_to_pixels(west, south, east, north).unwrap();
    let native = crate::reader::read_bbox(
        entry.source().unwrap(),
        metadata,
        c0,
        r0,
        c1,
        r1,
        None,
        &entry.path,
        engine.band_index,
    )
    .unwrap();
    let x_native: Vec<f64> = (c0..c1).map(|c| gt.pixel_to_world(c, r0).0).collect();
    let y_native: Vec<f64> = (r0..r1).map(|r| gt.pixel_to_world(c0, r).1).collect();

    let one = Some((t0, t0));
    let bbox = engine
        .query_bbox(west, south, east, north, one, None)
        .unwrap();
    assert_eq!(axes(&bbox), (&x_native[..], &y_native[..]));
    assert_eq!(values(&bbox), &native[..]);

    let polygon = format!(
        "POLYGON(({west} {south}, {east} {south}, {east} {north}, {west} {north}, {west} {south}))"
    );
    let CoverageResponse::Single(area) =
        engine.query_area(&polygon, one, None, None, None).unwrap()
    else {
        panic!("expected one coverage");
    };
    assert_eq!(axes(&area), (&x_native[..], &y_native[..]));
    let ring = ds_core::feature::parse_area_coords(&polygon).unwrap();
    let nx = x_native.len();
    let masked: Vec<Option<f64>> = native
        .iter()
        .enumerate()
        .map(|(k, v)| {
            let (lon, lat) = (x_native[k % nx], y_native[k / nx]);
            v.filter(|_| ring.contains(lon, lat))
        })
        .collect();
    assert_eq!(values(&area), &masked[..]);

    // Across the antimeridian, far from the raster: the 400 it always was.
    assert!(matches!(
        engine.query_area("170,10,-170,20", one, None, None, None),
        Err(DataServerError::InvalidParameter(_))
    ));
}

/// Re-review of #1435: a bbox inside the raster's CRS84 envelope but off
/// the rotated raster, in a corner of the envelope it leaves empty, has no
/// cell on the raster. It is the 400 it was before, from the plan before
/// any read, not a 200 grid of nulls.
#[test]
fn a_bbox_in_an_empty_corner_of_the_envelope_misses_the_raster() {
    let fixture = Fixture::new("corner", tm35fin(), [20.0, 67.5, 24.0, 68.8], 250.0);
    let gt = fixture.geo_transform();
    let [west, south, east, north] = gt.footprint_envelope().unwrap();
    let off_raster = |[w, s, e, n]: [f64; 4]| {
        [
            (w, s),
            (e, s),
            (e, n),
            (w, n),
            ((w + e) / 2.0, (s + n) / 2.0),
        ]
        .iter()
        .all(|&(lon, lat)| gt.world_to_pixel(lon, lat).is_none())
    };
    // A 0.05° × 0.02° box just inside each corner of the envelope.
    let corners = [
        [west + 0.005, south + 0.005, west + 0.055, south + 0.025],
        [east - 0.055, south + 0.005, east - 0.005, south + 0.025],
        [east - 0.055, north - 0.025, east - 0.005, north - 0.005],
        [west + 0.005, north - 0.025, west + 0.055, north - 0.005],
    ];
    let empty: Vec<[f64; 4]> = corners.into_iter().filter(|&b| off_raster(b)).collect();
    assert!(!empty.is_empty(), "a rotated raster leaves a corner empty");
    for [w, s, e, n] in empty {
        let coords = format!("{w},{s},{e},{n}");
        match fixture.area(&coords) {
            Err(DataServerError::InvalidParameter(msg)) => {
                assert!(msg.contains("does not intersect the raster"), "{msg}");
            }
            Err(e) => panic!("{coords}: expected the 400, got {e:?}"),
            Ok(result) => panic!(
                "{coords}: expected the 400, got {} cells",
                values(&result).len()
            ),
        }
    }
}

/// Re-review of #1435: a bbox nearly a whole turn wide whose gap lies inside
/// the raster's envelope meets it in two pieces. The answer is the shorter
/// arc joining them, the envelope's side, not the arc round the bbox.
#[test]
fn two_pieces_join_on_the_envelope_side() {
    let close = |arc: Option<(f64, f64)>, (west, east): (f64, f64)| {
        let (w, e) = arc.unwrap_or_else(|| panic!("no arc, expected {west}..{east}"));
        assert!(
            (w - west).abs() < 1e-9 && (e - east).abs() < 1e-9,
            "{w}..{e}, expected {west}..{east}"
        );
    };
    // A raster at 170–190°E: the bbox's gap 179°E–179°W lies inside it.
    close(
        lon_arc((-179.0, 358.0), (170.0, 20.0), Some(180.0)),
        (170.0, 190.0),
    );
    // A bbox ending at the raster's west edge: that zero-width piece goes.
    close(
        lon_arc((-180.0, 350.0), (170.0, 20.0), Some(180.0)),
        (180.0, 190.0),
    );
    // TM35FIN, 25,60,24,61 over a raster at 23.9–26.1°E: the raster's
    // side, the gap 24–25°E inside it.
    close(
        lon_arc((25.0, 359.0), (23.9, 2.2), Some(27.0)),
        (23.9, 26.1),
    );
    // One piece, a bbox inside the envelope, and none.
    close(lon_arc((24.5, 1.0), (24.0, 2.0), Some(27.0)), (24.5, 25.5));
    assert_eq!(lon_arc((100.0, 10.0), (24.0, 2.0), Some(27.0)), None);

    // The same through the engine: the cells span the raster, not the bbox,
    // and those whose footprint lies wholly in the gap are null.
    let seam = Fixture::new(
        "two-pieces",
        tm_at(180.0),
        [170.0, 10.0, 190.0, 20.0],
        10_000.0,
    );
    let [w, _, e, _] = seam.geo_transform().footprint_envelope().unwrap();
    for (coords, gap) in [
        ("-179,10,179,20", (179.0, 181.0)),
        ("-180,10,170,20", (170.0, 180.0)),
    ] {
        let result = seam.area(coords).unwrap();
        let (x, y) = axes(&result);
        // About 21° of 0.09° cells, not the bbox's 350° or more.
        let unrolled = |lon: f64| if lon < 0.0 { lon + 360.0 } else { lon };
        assert!(x.len() < 300, "{coords}: {} cells wide", x.len());
        assert!(
            unrolled(x[0]) >= w && unrolled(x[x.len() - 1]) <= e,
            "{coords}"
        );
        let values = values(&result);
        let half = (x[1] - x[0]) / 2.0;
        for (i, &lon) in x.iter().enumerate() {
            if unrolled(lon) - half > gap.0 && unrolled(lon) + half < gap.1 {
                assert!(
                    (0..y.len()).all(|j| values[j * x.len() + i].is_none()),
                    "{coords}: cells at {lon} lie in the bbox's gap"
                );
            }
        }
        assert!(values.iter().any(Option::is_some), "{coords}");
        eprintln!("{coords} over 170–190°E: {} × {} cells", x.len(), y.len());
    }

    let tm35 = Fixture::new(
        "two-pieces-tm35",
        tm35fin(),
        [24.0, 60.0, 26.0, 61.0],
        1_000.0,
    );
    let result = tm35.area("25,60,24,61").unwrap();
    let (x, y) = axes(&result);
    let values = values(&result);
    let gt = tm35.geo_transform();
    let [w, _, e, _] = gt.footprint_envelope().unwrap();
    assert!(
        x[0] >= w && x[x.len() - 1] <= e,
        "the cells span the raster"
    );
    for (j, &lat) in y.iter().enumerate() {
        for (i, &lon) in x.iter().enumerate() {
            let value = values[j * x.len() + i];
            let half = (x[1] - x[0]) / 2.0;
            if lon - half > 24.0 && lon + half < 25.0 {
                assert_eq!(value, None, "({lon}, {lat}) lies in the bbox's gap");
            } else {
                assert!(
                    pixel_values(&gt, lon, lat).contains(&value)
                        || (value.is_none() && !surely_on_raster(&gt, lon, lat)),
                    "({lon}, {lat}) holds {value:?}"
                );
            }
        }
    }
    eprintln!("25,60,24,61 over 24–26°E: {} × {} cells", x.len(), y.len());
}
