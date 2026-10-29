//! Manifests and shard indexes are loaded at catalog build (#1353): after a
//! snapshot swap, a position query's cold read is one request per inner
//! chunk, over storage that makes every request slow.
use super::*;
use crate::{catalog::single_threaded_opts, decoded::DecodedArray, ZarrEngine};
use ds_core::{edr_engine::EdrEngine, map_engine::MapEngine, model::CoverageResponse};
use icechunk::storage::latency::LatencyStorage;
use zarrs::{
    array::{
        codec::{GzipCodec, ShardingCodecBuilder},
        data_type, Array, ArrayBuilder, ArrayBytes, ArraySubset, FromArrayBytes,
    },
    group::GroupBuilder,
};

const VARIABLES: [(&str, f32); 2] = [("msl", 1000.0), ("t2m", 250.0)];
const LATS: [f64; 4] = [60.0, 59.0, 58.0, 57.0];
const LONS: [f64; 4] = [10.0, 11.0, 12.0, 13.0];
/// Every request pays this, as a remote repository's round trip does.
const LATENCY_MS: u64 = 100;

/// `base + 1000·run + lead + 0.1·row + 0.01·col`, plus `shift` after a
/// correction commit.
fn value(base: f32, run: u64, lead: u64, row: u64, col: u64, shift: f32) -> f32 {
    base + 1000.0 * run as f32 + lead as f32 + 0.1 * row as f32 + 0.01 * col as f32 + shift
}

fn field(base: f32, runs: std::ops::Range<u64>, shift: f32) -> Vec<f32> {
    let mut values = Vec::new();
    for run in runs {
        for lead in 0..3 {
            for row in 0..LATS.len() as u64 {
                for col in 0..LONS.len() as u64 {
                    values.push(value(base, run, lead, row, col, shift));
                }
            }
        }
    }
    values
}

/// A forecast repository laid out like the GFS store: one shard per run and
/// longitude half, each holding two inner chunks of every lead.
async fn forecast_repository(dir: &std::path::Path) -> Repository {
    let storage = icechunk::new_local_filesystem_storage(dir).await.unwrap();
    let repo = Repository::create(storage)
        .config(RepositoryConfig {
            inline_chunk_threshold_bytes: Some(0),
            ..Default::default()
        })
        .execute()
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
    let attributes = |json: serde_json::Value| json.as_object().unwrap().clone();
    for (name, values, attrs) in [
        (
            "init_time",
            vec![1_767_225_600.0, 1_767_268_800.0],
            serde_json::json!({
                "units": "seconds since 1970-01-01",
                "standard_name": "forecast_reference_time"
            }),
        ),
        (
            "lead_time",
            vec![0.0, 3600.0, 7200.0],
            serde_json::json!({"units": "seconds", "standard_name": "forecast_period"}),
        ),
        (
            "latitude",
            LATS.to_vec(),
            serde_json::json!({"units": "degrees_north", "standard_name": "latitude"}),
        ),
        (
            "longitude",
            LONS.to_vec(),
            serde_json::json!({"units": "degrees_east", "standard_name": "longitude"}),
        ),
    ] {
        let array = ArrayBuilder::new(
            vec![values.len() as u64],
            vec![values.len() as u64],
            data_type::float64(),
            f64::NAN,
        )
        .dimension_names(Some([name]))
        .attributes(attributes(attrs))
        .build(store.clone(), &format!("/{name}"))
        .unwrap();
        array.async_store_metadata().await.unwrap();
        array.async_store_chunk(&[0], values).await.unwrap();
    }
    for (name, base) in VARIABLES {
        let shard = ShardingCodecBuilder::new(
            [1, 3, 2, 2].map(|n: u64| n.try_into().unwrap()).to_vec(),
            &data_type::float32(),
        )
        .bytes_to_bytes_codecs(vec![Arc::new(GzipCodec::new(1).unwrap())])
        .build();
        let array = ArrayBuilder::new(
            vec![2, 3, 4, 4],
            vec![1, 3, 4, 2],
            data_type::float32(),
            f32::NAN,
        )
        .array_to_bytes_codec(Arc::new(shard))
        .dimension_names(Some(["init_time", "lead_time", "latitude", "longitude"]))
        .attributes(attributes(serde_json::json!({"units": "1"})))
        .build(store.clone(), &format!("/{name}"))
        .unwrap();
        array.async_store_metadata().await.unwrap();
        array
            .async_store_array_subset(
                &ArraySubset::new_with_shape(vec![2, 3, 4, 4]),
                field(base, 0..2, 0.0),
            )
            .await
            .unwrap();
    }
    commit(&store, "fixture").await;
    repo
}

async fn commit(store: &AsyncIcechunkStore, message: &str) {
    store
        .session()
        .write()
        .await
        .commit(message)
        .execute()
        .await
        .unwrap();
}

/// Rewrite the latest run of every variable, so its chunks are new objects.
async fn correct_latest_run(repo: &Repository, shift: f32) {
    let store = Arc::new(AsyncIcechunkStore::new(
        repo.writable_session("main").await.unwrap(),
    ));
    for (name, base) in VARIABLES {
        let array = Array::async_open(store.clone(), &format!("/{name}"))
            .await
            .unwrap();
        array
            .async_store_array_subset(
                &ArraySubset::new_with_ranges(&[1..2, 0..3, 0..4, 0..4]),
                field(base, 1..2, shift),
            )
            .await
            .unwrap();
    }
    commit(&store, "correct the latest run").await;
}

fn config(dir: &std::path::Path) -> ZarrConfig {
    let mut config = ZarrConfig::auto_local(dir.to_string_lossy().into_owned());
    config.cache_mb = 4;
    config.icechunk = Some(IcechunkConfig {
        decoded_cache_mb: 4,
        branch: Some("main".into()),
        tag: None,
        snapshot: None,
        region: None,
        force_path_style: None,
    });
    config
}

fn slow_engine(id: &str, config: &ZarrConfig) -> ZarrEngine {
    let source = Source::open_with(id, config, |backend| {
        Arc::new(LatencyStorage::new(backend, 0, LATENCY_MS))
    })
    .unwrap();
    ZarrEngine::with_source(
        id,
        config,
        crate::source::Source::Icechunk(Box::new(source)),
    )
    .unwrap()
}

fn meter(engine: &ZarrEngine) -> &Meter {
    match &engine.source {
        crate::source::Source::Icechunk(source) => source.meter(),
        crate::source::Source::Plain(_) => unreachable!("an Icechunk engine"),
    }
}

/// The values a position between the first two latitudes and longitudes
/// reads: the mean of its four cells, all in inner chunk (run, 0, 0, 0).
fn expected_series(base: f32, shift: f32) -> Vec<f64> {
    (0..3)
        .map(|lead| {
            [(0, 0), (0, 1), (1, 0), (1, 1)]
                .map(|(row, col)| f64::from(value(base, 1, lead, row, col, shift)))
                .iter()
                .sum::<f64>()
                / 4.0
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn after_a_snapshot_swap_a_cold_position_reads_only_its_inner_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let repo = forecast_repository(dir.path()).await;
    let engine = slow_engine("preload", &config(dir.path()));
    let version = engine.content_version();
    meter(&engine).take_log();

    correct_latest_run(&repo, 0.5).await;
    engine.poll_once();
    assert_ne!(
        version,
        engine.content_version(),
        "the correction is published"
    );
    let swap = meter(&engine).take_log();
    assert!(
        swap.iter().any(|(path, _)| path.starts_with("manifests/")),
        "catalog build loads the new manifests: {swap:?}"
    );

    let (requests, bytes) = engine.icechunk_storage_counts().unwrap();
    let misses = engine.icechunk_payload_cache_metrics().unwrap().misses;
    let names: Vec<String> = VARIABLES.iter().map(|(name, _)| name.to_string()).collect();
    let started = std::time::Instant::now();
    let CoverageResponse::Single(result) = engine
        .query_position("POINT(10.5 59.5)", None, Some(&names), None, None)
        .unwrap()
    else {
        panic!("single coverage")
    };
    let elapsed = started.elapsed();

    // One request per variable, for its one cold inner chunk: no manifest,
    // no shard index. Before #1353 each variable paid all three in turn.
    let log = meter(&engine).take_log();
    assert_eq!(log.len(), VARIABLES.len(), "{log:?}");
    let mut fetched = 0;
    for (path, range) in &log {
        assert!(path.starts_with("chunks/"), "{path}");
        let range = range.clone().expect("a ranged chunk read");
        fetched += range.end - range.start;
    }
    let (after_requests, after_bytes) = engine.icechunk_storage_counts().unwrap();
    assert_eq!(after_requests - requests, VARIABLES.len() as u64);
    assert_eq!(after_bytes - bytes, fetched);
    assert_eq!(
        engine.icechunk_payload_cache_metrics().unwrap().misses - misses,
        VARIABLES.len() as u64
    );
    assert!(
        elapsed.as_millis() >= u128::from(LATENCY_MS),
        "the reads paid the latency: {elapsed:?}"
    );

    for (name, base) in VARIABLES {
        let values = &result.ranges[name].values;
        let expected = expected_series(base, 0.5);
        assert_eq!(values.len(), expected.len());
        for (value, expected) in values.iter().zip(expected) {
            assert!(
                (value.unwrap() - expected).abs() < 1e-3,
                "{name}: {value:?} vs {expected}"
            );
        }
    }

    // Read again: the decoded chunks answer, with no request at all.
    engine
        .query_position("POINT(10.5 59.5)", None, Some(&names), None, None)
        .unwrap();
    assert!(meter(&engine).take_log().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resident_shard_reads_match_the_upstream_subset_read_bit_for_bit() {
    let dir = tempfile::tempdir().unwrap();
    let repo = forecast_repository(dir.path()).await;
    correct_latest_run(&repo, 0.25).await;
    let source = Source::open("bits", &config(dir.path())).unwrap();
    let store = source.snapshot(None).unwrap().unwrap();
    let options = single_threaded_opts();
    for (name, _) in VARIABLES {
        let array = crate::codec_limits::bounded_array(
            Array::open(store.clone(), &format!("/{name}")).unwrap(),
        )
        .unwrap();
        let decoded = DecodedArray::new(
            &array,
            store.revision.as_deref(),
            name,
            Arc::new(crate::decoded::DecodedCache::new(0)),
        )
        .unwrap();
        let shards = decoded
            .shard_indexes()
            .expect("an exclusively sharded array");
        let grid = zarrs::array::ArrayShardedExt::subchunk_grid(&array);
        let mut subsets: Vec<ArraySubset> = grid
            .chunks_in_array_subset(&array.subset_all())
            .unwrap()
            .unwrap()
            .indices()
            .into_iter()
            .map(|chunk| grid.subset(&chunk).unwrap().unwrap())
            .collect();
        // A partial inner chunk, as an uncached read of an oversized one.
        subsets.push(ArraySubset::new_with_ranges(&[1..2, 1..3, 1..2, 2..4]));
        for subset in &subsets {
            let resident = shards
                .read(&array, subset, &options)
                .unwrap()
                .expect("a subset within one shard");
            let upstream = array
                .retrieve_array_subset_opt::<ArrayBytes<'static>>(subset, &options)
                .unwrap()
                .into_fixed()
                .unwrap()
                .into_owned();
            assert_eq!(resident, upstream, "{name} {subset:?}");
            assert!(Vec::<f32>::from_array_bytes(
                ArrayBytes::new_flen(resident),
                subset.shape(),
                array.data_type()
            )
            .unwrap()
            .iter()
            .all(|v| v.is_finite()));
        }
        // Two runs, two longitude halves.
        assert_eq!(shards.len(), 4, "{name}");
        // A whole shard keeps upstream's single read.
        assert!(shards
            .read(
                &array,
                &ArraySubset::new_with_ranges(&[0..1, 0..3, 0..4, 0..2]),
                &options
            )
            .unwrap()
            .is_none());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catalog_build_loads_the_default_runs_shard_indexes() {
    let dir = tempfile::tempdir().unwrap();
    forecast_repository(dir.path()).await;
    let engine = ZarrEngine::new("default-run", &config(dir.path())).unwrap();
    let catalog = engine.catalog.load();
    let run = catalog.resolve_run(None).unwrap();
    assert_eq!(run, Some(1), "the latest run is the default");
    let halves = vec![vec![1, 0, 0, 0], vec![1, 0, 0, 1]];
    assert_eq!(
        catalog.preloaded_shards(),
        vec![halves; VARIABLES.len()],
        "both longitude halves of run 1, none of run 0"
    );
}
