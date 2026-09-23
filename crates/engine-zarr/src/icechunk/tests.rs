use super::*;
use crate::ZarrEngine;
use ds_core::{
    edr_engine::EdrEngine,
    map_engine::{MapEngine, OutputCrs},
};
use zarrs::{
    array::{
        codec::{GzipCodec, ShardingCodecBuilder},
        data_type, Array, ArrayBuilder, ArraySubset,
    },
    group::GroupBuilder,
};

/// External chunk objects and compressed inner chunks exercise the range-read
/// path used by remote maps (the original tiny fixture is entirely inline).
async fn fixture(dir: &std::path::Path) -> Repository {
    fixture_with_codec(dir, Arc::new(GzipCodec::new(1).unwrap())).await
}

async fn fixture_with_codec(
    dir: &std::path::Path,
    codec: Arc<dyn zarrs::array::codec::api::BytesToBytesCodecTraits>,
) -> Repository {
    let storage = icechunk::new_local_filesystem_storage(dir).await.unwrap();
    let repo = Repository::create(
        Some(RepositoryConfig {
            inline_chunk_threshold_bytes: Some(0),
            caching: Some(icechunk::config::CachingConfig {
                // The reader's cache_mb=0 must override this persisted setting.
                num_bytes_chunks: Some(1024 * 1024),
                ..Default::default()
            }),
            ..Default::default()
        }),
        storage,
        HashMap::new(),
        Default::default(),
        true,
    )
    .await
    .unwrap();
    let store = Arc::new(AsyncIcechunkStore::new(
        repo.writable_session("main").await.unwrap(),
    ));
    GroupBuilder::new()
        .build(store.clone(), "/")
        .unwrap()
        .async_store_metadata()
        .await
        .unwrap();
    for (name, values, units) in [
        ("time", vec![0.0, 6.0], "hours since 2026-01-01"),
        ("lat", vec![59.0, 60.0], "degrees_north"),
        ("lon", vec![0.0, 1.0, 100.0], "degrees_east"),
    ] {
        let array = ArrayBuilder::new(
            vec![values.len() as u64],
            vec![values.len() as u64],
            data_type::float64(),
            f64::NAN,
        )
        .dimension_names(Some([name]))
        .attributes(
            serde_json::json!({"units": units})
                .as_object()
                .unwrap()
                .clone(),
        )
        .build(store.clone(), &format!("/{name}"))
        .unwrap();
        array.async_store_metadata().await.unwrap();
        array.async_store_chunk(&[0], values).await.unwrap();
    }
    let shard = ShardingCodecBuilder::new(
        vec![
            2.try_into().unwrap(),
            1.try_into().unwrap(),
            1.try_into().unwrap(),
        ],
        &data_type::float32(),
    )
    .bytes_to_bytes_codecs(vec![codec])
    .build();
    let array = ArrayBuilder::new(vec![2, 2, 3], vec![2, 2, 3], data_type::float32(), f32::NAN)
        .array_to_bytes_codec(Arc::new(shard))
        .dimension_names(Some(["time", "lat", "lon"]))
        .build(store.clone(), "/temp")
        .unwrap();
    array.async_store_metadata().await.unwrap();
    array
        .async_store_array_subset(
            &ArraySubset::new_with_shape(vec![2, 2, 3]),
            vec![10.0f32; 12],
        )
        .await
        .unwrap();
    store
        .session()
        .write()
        .await
        .commit("fixture")
        .execute()
        .await
        .unwrap();
    repo
}

fn config(dir: &std::path::Path) -> ZarrConfig {
    let mut config = ZarrConfig::auto_local(dir.to_string_lossy().into_owned());
    config.cache_mb = 4;
    config.icechunk = Some(IcechunkConfig {
        decoded_cache_mb: 0,
        branch: Some("main".into()),
        tag: None,
        snapshot: None,
        region: None,
        force_path_style: None,
    });
    config
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blosc_maps_reject_oversized_inner_frames_and_recover_after_refresh() {
    use zarrs::{
        array::codec::{BloscCodec, BloscCompressor, BloscShuffleMode},
        storage::{AsyncReadableStorageTraits, AsyncWritableStorageTraits},
    };
    let dir = tempfile::tempdir().unwrap();
    let codec = BloscCodec::new(
        BloscCompressor::Zstd,
        1.try_into().unwrap(),
        None,
        BloscShuffleMode::BitShuffle,
        Some(4),
    )
    .unwrap();
    let repo = fixture_with_codec(dir.path(), Arc::new(codec)).await;
    let engines: Vec<_> = [0, 1]
        .into_iter()
        .map(|decoded_mb| {
            let mut config = config(dir.path());
            config.icechunk.as_mut().unwrap().decoded_cache_mb = decoded_mb;
            let engine = ZarrEngine::new("blosc", &config).unwrap();
            for _ in 0..2 {
                assert!(render(&engine)
                    .unwrap()
                    .values
                    .iter_values()
                    .all(|v| v == Some(10.)));
            }
            engine
        })
        .collect();
    let writer = AsyncIcechunkStore::new(repo.writable_session("main").await.unwrap());
    let key = StoreKey::new("temp/c/0/0/0").unwrap();
    let original = writer.get(&key).await.unwrap().unwrap();
    let mut corrupt = original.to_vec();
    // Six inner chunks; the default shard index is six (offset,length) pairs
    // plus CRC32C at the end. Keep offsets, encoded sizes and the index intact.
    let index_start = corrupt.len() - (6 * 16 + 4);
    for chunk in 0..6 {
        let entry = index_start + chunk * 16;
        let offset = u64::from_le_bytes(corrupt[entry..entry + 8].try_into().unwrap()) as usize;
        assert_eq!(
            u32::from_le_bytes(corrupt[offset + 4..offset + 8].try_into().unwrap()),
            8
        );
        corrupt[offset + 4..offset + 8].copy_from_slice(&16u32.to_le_bytes());
    }
    writer.set(&key, corrupt.into()).await.unwrap();
    writer
        .session()
        .write()
        .await
        .commit("oversized Blosc inner frames")
        .execute()
        .await
        .unwrap();
    for engine in &engines {
        engine.poll_once();
        assert!(
            matches!(render(engine), Err(DataServerError::Engine(message))
            if message.contains("Blosc output does not match"))
        );
    }
    let writer = AsyncIcechunkStore::new(repo.writable_session("main").await.unwrap());
    writer.set(&key, original).await.unwrap();
    writer
        .session()
        .write()
        .await
        .commit("repair inner frames")
        .execute()
        .await
        .unwrap();
    for engine in &engines {
        engine.poll_once();
        assert!(render(engine)
            .unwrap()
            .values
            .iter_values()
            .all(|v| v == Some(10.)));
    }
}

fn render(engine: &ZarrEngine) -> Result<ds_core::map_engine::RasterTile, DataServerError> {
    engine.get_raster_tile(
        [49.0, 59.1, 51.0, 59.9],
        4,
        4,
        None,
        &OutputCrs::Wgs84,
        Some("temp"),
        None,
        None,
    )
}

async fn commit_values(repo: &Repository, value: f32) {
    let store = Arc::new(AsyncIcechunkStore::new(
        repo.writable_session("main").await.unwrap(),
    ));
    let array = Array::async_open(store.clone(), "/temp").await.unwrap();
    array
        .async_store_array_subset(&ArraySubset::new_with_shape(vec![2, 2, 3]), vec![value; 12])
        .await
        .unwrap();
    store
        .session()
        .write()
        .await
        .commit("replace field")
        .execute()
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_override_preserves_persisted_repository_settings() {
    use icechunk::{
        config::{CachingConfig, CompressionConfig, ObjectStoreConfig, S3Options},
        format::format_constants::SpecVersionBin,
        storage::{ConcurrencySettings, Settings},
        virtual_chunks::VirtualChunkContainer,
    };

    // V1 stores config.yaml; V2 embeds configuration in the repo info object.
    for version in [SpecVersionBin::V1, SpecVersionBin::V2] {
        let dir = tempfile::tempdir().unwrap();
        let storage = icechunk::new_local_filesystem_storage(dir.path())
            .await
            .unwrap();
        let mut persisted = RepositoryConfig {
            inline_chunk_threshold_bytes: Some(17),
            get_partial_values_concurrency: Some(3),
            max_concurrent_requests: Some(7),
            compression: Some(CompressionConfig {
                level: Some(1),
                ..Default::default()
            }),
            caching: Some(CachingConfig {
                num_snapshot_nodes: Some(1234),
                num_chunk_refs: Some(2345),
                num_transaction_changes: Some(3456),
                num_bytes_attributes: Some(4567),
                num_bytes_chunks: Some(1024 * 1024),
            }),
            storage: Some(Settings {
                concurrency: Some(ConcurrencySettings {
                    max_concurrent_requests_for_object: Some(3.try_into().unwrap()),
                    ideal_concurrent_request_size: Some(65536.try_into().unwrap()),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        persisted
            .set_virtual_chunk_container(
                VirtualChunkContainer::new(
                    "s3://preserved-virtual-data/".into(),
                    ObjectStoreConfig::S3(S3Options::default().with_region("us-east-1")),
                )
                .unwrap(),
            )
            .unwrap();
        let repo = Repository::create(
            Some(persisted),
            storage.clone(),
            HashMap::new(),
            Some(version),
            true,
        )
        .await
        .unwrap();
        let original = repo.config().clone();
        drop(repo);

        for cache_mb in [0, 4] {
            let mut config = config(dir.path());
            config.cache_mb = cache_mb;
            let source = Source::open("config-merge", &config).unwrap();
            let mut expected = original.clone();
            expected.caching.as_mut().unwrap().num_bytes_chunks = Some(cache_mb * ds_cache::MIB);
            assert_eq!(source.repo.config(), &expected);
        }
        // Opening a reader must not persist its per-server cache override.
        let reopened = Repository::open(None, storage, HashMap::new())
            .await
            .unwrap();
        assert_eq!(reopened.config(), &original);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn payload_cache_obeys_budget_and_zero_disables_retention() {
    for enabled in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path()).await;
        let mut config = config(dir.path());
        if !enabled {
            config.cache_mb = 0;
        }
        let engine = ZarrEngine::new("cache", &config).unwrap();
        assert!(render(&engine)
            .unwrap()
            .values
            .iter_values()
            .all(|v| v == Some(10.0)));
        std::fs::remove_dir_all(dir.path().join("chunks")).unwrap();
        let second = render(&engine);
        assert_eq!(second.is_ok(), enabled, "cache_mb={}", config.cache_mb);
        if enabled {
            assert!(second
                .unwrap()
                .values
                .iter_values()
                .all(|v| v == Some(10.0)));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encoded_full_and_range_reads_reserve_before_external_payload_io() {
    use crate::{encoded, read_budget::Budget};
    use zarrs::storage::byte_range::ByteRange;
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path()).await;
    let mut config = config(dir.path());
    config.cache_mb = 0;
    let source = Source::open("encoded", &config).unwrap();
    let store = source.snapshot(None).unwrap().unwrap();
    let key = StoreKey::new("temp/c/0/0/0").unwrap();
    let size = store.size_key(&key).unwrap().unwrap();
    let budget = Arc::new(Budget::new(size * 4));
    let full = {
        let _scope = encoded::enter(Some(budget.clone()));
        let full = store.get(&key).unwrap().unwrap();
        assert_eq!(budget.metrics().0, size * 2);
        full
    };
    assert_eq!(budget.metrics().0, 0);
    let ranges = [
        ByteRange::from(0..8),
        ByteRange::Suffix(4),
        ByteRange::FromStart(8, None),
    ];
    {
        let _scope = encoded::enter(Some(budget.clone()));
        let bytes = store
            .get_partial_many(&key, Box::new(ranges.into_iter()))
            .unwrap()
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            bytes,
            vec![
                full.slice(..8),
                full.slice(full.len() - 4..),
                full.slice(8..)
            ]
        );
        assert_eq!(budget.metrics().0, (size + 4) * 2);
    }
    assert_eq!(budget.metrics().0, 0);
    std::fs::remove_dir_all(dir.path().join("chunks")).unwrap();
    for partial in [false, true] {
        let limit = if partial {
            (size + 4) * 2 - 1
        } else {
            size * 2 - 1
        };
        let budget = Arc::new(Budget::new(limit));
        {
            let _scope = encoded::enter(Some(budget.clone()));
            let error = if partial {
                store
                    .get_partial_many(&key, Box::new(ranges.into_iter()))
                    .err()
                    .unwrap()
            } else {
                store.get(&key).unwrap_err()
            };
            assert!(
                matches!(error, StorageError::IOError(error) if matches!(
                    error.get_ref().and_then(|e| e.downcast_ref::<DataServerError>()),
                    Some(DataServerError::ResourceExhausted)
                )),
                "admission must precede the now-missing payload read"
            );
        }
        assert_eq!(budget.metrics(), (0, limit, 1));
    }
    assert!(
        store.get(&key).is_err(),
        "without admission the deleted payload is a storage failure"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decoded_cache_hits_need_no_encoded_reservation_and_misses_keep_typed_errors() {
    use crate::{
        decoded::{DecodedArray, DecodedCache},
        encoded,
        read_budget::Budget,
    };
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path()).await;
    let mut config = config(dir.path());
    config.cache_mb = 0;
    let source = Source::open("encoded", &config).unwrap();
    let store = source.snapshot(None).unwrap().unwrap();
    let array = Array::open(store.clone(), "/temp").unwrap();
    let reader = DecodedArray::new(
        &array,
        store.revision.as_deref(),
        "temp",
        Arc::new(DecodedCache::new(ds_cache::MIB)),
    )
    .unwrap();
    let subset = array.subset_all();
    let options = crate::catalog::single_threaded_opts();
    let budget = Arc::new(Budget::new(0));
    {
        let _scope = encoded::enter(Some(budget.clone()));
        assert!(matches!(
            reader.read(&array, &subset, &options, 4),
            Err(DataServerError::ResourceExhausted)
        ));
    }
    // A rejected admission must not poison a later cache fill.
    let expected = reader
        .read(&array, &subset, &options, 4)
        .unwrap()
        .into_fixed()
        .unwrap()
        .into_owned();
    std::fs::remove_dir_all(dir.path().join("chunks")).unwrap();
    // All six cached 8-byte inner chunks fit, but even one cold read's
    // native/index workspace (416 bytes) cannot. No encoded payloads remain.
    let budget = Arc::new(Budget::new(48));
    let _scope = encoded::enter(Some(budget.clone()));
    let actual = reader
        .read(&array, &subset, &options, 4)
        .unwrap()
        .into_fixed()
        .unwrap()
        .into_owned();
    assert_eq!(actual, expected);
    assert_eq!(budget.metrics().0, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_shards_keep_encoded_and_blosc_scratch_headroom_under_tight_budgets() {
    use crate::{
        decoded::{DecodedArray, DecodedCache},
        encoded,
        read_budget::Budget,
    };
    use zarrs::array::{
        codec::{BloscCodec, BloscCompressor, BloscShuffleMode},
        FromArrayBytes,
    };

    let codecs: Vec<Arc<dyn zarrs::array::codec::api::BytesToBytesCodecTraits>> = vec![
        Arc::new(GzipCodec::new(1).unwrap()),
        Arc::new(
            BloscCodec::new(
                BloscCompressor::LZ4,
                1.try_into().unwrap(),
                None,
                BloscShuffleMode::Shuffle,
                Some(4),
            )
            .unwrap(),
        ),
    ];
    for codec in codecs {
        let dir = tempfile::tempdir().unwrap();
        fixture_with_codec(dir.path(), codec).await;
        let mut config = config(dir.path());
        config.cache_mb = 0;
        let source = Source::open("headroom", &config).unwrap();
        let store = source.snapshot(None).unwrap().unwrap();
        let array =
            crate::codec_limits::bounded_array(Array::open(store.clone(), "/temp").unwrap())
                .unwrap();
        let subset = array.subset_all();
        let reader = DecodedArray::new(
            &array,
            store.revision.as_deref(),
            "temp",
            Arc::new(DecodedCache::new(0)),
        )
        .unwrap();
        // Source: 12 f32 values. Each inner chunk: 8 native bytes + 96
        // index bytes, times four buffers. Native-only admission filled the
        // entire transient budget with four chunks and rejected encoded I/O.
        let budget = Arc::new(Budget::new(288 + 4 * 416));
        let permit = budget.reserve(&array, &subset, Some(0), true).unwrap();
        {
            let _encoded = encoded::enter(Some(budget.clone()));
            let bytes = reader
                .read(
                    &array,
                    &subset,
                    &crate::catalog::single_threaded_opts(),
                    permit.parallelism(),
                )
                .unwrap();
            assert_eq!(
                Vec::<f32>::from_array_bytes(bytes, subset.shape(), array.data_type()).unwrap(),
                vec![10.; 12]
            );
        }
        assert_eq!(budget.metrics(), (288, 1952, 0));
        drop(permit);
        assert_eq!(budget.metrics().0, 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outer_compressed_shards_release_temporary_admission_between_chunks() {
    use crate::{
        decoded::{DecodedArray, DecodedCache},
        encoded,
        read_budget::Budget,
    };
    use zarrs::array::{ArrayBytes, FromArrayBytes};

    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path()).await;
    let store = Arc::new(AsyncIcechunkStore::new(
        repo.writable_session("main").await.unwrap(),
    ));
    let writer = ArrayBuilder::new(
        vec![2, 2, 3],
        vec![1, 2, 1],
        data_type::float32(),
        -999.0f32,
    )
    .array_to_bytes_codec(Arc::new(
        ShardingCodecBuilder::new(vec![1.try_into().unwrap(); 3], &data_type::float32())
            .bytes_to_bytes_codecs(vec![Arc::new(GzipCodec::new(1).unwrap())])
            .build(),
    ))
    .bytes_to_bytes_codecs(vec![Arc::new(GzipCodec::new(1).unwrap())])
    .dimension_names(Some(["time", "lat", "lon"]))
    .build(store.clone(), "/outer")
    .unwrap();
    writer.async_store_metadata().await.unwrap();
    writer
        .async_store_array_subset(&writer.subset_all(), vec![17.0f32; 12])
        .await
        .unwrap();
    store
        .session()
        .write()
        .await
        .commit("outer compressed shards")
        .execute()
        .await
        .unwrap();
    let options = crate::catalog::single_threaded_opts();
    for cache_mb in [0, 4] {
        let mut config = config(dir.path());
        config.cache_mb = cache_mb;
        // Even with decoded retention enabled, outer transforms use the serial
        // path and admit full stored chunks plus intermediate shard bodies.
        config.icechunk.as_mut().unwrap().decoded_cache_mb = 1;
        let open = || {
            let source = Source::open("scopes", &config).unwrap();
            let store = source.snapshot(None).unwrap().unwrap();
            crate::codec_limits::bounded_array(Array::open(store, "/outer").unwrap()).unwrap()
        };
        for subset in [
            writer.subset_all(),
            ArraySubset::new_with_ranges(&[0..1, 0..1, 0..3]),
        ] {
            let array = open();
            assert!(DecodedArray::new(
                &array,
                Some("snapshot"),
                "outer",
                Arc::new(DecodedCache::new(ds_cache::MIB))
            )
            .is_none());
            let probe = Arc::new(Budget::new(ds_cache::MIB));
            let permit = probe.reserve(&array, &subset, Some(0), false).unwrap();
            let baseline = probe.metrics().0;
            let mut maximum = 0;
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
                let _scope = encoded::enter(Some(probe.clone()));
                let _: ArrayBytes = array.retrieve_array_subset_opt(&overlap, &options).unwrap();
                maximum = maximum.max(probe.metrics().0 - baseline);
            }
            drop(permit);
            let array = open(); // Fresh payload cache for the first read.
            let budget = Arc::new(Budget::new(baseline + maximum));
            let permit = budget.reserve(&array, &subset, Some(0), false).unwrap();
            let scope = encoded::enter(Some(budget.clone()));
            for _ in 0..2 {
                let bytes = crate::retrieval::serial(&array, &subset, &options).unwrap();
                assert_eq!(
                    Vec::<f32>::from_array_bytes(bytes, subset.shape(), array.data_type()).unwrap(),
                    vec![17.; subset.num_elements() as usize]
                );
                assert_eq!(budget.metrics(), (baseline, baseline + maximum, 0));
            }
            assert!(matches!(
                array
                    .retrieve_array_subset_opt::<ArrayBytes>(&subset, &options)
                    .map_err(crate::catalog::chunk_read_error),
                Err(DataServerError::ResourceExhausted)
            ));
            drop(scope);
            assert_eq!(budget.metrics().0, baseline);
            drop(permit);
            assert_eq!(budget.metrics().0, 0);
        }
        let engine = ZarrEngine::new("outer", &config).unwrap();
        assert!(engine
            .get_raster_tile(
                [0., 59., 100., 60.],
                3,
                2,
                None,
                &OutputCrs::Wgs84,
                Some("outer"),
                None,
                None
            )
            .unwrap()
            .values
            .iter_values()
            .all(|value| value == Some(17.)));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn absent_icechunk_payloads_still_return_fill_under_encoded_admission() {
    use crate::{encoded, read_budget::Budget};
    use zarrs::storage::{byte_range::ByteRange, AsyncWritableStorageTraits};
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path()).await;
    let writer = AsyncIcechunkStore::new(repo.writable_session("main").await.unwrap());
    let key = StoreKey::new("temp/c/0/0/0").unwrap();
    writer.erase(&key).await.unwrap();
    writer
        .session()
        .write()
        .await
        .commit("missing shard")
        .execute()
        .await
        .unwrap();
    let source = Source::open("missing", &config(dir.path())).unwrap();
    let store = source.snapshot(None).unwrap().unwrap();
    let array = Array::open(store.clone(), "/temp").unwrap();
    let budget = Arc::new(Budget::new(0));
    let _scope = encoded::enter(Some(budget.clone()));
    assert!(store.get(&key).unwrap().is_none());
    let ranges = [ByteRange::from(16..32), ByteRange::Suffix(16)];
    assert!(store
        .get_partial_many(&key, Box::new(ranges.into_iter()))
        .unwrap()
        .is_none());
    let values = array
        .retrieve_array_subset_opt::<Vec<f32>>(
            &array.subset_all(),
            &crate::catalog::single_threaded_opts(),
        )
        .unwrap();
    assert!(values.iter().all(|value| value.is_nan()));
    assert_eq!(budget.metrics().0, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_maps_read_icechunk_through_render_executor() {
    // Maps, WMS, and Tiles all use acquire_raster + RenderJob::run. Exercise
    // actual async file/range reads and shard decoding inside spawn_blocking,
    // with more requests than render slots and both cold and warm payloads.
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path()).await;
    for (cache_mb, decoded_mb) in [(0, 0), (4, 0), (0, 4)] {
        let mut config = config(dir.path());
        config.cache_mb = cache_mb;
        config.icechunk.as_mut().unwrap().decoded_cache_mb = decoded_mb;
        let engine = Arc::new(ZarrEngine::new("render-executor", &config).unwrap());
        let slots = Arc::new(tokio::sync::Semaphore::new(4));
        for _ in 0..3 {
            let start = Arc::new(tokio::sync::Barrier::new(8));
            let mut requests = tokio::task::JoinSet::new();
            for _ in 0..8 {
                let (engine, slots, start) = (engine.clone(), slots.clone(), start.clone());
                requests.spawn(async move {
                    start.wait().await;
                    let (job, memory) = ds_executor::RenderJob::acquire_raster(slots, 4, 4)
                        .await
                        .unwrap();
                    let worker_memory = memory.clone();
                    let tile = job
                        .run(move || {
                            let _memory = worker_memory;
                            assert!(ds_core::deadline::current().is_some());
                            render(&engine)
                        })
                        .await
                        .expect("Icechunk render worker must complete without a panic")
                        .expect("Icechunk range reads and decoding must succeed");
                    assert!(tile.values.iter_values().all(|v| v == Some(10.0)));
                });
            }
            while let Some(result) = requests.join_next().await {
                result.unwrap();
            }
            assert_eq!(slots.available_permits(), 4);
        }
        if decoded_mb > 0 {
            let metrics = engine.decoded_cache_metrics();
            assert_eq!(
                metrics.misses, 6,
                "one fill per inner chunk across all requests"
            );
            assert!(metrics.hits > 0);
            assert!(metrics.bytes <= metrics.capacity_bytes);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decoded_inner_chunks_reuse_other_leads_and_preserve_deadlines_without_payload_cache() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path()).await;
    let mut config = config(dir.path());
    config.cache_mb = 0;
    config.icechunk.as_mut().unwrap().decoded_cache_mb = 4;
    let engine = ZarrEngine::new("decoded", &config).unwrap();
    render(&engine).unwrap();
    let metrics = engine.decoded_cache_metrics();
    assert_eq!(metrics.misses, 6);
    assert!(
        metrics.bytes < 4096,
        "retain the small native chunks, not expanded f64 windows"
    );
    std::fs::remove_dir_all(dir.path().join("chunks")).unwrap();
    let tile = engine
        .get_raster_tile(
            [49.2, 59.2, 51.2, 59.8],
            8,
            8,
            Some(engine.get_available_times().unwrap()[1]),
            &OutputCrs::Wgs84,
            Some("temp"),
            None,
            None,
        )
        .unwrap();
    assert!(tile.values.iter_values().all(|v| v == Some(10.0)));
    assert!(engine
        .query_position("POINT(50 59.5)", None, Some(&["temp".into()]), None, None)
        .is_ok());
    assert_eq!(engine.decoded_cache_metrics().misses, 6);
    let _deadline = ds_core::deadline::enter(Some(std::time::Instant::now()));
    assert!(matches!(
        render(&engine),
        Err(DataServerError::DeadlineExceeded)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn renders_reject_expired_deadlines_on_cold_and_warm_reads() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path()).await;
    let engine = ZarrEngine::new("deadline", &config(dir.path())).unwrap();
    for _ in 0..2 {
        let scope = ds_core::deadline::enter(Some(
            std::time::Instant::now() - std::time::Duration::from_secs(1),
        ));
        assert!(matches!(
            render(&engine),
            Err(DataServerError::DeadlineExceeded)
        ));
        drop(scope);
        assert!(render(&engine).is_ok());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unchanged_payloads_stay_cached_across_snapshot_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path()).await;
    let engine = ZarrEngine::new("cache-refresh", &config(dir.path())).unwrap();
    render(&engine).unwrap();
    let version = engine.content_version();
    let writer = Arc::new(AsyncIcechunkStore::new(
        repo.writable_session("main").await.unwrap(),
    ));
    let mut group = zarrs::group::Group::async_open(writer.clone(), "/")
        .await
        .unwrap();
    group
        .attributes_mut()
        .insert("description".into(), "metadata revision".into());
    group.async_store_metadata().await.unwrap();
    writer
        .session()
        .write()
        .await
        .commit("metadata only")
        .execute()
        .await
        .unwrap();
    std::fs::remove_dir_all(dir.path().join("chunks")).unwrap();
    engine.poll_once();
    assert_ne!(
        version,
        engine.content_version(),
        "new snapshot must publish"
    );
    assert!(render(&engine)
        .unwrap()
        .values
        .iter_values()
        .all(|v| v == Some(10.0)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn irregular_grid_map_matches_position_sample() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path()).await;
    let engine = ZarrEngine::new("irregular", &config(dir.path())).unwrap();
    let ds_core::model::CoverageResponse::Single(result) = engine
        .query_position("POINT(50 59.5)", None, Some(&["temp".into()]), None, None)
        .unwrap()
    else {
        panic!("single coverage")
    };
    assert_eq!(result.ranges["temp"].values, vec![Some(10.0); 2]);
    assert!(render(&engine)
        .unwrap()
        .values
        .iter_values()
        .all(|v| v == Some(10.0)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn polling_swaps_snapshots_preserves_old_readers_and_versions_render_content() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path()).await;
    let mut config = config(dir.path());
    config.cache_mb = 0;
    config.icechunk.as_mut().unwrap().decoded_cache_mb = 4;
    let engine = ZarrEngine::new("refresh", &config).unwrap();
    render(&engine).unwrap();
    let old = engine.catalog.load_full();
    let version = engine.content_version();
    assert_ne!(version, 0);
    engine.poll_once();
    assert!(
        Arc::ptr_eq(&old, &engine.catalog.load_full()),
        "unchanged HEAD retains the catalog and caches"
    );
    assert_eq!(version, engine.content_version());
    assert_eq!(
        version,
        ZarrEngine::new("rebuild", &config)
            .unwrap()
            .content_version()
    );

    commit_values(&repo, 20.0).await;
    engine.poll_once();
    assert_ne!(
        version,
        engine.content_version(),
        "same-time correction invalidates rendered images"
    );
    assert!(render(&engine)
        .unwrap()
        .values
        .iter_values()
        .all(|v| v == Some(20.0)));
    let old_window = old
        .read_window(&old.vars[0], None, 0, [49.0, 59.1, 51.0, 59.9])
        .unwrap()
        .unwrap();
    assert_eq!(
        old_window.sample(50.0, 59.5),
        Some(10.0),
        "in-flight readers remain pinned"
    );
    let version = engine.content_version();
    engine.poll_once();
    assert_eq!(version, engine.content_version());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pinned_snapshot_and_tag_do_not_follow_branch_commits() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path()).await;
    let snapshot = repo
        .resolve_version(&VersionInfo::BranchTipRef("main".into()))
        .await
        .unwrap();
    repo.create_tag("fixed", &snapshot).await.unwrap();
    let mut pinned_config = config(dir.path());
    let ic = pinned_config.icechunk.as_mut().unwrap();
    ic.branch = None;
    ic.snapshot = Some(snapshot.to_string());
    let pinned = ZarrEngine::new("pinned", &pinned_config).unwrap();
    let ic = pinned_config.icechunk.as_mut().unwrap();
    ic.snapshot = None;
    ic.tag = Some("fixed".into());
    let tagged = ZarrEngine::new("tagged", &pinned_config).unwrap();
    commit_values(&repo, 20.0).await;
    for engine in [pinned, tagged] {
        let version = engine.content_version();
        engine.poll_once();
        assert_eq!(version, engine.content_version());
        assert!(render(&engine)
            .unwrap()
            .values
            .iter_values()
            .all(|v| v == Some(10.0)));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_refresh_keeps_catalog_and_retries_the_same_revision() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path()).await;
    let engine = ZarrEngine::new("retry", &config(dir.path())).unwrap();
    let old = engine.catalog.load_full();
    let writer = Arc::new(AsyncIcechunkStore::new(
        repo.writable_session("main").await.unwrap(),
    ));
    let time = Array::async_open(writer.clone(), "/time").await.unwrap();
    time.async_store_chunk(&[0], vec![24.0f64, 30.0])
        .await
        .unwrap();
    writer
        .session()
        .write()
        .await
        .commit("new times")
        .execute()
        .await
        .unwrap();
    // A temporary backend failure must not advance the published snapshot ID.
    let chunks = dir.path().join("chunks");
    let held = dir.path().join("held-chunks");
    std::fs::rename(&chunks, &held).unwrap();
    engine.poll_once();
    assert!(Arc::ptr_eq(&old, &engine.catalog.load_full()));
    std::fs::rename(held, chunks).unwrap();
    engine.poll_once();
    assert_ne!(old.revision, engine.catalog.load().revision);
    assert_ne!(old.times, engine.catalog.load().times);
}

fn public_aifs_config() -> ZarrConfig {
    ZarrConfig {
        data_path: None,
        endpoint: Some("https://s3.us-west-2.amazonaws.com".into()),
        bucket: Some("dynamical-ecmwf-aifs-single".into()),
        path: Some("ecmwf-aifs-single-forecast/v0.1.0.icechunk".into()),
        zarr_version: None,
        parameters: Some(vec!["temperature_2m".into()]),
        poll_interval_secs: 300,
        cache_mb: 0,
        icechunk: Some(IcechunkConfig {
            decoded_cache_mb: 0,
            branch: Some("main".into()),
            tag: None,
            snapshot: None,
            region: Some("us-west-2".into()),
            force_path_style: Some(true),
        }),
    }
}

/// Compare cold/repeated/panned/animated engine reads at one fixed snapshot.
/// Network and debug-build timings are observations, never CI assertions.
#[ignore = "public S3 access; manual performance probe"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn map_cache_latency_probe() {
    use std::time::Instant;
    let mut config = public_aifs_config();
    let mut baseline = Vec::new();
    for (cache_mb, decoded_mb) in [(0, 0), (256, 0), (256, 256)] {
        config.cache_mb = cache_mb;
        config.icechunk.as_mut().unwrap().decoded_cache_mb = decoded_mb;
        let engine = ZarrEngine::new("cache-probe", &config).unwrap();
        let cat = engine.catalog.load_full();
        let ic = config.icechunk.as_mut().unwrap();
        ic.branch = None;
        ic.snapshot = cat.revision.clone();
        eprintln!(
            "snapshot={:?} cache_mb={cache_mb} decoded_mb={decoded_mb}",
            cat.revision
        );
        let times = engine.get_available_times().unwrap();
        for (i, (name, bbox, time)) in [
            ("cold", [20., 55., 30., 65.], times[0]),
            ("repeat", [20., 55., 30., 65.], times[0]),
            ("pan", [20.1, 55., 30.1, 65.], times[0]),
            ("next lead", [20., 55., 30., 65.], times[1]),
        ]
        .into_iter()
        .enumerate()
        {
            let start = Instant::now();
            let tile = engine
                .get_raster_tile(
                    bbox,
                    128,
                    128,
                    Some(time),
                    &OutputCrs::Wgs84,
                    Some("temperature_2m"),
                    None,
                    None,
                )
                .unwrap();
            let elapsed = start.elapsed();
            let values: Vec<_> = tile.values.iter_values().collect();
            assert!(values.iter().any(Option::is_some));
            eprintln!(
                "cache_mb={cache_mb} decoded_mb={decoded_mb} {name}: {} ms {:?}",
                elapsed.as_millis(),
                engine.decoded_cache_metrics()
            );
            if cache_mb == 0 {
                baseline.push(values);
            } else {
                assert_eq!(values, baseline[i], "cache must not change pixels");
            }
        }
    }
}

/// Four cold inner chunks, fresh clients/caches, alternating measurement order.
#[ignore = "public S3 access; manual performance probe"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chunk_concurrency_latency_probe() {
    use crate::decoded::{DecodedArray, DecodedCache};
    use zarrs::array::ArrayShardedExt;
    let mut config = public_aifs_config();
    let mut baseline = None;
    for parallelism in [1, 4, 4, 1, 1, 4] {
        let source = Source::open("chunk-probe", &config).unwrap();
        let store = source.snapshot(None).unwrap().unwrap();
        let ic = config.icechunk.as_mut().unwrap();
        ic.branch = None;
        ic.snapshot = store.revision.clone();
        let array = Array::open(store.clone(), "/temperature_2m").unwrap();
        let inner = array.effective_subchunk_shape().unwrap();
        let ndim = array.shape().len();
        let mut ranges = vec![0..1; ndim];
        for axis in ndim - 2..ndim {
            ranges[axis] = 0..array.shape()[axis].min(inner[axis].get() + 1);
        }
        let subset = ArraySubset::new_with_ranges(&ranges);
        let cache = Arc::new(DecodedCache::new(0));
        let reader =
            DecodedArray::new(&array, store.revision.as_deref(), "temperature_2m", cache).unwrap();
        let permit = crate::read_budget::BUDGET
            .reserve(&array, &subset, Some(0), true)
            .unwrap();
        assert!(permit.parallelism() >= parallelism);
        let _encoded = crate::encoded::enter(Some(crate::read_budget::BUDGET.clone()));
        let started = std::time::Instant::now();
        let bytes = reader
            .read(
                &array,
                &subset,
                &crate::catalog::single_threaded_opts(),
                parallelism,
            )
            .unwrap()
            .into_fixed()
            .unwrap()
            .into_owned();
        eprintln!(
            "snapshot={:?} subset={ranges:?} parallelism={parallelism} elapsed_ms={}",
            store.revision,
            started.elapsed().as_millis()
        );
        if let Some(expected) = &baseline {
            assert_eq!(
                &bytes, expected,
                "parallelism must not change native values"
            );
        } else {
            baseline = Some(bytes);
        }
    }
}
