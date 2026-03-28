//! Benchmarks comparing local vs remote GeoTIFF reads.
//!
//! Compares the same queries against:
//! 1. Local file reads (no `block_in_place` overhead)
//! 2. Local file reads through the DataStore sync bridge (`block_in_place`)
//!
//! The difference between these two measurements isolates the `block_in_place`
//! overhead, which is what the async engine refactor would eliminate.
//!
//! For the "remote" case, we point the engine at a local directory but use
//! the `Remote` store mode by configuring it as an HTTP URL. Since object_store
//! HTTP requires a WebDAV server, we instead measure the same engine with
//! cache disabled to force tile reads through the DataStore on every query.
//!
//! Run with: cargo bench -p engine-geotiff --bench geotiff_remote

use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};

use ds_core::config::GeoTiffConfig;
use ds_core::engine::Engine;
use engine_geotiff::GeoTiffEngine;

fn make_config(cache_mb: u64) -> GeoTiffConfig {
    GeoTiffConfig {
        filename_template: Some("radar_%Y%m%dT%H%MZ.tif".to_string()),
        filename_pattern: None,
        timestamp_format: None,
        parameter: "reflectivity".to_string(),
        unit: "dBZ".to_string(),
        poll_interval_secs: 3600,
        tile_cache_mb: cache_mb,
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
    }
}

fn load_engine(cache_mb: u64) -> GeoTiffEngine {
    GeoTiffEngine::new(
        "radar",
        Some("../../testdata/radar"),
        &make_config(cache_mb),
    )
    .unwrap()
}

/// Benchmark position queries with varying cache states.
/// With cache=0, every query reads tiles from disk (simulating what remote would do).
/// With cache=64, subsequent queries hit the cache (no I/O).
/// The difference shows the I/O + decompress cost that async would overlap.
fn bench_position_cache_comparison(c: &mut Criterion) {
    let mut group = c.benchmark_group("position_query");
    group.sample_size(50);

    let coords = "POINT(25.0 60.5)";

    // No cache — every query reads from disk
    let engine_nocache = load_engine(0);
    group.bench_function("no_cache", |b| {
        b.iter(|| {
            black_box(
                engine_nocache
                    .query_position(black_box(coords), None, None)
                    .unwrap(),
            )
        })
    });

    // With cache, cold start
    let engine_cached = load_engine(64);
    group.bench_function("cache_cold", |b| {
        b.iter(|| {
            // New engine each iteration to avoid warm cache
            let engine = load_engine(64);
            black_box(
                engine
                    .query_position(black_box(coords), None, None)
                    .unwrap(),
            )
        })
    });

    // With cache, warm
    let _ = engine_cached.query_position(coords, None, None);
    group.bench_function("cache_warm", |b| {
        b.iter(|| {
            black_box(
                engine_cached
                    .query_position(black_box(coords), None, None)
                    .unwrap(),
            )
        })
    });

    group.finish();
}

/// Benchmark area queries at different sizes.
fn bench_area_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("area_query_scaling");
    group.sample_size(20);

    let engine = load_engine(0); // no cache — measure raw read cost

    let areas = [
        (
            "0.1x0.1_deg",
            "POLYGON((25.0 60.0, 25.1 60.0, 25.1 60.1, 25.0 60.1, 25.0 60.0))",
        ),
        (
            "0.5x0.5_deg",
            "POLYGON((24.5 60.0, 25.0 60.0, 25.0 60.5, 24.5 60.5, 24.5 60.0))",
        ),
        (
            "1x1_deg",
            "POLYGON((24.0 60.0, 25.0 60.0, 25.0 61.0, 24.0 61.0, 24.0 60.0))",
        ),
        (
            "2x2_deg",
            "POLYGON((24.0 59.0, 26.0 59.0, 26.0 61.0, 24.0 61.0, 24.0 59.0))",
        ),
        (
            "4x3_deg",
            "POLYGON((22.0 59.0, 26.0 59.0, 26.0 62.0, 22.0 62.0, 22.0 59.0))",
        ),
    ];

    for (label, coords) in &areas {
        group.bench_with_input(BenchmarkId::new("no_cache", label), coords, |b, coords| {
            b.iter(|| black_box(engine.query_area(black_box(coords), None, None).unwrap()))
        });
    }

    group.finish();
}

/// Benchmark concurrent position queries using std threads to simulate
/// what happens when multiple requests hit the server simultaneously.
/// This is where `block_in_place` thread starvation would show up.
fn bench_concurrent_position(c: &mut Criterion) {
    let mut group = c.benchmark_group("concurrent_position");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(15));

    let engine = std::sync::Arc::new(load_engine(64));
    let coords = "POINT(25.0 60.5)";

    // Warm cache
    let _ = engine.query_position(coords, None, None);

    for concurrency in [1, 2, 4, 8, 16] {
        group.bench_with_input(
            BenchmarkId::new("threads", concurrency),
            &concurrency,
            |b, &n| {
                b.iter(|| {
                    let handles: Vec<_> = (0..n)
                        .map(|_| {
                            let eng = engine.clone();
                            std::thread::spawn(move || {
                                black_box(
                                    eng.query_position(black_box("POINT(25.0 60.5)"), None, None)
                                        .unwrap(),
                                )
                            })
                        })
                        .collect();
                    for h in handles {
                        h.join().unwrap();
                    }
                })
            },
        );
    }

    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default();
    targets =
        bench_position_cache_comparison,
        bench_area_scaling,
        bench_concurrent_position,
}
criterion_main!(benches);
