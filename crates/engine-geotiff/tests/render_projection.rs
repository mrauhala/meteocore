//! End-to-end render test for a projected-CRS (EPSG:3067 TM35FIN) GeoTIFF.
//!
//! Exercises the full `MapEngine::get_raster_tile` path — catalog scan, COG
//! tile read, and the coarse-grid resampler introduced for issue #203 — and
//! checks that the projected source data lands where it should for both the
//! WGS84 and Web Mercator output CRSs.

use ds_core::config::GeoTiffConfig;
use ds_core::map_engine::{MapEngine, OutputCrs};
use engine_geotiff::GeoTiffEngine;

/// Builds the engine over the committed 480×360 TM35FIN radar fixture
/// (`testdata/radar-tm35fin/`).
fn tm35fin_engine() -> GeoTiffEngine {
    let config = GeoTiffConfig {
        filename_template: Some("radar_tm35_%Y%m%dT%H%MZ.tif".to_string()),
        filename_pattern: None,
        timestamp_format: None,
        parameter: "reflectivity".to_string(),
        unit: "dBZ".to_string(),
        poll_interval_secs: 3600, // no polling during the test
        tile_cache_mb: 16,
        band: 1,
        max_files: None,
        nodata: None,
        scale: None,
        offset: None,
        exclude_patterns: vec![],
        endpoint: None,
        bucket: None,
        prefix_pattern: None,
        time_window: None,
        scan_days: None,
        stac_url: None,
        stac_asset_key: "data".to_string(),
        stac_asset_allowlist: None,
    };
    GeoTiffEngine::new(
        "radar-tm35fin",
        Some("../../testdata/radar-tm35fin"),
        &config,
    )
    .expect("engine should build from the TM35FIN fixture")
}

fn count_data(tile: &ds_core::map_engine::RasterTile) -> usize {
    tile.values.iter_values().filter(|v| v.is_some()).count()
}

// A bbox well inside the fixture's coverage (its trapezoid spans lon 6.7–43°E,
// lat 56–72°N). A radar composite is mostly no-echo nodata, so only a fraction
// of pixels carry data — the resampler must still place that data correctly.
const COVERED_BBOX: [f64; 4] = [20.0, 60.0, 32.0, 67.0];

// Lower bound on the share of output pixels that must carry data for a render
// over `COVERED_BBOX`. This is intentionally coupled to the committed fixture:
// the FMI composite in `testdata/radar-tm35fin/` has dense echo over southern
// Finland and fills well over half of this bbox. The 25% floor sits far below
// that actual coverage, so the assertion proves the resampler placed a broad
// swath of projected data without being brittle to the exact echo pattern. If
// the fixture is ever regenerated from a clearer-sky scan, revisit this floor.
const MIN_DATA_FRACTION_DENOM: usize = 4;

#[test]
fn renders_projected_geotiff_to_wgs84() {
    let engine = tm35fin_engine();
    let (w, h) = (256, 256);
    let tile = engine
        .get_raster_tile(
            COVERED_BBOX,
            w,
            h,
            None,
            &OutputCrs::Wgs84,
            None,
            None,
            None,
        )
        .expect("render should succeed");

    assert_eq!(tile.width, w);
    assert_eq!(tile.height, h);
    assert_eq!(tile.values.len() as u32, w * h);
    let data = count_data(&tile);
    // The coarse-grid resampler must pull a substantial amount of the
    // projected source data into the output (see MIN_DATA_FRACTION_DENOM).
    assert!(
        data > (w * h) as usize / MIN_DATA_FRACTION_DENOM,
        "expected broad data coverage, got {data}/{} pixels",
        w * h
    );
}

#[test]
fn web_mercator_and_wgs84_render_consistently() {
    // Both output-CRS paths feed the same coarse-grid resampler. Over a
    // mid-latitude bbox they should pull in nearly the same amount of data;
    // a large divergence would mean one projection path is broken.
    let engine = tm35fin_engine();
    let (w, h) = (256, 256);
    let merc = engine
        .get_raster_tile(
            COVERED_BBOX,
            w,
            h,
            None,
            &OutputCrs::WebMercator,
            None,
            None,
            None,
        )
        .expect("render should succeed");
    let wgs = engine
        .get_raster_tile(
            COVERED_BBOX,
            w,
            h,
            None,
            &OutputCrs::Wgs84,
            None,
            None,
            None,
        )
        .expect("render should succeed");

    let (dm, dw) = (count_data(&merc) as f64, count_data(&wgs) as f64);
    assert!(dm > 0.0 && dw > 0.0, "both renders must contain data");
    assert!(
        (dm - dw).abs() / dw < 0.1,
        "WebMercator ({dm}) and WGS84 ({dw}) data counts diverge too far"
    );
}

#[test]
fn renders_projected_geotiff_to_epsg3067() {
    // Regression for #251/#160: rendering with a *projected* output CRS
    // (EPSG:3067) must place real data, not a fully-transparent tile. The
    // source raster is itself TM35FIN, so output crs=3067 is a near-identity
    // resample and should be at least as well-covered as the WGS84 render.
    let engine = tm35fin_engine();
    let crs = ds_core::geo::projected_output_crs("EPSG:3067").expect("3067 defined");
    // Project COVERED_BBOX into EPSG:3067 metres (the request rectangle a
    // Finland-native client sends), and derive the WGS84 read window the API
    // layer would pass alongside it.
    let proj_bbox = ds_core::geo::projected_envelope(&crs, COVERED_BBOX);
    let wgs84 = ds_core::geo::wgs84_envelope(&crs, proj_bbox).expect("in-domain bbox has envelope");
    let output_crs = OutputCrs::Projected {
        crs,
        bbox: proj_bbox,
    };
    let (w, h) = (256, 256);
    let tile = engine
        .get_raster_tile(wgs84, w, h, None, &output_crs, None, None, None)
        .expect("projected render should succeed");

    assert_eq!(tile.values.len() as u32, w * h);
    let data = count_data(&tile);
    assert!(
        data > (w * h) as usize / MIN_DATA_FRACTION_DENOM,
        "EPSG:3067 render must place a broad swath of data, got {data}/{} \
         (the #251 bug returned 0)",
        w * h
    );

    // Sanity: the projected render covers a comparable amount of data to the
    // WGS84 render over the same geographic region.
    let wgs = engine
        .get_raster_tile(
            COVERED_BBOX,
            w,
            h,
            None,
            &OutputCrs::Wgs84,
            None,
            None,
            None,
        )
        .expect("wgs84 render should succeed");
    let (dp, dw) = (data as f64, count_data(&wgs) as f64);
    assert!(
        (dp - dw).abs() / dw < 0.25,
        "EPSG:3067 ({dp}) and WGS84 ({dw}) data counts diverge too far"
    );
}

#[test]
fn bbox_outside_coverage_is_empty() {
    let engine = tm35fin_engine();
    // Mid-Atlantic — nowhere near the TM35FIN raster.
    let bbox = [-50.0, 10.0, -40.0, 20.0];
    let tile = engine
        .get_raster_tile(bbox, 64, 64, None, &OutputCrs::Wgs84, None, None, None)
        .expect("render should succeed even with no overlap");
    assert!(
        tile.is_empty(),
        "a bbox outside the raster must resample to all-nodata"
    );
}

#[test]
fn renders_via_overview_for_small_output() {
    // A small output of the whole raster forces `select_overview` to pick a
    // COG overview level, so the coarse-grid resampler runs against an
    // overview GeoTransform rather than the full-resolution one. The fixture
    // ships 240×180 and 120×90 overviews.
    let engine = tm35fin_engine();
    // Bbox spanning the fixture's full extent (trapezoid lon ~6.7–43°E).
    let bbox = [8.0, 57.0, 42.0, 71.0];
    let (w, h) = (96, 96);
    let tile = engine
        .get_raster_tile(bbox, w, h, None, &OutputCrs::Wgs84, None, None, None)
        .expect("overview render should succeed");
    assert_eq!(tile.values.len() as u32, w * h);
    // The whole-extent render of this echo-dense fixture always has data;
    // the assertion's job is to confirm the overview path renders at all.
    assert!(
        count_data(&tile) > 0,
        "overview render must still place projected data"
    );
}

#[test]
fn partially_overlapping_bbox_is_partially_filled() {
    let engine = tm35fin_engine();
    // Straddles the southern edge of coverage: the lat 50–56 strip is below
    // the raster (its bottom edge runs near lat ~56), the lat 56–62 strip is
    // on it and over echo-dense southern Finland. So the resampler must place
    // data on one side and nodata on the other — catching gross projection
    // mis-placement. The on-raster half is the same dense region the
    // COVERED_BBOX tests use, so `data > 0` is not brittle to the echo pattern.
    let bbox = [20.0, 50.0, 32.0, 62.0];
    let (w, h) = (128, 128);
    let tile = engine
        .get_raster_tile(bbox, w, h, None, &OutputCrs::Wgs84, None, None, None)
        .expect("render should succeed");
    let data = count_data(&tile);
    assert!(data > 0, "the on-raster side should have data");
    assert!(
        data < (w * h) as usize,
        "the off-raster side should be nodata"
    );
}

/// #211: `raster_info()` is served from a snapshot rebuilt at each catalog
/// swap, so it is populated (CRS, grid, timestamps) at construction — the
/// first request needs no per-call CRS scan / timestamp-Vec build / metadata
/// fetch — and is stable across calls.
#[test]
fn raster_info_is_cached_and_populated_at_construction() {
    let engine = tm35fin_engine();

    let info = engine.raster_info();
    assert!(
        !info.times.is_empty(),
        "times populated from the committed fixture at construction"
    );
    assert_ne!(
        info.native_crs, "CRS:84",
        "CRS comes from metadata loaded during the scan, not the cold-start default"
    );
    assert!(
        info.grid_size.is_some(),
        "grid size read from the (already-loaded) metadata"
    );

    // Cached snapshot: a second call returns identical data.
    let info2 = engine.raster_info();
    assert_eq!(info.times, info2.times);
    assert_eq!(info.native_crs, info2.native_crs);
    assert_eq!(info.grid_size, info2.grid_size);
}
