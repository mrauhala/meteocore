//! Reproducible decoded-grid memory / cache / sampling benchmark.
//! Run with `cargo run --release -p engine-grib --example bench_grid_cache`.
//! Optional arguments: a single-message GRIB2 path and an iteration count.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ds_core::map_engine::OutputCrs;
use engine_grib::{cache::GridCache, reader::decode_message};

fn median(mut times: Vec<Duration>) -> Duration {
    times.sort_unstable();
    times[times.len() / 2]
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| {
        format!(
            "{}/../../testdata/grib-local/sample-message.grib2",
            env!("CARGO_MANIFEST_DIR")
        )
    });
    let iterations: usize = args.next().map(|s| s.parse()).transpose()?.unwrap_or(20);
    assert!(iterations > 0);
    let bytes = std::fs::read(path)?;
    let grid = decode_message(&bytes, "benchmark")?;
    println!("grid={}x{} cells={}", grid.ni, grid.nj, grid.values.len());
    println!(
        "value_buffer_bytes={}",
        std::mem::size_of_val(grid.values.as_slice())
    );

    let mut decode_times = Vec::with_capacity(iterations);
    let mut render_times = Vec::with_capacity(iterations);
    let bbox = [19.0, 59.0, 32.0, 71.0];
    for _ in 0..iterations {
        let start = Instant::now();
        black_box(decode_message(black_box(&bytes), "benchmark")?);
        decode_times.push(start.elapsed());
        let start = Instant::now();
        black_box(grid.resample(black_box(bbox), 512, 256, &OutputCrs::Wgs84));
        render_times.push(start.elapsed());
    }
    println!("decode_median_us={}", median(decode_times).as_micros());
    println!("resample_median_us={}", median(render_times).as_micros());

    // Replay twelve distinct message keys through the same fixed cache budget.
    // Input bytes are reused to make this independent of network conditions.
    let cache = GridCache::new(64).unwrap();
    let mut decodes = 0;
    for _ in 0..3 {
        for field in 0..12 {
            black_box(
                cache.get_or_insert_with(&format!("field-{field}.grib2"), 0, || {
                    decodes += 1;
                    decode_message(&bytes, "benchmark").map(Arc::new)
                })?,
            );
        }
    }
    println!(
        "replay_requests=36 replay_decodes={decodes} cached_grids={} cache_bytes={}",
        cache.len(),
        cache.weight()
    );

    // Compare this fingerprint across revisions: f64 query outputs, including
    // projected rendering, interpolation, and area axes/values, must agree.
    let mut hash = DefaultHasher::new();
    let mut hash_values = |values: Vec<Option<f64>>| {
        for value in values {
            value.map(f64::to_bits).hash(&mut hash);
        }
    };
    for output in [OutputCrs::Wgs84, OutputCrs::WebMercator] {
        hash_values(grid.resample(bbox, 512, 256, &output));
    }
    let crs = ds_core::geo::projected_output_crs("EPSG:3035").unwrap();
    let projected = ds_core::geo::projected_envelope(&crs, bbox);
    hash_values(grid.resample(
        bbox,
        512,
        256,
        &OutputCrs::Projected {
            crs,
            bbox: projected,
        },
    ));
    hash_values(
        (0..1000)
            .map(|i| grid.bilinear_value(19.0 + f64::from(i) * 0.01, 60.123))
            .collect(),
    );
    if let Some((x, y, values)) = grid.extract_bbox(bbox) {
        hash_values(values);
        x.into_iter()
            .map(f64::to_bits)
            .collect::<Vec<_>>()
            .hash(&mut hash);
        y.into_iter()
            .map(f64::to_bits)
            .collect::<Vec<_>>()
            .hash(&mut hash);
    }
    println!("query_output_hash={:016x}", hash.finish());
    Ok(())
}
