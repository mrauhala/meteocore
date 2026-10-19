//! engine-satellite on KMA GK2A AMI L1B (`testdata/gk2a-ami`): a 96 × 160
//! window of a real IR105 full disk straddling 180° at 14–16°N, with the
//! file's own CGMS navigation, calibration and original count words.
//!
//! The expected values are independent of this code: pixel centres from
//! the CGMS scan angles through `cs2cs +proj=geos +h=35785863 +lon_0=128.2
//! +a=6378137 +b=6356752.3 +sweep=y +units=m +to +proj=longlat +a=6378137
//! +b=6356752.3` (PROJ 9.8.1), and brightness temperatures from the
//! window's words and the file's coefficients, computed in Python with the
//! manual's IR105 wavenumber (966.153383926055 cm⁻¹).

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use ds_core::config::{SatelliteConfig, SatelliteProductConfig};
use ds_core::edr_engine::EdrEngine;
use ds_core::map_engine::{MapEngine, OutputCrs};
use ds_core::model::{CoverageResponse, DomainDescription};
use engine_satellite::SatelliteEngine;

const FILE: &str = "gk2a_ami_le1b_ir105_fd020ge_202609281200.nc";

/// Stored values are centi-kelvin: half a step.
const QUANTUM: f64 = 0.005 + 1e-9;

/// (window row, window column, longitude, latitude, count word, kelvin):
/// the corners, the two pixels either side of 180° on row 47, and one more.
const PIXELS: [(u32, u32, f64, f64, u16, f64); 5] = [
    (0, 0, 177.399_832_602_1, 15.869_004_593_4, 3286, 294.396_639),
    (
        47,
        79,
        179.990_093_483_9,
        15.006_660_814_2,
        3652,
        289.640_057,
    ),
    (
        47,
        80,
        -179.970_165_644_9,
        15.008_124_471_6,
        3806,
        287.569_999,
    ),
    (
        60,
        130,
        -178.038_123_584_0,
        14.813_639_910_5,
        3516,
        291.433_382,
    ),
    (
        95,
        159,
        -177.130_292_331_6,
        14.130_534_110_3,
        3441,
        292.409_022,
    ),
];

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/gk2a-ami")
        .join(FILE)
}

fn config(dir: Option<&Path>) -> SatelliteConfig {
    SatelliteConfig {
        provider: "gk2a".into(),
        data_path: dir.map(|d| d.to_string_lossy().into_owned()),
        endpoint: dir
            .is_none()
            .then(|| "https://s3.us-east-1.amazonaws.com".into()),
        bucket: dir.is_none().then(|| "noaa-gk2a-pds".into()),
        time_window: dir.is_none().then(|| "-PT1H".into()),
        poll_interval_secs: 60,
        composites: Vec::new(),
        products: vec![SatelliteProductConfig {
            parameter: "ir_10_5".into(),
            title: "IR 10.5 µm brightness temperature".into(),
            unit: "K".into(),
            product: "FD".into(),
            band: Some(13),
            variable: "image_pixel_values".into(),
        }],
    }
}

/// The window in a temp directory nested like the bucket's hour prefix.
fn engine() -> (SatelliteEngine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("AMI/L1B/FD/202609/28/12");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::copy(fixture(), nested.join(FILE)).unwrap();
    let engine = SatelliteEngine::new("gk2a-fd", &config(Some(dir.path()))).unwrap();
    engine.poll_once();
    (engine, dir)
}

fn render(
    engine: &SatelliteEngine,
    bbox: [f64; 4],
    size: (u32, u32),
    crs: &OutputCrs,
) -> Vec<Option<f64>> {
    let tile = engine
        .get_raster_tile(bbox, size.0, size.1, None, crs, None, None, None)
        .unwrap();
    (0..(size.0 * size.1) as usize)
        .map(|i| tile.values.value_at(i))
        .collect()
}

#[test]
fn discovers_the_scan_and_its_extent_across_the_seam() {
    let (engine, _dir) = engine();
    let info = engine.raster_info();
    let slot: DateTime<Utc> = "2026-09-28T12:00:00Z".parse().unwrap();
    assert_eq!(info.times, [slot]);
    assert_eq!(&*engine.parameter_times("ir_10_5").unwrap(), [slot]);
    assert_eq!(info.grid_size, Some([160, 96]));
    // The window's corners are at 176.62°E–176.07°W, 13.91–16.14°N
    // (cs2cs of its pixel edges): west > east.
    let [west, south, east, north] = info.spatial_extent.unwrap();
    assert!((176.55..176.7).contains(&west), "west {west}");
    assert!((-176.15..-176.0).contains(&east), "east {east}");
    assert!((13.85..13.95).contains(&south), "south {south}");
    assert!((16.1..16.2).contains(&north), "north {north}");
}

/// Each reference pixel, as an EDR position and as the centre of a 3 × 3
/// render, reads the brightness temperature of its own count.
#[test]
fn pixels_are_where_proj_puts_them_with_their_own_temperature() {
    let (engine, _dir) = engine();
    // The words the temperatures were computed from are the window's.
    let words = netcdf_reader::NcFile::open(fixture())
        .unwrap()
        .read_variable::<u16>("image_pixel_values")
        .unwrap();
    for (row, col, lon, lat, word, kelvin) in PIXELS {
        assert_eq!(words[[row as usize, col as usize]], word, "({row}, {col})");
        let CoverageResponse::Single(result) = engine
            .query_position(&format!("POINT({lon} {lat})"), None, None, None, None)
            .unwrap()
        else {
            panic!("expected one coverage");
        };
        let value = result.ranges["ir_10_5"].values[0].expect("a value");
        assert!(
            (value - kelvin).abs() <= QUANTUM,
            "({row}, {col}): EDR {value} vs {kelvin}"
        );
        let d = 0.0005;
        let values = render(
            &engine,
            [lon - d, lat - d, lon + d, lat + d],
            (3, 3),
            &OutputCrs::Wgs84,
        );
        let value = values[4].unwrap_or_else(|| panic!("({row}, {col}): no value"));
        assert!(
            (value - kelvin).abs() <= QUANTUM,
            "({row}, {col}): render {value} vs {kelvin}"
        );
    }
}

/// The canonical seam box, 170°E–170°W × 10–20°N, as a client wrapping the
/// world sends it (east past 180°) in CRS84 and Web Mercator, and as an
/// EDR area (west > east): both halves carry the window's values, which lie
/// within its range of counts (3147–4746 → 296.15–273.86 K).
#[test]
fn the_seam_box_reads_both_sides() {
    let (engine, _dir) = engine();
    let in_range = |v: f64| (273.85..=296.15).contains(&v);
    for crs in [OutputCrs::Wgs84, OutputCrs::WebMercator] {
        let (width, height) = (80, 40);
        let values = render(&engine, [170.0, 10.0, 190.0, 20.0], (width, height), &crs);
        let covered = |cols: std::ops::Range<u32>| {
            cols.flat_map(|c| (0..height).map(move |r| (r * width + c) as usize))
                .filter_map(|i| values[i])
                .inspect(|&v| assert!(in_range(v), "{crs:?}: {v}"))
                .count()
        };
        assert!(covered(0..width / 2) > 0, "{crs:?}: nothing east of 180°E");
        assert!(
            covered(width / 2..width) > 0,
            "{crs:?}: nothing west of 180°W"
        );
    }

    let CoverageResponse::Single(result) = engine
        .query_area("170,10,-170,20", None, None, None, None)
        .unwrap()
    else {
        panic!("expected one coverage");
    };
    let DomainDescription::Grid { x, .. } = &result.domain else {
        panic!("expected a grid");
    };
    let values = &result.ranges["ir_10_5"].values;
    let (mut east, mut west) = (0, 0);
    for (i, value) in values.iter().enumerate() {
        if let Some(v) = value {
            assert!(in_range(*v), "{v}");
            if x[i % x.len()] > 0.0 {
                east += 1;
            } else {
                west += 1;
            }
        }
    }
    assert!(east > 0 && west > 0, "east {east}, west {west}");
}

/// Zoomed out (1.25° output pixels, 10 source columns each), a render
/// samples the overview built at ingest from the 2-D chunk blocks, and
/// still lands on the window.
#[test]
fn zoomed_out_renders_sample_the_overview() {
    let (engine, _dir) = engine();
    let values = render(
        &engine,
        [170.0, 10.0, 190.0, 20.0],
        (16, 8),
        &OutputCrs::Wgs84,
    );
    let seen: Vec<f64> = values.into_iter().flatten().collect();
    assert!(!seen.is_empty());
    assert!(
        seen.iter().all(|&v| (273.85..=296.15).contains(&v)),
        "{seen:?}"
    );
}

#[test]
fn gk2a_configs_are_checked() {
    // A bucket source: hourly prefixes, a window of at most 24 hours.
    let mut bucket = config(None);
    assert!(SatelliteEngine::new("gk2a", &bucket).is_ok());
    bucket.time_window = Some("-PT25H".into());
    assert!(SatelliteEngine::new("gk2a", &bucket).is_err());
    // Only IR105 has a sourced central wavenumber.
    let mut other_band = config(None);
    other_band.products[0].band = Some(14);
    let err = SatelliteEngine::new("gk2a", &other_band)
        .unwrap_err()
        .to_string();
    assert!(err.contains("only band 13 (IR105)"), "{err}");

    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../collections.d/gk2a-fd.toml");
    let collection: ds_core::config::CollectionConfig =
        toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let satellite = collection.satellite.as_ref().unwrap();
    assert_eq!(satellite.provider, "gk2a");
    let engine = SatelliteEngine::new(&collection.id, satellite).unwrap();
    assert!(engine.raster_info().times.is_empty());
}
