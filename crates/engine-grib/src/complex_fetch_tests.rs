//! Position queries on fields in complex packing with spatial differencing
//! (#1302), as GFS packs every field: through the point index bit for bit
//! as the full decode, and fetching only the bytes their points need, every
//! GET and its bytes counted over an instrumented store.

use super::*;
use crate::ccsds::Kin;
use crate::complex::{ComplexIndex, Header};
use crate::point_fetch_tests::{counted, gets, series, write_run, RUN};
use crate::test_support::{Complex, TestSource, Value};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/../../testdata/grib-gfs-complex/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn temperature() -> Vec<u8> {
    fixture("gfs-tmp2m-1deg.grib2")
}

fn soil() -> Vec<u8> {
    fixture("gfs-soilw-1deg.grib2")
}

/// The message's complete index, to state what a read should fetch.
fn indexed(bytes: &[u8]) -> ComplexIndex {
    Header::parse(bytes).unwrap().index(bytes, &[]).unwrap().0
}

/// GETs and bytes by mode: whole, prefix, range, continuation.
fn fetched(engine: &GribEngine) -> Vec<(u64, u64)> {
    engine
        .point_fetch_counts()
        .iter()
        .map(|&(_, gets, bytes)| (gets, bytes))
        .collect()
}

const NORTH: (f64, f64) = (24.96, 60.32);
const MIDDLE: (f64, f64) = (139.8, 35.6);
const SOUTH: (f64, f64) = (151.2, -33.9);

/// With every message in the compressed-message cache, fields read through
/// the index: built on the first read of each message, which decodes only
/// through the rows its points read (#1337); a query further south extends
/// those indexes from the cached bytes, with no GET, and a repeat reuses
/// them. Each index weighs as one built that far at once. No grid is
/// decoded.
#[test]
fn whole_messages_read_through_the_index_bit_for_bit() {
    for message in [temperature(), soil()] {
        let source = TestSource::new();
        let steps: Vec<_> = (0..4).map(|_| vec![("q", message.clone())]).collect();
        write_run(&source, "", RUN, &steps, true);
        let full = counted(&source, 0, 64, 0).0;
        let (engine, store) = counted(&source, 16, 64, 0);
        let south = vec![SOUTH, MIDDLE, (0.0, -89.5)];
        let mut weights = Vec::new();
        for (round, points) in [vec![NORTH], south.clone(), vec![NORTH, SOUTH]]
            .into_iter()
            .enumerate()
        {
            assert_eq!(
                series(&engine, "q", &points, RUN, None),
                series(&full, "q", &points, RUN, None)
            );
            let gets = gets(&store);
            if round == 0 {
                assert_eq!(gets.len(), 4);
                assert!(gets
                    .iter()
                    .all(|&(_, first, bytes)| first == 0 && bytes == message.len() as u64));
            } else {
                assert_eq!(gets, [], "{points:?} from the cached messages");
            }
            weights.push(engine.point_index_cache_metrics().bytes);
        }
        assert_eq!(full.point_field_counts(), (0, 12));
        assert_eq!(engine.point_field_counts(), (12, 0));
        let index = engine.point_index_cache_metrics();
        assert_eq!((index.hits, index.misses), (4, 8));
        assert!(
            weights[0] < weights[1] && weights[1] == weights[2],
            "{weights:?}"
        );
        let direct = counted(&source, 16, 64, 0).0;
        series(&direct, "q", &south, RUN, None);
        assert_eq!(direct.point_index_cache_metrics().bytes, weights[1]);
        assert_eq!(engine.grid_cache_stats(), (0, 0), "no grid loaded");
    }
}

/// The first read of the source fetches its message whole and indexes it.
/// A later step is one prefix GET predicted from that index, at least what
/// the point needs and at most 2 % past the end of its last row; a repeat
/// is one range GET of exactly the rows' data; a point further south reads
/// on from the last row the prefix indexed, and its repeat is again exact.
#[test]
fn cold_reads_fetch_one_prefix_and_warm_reads_one_span() {
    let message = temperature();
    let len = message.len() as u64;
    let source = TestSource::new();
    let steps: Vec<_> = (0..2).map(|_| vec![("q", message.clone())]).collect();
    write_run(&source, "", RUN, &steps, true);
    let full = counted(&source, 0, 0, 0).0;
    let index = indexed(&message);
    let span = |point| index.span(&[point], None).unwrap();
    for point in [NORTH, MIDDLE, SOUTH] {
        let (engine, store) = counted(&source, 16, 0, 0);
        let one = |step| Some((step, step));
        assert_eq!(
            series(&engine, "q", &[point], RUN, one(0)),
            series(&full, "q", &[point], RUN, one(0))
        );
        assert_eq!(gets(&store), [("f000.grib2".into(), 0, len)]);

        assert_eq!(
            series(&engine, "q", &[point], RUN, one(1)),
            series(&full, "q", &[point], RUN, one(1))
        );
        let prefix = index.predict_prefix(&[point], message.len(), Kin::SameRun) as u64;
        assert_eq!(gets(&store), [("f001.grib2".into(), 0, prefix)]);
        let bound = index.prefix_bound(&[point]).unwrap() as u64;
        assert!(prefix >= span(point).end as u64);
        assert!(
            prefix as f64 <= 1.02 * bound as f64 + 1.0,
            "{prefix} for {bound}"
        );

        // The prefix indexed the rows through the point's: a repeat reads
        // exactly their data.
        assert_eq!(
            series(&engine, "q", &[point], RUN, one(1)),
            series(&full, "q", &[point], RUN, one(1))
        );
        let warm = span(point);
        assert_eq!(
            gets(&store),
            [("f001.grib2".into(), warm.start as u64, warm.len() as u64)]
        );
        assert!(warm.len() < 2048, "{warm:?}");

        // Further south: one read on, from inside the prefix, to exactly
        // the southern point's last value.
        let south = (0.0, -80.0);
        assert_eq!(
            series(&engine, "q", &[south], RUN, one(1)),
            series(&full, "q", &[south], RUN, one(1))
        );
        let read_on = gets(&store);
        let [(path, first, bytes)] = &read_on[..] else {
            panic!("one read on: {read_on:?}")
        };
        assert_eq!(path, "f001.grib2");
        assert!(*first < prefix && *first > index.header().data_start() as u64);
        assert_eq!(first + bytes, span(south).end as u64);
        series(&engine, "q", &[south], RUN, one(1));
        let exact = span(south);
        assert_eq!(
            gets(&store),
            [("f001.grib2".into(), exact.start as u64, exact.len() as u64)]
        );
        assert_eq!(
            fetched(&engine),
            [
                (1, len),
                (1, prefix),
                (3, warm.len() as u64 + bytes + exact.len() as u64),
                (0, 0)
            ]
        );
        assert_eq!(engine.point_field_counts(), (5, 0));
    }
}

/// A sibling whose northern half is constant predicts far too short a
/// prefix for a field noisy everywhere: the prefix's own descriptors then
/// give the exact end, so one continuation reads on to exactly there.
#[test]
fn a_short_prefix_takes_one_exact_continuation() {
    let spec = Complex::global();
    let nodes = (spec.ni * spec.nj) as usize;
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut noise = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        Value::Present((seed % 4000) as i64)
    };
    let calm: Vec<_> = (0..nodes)
        .map(|k| {
            if k < nodes / 2 {
                Value::Present(7)
            } else {
                noise()
            }
        })
        .collect();
    let noisy: Vec<_> = (0..nodes).map(|_| noise()).collect();
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
    let [(_, 0, prefix), (_, more_at, more)] = gets[..] else {
        panic!("a prefix and one continuation: {gets:?}")
    };
    assert_eq!(
        more_at, prefix,
        "the continuation starts where the prefix ends"
    );
    let needed = indexed(&noisy).span(&point, None).unwrap().end as u64;
    assert_eq!(prefix + more, needed, "and ends with the last needed value");
    assert_eq!(
        fetched(&engine).iter().map(|f| f.0).collect::<Vec<_>>(),
        [1, 1, 0, 1]
    );
}

/// A bitmapped field (GFS soil and land fields) places its values through
/// the bitmap, before Section 7: a cold read is one prefix as for any
/// field, and a warm one a prefix through the end of the row after the
/// point's, which the checkpoints give.
#[test]
fn bitmapped_fields_read_prefixes() {
    let message = soil();
    let source = TestSource::new();
    let steps: Vec<_> = (0..2).map(|_| vec![("q", message.clone())]).collect();
    write_run(&source, "", RUN, &steps, true);
    let full = counted(&source, 0, 0, 0).0;
    let index = indexed(&message);
    let one = Some((1, 1));
    for point in [NORTH, SOUTH] {
        let (engine, store) = counted(&source, 16, 0, 0);
        series(&engine, "q", &[point], RUN, Some((0, 0)));
        gets(&store);
        for round in 0..2 {
            assert_eq!(
                series(&engine, "q", &[point], RUN, one),
                series(&full, "q", &[point], RUN, one)
            );
            let expected = if round == 0 {
                index.predict_prefix(&[point], message.len(), Kin::SameRun)
            } else {
                index.prefix_bound(&[point]).unwrap()
            } as u64;
            assert_eq!(gets(&store), [("f001.grib2".into(), 0, expected)]);
        }
        assert_eq!(fetched(&engine)[1].0, 2, "two prefixes");
        assert!(fetched(&engine)[1].1 < 2 * message.len() as u64);
    }
}

/// A 5.3 message in a layout the reader does not take (rows south to north
/// here) is decoded in full from the bytes already read, and its parameter
/// then goes to the full decode directly.
#[test]
fn other_layouts_are_read_once_then_decoded_directly() {
    let spec = Complex {
        scan: 64,
        lat_first: -90_000_000,
        ..Complex::global()
    };
    let nodes = (spec.ni * spec.nj) as i64;
    let values: Vec<_> = (0..nodes).map(|k| Value::Present(k % 1000)).collect();
    let message = spec.message(&values);
    let source = TestSource::new();
    let steps: Vec<_> = (0..3).map(|_| vec![("q", message.clone())]).collect();
    write_run(&source, "", RUN, &steps, true);
    let full = counted(&source, 0, 0, 0).0;
    let (engine, store) = counted(&source, 16, 0, 0);
    assert_eq!(
        series(&engine, "q", &[NORTH, SOUTH], RUN, None),
        series(&full, "q", &[NORTH, SOUTH], RUN, None)
    );
    assert_eq!(engine.point_field_counts(), (0, 3));
    let gets = gets(&store);
    assert_eq!(gets.len(), 3, "each message once: {gets:?}");
    let key = ParameterKey {
        param: "q".into(),
        levtype: "pl".into(),
        level: Some(150),
    };
    assert!(!engine.source.point_index.as_ref().unwrap().may_read(&key));
}
