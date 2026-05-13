use criterion::{black_box, criterion_group, criterion_main, Criterion};

use ds_core::config::GeoTiffConfig;
use ds_core::edr_engine::EdrEngine;
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
                    .query_position(black_box(coords), None, None)
                    .unwrap(),
            )
        })
    });
}

fn bench_query_position_cached(c: &mut Criterion) {
    let engine = load_engine();
    let coords = "POINT(25.0 60.5)";

    // Warm the cache
    let _ = engine.query_position(coords, None, None);

    c.bench_function("geotiff_query_position_cached", |b| {
        b.iter(|| {
            black_box(
                engine
                    .query_position(black_box(coords), None, None)
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
    let _ = engine.query_area(coords, None, None);

    c.bench_function("geotiff_query_area_small", |b| {
        b.iter(|| black_box(engine.query_area(black_box(coords), None, None).unwrap()))
    });
}

fn bench_query_area_large(c: &mut Criterion) {
    let engine = load_engine();
    // Larger area — roughly 4 x 3 degrees
    let coords = "POLYGON((22.0 59.0, 26.0 59.0, 26.0 62.0, 22.0 62.0, 22.0 59.0))";

    // Warm cache
    let _ = engine.query_area(coords, None, None);

    c.bench_function("geotiff_query_area_large", |b| {
        b.iter(|| black_box(engine.query_area(black_box(coords), None, None).unwrap()))
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
    bench_get_parameters,
    bench_get_temporal_extent,
    bench_get_spatial_extent,
);
criterion_main!(benches);
