use criterion::{black_box, criterion_group, criterion_main, Criterion};

use ds_core::feature::{Bbox, FeatureQuery};
use ds_core::feature_engine::FeatureEngine;
use engine_geojson::GeoJsonEngine;

fn bench_load(c: &mut Criterion) {
    c.bench_function("geojson_load_municipalities", |b| {
        b.iter(|| black_box(GeoJsonEngine::load("../../testdata/municipalities.geojson").unwrap()))
    });
}

fn bench_get_features(c: &mut Criterion) {
    let engine = GeoJsonEngine::load("../../testdata/municipalities.geojson").unwrap();

    c.bench_function("geojson_get_features_no_filter", |b| {
        let query = FeatureQuery {
            limit: 1000,
            ..Default::default()
        };
        b.iter(|| black_box(engine.get_features(black_box(&query)).unwrap()))
    });

    // Bbox covering southern Finland
    let bbox = Bbox::new(24.0, 60.0, 26.0, 61.0).unwrap();
    c.bench_function("geojson_get_features_bbox", |b| {
        let query = FeatureQuery {
            bbox: Some(bbox),
            limit: 1000,
            ..Default::default()
        };
        b.iter(|| black_box(engine.get_features(black_box(&query)).unwrap()))
    });

    // Small bbox — few results
    let small_bbox = Bbox::new(24.9, 60.15, 25.0, 60.2).unwrap();
    c.bench_function("geojson_get_features_small_bbox", |b| {
        let query = FeatureQuery {
            bbox: Some(small_bbox),
            limit: 1000,
            ..Default::default()
        };
        b.iter(|| black_box(engine.get_features(black_box(&query)).unwrap()))
    });
}

fn bench_get_feature(c: &mut Criterion) {
    let engine = GeoJsonEngine::load("../../testdata/municipalities.geojson").unwrap();
    // Get the first feature ID
    let page = engine
        .get_features(&FeatureQuery {
            limit: 1,
            ..Default::default()
        })
        .unwrap();
    let id = &page.features[0].id;

    c.bench_function("geojson_get_feature_by_id", |b| {
        b.iter(|| black_box(engine.get_feature(black_box(id)).unwrap()))
    });
}

fn bench_spatial_extent(c: &mut Criterion) {
    let engine = GeoJsonEngine::load("../../testdata/municipalities.geojson").unwrap();
    c.bench_function("geojson_spatial_extent", |b| {
        b.iter(|| black_box(engine.spatial_extent()))
    });
}

fn bench_pagination(c: &mut Criterion) {
    let engine = GeoJsonEngine::load("../../testdata/municipalities.geojson").unwrap();
    let total = engine.feature_count();

    // First page
    c.bench_function("geojson_pagination_first_page", |b| {
        let query = FeatureQuery {
            limit: 100,
            offset: 0,
            ..Default::default()
        };
        b.iter(|| black_box(engine.get_features(black_box(&query)).unwrap()))
    });

    // Middle page
    c.bench_function("geojson_pagination_middle_page", |b| {
        let query = FeatureQuery {
            limit: 100,
            offset: total / 2,
            ..Default::default()
        };
        b.iter(|| black_box(engine.get_features(black_box(&query)).unwrap()))
    });
}

criterion_group!(
    benches,
    bench_load,
    bench_get_features,
    bench_get_feature,
    bench_spatial_extent,
    bench_pagination,
);
criterion_main!(benches);
