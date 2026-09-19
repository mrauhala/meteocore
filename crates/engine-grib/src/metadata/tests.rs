use super::*;
use crate::test_support::{message, store::TestStore, TestSource};
use crate::{
    catalog::{Catalog, ForecastRun, StepFile},
    reader, GribEngine,
};
use ds_core::edr_engine::EdrEngine;
use ds_storage::object_store::{GetRange, ObjectStoreExt};
use std::{
    collections::BTreeMap,
    sync::{atomic::Ordering, Arc},
};

const ECMWF: &[u8] = include_bytes!("../../../../testdata/grib-local/sample-message.grib2");

fn entry(length: Option<usize>) -> MessageEntry {
    MessageEntry {
        source_url: None,
        param: "TMP".into(),
        levtype: "sfc".into(),
        level: None,
        offset: 0,
        length: length.map(|n| n as u64),
        step_kind: crate::wgrib2_index::StepKind::Instant,
    }
}

async fn source(bytes: &[u8]) -> (Arc<TestStore>, DataStore, Path) {
    let store = Arc::new(TestStore {
        suffix: ".grib2",
        ..Default::default()
    });
    let path = Path::from("field.grib2");
    store.inner.put(&path, bytes.to_vec().into()).await.unwrap();
    let data = DataStore::new(store.clone());
    (store, data, path)
}

fn fix_length(bytes: &mut [u8]) {
    let length = bytes.len() as u64;
    bytes[8..16].copy_from_slice(&length.to_be_bytes());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headers_match_real_decode_with_explicit_and_unknown_tail_lengths() {
    let expected = MessageMetadata::from(&reader::decode_message(ECMWF, "TMP").unwrap());
    for length in [Some(ECMWF.len()), None] {
        let mut bytes = vec![0; 37];
        bytes.extend_from_slice(ECMWF);
        let (observed, store, path) = source(&bytes).await;
        let mut entry = entry(length);
        entry.offset = 37;
        assert_eq!(read_metadata(&store, &path, &entry).unwrap(), expected);
        assert_eq!(store.bytes_read(), READ_AHEAD as u64);
        let reads = observed.reads.lock().unwrap();
        assert_eq!(reads.attempts[path.as_ref()], 1);
        assert_eq!(
            reads.ranges,
            vec![(
                path.to_string(),
                Some(GetRange::Bounded(37..37 + READ_AHEAD as u64))
            )]
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probes_skip_large_local_sections_and_read_extended_product_headers() {
    let original = message(0, 280.0, [1, 2, 3, 4], 103, 2);
    let expected = MessageMetadata::from(&reader::decode_message(&original, "TMP").unwrap());
    // A 1 MiB local-use section before the grid definition must be skipped.
    let mut large_local = original.clone();
    let mut local = vec![0; 1024 * 1024];
    let local_len = local.len() as u32;
    local[..4].copy_from_slice(&local_len.to_be_bytes());
    local[4] = 2;
    large_local.splice(37..37, local);
    fix_length(&mut large_local);
    let (observed, store, path) = source(&large_local).await;
    assert_eq!(
        read_metadata(&store, &path, &entry(None)).unwrap(),
        expected
    );
    assert!(store.bytes_read() < 2 * READ_AHEAD as u64);
    assert_eq!(observed.reads.lock().unwrap().attempts[path.as_ref()], 2);

    // Section 4 can carry vertical coordinate values after the template.
    let mut extended = original.clone();
    extended.splice(143..143, vec![0; 8000]);
    extended[109..113].copy_from_slice(&8034u32.to_be_bytes());
    extended[114..116].copy_from_slice(&2000u16.to_be_bytes());
    fix_length(&mut extended);
    let (_, store, path) = source(&extended).await;
    assert_eq!(
        read_metadata(&store, &path, &entry(Some(extended.len()))).unwrap(),
        expected
    );
    assert!(store.bytes_read() < 3 * READ_AHEAD as u64);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn header_discovery_does_not_require_decodable_values() {
    let mut bytes = message(0, 280.0, [1, 2, 3, 4], 103, 2);
    let expected = MessageMetadata::from(&reader::decode_message(&bytes, "TMP").unwrap());
    bytes[152..154].copy_from_slice(&u16::MAX.to_be_bytes()); // unsupported packing
                                                              // A full decode cannot handle this packing template; header discovery
                                                              // must not enter the packing decoder at all.
    let (_, store, path) = source(&bytes).await;
    assert_eq!(
        read_metadata(&store, &path, &entry(None)).unwrap(),
        expected
    );
}

#[test]
fn truncated_product_payloads_do_not_panic_in_grib_surface_accessors() {
    // Exercise the dependency's supported and unsupported template numbers,
    // including nonstandard locations of the fixed-surface fields.
    for template in 0..=1101u16 {
        for length in 4..64 {
            let mut payload = vec![0; length];
            payload[2..4].copy_from_slice(&template.to_be_bytes());
            let product = grib::ProdDefinition::from_payload(payload.into_boxed_slice()).unwrap();
            let _ = MessageMetadata::from_product(0, 7, &product);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_or_oversized_headers_fail_with_bounded_reads() {
    let original = message(0, 280.0, [0; 4], 103, 2);
    let mut cases = Vec::new();
    for (offset, replacement) in [
        (0, b"NOPE".as_slice()),
        (7, &[1]),
        (8, &[0; 8]),
        (16, &4u32.to_be_bytes()),
        (41, &[7]),
        // Product template 0 present, but its fixed surfaces are truncated.
        (109, &9u32.to_be_bytes()),
    ] {
        let mut bytes = original.clone();
        bytes[offset..offset + replacement.len()].copy_from_slice(replacement);
        cases.push(bytes);
    }
    let mut oversized = original.clone();
    oversized.splice(143..143, vec![0; MAX_SECTION_BYTES]);
    oversized[109..113].copy_from_slice(&(34 + MAX_SECTION_BYTES as u32).to_be_bytes());
    fix_length(&mut oversized);
    cases.push(oversized);
    cases.push(original[..125].to_vec());
    for bytes in cases {
        let (_, store, path) = source(&bytes).await;
        assert!(read_metadata(&store, &path, &entry(None)).is_err());
        assert!(store.bytes_read() <= READ_AHEAD as u64);
    }
    let (_, store, path) = source(&original).await;
    let before = store.bytes_read();
    assert!(read_metadata(&store, &path, &entry(Some(10))).is_err());
    assert_eq!(store.bytes_read(), before);
    let mut overflow = entry(None);
    overflow.offset = u64::MAX;
    assert!(read_metadata(&store, &path, &overflow).is_err());
    assert_eq!(store.bytes_read(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_probes_keep_order_isolate_failures_and_drain_at_deadline() {
    let observed = Arc::new(TestStore {
        suffix: ".grib2",
        delay: Duration::from_millis(50),
        ..Default::default()
    });
    let store = DataStore::new(observed.clone());
    let names: Vec<_> = (0..19).map(|i| format!("f{i:03}.grib2")).collect();
    for (i, name) in names.iter().enumerate() {
        observed
            .inner
            .put(
                &Path::from(name.as_str()),
                message(0, 280.0, [0; 4], 103, i as u32 + 1).into(),
            )
            .await
            .unwrap();
    }
    observed
        .reads
        .lock()
        .unwrap()
        .fail_once
        .insert(names[2].clone());
    let entry = entry(None);
    let entries: Vec<_> = names.iter().map(|name| (name.as_str(), &entry)).collect();
    let results = read_batch(&store, &entries);
    assert_eq!(results.len(), entries.len());
    for (i, result) in results.into_iter().enumerate() {
        if i == 2 {
            assert!(result.is_err());
        } else {
            assert_eq!(result.unwrap().first_surface_value, Some(i as f64 + 1.0));
        }
    }
    assert!((2..=PROBE_CONCURRENCY).contains(&observed.peak.load(Ordering::SeqCst)));
    assert_eq!(observed.active.load(Ordering::SeqCst), 0);
    assert!(read_batch(&store, &entries[2..3])[0].is_ok());

    let before = store.bytes_read();
    let _deadline = ds_core::deadline::enter(Some(Instant::now() + Duration::from_millis(5)));
    assert!(read_batch(&store, &entries)
        .iter()
        .all(|r| matches!(r, Err(DataServerError::DeadlineExceeded))));
    assert_eq!(store.bytes_read(), before);
    assert_eq!(observed.active.load(Ordering::SeqCst), 0);
    assert_eq!(Arc::strong_count(&observed), 2, "all probe workers joined");
}

async fn engine(count: usize, delay: Duration, cache_mb: u64) -> (GribEngine, Vec<MessageEntry>) {
    let temporary = TestSource::new();
    let mut config = temporary.config();
    config.grid_cache_mb = cache_mb;
    let mut engine = GribEngine::new("probes", &config).unwrap();
    let observed = Arc::new(TestStore {
        suffix: ".grib2",
        delay,
        ..Default::default()
    });
    let mut entries = Vec::new();
    for i in 0..count {
        let url = format!("f{i:03}.grib2");
        observed
            .inner
            .put(&Path::from(url.as_str()), ECMWF.to_vec().into())
            .await
            .unwrap();
        let mut entry = entry(Some(ECMWF.len()));
        entry.param = format!("P{i:02}");
        entry.source_url = Some(url.into());
        entries.push(entry);
    }
    let reference = "2026-04-05T00:00:00Z".parse().unwrap();
    let mut catalog = Catalog::new();
    catalog.runs.insert(
        reference,
        ForecastRun {
            reference_time: reference,
            steps: [(
                0,
                StepFile {
                    grib_url: "f000.grib2".into(),
                    messages: entries.clone(),
                },
            )]
            .into_iter()
            .collect(),
        },
    );
    catalog.refresh_metadata();
    let source = Arc::get_mut(&mut engine.source).unwrap();
    source.store = DataStore::new(observed);
    source.catalog.store(Arc::new(catalog));
    (engine, entries)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_probes_preserve_the_query_grid_cache() {
    let (engine, entries) = engine(32, Duration::ZERO, 8).await;
    let grid = engine
        .fetch_grid_by_entry("f000.grib2", &entries[0])
        .unwrap();
    let before = engine.storage_bytes_read();
    engine.probe_new_parameters();
    assert_eq!(engine.source.param_meta.read().unwrap().by_level.len(), 32);
    assert_eq!(engine.storage_bytes_read() - before, 31 * READ_AHEAD as u64);
    let cache = engine.source.grid_cache.as_ref().unwrap();
    assert_eq!(cache.len(), 1);
    assert!(Arc::ptr_eq(&grid, &cache.get("f000.grib2", 0).unwrap()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual metadata replay with a real global field and simulated GET latency"]
async fn metadata_probe_replay() {
    let mut baseline = None;
    for headers_only in [false, true] {
        let (engine, entries) = engine(32, Duration::from_millis(150), 256).await;
        let start = Instant::now();
        if headers_only {
            engine.probe_new_parameters();
        } else {
            for entry in entries {
                engine
                    .fetch_grid_by_entry(entry.source_url.as_deref().unwrap(), &entry)
                    .unwrap();
            }
        }
        let elapsed = start.elapsed();
        let descriptions: BTreeMap<_, _> = engine
            .get_parameter_descriptions()
            .into_iter()
            .map(|(name, meta)| (name, (meta.label, meta.unit, meta.observed_property)))
            .collect();
        if let Some(expected) = &baseline {
            assert_eq!(&descriptions, expected);
        } else {
            baseline = Some(descriptions);
        }
        let cache = engine.source.grid_cache.as_ref().unwrap();
        if headers_only {
            assert_eq!(cache.len(), 0);
        }
        eprintln!("probes=32 headers_only={headers_only} elapsed_ms={} bytes={} cached_grids={} cache_bytes={}", elapsed.as_millis(), engine.storage_bytes_read(), cache.len(), cache.weight());
    }
}
