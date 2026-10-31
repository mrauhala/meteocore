//! A query's cold inner chunks are fetched together (#1353), and only where
//! a decode then reads each of them as the one byte range its shard index
//! gives, under the payload cache's key for it.
use std::time::{Duration, Instant};

use super::preload_tests::{
    config, correct_latest_run, expected_series, forecast_repository, forecast_repository_with,
    meter, value, LATENCY_MS, LATS, VARIABLES,
};
use super::*;
use crate::{
    catalog::single_threaded_opts,
    decoded::{DecodedArray, DecodedCache},
    read_budget::Budget,
    ReadAhead, ZarrEngine,
};
use ds_core::{edr_engine::EdrEngine, map_engine::MapEngine, model::CoverageResponse};
use icechunk::storage::latency::LatencyStorage;
use zarrs::array::{
    codec::{
        api::BytesToBytesCodecTraits, BloscCodec, BloscCompressor, BloscShuffleMode, Crc32cCodec,
        ShardingCodec,
    },
    Array, ArraySubset, FromArrayBytes,
};

/// Five longitudes: the third shard reaches one column past the grid.
const LONS: [f64; 5] = [10.0, 11.0, 12.0, 13.0, 14.0];
/// One more variable than the fetches in flight at once.
const NINE: [(&str, f32); 9] = [
    ("v1", 100.0),
    ("v2", 200.0),
    ("v3", 300.0),
    ("v4", 400.0),
    ("v5", 500.0),
    ("v6", 600.0),
    ("v7", 700.0),
    ("v8", 800.0),
    ("v9", 900.0),
];

fn blosc() -> Arc<dyn BytesToBytesCodecTraits> {
    Arc::new(
        BloscCodec::new(
            BloscCompressor::LZ4,
            5.try_into().unwrap(),
            None,
            BloscShuffleMode::Shuffle,
            Some(4),
        )
        .unwrap(),
    )
}

/// An engine whose storage answers each request after a latency that
/// starts at zero, so the catalog builds at once, and that the test then
/// raises.
fn engine(id: &str, config: &ZarrConfig) -> (ZarrEngine, Arc<LatencyStorage>) {
    let mut latency = None;
    let source = Source::open_with(id, config, |backend| {
        let storage = Arc::new(LatencyStorage::new(backend, 0, 0));
        latency = Some(storage.clone());
        storage as Arc<dyn Storage + Send + Sync>
    })
    .unwrap();
    let engine = ZarrEngine::with_source(
        id,
        config,
        crate::source::Source::Icechunk(Box::new(source)),
    )
    .unwrap();
    (engine, latency.unwrap())
}

/// A new session of the engine's repository, at its published snapshot.
fn session(engine: &ZarrEngine) -> Arc<EngineStore> {
    match &engine.source {
        crate::source::Source::Icechunk(source) => source.snapshot(None).unwrap().unwrap(),
        crate::source::Source::Plain(_) => unreachable!("an Icechunk engine"),
    }
}

fn read_ahead(engine: &ZarrEngine) -> ReadAhead {
    engine.icechunk_read_ahead().unwrap()
}

fn series(result: &CoverageResponse, name: &str) -> Vec<f64> {
    let CoverageResponse::Single(result) = result else {
        panic!("single coverage")
    };
    result.ranges[name]
        .values
        .iter()
        .map(|v| v.unwrap())
        .collect()
}

fn assert_close(actual: &[f64], expected: &[f64]) {
    assert_eq!(actual.len(), expected.len());
    for (actual, expected) in actual.iter().zip(expected) {
        assert!((actual - expected).abs() < 1e-3, "{actual} vs {expected}");
    }
}

/// Each inner chunk's `(offset, length)` in its shard, from the shard
/// index as upstream decodes it from the whole shard object.
fn index_entries(store: &Arc<EngineStore>, name: &str, shard: &[u64]) -> Vec<(u64, u64)> {
    let array = Array::open(store.clone(), &format!("/{name}")).unwrap();
    let codec = array.codecs();
    let sharding = codec
        .array_to_bytes_codec()
        .as_any()
        .downcast_ref::<ShardingCodec>()
        .expect("a sharded array");
    let encoded = store.get(&array.chunk_key(shard)).unwrap().unwrap();
    let per_shard = [1u64, 1, 2, 1].map(|n| n.try_into().unwrap());
    let index = sharding
        .decode_index(&encoded, &per_shard, &single_threaded_opts())
        .unwrap();
    index.chunks(2).map(|entry| (entry[0], entry[1])).collect()
}

/// Read the coordinate arrays, as a catalog build does. A new session
/// preloads their manifests in the background (Icechunk's default
/// `preload_if`); reading them first leaves nothing for that to fetch while
/// a test counts requests.
fn read_coordinates(store: &Arc<EngineStore>) {
    for name in ["init_time", "lead_time", "latitude", "longitude"] {
        let array = Array::open(store.clone(), &format!("/{name}")).unwrap();
        array
            .retrieve_array_subset_opt::<Vec<f64>>(&array.subset_all(), &single_threaded_opts())
            .unwrap();
    }
}

/// The check the prefetch rests on: a cold Blosc decode of an inner chunk
/// reads it as exactly its shard index entry, edge chunks included, and a
/// partial decode of it asks for the same range, which the payload cache
/// then answers. The resident index the prefetch plans from agrees.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cold_blosc_decode_reads_each_inner_chunk_as_one_range_of_its_index() {
    let dir = tempfile::tempdir().unwrap();
    forecast_repository_with(dir.path(), &[("msl", 1000.0)], &LONS, blosc()).await;
    let mut oracle = config(dir.path());
    oracle.cache_mb = 0;
    let oracle = Source::open("oracle", &oracle).unwrap();
    let oracle = oracle.snapshot(None).unwrap().unwrap();
    let source = Source::open("blosc", &config(dir.path())).unwrap();
    let store = source.snapshot(None).unwrap().unwrap();
    read_coordinates(&store);
    let array =
        crate::codec_limits::bounded_array(Array::open(store.clone(), "/msl").unwrap()).unwrap();
    let options = single_threaded_opts();
    // A reader that keeps whole inner chunks, and one that keeps none and
    // so decodes only a position's corner cells of each.
    let readers = [ds_cache::MIB, 0].map(|capacity| {
        DecodedArray::new(
            &array,
            store.revision.as_deref(),
            "msl",
            Arc::new(DecodedCache::new(capacity)),
        )
        .unwrap()
    });
    let shards: Vec<Vec<u64>> = (0..3).map(|lon| vec![1, 0, 0, lon]).collect();
    for reader in &readers {
        for shard in &shards {
            reader
                .shard_indexes()
                .unwrap()
                .load(&array, shard, &options)
                .unwrap();
        }
    }
    source.meter().take_log();
    let misses = source.meter().payload_metrics().misses;
    let read = |reader: &DecodedArray, subset: &ArraySubset| {
        Vec::<f32>::from_array_bytes(
            reader.read(&array, subset, &options, 1).unwrap(),
            subset.shape(),
            array.data_type(),
        )
        .unwrap()
    };
    for (s, shard) in shards.iter().enumerate() {
        let entries = index_entries(&oracle, "msl", shard);
        assert_eq!(entries.len(), LATS.len() / 2, "two inner chunks per shard");
        for (inner, &(offset, length)) in entries.iter().enumerate() {
            let rows = inner as u64 * 2..inner as u64 * 2 + 2;
            let cols = shard[3] * 2..(shard[3] * 2 + 2).min(LONS.len() as u64);
            let chunk = ArraySubset::new_with_ranges(&[1..2, 0..3, rows.clone(), cols.clone()]);
            let planned = readers[0].cold_ranges(&array, &chunk);
            assert_eq!(planned.len(), 1, "shard {s} inner {inner}");
            assert!(planned[0].key == array.chunk_key(shard));
            assert_eq!(
                (planned[0].offset, planned[0].length),
                (offset, length),
                "shard {s} inner {inner}: the resident index is upstream's"
            );

            let values = read(&readers[0], &chunk);
            let log = source.meter().take_log();
            assert_eq!(log.len(), 1, "shard {s} inner {inner}: {log:?}");
            let (path, range) = &log[0];
            assert!(path.starts_with("chunks/"), "{path}");
            assert_eq!(
                range.clone(),
                Some(offset..offset + length),
                "shard {s} inner {inner}: the index entry, nothing more or less"
            );
            let mut expected = Vec::new();
            for lead in 0..3 {
                for row in rows.clone() {
                    for col in cols.clone() {
                        expected.push(value(1000.0, 1, lead, row, col, 0.0));
                    }
                }
            }
            assert_eq!(values, expected);
            assert!(
                readers[0].cold_ranges(&array, &chunk).is_empty(),
                "a decoded chunk is not fetched again"
            );

            // A position's corner cells, decoded from the same range: the
            // payload cache answers under the key the whole chunk filled.
            let corner =
                ArraySubset::new_with_ranges(&[1..2, 1..3, rows.start..rows.start + 1, cols]);
            let hits = source.meter().payload_metrics().hits;
            read(&readers[1], &corner);
            let log = source.meter().take_log();
            assert!(log.is_empty(), "shard {s} inner {inner}: {log:?}");
            assert_eq!(source.meter().payload_metrics().hits, hits + 1);
        }
    }
    assert_eq!(
        source.meter().payload_metrics().misses - misses,
        (shards.len() * 2) as u64
    );
}

/// A checksum outside the compression is read as the parts a decode
/// wants, so such chunks are never fetched ahead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checksummed_inner_chunks_are_left_to_their_reads() {
    let dir = tempfile::tempdir().unwrap();
    forecast_repository_with(
        dir.path(),
        &[("msl", 1000.0)],
        &LONS,
        Arc::new(Crc32cCodec::new()),
    )
    .await;
    let source = Source::open("crc", &config(dir.path())).unwrap();
    let store = source.snapshot(None).unwrap().unwrap();
    let array =
        crate::codec_limits::bounded_array(Array::open(store.clone(), "/msl").unwrap()).unwrap();
    let reader = DecodedArray::new(
        &array,
        store.revision.as_deref(),
        "msl",
        Arc::new(DecodedCache::new(ds_cache::MIB)),
    )
    .unwrap();
    reader
        .shard_indexes()
        .unwrap()
        .load(&array, &[1, 0, 0, 0], &single_threaded_opts())
        .unwrap();
    assert!(reader
        .cold_ranges(
            &array,
            &ArraySubset::new_with_ranges(&[1..2, 0..3, 0..4, 0..2])
        )
        .is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cold_position_fetches_its_variables_inner_chunks_eight_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    forecast_repository_with(dir.path(), &NINE, &LONS[..4], blosc()).await;
    let (engine, latency) = engine("nine", &config(dir.path()));
    latency.set_read_delay_ms(LATENCY_MS);
    let meter = meter(&engine);
    meter.take_log();
    let before = engine.icechunk_payload_cache_metrics().unwrap();
    let (hits, misses) = (before.hits, before.misses);
    let names: Vec<String> = NINE.iter().map(|(name, _)| name.to_string()).collect();

    let started = Instant::now();
    let result = engine
        .query_position("POINT(10.5 59.5)", None, Some(&names), None, None)
        .unwrap();
    let elapsed = started.elapsed();

    // One request per variable's cold inner chunk, and nothing else.
    let issued = meter.issued();
    let log = meter.take_log();
    assert_eq!(log.len(), NINE.len(), "{log:?}");
    assert!(log
        .iter()
        .all(|(path, range)| path.starts_with("chunks/") && range.is_some()));
    // Eight go out together; the ninth once one of them has answered.
    let latency = Duration::from_millis(LATENCY_MS);
    assert!(
        issued[..8]
            .iter()
            .all(|at| at.duration_since(issued[0]) < latency / 2),
        "{issued:?}"
    );
    assert!(issued[8].duration_since(issued[0]) >= latency * 9 / 10);
    // Two round trips, where reading one variable after another took nine.
    assert!(elapsed >= latency * 2, "{elapsed:?}");
    assert!(elapsed < latency * 5, "{elapsed:?}");

    for (name, base) in NINE {
        assert_close(&series(&result, name), &expected_series(base, 0.0));
    }
    // Each decode missed the payload cache and took its staged range.
    let payload = engine.icechunk_payload_cache_metrics().unwrap();
    assert_eq!(payload.misses - misses, NINE.len() as u64);
    assert_eq!(payload.hits, hits, "no decode counted a hit");
    assert_eq!(
        read_ahead(&engine),
        ReadAhead {
            used: NINE.len() as u64,
            ..Default::default()
        }
    );

    // Decoded now: no request, and nothing to fetch ahead.
    engine
        .query_position("POINT(10.5 59.5)", None, Some(&names), None, None)
        .unwrap();
    assert!(meter.take_log().is_empty());
    assert_eq!(read_ahead(&engine).used, NINE.len() as u64);
}

/// Ranges fetched ahead are held memory until their decodes take them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staged_ranges_are_metered_until_their_decodes_take_them() {
    let dir = tempfile::tempdir().unwrap();
    forecast_repository_with(dir.path(), &NINE, &LONS[..4], blosc()).await;
    let (engine, _) = engine("staged", &config(dir.path()));
    assert_eq!(engine.icechunk_staged_bytes(), Some(0));
    let catalog = engine.catalog.load();
    let run = catalog.resolve_run(None).unwrap();
    let steps: Vec<usize> = (0..3).collect();
    let reads: Vec<_> = catalog
        .vars
        .iter()
        .map(|var| {
            let read = catalog.position_read(var, run, 10.5, 59.5, &steps).unwrap();
            (var, read.unwrap().subset)
        })
        .collect();
    catalog.prefetch(&reads);
    let staged = engine.icechunk_staged_bytes().unwrap();
    // Nine compressed ranges, each weighed with its key as the cache does.
    assert!(staged > 9 * 80, "{staged}");
    assert!(staged <= meter(&engine).prefetch_room());
    let names: Vec<String> = NINE.iter().map(|(name, _)| name.to_string()).collect();
    engine
        .query_position("POINT(10.5 59.5)", None, Some(&names), None, None)
        .unwrap();
    assert_eq!(engine.icechunk_staged_bytes(), Some(0), "all taken");
    assert_eq!(read_ahead(&engine).used, NINE.len() as u64);
}

/// Fetched ahead or read one variable at a time, the payload cache ends up
/// the same: the same misses, entries and churn, and no hits. Asked again,
/// with no decoded cache in front, both answer from it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ranges_fetched_ahead_fill_the_payload_cache_as_serial_reads_do() {
    let dir = tempfile::tempdir().unwrap();
    forecast_repository_with(dir.path(), &NINE, &LONS[..4], blosc()).await;
    let mut config = config(dir.path());
    config.icechunk.as_mut().unwrap().decoded_cache_mb = 0;
    let (ahead, _) = engine("ahead", &config);
    let (serial, _) = engine("serial", &config);
    let names: Vec<String> = NINE.iter().map(|(name, _)| name.to_string()).collect();
    let before = ahead.icechunk_payload_cache_metrics().unwrap();
    assert_eq!(before, serial.icechunk_payload_cache_metrics().unwrap());
    for round in 0..2 {
        meter(&ahead).take_log();
        meter(&serial).take_log();
        ahead
            .query_position("POINT(10.5 59.5)", None, Some(&names), None, None)
            .unwrap();
        for name in &names {
            serial
                .query_position(
                    "POINT(10.5 59.5)",
                    None,
                    Some(std::slice::from_ref(name)),
                    None,
                    None,
                )
                .unwrap();
        }
        let cache = ahead.icechunk_payload_cache_metrics().unwrap();
        assert_eq!(
            cache,
            serial.icechunk_payload_cache_metrics().unwrap(),
            "round {round}"
        );
        let nine = NINE.len() as u64;
        let requests = meter(&ahead).take_log().len();
        if round == 0 {
            assert_eq!((cache.hits, cache.misses - before.misses), (0, nine));
            assert_eq!(requests, NINE.len());
        } else {
            assert_eq!((cache.hits, cache.misses - before.misses), (nine, nine));
            assert_eq!(requests, 0);
            assert!(meter(&serial).take_log().is_empty());
        }
    }
    assert_eq!(
        read_ahead(&ahead),
        ReadAhead {
            used: NINE.len() as u64,
            ..Default::default()
        }
    );
    assert_eq!(read_ahead(&serial), ReadAhead::default());
}

/// EDR runs queries on blocking workers too, under the request's deadline:
/// the fetches there go out together as well, and stop at the deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_prefetch_on_a_blocking_worker_keeps_the_request_deadline() {
    let dir = tempfile::tempdir().unwrap();
    forecast_repository_with(dir.path(), &NINE, &LONS[..4], blosc()).await;
    let (engine, latency) = engine("deadline", &config(dir.path()));
    latency.set_read_delay_ms(LATENCY_MS);
    let engine = Arc::new(engine);
    let names: Vec<String> = NINE.iter().map(|(name, _)| name.to_string()).collect();
    let query = |engine: Arc<ZarrEngine>, names: Vec<String>, budget: Duration| {
        tokio::task::spawn_blocking(move || {
            let _deadline = ds_core::deadline::enter(Some(Instant::now() + budget));
            let started = Instant::now();
            let result = engine.query_position("POINT(10.5 59.5)", None, Some(&names), None, None);
            (result, started.elapsed())
        })
    };
    let latency = Duration::from_millis(LATENCY_MS);

    // Out of time inside the first round trip: the query ends there.
    let (result, elapsed) = query(engine.clone(), names.clone(), latency / 2)
        .await
        .unwrap();
    assert!(
        matches!(result, Err(DataServerError::DeadlineExceeded)),
        "{result:?}"
    );
    assert!(elapsed < latency, "{elapsed:?}");

    meter(&engine).take_log();
    let (result, _) = query(engine.clone(), names.clone(), Duration::from_secs(10))
        .await
        .unwrap();
    let result = result.unwrap();
    let issued = meter(&engine).issued();
    // Whatever the cut-off fetches left in the payload cache needs no
    // request now; the rest go out eight at a time.
    assert!(issued.len() <= NINE.len(), "{issued:?}");
    assert!(issued
        .iter()
        .take(8)
        .all(|at| at.duration_since(issued[0]) < latency / 2));
    for (name, base) in NINE {
        assert_close(&series(&result, name), &expected_series(base, 0.0));
    }
}

/// With the read budget taken by other reads, nothing is fetched ahead and
/// nothing fails: each decode reads its chunk itself, one after another,
/// as before #1353.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_read_budget_to_spare_every_chunk_is_left_to_its_serial_read() {
    let dir = tempfile::tempdir().unwrap();
    forecast_repository_with(dir.path(), &NINE, &LONS[..4], blosc()).await;
    let (engine, latency) = engine("budget", &config(dir.path()));
    // A catalog of the engine's repository, on a budget of this test's.
    let budget = Arc::new(Budget::new(64 * ds_cache::MIB));
    let catalog = crate::catalog::build(
        session(&engine),
        "budget",
        None,
        Arc::new(DecodedCache::new(4 * ds_cache::MIB)),
    )
    .unwrap()
    .with_read_budget(budget.clone());
    assert!(!catalog.preload("budget", crate::catalog::PRELOAD_BOUND));
    let run = catalog.resolve_run(None).unwrap();
    let steps: Vec<usize> = (0..3).collect();
    let reads: Vec<_> = catalog
        .vars
        .iter()
        .map(|var| {
            let read = catalog
                .position_read(var, run, 10.5, 59.5, &steps)
                .unwrap()
                .unwrap();
            (var, read.subset)
        })
        .collect();
    latency.set_read_delay_ms(LATENCY_MS);
    let meter = meter(&engine);
    meter.take_log();

    // Other reads hold all but a byte of the budget.
    let held = budget.reserve_bytes(64 * ds_cache::MIB - 1).unwrap();
    catalog.prefetch(&reads);
    assert!(meter.take_log().is_empty(), "nothing fetched ahead");
    assert_eq!(
        read_ahead(&engine),
        ReadAhead {
            serial: NINE.len() as u64,
            ..Default::default()
        }
    );
    assert_eq!(budget.metrics().2, 0, "a prefetch rejects no request");

    drop(held);
    let started = Instant::now();
    for (var, _) in &reads {
        let base = NINE.iter().find(|(name, _)| *name == var.name).unwrap().1;
        let values = catalog.sample_series(var, run, 10.5, 59.5, &steps).unwrap();
        let values: Vec<f64> = values.into_iter().map(Option::unwrap).collect();
        assert_close(&values, &expected_series(base, 0.0));
    }
    let latency = Duration::from_millis(LATENCY_MS);
    let issued = meter.issued();
    assert_eq!(meter.take_log().len(), NINE.len());
    assert!(issued
        .windows(2)
        .all(|pair| pair[1].duration_since(pair[0]) >= latency * 9 / 10));
    assert!(started.elapsed() >= latency * NINE.len() as u32);
    assert_eq!(budget.metrics(), (0, 64 * ds_cache::MIB, 0));
}

/// MULTIPOINT, area and trajectory plan from the reads they sample: both
/// variables' cold inner chunks come in one round trip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multipoint_area_and_trajectory_fetch_their_cold_inner_chunks_together() {
    let dir = tempfile::tempdir().unwrap();
    forecast_repository(dir.path()).await;
    let names: Vec<String> = VARIABLES.iter().map(|(name, _)| name.to_string()).collect();
    let latency = Duration::from_millis(LATENCY_MS);
    // The points read rows 0..2 of both longitude shards: two inner chunks
    // per variable. The windows add a cell around their bbox, reaching
    // row 2: four.
    let multipoint = |engine: &ZarrEngine| {
        let points = ["POINT(10.5 59.5)".to_owned(), "POINT(12.5 59.5)".to_owned()];
        let mut emitted = Vec::new();
        engine
            .query_positions(&points, None, Some(&names), None, None, &mut |coverage| {
                emitted.push(coverage);
                Ok(())
            })
            .unwrap();
        assert_eq!(emitted.len(), 2);
        for (name, base) in VARIABLES {
            assert_close(&series(&emitted[0], name), &expected_series(base, 0.0));
        }
    };
    let area = |engine: &ZarrEngine| {
        engine
            .query_area(
                "POLYGON((10.2 59.2,12.8 59.2,12.8 59.8,10.2 59.8,10.2 59.2))",
                None,
                Some(&names),
                None,
                None,
            )
            .unwrap();
    };
    let trajectory = |engine: &ZarrEngine| {
        engine
            .query_trajectory(
                "LINESTRING(10.5 59.5,12.5 59.5)",
                None,
                Some(&names),
                None,
                None,
            )
            .unwrap();
    };
    type Query<'a> = &'a dyn Fn(&ZarrEngine);
    let queries: [(&str, Query, u64); 3] = [
        ("multipoint", &multipoint, 4),
        ("area", &area, 8),
        ("trajectory", &trajectory, 8),
    ];
    for (kind, query, chunks) in queries {
        let (engine, storage) = engine(kind, &config(dir.path()));
        storage.set_read_delay_ms(LATENCY_MS);
        let meter = meter(&engine);
        meter.take_log();
        query(&engine);
        // All issued at once, before any answered. (Not timed end to end:
        // a window's decodes share the four-thread chunk pool with every
        // other test running.)
        let issued = meter.issued();
        let log = meter.take_log();
        assert_eq!(log.len() as u64, chunks, "{kind}: {log:?}");
        assert!(
            issued
                .iter()
                .all(|at| at.duration_since(issued[0]) < latency / 2),
            "{kind}: {issued:?}"
        );
        assert_eq!(read_ahead(&engine).used, chunks, "{kind}");
    }
}

/// Storage slower than the preload's bound: the build stops loading there
/// and the new catalog publishes, its reads fetching the rest themselves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_preload_cut_off_by_slow_storage_still_publishes_the_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let repo = forecast_repository(dir.path()).await;
    let (mut engine, latency) = engine("bound", &config(dir.path()));
    let bound = Duration::from_millis(30);
    engine.preload_bound = bound;
    assert_eq!(read_ahead(&engine).preloads_stopped, 0);
    let version = engine.content_version();
    correct_latest_run(&repo, 0.5).await;
    latency.set_read_delay_ms(LATENCY_MS);

    // Loading a shard index of the new run takes its manifest, then the
    // index: two round trips. The bound ends the load inside the first.
    let catalog = crate::catalog::build(
        session(&engine),
        "bound",
        None,
        Arc::new(DecodedCache::new(4 * ds_cache::MIB)),
    )
    .unwrap();
    let started = Instant::now();
    assert!(catalog.preload("bound", bound), "stopped at its bound");
    let elapsed = started.elapsed();
    assert!(elapsed >= bound, "{elapsed:?}");
    assert!(
        elapsed < Duration::from_millis(LATENCY_MS) * 2,
        "{elapsed:?}: never waits for a load to finish"
    );
    drop(catalog);

    engine.poll_once();
    assert_ne!(
        version,
        engine.content_version(),
        "the correction publishes"
    );
    assert_eq!(read_ahead(&engine).preloads_stopped, 1);
    let resident: usize = engine
        .catalog
        .load()
        .preloaded_shards()
        .iter()
        .map(Vec::len)
        .sum();
    assert!(resident < 2 * VARIABLES.len(), "{resident} shards loaded");

    let names: Vec<String> = VARIABLES.iter().map(|(name, _)| name.to_string()).collect();
    let result = engine
        .query_position("POINT(10.5 59.5)", None, Some(&names), None, None)
        .unwrap();
    for (name, base) in VARIABLES {
        assert_close(&series(&result, name), &expected_series(base, 0.5));
    }
}
