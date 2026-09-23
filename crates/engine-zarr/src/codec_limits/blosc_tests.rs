use super::*;
use zarrs::{
    array::codec::bytes_to_bytes::blosc::{BloscCompressor, BloscShuffleMode},
    storage::byte_range::ByteRange,
};

#[cfg(feature = "icechunk")]
#[path = "blosc_async_tests.rs"]
mod asynchronous;

pub(super) fn codec() -> BoundedCodec {
    configured(BloscCompressor::LZ4, BloscShuffleMode::Shuffle)
}

fn configured(compressor: BloscCompressor, shuffle: BloscShuffleMode) -> BoundedCodec {
    BoundedCodec {
        inner: Arc::new(
            BloscCodec::new(
                compressor,
                1.try_into().unwrap(),
                Some(64),
                shuffle,
                Some(4),
            )
            .unwrap(),
        ),
        kind: Kind::Blosc,
    }
}

#[test]
fn blosc_full_and_partial_reads_preserve_compressors_and_shuffle_modes() {
    let options = crate::catalog::single_threaded_opts();
    let raw: Vec<u8> = (0..1024u32).flat_map(u32::to_le_bytes).collect();
    for compressor in [
        BloscCompressor::BloscLZ,
        BloscCompressor::LZ4,
        BloscCompressor::Zlib,
        BloscCompressor::Zstd,
    ] {
        for shuffle in [
            BloscShuffleMode::NoShuffle,
            BloscShuffleMode::Shuffle,
            BloscShuffleMode::BitShuffle,
        ] {
            let codec = Arc::new(configured(compressor, shuffle));
            let encoded = codec
                .encode(Cow::Borrowed(&raw), &options)
                .unwrap()
                .into_owned();
            let repr = BytesRepresentation::FixedSize(raw.len() as u64);
            assert_eq!(
                codec
                    .decode(Cow::Borrowed(&encoded), &repr, &options)
                    .unwrap(),
                raw
            );
            let partial = codec
                .partial_decoder(Arc::new(Cow::Owned(encoded)), &repr, &options)
                .unwrap();
            assert!(partial.supports_partial_decode(), "retain Blosc getitem");
            let values = partial
                .partial_decode_many(
                    Box::new(
                        [
                            ByteRange::FromStart(4, Some(12)),
                            ByteRange::FromStart(60, Some(20)),
                            ByteRange::Suffix(8),
                        ]
                        .into_iter(),
                    ),
                    &options,
                )
                .unwrap()
                .unwrap();
            assert_eq!(
                values,
                [
                    Cow::Borrowed(&raw[4..16]),
                    Cow::Borrowed(&raw[60..80]),
                    Cow::Borrowed(&raw[raw.len() - 8..])
                ]
            );
        }
    }
}

#[test]
fn blosc_frame_size_mismatches_fail_before_full_or_partial_decoding() {
    let options = crate::catalog::single_threaded_opts();
    let codec = Arc::new(codec());
    let encoded = codec
        .encode(Cow::Owned(vec![0; 1024 * 1024]), &options)
        .unwrap()
        .into_owned();
    for repr in [
        BytesRepresentation::FixedSize(16),
        BytesRepresentation::BoundedSize(16),
        BytesRepresentation::UnboundedSize,
    ] {
        // Zero capacity also proves invalid frame/representation pairs fail
        // before admission and any output or native scratch allocation.
        let budget = Arc::new(crate::read_budget::Budget::new(0));
        let _scope = crate::encoded::enter(Some(budget.clone()));
        let error = codec
            .decode(Cow::Borrowed(&encoded), &repr, &options)
            .unwrap_err();
        assert!(matches!(
            crate::catalog::chunk_read_error(error.into()),
            DataServerError::Engine(_)
        ));
        let partial = codec
            .clone()
            .partial_decoder(Arc::new(Cow::Owned(encoded.clone())), &repr, &options)
            .unwrap();
        let error = partial
            .partial_decode(ByteRange::FromStart(0, Some(4)), &options)
            .unwrap_err();
        assert!(matches!(
            crate::catalog::chunk_read_error(error.into()),
            DataServerError::Engine(_)
        ));
        assert_eq!(budget.metrics(), (0, 0, 0));
    }
    let small = codec.encode(Cow::Owned(vec![0; 16]), &options).unwrap();
    assert!(codec
        .decode(small, &BytesRepresentation::FixedSize(32), &options)
        .is_err());
}

#[test]
fn blosc_rejects_malformed_headers_without_reserving_scratch() {
    let options = crate::catalog::single_threaded_opts();
    let codec = Arc::new(codec());
    let good = codec
        .encode(Cow::Owned(vec![0; 1024]), &options)
        .unwrap()
        .into_owned();
    let mut frames = vec![vec![], good[..15].to_vec(), good[..good.len() - 1].to_vec()];
    for block in [0u32, 2048, u32::MAX] {
        let mut bad = good.clone();
        bad[8..12].copy_from_slice(&block.to_le_bytes());
        frames.push(bad);
    }
    for (offset, value) in [(0, 0), (3, 0)] {
        let mut bad = good.clone();
        bad[offset] = value;
        frames.push(bad);
    }
    for frame in frames {
        let budget = Arc::new(crate::read_budget::Budget::new(0));
        let _scope = crate::encoded::enter(Some(budget.clone()));
        let repr = BytesRepresentation::FixedSize(1024);
        assert!(codec
            .decode(Cow::Borrowed(&frame), &repr, &options)
            .is_err());
        let partial = codec
            .clone()
            .partial_decoder(Arc::new(Cow::Owned(frame)), &repr, &options)
            .unwrap();
        assert!(partial
            .partial_decode(ByteRange::FromStart(0, Some(4)), &options)
            .is_err());
        assert_eq!(budget.metrics(), (0, 0, 0));
    }
}

#[test]
fn blosc_scratch_and_intermediate_admission_is_typed_and_released() {
    let options = crate::catalog::single_threaded_opts();
    let codec = Arc::new(codec());
    let raw = vec![0; 1024];
    let encoded = codec
        .encode(Cow::Borrowed(&raw), &options)
        .unwrap()
        .into_owned();
    for partial in [false, true] {
        for repr in [
            BytesRepresentation::FixedSize(1024),
            BytesRepresentation::BoundedSize(2048),
        ] {
            let budget = Arc::new(crate::read_budget::Budget::new(8192));
            let decode = || {
                if partial {
                    codec
                        .clone()
                        .partial_decoder(Arc::new(Cow::Owned(encoded.clone())), &repr, &options)?
                        .partial_decode(ByteRange::FromStart(0, Some(4)), &options)
                        .map(|v| v.unwrap().into_owned())
                } else {
                    codec
                        .decode(Cow::Borrowed(&encoded), &repr, &options)
                        .map(Cow::into_owned)
                }
            };
            {
                let _scope = crate::encoded::enter(Some(budget.clone()));
                let output = decode().unwrap();
                assert_eq!(output, raw[..if partial { 4 } else { raw.len() }]);
                let retained = if matches!(repr, BytesRepresentation::BoundedSize(_)) {
                    2048
                } else {
                    0
                };
                assert_eq!(budget.metrics().0, retained, "native scratch has ended");
                drop(output);
                assert_eq!(
                    budget.metrics().0,
                    retained,
                    "intermediate copies still live through retrieval"
                );
            }
            assert_eq!(budget.metrics().0, 0);
            let budget = Arc::new(crate::read_budget::Budget::new(0));
            let _scope = crate::encoded::enter(Some(budget.clone()));
            let error = decode().unwrap_err();
            assert!(matches!(
                crate::catalog::chunk_read_error(error.into()),
                DataServerError::ResourceExhausted
            ));
            assert_eq!(budget.metrics().0, 0);
            let _deadline = deadline::enter(Some(std::time::Instant::now()));
            let error = decode().unwrap_err();
            assert!(matches!(
                crate::catalog::chunk_read_error(error.into()),
                DataServerError::DeadlineExceeded
            ));
        }
    }
}

#[test]
fn sequential_blosc_calls_reuse_scratch_while_encoded_buffers_remain_admitted() {
    let options = crate::catalog::single_threaded_opts();
    let codec = Arc::new(codec());
    let raw = vec![0; 1024];
    let encoded = codec.encode(Cow::Borrowed(&raw), &options).unwrap();
    let block = u32::from_le_bytes(encoded[8..12].try_into().unwrap()) as u64;
    let typesize = u64::from(encoded[3]);
    let repr = BytesRepresentation::FixedSize(1024);
    let decoder = codec
        .clone()
        .partial_decoder(Arc::new(Cow::Owned(encoded.to_vec())), &repr, &options)
        .unwrap();
    for partial in [false, true] {
        let scratch = block * if partial { 3 } else { 2 } + 4 * typesize;
        let budget = Arc::new(crate::read_budget::Budget::new(14 + scratch));
        let scope = crate::encoded::enter(Some(budget.clone()));
        crate::encoded::current()
            .unwrap()
            .object("payload", 7)
            .unwrap();
        for _ in 0..8 {
            let output = if partial {
                let regions = std::iter::once_with(|| {
                    assert_eq!(
                        budget.metrics().0,
                        14 + scratch,
                        "getitem is still admitted"
                    );
                    ByteRange::FromStart(60, Some(12))
                });
                decoder
                    .partial_decode_many(Box::new(regions), &options)
                    .unwrap()
                    .unwrap()
                    .remove(0)
            } else {
                codec
                    .decode(Cow::Borrowed(&encoded), &repr, &options)
                    .unwrap()
            };
            assert_eq!(output.as_ref(), &raw[..if partial { 12 } else { 1024 }]);
            assert_eq!(budget.metrics(), (14, 14 + scratch, 0));
        }
        drop(scope);
        assert_eq!(budget.metrics().0, 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plain_multichunk_reads_fit_one_scratch_allowance_with_cold_and_cached_payloads() {
    use zarrs::{
        array::{data_type, ArrayBuilder, FromArrayBytes},
        filesystem::FilesystemStore,
    };

    let dir = tempfile::tempdir().unwrap();
    let options = crate::catalog::single_threaded_opts();
    let writer = ArrayBuilder::new(vec![8, 8], vec![4, 4], data_type::float32(), -999.0f32)
        .bytes_to_bytes_codecs(vec![codec().inner])
        .build(Arc::new(FilesystemStore::new(dir.path()).unwrap()), "/a")
        .unwrap();
    writer.store_metadata().unwrap();
    let subset = writer.subset_all();
    let expected: Vec<f32> = (0..64).map(|n| n as f32).collect();
    writer
        .store_array_subset_opt(&subset, expected.clone(), &options)
        .unwrap();
    let mut encoded_bytes = 0;
    let mut scratch = 0;
    let mut paths = Vec::new();
    for indices in [[0, 0], [0, 1], [1, 0], [1, 1]] {
        let path = dir.path().join(writer.chunk_key(&indices).as_str());
        let bytes = std::fs::read(&path).unwrap();
        encoded_bytes = encoded_bytes.max(2 * bytes.len() as u64);
        scratch = scratch.max(
            3 * u64::from(u32::from_le_bytes(bytes[8..12].try_into().unwrap()))
                + 4 * u64::from(bytes[3]),
        );
        paths.push(path);
    }
    let config = ds_core::config::ZarrConfig::auto_local(dir.path().to_string_lossy().into_owned());
    let store = Arc::new(EngineStore::plain(
        crate::build_store("scratch", &config).unwrap(),
    ));
    let array = bounded_array(Array::open(store, "/a").unwrap()).unwrap();
    // 64 source values * 24 bytes, plus four 64-byte native buffers. Temporary
    // capacity fits one chunk's encoded copies and one native scratch buffer.
    let baseline = 1536 + 256;
    let budget = Arc::new(crate::read_budget::Budget::new(
        baseline + encoded_bytes + scratch,
    ));
    let source = budget.reserve(&array, &subset, Some(0), false).unwrap();
    assert_eq!(budget.metrics().0, baseline);
    for cached in [false, true] {
        if cached {
            for path in &paths {
                std::fs::remove_file(path).unwrap();
            }
        }
        let scope = crate::encoded::enter(Some(budget.clone()));
        let bytes = crate::retrieval::serial(&array, &subset, &options).unwrap();
        let actual =
            Vec::<f32>::from_array_bytes(bytes, subset.shape(), array.data_type()).unwrap();
        assert_eq!(actual, expected);
        assert_eq!(budget.metrics().0, baseline);
        assert_eq!(budget.metrics().2, 0);
        drop(scope);
        assert_eq!(budget.metrics().0, baseline);
    }
    drop(source);
    assert_eq!(budget.metrics().0, 0);
}

#[test]
fn partial_scratch_releases_on_invalid_ranges_and_unwinding() {
    let options = crate::catalog::single_threaded_opts();
    let codec = Arc::new(codec());
    let encoded = codec
        .encode(Cow::Owned(vec![0; 1024]), &options)
        .unwrap()
        .into_owned();
    let scratch = 3 * u64::from(u32::from_le_bytes(encoded[8..12].try_into().unwrap()))
        + 4 * u64::from(encoded[3]);
    let decoder = codec
        .partial_decoder(
            Arc::new(Cow::Owned(encoded)),
            &BytesRepresentation::FixedSize(1024),
            &options,
        )
        .unwrap();
    let budget = Arc::new(crate::read_budget::Budget::new(scratch));
    let _scope = crate::encoded::enter(Some(budget.clone()));
    for bad in [
        ByteRange::FromStart(1025, None),
        ByteRange::FromStart(1020, Some(8)),
        ByteRange::FromStart(u64::MAX, Some(1)),
        ByteRange::Suffix(1025),
    ] {
        let regions = [ByteRange::FromStart(0, Some(4)), bad];
        let result = decoder.partial_decode_many(Box::new(regions.into_iter()), &options);
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("outside the frame"));
        assert_eq!(budget.metrics(), (0, scratch, 0));
    }
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let regions = std::iter::once_with(|| {
            assert_eq!(
                budget.metrics().0,
                scratch,
                "scratch covers the upstream decoder call"
            );
            panic!("upstream iterator panic");
        });
        decoder.partial_decode_many(Box::new(regions), &options)
    }));
    assert!(unwound.is_err());
    assert_eq!(budget.metrics(), (0, scratch, 0));
    assert_eq!(
        decoder
            .partial_decode(ByteRange::Suffix(4), &options)
            .unwrap()
            .unwrap()
            .as_ref(),
        &[0; 4]
    );
    assert_eq!(budget.metrics(), (0, scratch, 0));
}

#[cfg(feature = "icechunk")]
#[tokio::test]
async fn blosc_async_partial_reads_preserve_values_and_bounds() {
    let options = crate::catalog::single_threaded_opts();
    let codec = Arc::new(codec());
    let raw: Vec<_> = (0..128u32).flat_map(u32::to_le_bytes).collect();
    let encoded = codec
        .encode(Cow::Borrowed(&raw), &options)
        .unwrap()
        .into_owned();
    for (size, valid) in [(512, true), (4, false)] {
        let partial = codec
            .clone()
            .async_partial_decoder(
                Arc::new(Cow::Owned(encoded.clone())),
                &BytesRepresentation::FixedSize(size),
                &options,
            )
            .await
            .unwrap();
        assert!(partial.supports_partial_decode());
        let budget = Arc::new(crate::read_budget::Budget::new(8192));
        let scope = crate::encoded::enter(Some(budget.clone()));
        let result = partial
            .partial_decode(ByteRange::FromStart(60, Some(12)), &options)
            .await;
        if valid {
            assert_eq!(result.unwrap().unwrap().as_ref(), &raw[60..72]);
            assert_eq!(budget.metrics().0, 0, "getitem scratch is released");
        } else {
            assert!(matches!(
                crate::catalog::chunk_read_error(result.unwrap_err().into()),
                DataServerError::Engine(_)
            ));
            assert_eq!(budget.metrics().0, 0);
        }
        drop(scope);
        assert_eq!(budget.metrics().0, 0);
    }
}
