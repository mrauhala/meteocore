use super::*;
use zarrs::array::{codec::ShardingCodecBuilder, data_type, ArrayBuilder};
use zarrs::filesystem::FilesystemStore;

#[test]
fn gzip_shard_estimate_covers_one_inner_payload_and_index_without_io() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(EngineStore::new(Arc::new(
        FilesystemStore::new(dir.path()).unwrap(),
    )));
    let array = ArrayBuilder::new(vec![1, 1], vec![8, 8], data_type::float32(), 0f32)
        .array_to_bytes_codec(Arc::new(
            ShardingCodecBuilder::new(vec![2.try_into().unwrap(); 2], &data_type::float32())
                .bytes_to_bytes_codecs(vec![Arc::new(GzipCodec::new(1).unwrap())])
                .build(),
        ))
        .build(store, "/a")
        .unwrap();
    // Stored padding counts: 16 native bytes in an inner chunk, 256 index
    // bytes plus its crc32c trailer. Gzip's declared encoder bound is 42.
    let plan = Plan::new(&array);
    assert_eq!(
        plan.bytes(&array, &array.subset_all()),
        Some(2 * (42 + 260))
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn stacked_codecs_include_intermediates_and_blosc_scratch() {
    let codecs: Vec<Arc<dyn BytesToBytesCodecTraits>> = vec![
        Arc::new(GzipCodec::new(1).unwrap()),
        super::super::blosc_tests::codec().inner,
        Arc::new(ZstdCodec::new(1, false)),
    ];
    let chain = CodecChain::new(vec![], Arc::new(BytesCodec::default()), codecs);
    let estimate = Chain::new(&chain).unwrap().bytes(192).unwrap();
    // Gzip input is fixed (native workspace). Its bounded 242-byte output
    // is Blosc's input; Blosc adds a 16-byte header before the zstd stage.
    let gzip = 242u64;
    let blosc = gzip + 16;
    let zstd = blosc + 22 + 3;
    assert_eq!(
        estimate,
        2 * gzip + 3 * gzip + 4 * 255 + 2 * blosc + 2 * zstd
    );
    assert_eq!(Chain::new(&chain).unwrap().bytes(u64::MAX), None);
}

#[test]
fn stacked_blosc_prepays_peak_scratch_and_retains_every_intermediate() {
    use zarrs::array::codec::{BloscCompressor, BloscShuffleMode};

    let blosc = Arc::new(
        BloscCodec::new(
            BloscCompressor::LZ4,
            1.try_into().unwrap(),
            None,
            BloscShuffleMode::NoShuffle,
            Some(1),
        )
        .unwrap(),
    );
    for stages in 1..=3 {
        let codecs: Vec<Arc<dyn BytesToBytesCodecTraits>> = (0..stages)
            .map(|_| blosc.clone() as Arc<dyn BytesToBytesCodecTraits>)
            .collect();
        let chain = CodecChain::new(vec![], Arc::new(BytesCodec::default()), codecs);
        let chain = Chain::new(&chain).unwrap();
        for native in [192u64, 4096] {
            // Each Blosc encoder adds at most its 16-byte header. Only the
            // first decoder output is fixed native bytes; subsequent bounded
            // representations retain their two-copy intermediate allowance.
            let intermediates = (1..stages).map(|n| 2 * (native + n * 16)).sum::<u64>();
            let encoded = 2 * (native + stages * 16);
            let peak_scratch = 3 * (native + (stages - 1) * 16) + 4 * 255;
            assert_eq!(
                chain.bytes(native),
                Some(intermediates + encoded + peak_scratch)
            );
        }
        assert_eq!(chain.bytes(u64::MAX), None);
    }
}

#[test]
fn nested_shards_use_unknown_plan_instead_of_expanding_whole_shards() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(EngineStore::new(Arc::new(
        FilesystemStore::new(dir.path()).unwrap(),
    )));
    let inner =
        ShardingCodecBuilder::new(vec![2.try_into().unwrap(); 2], &data_type::float32()).build();
    let outer = ShardingCodecBuilder::new(vec![4.try_into().unwrap(); 2], &data_type::float32())
        .array_to_bytes_codec(Arc::new(inner))
        .build();
    let array = ArrayBuilder::new(vec![8, 8], vec![8, 8], data_type::float32(), 0f32)
        .array_to_bytes_codec(Arc::new(outer))
        .build(store, "/a")
        .unwrap();
    assert!(matches!(Plan::new(&array), Plan::Unknown));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stacked_full_and_partial_decoders_reuse_actual_peak_scratch_credit() {
    use crate::{encoded, read_budget::Budget};
    use zarrs::array::{
        codec::{BloscCompressor, BloscShuffleMode},
        ArrayBytes, FromArrayBytes,
    };

    let options = crate::catalog::single_threaded_opts();
    let codec = Arc::new(
        BloscCodec::new(
            BloscCompressor::LZ4,
            1.try_into().unwrap(),
            Some(64),
            BloscShuffleMode::NoShuffle,
            Some(1),
        )
        .unwrap(),
    );
    let dir = tempfile::tempdir().unwrap();
    let writer = ArrayBuilder::new(vec![192], vec![192], data_type::uint8(), 0u8)
        .bytes_to_bytes_codecs(vec![codec.clone(), codec.clone()])
        .build(Arc::new(FilesystemStore::new(dir.path()).unwrap()), "/a")
        .unwrap();
    writer.store_metadata().unwrap();
    let expected: Vec<u8> = (0..192).collect();
    writer
        .store_chunk_opt(&[0], expected.clone(), &options)
        .unwrap();
    let outer = std::fs::read(dir.path().join(writer.chunk_key(&[0]).as_str())).unwrap();
    let inner = codec
        .decode(
            Cow::Borrowed(&outer),
            &BytesRepresentation::BoundedSize(208),
            &options,
        )
        .unwrap();
    let config = ds_core::config::ZarrConfig::auto_local(dir.path().to_string_lossy().into_owned());
    let array = bounded_array(
        Array::open(
            Arc::new(EngineStore::plain(
                crate::build_store("scratch-peak", &config).unwrap(),
            )),
            "/a",
        )
        .unwrap(),
    )
    .unwrap();
    let retained = 2 * (outer.len() + inner.len()) as u64;
    for partial in [false, true] {
        let scratch = |frame: &[u8]| {
            let block = u64::from(u32::from_le_bytes(frame[8..12].try_into().unwrap()));
            block * if partial { 3 } else { 2 } + 4 * u64::from(frame[3])
        };
        let peak = scratch(&outer).max(scratch(&inner));
        assert!(peak < scratch(&outer) + scratch(&inner));
        let budget = Arc::new(Budget::new(retained + peak));
        let permit = budget.reserve_bytes(retained + peak).unwrap();
        let scope =
            encoded::enter_prepaid(Some(budget.clone()), Some(permit.clone()), retained + peak);
        let subset = ArraySubset::new_with_ranges(&[if partial { 1..191 } else { 0..192 }]);
        let bytes: ArrayBytes = array.retrieve_array_subset_opt(&subset, &options).unwrap();
        assert_eq!(
            Vec::<u8>::from_array_bytes(bytes, subset.shape(), array.data_type()).unwrap(),
            if partial {
                &expected[1..191]
            } else {
                &expected[..]
            }
        );
        // The frame-derived peak (not the larger metadata bound) has been
        // refunded. All encoded/intermediate copies remain charged, so holding
        // the peak again leaves no room for even one further scratch byte.
        let context = encoded::current().unwrap();
        let scratch = context.codec_scratch(peak).unwrap();
        assert_eq!(budget.metrics().2, 0);
        assert!(context.codec_scratch(1).is_err());
        drop(scratch);
        drop(context);
        drop(scope);
        assert_eq!(budget.metrics().0, retained + peak);
        drop(permit);
        assert_eq!(budget.metrics().0, 0);
    }
}
