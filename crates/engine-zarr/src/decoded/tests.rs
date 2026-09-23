use super::*;
use zarrs::array::{
    codec::{GzipCodec, ShardingCodecBuilder},
    data_type, ArrayBuilder, FromArrayBytes,
};
use zarrs::filesystem::FilesystemStore;

pub(super) fn fixture(
    dir: &std::path::Path,
    name: &str,
    sharded: bool,
    offset: f32,
) -> Array<EngineStore> {
    let storage = Arc::new(FilesystemStore::new(dir).unwrap());
    let mut builder = ArrayBuilder::new(
        vec![3, 5, 7],
        vec![2, 4, 6],
        data_type::float32(),
        -999.0f32,
    );
    if sharded {
        builder.array_to_bytes_codec(Arc::new(
            ShardingCodecBuilder::new(
                vec![
                    2.try_into().unwrap(),
                    2.try_into().unwrap(),
                    2.try_into().unwrap(),
                ],
                &data_type::float32(),
            )
            .bytes_to_bytes_codecs(vec![Arc::new(GzipCodec::new(1).unwrap())])
            .build(),
        ));
    } else {
        builder.bytes_to_bytes_codecs(vec![Arc::new(GzipCodec::new(1).unwrap())]);
    }
    let writer = builder.build(storage.clone(), name).unwrap();
    writer.store_metadata().unwrap();
    let subset = ArraySubset::new_with_ranges(&[0..2, 0..4, 0..6]);
    writer
        .store_array_subset(
            &subset,
            (0..48).map(|i| i as f32 + offset).collect::<Vec<_>>(),
        )
        .unwrap();
    Array::open(Arc::new(EngineStore::new(storage)), name).unwrap()
}

fn read(reader: &DecodedArray, array: &Array<EngineStore>, subset: &ArraySubset) -> Vec<f32> {
    Vec::<f32>::from_array_bytes(
        reader
            .read(
                array,
                subset,
                &crate::catalog::single_threaded_opts(),
                MAX_PARALLEL_CHUNKS,
            )
            .unwrap(),
        subset.shape(),
        array.data_type(),
    )
    .unwrap()
}

#[test]
fn cached_chunks_match_subsets_across_shards_edges_and_missing_chunks() {
    for sharded in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let array = fixture(dir.path(), "/temperature", sharded, 0.0);
        let cache = Arc::new(DecodedCache::new(MIB));
        let reader =
            DecodedArray::new(&array, Some("snapshot"), "temperature", cache.clone()).unwrap();
        for ranges in [[1..3, 1..5, 1..7], [0..1, 2..5, 4..7], [0..3, 0..5, 0..7]] {
            let subset = ArraySubset::new_with_ranges(&ranges);
            let expected = array
                .retrieve_array_subset_opt::<Vec<f32>>(
                    &subset,
                    &crate::catalog::single_threaded_opts(),
                )
                .unwrap();
            assert_eq!(read(&reader, &array, &subset), expected);
            assert_eq!(read(&reader, &array, &subset), expected);
        }
        let all = array.subset_all();
        let expected = read(&reader, &array, &all);
        std::fs::remove_dir_all(dir.path().join("temperature")).unwrap();
        assert_eq!(read(&reader, &array, &all), expected);
        assert!(cache.metrics().hits > 0);
        assert!(cache.metrics().bytes <= MIB);
    }
}

#[test]
fn cache_keys_separate_arrays_and_snapshots() {
    let dir = tempfile::tempdir().unwrap();
    let a = fixture(dir.path(), "/a", true, 0.0);
    let b = fixture(dir.path(), "/b", true, 100.0);
    let cache = Arc::new(DecodedCache::new(MIB));
    let a_old = DecodedArray::new(&a, Some("old"), "a", cache.clone()).unwrap();
    let b_old = DecodedArray::new(&b, Some("old"), "b", cache.clone()).unwrap();
    let subset = ArraySubset::new_with_ranges(&[0..1, 0..1, 0..1]);
    assert_eq!(read(&a_old, &a, &subset), vec![0.0]);
    assert_eq!(read(&b_old, &b, &subset), vec![100.0]);
    let a_new = fixture(dir.path(), "/a", true, 200.0);
    let new_reader = DecodedArray::new(&a_new, Some("new"), "a", cache).unwrap();
    assert_eq!(read(&new_reader, &a_new, &subset), vec![200.0]);
    assert_eq!(read(&a_old, &a, &subset), vec![0.0]);
}

#[test]
fn cached_windows_fit_without_cold_workspace_and_release_batch_reservations() {
    use crate::{encoded, read_budget::Budget};

    for sharded in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let array = fixture(dir.path(), "/a", sharded, 0.0);
        let array = crate::codec_limits::bounded_array(array).unwrap();
        let subset = array.subset_all();
        let cache = Arc::new(DecodedCache::new(MIB));
        let reader = DecodedArray::new(&array, Some("snapshot"), "a", cache.clone()).unwrap();
        // The 105-value f32 window needs 2520 bytes for native/conversion
        // buffers. Another 192 bytes can pin the largest cached chunk, but
        // cannot admit a cold decode (including full padded shapes/indexes).
        let budget = Arc::new(Budget::new(2520 + 192));
        let source = budget.reserve(&array, &subset, Some(0), true).unwrap();
        {
            let _scope = encoded::enter(Some(budget.clone()));
            assert!(matches!(
                reader.read(&array, &subset, &crate::catalog::single_threaded_opts(), 4),
                Err(DataServerError::ResourceExhausted)
            ));
        }
        assert_eq!(budget.metrics(), (2520, 2712, 1));
        assert_eq!(
            cache.metrics().misses,
            0,
            "reject before attempting a cache fill"
        );

        let expected = read(&reader, &array, &subset);
        let chunks = cache.metrics().misses;
        std::fs::remove_dir_all(dir.path().join("a")).unwrap();
        {
            let _scope = encoded::enter(Some(budget.clone()));
            assert_eq!(read(&reader, &array, &subset), expected);
        }
        assert_eq!(
            cache.metrics().hits,
            chunks,
            "one hit per chunk, including retried batch slots"
        );
        assert_eq!(
            budget.metrics(),
            (2520, 2712, 1),
            "only source buffers survive the read"
        );
        drop(source);
        assert_eq!(budget.metrics().0, 0);
    }
}

#[test]
fn admitted_cached_buffer_survives_eviction_without_a_cold_read() {
    use crate::{encoded, read_budget::Budget};

    let dir = tempfile::tempdir().unwrap();
    let array = fixture(dir.path(), "/a", true, 0.0);
    let subset = ArraySubset::new_with_ranges(&[0..1, 0..1, 0..1]);
    let cache = Arc::new(DecodedCache::new(MIB));
    let reader = DecodedArray::new(&array, Some("snapshot"), "a", cache.clone()).unwrap();
    assert_eq!(read(&reader, &array, &subset), vec![0.0]);
    let budget = Arc::new(Budget::new(32));
    let mut prepared = reader
        .prepare_chunk(&array, &subset, vec![0, 0, 0], 4, Duration::ZERO)
        .unwrap()
        .unwrap();
    prepared.permit = Some(
        budget
            .reserve_bytes(prepared.reservation_bytes(&array).unwrap())
            .unwrap(),
    );
    assert_eq!(budget.metrics(), (32, 32, 0));
    cache.0.retain(|_, _| false);
    assert_eq!(cache.metrics().bytes, 0);
    std::fs::remove_dir_all(dir.path().join("a")).unwrap();
    let _scope = encoded::enter(Some(budget.clone()));
    let loaded = reader
        .load_chunk(
            &array,
            &crate::catalog::single_threaded_opts(),
            prepared,
            Some(budget.clone()),
        )
        .unwrap();
    assert_eq!(&loaded.bytes[..4], &0.0f32.to_ne_bytes());
    assert_eq!(
        budget.metrics(),
        (32, 32, 0),
        "pin remains charged after eviction and loading"
    );
    assert_eq!(cache.metrics().hits, 1);
    assert_eq!(cache.metrics().misses, 1);
    drop(loaded);
    assert_eq!(budget.metrics().0, 0);
}

#[test]
fn cache_eviction_and_oversized_bypass_preserve_pixels() {
    let dir = tempfile::tempdir().unwrap();
    let array = fixture(dir.path(), "/a", true, 0.0);
    let all = array.subset_all();
    let expected = array.retrieve_array_subset::<Vec<f32>>(&all).unwrap();
    assert!(DecodedArray::new(&array, None, "a", Arc::new(DecodedCache::new(MIB))).is_none());
    for capacity in [0, 1, 512] {
        let cache = Arc::new(DecodedCache::new(capacity));
        let reader = DecodedArray::new(&array, Some("snap"), "a", cache.clone()).unwrap();
        assert_eq!(read(&reader, &array, &all), expected);
        assert_eq!(read(&reader, &array, &all), expected);
        assert!(cache.metrics().bytes <= capacity);
        if capacity <= 1 {
            assert_eq!(cache.metrics().bytes, 0);
        } else {
            assert!(cache.metrics().misses > 0);
        }
    }
}

#[test]
fn sharded_arrays_with_outer_compression_bypass_decoded_retention() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Arc::new(EngineStore::new(Arc::new(
        FilesystemStore::new(dir.path()).unwrap(),
    )));
    let array = ArrayBuilder::new(vec![8, 8], vec![4, 4], data_type::float32(), -999.0f32)
        .array_to_bytes_codec(Arc::new(
            ShardingCodecBuilder::new(vec![2.try_into().unwrap(); 2], &data_type::float32())
                .build(),
        ))
        .bytes_to_bytes_codecs(vec![Arc::new(GzipCodec::new(1).unwrap())])
        .build(storage, "/a")
        .unwrap();
    assert!(array.is_sharded());
    assert!(DecodedArray::new(
        &array,
        Some("snapshot"),
        "a",
        Arc::new(DecodedCache::new(MIB))
    )
    .is_none());
}

#[test]
fn subset_byte_count_rejects_overflow() {
    assert!(matches!(
        byte_length(&[u64::MAX, 2], 8),
        Err(DataServerError::ResourceExhausted)
    ));
}

#[test]
fn native_integer_chunks_preserve_values_and_fill() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Arc::new(FilesystemStore::new(dir.path()).unwrap());
    let writer = ArrayBuilder::new(vec![3, 3], vec![2, 2], data_type::int16(), i16::MIN)
        .bytes_to_bytes_codecs(vec![Arc::new(GzipCodec::new(1).unwrap())])
        .build(storage.clone(), "/packed")
        .unwrap();
    writer.store_metadata().unwrap();
    writer
        .store_chunk(&[0, 0], vec![-5i16, 300, 42, -1000])
        .unwrap();
    let array = Array::open(Arc::new(EngineStore::new(storage)), "/packed").unwrap();
    let cache = Arc::new(DecodedCache::new(MIB));
    let reader = DecodedArray::new(&array, Some("snapshot"), "packed", cache).unwrap();
    let all = array.subset_all();
    let bytes = reader
        .read(
            &array,
            &all,
            &crate::catalog::single_threaded_opts(),
            MAX_PARALLEL_CHUNKS,
        )
        .unwrap();
    let values = Vec::<i16>::from_array_bytes(bytes, all.shape(), array.data_type()).unwrap();
    assert_eq!(
        values,
        vec![
            -5,
            300,
            i16::MIN,
            42,
            -1000,
            i16::MIN,
            i16::MIN,
            i16::MIN,
            i16::MIN
        ]
    );
}

#[test]
fn very_large_chunks_keep_partial_read_path() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Arc::new(FilesystemStore::new(dir.path()).unwrap());
    let writer = ArrayBuilder::new(
        vec![8192, 4096],
        vec![8192, 4096],
        data_type::float32(),
        -999.0f32,
    )
    .build(storage.clone(), "/huge")
    .unwrap();
    writer.store_metadata().unwrap();
    let array = Array::open(Arc::new(EngineStore::new(storage)), "/huge").unwrap();
    let cache = Arc::new(DecodedCache::new(256 * MIB));
    let reader = DecodedArray::new(&array, Some("snapshot"), "huge", cache.clone()).unwrap();
    let subset = ArraySubset::new_with_ranges(&[2..3, 2..3]);
    assert_eq!(read(&reader, &array, &subset), vec![-999.0]);
    assert_eq!(cache.metrics().bytes, 0);
    assert_eq!(
        cache.metrics().misses,
        0,
        "128 MiB chunk must not start a full cache fill"
    );
}

#[test]
fn failed_decode_is_not_cached_and_retry_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let array = fixture(dir.path(), "/a", false, 0.0);
    let cache = Arc::new(DecodedCache::new(MIB));
    let reader = DecodedArray::new(&array, Some("snapshot"), "a", cache.clone()).unwrap();
    let subset = ArraySubset::new_with_ranges(&[0..1, 0..1, 0..1]);
    let path = dir.path().join(array.chunk_key(&[0, 0, 0]).as_str());
    let original = std::fs::read(&path).unwrap();
    std::fs::write(&path, b"not a gzip chunk").unwrap();
    assert!(reader
        .read(
            &array,
            &subset,
            &crate::catalog::single_threaded_opts(),
            MAX_PARALLEL_CHUNKS,
        )
        .is_err());
    assert_eq!(cache.metrics().bytes, 0);
    std::fs::write(path, original).unwrap();
    assert_eq!(read(&reader, &array, &subset), vec![0.0]);
    assert_eq!(cache.metrics().misses, 2);
}

#[test]
fn waiting_for_a_chunk_observes_the_waiters_deadline() {
    use std::sync::mpsc;
    let dir = tempfile::tempdir().unwrap();
    let array = fixture(dir.path(), "/a", true, 0.0);
    let cache = Arc::new(DecodedCache::new(MIB));
    let reader = DecodedArray::new(&array, Some("snapshot"), "a", cache.clone()).unwrap();
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            let key = Key {
                revision: "snapshot".into(),
                array: "a".into(),
                indices: vec![0, 0, 0],
            };
            cache
                .0
                .get_or_insert_with(&key, || {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Err::<Arc<Vec<u8>>, ()>(())
                })
                .unwrap_err();
        });
        started_rx.recv().unwrap();
        let _scope = deadline::enter(Some(Instant::now() + Duration::from_millis(20)));
        let result = reader.read(
            &array,
            &ArraySubset::new_with_ranges(&[0..1, 0..1, 0..1]),
            &crate::catalog::single_threaded_opts(),
            MAX_PARALLEL_CHUNKS,
        );
        release_tx.send(()).unwrap();
        assert!(matches!(result, Err(DataServerError::DeadlineExceeded)));
    });
}
