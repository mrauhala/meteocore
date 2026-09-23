use super::*;
use std::io::Write;
use zarrs::{
    array::{codec::ShardingCodecBuilder, data_type, ArrayBuilder, ArrayShardedExt, ArraySubset},
    filesystem::FilesystemStore,
};

fn codecs() -> [BoundedCodec; 2] {
    [
        BoundedCodec {
            inner: Arc::new(GzipCodec::new(1).unwrap()),
            kind: Kind::Gzip,
        },
        BoundedCodec {
            inner: Arc::new(ZstdCodec::new(1, true)),
            kind: Kind::Zstd,
        },
    ]
}

#[test]
fn round_trips_enforce_fixed_and_bounded_lengths_including_empty_and_exact_eof() {
    let options = crate::catalog::single_threaded_opts();
    for codec in codecs() {
        for length in [0, 1, 3, 65_537] {
            let raw: Vec<_> = (0..length).map(|i| (i % 251) as u8).collect();
            let encoded = codec.encode(Cow::Borrowed(&raw), &options).unwrap();
            for representation in [
                BytesRepresentation::FixedSize(length as u64),
                BytesRepresentation::BoundedSize(length as u64),
                BytesRepresentation::BoundedSize(length as u64 + 1),
            ] {
                assert_eq!(
                    codec
                        .decode(encoded.clone(), &representation, &options)
                        .unwrap(),
                    raw
                );
            }
            assert!(codec
                .decode(
                    encoded.clone(),
                    &BytesRepresentation::FixedSize(length as u64 + 1),
                    &options
                )
                .is_err());
            if length > 0 {
                assert!(codec
                    .decode(
                        encoded.clone(),
                        &BytesRepresentation::BoundedSize(length as u64 - 1),
                        &options
                    )
                    .is_err());
            }
            assert!(codec
                .decode(encoded, &BytesRepresentation::UnboundedSize, &options)
                .is_err());
        }
    }
}

#[test]
fn malformed_frames_and_decompression_bombs_fail_with_a_small_destination() {
    let options = crate::catalog::single_threaded_opts();
    for codec in codecs() {
        let encoded = codec
            .encode(Cow::Owned(vec![42; 1024 * 1024]), &options)
            .unwrap();
        assert!(encoded.len() < 8192);
        assert!(codec
            .decode(encoded, &BytesRepresentation::FixedSize(4), &options)
            .is_err());
        let mut encoded = codec
            .encode(Cow::Owned(vec![1, 2, 3, 4]), &options)
            .unwrap()
            .into_owned();
        assert!(codec
            .decode(
                Cow::Borrowed(&encoded[..encoded.len() - 1]),
                &BytesRepresentation::FixedSize(4),
                &options
            )
            .is_err());
        let last = encoded.len() - 1;
        encoded[last] ^= 1;
        assert!(codec
            .decode(
                Cow::Borrowed(&encoded),
                &BytesRepresentation::FixedSize(4),
                &options
            )
            .is_err());
    }
}

#[test]
fn zstd_frames_without_content_size_and_concatenated_frames_stay_bounded() {
    let [_, codec] = codecs();
    let raw = vec![7; 65_537];
    let mut writer = zstd::stream::write::Encoder::new(Vec::new(), 1).unwrap();
    writer.include_contentsize(false).unwrap();
    writer.write_all(&raw).unwrap();
    let encoded = writer.finish().unwrap();
    assert_eq!(
        zstd::zstd_safe::get_frame_content_size(&encoded).unwrap(),
        None
    );
    let options = crate::catalog::single_threaded_opts();
    assert_eq!(
        codec
            .decode(
                Cow::Borrowed(&encoded),
                &BytesRepresentation::FixedSize(raw.len() as u64),
                &options
            )
            .unwrap(),
        raw
    );
    assert!(codec
        .decode(
            Cow::Borrowed(&encoded),
            &BytesRepresentation::FixedSize(4),
            &options
        )
        .is_err());
    let both = [encoded.as_slice(), encoded.as_slice()].concat();
    assert_eq!(
        codec
            .decode(
                Cow::Borrowed(&both),
                &BytesRepresentation::FixedSize(2 * raw.len() as u64),
                &options
            )
            .unwrap()
            .len(),
        2 * raw.len()
    );
    assert!(codec
        .decode(
            Cow::Borrowed(&both),
            &BytesRepresentation::FixedSize(raw.len() as u64),
            &options
        )
        .is_err());
}

#[test]
fn intermediate_admission_is_typed_and_lives_through_retrieval() {
    let options = crate::catalog::single_threaded_opts();
    for codec in codecs() {
        let encoded = codec
            .encode(Cow::Owned(vec![3; 100_000]), &options)
            .unwrap();
        let budget = Arc::new(crate::read_budget::Budget::new(1024 * 1024));
        {
            let _scope = crate::encoded::enter(Some(budget.clone()));
            let decoded = codec
                .decode(
                    encoded.clone(),
                    &BytesRepresentation::BoundedSize(200_000),
                    &options,
                )
                .unwrap();
            assert!(budget.metrics().0 >= 200_000);
            drop(decoded);
            assert!(
                budget.metrics().0 >= 200_000,
                "scope owns codec copy allowances"
            );
        }
        assert_eq!(budget.metrics().0, 0);
        let budget = Arc::new(crate::read_budget::Budget::new(0));
        {
            let _scope = crate::encoded::enter(Some(budget.clone()));
            let error = codec
                .decode(
                    encoded,
                    &BytesRepresentation::BoundedSize(200_000),
                    &options,
                )
                .unwrap_err();
            assert!(matches!(
                crate::catalog::chunk_read_error(error.into()),
                DataServerError::ResourceExhausted
            ));
        }
        assert_eq!(budget.metrics().0, 0);
    }
}

#[test]
fn native_outputs_use_the_existing_workspace_and_expired_decodes_stay_typed() {
    let options = crate::catalog::single_threaded_opts();
    for codec in codecs() {
        let encoded = codec.encode(Cow::Owned(vec![3; 100]), &options).unwrap();
        let budget = Arc::new(crate::read_budget::Budget::new(0));
        let _scope = crate::encoded::enter(Some(budget.clone()));
        assert!(codec
            .decode(
                encoded.clone(),
                &BytesRepresentation::FixedSize(100),
                &options
            )
            .is_ok());
        assert_eq!(budget.metrics().0, 0);
        let _deadline = deadline::enter(Some(std::time::Instant::now()));
        let error = codec
            .decode(encoded, &BytesRepresentation::FixedSize(100), &options)
            .unwrap_err();
        assert!(matches!(
            crate::catalog::chunk_read_error(error.into()),
            DataServerError::DeadlineExceeded
        ));
    }
}

#[test]
fn shard_round_trips_preserve_inner_reads_fill_and_metadata_with_outer_compression() {
    let options = crate::catalog::single_threaded_opts();
    for codec in codecs() {
        for outer in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let storage = Arc::new(FilesystemStore::new(dir.path()).unwrap());
            let mut builder =
                ArrayBuilder::new(vec![8, 8], vec![4, 4], data_type::float32(), -999f32);
            builder.dimension_names(Some(["lat", "lon"]));
            builder.attributes(
                serde_json::json!({"units":"K"})
                    .as_object()
                    .unwrap()
                    .clone(),
            );
            let shard =
                ShardingCodecBuilder::new(vec![2.try_into().unwrap(); 2], &data_type::float32())
                    .bytes_to_bytes_codecs(vec![codec.inner.clone()])
                    .build();
            builder.array_to_bytes_codec(Arc::new(shard));
            if outer {
                builder.bytes_to_bytes_codecs(vec![codec.inner.clone()]);
            }
            let writer = builder.build(storage.clone(), "/data").unwrap();
            writer.store_metadata().unwrap();
            let values: Vec<f32> = (0..16).map(|n| n as f32).collect();
            writer.store_chunk_opt(&[0, 0], values, &options).unwrap();
            let metadata = std::fs::read(dir.path().join("data/zarr.json")).unwrap();
            let array = Array::open(Arc::new(EngineStore::new(storage)), "/data").unwrap();
            let expected_codecs = array
                .codecs()
                .create_metadatas(&CodecMetadataOptions::default());
            let array = bounded_array(array).unwrap();
            assert!(array.is_sharded());
            assert_eq!(array.is_exclusively_sharded(), !outer);
            assert_eq!(
                array
                    .codecs()
                    .create_metadatas(&CodecMetadataOptions::default()),
                expected_codecs
            );
            assert_eq!(array.attributes()["units"], "K");
            assert_eq!(
                std::fs::read(dir.path().join("data/zarr.json")).unwrap(),
                metadata
            );
            for subset in [
                ArraySubset::new_with_ranges(&[1..3, 1..3]),
                array.subset_all(),
            ] {
                let expected: Vec<f32> =
                    writer.retrieve_array_subset_opt(&subset, &options).unwrap();
                let scope = crate::encoded::enter(Some(Arc::new(crate::read_budget::Budget::new(
                    1024 * 1024,
                ))));
                let actual: Vec<f32> = array.retrieve_array_subset_opt(&subset, &options).unwrap();
                drop(scope);
                assert_eq!(actual, expected);
            }
        }
    }
}

#[test]
fn independent_v2_compressed_fortran_arrays_keep_layout_and_chunk_keys() {
    let options = crate::catalog::single_threaded_opts();
    for codec in codecs() {
        let dir = tempfile::tempdir().unwrap();
        let compressor = match codec.kind {
            Kind::Gzip => serde_json::json!({"id":"gzip", "level":1}),
            Kind::Zstd => serde_json::json!({"id":"zstd", "level":1, "checksum":true}),
        };
        let metadata = serde_json::to_vec(&serde_json::json!({
            "zarr_format":2, "shape":[2,3], "chunks":[2,3],
            "dtype":"<f4", "compressor":compressor, "fill_value":-999,
            "order":"F", "filters":null, "dimension_separator":"."
        }))
        .unwrap();
        std::fs::write(dir.path().join(".zarray"), &metadata).unwrap();
        std::fs::write(
            dir.path().join(".zattrs"),
            br#"{"_ARRAY_DIMENSIONS":["lat","lon"],"units":"K"}"#,
        )
        .unwrap();
        let raw = [0f32, 3., 1., 4., 2., 5.]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect::<Vec<_>>();
        let encoded = match codec.kind {
            Kind::Gzip => {
                let mut writer =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
                writer.write_all(&raw).unwrap();
                writer.finish().unwrap()
            }
            Kind::Zstd => zstd::bulk::compress(&raw, 1).unwrap(),
        };
        std::fs::write(dir.path().join("0.0"), encoded).unwrap();
        let storage = Arc::new(EngineStore::new(Arc::new(
            FilesystemStore::new(dir.path()).unwrap(),
        )));
        let array = bounded_array(Array::open(storage, "/").unwrap()).unwrap();
        let values: Vec<f32> = array
            .retrieve_array_subset_opt(&array.subset_all(), &options)
            .unwrap();
        assert_eq!(values, [0., 1., 2., 3., 4., 5.]);
        let values: Vec<f32> = array
            .retrieve_array_subset_opt(&ArraySubset::new_with_ranges(&[0..2, 1..3]), &options)
            .unwrap();
        assert_eq!(values, [1., 2., 4., 5.]);
        assert_eq!(array.attributes()["units"], "K");
        assert_eq!(std::fs::read(dir.path().join(".zarray")).unwrap(), metadata);
        assert!(!dir.path().join("zarr.json").exists());
    }
}

#[test]
fn stacked_compression_in_nested_shards_remains_bounded() {
    let options = crate::catalog::single_threaded_opts();
    let dir = tempfile::tempdir().unwrap();
    let storage = Arc::new(FilesystemStore::new(dir.path()).unwrap());
    let [gzip, zstd] = codecs();
    let inner = ShardingCodecBuilder::new(vec![2.try_into().unwrap(); 2], &data_type::float32())
        .bytes_to_bytes_codecs(vec![gzip.inner, zstd.inner])
        .build();
    let outer = ShardingCodecBuilder::new(vec![4.try_into().unwrap(); 2], &data_type::float32())
        .array_to_bytes_codec(Arc::new(inner))
        .build();
    let writer = ArrayBuilder::new(vec![8, 8], vec![8, 8], data_type::float32(), -999f32)
        .array_to_bytes_codec(Arc::new(outer))
        .build(storage.clone(), "/")
        .unwrap();
    writer.store_metadata().unwrap();
    writer
        .store_chunk_opt(
            &[0, 0],
            (0..64).map(|n| n as f32).collect::<Vec<_>>(),
            &options,
        )
        .unwrap();
    let array =
        bounded_array(Array::open(Arc::new(EngineStore::new(storage)), "/").unwrap()).unwrap();
    let budget = Arc::new(crate::read_budget::Budget::new(1024 * 1024));
    for subset in [
        ArraySubset::new_with_ranges(&[1..7, 1..7]),
        array.subset_all(),
    ] {
        let expected: Vec<f32> = writer.retrieve_array_subset_opt(&subset, &options).unwrap();
        let scope = crate::encoded::enter(Some(budget.clone()));
        let actual: Vec<f32> = array.retrieve_array_subset_opt(&subset, &options).unwrap();
        assert_eq!(actual, expected);
        assert!(
            budget.metrics().0 > 0,
            "stacked compression admits intermediate buffers"
        );
        drop(scope);
        assert_eq!(budget.metrics().0, 0);
    }
}
