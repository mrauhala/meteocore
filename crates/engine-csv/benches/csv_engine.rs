use criterion::{black_box, criterion_group, criterion_main, Criterion};

use ds_core::engine::Engine;
use ds_core::feature::{Bbox, FeatureQuery};
use ds_core::feature_engine::FeatureEngine;
use engine_csv::engine::CsvEngine;
use engine_csv::loader::CsvDataStore;

fn load_engine() -> CsvEngine {
    CsvEngine::new(CsvDataStore::load("../../testdata/weather.csv").unwrap())
}

fn bench_get_locations(c: &mut Criterion) {
    let engine = load_engine();
    c.bench_function("csv_get_locations", |b| {
        b.iter(|| black_box(engine.get_locations().unwrap()))
    });
}

fn bench_query_location(c: &mut Criterion) {
    let engine = load_engine();
    let locations = engine.get_locations().unwrap();
    let loc_id = &locations[0].id;

    c.bench_function("csv_query_location_all_params", |b| {
        b.iter(|| {
            black_box(
                engine
                    .query_location(black_box(loc_id), None, None)
                    .unwrap(),
            )
        })
    });

    let params = vec!["temperature".to_string()];
    c.bench_function("csv_query_location_single_param", |b| {
        b.iter(|| {
            black_box(
                engine
                    .query_location(black_box(loc_id), None, Some(&params))
                    .unwrap(),
            )
        })
    });
}

fn bench_get_features(c: &mut Criterion) {
    let engine = load_engine();

    c.bench_function("csv_get_features_default", |b| {
        b.iter(|| black_box(engine.get_features(&FeatureQuery::default()).unwrap()))
    });

    let bbox = Bbox::new(24.8, 60.1, 25.1, 60.25).unwrap();
    let query = FeatureQuery {
        bbox: Some(bbox),
        ..Default::default()
    };
    c.bench_function("csv_get_features_bbox", |b| {
        b.iter(|| black_box(engine.get_features(black_box(&query)).unwrap()))
    });
}

fn bench_query_area(c: &mut Criterion) {
    let engine = load_engine();
    let coords = "POLYGON((24.0 60.0, 26.0 60.0, 26.0 61.0, 24.0 61.0, 24.0 60.0))";

    c.bench_function("csv_query_area", |b| {
        b.iter(|| black_box(engine.query_area(black_box(coords), None, None).unwrap()))
    });
}

criterion_group!(
    benches,
    bench_get_locations,
    bench_query_location,
    bench_get_features,
    bench_query_area,
);
criterion_main!(benches);
