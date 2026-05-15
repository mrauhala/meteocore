//! End-to-end test that wires `OdimEngine` into the `MapEngine`
//! trait surface and exercises the full render pipeline against a
//! real DMI composite (`testdata/odim-dmi-fixture.h5`).
//!
//! The catalog-scan + reader + projection + render machinery has
//! good unit-test coverage in `src/`, but unit tests stub out file
//! I/O and never compose the layers. This file closes that gap:
//! `OdimEngine::new` runs against a real on-disk fixture, and a
//! `MapEngine::get_raster_tile` call goes through projection
//! reprojection + nearest-neighbor sampling for a bbox over Denmark.

use std::path::Path;

use ds_core::config::OdimConfig;
use ds_core::map_engine::{MapEngine, OutputCrs};

/// Construct an `OdimEngine` over `testdata/odim-dmi-fixture.h5`
/// (a real DMI v2.0 ODIM_H5 composite shipped in this repo). Returns
/// the engine plus the tempdir whose lifetime must outlive it.
fn dmi_engine() -> (engine_odim::OdimEngine, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/odim-dmi-fixture.h5")
        .canonicalize()
        .expect("fixture path canonicalises");
    let dst = dir.path().join("dk.com.202601201125.500_max.h5");
    std::fs::copy(&src, &dst).expect("copy fixture");

    let config = OdimConfig {
        filename_template: Some("dk.com.%Y%m%d%H%M.500_max.h5".into()),
        filename_pattern: None,
        timestamp_format: None,
        parameter: "reflectivity".into(),
        unit: "dBZ".into(),
        nodata: None,
        gain: None,
        offset: None,
        poll_interval_secs: 30,
        max_files: None,
        endpoint: None,
        bucket: None,
        prefix_pattern: None,
    };

    let engine = engine_odim::OdimEngine::new(dir.path(), "dmi-test", &config)
        .expect("OdimEngine::new succeeds on the DMI fixture");

    (engine, dir)
}

/// `MapEngine::raster_info()` advertises the native CRS as
/// stereographic and the WGS84 corner envelope as Denmark-ish — a
/// sanity check that the projection-string parser, projection math,
/// and metadata bookkeeping are connected.
#[test]
fn raster_info_reports_dmi_extents() {
    let (engine, _guard) = dmi_engine();
    let info = engine.raster_info();

    assert!(
        info.native_crs.to_lowercase().contains("stere"),
        "native_crs should report stereographic, got `{}`",
        info.native_crs
    );
    let bbox = info
        .spatial_extent
        .expect("DMI fixture has a spatial extent");
    assert!(
        bbox[0] > 0.0 && bbox[2] < 25.0,
        "longitude bbox should cover Denmark-ish range, got {bbox:?}"
    );
    assert!(
        bbox[1] > 50.0 && bbox[3] < 60.0,
        "latitude bbox should cover Denmark-ish range, got {bbox:?}"
    );
    assert_eq!(info.times.len(), 1, "fixture has a single timestep");
}

/// `MapEngine::get_raster_tile()` produces a 64×64 tile over
/// central Denmark without erroring — i.e. the projection-string
/// parser, stereographic reprojection, and nearest-neighbor sample
/// chain compose correctly. The tile structure is validated; pixel
/// content is not, because the checked-in fixture is a clear-air
/// snapshot (DMI's January archive happens to be mostly undetect
/// returns) and `undetect` is mapped to `None` by design.
#[test]
fn get_raster_tile_over_denmark_renders() {
    let (engine, _guard) = dmi_engine();
    let tile = engine
        .get_raster_tile(
            [9.5, 55.0, 12.5, 57.0], // [W, S, E, N] over Jylland / Sjælland
            64,
            64,
            None,
            &OutputCrs::Wgs84,
            None,
        )
        .expect("render succeeds");

    assert_eq!(tile.width, 64);
    assert_eq!(tile.height, 64);
    assert_eq!(tile.values.len(), 64 * 64);
}

/// `MapEngine::get_raster_tile()` on an `OutputCrs::WebMercator`
/// request returns a well-formed tile. This exercises the Mercator-Y
/// branch in `engine.rs`, which converts pixel-row indices to meters
/// and reprojects through `merc_y_to_lat`. Same fixture-content
/// caveat as the WGS84 case.
#[test]
fn get_raster_tile_in_web_mercator_renders() {
    let (engine, _guard) = dmi_engine();
    let tile = engine
        .get_raster_tile(
            [9.5, 55.0, 12.5, 57.0],
            64,
            64,
            None,
            &OutputCrs::WebMercator,
            None,
        )
        .expect("Mercator render succeeds");

    assert_eq!(tile.width, 64);
    assert_eq!(tile.height, 64);
    assert_eq!(tile.values.len(), 64 * 64);
}

/// A wide bbox spanning the full DMI composite extent at low
/// resolution is enough to land at least one nearest-neighbor sample
/// on a source pixel for any non-empty composite — even a clear-air
/// snapshot has thousands of `undetect` pixels distinct from
/// `nodata`. The contrast here is **render succeeded vs panicked or
/// errored**, not pixel content. We assert the tile is a valid
/// 32×32 grid and that the rows iterate through the projection
/// fall-through path without panicking.
#[test]
fn get_raster_tile_full_extent_does_not_panic() {
    let (engine, _guard) = dmi_engine();
    let info = engine.raster_info();
    let bbox = info.spatial_extent.expect("fixture has spatial extent");

    let tile = engine
        .get_raster_tile(bbox, 32, 32, None, &OutputCrs::Wgs84, None)
        .expect("full-extent render succeeds");

    assert_eq!(tile.values.len(), 32 * 32);
}
