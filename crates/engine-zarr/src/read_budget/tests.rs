use super::*;
use zarrs::array::{
    codec::{GzipCodec, ShardingCodecBuilder},
    data_type, ArrayBuilder,
};
use zarrs::filesystem::FilesystemStore;

fn array(shape: Vec<u64>, chunk: Vec<u64>, inner: Option<Vec<u64>>) -> Array<EngineStore> {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(EngineStore::new(Arc::new(
        FilesystemStore::new(dir.path()).unwrap(),
    )));
    let mut builder = ArrayBuilder::new(shape, chunk, data_type::float32(), f32::NAN);
    if let Some(inner) = inner {
        builder.array_to_bytes_codec(Arc::new(
            ShardingCodecBuilder::new(
                inner.into_iter().map(|n| n.try_into().unwrap()).collect(),
                &data_type::float32(),
            )
            .bytes_to_bytes_codecs(vec![Arc::new(GzipCodec::new(1).unwrap())])
            .build(),
        ));
    }
    // No metadata or payloads are written: planning must not need storage I/O.
    builder.build(store, "/data").unwrap()
}

#[test]
fn tiny_subset_budgets_full_native_chunk_including_edge_padding() {
    let a = array(vec![1, 1, 1], vec![61, 241, 240], None);
    let subset = a.subset_all();
    let decode_bytes = 61 * 241 * 240 * 4;
    assert_eq!(
        estimate(&a, &subset, 0, false, u64::MAX).unwrap(),
        24 + decode_bytes * 4
    );
    let budget = Arc::new(Budget::new(ds_cache::MIB));
    assert!(matches!(
        budget.reserve(&a, &subset, Some(0), false),
        Err(DataServerError::ResourceExhausted)
    ));
    assert_eq!(budget.used.load(Ordering::Relaxed), 0);
    assert_eq!(budget.rejected.load(Ordering::Relaxed), 1);
}

#[test]
fn source_window_is_limited_independently_of_chunk_size() {
    let a = array(vec![4096, 4096], vec![16, 16], None);
    let budget = Arc::new(Budget::new(ds_cache::MIB));
    // Reject the 16M-cell source window before walking its chunk grid.
    assert!(matches!(
        budget.reserve(&a, &a.subset_all(), Some(0), false),
        Err(DataServerError::ResourceExhausted)
    ));
    assert!(budget
        .reserve(
            &a,
            &ArraySubset::new_with_ranges(&[0..16, 0..16]),
            Some(0),
            false
        )
        .is_ok());
}

#[test]
fn sharded_estimate_distinguishes_inner_reads_from_full_shard_fast_path() {
    let a = array(vec![2, 8, 8], vec![2, 8, 8], Some(vec![2, 2, 2]));
    let one = ArraySubset::new_with_ranges(&[0..1, 0..1, 0..1]);
    // 8 f32 values per inner chunk, 16 index entries of 16 bytes each.
    assert_eq!(
        estimate(&a, &one, 0, false, u64::MAX).unwrap(),
        24 + 4 * (32 + 256)
    );
    let all = a.subset_all();
    let source = 128 * 24;
    assert_eq!(
        estimate(&a, &all, 0, false, u64::MAX).unwrap(),
        source + 4 * (512 + 256)
    );
    assert_eq!(
        estimate(&a, &all, 0, true, u64::MAX).unwrap(),
        source + 4 * (32 + 256)
    );
}

#[test]
fn outer_compression_reserves_a_full_shard_even_for_one_value() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(EngineStore::new(Arc::new(
        FilesystemStore::new(dir.path()).unwrap(),
    )));
    let array = ArrayBuilder::new(vec![8, 8], vec![8, 8], data_type::float32(), f32::NAN)
        .array_to_bytes_codec(Arc::new(
            ShardingCodecBuilder::new(vec![2.try_into().unwrap(); 2], &data_type::float32())
                .build(),
        ))
        .bytes_to_bytes_codecs(vec![Arc::new(GzipCodec::new(1).unwrap())])
        .build(store, "/data")
        .unwrap();
    let subset = ArraySubset::new_with_ranges(&[0..1, 0..1]);
    assert_eq!(
        estimate(&array, &subset, 0, false, u64::MAX).unwrap(),
        24 + 4 * (256 + 256)
    );
}

#[test]
fn concurrent_reservations_cannot_overcommit_and_last_owner_releases() {
    let a = array(vec![2, 2], vec![2, 2], None);
    let subset = a.subset_all();
    let size = estimate(&a, &subset, 0, false, u64::MAX).unwrap();
    let budget = Arc::new(Budget::new(size * 3));
    let barrier = std::sync::Barrier::new(12);
    std::thread::scope(|scope| {
        for _ in 0..12 {
            let (budget, a, subset, barrier) = (&budget, &a, &subset, &barrier);
            scope.spawn(move || {
                let held = budget.reserve(a, subset, Some(0), false);
                barrier.wait();
                assert_eq!(budget.used.load(Ordering::Relaxed), size * 3);
                barrier.wait();
                drop(held);
            });
        }
    });
    assert_eq!(budget.used.load(Ordering::Relaxed), 0);
    assert_eq!(budget.rejected.load(Ordering::Relaxed), 9);
    let request = budget.reserve(&a, &subset, Some(0), false).unwrap();
    let worker = request.clone();
    drop(request);
    assert_eq!(budget.used.load(Ordering::Relaxed), size);
    drop(worker);
    assert_eq!(budget.used.load(Ordering::Relaxed), 0);
    let held = budget.reserve(&a, &subset, Some(0), false).unwrap();
    let _ = std::panic::catch_unwind(move || {
        let _held = held;
        panic!("decode panic");
    });
    assert_eq!(budget.used.load(Ordering::Relaxed), 0);
}

#[test]
fn expired_deadline_and_overflow_never_reserve() {
    let a = array(vec![2, 2], vec![2, 2], None);
    let budget = Arc::new(Budget::new(1000));
    assert!(matches!(
        budget.reserve(&a, &a.subset_all(), Some(u64::MAX), false),
        Err(DataServerError::ResourceExhausted)
    ));
    assert!(matches!(
        bytes(&[u64::MAX, 2], 8),
        Err(DataServerError::ResourceExhausted)
    ));
    let _scope = deadline::enter(Some(std::time::Instant::now()));
    assert!(matches!(
        budget.reserve(&a, &a.subset_all(), Some(0), false),
        Err(DataServerError::DeadlineExceeded)
    ));
    assert_eq!(budget.used.load(Ordering::Relaxed), 0);
    assert_eq!(budget.rejected.load(Ordering::Relaxed), 1);
}
