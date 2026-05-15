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
use ds_core::edr_engine::EdrEngine;
use ds_core::map_engine::{MapEngine, OutputCrs};
use ds_core::model::{AreaQueryResult, DomainDescription};

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
        time_window: None,
    };

    let engine = engine_odim::OdimEngine::new(
        "dmi-test",
        Some(dir.path().to_str().expect("utf8 fixture path")),
        &config,
    )
    .expect("OdimEngine::new succeeds on the DMI fixture");

    (engine, dir)
}

/// `MapEngine::raster_info()` advertises the native CRS as
/// stereographic and the WGS84 envelope as Denmark-and-surrounds —
/// a sanity check that the projection-string parser, projection
/// math, and metadata bookkeeping are connected.
///
/// The envelope is the edge-sampled lon/lat extent of the
/// stereographic grid (`reader::wgs84_envelope`), not the raw
/// LL→UR corner diagonal — a DMI 500 m composite covers Denmark
/// plus the surrounding seas and neighbouring coastlines, and the
/// grid edges bow slightly past the corner latitudes/longitudes.
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
    assert_dmi_envelope(bbox);
    assert_eq!(info.times.len(), 1, "fixture has a single timestep");
}

/// Sanity-check a DMI-fixture WGS84 envelope `[w, s, e, n]`. The
/// fixture's grid is fixed, so the edge-sampled envelope is
/// deterministic (~`[3.0, 52.15, 20.74, 60.21]`); these bands are
/// wide enough not to be brittle to projection-math tweaks but
/// tight enough to catch a gross regression (e.g. the old
/// LL→UR-corner shortcut, or a degenerate `[MAX,…,MIN]`).
fn assert_dmi_envelope(bbox: [f64; 4]) {
    let [w, s, e, n] = bbox;
    assert!(w < e && s < n, "envelope must be well-formed, got {bbox:?}");
    assert!(
        (0.0..6.0).contains(&w) && (16.0..24.0).contains(&e),
        "longitude envelope should cover Denmark-and-surrounds, got {bbox:?}"
    );
    assert!(
        (48.0..56.0).contains(&s) && (57.0..63.0).contains(&n),
        "latitude envelope should cover Denmark-and-surrounds, got {bbox:?}"
    );
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

// ---------------------------------------------------------------------------
// EdrEngine (Phase 1.5)
// ---------------------------------------------------------------------------

/// EDR collection-metadata accessors report sane values for the DMI
/// fixture: one parameter, position+area query types, a single-step
/// temporal extent, and a Denmark-ish spatial extent.
#[test]
fn edr_metadata_is_consistent() {
    let (engine, _guard) = dmi_engine();

    assert_eq!(engine.get_parameters(), vec!["reflectivity".to_string()]);

    let mut qt = engine.supported_query_types();
    qt.sort();
    assert_eq!(qt, vec!["area".to_string(), "position".to_string()]);

    let (start, end) = engine
        .get_temporal_extent()
        .expect("fixture has a temporal extent");
    assert_eq!(start, end, "single-file fixture → degenerate interval");

    let times = engine
        .get_available_times()
        .expect("fixture advertises discrete times");
    assert_eq!(times.len(), 1);

    let bbox = engine
        .get_spatial_extent()
        .expect("fixture has a spatial extent");
    assert_dmi_envelope(bbox);

    // ODIM has no station list — locations is empty, query_location
    // is unsupported.
    assert!(engine.get_locations().unwrap().is_empty());
    assert!(engine.query_location("anything", None, None).is_err());
}

/// `query_position` over central Denmark returns a `PointSeries`
/// coverage: x/y pinned to the requested point, one timestep, an
/// `NdArray` shaped `[1]` over the `t` axis. Pixel content isn't
/// asserted — the checked-in fixture is a clear-air snapshot.
#[test]
fn edr_query_position_returns_point_series() {
    let (engine, _guard) = dmi_engine();
    let result = engine
        .query_position("POINT(10.5 56.0)", None, None)
        .expect("position query succeeds");

    match result.domain {
        DomainDescription::PointSeries { x, y, ref t } => {
            assert_eq!((x, y), (10.5, 56.0));
            assert_eq!(t.len(), 1);
        }
        other => panic!("expected PointSeries, got {other:?}"),
    }
    let range = result
        .ranges
        .get("reflectivity")
        .expect("range keyed by parameter name");
    assert_eq!(range.shape, vec![1]);
    assert_eq!(range.axis_names, vec!["t".to_string()]);
    assert_eq!(range.values.len(), 1);
}

/// A position query for a point far outside the composite still
/// returns a well-formed coverage — the single value is simply
/// `None` (off-grid), not an error.
#[test]
fn edr_query_position_off_grid_is_none() {
    let (engine, _guard) = dmi_engine();
    let result = engine
        .query_position("POINT(-140.0 12.0)", None, None)
        .expect("off-grid position query still succeeds");
    let range = &result.ranges["reflectivity"];
    assert_eq!(range.values, vec![None]);
}

/// An unknown `parameters` filter is rejected.
#[test]
fn edr_query_position_rejects_unknown_parameter() {
    let (engine, _guard) = dmi_engine();
    let err = engine
        .query_position("POINT(10.5 56.0)", None, Some(&["temperature".to_string()]))
        .unwrap_err();
    assert!(format!("{err}").contains("Unknown parameter"));
}

/// `query_area` over a Denmark bbox returns a `Single` `Grid`
/// coverage whose `NdArray` length equals the product of its shape
/// dimensions (the CoverageJSON NdArray invariant).
#[test]
fn edr_query_area_returns_grid() {
    let (engine, _guard) = dmi_engine();
    let result = engine
        .query_area("9.0,55.0,12.0,57.5", None, None)
        .expect("area query succeeds");

    let coverage = match result {
        AreaQueryResult::Single(qr) => qr,
        AreaQueryResult::Collection(_) => panic!("ODIM area query must return Single"),
    };

    let (nx, ny) = match coverage.domain {
        DomainDescription::Grid {
            ref x,
            ref y,
            ref t,
        } => {
            assert!(t.is_none(), "single-file fixture → no time axis");
            (x.len(), y.len())
        }
        other => panic!("expected Grid, got {other:?}"),
    };
    assert!(nx > 0 && ny > 0);

    let range = &coverage.ranges["reflectivity"];
    assert_eq!(range.shape, vec![ny, nx]);
    assert_eq!(range.axis_names, vec!["y".to_string(), "x".to_string()]);
    assert_eq!(
        range.values.len(),
        ny * nx,
        "NdArray length must equal the product of shape dims"
    );
}

/// An unbounded area query (`datetime = None`) over a catalog with
/// more than `MAX_AREA_TIMESTEPS` (64) entries is rejected with a
/// `400`-class error rather than allocating a hundreds-of-MB
/// coverage cube. Builds a 65-file catalog by copying the DMI
/// fixture to distinct 5-minute-spaced timestamps.
#[test]
fn edr_query_area_rejects_too_many_timesteps() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/odim-dmi-fixture.h5")
        .canonicalize()
        .expect("fixture path canonicalises");
    // 65 files at 5-min spacing from 2026-05-14 00:00 — one past the
    // 64-timestep cap.
    for i in 0..65 {
        let total_min = i * 5;
        let hh = total_min / 60;
        let mm = total_min % 60;
        let name = format!("dk.com.20260514{hh:02}{mm:02}.500_max.h5");
        std::fs::copy(&src, dir.path().join(name)).expect("copy fixture");
    }

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
        time_window: None,
    };
    let engine = engine_odim::OdimEngine::new(
        "dmi-cap-test",
        Some(dir.path().to_str().expect("utf8 fixture path")),
        &config,
    )
    .expect("engine builds over the 65-file catalog");

    // No datetime filter → all 65 entries → over the cap → error.
    let err = engine
        .query_area("9.0,55.0,12.0,57.5", None, None)
        .unwrap_err();
    assert!(
        format!("{err}").contains("maximum is 64"),
        "expected a timestep-cap error, got: {err}"
    );

    // A narrow datetime range keeps it under the cap → succeeds.
    use chrono::{TimeZone, Utc};
    let start = Utc.with_ymd_and_hms(2026, 5, 14, 0, 0, 0).unwrap();
    let end = Utc.with_ymd_and_hms(2026, 5, 14, 0, 30, 0).unwrap();
    assert!(
        engine
            .query_area("9.0,55.0,12.0,57.5", Some((start, end)), None)
            .is_ok(),
        "a 7-timestep window must stay under the cap"
    );
}

/// A `POLYGON(...)` area query masks cells outside the ring to
/// `None`. A degenerate thin triangle leaves most of its bbox grid
/// masked, so the masked-cell count is strictly positive.
#[test]
fn edr_query_area_masks_outside_polygon() {
    let (engine, _guard) = dmi_engine();
    let result = engine
        .query_area(
            "POLYGON((9.0 55.0, 12.0 55.0, 9.0 57.0, 9.0 55.0))",
            None,
            None,
        )
        .expect("polygon area query succeeds");
    let coverage = match result {
        AreaQueryResult::Single(qr) => qr,
        AreaQueryResult::Collection(_) => panic!("expected Single"),
    };
    let range = &coverage.ranges["reflectivity"];
    let masked = range.values.iter().filter(|v| v.is_none()).count();
    assert!(
        masked > 0,
        "a thin triangle must leave some bbox cells outside the polygon"
    );
}
