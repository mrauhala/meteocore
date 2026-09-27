//! engine-satellite on Himawari-9 ISatSS (`testdata/himawari9-isatss`):
//! three real band 13 tiles of one full-disk scan, cropped to 64 × 64
//! around a lattice corner on the limb whose fourth cell is space and has
//! no tile. Discovery groups the tiles into one scan, and renders and EDR
//! positions return the tiles' values, checked against each tile decoded
//! independently with its own coordinates and projection attributes
//! (µrad scan angles, sweep y).

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use ds_core::config::{SatelliteConfig, SatelliteProductConfig};
use ds_core::edr_engine::EdrEngine;
use ds_core::geo::{Crs, SweepAxis};
use ds_core::map_engine::{MapEngine, OutputCrs};
use ds_core::model::CoverageResponse;
use engine_satellite::SatelliteEngine;
use netcdf_reader::{NcFile, NcVariable};

const TILES: [&str; 3] = [
    "OR_HFD-020-B12-M1C13-T001_GH9_s20262701920000_c20262701928140.nc",
    "OR_HFD-020-B12-M1C13-T007_GH9_s20262701920000_c20262701928130.nc",
    "OR_HFD-020-B12-M1C13-T008_GH9_s20262701920000_c20262701928130.nc",
];

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/himawari9-isatss")
        .join(name)
}

fn config(dir: Option<&Path>) -> SatelliteConfig {
    SatelliteConfig {
        provider: "isatss".into(),
        data_path: dir.map(|d| d.to_string_lossy().into_owned()),
        endpoint: dir
            .is_none()
            .then(|| "https://s3.us-east-1.amazonaws.com".into()),
        bucket: dir.is_none().then(|| "noaa-himawari9".into()),
        time_window: dir.is_none().then(|| "-PT1H".into()),
        poll_interval_secs: 60,
        products: vec![SatelliteProductConfig {
            parameter: "ir_10_4".into(),
            title: "IR 10.4 µm brightness temperature".into(),
            unit: "K".into(),
            product: "HFD".into(),
            band: Some(13),
            variable: "Sectorized_CMI".into(),
        }],
    }
}

/// The tiles in a temp directory nested like the bucket's scan directory.
fn engine() -> (SatelliteEngine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("AHI-L2-FLDK-ISatSS/2026/09/27/1920");
    std::fs::create_dir_all(&nested).unwrap();
    for tile in TILES {
        std::fs::copy(fixture(tile), nested.join(tile)).unwrap();
    }
    let engine = SatelliteEngine::new("himawari9-fd", &config(Some(dir.path()))).unwrap();
    engine.poll_once();
    (engine, dir)
}

/// Longitude, latitude and value of the tile's on-disk pixel with the
/// warmest value, decoded straight from the file; `None` when no pixel
/// centre of the tile is on the disk.
fn warmest_pixel(tile: &str) -> Option<(f64, f64, f64)> {
    let nc = NcFile::open(fixture(tile)).unwrap();
    let attr = |v: &NcVariable, a: &str| v.attribute(a).and_then(|x| x.value.as_f64()).unwrap();
    let projection = nc.variable("fixedgrid_projection").unwrap();
    let h = attr(projection, "perspective_point_height");
    let crs = Crs::Geostationary {
        lon0: attr(projection, "longitude_of_projection_origin").to_radians(),
        height: h,
        semi_major: attr(projection, "semi_major"),
        semi_minor: attr(projection, "semi_minor"),
        sweep: SweepAxis::Y,
    };
    // Scan angles in metres: packed absolute grid index × µrad scale +
    // offset.
    let axis = |name: &str| -> Vec<f64> {
        let v = nc.variable(name).unwrap();
        let (scale, offset) = (attr(v, "scale_factor"), attr(v, "add_offset"));
        let raw = nc.read_variable_as_f64(name).unwrap();
        raw.iter()
            .map(|p| (p * scale + offset) * 1e-6 * h)
            .collect()
    };
    let (x, y) = (axis("x"), axis("y"));
    let cmi = nc.variable("Sectorized_CMI").unwrap();
    let (scale, offset) = (attr(cmi, "scale_factor"), attr(cmi, "add_offset"));
    let values = nc.read_variable_as_f64("Sectorized_CMI").unwrap();
    let (mut best, mut at) = (f64::MIN, (0, 0));
    for row in 0..y.len() {
        for col in 0..x.len() {
            let value = values[[row, col]] * scale + offset;
            if value > best && crs.inverse(x[col], y[row]).is_some() {
                (best, at) = (value, (row, col));
            }
        }
    }
    let (lon, lat) = crs.inverse(x[at.1], y[at.0])?;
    Some((lon, lat, best))
}

#[test]
fn discovers_one_scan_of_three_tiles() {
    let (engine, _dir) = engine();
    let info = engine.raster_info();
    let scan: DateTime<Utc> = "2026-09-27T19:20:00Z".parse().unwrap();
    assert_eq!(info.times, [scan]);
    assert_eq!(&*engine.parameter_times("ir_10_4").unwrap(), [scan]);
    // The 2 × 2 lattice of 64-pixel tiles, one cell without a tile.
    assert_eq!(info.grid_size, Some([128, 128]));
    let [west, south, east, north] = info.spatial_extent.unwrap();
    assert!(west < east && south < north, "{:?}", info.spatial_extent);
    // The north-west limb of a disk centred on 140.7°E.
    assert!((60.0..120.0).contains(&west) && (40.0..80.0).contains(&north));
}

/// A 3×3 render centred on the warmest on-disk pixel of each tile on the
/// disk returns it: T007 and T008, the two lattice columns. T001's crop
/// lies wholly beyond the limb (its warm values are the atmosphere a line
/// of sight crosses there), so no map point reaches it; the frame's unit
/// test pins its place in the mosaic.
#[test]
fn renders_each_tile_of_the_mosaic() {
    let (engine, _dir) = engine();
    assert_eq!(warmest_pixel(TILES[0]), None);
    for tile in &TILES[1..] {
        let (lon, lat, expected) = warmest_pixel(tile).unwrap();
        assert!((250.0..330.0).contains(&expected), "{tile}: {expected} K");
        let d = 0.0005;
        let raster = engine
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
        let value = raster
            .values
            .value_at(4)
            .unwrap_or_else(|| panic!("{tile}: no value at {lon} {lat}"));
        assert!(
            (value - expected).abs() < 1e-6,
            "{tile}: {value} vs {expected}"
        );
    }
    // Zoomed out, the overview renders the disk too.
    let [w, s, e, n] = engine.raster_info().spatial_extent.unwrap();
    let raster = engine
        .get_raster_tile(
            [w, s, e, n],
            8,
            8,
            None,
            &OutputCrs::WebMercator,
            None,
            None,
            None,
        )
        .unwrap();
    assert!((0..64).any(|i| raster.values.value_at(i).is_some()));
}

#[test]
fn edr_position_reads_the_tile_under_the_point() {
    let (engine, _dir) = engine();
    let (lon, lat, expected) = warmest_pixel(TILES[2]).unwrap();
    let CoverageResponse::Single(result) = engine
        .query_position(&format!("POINT({lon} {lat})"), None, None, None, None)
        .unwrap()
    else {
        panic!("expected one coverage");
    };
    let value = result.ranges["ir_10_4"].values[0].expect("a value");
    assert!((value - expected).abs() < 1e-6, "{value} vs {expected}");
}

#[test]
fn isatss_configs_are_checked() {
    // A bucket source: a band per product, a window of at most 6 hours.
    let mut bucket = config(None);
    assert!(SatelliteEngine::new("h9", &bucket).is_ok());
    bucket.time_window = Some("-PT7H".into());
    assert!(SatelliteEngine::new("h9", &bucket).is_err());
    let mut no_band = config(None);
    no_band.products[0].band = None;
    assert!(ds_core::config::validate_satellite("h9", &no_band).is_err());

    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../collections.d/himawari9-fd.toml");
    let collection: ds_core::config::CollectionConfig =
        toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let satellite = collection.satellite.as_ref().unwrap();
    assert_eq!(satellite.provider, "isatss");
    let engine = SatelliteEngine::new(&collection.id, satellite).unwrap();
    assert!(engine.raster_info().times.is_empty());
}
