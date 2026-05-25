use criterion::{black_box, criterion_group, criterion_main, Criterion};

use ds_core::config::GeoTiffConfig;
use ds_core::edr_engine::EdrEngine;
use ds_core::map_engine::{MapEngine, OutputCrs};
use engine_geotiff::GeoTiffEngine;

fn make_config() -> GeoTiffConfig {
    GeoTiffConfig {
        filename_template: Some("radar_%Y%m%dT%H%MZ.tif".to_string()),
        filename_pattern: None,
        timestamp_format: None,
        parameter: "reflectivity".to_string(),
        unit: "dBZ".to_string(),
        poll_interval_secs: 3600, // long interval — we don't want polling during benchmarks
        tile_cache_mb: 64,
        band: 1,
        max_files: None,
        nodata: Some(255.0),
        scale: None,
        offset: None,
        exclude_patterns: vec!["*.tmp".to_string(), "*.part".to_string()],
        endpoint: None,
        bucket: None,
        prefix_pattern: None,
        time_window: None,
        scan_days: None,
        stac_url: None,
        stac_asset_key: "data".to_string(),
        stac_asset_allowlist: None,
    }
}

fn load_engine() -> GeoTiffEngine {
    GeoTiffEngine::new("radar", Some("../../testdata/radar"), &make_config()).unwrap()
}

fn bench_get_locations(c: &mut Criterion) {
    let engine = load_engine();
    c.bench_function("geotiff_get_locations", |b| {
        b.iter(|| black_box(engine.get_locations().unwrap()))
    });
}

fn bench_query_position(c: &mut Criterion) {
    let engine = load_engine();
    // Point in southern Finland (inside radar coverage)
    let coords = "POINT(25.0 60.5)";

    c.bench_function("geotiff_query_position", |b| {
        b.iter(|| {
            black_box(
                engine
                    .query_position(black_box(coords), None, None, None)
                    .unwrap(),
            )
        })
    });
}

fn bench_query_position_cached(c: &mut Criterion) {
    let engine = load_engine();
    let coords = "POINT(25.0 60.5)";

    // Warm the cache
    let _ = engine.query_position(coords, None, None, None);

    c.bench_function("geotiff_query_position_cached", |b| {
        b.iter(|| {
            black_box(
                engine
                    .query_position(black_box(coords), None, None, None)
                    .unwrap(),
            )
        })
    });
}

fn bench_query_area_small(c: &mut Criterion) {
    let engine = load_engine();
    // Small area — roughly 0.5 x 0.5 degrees
    let coords = "POLYGON((24.5 60.0, 25.0 60.0, 25.0 60.5, 24.5 60.5, 24.5 60.0))";

    // Warm cache
    let _ = engine.query_area(coords, None, None, None);

    c.bench_function("geotiff_query_area_small", |b| {
        b.iter(|| {
            black_box(
                engine
                    .query_area(black_box(coords), None, None, None)
                    .unwrap(),
            )
        })
    });
}

fn bench_query_area_large(c: &mut Criterion) {
    let engine = load_engine();
    // Larger area — roughly 4 x 3 degrees
    let coords = "POLYGON((22.0 59.0, 26.0 59.0, 26.0 62.0, 22.0 62.0, 22.0 59.0))";

    // Warm cache
    let _ = engine.query_area(coords, None, None, None);

    c.bench_function("geotiff_query_area_large", |b| {
        b.iter(|| {
            black_box(
                engine
                    .query_area(black_box(coords), None, None, None)
                    .unwrap(),
            )
        })
    });
}

fn load_tm35fin_engine() -> GeoTiffEngine {
    // The 480×360 EPSG:3067 (Transverse Mercator) radar fixture — exercises
    // the coarse-grid resampler's per-pixel projection avoidance (issue #203).
    let mut config = make_config();
    config.filename_template = Some("radar_tm35_%Y%m%dT%H%MZ.tif".to_string());
    GeoTiffEngine::new(
        "radar-tm35fin",
        Some("../../testdata/radar-tm35fin"),
        &config,
    )
    .unwrap()
}

fn bench_get_raster_tile_projected(c: &mut Criterion) {
    let engine = load_tm35fin_engine();
    // Finland-sized bbox, fullscreen-ish output — the hot WMS render path.
    let bbox = [20.0, 60.0, 32.0, 67.0];
    c.bench_function("geotiff_get_raster_tile_tm35fin_1024", |b| {
        b.iter(|| {
            black_box(
                engine
                    .get_raster_tile(
                        black_box(bbox),
                        1024,
                        1024,
                        None,
                        &OutputCrs::WebMercator,
                        None,
                        None,
                    )
                    .unwrap(),
            )
        })
    });
}

/// The production COG: `testdata/fmi-radar/20260406064000_fmi_radar_composite_dbz.tif`
/// (the `fmi-radar-composite-dbz` collection that drives the ~1.5 s cold-render
/// tail). Used to profile where a cold meta-tile render spends time.
///
/// This is a large local-only fixture **not committed to the repo** (see
/// `testdata/RADAR_SOURCES.md`). Returns `None` when absent so `cargo bench`
/// skips these on CI / fresh clones instead of panicking.
fn load_fmi_engine() -> Option<GeoTiffEngine> {
    let mut config = make_config();
    config.filename_template = Some("%Y%m%d%H%M%S_fmi_radar_composite_dbz.tif".to_string());
    config.parameter = "reflectivity".to_string();
    let engine = GeoTiffEngine::new("fmi-radar", Some("../../testdata/fmi-radar"), &config).ok()?;
    // `new()` succeeds with an *empty* catalog when the directory is missing or
    // no file matches the template — guard on a timestep actually having loaded,
    // so the benches skip cleanly (rather than `unwrap`-panicking) on CI / fresh
    // clones where this large local-only fixture is absent.
    if engine.raster_info().times.is_empty() {
        return None;
    }
    Some(engine)
}

/// One 256×256 Web Mercator meta-tile over southern Finland — the unit of work
/// meta-tiling (#202) calls `get_raster_tile` for, repeated N× per cold render.
fn bench_get_raster_tile_fmi_metatile(c: &mut Criterion) {
    let Some(engine) = load_fmi_engine() else {
        eprintln!("skipping fmi 256 metatile bench: testdata/fmi-radar absent");
        return;
    };
    let bbox = [24.0, 60.0, 25.5, 61.0];
    c.bench_function("geotiff_get_raster_tile_fmi_256_metatile", |b| {
        b.iter(|| {
            black_box(
                engine
                    .get_raster_tile(
                        black_box(bbox),
                        256,
                        256,
                        None,
                        &OutputCrs::WebMercator,
                        None,
                        None,
                    )
                    .unwrap(),
            )
        })
    });
}

/// A fullscreen single-shot render over all of Finland — the direct (non-meta)
/// path, for comparison against the per-tile cost above.
fn bench_get_raster_tile_fmi_fullscreen(c: &mut Criterion) {
    let Some(engine) = load_fmi_engine() else {
        eprintln!("skipping fmi 1024 fullscreen bench: testdata/fmi-radar absent");
        return;
    };
    let bbox = [19.0, 59.0, 32.0, 71.0];
    c.bench_function("geotiff_get_raster_tile_fmi_1024_fullscreen", |b| {
        b.iter(|| {
            black_box(
                engine
                    .get_raster_tile(
                        black_box(bbox),
                        1024,
                        1024,
                        None,
                        &OutputCrs::WebMercator,
                        None,
                        None,
                    )
                    .unwrap(),
            )
        })
    });
}

fn bench_get_parameters(c: &mut Criterion) {
    let engine = load_engine();
    c.bench_function("geotiff_get_parameters", |b| {
        b.iter(|| black_box(engine.get_parameters()))
    });
}

fn bench_get_temporal_extent(c: &mut Criterion) {
    let engine = load_engine();
    c.bench_function("geotiff_get_temporal_extent", |b| {
        b.iter(|| black_box(engine.get_temporal_extent()))
    });
}

fn bench_get_spatial_extent(c: &mut Criterion) {
    let engine = load_engine();
    c.bench_function("geotiff_get_spatial_extent", |b| {
        b.iter(|| black_box(engine.get_spatial_extent()))
    });
}

criterion_group!(
    benches,
    bench_get_locations,
    bench_query_position,
    bench_query_position_cached,
    bench_query_area_small,
    bench_query_area_large,
    bench_get_raster_tile_projected,
    bench_get_raster_tile_fmi_metatile,
    bench_get_raster_tile_fmi_fullscreen,
    bench_get_parameters,
    bench_get_temporal_extent,
    bench_get_spatial_extent,
);
criterion_main!(benches);
