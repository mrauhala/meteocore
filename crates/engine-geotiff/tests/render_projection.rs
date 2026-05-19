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
    tile.values.iter().filter(|v| v.is_some()).count()
}

// A bbox well inside the fixture's coverage (its trapezoid spans lon 6.7–43°E,
// lat 56–72°N). A radar composite is mostly no-echo nodata, so only a fraction
// of pixels carry data — the resampler must still place that data correctly.
const COVERED_BBOX: [f64; 4] = [20.0, 60.0, 32.0, 67.0];

#[test]
fn renders_projected_geotiff_to_wgs84() {
    let engine = tm35fin_engine();
    let (w, h) = (256, 256);
    let tile = engine
        .get_raster_tile(COVERED_BBOX, w, h, None, &OutputCrs::Wgs84, None, None)
        .expect("render should succeed");

    assert_eq!(tile.width, w);
    assert_eq!(tile.height, h);
    assert_eq!(tile.values.len() as u32, w * h);
    let data = count_data(&tile);
    // The coarse-grid resampler must pull a substantial amount of the
    // projected source data into the output (not just a stray pixel).
    assert!(
        data > (w * h) as usize / 4,
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
        )
        .expect("render should succeed");
    let wgs = engine
        .get_raster_tile(COVERED_BBOX, w, h, None, &OutputCrs::Wgs84, None, None)
        .expect("render should succeed");

    let (dm, dw) = (count_data(&merc) as f64, count_data(&wgs) as f64);
    assert!(dm > 0.0 && dw > 0.0, "both renders must contain data");
    assert!(
        (dm - dw).abs() / dw < 0.1,
        "WebMercator ({dm}) and WGS84 ({dw}) data counts diverge too far"
    );
}

#[test]
fn bbox_outside_coverage_is_empty() {
    let engine = tm35fin_engine();
    // Mid-Atlantic — nowhere near the TM35FIN raster.
    let bbox = [-50.0, 10.0, -40.0, 20.0];
    let tile = engine
        .get_raster_tile(bbox, 64, 64, None, &OutputCrs::Wgs84, None, None)
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
        .get_raster_tile(bbox, w, h, None, &OutputCrs::Wgs84, None, None)
        .expect("overview render should succeed");
    assert_eq!(tile.values.len() as u32, w * h);
    assert!(
        count_data(&tile) > 0,
        "overview render must still place projected data"
    );
}

#[test]
fn partially_overlapping_bbox_is_partially_filled() {
    let engine = tm35fin_engine();
    // Straddles the western edge of coverage: the left part of the bbox is
    // off-raster, the right part is on it — so the resampler must place data
    // on one side only. This catches gross projection mis-placement.
    let bbox = [-30.0, 60.0, 15.0, 68.0];
    let (w, h) = (128, 128);
    let tile = engine
        .get_raster_tile(bbox, w, h, None, &OutputCrs::Wgs84, None, None)
        .expect("render should succeed");
    let data = count_data(&tile);
    assert!(data > 0, "the on-raster side should have data");
    assert!(
        data < (w * h) as usize,
        "the off-raster side should be nodata"
    );
}
