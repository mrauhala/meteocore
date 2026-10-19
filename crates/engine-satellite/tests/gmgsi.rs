//! engine-satellite over NOAA's GMGSI global mosaic, on a decimated copy of
//! a real longwave-IR file (`testdata/gmgsi`, 60 × 102 pixels of 3.5°): the
//! spherical-Mercator grid recognised from the file's 2-D lat/lon arrays,
//! renders across its seam at 180° and in projected views, and EDR over
//! display counts.
//!
//! The reference is the file itself: each output pixel's value is checked
//! against the pixel whose `lat`/`lon` centre is nearest, read with
//! netcdf-reader, never through the engine's grid.

use std::path::{Path, PathBuf};

use ds_core::config::{SatelliteConfig, SatelliteProductConfig};
use ds_core::edr_engine::EdrEngine;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterTile};
use ds_core::model::{CoverageResponse, DomainDescription, QueryResult};
use ds_core::web_mercator::lat_to_y;
use engine_satellite::SatelliteEngine;
use netcdf_reader::NcFile;

const LW: &str = "GLOBCOMPLIR_v3r0_blend_s202609281200000_e202609281209599_c202609281234509.nc";

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/gmgsi")
        .join(LW)
}

fn config(dir: &Path) -> SatelliteConfig {
    SatelliteConfig {
        provider: "gmgsi".into(),
        data_path: Some(dir.to_string_lossy().into_owned()),
        endpoint: None,
        bucket: None,
        time_window: None,
        poll_interval_secs: 60,
        products: vec![SatelliteProductConfig {
            parameter: "ir_longwave".into(),
            title: "Longwave IR (display counts)".into(),
            unit: "1".into(),
            product: "LW".into(),
            band: None,
            variable: "data".into(),
        }],
        composites: Vec::new(),
    }
}

/// The fixture in a directory nested like the bucket.
fn engine() -> (SatelliteEngine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("GMGSI_LW/2026/09/28/12");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::copy(fixture(), nested.join(LW)).unwrap();
    let engine = SatelliteEngine::new("gmgsi-global", &config(dir.path())).unwrap();
    engine.poll_once();
    (engine, dir)
}

/// The file's own arrays: each column's longitude, each row's latitude
/// (the arrays are separable) and the counts, row-major.
struct Reference {
    lon: Vec<f64>,
    lat: Vec<f64>,
    data: Vec<f64>,
}

impl Reference {
    fn read() -> Self {
        let nc = NcFile::open(fixture()).unwrap();
        let lon = nc.read_variable_as_f64("lon").unwrap();
        let lat = nc.read_variable_as_f64("lat").unwrap();
        let data = nc.read_variable_as_f64("data").unwrap();
        let (ny, nx) = (lat.shape()[0], lat.shape()[1]);
        Reference {
            lon: (0..nx).map(|c| lon[[0, c]]).collect(),
            lat: (0..ny).map(|r| lat[[r, 0]]).collect(),
            data: data.iter().copied().collect(),
        }
    }

    fn value(&self, row: usize, col: usize) -> f64 {
        self.data[row * self.lon.len() + col]
    }

    /// The value of the pixel whose centre is nearest `(lon, lat)`.
    fn at(&self, lon: f64, lat: f64) -> Option<f64> {
        let (row, col) = self.nearest(lon, lat)?;
        Some(self.value(row, col))
    }

    /// The value a zoomed-out render reads at `(lon, lat)`: the overview
    /// cell holding the nearest pixel keeps its centre pixel, every
    /// `OVERVIEW_FACTOR` (4)th from pixel 2, clamped to the grid.
    fn overview_at(&self, lon: f64, lat: f64) -> Option<f64> {
        let (row, col) = self.nearest(lon, lat)?;
        let centre = |i: usize, n: usize| (i / 4 * 4 + 2).min(n - 1);
        Some(self.value(centre(row, self.lat.len()), centre(col, self.lon.len())))
    }

    /// `(row, col)` of the pixel whose centre is nearest `(lon, lat)`: by
    /// longitude around the globe (so the seam's sliver splits between the
    /// last column and the first), by Mercator northing down the rows;
    /// `None` past the first or last row's half pixel, or for no point (a
    /// projected output's corner outside its domain).
    fn nearest(&self, lon: f64, lat: f64) -> Option<(usize, usize)> {
        if !(lon.is_finite() && lat.is_finite()) {
            return None;
        }
        let turn = |d: f64| (d + 540.0).rem_euclid(360.0) - 180.0;
        let col = (0..self.lon.len())
            .min_by(|&a, &b| {
                turn(self.lon[a] - lon)
                    .abs()
                    .total_cmp(&turn(self.lon[b] - lon).abs())
            })
            .unwrap();
        let y = lat_to_y(lat);
        let rows: Vec<f64> = self.lat.iter().map(|&l| lat_to_y(l)).collect();
        let half = (rows[0] - rows[1]) / 2.0;
        if y > rows[0] + half || y < rows[rows.len() - 1] - half {
            return None;
        }
        let row = (0..rows.len())
            .min_by(|&a, &b| (rows[a] - y).abs().total_cmp(&(rows[b] - y).abs()))
            .unwrap();
        Some((row, col))
    }
}

fn render(engine: &SatelliteEngine, bbox: [f64; 4], w: u32, h: u32, crs: &OutputCrs) -> RasterTile {
    engine
        .get_raster_tile(bbox, w, h, None, crs, None, None, None)
        .unwrap()
}

/// How many of a render's pixels differ from the file's value at their
/// centre (`at(fx, fy)` gives the centre's lon/lat), of those the file
/// covers; and how many of those have no value at all.
fn mismatches(
    tile: &RasterTile,
    reference: &Reference,
    at: impl Fn(f64, f64) -> (f64, f64),
) -> (usize, usize, usize) {
    mismatches_by(tile, |lon, lat| reference.at(lon, lat), at)
}

/// [`mismatches`] against any expected value: `expected(lon, lat)`.
fn mismatches_by(
    tile: &RasterTile,
    expected: impl Fn(f64, f64) -> Option<f64>,
    at: impl Fn(f64, f64) -> (f64, f64),
) -> (usize, usize, usize) {
    let (w, h) = (tile.width as usize, tile.height as usize);
    let (mut covered, mut wrong, mut missing) = (0, 0, 0);
    for oy in 0..h {
        for ox in 0..w {
            let (lon, lat) = at((ox as f64 + 0.5) / w as f64, (oy as f64 + 0.5) / h as f64);
            let Some(expected) = expected(lon, lat) else {
                continue;
            };
            covered += 1;
            match tile.values.value_at(oy * w + ox) {
                Some(value) => wrong += usize::from(value != expected),
                None => missing += 1,
            }
        }
    }
    (covered, wrong, missing)
}

#[test]
fn discovers_the_hourly_mosaic_as_a_global_mercator_grid() {
    let (engine, _dir) = engine();
    let info = engine.raster_info();
    assert_eq!(info.native_crs, "EPSG:3857");
    assert_eq!(
        info.times,
        ["2026-09-28T12:00:00Z"
            .parse::<chrono::DateTime<chrono::Utc>>()
            .unwrap()]
    );
    assert_eq!(info.unit, "1");
    assert_eq!(info.grid_size, Some([102, 60]));
    // Every longitude, between the first and last rows' outer edges.
    let [w, s, e, n] = info.spatial_extent.unwrap();
    assert_eq!((w, e), (-180.0, 180.0));
    assert!((72.8..73.5).contains(&n), "north edge past the first row");
    assert!((-72.5..-71.7).contains(&s), "south edge past the last row");
}

/// A 3×3 render centred on a pixel's centre (the file's lat/lon) returns
/// that pixel's count: at the corners, either side of the seam, in the
/// middle, and a turn further east.
#[test]
fn renders_the_file_counts_at_their_pixel_centres() {
    let (engine, _dir) = engine();
    let reference = Reference::read();
    for (row, col) in [
        (0, 0),
        (0, 101),
        (59, 0),
        (59, 101),
        (30, 51),
        (10, 30),
        (45, 80),
    ] {
        let (lon, lat) = (reference.lon[col], reference.lat[row]);
        let expected = reference.value(row, col);
        for (turn, lon) in [lon, lon + 360.0].into_iter().enumerate() {
            let d = 0.01;
            let tile = render(
                &engine,
                [lon - d, lat - d, lon + d, lat + d],
                3,
                3,
                &OutputCrs::Wgs84,
            );
            assert_eq!(
                tile.values.value_at(4),
                Some(expected),
                "pixel ({row}, {col}), turn {turn}"
            );
        }
    }
}

/// The canonical seam box, 170°E → 170°W, as a client wrapping the world
/// requests it (east past 180°): continuous, no gap at the seam, and each
/// pixel the file's value — in CRS84 and in Web Mercator.
#[test]
fn renders_continuously_across_the_seam() {
    let (engine, _dir) = engine();
    let reference = Reference::read();
    let bbox = [170.0, 10.0, 190.0, 20.0];
    for crs in [OutputCrs::Wgs84, OutputCrs::WebMercator] {
        let tile = render(&engine, bbox, 128, 64, &crs);
        let (covered, wrong, missing) =
            mismatches(&tile, &reference, |fx, fy| crs.project_node(bbox, fx, fy));
        assert_eq!(covered, 128 * 64, "{crs:?}");
        assert_eq!(missing, 0, "{crs:?}: a gap");
        // Pixel edges may differ within the projection grid's 0.2 px.
        assert!(wrong * 50 < covered, "{crs:?}: {wrong} of {covered} differ");
    }
}

/// The whole world, a turn wide and wider, at full resolution and zoomed
/// out onto the overview: every pixel on the mosaic's latitudes has the
/// file's value, none wrong at the seam or anywhere else. Web Mercator
/// output maps linearly onto the grid, so the coarse projection grid is
/// exact and every pixel must match.
#[test]
fn renders_the_world_and_its_wrapped_copies() {
    let (engine, _dir) = engine();
    let reference = Reference::read();
    for (k, bbox) in [
        [-180.0, -85.0, 180.0, 85.0],
        [0.0, -85.0, 360.0, 85.0],
        [-540.0, -85.0, 540.0, 85.0],
    ]
    .into_iter()
    .enumerate()
    {
        // 64 px over 102 columns reads the grid; 16 px, the overview.
        for (width, height, overview) in [(64, 32, false), (16, 8, true)] {
            let tile = render(&engine, bbox, width, height, &OutputCrs::WebMercator);
            let (covered, wrong, missing) = mismatches_by(
                &tile,
                |lon, lat| {
                    if overview {
                        reference.overview_at(lon, lat)
                    } else {
                        reference.at(lon, lat)
                    }
                },
                |fx, fy| OutputCrs::WebMercator.project_node(bbox, fx, fy),
            );
            let label = format!("box {k}, overview {overview}");
            // 72°S–72°N is 60 % of the Web Mercator square.
            assert!(covered * 3 > (width * height) as usize, "{label}");
            assert_eq!(missing, 0, "{label}");
            assert_eq!(wrong, 0, "{label}");
        }
    }
}

/// Zoomed out, a pixel in the eastern half of the seam's sliver reads the
/// first column's overview cell, as the full grid reads the first column
/// there (#906 review). The overview's last cell reaches past the turn:
/// resolved at the overview's own scale, that half read the last cell.
#[test]
fn a_zoomed_out_render_splits_the_seam_like_the_grid() {
    let (engine, _dir) = engine();
    let reference = Reference::read();
    let (nx, ny) = (reference.lon.len(), reference.lat.len());
    // The grid's spacing and turn, from the file's first and last columns.
    let east = (reference.lon[nx - 1] - reference.lon[0]).rem_euclid(360.0);
    let step = east / (nx - 1) as f64;
    let period = 360.0 / step;
    // Three quarters of the way across the sliver [nx, period), in
    // pixels from the grid's west edge.
    let at = nx as f64 + 0.75 * (period - nx as f64);
    let lon = reference.lon[0] + (at - 0.5) * step - 360.0;
    // A row whose first-column and last-column cells differ.
    let row = (0..ny / 4)
        .map(|j| j * 4 + 2)
        .find(|&r| reference.value(r, 2) != reference.value(r, nx - 1))
        .expect("the edge cells differ in some row");
    let lat = reference.lat[row];
    assert_eq!(reference.nearest(lon, lat), Some((row, 0)));
    // Three pixels of 4.5 columns each: the overview, the middle pixel
    // centred on `lon`.
    let half = 1.5 * 4.5 * step;
    let tile = render(
        &engine,
        [lon - half, lat - 0.01, lon + half, lat + 0.01],
        3,
        1,
        &OutputCrs::Wgs84,
    );
    assert_eq!(tile.values.value_at(1), Some(reference.value(row, 2)));
    assert_eq!(
        tile.values.value_at(1),
        reference.overview_at(lon, lat),
        "the overview reference agrees"
    );
}

/// Several products of one GMGSI collection render together (one grid,
/// one coordinate map), each band exactly as its own render: across the
/// seam, and zoomed out onto the overview.
#[test]
fn multi_band_renders_match_single_bands() {
    let dir = tempfile::tempdir().unwrap();
    for (product, band) in [("LW", "LIR"), ("WV", "WV")] {
        let nested = dir.path().join(format!("GMGSI_{product}/2026/09/28/12"));
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::copy(fixture(), nested.join(LW.replace("LIR", band))).unwrap();
    }
    let mut config = config(dir.path());
    config.products.push(SatelliteProductConfig {
        parameter: "water_vapour".into(),
        title: "Water vapour (display counts)".into(),
        unit: "1".into(),
        product: "WV".into(),
        band: None,
        variable: "data".into(),
    });
    let engine = SatelliteEngine::new("gmgsi-bands", &config).unwrap();
    engine.poll_once();
    let parameters = ["ir_longwave", "water_vapour"];
    for (bbox, width, height) in [
        ([170.0, 10.0, 190.0, 20.0], 128, 64),
        ([-180.0, -85.0, 180.0, 85.0], 16, 8),
    ] {
        let tiles = engine
            .get_raster_tiles(
                bbox,
                width,
                height,
                None,
                &OutputCrs::WebMercator,
                &parameters,
                None,
                None,
            )
            .unwrap();
        assert_eq!(tiles.len(), 2);
        for (tile, parameter) in tiles.iter().zip(parameters) {
            let single = engine
                .get_raster_tile(
                    bbox,
                    width,
                    height,
                    None,
                    &OutputCrs::WebMercator,
                    Some(parameter),
                    None,
                    None,
                )
                .unwrap();
            assert!(
                tile.values.iter_values().eq(single.values.iter_values()),
                "{parameter}"
            );
            assert!(
                tile.values.iter_values().any(|v| v.is_some()),
                "{parameter}"
            );
        }
    }
}

/// A projected view reaching round to the far side of the globe: LAEA
/// Europe (EPSG:3035) returns longitudes within ±180° of 10°E, so they
/// jump a turn along 170°W inside the image. Cells across that cut still
/// read the file's pixels.
#[test]
fn renders_a_projected_view_across_the_output_longitude_cut() {
    let (engine, _dir) = engine();
    let reference = Reference::read();
    let crs = ds_core::geo::projected_output_crs("EPSG:3035").unwrap();
    // ±12 000 km around the projection centre: most of the globe.
    let (e0, n0) = (4_321_000.0, 3_210_000.0);
    let proj = [e0 - 12.0e6, n0 - 12.0e6, e0 + 12.0e6, n0 + 12.0e6];
    let output = OutputCrs::Projected {
        crs: crs.clone(),
        bbox: proj,
    };
    let world = [-180.0, -90.0, 180.0, 90.0];
    let at = |ox: usize, oy: usize| {
        output.project_node(world, (ox as f64 + 0.5) / 96.0, (oy as f64 + 0.5) / 96.0)
    };
    // The cut runs through the mosaic in the image: side by side, two
    // pixel centres on it whose longitudes differ by more than half a turn.
    let cut = (0..96)
        .flat_map(|oy| (0..95).map(move |ox| (ox, oy)))
        .filter(|&(ox, oy)| {
            let (a, b) = (at(ox, oy), at(ox + 1, oy));
            (a.0 - b.0).abs() > 180.0
                && reference.at(a.0, a.1).is_some()
                && reference.at(b.0, b.1).is_some()
        })
        .count();
    assert!(cut > 5, "{cut} pixel pairs straddle the cut");

    let tile = render(&engine, world, 96, 96, &output);
    let (covered, wrong, missing) = mismatches(&tile, &reference, |fx, fy| {
        output.project_node(world, fx, fy)
    });
    assert!(covered > 96 * 96 / 2, "{covered}");
    // A render sweeping the grid across the cut would get whole columns
    // wrong; nearest-neighbour pixel edges may differ.
    assert!(wrong * 50 < covered, "{wrong} of {covered} differ");
    // The boundary refinement may leave ~2 px unfilled along the rim of
    // LAEA's domain (the antipode's circle), and only there.
    assert!(missing * 50 < covered, "{missing} of {covered} missing");
    for oy in 0..96 {
        for ox in 0..96 {
            let (lon, lat) = at(ox, oy);
            if tile.values.value_at(oy * 96 + ox).is_none() && reference.at(lon, lat).is_some() {
                let (e, n) = (
                    proj[0] + (ox as f64 + 0.5) / 96.0 * (proj[2] - proj[0]),
                    proj[3] - (oy as f64 + 0.5) / 96.0 * (proj[3] - proj[1]),
                );
                let rim = 2.0 * ds_core::geo::WGS84_A;
                let pixel = (proj[2] - proj[0]) / 96.0;
                assert!(
                    ((e - e0).hypot(n - n0) - rim).abs() < 3.0 * pixel,
                    "pixel ({ox}, {oy}) missing inside the domain"
                );
            }
        }
    }
}

fn single(response: CoverageResponse) -> QueryResult {
    match response {
        CoverageResponse::Single(result) => result,
        other => panic!("expected one coverage, got {other:?}"),
    }
}

/// EDR over counts: a position reads the pixel under it, either side of
/// the seam, with unit "1"; off the mosaic's latitudes is not found.
#[test]
fn edr_position_reads_counts() {
    let (engine, _dir) = engine();
    let reference = Reference::read();
    for (row, col) in [(20, 0), (20, 101), (30, 51)] {
        let (lon, lat) = (reference.lon[col], reference.lat[row]);
        let lon = (lon + 540.0).rem_euclid(360.0) - 180.0;
        let result = single(
            engine
                .query_position(&format!("POINT({lon} {lat})"), None, None, None, None)
                .unwrap(),
        );
        assert_eq!(
            result.ranges["ir_longwave"].values,
            [Some(reference.value(row, col))],
            "pixel ({row}, {col})"
        );
        assert_eq!(result.parameters["ir_longwave"].unit, "1");
    }
    assert!(matches!(
        engine.query_position("POINT(25 80)", None, None, None, None),
        Err(ds_core::error::DataServerError::LocationNotFound(_))
    ));
}

/// An EDR area across the seam, given west > east: every cell of the grid
/// has a count.
#[test]
fn edr_area_across_the_seam() {
    let (engine, _dir) = engine();
    let result = single(
        engine
            .query_area("170,10,-170,20", None, None, None, None)
            .unwrap(),
    );
    let DomainDescription::Grid { x, y, .. } = &result.domain else {
        panic!("expected a grid");
    };
    assert!(x.len() >= 5 && y.len() >= 2, "a grid of cells");
    let values = &result.ranges["ir_longwave"].values;
    assert_eq!(values.len(), x.len() * y.len());
    assert!(
        values
            .iter()
            .all(|v| v.is_some_and(|v| (0.0..=255.0).contains(&v))),
        "a count in every cell"
    );
}

/// The shipped example collection parses, validates and builds an engine.
#[test]
fn example_collection_config_is_valid() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../collections.d/gmgsi-global.toml");
    let collection: ds_core::config::CollectionConfig =
        toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(collection.engine_type, "satellite");
    assert_eq!(collection.apis, ["edr", "wms", "maps", "tiles"]);
    let satellite = collection.satellite.as_ref().unwrap();
    assert_eq!(satellite.provider, "gmgsi");
    ds_core::config::validate_satellite(&collection.id, satellite).unwrap();
    let engine = SatelliteEngine::new(&collection.id, satellite).unwrap();
    let info = engine.raster_info();
    assert_eq!(info.native_crs, "EPSG:3857");
    assert!(info.parameters.iter().all(|p| p.unit == "1"));
    assert!(
        info.times.is_empty(),
        "nothing is fetched before the first poll"
    );
}
