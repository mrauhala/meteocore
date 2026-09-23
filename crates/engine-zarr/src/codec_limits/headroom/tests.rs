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
