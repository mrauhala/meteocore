//! Position queries fetching only the bytes their points need (#1301):
//! every GET and its bytes counted over an instrumented store, every value
//! bit for bit the full decode's.

use super::*;
use crate::ccsds::{Header, Kin};
use crate::runtime::run_fetches;
use crate::test_support::{message, store::TestStore, Ccsds, TestSource};
use ds_storage::object_store::{path::Path, GetRange, ObjectStoreExt};

pub(crate) const RUN: &str = "2026040500";

fn fixture() -> Vec<u8> {
    std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../testdata/grib-local/sample-message.grib2"
    ))
    .unwrap()
}

/// Step files `{prefix}f000`… of the run at `date`, each holding
/// `fields` (parameter, message) at 150 hPa; a tail record after them gives
/// each an indexed length unless `tail` is false.
pub(crate) fn write_run(
    source: &TestSource,
    prefix: &str,
    date: &str,
    steps: &[Vec<(&str, Vec<u8>)>],
    tail: bool,
) {
    for (step, fields) in steps.iter().enumerate() {
        let forecast = format!("{step} hour fcst");
        let mut records: Vec<_> = fields
            .iter()
            .map(|(param, bytes)| (*param, "150 mb", forecast.as_str(), bytes.clone()))
            .collect();
        if tail {
            records.push((
                "TAIL",
                "surface",
                forecast.as_str(),
                message(0, 0.0, [0; 4], 1, 0),
            ));
        }
        source.write_run(&format!("{prefix}f{step:03}"), date, &records);
    }
}

/// The pressure view of `source` with these budgets in MiB, 0 disabling
/// each, reading through an instrumented in-memory copy of its files.
pub(crate) fn counted(
    source: &TestSource,
    point_index_mb: u64,
    message_mb: u64,
    grid_mb: u64,
) -> (GribEngine, Arc<TestStore>) {
    let mut config = source.config();
    config.point_index_cache_mb = Some(point_index_mb);
    config.message_cache_mb = message_mb;
    config.grid_cache_mb = grid_mb;
    config.level_types = Some(vec![GribLevelType::Pressure]);
    let mut owner = GribEngine::new("fetches", &config).unwrap();
    let store = Arc::new(TestStore {
        suffix: ".grib2",
        allow_heads: true,
        ..Default::default()
    });
    run_fetches(async {
        for file in std::fs::read_dir(&source.dir).unwrap() {
            let path = file.unwrap().path();
            let name = path.file_name().unwrap().to_str().unwrap().to_owned();
            if name.ends_with(".grib2") {
                let bytes = std::fs::read(&path).unwrap();
                store
                    .inner
                    .put(&Path::from(name), bytes.into())
                    .await
                    .unwrap();
            }
        }
    });
    Arc::get_mut(&mut owner.source).unwrap().store = ds_storage::DataStore::new(store.clone());
    let view = owner.level_collections().into_iter().next().unwrap();
    (view, store)
}

/// The GETs since the last call, as `(path, first byte, bytes)`.
pub(crate) fn gets(store: &TestStore) -> Vec<(String, u64, u64)> {
    let mut reads = store.reads.lock().unwrap();
    let gets = reads
        .ranges
        .iter()
        .map(|(path, range)| match range {
            Some(GetRange::Bounded(range)) => (path.clone(), range.start, range.end - range.start),
            other => panic!("unbounded GET {other:?}"),
        })
        .collect();
    reads.ranges.clear();
    gets
}

fn at(date: &str, hour: u32) -> DateTime<Utc> {
    let run = chrono::NaiveDateTime::parse_from_str(&format!("{date}0000"), "%Y%m%d%H%M%S")
        .unwrap()
        .and_utc();
    run + chrono::Duration::hours(hour.into())
}

/// `parameter`'s series at `points` over the run at `date`, steps
/// `first..=last` (all when `None`), values as bits.
pub(crate) fn series(
    engine: &GribEngine,
    parameter: &str,
    points: &[(f64, f64)],
    run: &str,
    steps: Option<(u32, u32)>,
) -> Vec<Vec<Option<u64>>> {
    let points: Vec<String> = points
        .iter()
        .map(|(lon, lat)| format!("POINT({lon} {lat})"))
        .collect();
    let mut output = Vec::new();
    engine
        .query_positions(
            &points,
            steps.map(|(first, last)| (at(run, first), at(run, last))),
            Some(&[parameter.to_owned()]),
            Some(&[150.0]),
            Some(at(run, 0)),
            &mut |response| {
                let CoverageResponse::Single(result) = response else {
                    panic!("expected a series")
                };
                output.push(
                    result.ranges[parameter]
                        .values
                        .iter()
                        .map(|v| v.map(f64::to_bits))
                        .collect(),
                );
                Ok(())
            },
        )
        .unwrap();
    output
}

/// The fixture's complete index, to state what a read should fetch.
fn indexed(bytes: &[u8]) -> crate::ccsds::CcsdsIndex {
    Header::parse(bytes).unwrap().index(bytes, &[]).unwrap().0
}

const NORTH: (f64, f64) = (24.96, 60.32);
const SOUTH: (f64, f64) = (151.2, -33.9);

/// Before any message of the source is indexed nothing predicts a prefix:
/// the first read fetches the whole message. Another parameter on the same
/// grid then predicts linearly from it: one prefix GET of that length.
#[test]
fn a_cold_read_without_a_sibling_fetches_a_linear_prefix() {
    let q = fixture();
    let source = TestSource::new();
    write_run(
        &source,
        "",
        RUN,
        &[vec![("q", q.clone()), ("w", q.clone())]],
        true,
    );
    let expected = |param| series(&counted(&source, 0, 0, 0).0, param, &[NORTH], RUN, None);
    let (engine, store) = counted(&source, 16, 0, 0);
    assert_eq!(series(&engine, "q", &[NORTH], RUN, None), expected("q"));
    assert_eq!(gets(&store), [("f000.grib2".into(), 0, q.len() as u64)]);

    assert_eq!(series(&engine, "w", &[NORTH], RUN, None), expected("w"));
    let prefix = indexed(&q).predict_prefix(&[NORTH], q.len(), Kin::OtherParameter) as u64;
    assert_eq!(
        gets(&store),
        [("f000.grib2".into(), q.len() as u64, prefix)]
    );
    assert!(prefix < q.len() as u64 / 3, "{prefix} of {}", q.len());
    assert_eq!(engine.point_field_counts(), (2, 0));
    let fetched: Vec<_> = engine
        .point_fetch_counts()
        .iter()
        .map(|c| (c.1, c.2))
        .collect();
    assert_eq!(fetched, [(1, q.len() as u64), (1, prefix), (0, 0), (0, 0)]);
}

/// A sibling in the same run predicts the prefix from where its own needed
/// RSIs end: one GET, at most 2 % past what the points need.
#[test]
fn a_cold_read_with_a_sibling_fetches_one_prefix_within_two_percent() {
    let q = fixture();
    let source = TestSource::new();
    write_run(
        &source,
        "",
        RUN,
        &[vec![("q", q.clone())], vec![("q", q.clone())]],
        true,
    );
    let full = counted(&source, 0, 0, 0).0;
    for point in [NORTH, SOUTH] {
        let (engine, store) = counted(&source, 16, 0, 0);
        series(&engine, "q", &[point], RUN, Some((0, 0)));
        gets(&store);
        assert_eq!(
            series(&engine, "q", &[point], RUN, Some((1, 1))),
            series(&full, "q", &[point], RUN, Some((1, 1)))
        );
        let index = indexed(&q);
        let needed = index
            .rsi_bytes(&index.needs(&[point]).unwrap())
            .unwrap()
            .end as u64;
        let gets = gets(&store);
        let [(path, first, bytes)] = &gets[..] else {
            panic!("one GET: {gets:?}")
        };
        assert_eq!((path.as_str(), *first), ("f001.grib2", 0));
        assert!(
            *bytes >= needed && *bytes as f64 <= needed as f64 * 1.02 + 1.0,
            "{bytes} for {needed}"
        );
    }
}

/// A sibling whose northern half packs to almost nothing predicts far too
/// short a prefix for a field noisy everywhere: one continuation reads on
/// to where the partial index predicts, and no third GET follows.
#[test]
fn a_short_prediction_takes_exactly_one_continuation() {
    let spec = Ccsds::global();
    let nodes = (spec.ni * spec.nj) as usize;
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut noise = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed & 0xffff) as u32
    };
    let calm: Vec<u32> = (0..nodes)
        .map(|k| if k < nodes / 2 { 7 } else { noise() })
        .collect();
    let noisy: Vec<u32> = (0..nodes).map(|_| noise()).collect();
    let (calm, noisy) = (spec.message(&calm), spec.message(&noisy));
    let source = TestSource::new();
    write_run(
        &source,
        "",
        RUN,
        &[vec![("q", calm)], vec![("q", noisy.clone())]],
        true,
    );
    let (engine, store) = counted(&source, 16, 0, 0);
    let full = counted(&source, 0, 0, 0).0;
    let point = [(10.0, -5.0)];
    series(&engine, "q", &point, RUN, Some((0, 0)));
    gets(&store);
    assert_eq!(
        series(&engine, "q", &point, RUN, Some((1, 1))),
        series(&full, "q", &point, RUN, Some((1, 1)))
    );
    let gets = gets(&store);
    assert_eq!(gets.len(), 2, "{gets:?}");
    let (prefix, more) = (&gets[0], &gets[1]);
    assert_eq!(prefix.1, 0);
    assert_eq!(
        more.1, prefix.2,
        "the continuation starts where the prefix ends"
    );
    assert!(prefix.2 + more.2 <= noisy.len() as u64);
    let fetched: Vec<_> = engine.point_fetch_counts().iter().map(|c| c.1).collect();
    assert_eq!(fetched, [1, 1, 0, 1]);
}

/// An indexed message serves later queries with one range GET of the RSIs
/// they need, a MULTIPOINT batch from the far north to the far south too.
#[test]
fn warm_reads_fetch_one_range_of_the_needed_rsis() {
    let q = fixture();
    let source = TestSource::new();
    write_run(&source, "", RUN, &[vec![("q", q.clone())]], true);
    let (engine, store) = counted(&source, 16, 0, 0);
    let full = counted(&source, 0, 0, 0).0;
    series(&engine, "q", &[NORTH], RUN, None);
    gets(&store);
    let index = indexed(&q);
    for points in [
        vec![NORTH],
        vec![SOUTH],
        vec![(0.0, 89.0), NORTH, SOUTH, (-60.0, -80.0)],
    ] {
        assert_eq!(
            series(&engine, "q", &points, RUN, None),
            series(&full, "q", &points, RUN, None)
        );
        let span = index.rsi_bytes(&index.needs(&points).unwrap()).unwrap();
        assert_eq!(
            gets(&store),
            [("f000.grib2".into(), span.start as u64, span.len() as u64)],
            "{points:?}"
        );
    }
    assert!(
        index
            .rsi_bytes(&index.needs(&[NORTH]).unwrap())
            .unwrap()
            .len()
            < 16 * 1024
    );
    let ranges = engine.point_fetch_counts()[2];
    assert_eq!((ranges.0, ranges.1), ("range", 3));
}

/// A prefix leaves a partial index; a later point further south reads on
/// from the last RSI it knows the start of, one GET, never refetching the
/// bytes before it.
#[test]
fn a_partial_index_is_read_on_from_where_it_ends() {
    let q = fixture();
    let source = TestSource::new();
    write_run(
        &source,
        "",
        RUN,
        &[vec![("q", q.clone())], vec![("q", q.clone())]],
        true,
    );
    let (engine, store) = counted(&source, 16, 0, 0);
    let full = counted(&source, 0, 0, 0).0;
    series(&engine, "q", &[NORTH], RUN, Some((0, 0)));
    series(&engine, "q", &[NORTH], RUN, Some((1, 1)));
    let first_reads = gets(&store);
    let [_, (_, 0, prefix)] = &first_reads[..] else {
        panic!("a whole read, then one prefix: {first_reads:?}")
    };
    let prefix = *prefix;
    assert_eq!(
        series(&engine, "q", &[SOUTH], RUN, Some((1, 1))),
        series(&full, "q", &[SOUTH], RUN, Some((1, 1)))
    );
    let read_on = gets(&store);
    let [(path, first, _)] = &read_on[..] else {
        panic!("one read on: {read_on:?}")
    };
    let first = *first;
    assert_eq!(path, "f001.grib2");
    assert!(
        first > 0 && first < prefix,
        "{first} in a prefix of {prefix}"
    );
    assert_eq!(engine.point_fetch_counts()[2].1, 1, "a range");
    // The index now reaches the south: a repeat is one range of its RSIs.
    series(&engine, "q", &[SOUTH], RUN, Some((1, 1)));
    let index = indexed(&q);
    let span = index.rsi_bytes(&index.needs(&[SOUTH]).unwrap()).unwrap();
    assert_eq!(
        gets(&store),
        [("f001.grib2".into(), span.start as u64, span.len() as u64)]
    );
}

/// A message cache that holds the query's messages fetches each whole, as
/// before #1301, and serves the repeat from memory; one too small for them
/// fetches parts and keeps nothing.
#[test]
fn the_message_cache_decides_whole_or_partial() {
    let q = fixture();
    let source = TestSource::new();
    let steps: Vec<_> = (0..4).map(|_| vec![("q", q.clone())]).collect();
    write_run(&source, "", RUN, &steps, true);
    let full = counted(&source, 0, 0, 0).0;

    let (engine, store) = counted(&source, 16, 64, 0);
    for points in [[NORTH], [SOUTH]] {
        assert_eq!(
            series(&engine, "q", &points, RUN, None),
            series(&full, "q", &points, RUN, None)
        );
    }
    let whole = gets(&store);
    assert_eq!(whole.len(), 4);
    assert!(whole
        .iter()
        .all(|&(_, first, bytes)| first == 0 && bytes == q.len() as u64));
    assert_eq!(engine.message_cache_metrics().misses, 4);

    // 1 MiB holds one 0.57 MB message, not four: a one-step query fetches
    // its message whole and keeps it, the four-step one fetches parts.
    let (engine, store) = counted(&source, 16, 1, 0);
    let one = Some((0, 0));
    assert_eq!(
        series(&engine, "q", &[NORTH], RUN, one),
        series(&full, "q", &[NORTH], RUN, one)
    );
    assert_eq!(gets(&store), [("f000.grib2".into(), 0, q.len() as u64)]);
    let kept = engine.message_cache_metrics().bytes;
    assert!(kept > q.len() as u64);
    assert_eq!(
        series(&engine, "q", &[SOUTH], RUN, None),
        series(&full, "q", &[SOUTH], RUN, None)
    );
    // The kept message serves its step; the rest are prefixes predicted
    // from its index.
    let parts = gets(&store);
    assert_eq!(parts.len(), 3, "{parts:?}");
    assert!(parts
        .iter()
        .all(|&(_, first, bytes)| first == 0 && bytes < q.len() as u64));
    let fetched: Vec<_> = engine.point_fetch_counts().iter().map(|c| c.1).collect();
    assert_eq!(fetched, [1, 3, 0, 0]);
    assert_eq!(
        engine.message_cache_metrics().bytes,
        kept,
        "parts are never kept"
    );
}

/// A field without an indexed length, a wgrib2 tail record, is fetched
/// whole every time.
#[test]
fn a_field_without_a_length_is_fetched_whole() {
    let q = fixture();
    let source = TestSource::new();
    write_run(
        &source,
        "",
        RUN,
        &[vec![("q", q.clone())], vec![("q", q.clone())]],
        false,
    );
    let (engine, store) = counted(&source, 16, 0, 0);
    let full = counted(&source, 0, 0, 0).0;
    for _ in 0..2 {
        assert_eq!(
            series(&engine, "q", &[NORTH], RUN, None),
            series(&full, "q", &[NORTH], RUN, None)
        );
        let gets = gets(&store);
        assert_eq!(gets.len(), 2);
        assert!(gets
            .iter()
            .all(|&(_, first, bytes)| first == 0 && bytes == q.len() as u64));
    }
    assert!(store.heads.load(std::sync::atomic::Ordering::SeqCst) > 0);
}

/// What a 41-step point query fetches from any local GRIB2 message served
/// as every step of two runs, against whole messages: the first query of
/// the source, a warm repeat, and a new run with only the older run's
/// indexes to predict from. `MC_CCSDS_REPLAY=ifs01-2t.grib2 cargo test -p
/// engine-grib --release point_fetch_replay -- --ignored --nocapture`.
/// Every step is the same message, so same-run predictions are as good as
/// they get. Prints measurements; no thresholds.
#[test]
#[ignore = "needs MC_CCSDS_REPLAY; prints measurements"]
fn point_fetch_replay() {
    let path = std::env::var("MC_CCSDS_REPLAY").expect("MC_CCSDS_REPLAY");
    let bytes = std::fs::read(&path).unwrap();
    let steps: Vec<_> = (0..41).map(|_| vec![("q", bytes.clone())]).collect();
    let source = TestSource::new();
    write_run(&source, "a", RUN, &steps, true);
    write_run(&source, "b", "2026040512", &steps, true);
    eprintln!(
        "{path}: 41 steps of {} bytes, whole: 41 GETs, {} bytes",
        bytes.len(),
        41 * bytes.len()
    );
    let next = "2026040512";
    for (name, phases) in [
        (
            "60°N",
            vec![
                ("first query of the source", NORTH, RUN),
                ("warm repeat", NORTH, RUN),
                ("new run, older run indexed", NORTH, next),
                ("new run, warm repeat", NORTH, next),
                ("then 34°S, read on from the indexes", SOUTH, next),
            ],
        ),
        (
            "34°S",
            vec![
                ("first query of the source", SOUTH, RUN),
                ("warm repeat", SOUTH, RUN),
                ("new run, older run indexed", SOUTH, next),
                ("new run, warm repeat", SOUTH, next),
            ],
        ),
    ] {
        let (engine, store) = counted(&source, 16, 0, 0);
        let full = counted(&source, 0, 0, 0).0;
        let mut previous = engine.point_fetch_counts();
        for (phase, point, run) in phases {
            let start = std::time::Instant::now();
            let values = series(&engine, "q", &[point], run, None);
            let elapsed = start.elapsed();
            assert_eq!(values, series(&full, "q", &[point], run, None));
            gets(&store);
            let now = engine.point_fetch_counts();
            let modes: Vec<String> = now
                .iter()
                .zip(&previous)
                .filter(|(now, was)| now.1 > was.1)
                .map(|(now, was)| format!("{} {} GETs {} B", now.0, now.1 - was.1, now.2 - was.2))
                .collect();
            let total: u64 = now
                .iter()
                .zip(&previous)
                .map(|(now, was)| now.2 - was.2)
                .sum();
            eprintln!(
                "  {name} {phase}: {} B = {:.1} % of whole; {} ({:.0} ms)",
                total,
                100.0 * total as f64 / (41 * bytes.len()) as f64,
                modes.join(", "),
                elapsed.as_secs_f64() * 1e3
            );
            previous = now;
        }
    }
}
