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
use ds_core::feature::{Bbox, FeatureQuery, Geometry, PropertyValue};
use ds_core::feature_engine::FeatureEngine;
use ds_core::map_engine::{MapEngine, OutputCrs};
use ds_core::model::{CoverageResponse, DomainDescription};

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
        parameter: Some("reflectivity".into()),
        unit: Some("dBZ".into()),
        nodata: None,
        gain: None,
        offset: None,
        poll_interval_secs: 30,
        max_files: None,
        endpoint: None,
        bucket: None,
        prefix_pattern: None,
        time_window: None,
        discovery: None,
        cadence_secs: None,
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
            None,
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
            None,
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
        .get_raster_tile(bbox, 32, 32, None, &OutputCrs::Wgs84, None, None, None)
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
    assert!(engine
        .query_location("anything", None, None, None, None)
        .is_err());
}

/// `query_position` over central Denmark returns a `PointSeries`
/// coverage: x/y pinned to the requested point, one timestep, an
/// `NdArray` shaped `[1]` over the `t` axis. Pixel content isn't
/// asserted — the checked-in fixture is a clear-air snapshot.
#[test]
fn edr_query_position_returns_point_series() {
    let (engine, _guard) = dmi_engine();
    let response = engine
        .query_position("POINT(10.5 56.0)", None, None, None, None)
        .expect("position query succeeds");
    let result = match response {
        CoverageResponse::Single(qr) => qr,
        CoverageResponse::Collection(_) => panic!("a no-vertical COMP query must return Single"),
    };

    match result.domain {
        DomainDescription::PointSeries { x, y, ref t, .. } => {
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
    let response = engine
        .query_position("POINT(-140.0 12.0)", None, None, None, None)
        .expect("off-grid position query still succeeds");
    let result = match response {
        CoverageResponse::Single(qr) => qr,
        CoverageResponse::Collection(_) => panic!("expected Single"),
    };
    let range = &result.ranges["reflectivity"];
    assert_eq!(range.values, vec![None]);
}

/// An unknown `parameters` filter is rejected.
#[test]
fn edr_query_position_rejects_unknown_parameter() {
    let (engine, _guard) = dmi_engine();
    let err = engine
        .query_position(
            "POINT(10.5 56.0)",
            None,
            Some(&["temperature".to_string()]),
            None,
            None,
        )
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
        .query_area("9.0,55.0,12.0,57.5", None, None, None, None)
        .expect("area query succeeds");

    let coverage = match result {
        CoverageResponse::Single(qr) => qr,
        CoverageResponse::Collection(_) => panic!("ODIM area query must return Single"),
    };

    let (nx, ny) = match coverage.domain {
        DomainDescription::Grid {
            ref x,
            ref y,
            ref t,
            ..
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
        parameter: Some("reflectivity".into()),
        unit: Some("dBZ".into()),
        nodata: None,
        gain: None,
        offset: None,
        poll_interval_secs: 30,
        max_files: None,
        endpoint: None,
        bucket: None,
        prefix_pattern: None,
        time_window: None,
        discovery: None,
        cadence_secs: None,
    };
    let engine = engine_odim::OdimEngine::new(
        "dmi-cap-test",
        Some(dir.path().to_str().expect("utf8 fixture path")),
        &config,
    )
    .expect("engine builds over the 65-file catalog");

    // No datetime filter → all 65 entries → over the cap → error.
    let err = engine
        .query_area("9.0,55.0,12.0,57.5", None, None, None, None)
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
            .query_area("9.0,55.0,12.0,57.5", Some((start, end)), None, None, None)
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
            None,
            None,
        )
        .expect("polygon area query succeeds");
    let coverage = match result {
        CoverageResponse::Single(qr) => qr,
        CoverageResponse::Collection(_) => panic!("expected Single"),
    };
    let range = &coverage.ranges["reflectivity"];
    let masked = range.values.iter().filter(|v| v.is_none()).count();
    assert!(
        masked > 0,
        "a thin triangle must leave some bbox cells outside the polygon"
    );
}

// ---------------------------------------------------------------------------
// PolarVolumeEngine — ODIM polar-volume (PVOL) MapEngine
// ---------------------------------------------------------------------------

/// End-to-end render against the real FMI Vihti polar volume
/// (`testdata/radar-fmi-pvol/202605191050_fivih_PVOL.h5`).
///
/// The 15 MB fixture is **not committed to git**, so this test skips
/// gracefully when it is absent — CI stays green; a local checkout
/// with the fixture exercises the full PVOL render pipeline.
#[test]
fn pvol_engine_renders_fmi_vihti_volume() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/radar-fmi-pvol/202605191050_fivih_PVOL.h5");
    if !fixture.exists() {
        eprintln!("skipping pvol_engine_renders_fmi_vihti_volume: fixture absent at {fixture:?}");
        return;
    }

    // The PVOL engine scans a directory; point it at the directory the
    // fixture lives in.
    let data_dir = fixture
        .parent()
        .expect("fixture has a parent directory")
        .to_str()
        .expect("utf8 fixture dir");

    // `odim-volume` ignores `parameter`/`unit`; both may be `None`.
    let config = OdimConfig {
        filename_template: None,
        filename_pattern: None,
        timestamp_format: None,
        parameter: None,
        unit: None,
        nodata: None,
        gain: None,
        offset: None,
        poll_interval_secs: 30,
        max_files: None,
        endpoint: None,
        bucket: None,
        prefix_pattern: None,
        time_window: None,
        discovery: None,
        cadence_secs: None,
    };

    let engine = engine_odim::PolarVolumeEngine::new("fivih-pvol-test", Some(data_dir), &config)
        .expect("PolarVolumeEngine::new over the PVOL directory");

    // The source expands into per-site collections. Take the Vihti
    // site view and render through it.
    let view = engine.site_view("fivih", "fivih-pvol-test-fivih");

    let info = view.raster_info();
    assert_eq!(info.native_crs, "CRS:84");
    assert!(!info.times.is_empty(), "PVOL site must have a timestep");
    assert!(
        info.spatial_extent.is_some(),
        "PVOL site must report a coverage bbox"
    );

    // The FMI Vihti lowest sweep exposes TH (and DBZH); the
    // parameters are **bare quantities** — no `fivih:` prefix.
    let params: Vec<&str> = info.parameters.iter().map(|(n, _)| n.as_str()).collect();
    assert!(
        !params.is_empty(),
        "expected bare-quantity parameters, got {:?}",
        info.parameters
    );
    assert!(
        params.iter().all(|n| !n.contains(':')),
        "per-site parameters must be bare quantities, got {params:?}"
    );
    // Prefer TH if present (every FMI lowest sweep carries it), else any.
    let render_param = if params.contains(&"TH") {
        "TH"
    } else if params.contains(&"DBZH") {
        "DBZH"
    } else {
        params[0]
    }
    .to_string();

    // Render a tile over the radar's coverage bbox. Vihti sits near
    // 24.50°E, 60.56°N; a ~2° box centred there is well inside the
    // ~250 km sweep, so a real volume must produce some echoes.
    let bbox = [23.4, 59.6, 25.6, 61.5];
    let tile = view
        .get_raster_tile(
            bbox,
            128,
            128,
            None,
            &OutputCrs::Wgs84,
            Some(&render_param),
            None,
            None,
        )
        .expect("PVOL site get_raster_tile over the coverage bbox");

    assert_eq!(tile.width, 128);
    assert_eq!(tile.height, 128);
    assert_eq!(tile.values.len(), 128 * 128);
    let non_none = tile.values.iter().filter(|v| v.is_some()).count();
    assert!(
        non_none > 0,
        "a render over the radar's own coverage bbox must sample some \
         non-None values (parameter {render_param})"
    );
}

/// `VolumeEngine` surface on the real FMI Vihti volume: sample the polar
/// volume into a 3-D point cloud and encode it as a 3D Tiles `.pnts` tile +
/// `tileset.json` via `ds-3dtiles`. Skips when the uncommitted fixture is
/// absent (CI stays green).
#[test]
fn pvol_volume_engine_emits_point_cloud() {
    use ds_core::volume::VolumeEngine;

    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/radar-fmi-pvol/202605191050_fivih_PVOL.h5");
    if !fixture.exists() {
        eprintln!("skipping pvol_volume_engine_emits_point_cloud: fixture absent at {fixture:?}");
        return;
    }
    let data_dir = fixture
        .parent()
        .expect("fixture has a parent directory")
        .to_str()
        .expect("utf8 fixture dir");

    let config = OdimConfig {
        filename_template: None,
        filename_pattern: None,
        timestamp_format: None,
        parameter: None,
        unit: None,
        nodata: None,
        gain: None,
        offset: None,
        poll_interval_secs: 30,
        max_files: None,
        endpoint: None,
        bucket: None,
        prefix_pattern: None,
        time_window: None,
        discovery: None,
        cadence_secs: None,
    };
    let engine = engine_odim::PolarVolumeEngine::new("fivih-vol-test", Some(data_dir), &config)
        .expect("PolarVolumeEngine::new over the PVOL directory");
    let view = engine.site_view("fivih", "fivih-vol-test-fivih");

    // Metadata surface.
    let vinfo = view.volume_info();
    assert!(
        !vinfo.quantities.is_empty(),
        "PVOL site must advertise quantities"
    );
    assert!(!vinfo.times.is_empty(), "PVOL site must have a valid time");
    assert!(!vinfo.default_quantity.is_empty());

    // Sample the full volume into a point cloud (default quantity, latest
    // time, a 5 dBZ floor).
    let cloud = view
        .read_point_cloud(None, None, Some(5.0), None)
        .expect("read_point_cloud over the real volume");
    assert!(
        !cloud.points.is_empty(),
        "a real volume must yield echo points"
    );

    // RTC center is the antenna's ECEF position (geocentric radius ~6.36e6 m
    // at 60.6°N); the per-point offsets are small and finite.
    let rtc_mag =
        (cloud.rtc_center[0].powi(2) + cloud.rtc_center[1].powi(2) + cloud.rtc_center[2].powi(2))
            .sqrt();
    assert!(
        (6.0e6..6.6e6).contains(&rtc_mag),
        "RTC center near Earth radius, got {rtc_mag}"
    );
    for p in &cloud.points {
        assert!(p.offset.iter().all(|c| c.is_finite()), "finite offsets");
        let d = (p.offset[0].powi(2) + p.offset[1].powi(2) + p.offset[2].powi(2)).sqrt();
        assert!(d < 350_000.0, "point within sweep range, got {d} m");
        assert!(p.value >= 5.0, "min_value floor honoured, got {}", p.value);
    }

    // Region is a sane geodetic box (radians/metres) around Vihti.
    let [w, s, e, n, min_h, max_h] = cloud.region;
    assert!(w < e && s < n, "region ordered: {:?}", cloud.region);
    assert!(
        (0.40..0.60).contains(&e),
        "east edge near ~25-29°E, got {e}"
    );
    assert!(
        (1.00..1.15).contains(&n),
        "north edge near ~58-63°N, got {n}"
    );
    assert!(
        min_h > 0.0 && min_h < max_h && max_h < 25_000.0,
        "heights sane: {min_h}..{max_h}"
    );

    // Encode through ds-3dtiles: a valid .pnts tile + a tileset.json.
    let cmap =
        ds_render::LutColorMap::from_builtin(ds_render::BuiltinColormap::RadarDbz, -32.0, 95.0);
    let pnts = ds_3dtiles::encode_pnts(&cloud, &cmap).expect("encode pnts");
    assert_eq!(&pnts[0..4], b"pnts", "pnts magic");
    let byte_len = u32::from_le_bytes(pnts[8..12].try_into().unwrap()) as usize;
    assert_eq!(
        byte_len,
        pnts.len(),
        "pnts byteLength matches actual length"
    );

    let tileset = ds_3dtiles::tileset_json(&cloud, "content.pnts").expect("tileset json");
    assert!(
        tileset.contains("\"content.pnts\""),
        "tileset names content"
    );
    assert!(
        tileset.contains("\"region\""),
        "tileset has a region bounding volume"
    );

    // An unknown quantity is a clear InvalidParameter (→ 400), not a
    // "no echoes" LocationNotFound (→ 404).
    assert!(
        matches!(
            view.read_point_cloud(Some("NOSUCHQ"), None, None, None),
            Err(ds_core::error::DataServerError::InvalidParameter(_))
        ),
        "unknown quantity must be InvalidParameter"
    );
}

/// `VolumeEngine::read_voxel_grid` on the real FMI Vihti volume: resample the
/// polar volume into a regular cylindrical voxel grid. Skips when the
/// uncommitted fixture is absent.
#[test]
fn pvol_volume_engine_voxel_grid() {
    use ds_core::volume::VolumeEngine;

    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/radar-fmi-pvol/202605191050_fivih_PVOL.h5");
    if !fixture.exists() {
        eprintln!("skipping pvol_volume_engine_voxel_grid: fixture absent at {fixture:?}");
        return;
    }
    let data_dir = fixture.parent().unwrap().to_str().unwrap();
    let config = OdimConfig {
        filename_template: None,
        filename_pattern: None,
        timestamp_format: None,
        parameter: None,
        unit: None,
        nodata: None,
        gain: None,
        offset: None,
        poll_interval_secs: 30,
        max_files: None,
        endpoint: None,
        bucket: None,
        prefix_pattern: None,
        time_window: None,
        discovery: None,
        cadence_secs: None,
    };
    let engine = engine_odim::PolarVolumeEngine::new("fivih-voxel-test", Some(data_dir), &config)
        .expect("PolarVolumeEngine::new");
    let view = engine.site_view("fivih", "fivih-voxel-test-fivih");

    // A modest grid keeps the test quick.
    let dims = [48, 180, 24];
    let grid = view
        .read_voxel_grid(Some("DBZH"), None, Some(dims), None)
        .expect("read_voxel_grid over the real volume");

    assert_eq!(grid.dims, dims);
    assert_eq!(
        grid.values.len(),
        dims[0] * dims[1] * dims[2],
        "values tile the grid"
    );
    assert_eq!(grid.quantity, "DBZH");
    assert_eq!(grid.angle_range, [0.0, std::f64::consts::TAU]);
    assert!(
        grid.radius_range[1] > 100_000.0,
        "coverage radius is ~250 km"
    );
    assert!(grid.height_range[1] > 0.0 && grid.height_range[0] == 0.0);

    // Real echoes are present — cells *above* the clear-air floor (#360 fills
    // clear air with exactly `NO_ECHO_FLOOR_DBZ`, so `valid_count()` alone, which
    // now also counts those floor cells, would no longer prove "has echo"). Uses
    // `>` (not `>=`): an echo measured at exactly the floor is indistinguishable
    // from clear air at the grid level — fine here, precip is well above −32 dBZ.
    let echoes = grid
        .values
        .iter()
        .filter(|v| **v > ds_core::volume::NO_ECHO_FLOOR_DBZ)
        .count();
    assert!(
        echoes > 0,
        "a real volume must yield echo voxels above the floor"
    );
    // …but not every cell is finite — the cone of silence + below the lowest
    // sweep + out-of-range stay NaN (no fabricated data).
    assert!(
        grid.valid_count() < grid.values.len(),
        "expect NaN gaps (cone of silence etc.)"
    );
    // Every sampled value is a finite dBZ in a sane band.
    for v in grid.values.iter().filter(|v| v.is_finite()) {
        assert!((-40.0..100.0).contains(v), "sane dBZ, got {v}");
    }
}

/// End-to-end `FeatureEngine` surface on the real FMI Vihti volume: the
/// owning `PolarVolumeEngine` exposes its sites as a Features collection (one
/// Point Feature per site). Skips when the uncommitted 15 MB fixture is absent.
#[test]
fn pvol_engine_exposes_sites_as_features() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/radar-fmi-pvol/202605191050_fivih_PVOL.h5");
    if !fixture.exists() {
        eprintln!("skipping pvol_engine_exposes_sites_as_features: fixture absent at {fixture:?}");
        return;
    }
    let data_dir = fixture
        .parent()
        .expect("fixture has a parent directory")
        .to_str()
        .expect("utf8 fixture dir");

    let config = OdimConfig {
        filename_template: None,
        filename_pattern: None,
        timestamp_format: None,
        parameter: None,
        unit: None,
        nodata: None,
        gain: None,
        offset: None,
        poll_interval_secs: 30,
        max_files: None,
        endpoint: None,
        bucket: None,
        prefix_pattern: None,
        time_window: None,
        discovery: None,
        cadence_secs: None,
    };

    let engine =
        engine_odim::PolarVolumeEngine::new("radar-fi-volume-local-h5", Some(data_dir), &config)
            .expect("PolarVolumeEngine::new over the PVOL directory");

    // Inventory page: one Point Feature per site; fivih (Vihti) must be present.
    let page = FeatureEngine::get_features(&engine, &FeatureQuery::default()).unwrap();
    assert!(page.number_matched >= 1, "at least the Vihti site");
    assert_eq!(page.number_returned, page.features.len());
    assert_eq!(
        FeatureEngine::feature_count(&engine),
        page.number_matched,
        "feature_count agrees with the unpaged match count"
    );
    assert!(
        FeatureEngine::spatial_extent(&engine).is_some(),
        "inventory has a spatial extent"
    );

    let fivih = FeatureEngine::get_feature(&engine, "fivih").expect("fivih site feature");
    assert_eq!(fivih.id, "fivih");
    match fivih.geometry.as_ref() {
        Geometry::Point { x, y } => {
            assert!(
                (23.0..26.0).contains(x) && (59.0..62.0).contains(y),
                "Vihti antenna sits near 24.5E/60.5N, got {x},{y}"
            );
        }
        other => panic!("expected Point geometry, got {other:?}"),
    }
    // `quantities` is a non-empty List of *bare* quantity codes (no `nod:`).
    match fivih.properties.get("quantities") {
        Some(PropertyValue::List(items)) => {
            assert!(!items.is_empty(), "site advertises measured quantities");
            assert!(
                items
                    .iter()
                    .all(|q| matches!(q, PropertyValue::String(s) if !s.contains(':'))),
                "quantities must be bare codes, got {items:?}"
            );
        }
        other => panic!("quantities must be a List, got {other:?}"),
    }
    // `collection` points at the per-site EDR/WMS collection id.
    assert_eq!(
        fivih.properties.get("collection"),
        Some(&PropertyValue::String(
            "radar-fi-volume-local-h5-fivih".into()
        ))
    );

    // Unknown site → FeatureNotFound (→ 404 at the API layer).
    assert!(matches!(
        FeatureEngine::get_feature(&engine, "nope"),
        Err(ds_core::error::DataServerError::FeatureNotFound(_))
    ));

    // A bbox over Vihti's antenna keeps fivih in the result.
    let bbox = Bbox::new(23.4, 59.6, 25.6, 61.5).unwrap();
    let in_box = FeatureEngine::get_features(
        &engine,
        &FeatureQuery {
            bbox: Some(bbox),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        in_box.features.iter().any(|f| f.id == "fivih"),
        "fivih antenna falls inside the query bbox"
    );
}

/// Exercises the PVOL engine's **remote (S3/HTTP) scan** end-to-end
/// without a network: a `ds_storage::DataStore` backed by
/// `object_store`'s `LocalFileSystem` behaves like S3 for `list` /
/// `get`, so building one over `testdata/radar-fmi-pvol/` drives the
/// real remote code path — the same trick PR #182 used for the COMP
/// engine.
///
/// Asserts the Vihti (`fivih`) volume is discovered with its
/// 13 elevation sweeps. The 15 MB fixture is **not committed to git**,
/// so the test skips gracefully when it is absent.
#[test]
fn pvol_engine_remote_scan_discovers_fmi_volume() {
    let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/radar-fmi-pvol");
    let fixture = fixture_dir.join("202605191050_fivih_PVOL.h5");
    if !fixture.exists() {
        eprintln!("skipping pvol_engine_remote_scan_discovers_fmi_volume: fixture absent");
        return;
    }

    // A `DataStore` over the fixture directory — `LocalFileSystem`
    // exercises the same `list`/`get` surface the S3 backend does.
    let (store, _base) = ds_storage::build_store(
        fixture_dir
            .canonicalize()
            .expect("fixture dir canonicalises")
            .to_str()
            .expect("utf8 fixture dir"),
    )
    .expect("build a DataStore over the fixture directory");

    // The FMI Vihti polar volume is a multi-sweep elevation stack
    // (9 distinct angles, 0.3°–9°, with low-elevation split cuts) —
    // assert that directly from the parsed fixture so the remote-scan
    // path below is verified against a known volume shape.
    let bytes = std::fs::read(&fixture).expect("read PVOL fixture bytes");
    let volume = engine_odim::pvol::read_polar_volume(&bytes).expect("parse PVOL fixture");
    assert_eq!(volume.site.nod.as_deref(), Some("fivih"));
    assert!(
        volume.sweeps.len() >= 9,
        "FMI Vihti volume has ≥9 elevation sweeps, got {}",
        volume.sweeps.len()
    );

    // Empty prefix scans the store root, where the fixture lives.
    let engine =
        engine_odim::PolarVolumeEngine::new_remote_for_test("fivih-remote-test", store, "")
            .expect("PolarVolumeEngine remote scan over the fixture directory");

    // The `fivih` site must surface — view it as a per-site collection.
    assert!(
        engine.sites().iter().any(|(n, _)| n == "fivih"),
        "remote scan must discover the `fivih` site, got {:?}",
        engine.sites()
    );
    let view = engine.site_view("fivih", "fivih-remote-test-fivih");

    let info = view.raster_info();
    assert_eq!(info.native_crs, "CRS:84");
    assert!(
        !info.times.is_empty(),
        "remote scan must discover the volume's timestep"
    );
    assert!(
        info.spatial_extent.is_some(),
        "remote scan must report a coverage bbox"
    );

    // Parameters are bare quantities (no `fivih:` prefix).
    let params: Vec<&str> = info.parameters.iter().map(|(n, _)| n.as_str()).collect();
    assert!(
        !params.is_empty() && params.iter().all(|n| !n.contains(':')),
        "remote scan must discover bare-quantity parameters, got {:?}",
        info.parameters
    );

    // A render over the radar's coverage bbox proves the
    // streamed-then-parsed volume is intact end-to-end.
    let render_param = params
        .iter()
        .find(|n| **n == "TH")
        .or_else(|| params.iter().find(|n| **n == "DBZH"))
        .copied()
        .unwrap_or(params[0])
        .to_string();
    let tile = view
        .get_raster_tile(
            [23.4, 59.6, 25.6, 61.5],
            64,
            64,
            None,
            &OutputCrs::Wgs84,
            Some(&render_param),
            None,
            None,
        )
        .expect("render of the remotely-scanned volume succeeds");
    assert_eq!(tile.values.len(), 64 * 64);
    assert!(
        tile.values.iter().any(Option::is_some),
        "a render over the radar's own coverage must sample some echoes"
    );
}

/// A `get_raster_tile` call with no `parameter` defaults to the site's
/// primary (first) quantity — a bare `LAYERS={site}` WMS / Maps request
/// renders the primary moment rather than erroring.
#[test]
fn pvol_bare_render_defaults_to_primary_quantity() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/radar-fmi-pvol/202605191050_fivih_PVOL.h5");
    if !fixture.exists() {
        eprintln!("skipping pvol_bare_render_defaults_to_primary_quantity: fixture absent");
        return;
    }
    let data_dir = fixture.parent().unwrap().to_str().unwrap();
    let config = OdimConfig {
        filename_template: None,
        filename_pattern: None,
        timestamp_format: None,
        parameter: None,
        unit: None,
        nodata: None,
        gain: None,
        offset: None,
        poll_interval_secs: 30,
        max_files: None,
        endpoint: None,
        bucket: None,
        prefix_pattern: None,
        time_window: None,
        discovery: None,
        cadence_secs: None,
    };
    let engine =
        engine_odim::PolarVolumeEngine::new("fivih-pvol-test", Some(data_dir), &config).unwrap();
    let view = engine.site_view("fivih", "fivih-pvol-test-fivih");

    // No parameter named → render the primary quantity (Ok), not a 400.
    let tile = view
        .get_raster_tile(
            [23.4, 59.6, 25.6, 61.5],
            8,
            8,
            None,
            &OutputCrs::Wgs84,
            None,
            None,
            None,
        )
        .expect("a bare (no-parameter) render must default to the primary quantity");
    assert_eq!(tile.values.len(), 8 * 8);
}

// ---------------------------------------------------------------------------
// PolarVolumeEngine — EdrEngine (M3a: sites as locations + position queries)
// ---------------------------------------------------------------------------

/// Build the per-site `PolarVolumeSiteView` for the Vihti (`fivih`)
/// radar over the local `radar-fmi-pvol` fixture directory, or `None` when
/// the (uncommitted, 15 MB) fixture is absent — so these tests skip
/// gracefully in CI. Each radar site is its own collection,
/// served by such a view; the owning engine may be dropped because the
/// view holds an `Arc` of the shared catalog.
fn pvol_fixture_view() -> Option<engine_odim::PolarVolumeSiteView> {
    let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/radar-fmi-pvol");
    if !fixture_dir.join("202605191050_fivih_PVOL.h5").exists() {
        return None;
    }
    let config = OdimConfig {
        filename_template: None,
        filename_pattern: None,
        timestamp_format: None,
        parameter: None,
        unit: None,
        nodata: None,
        gain: None,
        offset: None,
        poll_interval_secs: 30,
        max_files: None,
        endpoint: None,
        bucket: None,
        prefix_pattern: None,
        time_window: None,
        discovery: None,
        cadence_secs: None,
    };
    let engine = engine_odim::PolarVolumeEngine::new(
        "fivih-edr-test",
        Some(fixture_dir.to_str().expect("utf8 fixture dir")),
        &config,
    )
    .expect("PolarVolumeEngine::new over the PVOL fixture directory");
    Some(engine.site_view("fivih", "fivih-edr-test-fivih"))
}

/// `get_locations` exposes each radar site as an EDR location keyed by
/// its ODIM NOD code, with the antenna position as the point geometry.
#[test]
fn pvol_edr_get_locations_lists_sites() {
    let Some(view) = pvol_fixture_view() else {
        eprintln!("skipping pvol_edr_get_locations_lists_sites: fixture absent");
        return;
    };
    let locations = EdrEngine::get_locations(&view).expect("get_locations");
    assert!(
        !locations.is_empty(),
        "the PVOL fixture must surface at least one radar site"
    );
    let fivih = locations
        .iter()
        .find(|l| l.id == "fivih")
        .expect("Vihti site keyed by NOD code `fivih`");
    // Vihti sits near 24.50°E, 60.56°N.
    assert!(
        (23.5..25.5).contains(&fivih.longitude) && (60.0..61.0).contains(&fivih.latitude),
        "fivih antenna near 24.50E/60.56N, got {},{}",
        fivih.longitude,
        fivih.latitude
    );
    assert!(!fivih.label.is_empty(), "a location carries a label");
}

/// A position query with no `z` returns a `CoverageCollection` of
/// A position query for a point far outside the real radar's coverage
/// (Kansas vs a Finnish radar) is `LocationNotFound` (404), not HTTP 200
/// all-null — exercises the coverage-radius guard on real data.
#[test]
fn pvol_edr_position_outside_coverage_is_404() {
    let Some(view) = pvol_fixture_view() else {
        eprintln!("skipping pvol_edr_position_outside_coverage_is_404: fixture absent");
        return;
    };
    assert!(
        matches!(
            EdrEngine::query_position(&view, "POINT(-100 40)", None, None, None, None),
            Err(ds_core::error::DataServerError::LocationNotFound(_))
        ),
        "a point ~7000 km from the radar must be outside coverage (404)"
    );
}

/// `VerticalProfile`s — one per timestep — sampling every sweep.
#[test]
fn pvol_edr_query_position_returns_vertical_profiles() {
    let Some(view) = pvol_fixture_view() else {
        eprintln!("skipping pvol_edr_query_position_returns_vertical_profiles: fixture absent");
        return;
    };
    // ~30 km north of Vihti — inside the lowest sweep.
    let response = EdrEngine::query_position(&view, "POINT(24.5 60.85)", None, None, None, None)
        .expect("position query inside radar coverage");
    let coverages = match response {
        CoverageResponse::Collection(c) => c,
        CoverageResponse::Single(_) => panic!("a no-z PVOL query must return a Collection"),
    };
    assert!(!coverages.is_empty(), "fixture has at least one timestep");
    for qr in &coverages {
        match &qr.domain {
            DomainDescription::VerticalProfile { x, y, z, .. } => {
                assert!((*x - 24.5).abs() < 1e-9 && (*y - 60.85).abs() < 1e-9);
                assert!(!z.values.is_empty(), "a profile spans the sweep angles");
                for arr in qr.ranges.values() {
                    assert_eq!(arr.shape, vec![z.values.len()]);
                    assert_eq!(arr.axis_names, vec!["z".to_string()]);
                }
            }
            other => panic!("expected VerticalProfile, got {other:?}"),
        }
        assert!(
            !qr.ranges.is_empty(),
            "a sweep exposes at least one quantity"
        );
    }
}

/// A position query pinned to a single `z` level returns one
/// `PointSeries` (a `Single` coverage).
#[test]
fn pvol_edr_query_position_with_z_returns_point_series() {
    let Some(view) = pvol_fixture_view() else {
        eprintln!("skipping pvol_edr_query_position_with_z_returns_point_series: fixture absent");
        return;
    };
    let vertical = EdrEngine::get_vertical_extent(&view).expect("PVOL has a vertical extent");
    let level = vertical.levels[0];
    let response =
        EdrEngine::query_position(&view, "POINT(24.5 60.85)", None, None, Some(&[level]), None)
            .expect("z-pinned position query");
    let result = match response {
        CoverageResponse::Single(qr) => qr,
        CoverageResponse::Collection(_) => panic!("a single-z query must return Single"),
    };
    match &result.domain {
        DomainDescription::PointSeries { z, .. } => {
            assert_eq!(z.as_ref().expect("z axis").values, vec![level]);
        }
        other => panic!("expected PointSeries, got {other:?}"),
    }
}

/// `query_location` by NOD code returns the site's vertical profiles;
/// an unknown id is `LocationNotFound` (HTTP 404), not a panic.
#[test]
fn pvol_edr_query_location_by_nod() {
    let Some(view) = pvol_fixture_view() else {
        eprintln!("skipping pvol_edr_query_location_by_nod: fixture absent");
        return;
    };
    let response = EdrEngine::query_location(&view, "fivih", None, None, None, None)
        .expect("query_location for the fivih site");
    let coverages = match response {
        CoverageResponse::Collection(c) => c,
        CoverageResponse::Single(_) => panic!("a no-z site query must return a Collection"),
    };
    assert!(!coverages.is_empty());
    for qr in &coverages {
        assert!(matches!(
            qr.domain,
            DomainDescription::VerticalProfile { .. }
        ));
    }

    match EdrEngine::query_location(&view, "nosuchsite", None, None, None, None) {
        Err(ds_core::error::DataServerError::LocationNotFound(_)) => {}
        other => panic!("unknown location id must be LocationNotFound, got {other:?}"),
    }
}

/// An area query collects every in-polygon radar site's coverages; a
/// polygon far from any radar is `LocationNotFound`.
#[test]
fn pvol_edr_query_area_collects_sites() {
    let Some(view) = pvol_fixture_view() else {
        eprintln!("skipping pvol_edr_query_area_collects_sites: fixture absent");
        return;
    };
    // A bbox enclosing Vihti (24.50E, 60.56N).
    let result = EdrEngine::query_area(&view, "23.0,59.0,26.0,62.0", None, None, None, None)
        .expect("area query enclosing the fivih site");
    match result {
        CoverageResponse::Collection(coverages) => {
            assert!(
                !coverages.is_empty(),
                "the polygon encloses fivih, so the collection is non-empty"
            );
            for qr in &coverages {
                assert!(matches!(
                    qr.domain,
                    DomainDescription::VerticalProfile { .. }
                ));
            }
        }
        CoverageResponse::Single(_) => {
            panic!("a point/observation area query must return a Collection")
        }
    }

    // A polygon far from any FMI radar matches nothing.
    match EdrEngine::query_area(&view, "0.0,0.0,1.0,1.0", None, None, None, None) {
        Err(ds_core::error::DataServerError::LocationNotFound(_)) => {}
        other => panic!("an empty area must be LocationNotFound, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// PolarVolumeEngine — EdrEngine (M4: trajectory cross-sections)
// ---------------------------------------------------------------------------

/// A trajectory query returns a `Section` coverage whose composite axis
/// has one `(t, lon, lat)` triple per along-path node and whose `z`
/// axis is height above antenna in metres. The range ndarray is
/// `[nodes, z]`, and at least one cell over the radar's own coverage
/// must sample a finite value.
#[test]
fn pvol_edr_query_trajectory_returns_section() {
    let Some(view) = pvol_fixture_view() else {
        eprintln!("skipping pvol_edr_query_trajectory_returns_section: fixture absent");
        return;
    };
    // A ~65 km north-bound leg through Vihti (~24.50°E, 60.56°N), so the
    // path crosses the radar's lowest sweep coverage along its length.
    // `z` here selects the 0.5°–5° elevation angle band (the
    // cross-section's vertical axis is derived height).
    let coords = "LINESTRING(24.5 60.3, 24.5 60.9)";
    let response = EdrEngine::query_trajectory(
        &view,
        coords,
        None,
        Some(&["DBZH".to_string()]),
        Some(&[0.5, 5.0]),
        None,
    )
    .expect("trajectory query inside radar coverage");
    let qr = match &response {
        CoverageResponse::Single(q) => q.clone(),
        CoverageResponse::Collection(c) => {
            assert!(!c.is_empty(), "a non-empty fixture yields ≥1 section");
            c[0].clone()
        }
    };

    let (nodes, z) = match &qr.domain {
        DomainDescription::Section { nodes, z } => (nodes, z),
        other => panic!("expected Section domain, got {other:?}"),
    };
    assert!(
        nodes.len() >= 2,
        "Section path must keep ≥2 along-path nodes"
    );
    assert!(z.values.len() >= 2, "Section z axis must have ≥2 levels");

    let dbzh = qr.ranges.get("DBZH").expect("DBZH range");
    assert_eq!(dbzh.shape, vec![nodes.len(), z.values.len()]);
    assert_eq!(
        dbzh.axis_names,
        vec!["composite".to_string(), "z".to_string()]
    );
    let non_none = dbzh.values.iter().filter(|v| v.is_some()).count();
    assert!(
        non_none > 0,
        "a section over the radar's own coverage must sample ≥1 finite value"
    );
}

/// A trajectory whose path lies entirely outside this site's coverage
/// still yields a Section, but every cell is nodata: a per-site view
/// always samples its own radar (no nearest-site pick), so a far-out path
/// produces a Section shaped against this site's z extent with all-None
/// cells.
#[test]
fn pvol_edr_query_trajectory_out_of_coverage_yields_empty_cells() {
    let Some(view) = pvol_fixture_view() else {
        eprintln!(
            "skipping pvol_edr_query_trajectory_out_of_coverage_yields_empty_cells: fixture absent"
        );
        return;
    };
    // A line near the antipode of FMI radars — every sample is out of
    // range. `z` selects a low elevation-angle band.
    let coords = "LINESTRING(-150 -30, -150 -29)";
    match EdrEngine::query_trajectory(
        &view,
        coords,
        None,
        Some(&["DBZH".to_string()]),
        Some(&[0.5, 3.0]),
        None,
    ) {
        Ok(response) => {
            let qr = match response {
                CoverageResponse::Single(q) => q,
                CoverageResponse::Collection(c) => c.into_iter().next().expect("non-empty fixture"),
            };
            let dbzh = qr.ranges.get("DBZH").expect("DBZH range");
            assert!(
                dbzh.values.iter().all(Option::is_none),
                "out-of-coverage trajectory must produce all-None values"
            );
        }
        Err(ds_core::error::DataServerError::LocationNotFound(_)) => {
            // Acceptable degenerate: every selected volume yielded no
            // Section (e.g. catalog held no sites for the time range).
        }
        Err(e) => panic!("unexpected error for far-from-radar trajectory: {e:?}"),
    }
}

/// LINESTRING parsing failures bubble up as `InvalidParameter` so the
/// API layer can map to HTTP 400.
#[test]
fn pvol_edr_query_trajectory_rejects_malformed_linestring() {
    let Some(view) = pvol_fixture_view() else {
        eprintln!(
            "skipping pvol_edr_query_trajectory_rejects_malformed_linestring: fixture absent"
        );
        return;
    };
    for bad in [
        "POINT(27.1 60.9)",
        "LINESTRING(27.1 60.9)",
        "LINESTRINGZ(27.1 60.9 0, 27.1 61.1 100)",
        "LINESTRING(NaN 60.9, 27.1 61.1)",
    ] {
        match EdrEngine::query_trajectory(&view, bad, None, None, None, None) {
            Err(ds_core::error::DataServerError::InvalidParameter(_)) => {}
            other => panic!("expected InvalidParameter for `{bad}`, got {other:?}"),
        }
    }
}

/// #286 — the COMP engine over a *remote* `DataStore`.
///
/// `object_store`'s `LocalFileSystem` exposes the same `list`/`get`
/// surface an HTTP(S) `HttpStore` (or S3) does, so building one over a
/// tempdir holding the committed DMI fixture drives the exact remote
/// `scan_remote` → seed → render path that an `http(s)://` `data_path`
/// takes — without standing up a WebDAV server. (`build_source`'s URL
/// routing is unit-tested separately in `engine.rs`; this asserts the
/// resulting remote source scans, reports extents, and renders.)
#[test]
fn comp_engine_remote_scan_discovers_and_renders_dmi_fixture() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/odim-dmi-fixture.h5")
        .canonicalize()
        .expect("fixture path canonicalises");
    // Two timesteps so the catalog scan/sort/seed path is non-trivial.
    for name in [
        "dk.com.202601201125.500_max.h5",
        "dk.com.202601201130.500_max.h5",
    ] {
        std::fs::copy(&src, dir.path().join(name)).expect("copy fixture");
    }

    // A `DataStore` over the fixture directory — the same trick the PVOL
    // remote test and the catalog unit tests use to exercise the remote
    // backend offline. `_base` (the store-relative prefix) is empty here.
    let (store, _base) = ds_storage::build_store(
        dir.path()
            .canonicalize()
            .expect("tempdir canonicalises")
            .to_str()
            .expect("utf8 tempdir path"),
    )
    .expect("build a DataStore over the fixture directory");

    let config = OdimConfig {
        filename_template: Some("dk.com.%Y%m%d%H%M.500_max.h5".into()),
        filename_pattern: None,
        timestamp_format: None,
        parameter: Some("reflectivity".into()),
        unit: Some("dBZ".into()),
        nodata: None,
        gain: None,
        offset: None,
        poll_interval_secs: 30,
        max_files: None,
        endpoint: None,
        bucket: None,
        prefix_pattern: None,
        time_window: None,
        discovery: None,
        cadence_secs: None,
    };

    // Empty prefix scans the store root, where the fixtures live.
    let engine = engine_odim::OdimEngine::new_remote_for_test(
        "dmi-remote-test",
        store,
        "",
        "reflectivity",
        "dBZ",
        &config,
    )
    .expect("OdimEngine remote scan over the fixture directory");

    // Metadata is populated from the remotely-fetched seed composite.
    let info = engine.raster_info();
    assert!(
        info.native_crs.to_lowercase().contains("stere"),
        "remote scan must report the DMI stereographic CRS, got `{}`",
        info.native_crs
    );
    assert_eq!(
        info.times.len(),
        2,
        "both remote timesteps must be cataloged"
    );
    assert!(
        info.spatial_extent.is_some(),
        "remote scan must report a spatial extent"
    );

    // A render proves the streamed-then-parsed composite is intact: the
    // bytes came through `DataStore::get`, not a local `std::fs::read`.
    let tile = engine
        .get_raster_tile(
            [9.5, 55.0, 12.5, 57.0], // over Jylland / Sjælland
            64,
            64,
            None,
            &OutputCrs::Wgs84,
            None,
            None,
            None,
        )
        .expect("render of the remotely-scanned composite succeeds");
    assert_eq!(tile.values.len(), 64 * 64);
}

/// #287 — template (listing-free) HTTP discovery.
///
/// `object_store`'s `LocalFileSystem` answers `head` per object exactly
/// like an HTTP `HEAD` (and returns `NotFound` for a missing key), so a
/// `DataStore` over a tempdir drives the real `head_many` probe path
/// offline. The probe builds candidate filenames from the strftime
/// template for timestamps walked back from an **injected** `now`, so the
/// set of discovered files is deterministic. We lay down files for some
/// slots and leave a gap, and assert the probe finds exactly the present
/// ones — never listing the directory.
#[test]
fn comp_template_discovery_probes_present_slots_only() {
    use chrono::{TimeZone, Utc};

    let dir = tempfile::tempdir().expect("create tempdir");
    let template = "composite_hx_%Y%m%d_%H%M-hd5";
    // Aligned reference clock; 5-minute cadence.
    let now = Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap();
    let cadence_secs = 300;

    // Present: now, now-5m, now-10m, now-20m. Absent: now-15m (a gap), and
    // the older slots the -PT30M window still probes (now-25m, now-30m).
    for back_min in [0i64, 5, 10, 20] {
        let t = now - chrono::Duration::minutes(back_min);
        let name = t.format(template).to_string();
        std::fs::write(dir.path().join(name), b"x").expect("write candidate");
    }
    // A decoy with no parseable slot must never be probed/added.
    std::fs::write(dir.path().join("composite_hx_LATEST-hd5"), b"x").unwrap();

    let (store, _base) = ds_storage::build_store(
        dir.path()
            .canonicalize()
            .expect("tempdir canonicalises")
            .to_str()
            .expect("utf8 tempdir path"),
    )
    .expect("DataStore over the tempdir");

    // Empty base prefix → probe the store root, where the files live.
    let catalog = engine_odim::OdimEngine::discover_template_for_test(
        now,
        store,
        "",
        template,
        cadence_secs,
        Some("-PT30M"),
        None,
        &[], // cold scan — no prior catalog
    )
    .expect("template probe succeeds");

    // Exactly the four present dated slots, ascending; the 11:45 gap and the
    // empty 11:30/11:35 slots are excluded, and `LATEST` is never probed.
    let times: Vec<String> = catalog.iter().map(|e| e.time.to_rfc3339()).collect();
    assert_eq!(
        times,
        [
            "2026-06-03T11:40:00+00:00",
            "2026-06-03T11:50:00+00:00",
            "2026-06-03T11:55:00+00:00",
            "2026-06-03T12:00:00+00:00",
        ],
        "probe must discover exactly the present dated slots"
    );
    // Keys are the template-rendered names under the (empty) base prefix.
    assert!(catalog
        .iter()
        .all(|e| e.location.id().starts_with("composite_hx_")));
}

/// #287 — incremental polling: a slot already in `known` is carried
/// forward WITHOUT being re-probed (radar files are immutable), so only
/// genuinely-new slots cost a `HEAD`.
///
/// Proven by making the known slots' files **absent from disk**: if the
/// probe re-`HEAD`-ed them they'd 404 and drop out; their survival proves
/// they were carried forward, not re-probed. Meanwhile a new slot that
/// *does* exist on disk is discovered.
#[test]
fn comp_template_discovery_carries_forward_known_without_reprobe() {
    use chrono::{TimeZone, Utc};
    use engine_odim::catalog::{CatalogEntry, Location};

    let dir = tempfile::tempdir().expect("create tempdir");
    let template = "composite_hx_%Y%m%d_%H%M-hd5";
    let now = Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap();

    // On disk: ONLY the freshest slot (now). The two older slots are NOT
    // written — they exist only in `known`.
    let now_name = now.format(template).to_string();
    std::fs::write(dir.path().join(&now_name), b"x").unwrap();

    let (store, _base) = ds_storage::build_store(
        dir.path()
            .canonicalize()
            .unwrap()
            .to_str()
            .expect("utf8 path"),
    )
    .unwrap();

    // `known` carries the two older slots (11:55, 11:50) whose files are absent.
    let known: Vec<CatalogEntry> = [5i64, 10]
        .iter()
        .map(|&back| {
            let t = now - chrono::Duration::minutes(back);
            CatalogEntry {
                time: t,
                location: Location::Remote {
                    store: store.clone(),
                    key: t.format(template).to_string(),
                },
            }
        })
        .collect();

    let catalog = engine_odim::OdimEngine::discover_template_for_test(
        now,
        store,
        "",
        template,
        300,
        Some("-PT30M"),
        None,
        &known,
    )
    .expect("incremental probe succeeds");

    // All three present: the two carried-forward (file-less) known slots and
    // the one freshly-probed slot — proving known slots were NOT re-HEADed.
    let times: Vec<String> = catalog.iter().map(|e| e.time.to_rfc3339()).collect();
    assert_eq!(
        times,
        [
            "2026-06-03T11:50:00+00:00", // carried forward (no file on disk)
            "2026-06-03T11:55:00+00:00", // carried forward (no file on disk)
            "2026-06-03T12:00:00+00:00", // freshly discovered by HEAD
        ],
        "known slots must be carried forward without re-probing"
    );
}
