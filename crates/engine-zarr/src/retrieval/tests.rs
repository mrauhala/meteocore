use super::*;
use crate::{codec_limits::bounded_array, read_budget::Budget};
use std::sync::Arc;
use zarrs::{
    array::{
        codec::{GzipCodec, ShardingCodecBuilder, ZstdCodec},
        data_type, ArrayBuilder, FromArrayBytes,
    },
    filesystem::FilesystemStore,
};

fn fixture(dir: &std::path::Path, layout: &str) -> Array<EngineStore> {
    let mut builder = ArrayBuilder::new(vec![9, 9], vec![4, 4], data_type::float32(), -999.0f32);
    let gzip = Arc::new(GzipCodec::new(1).unwrap());
    if layout.contains("shard") {
        let mut shard =
            ShardingCodecBuilder::new(vec![2.try_into().unwrap(); 2], &data_type::float32());
        if layout.contains("nested") {
            shard.array_to_bytes_codec(Arc::new(
                ShardingCodecBuilder::new(vec![1.try_into().unwrap(); 2], &data_type::float32())
                    .bytes_to_bytes_codecs(vec![gzip.clone()])
                    .build(),
            ));
        } else {
            shard.bytes_to_bytes_codecs(vec![gzip.clone()]);
        }
        builder.array_to_bytes_codec(Arc::new(shard.build()));
        if layout.contains("outer") {
            builder.bytes_to_bytes_codecs(vec![gzip]);
        }
    } else if layout == "stacked" {
        builder.bytes_to_bytes_codecs(vec![gzip, Arc::new(ZstdCodec::new(1, true))]);
    } else {
        builder.bytes_to_bytes_codecs(vec![gzip]);
    }
    let writer = builder
        .build(Arc::new(FilesystemStore::new(dir).unwrap()), "/a")
        .unwrap();
    writer.store_metadata().unwrap();
    let options = crate::catalog::single_threaded_opts();
    for y in 0..3 {
        for x in 0..3 {
            if [y, x] != [2, 2] {
                writer
                    .store_chunk_opt(
                        &[y, x],
                        (0..16)
                            .map(|n| (y * 100 + x * 16 + n) as f32)
                            .collect::<Vec<_>>(),
                        &options,
                    )
                    .unwrap();
            }
        }
    }
    open(dir)
}

fn open(dir: &std::path::Path) -> Array<EngineStore> {
    let config = ds_core::config::ZarrConfig::auto_local(dir.to_string_lossy().into_owned());
    bounded_array(
        Array::open(
            Arc::new(EngineStore::plain(
                crate::build_store("scope", &config).unwrap(),
            )),
            "/a",
        )
        .unwrap(),
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serial_reads_reuse_one_chunks_encoded_and_intermediate_allowance() {
    let options = crate::catalog::single_threaded_opts();
    for layout in [
        "plain",
        "stacked",
        "shard",
        "outer-shard",
        "outer-nested-shard",
    ] {
        for ranges in [
            [0..9, 0..9],
            [1..7, 1..7],
            [3..9, 3..9],
            [0..4, 0..4],
            [8..9, 8..9],
        ] {
            let dir = tempfile::tempdir().unwrap();
            let array = fixture(dir.path(), layout);
            let subset = ArraySubset::new_with_ranges(&ranges);
            let expected: Vec<f32> = array.retrieve_array_subset_opt(&subset, &options).unwrap();
            // Measure each complete retrieval's retained temporary allowance.
            // No Blosc scratch in this fixture: encoded/intermediate charges
            // are monotonic within the chunk, so this is also their peak.
            let probe = Arc::new(Budget::new(ds_cache::MIB));
            let source = probe.reserve(&array, &subset, Some(0), false).unwrap();
            let baseline = probe.metrics().0;
            let mut maximum = 0;
            let mut total = 0;
            for indices in array
                .chunks_in_array_subset(&subset)
                .unwrap()
                .unwrap()
                .indices()
            {
                let overlap = array
                    .chunk_subset(&indices)
                    .unwrap()
                    .overlap(&subset)
                    .unwrap();
                let scope = encoded::enter(Some(probe.clone()));
                let _: ArrayBytes = array.retrieve_array_subset_opt(&overlap, &options).unwrap();
                let temporary = probe.metrics().0 - baseline;
                maximum = maximum.max(temporary);
                total += temporary;
                drop(scope);
                assert_eq!(probe.metrics().0, baseline);
            }
            drop(source);
            // A fresh adapter ensures the first scoped read is cold.
            let array = open(dir.path());
            let budget = Arc::new(Budget::new(baseline + maximum));
            let source = budget.reserve(&array, &subset, Some(0), false).unwrap();
            let scope = encoded::enter(Some(budget.clone()));
            for cached in [false, true] {
                if cached {
                    // Metadata and every payload have already entered the real
                    // plain-store cache. Missing chunks retain fill semantics.
                    std::fs::remove_dir_all(dir.path().join("a")).unwrap();
                }
                let bytes = serial(&array, &subset, &options).unwrap();
                assert_eq!(budget.metrics(), (baseline, baseline + maximum, 0));
                assert_eq!(
                    Vec::<f32>::from_array_bytes(bytes, subset.shape(), array.data_type()).unwrap(),
                    expected,
                    "{layout}: {ranges:?}, cached={cached}"
                );
            }
            if total > maximum {
                // The previous whole-window scope exhausted the same budget.
                assert!(matches!(
                    array
                        .retrieve_array_subset_opt::<ArrayBytes>(&subset, &options)
                        .map_err(chunk_read_error),
                    Err(DataServerError::ResourceExhausted)
                ));
            }
            drop(scope);
            assert_eq!(budget.metrics().0, baseline);
            drop(source);
            assert_eq!(budget.metrics().0, 0);
            if ranges == [0..9, 0..9] {
                eprintln!("{layout}: source/workspace={baseline}, per-chunk peak={maximum}, previous retained total={total}");
            }
        }
    }
}

#[path = "lifetime_tests.rs"]
mod lifetimes;
