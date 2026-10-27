//! How an EDR query reads its fields (#1297): up to sixteen at once, and
//! around the decoded-grid cache when the cache cannot hold them all, so a
//! long window neither evicts its own fields nor the grids other queries
//! and maps keep there.

use super::*;
use crate::runtime::{run_fetches, MAX_FIELD_JOBS};
use crate::test_support::{message, parameter_message, store::TestStore, TestSource};
use ds_core::cube::CubeResolution;
use ds_core::feature::Bbox;
use ds_storage::object_store::{path::Path, ObjectStoreExt};
use std::sync::atomic::Ordering;

const STEPS: u32 = 24;

/// Grids the test cache holds at the planner's estimate: a window of
/// [`STEPS`] fields is three times the cache.
const CACHE_FIELDS: u64 = 8;

fn at(hour: u32) -> DateTime<Utc> {
    "2026-04-05T00:00:00Z".parse::<DateTime<Utc>>().unwrap() + chrono::Duration::hours(hour.into())
}

fn temperature(step: u32) -> Vec<u8> {
    message(0, 280.0 + step as f32, [0, 2, 4, 6], 100, 85000)
}

/// Relative humidity (WMO 0/1/1, %) at 850 hPa.
fn humidity(step: u32) -> Vec<u8> {
    parameter_message(1, 1, 0x30, 40.0 + step as f32, [6, 4, 2, 0], 100, 85000)
}

/// Bytes one field read fetches: every fixture message is this long.
fn field_bytes() -> u64 {
    temperature(0).len() as u64
}

/// `TMP` and `RH` at 850 hPa over [`STEPS`] hourly steps, one file per step.
fn fixture() -> TestSource {
    let source = TestSource::new();
    for step in 0..STEPS {
        source.write(
            &format!("f{step:03}"),
            &[
                ("TMP", "850 mb", temperature(step)),
                ("RH", "850 mb", humidity(step)),
                // Explicit lengths for the selected fields: no tail HEADs.
                ("TAIL", "surface", message(0, 0.0, [0; 4], 1, 0)),
            ],
            step,
        );
    }
    source
}

/// The pressure view, with a decoded-grid cache of `cache` bytes (`None`:
/// disabled).
fn view(source: &TestSource, cache: Option<u64>) -> GribEngine {
    engine_on(source, cache, 0, None)
}

/// [`view`] with a compressed-message cache of `message_cache_mb`, reading
/// through `store` (a copy of the fixture) when given.
fn engine_on(
    source: &TestSource,
    cache: Option<u64>,
    message_cache_mb: u64,
    store: Option<Arc<TestStore>>,
) -> GribEngine {
    let mut config = source.config();
    config.grid_cache_mb = 0;
    config.message_cache_mb = message_cache_mb;
    config.level_types = Some(vec![GribLevelType::Pressure]);
    config.parameters = Some(vec!["TMP".into(), "RH".into()]);
    let mut owner = GribEngine::new("fields", &config).unwrap();
    let owned = Arc::get_mut(&mut owner.source).unwrap();
    owned.grid_cache = cache.map(GridCache::with_capacity_bytes);
    if let Some(store) = store {
        owned.store = ds_storage::DataStore::new(store);
    }
    let view = owner.level_collections().into_iter().next().unwrap();
    assert_eq!(view.raster_info_shared().grid_size, Some([1, 1]));
    view
}

/// A cache of [`CACHE_FIELDS`] fixture grids at the planner's estimate.
fn small_cache() -> Option<u64> {
    Some(CACHE_FIELDS * GridCache::estimated_weight(4))
}

fn cache(engine: &GribEngine) -> &GridCache {
    engine.source.grid_cache.as_ref().unwrap()
}

/// `(entries, bytes)` resident in the decoded-grid cache.
fn residents(engine: &GribEngine) -> (usize, u64) {
    (cache(engine).len(), cache(engine).weight())
}

/// The position series of `parameter` at one point over steps `first..=last`.
fn series(engine: &GribEngine, parameter: &str, first: u32, last: u32) -> CoverageResponse {
    engine
        .query_position(
            "POINT(0.3 0.6)",
            Some((at(first), at(last))),
            Some(&[parameter.to_owned()]),
            None,
            None,
        )
        .unwrap()
}

/// Every coverage's domain and ranges, values as bits: equal fingerprints
/// are bit-identical responses.
fn fingerprint(response: &CoverageResponse) -> Vec<String> {
    let results: Vec<&QueryResult> = match response {
        CoverageResponse::Single(result) => vec![result],
        CoverageResponse::Collection(results) => results.iter().collect(),
    };
    results
        .into_iter()
        .map(|result| {
            let ranges: BTreeMap<_, _> = result
                .ranges
                .iter()
                .map(|(name, range)| {
                    let bits: Vec<_> = range.values.iter().map(|v| v.map(f64::to_bits)).collect();
                    (name, (&range.shape, &range.axis_names, bits))
                })
                .collect();
            format!("{:?} {ranges:?}", result.domain)
        })
        .collect()
}

/// Every EDR query kind that reads fields through the shared scheduler,
/// each over all [`STEPS`] steps of `TMP`.
type Query = fn(&GribEngine) -> CoverageResponse;

fn whole_window() -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    Some((at(0), at(STEPS - 1)))
}

fn tmp() -> Vec<String> {
    vec!["TMP".into()]
}

const QUERIES: [(&str, Query); 4] = [
    ("position", |engine| series(engine, "TMP", 0, STEPS - 1)),
    ("trajectory", |engine| {
        engine
            .query_trajectory(
                "LINESTRING(0.1 0.2, 0.9 0.8)",
                whole_window(),
                Some(&tmp()),
                None,
                None,
            )
            .unwrap()
    }),
    ("area", |engine| {
        engine
            .query_area("0,0,1,1", whole_window(), Some(&tmp()), None, None)
            .unwrap()
    }),
    ("cube", |engine| {
        engine
            .query_cube(
                &Bbox::new(0.0, 0.0, 1.0, 1.0).unwrap(),
                whole_window(),
                Some(&tmp()),
                None,
                CubeResolution::default(),
                None,
            )
            .unwrap()
    }),
];

/// A window three times the cache, in every query kind: it reads around
/// the cache, which keeps exactly the grids an earlier query left there,
/// and answers bit for bit as with the cache disabled or large enough.
#[test]
fn an_oversized_window_leaves_the_cache_as_it_was() {
    let source = fixture();
    for (kind, query) in QUERIES {
        let engine = view(&source, small_cache());
        // Earlier entries: a window that fits, of another parameter.
        series(&engine, "RH", 0, 2);
        let before = residents(&engine);
        assert_eq!(before.0, 3, "{kind}");
        assert_eq!(cache(&engine).stats(), (0, 3), "{kind}");

        let read = engine.storage_bytes_read();
        let response = query(&engine);
        assert_eq!(
            engine.storage_bytes_read() - read,
            u64::from(STEPS) * field_bytes(),
            "{kind}: every field read once"
        );
        assert_eq!(
            residents(&engine),
            before,
            "{kind}: nothing inserted or evicted"
        );
        assert_eq!(cache(&engine).stats(), (0, 3 + u64::from(STEPS)), "{kind}");

        // The earlier window is still resident: all hits, nothing read.
        let read = engine.storage_bytes_read();
        series(&engine, "RH", 0, 2);
        assert_eq!(engine.storage_bytes_read(), read, "{kind}");
        assert_eq!(cache(&engine).stats(), (3, 3 + u64::from(STEPS)), "{kind}");

        let expected = fingerprint(&response);
        let uncached = view(&source, None);
        assert_eq!(fingerprint(&query(&uncached)), expected, "{kind}: no cache");
        let large = view(&source, Some(4 * ds_cache::MIB));
        assert_eq!(
            fingerprint(&query(&large)),
            expected,
            "{kind}: filled cache"
        );
        assert_eq!(
            residents(&large).0,
            STEPS as usize,
            "{kind}: a window that fits fills"
        );
    }
}

/// An oversized window still uses what is resident: the fields an earlier
/// window left are hits, and only the others are read.
#[test]
fn an_oversized_window_reuses_resident_fields() {
    let source = fixture();
    let engine = view(&source, small_cache());
    let first = series(&engine, "TMP", 0, 2);
    assert_eq!(residents(&engine).0, 3);
    let before = residents(&engine);
    let read = engine.storage_bytes_read();
    let whole = series(&engine, "TMP", 0, STEPS - 1);
    assert_eq!(
        engine.storage_bytes_read() - read,
        u64::from(STEPS - 3) * field_bytes()
    );
    assert_eq!(residents(&engine), before);
    assert_eq!(cache(&engine).stats(), (3, u64::from(STEPS)));
    // Its first three steps are the earlier window's samples.
    let values = |response: &CoverageResponse| match response {
        CoverageResponse::Single(result) => result.ranges["TMP"].values.clone(),
        CoverageResponse::Collection(_) => panic!("expected one series"),
    };
    assert_eq!(values(&whole)[..3], values(&first)[..]);
    assert_eq!(
        fingerprint(&whole),
        fingerprint(&series(&view(&source, None), "TMP", 0, STEPS - 1))
    );
}

/// Reading around the decoded-grid cache still fills the compressed-message
/// cache: the repeated window downloads nothing, decoding each message
/// again, and the decoded-grid cache is as it was.
#[test]
fn an_oversized_window_still_fills_the_message_cache() {
    let source = fixture();
    let engine = engine_on(&source, small_cache(), 1, None);
    let read = engine.storage_bytes_read();
    let first = series(&engine, "TMP", 0, STEPS - 1);
    assert_eq!(
        engine.storage_bytes_read() - read,
        u64::from(STEPS) * field_bytes()
    );
    let messages = engine.message_cache_metrics();
    assert_eq!((messages.hits, messages.misses), (0, u64::from(STEPS)));
    assert!(messages.bytes >= u64::from(STEPS) * field_bytes());
    let read = engine.storage_bytes_read();
    let again = series(&engine, "TMP", 0, STEPS - 1);
    assert_eq!(engine.storage_bytes_read(), read, "no download");
    let messages = engine.message_cache_metrics();
    assert_eq!(messages.hits, u64::from(STEPS));
    assert_eq!(residents(&engine), (0, 0));
    assert_eq!(fingerprint(&again), fingerprint(&first));
}

/// A window the cache holds caches as before #1297: repeated, it is all
/// hits and reads nothing.
#[test]
fn a_window_that_fits_is_a_hit_when_repeated() {
    let source = fixture();
    let engine = view(&source, small_cache());
    let read = engine.storage_bytes_read();
    let first = series(&engine, "TMP", 0, 3);
    assert_eq!(engine.storage_bytes_read() - read, 4 * field_bytes());
    assert_eq!(residents(&engine).0, 4);
    assert_eq!(cache(&engine).stats(), (0, 4));
    let read = engine.storage_bytes_read();
    let again = series(&engine, "TMP", 0, 3);
    assert_eq!(engine.storage_bytes_read(), read);
    assert_eq!(cache(&engine).stats(), (4, 4));
    assert_eq!(fingerprint(&again), fingerprint(&first));
}

/// A query over the fixture's 2 × 2 grid runs [`MAX_FIELD_JOBS`] reads at
/// once: each GET waits long enough for every dispatched job to start.
#[test]
fn a_small_grid_runs_sixteen_field_reads_at_once() {
    let source = fixture();
    let store = Arc::new(TestStore {
        suffix: ".grib2",
        delay: Duration::from_millis(200),
        ..Default::default()
    });
    run_fetches(async {
        for file in std::fs::read_dir(&source.dir).unwrap() {
            let path = file.unwrap().path();
            if path.extension().is_some_and(|ext| ext == "grib2") {
                let name = Path::from(path.file_name().unwrap().to_str().unwrap());
                let bytes = std::fs::read(&path).unwrap();
                store.inner.put(&name, bytes.into()).await.unwrap();
            }
        }
    });
    let engine = engine_on(&source, None, 0, Some(store.clone()));
    series(&engine, "TMP", 0, STEPS - 1);
    assert_eq!(store.reads.lock().unwrap().attempts.len(), STEPS as usize);
    assert_eq!(store.peak.load(Ordering::SeqCst), MAX_FIELD_JOBS);
    assert_eq!(store.active.load(Ordering::SeqCst), 0);
    let plan = engine.field_plan(STEPS as usize);
    assert_eq!(plan.concurrency, MAX_FIELD_JOBS);
    assert_eq!(plan.fill, GridFill::Insert, "no cache, nothing to bypass");
}

/// The plan of a query over the fixture grid: sixteen jobs, and the cache
/// filled only by a window it holds.
#[test]
fn the_plan_bypasses_only_a_window_the_cache_cannot_hold() {
    let source = fixture();
    let engine = view(&source, small_cache());
    for (fields, fill) in [
        (1, GridFill::Insert),
        (CACHE_FIELDS as usize, GridFill::Insert),
        (CACHE_FIELDS as usize + 1, GridFill::Bypass),
        (STEPS as usize, GridFill::Bypass),
    ] {
        let plan = engine.field_plan(fields);
        assert_eq!(
            plan,
            FieldPlan {
                concurrency: MAX_FIELD_JOBS,
                fill
            },
            "{fields} fields"
        );
    }
}
