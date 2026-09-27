//! engine-satellite across the antimeridian, on a cropped real GOES-18
//! (GOES-West, 137.0°W) band 13 scan (`testdata/goes18-abi`) that straddles
//! 180°: the extent wraps (west > east), and renders and EDR positions on
//! either side of the seam, and requests reaching past ±180°, return the
//! file's values.

use std::path::{Path, PathBuf};

use ds_core::config::{SatelliteConfig, SatelliteProductConfig};
use ds_core::edr_engine::EdrEngine;
use ds_core::geo::{Crs, SweepAxis};
use ds_core::map_engine::{MapEngine, OutputCrs};
use ds_core::model::CoverageResponse;
use engine_satellite::SatelliteEngine;
use netcdf_reader::{NcFile, NcVariable};

const C13: &str = "OR_ABI-L2-CMIPF-M6C13_G18_s20262701850224_e20262701859544_c20262701900014.nc";

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/goes18-abi")
        .join(C13)
}

fn engine() -> (SatelliteEngine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("ABI-L2-CMIPF/2026/270/18");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::copy(fixture(), nested.join(C13)).unwrap();
    let config = SatelliteConfig {
        provider: "goes-r".into(),
        data_path: Some(dir.path().to_string_lossy().into_owned()),
        endpoint: None,
        bucket: None,
        time_window: None,
        poll_interval_secs: 60,
        products: vec![SatelliteProductConfig {
            parameter: "ir_10_3".into(),
            title: "IR 10.3 µm brightness temperature".into(),
            unit: "K".into(),
            product: "ABI-L2-CMIPF".into(),
            band: Some(13),
            variable: "CMI".into(),
        }],
    };
    let engine = SatelliteEngine::new("goes18-fd", &config).unwrap();
    engine.poll_once();
    (engine, dir)
}

/// Longitude (in [-180, 180)), latitude and value of every pixel in `row`,
/// decoded straight from the file with its own projection attributes: the
/// independent reference the engine is checked against.
fn reference_row(row: usize) -> Vec<(f64, f64, Option<f64>)> {
    let nc = NcFile::open(fixture()).unwrap();
    let attr = |v: &NcVariable, a: &str| v.attribute(a).and_then(|x| x.value.as_f64()).unwrap();
    let projection = nc.variable("goes_imager_projection").unwrap();
    let h = attr(projection, "perspective_point_height");
    let crs = Crs::Geostationary {
        lon0: attr(projection, "longitude_of_projection_origin").to_radians(),
        height: h,
        semi_major: attr(projection, "semi_major_axis"),
        semi_minor: attr(projection, "semi_minor_axis"),
        sweep: SweepAxis::X,
    };
    let unpack = |name: &str| {
        let v = nc.variable(name).unwrap();
        let (scale, offset) = (attr(v, "scale_factor"), attr(v, "add_offset"));
        let raw = nc.read_variable_as_f64(name).unwrap();
        (raw, scale, offset)
    };
    let (x, x_scale, x_offset) = unpack("x");
    let (y, y_scale, y_offset) = unpack("y");
    let cmi = nc.variable("CMI").unwrap();
    let fill = attr(cmi, "_FillValue");
    let (values, scale, offset) = unpack("CMI");
    let y = (y[[row]] * y_scale + y_offset) * h;
    (0..values.shape()[1])
        .map(|col| {
            let (lon, lat) = crs
                .inverse((x[[col]] * x_scale + x_offset) * h, y)
                .expect("the crop lies on the disk");
            let lon = (lon + 540.0).rem_euclid(360.0) - 180.0;
            let raw = values[[row, col]];
            (lon, lat, (raw != fill).then_some(raw * scale + offset))
        })
        .collect()
}

/// The pixel columns of `row` either side of 180°: the last one east of
/// 180°E and the first one west of 180°W.
fn seam(row: &[(f64, f64, Option<f64>)]) -> usize {
    row.windows(2)
        .position(|w| w[0].0 > 0.0 && w[1].0 < 0.0)
        .expect("the crop straddles the antimeridian")
}

#[test]
fn extent_wraps_the_antimeridian() {
    let (engine, _dir) = engine();
    let [west, south, east, north] = engine.raster_info().spatial_extent.unwrap();
    assert!(west > east, "{west} .. {east}");
    assert!((170.0..180.0).contains(&west), "{west}");
    assert!((-180.0..-170.0).contains(&east), "{east}");
    assert!(
        10.0 < south && south < north && north < 17.0,
        "{south} .. {north}"
    );
}

/// A 3×3 render centred on each pixel next to the seam returns that
/// pixel's value, and one centred on 180° itself (its bbox reaching past
/// 180°E) returns one of the two pixels either side.
#[test]
fn renders_the_file_values_either_side_of_the_seam() {
    let (engine, _dir) = engine();
    let row = reference_row(120);
    let seam = seam(&row);
    for &(lon, lat, expected) in &row[seam - 1..=seam + 2] {
        let expected = expected.expect("an interior pixel");
        let d = 0.0005;
        let tile = engine
            .get_raster_tile(
                [lon - d, lat - d, lon + d, lat + d],
                3,
                3,
                None,
                &OutputCrs::Wgs84,
                None,
                None,
                None,
            )
            .unwrap();
        let value = tile
            .values
            .value_at(4)
            .unwrap_or_else(|| panic!("no value at {lon} {lat}"));
        assert!(
            (value - expected).abs() < 1e-6,
            "{value} vs {expected} at {lon}"
        );
    }

    let lat = (row[seam].1 + row[seam + 1].1) / 2.0;
    let d = 0.0005;
    let tile = engine
        .get_raster_tile(
            [180.0 - d, lat - d, 180.0 + d, lat + d],
            3,
            3,
            None,
            &OutputCrs::Wgs84,
            None,
            None,
            None,
        )
        .unwrap();
    let value = tile.values.value_at(4).expect("a value on 180°");
    assert!(
        [row[seam].2, row[seam + 1].2]
            .iter()
            .any(|v| (v.unwrap() - value).abs() < 1e-6),
        "{value} vs {:?} / {:?}",
        row[seam].2,
        row[seam + 1].2
    );
}

/// One image across the seam, as a client wrapping the world requests it
/// (east past 180°), covers both halves, in CRS84 and in Web Mercator.
#[test]
fn a_render_across_the_seam_covers_both_sides() {
    let (engine, _dir) = engine();
    for crs in [OutputCrs::Wgs84, OutputCrs::WebMercator] {
        let (width, height) = (64, 16);
        let tile = engine
            .get_raster_tile(
                [178.0, 12.0, 182.0, 15.0],
                width,
                height,
                None,
                &crs,
                None,
                None,
                None,
            )
            .unwrap();
        let covered = |cols: std::ops::Range<usize>| {
            cols.flat_map(|c| (0..height as usize).map(move |r| r * width as usize + c))
                .filter(|&i| tile.values.value_at(i).is_some())
                .count()
        };
        let half = width as usize / 2;
        assert!(covered(0..half) > 0, "{crs:?}: nothing east of 180°E");
        assert!(
            covered(half..width as usize) > 0,
            "{crs:?}: nothing west of 180°W"
        );
    }
}

/// EDR positions either side of the seam read the pixel under them.
#[test]
fn edr_positions_either_side_of_the_seam() {
    let (engine, _dir) = engine();
    let row = reference_row(120);
    let seam = seam(&row);
    for col in [seam, seam + 1] {
        let (lon, lat, expected) = row[col];
        let point = format!("POINT({lon} {lat})");
        let CoverageResponse::Single(result) = engine
            .query_position(&point, None, None, None, None)
            .unwrap()
        else {
            panic!("expected one coverage");
        };
        let value = result.ranges["ir_10_3"].values[0].expect("a value");
        assert!(
            (value - expected.unwrap()).abs() < 1e-6,
            "{value} vs {expected:?} at {lon}"
        );
    }
}
