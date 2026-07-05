//! Regression: low-zoom / extreme Web-Mercator viewports must not paint ghost
//! echoes far from the source footprint, and the data that IS painted must stay
//! within the raster's true extent. Reproduces the reported FMI (EPSG:3067)
//! "echoes way north / east curtains" bug against the committed TM35FIN fixture
//! (`testdata/radar-tm35fin/`, the same trapezoid footprint as the production
//! composite: lon ~6.7–43°E, lat ~56–72.8°N).

use ds_core::config::GeoTiffConfig;
use ds_core::map_engine::{MapEngine, OutputCrs};
use engine_geotiff::GeoTiffEngine;

fn engine() -> GeoTiffEngine {
    let config = GeoTiffConfig {
        filename_template: Some("radar_tm35_%Y%m%dT%H%MZ.tif".to_string()),
        filename_pattern: None,
        timestamp_format: None,
        parameter: "reflectivity".to_string(),
        unit: "dBZ".to_string(),
        poll_interval_secs: 3600,
        tile_cache_mb: 64,
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
    .expect("engine should build from the committed TM35FIN fixture")
}

fn wgs84_of_merc(minx: f64, miny: f64, maxx: f64, maxy: f64) -> [f64; 4] {
    const R: f64 = 6_378_137.0;
    let lon = |x: f64| (x / R).to_degrees();
    let lat = |y: f64| (2.0 * (y / R).exp().atan() - std::f64::consts::FRAC_PI_2).to_degrees();
    [lon(minx), lat(miny), lon(maxx), lat(maxy)]
}

fn normalize_lon(mut lon: f64) -> f64 {
    while lon > 180.0 {
        lon -= 360.0;
    }
    while lon < -180.0 {
        lon += 360.0;
    }
    lon
}

/// Count data pixels whose true geography is outside the footprint (ghosts).
fn ghost_count(engine: &GeoTiffEngine, bbox: [f64; 4], w: u32, h: u32) -> (usize, usize) {
    let crs = OutputCrs::WebMercator;
    let tile = engine
        .get_raster_tile(bbox, w, h, None, &crs, None, None, None)
        .expect("render");
    // Footprint trapezoid envelope ~ lon[6.7,43.1] lat[55.9,72.9], with slack.
    let (lon_lo, lon_hi, lat_lo, lat_hi) = (4.0, 46.0, 54.0, 74.0);
    let mut ghosts = 0;
    let mut total = 0;
    for oy in 0..h {
        for ox in 0..w {
            if tile.values.value_at((oy * w + ox) as usize).is_none() {
                continue;
            }
            total += 1;
            let (lon, lat) = crs.project_node(
                bbox,
                (ox as f64 + 0.5) / w as f64,
                (oy as f64 + 0.5) / h as f64,
            );
            let lonn = normalize_lon(lon);
            if lonn < lon_lo || lonn > lon_hi || lat < lat_lo || lat > lat_hi {
                ghosts += 1;
            }
        }
    }
    (total, ghosts)
}

#[test]
fn extreme_zoom_paints_no_ghosts() {
    let engine = engine();
    // URL2 (the user's "echoes way north" example): bbox wraps past ±180° and
    // reaches ~88°N — the worst case.
    let url2 = wgs84_of_merc(
        -24695942.371988516,
        -19018081.929075003,
        35450108.88338889,
        26935199.892201833,
    );
    let (total, ghosts) = ghost_count(&engine, url2, 700, 535);
    println!("URL2 extreme: {total} data px, {ghosts} ghosts");
    assert!(
        total > 0,
        "the primary copy of the footprint must still render"
    );
    assert_eq!(ghosts, 0, "no data may be painted outside the footprint");
}

#[test]
fn wide_views_stay_within_footprint() {
    let engine = engine();
    // A spread of zoomed-out views; none may leak data outside the footprint.
    for (label, mx0, my0, mx1, my1) in [
        ("url1", -1453541.8, 5261910.7, 8215850.6, 12649599.5),
        (
            "hemispheric",
            -10_000_000.0,
            3_000_000.0,
            12_000_000.0,
            16_000_000.0,
        ),
    ] {
        let bbox = wgs84_of_merc(mx0, my0, mx1, my1);
        let (total, ghosts) = ghost_count(&engine, bbox, 700, 535);
        println!("{label}: {total} data px, {ghosts} ghosts");
        assert!(total > 0, "{label}: footprint should render");
        assert_eq!(ghosts, 0, "{label}: no ghosts outside footprint");
    }
}
