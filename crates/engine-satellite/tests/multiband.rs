//! Multi-band renders (#819 phase 4, RGB composites) against the cropped
//! GOES-19 scans in `testdata/goes19-abi`: every band comes from one scan,
//! and each band's pixels are exactly the single-band render's.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use ds_core::config::{SatelliteConfig, SatelliteProductConfig};
use ds_core::error::DataServerError;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterTile};
use engine_satellite::SatelliteEngine;

const C13: &str = "OR_ABI-L2-CMIPF-M6C13_G19_s20262681900199_e20262681909519_c20262681909592.nc";
const ACHT: &str = "OR_ABI-L2-ACHTF-M6_G19_s20262681900199_e20262681909507_c20262681912337.nc";
/// The C13 scan ten minutes later, as the tests republish it.
const C13_LATER: &str =
    "OR_ABI-L2-CMIPF-M6C13_G19_s20262681910199_e20262681919519_c20262681919592.nc";
/// The cloud top scan twenty minutes later, a time C13 does not have.
const ACHT_LATER: &str =
    "OR_ABI-L2-ACHTF-M6_G19_s20262681920199_e20262681929507_c20262681932337.nc";

const IR: &str = "ir_10_3";
const CLOUD: &str = "cloud_top_temperature";

fn at(s: &str) -> DateTime<Utc> {
    s.parse().unwrap()
}

/// An engine over `files` (fixture name, published name), polled once.
fn engine(id: &str, files: &[(&str, &str)]) -> (SatelliteEngine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("ABI-L2/2026/268/19");
    std::fs::create_dir_all(&nested).unwrap();
    for (from, to) in files {
        let fixture: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/goes19-abi")
            .join(from);
        std::fs::copy(fixture, nested.join(to)).unwrap();
    }
    let product = |parameter: &str, product: &str, band, variable: &str| SatelliteProductConfig {
        parameter: parameter.into(),
        title: parameter.into(),
        unit: "K".into(),
        product: product.into(),
        band,
        variable: variable.into(),
    };
    let config = SatelliteConfig {
        provider: "goes-r".into(),
        data_path: Some(dir.path().to_string_lossy().into_owned()),
        endpoint: None,
        bucket: None,
        time_window: None,
        poll_interval_secs: 60,
        composites: Vec::new(),
        products: vec![
            product(IR, "ABI-L2-CMIPF", Some(13), "CMI"),
            product(CLOUD, "ABI-L2-ACHTF", None, "TEMP"),
        ],
    };
    let engine = SatelliteEngine::new(id, &config).unwrap();
    engine.poll_once();
    (engine, dir)
}

fn values(tile: &RasterTile) -> Vec<Option<f64>> {
    tile.values.iter_values().collect()
}

fn bands(
    engine: &SatelliteEngine,
    bbox: [f64; 4],
    size: u32,
    time: Option<DateTime<Utc>>,
    parameters: &[&str],
) -> Result<Vec<RasterTile>, DataServerError> {
    engine.get_raster_tiles(
        bbox,
        size,
        size,
        time,
        &OutputCrs::Wgs84,
        parameters,
        None,
        None,
    )
}

/// Each band of a multi-band render is exactly the single-band render of
/// its parameter at that scan: at full resolution, from the overview, in
/// Web Mercator, and with a band repeated.
#[test]
fn every_band_equals_its_single_band_render() {
    let (engine, _dir) = engine("goes19-bands", &[(C13, C13), (ACHT, ACHT)]);
    let t0 = at("2026-09-25T19:00:00Z");
    // The union of the two crops: each band sees its whole crop, 320 px
    // wide, so 256 px renders full resolution and 48 px the overview.
    let extent = engine.raster_info().spatial_extent.unwrap();
    let parameters = [IR, CLOUD, IR];
    for (size, output_crs) in [
        (256, OutputCrs::Wgs84),
        (48, OutputCrs::Wgs84),
        (128, OutputCrs::WebMercator),
    ] {
        let tiles = engine
            .get_raster_tiles(
                extent,
                size,
                size,
                Some(t0),
                &output_crs,
                &parameters,
                None,
                None,
            )
            .unwrap();
        assert_eq!(tiles.len(), parameters.len());
        for (tile, parameter) in tiles.iter().zip(parameters) {
            let single = engine
                .get_raster_tile(
                    extent,
                    size,
                    size,
                    Some(t0),
                    &output_crs,
                    Some(parameter),
                    None,
                    None,
                )
                .unwrap();
            assert_eq!((tile.width, tile.height), (size, size));
            let got = values(tile);
            assert_eq!(
                got,
                values(&single),
                "{parameter} at {size} px {output_crs:?}"
            );
            let on_disk = got.iter().flatten().count();
            assert!(on_disk > 0, "{parameter} at {size} px renders its crop");
        }
    }
}

/// A composite renders the latest scan every band has, keyed and rendered
/// through the same selection; an explicit time must be a scan of every
/// band.
#[test]
fn a_composite_uses_one_scan_every_band_has() {
    let (engine, _dir) = engine(
        "goes19-bands-time",
        &[(C13, C13), (ACHT, ACHT), (C13, C13_LATER)],
    );
    let (t0, t1) = (at("2026-09-25T19:00:00Z"), at("2026-09-25T19:10:00Z"));
    let resolve =
        |parameters: &[&str], time| engine.resolve_parameters_time(parameters, time, None);
    // IR alone has 19:10; with cloud top temperature the shared scan is 19:00.
    assert_eq!(resolve(&[IR], None), Some(t1));
    assert_eq!(resolve(&[IR, CLOUD], None), Some(t0));
    assert_eq!(resolve(&[CLOUD, IR], Some(t1)), Some(t0));
    assert_eq!(
        resolve(&[IR, CLOUD], Some(at("2026-09-25T19:05:00Z"))),
        Some(t0)
    );
    // Before every scan: the earliest shared one, as a single band snaps.
    assert_eq!(
        resolve(&[IR, CLOUD], Some(at("2026-09-25T18:00:00Z"))),
        Some(t0)
    );
    // One band resolves as `resolve_parameter_time` does.
    for time in [None, Some(t0), Some(at("2026-09-25T19:15:00Z"))] {
        assert_eq!(
            resolve(&[IR], time),
            engine.resolve_parameter_time(Some(IR), time, None)
        );
    }
    // An unknown parameter echoes the time: its render fails anyway.
    assert_eq!(resolve(&[IR, "nope"], Some(t1)), Some(t1));

    let extent = engine.raster_info().spatial_extent.unwrap();
    // No time renders the shared 19:00 scan, not IR's own latest 19:10.
    let latest = bands(&engine, extent, 32, None, &[IR, CLOUD]).unwrap();
    let pinned = bands(&engine, extent, 32, Some(t0), &[IR, CLOUD]).unwrap();
    for (a, b) in latest.iter().zip(&pinned) {
        assert_eq!(values(a), values(b));
    }
    // 19:10 is not a cloud top scan: the composite fails rather than mix
    // IR 19:10 with cloud top 19:00.
    match bands(&engine, extent, 32, Some(t1), &[IR, CLOUD]) {
        Err(DataServerError::InvalidParameter(message)) => {
            assert!(message.contains(CLOUD), "{message}");
            assert!(message.contains("2026-09-25T19:10:00Z"), "{message}");
        }
        other => panic!(
            "expected a missing-scan error, got {:?}",
            other.map(|t| t.len())
        ),
    }
    // The time is not snapped: it names the scan.
    assert!(bands(&engine, extent, 32, Some(at("2026-09-25T19:05:00Z")), &[IR]).is_err());
    assert_eq!(
        bands(&engine, extent, 32, Some(t1), &[IR]).unwrap().len(),
        1
    );
    assert!(matches!(
        bands(&engine, extent, 32, Some(t0), &[IR, "nope"]),
        Err(DataServerError::InvalidParameter(_))
    ));
    assert!(bands(&engine, extent, 32, Some(t0), &[])
        .unwrap()
        .is_empty());
}

/// Bands that share no scan resolve to no time and render nothing.
#[test]
fn bands_without_a_shared_scan_render_empty() {
    let (engine, _dir) = engine(
        "goes19-bands-disjoint",
        &[(C13, C13), (C13, C13_LATER), (ACHT, ACHT_LATER)],
    );
    assert_eq!(
        &*engine.parameter_times(CLOUD).unwrap(),
        [at("2026-09-25T19:20:00Z")]
    );
    assert_eq!(
        engine.resolve_parameters_time(&[IR, CLOUD], None, None),
        None
    );
    assert_eq!(
        engine.resolve_parameters_time(&[IR, CLOUD], Some(at("2026-09-25T19:20:00Z")), None),
        None
    );
    let extent = engine.raster_info().spatial_extent.unwrap();
    let tiles = bands(&engine, extent, 16, None, &[IR, CLOUD]).unwrap();
    assert_eq!(tiles.len(), 2);
    assert!(tiles.iter().all(RasterTile::is_empty));
}
