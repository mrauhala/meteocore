use super::*;
use zarrs::{
    array::{
        codec::{BloscCodec, BloscCompressor, BloscShuffleMode, ShardingCodecBuilder},
        data_type, ArrayBuilder,
    },
    filesystem::FilesystemStore,
};

#[test]
fn peak_scratch_headroom_admits_two_cold_chunks_where_summed_scratch_admitted_one() {
    let _exclusive = GATED_TESTS.lock().unwrap();
    let options = crate::catalog::single_threaded_opts();
    for sharded in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let codecs: Vec<Arc<dyn zarrs::array::codec::api::BytesToBytesCodecTraits>> = [
            (BloscShuffleMode::Shuffle, 4),
            (BloscShuffleMode::NoShuffle, 1),
        ]
        .into_iter()
        .map(|(shuffle, typesize)| {
            Arc::new(
                BloscCodec::new(
                    BloscCompressor::LZ4,
                    1.try_into().unwrap(),
                    None,
                    shuffle,
                    Some(typesize),
                )
                .unwrap(),
            ) as Arc<dyn zarrs::array::codec::api::BytesToBytesCodecTraits>
        })
        .collect();
        let mut builder = ArrayBuilder::new(
            vec![1, 8, 24],
            if sharded {
                vec![1, 8, 24]
            } else {
                vec![1, 8, 6]
            },
            data_type::float32(),
            -999.0f32,
        );
        if sharded {
            builder.array_to_bytes_codec(Arc::new(
                ShardingCodecBuilder::new(
                    vec![
                        1.try_into().unwrap(),
                        8.try_into().unwrap(),
                        6.try_into().unwrap(),
                    ],
                    &data_type::float32(),
                )
                .bytes_to_bytes_codecs(codecs)
                .build(),
            ));
        } else {
            builder.bytes_to_bytes_codecs(codecs);
        }
        let writer = builder
            .build(Arc::new(FilesystemStore::new(dir.path()).unwrap()), "/a")
            .unwrap();
        writer.store_metadata().unwrap();
        writer
            .store_array_subset_opt(
                &writer.subset_all(),
                (0..192).map(|n| n as f32).collect::<Vec<_>>(),
                &options,
            )
            .unwrap();
        let original = Array::open(Arc::new(EngineStore::new(writer.storage())), "/a").unwrap();
        for subset in [
            writer.subset_all(),
            ArraySubset::new_with_ranges(&[0..1, 1..7, 1..23]),
        ] {
            let expected = read_native(&original, &subset, &options).unwrap();
            let source_bytes = subset.num_elements() * 24;
            // Full stored native chunk: 192 bytes. Two Blosc stages retain
            // 416 intermediate + 448 encoded bytes, and share peak scratch
            // of 3*208 + 4*255 = 1644 bytes. Previously scratch also included
            // the first stage's 1596 bytes, even though its call is sequential.
            let per_chunk = if sharded {
                // Four-entry index: 64 native bytes and a four-byte CRC trailer.
                4 * (192 + 64) + 864 + 1644 + 2 * (64 + 4)
            } else {
                4 * 192 + 864 + 1644
            };
            let available = 2 * per_chunk;
            assert_eq!(available / (per_chunk + 1596), 1);
            let budget = Arc::new(Budget::new(source_bytes + available));
            let (started, ready) = mpsc::channel();
            let probe = Arc::new(Probe {
                deadline: None,
                active: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                calls: AtomicUsize::new(0),
                started,
                open: Mutex::new(false),
                wake: Condvar::new(),
                budget: budget.clone(),
                outcome: Outcome::Success,
            });
            let array = crate::codec_limits::bounded_array(
                Array::open(
                    Arc::new(EngineStore::new(ProbedStore {
                        inner: original.storage(),
                        probe: probe.clone(),
                    })),
                    "/a",
                )
                .unwrap(),
            )
            .unwrap();
            let reader = DecodedArray::new(
                &array,
                Some("snapshot"),
                "a",
                Arc::new(DecodedCache::new(0)),
            )
            .unwrap();
            let source = budget.reserve(&array, &subset, Some(0), true).unwrap();
            std::thread::scope(|scope| {
                let worker = scope.spawn(|| {
                    let _encoded = crate::encoded::enter(Some(budget.clone()));
                    reader.read(&array, &subset, &options, source.parallelism())
                });
                for _ in 0..2 {
                    ready.recv_timeout(Duration::from_secs(2)).unwrap();
                }
                assert_eq!(probe.active.load(Ordering::SeqCst), 2);
                assert_eq!(
                    budget.metrics(),
                    (source_bytes + available, source_bytes + available, 0)
                );
                *probe.open.lock().unwrap() = true;
                probe.wake.notify_all();
                assert_eq!(
                    worker
                        .join()
                        .unwrap()
                        .unwrap()
                        .into_fixed()
                        .unwrap()
                        .as_ref(),
                    expected
                );
            });
            assert_eq!(probe.peak.load(Ordering::SeqCst), 2);
            assert_eq!(
                budget.metrics(),
                (source_bytes, source_bytes + available, 0)
            );
            drop(source);
            assert_eq!(budget.metrics().0, 0);
        }
    }
}
