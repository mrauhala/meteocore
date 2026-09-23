use super::*;
use zarrs::{
    array::codec::bytes_to_bytes::blosc::{BloscCompressor, BloscShuffleMode},
    storage::byte_range::ByteRange,
};

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
                assert!(budget.metrics().0 > 0);
                drop(output);
                assert!(budget.metrics().0 > 0, "allowances live through retrieval");
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
            assert!(budget.metrics().0 > 0);
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
