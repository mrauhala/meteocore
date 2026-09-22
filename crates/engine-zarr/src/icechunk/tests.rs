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
    .bytes_to_bytes_codecs(vec![Arc::new(GzipCodec::new(1).unwrap())])
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
        branch: Some("main".into()),
        tag: None,
        snapshot: None,
        region: None,
        force_path_style: None,
    });
    config
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
async fn concurrent_maps_read_icechunk_through_render_executor() {
    // Maps, WMS, and Tiles all use acquire_raster + RenderJob::run. Exercise
    // actual async file/range reads and shard decoding inside spawn_blocking,
    // with more requests than render slots and both cold and warm payloads.
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path()).await;
    for cache_mb in [0, 4] {
        let mut config = config(dir.path());
        config.cache_mb = cache_mb;
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
    }
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
    let config = config(dir.path());
    let engine = ZarrEngine::new("refresh", &config).unwrap();
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

/// Compare cold/repeated/panned/animated engine reads at one fixed snapshot.
/// Network and debug-build timings are observations, never CI assertions.
#[ignore = "public S3 access; manual performance probe"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn map_cache_latency_probe() {
    use std::time::Instant;
    let mut config = ZarrConfig {
        data_path: None,
        endpoint: Some("https://s3.us-west-2.amazonaws.com".into()),
        bucket: Some("dynamical-ecmwf-aifs-single".into()),
        path: Some("ecmwf-aifs-single-forecast/v0.1.0.icechunk".into()),
        zarr_version: None,
        parameters: Some(vec!["temperature_2m".into()]),
        poll_interval_secs: 300,
        cache_mb: 0,
        icechunk: Some(IcechunkConfig {
            branch: Some("main".into()),
            tag: None,
            snapshot: None,
            region: Some("us-west-2".into()),
            force_path_style: Some(true),
        }),
    };
    let mut baseline = Vec::new();
    for cache_mb in [0, 256] {
        config.cache_mb = cache_mb;
        let engine = ZarrEngine::new("cache-probe", &config).unwrap();
        let cat = engine.catalog.load_full();
        let ic = config.icechunk.as_mut().unwrap();
        ic.branch = None;
        ic.snapshot = cat.revision.clone();
        eprintln!("snapshot={:?} cache_mb={cache_mb}", cat.revision);
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
            eprintln!("cache_mb={cache_mb} {name}: {} ms", elapsed.as_millis());
            if cache_mb == 0 {
                baseline.push(values);
            } else {
                assert_eq!(values, baseline[i], "cache must not change pixels");
            }
        }
    }
}
