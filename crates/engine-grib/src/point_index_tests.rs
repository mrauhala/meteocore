//! Position queries through the CCSDS point index (#1300): the same
//! responses as the full decode, bit for bit, with no grid decoded or
//! cached on the point path; one read per field whatever the message turns
//! out to hold; and the full decode wherever it serves a field better.

use super::*;
use crate::catalog::ParameterKey;
use crate::test_support::{message, TestSource};

fn fixture() -> Vec<u8> {
    std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../testdata/grib-local/sample-message.grib2"
    ))
    .unwrap()
}

/// One step file per message, each `param` at `level`. A tail record after
/// it gives the field an indexed length, as in real indexes.
fn source(param: &str, level: &str, messages: &[Vec<u8>]) -> TestSource {
    let source = TestSource::new();
    for (step, bytes) in messages.iter().enumerate() {
        source.write(
            &format!("f{step:03}"),
            &[
                (param, level, bytes.clone()),
                ("TAIL", "surface", message(0, 0.0, [0; 4], 1, 0)),
            ],
            step as u32,
        );
    }
    source
}

/// The pressure view of `source`'s `q` at 150 hPa with these budgets in
/// MiB, 0 disabling each.
fn view(source: &TestSource, point_index_mb: u64, message_mb: u64, grid_mb: u64) -> GribEngine {
    let mut config = source.config();
    config.point_index_cache_mb = Some(point_index_mb);
    config.message_cache_mb = message_mb;
    config.grid_cache_mb = grid_mb;
    config.level_types = Some(vec![GribLevelType::Pressure]);
    config.parameters = Some(vec!["q".into(), "TMP".into()]);
    let owner = GribEngine::new("points", &config).unwrap();
    owner.level_collections().into_iter().next().unwrap()
}

/// Seams, poles and a sweep, as one MULTIPOINT batch.
fn points() -> Vec<String> {
    let mut points = Vec::new();
    for lat in [90.0, 89.9, 60.32, 0.0, -33.9, -89.9, -90.0] {
        for lon in [-180.0, -179.9, 0.0, 24.96, 179.9, 180.0] {
            points.push(format!("POINT({lon} {lat})"));
        }
    }
    for i in 0..40 {
        points.push(format!(
            "POINT({} {})",
            -179.0 + i as f64 * 8.9,
            88.0 - i as f64 * 4.4
        ));
    }
    points
}

/// Every point's series of `parameter` at `z`: values as bits, then label
/// and unit.
fn query(
    engine: &GribEngine,
    parameter: &str,
    z: f64,
    points: &[String],
) -> Vec<(Vec<Option<u64>>, String, String)> {
    let mut output = Vec::new();
    engine
        .query_positions(
            points,
            None,
            Some(&[parameter.to_owned()]),
            Some(&[z]),
            None,
            &mut |response| {
                let CoverageResponse::Single(result) = response else {
                    panic!("expected a series")
                };
                let description = &result.parameters[parameter];
                output.push((
                    result.ranges[parameter]
                        .values
                        .iter()
                        .map(|v| v.map(f64::to_bits))
                        .collect(),
                    description.label.clone(),
                    description.unit.clone(),
                ));
                Ok(())
            },
        )
        .unwrap();
    output
}

fn q_key() -> ParameterKey {
    ParameterKey {
        param: "q".into(),
        levtype: "pl".into(),
        level: Some(150),
    }
}

#[test]
fn ccsds_points_match_the_full_decode_without_a_grid() {
    let source = source("q", "150 mb", &vec![fixture(); 4]);
    let points = points();
    let full = view(&source, 0, 64, 64);
    let partial = view(&source, 16, 64, 64);
    let expected = query(&full, "q", 150.0, &points);
    assert_eq!(full.point_field_counts(), (0, 4));
    assert_eq!(full.grid_cache_utilization().2, 4);
    assert!(expected.iter().any(|(values, _, _)| values[0].is_some()));
    for round in 1..=2 {
        assert_eq!(query(&partial, "q", 150.0, &points), expected);
        assert_eq!(partial.point_field_counts(), (4 * round, 0));
        assert_eq!(
            partial.grid_cache_utilization(),
            (0, 64 * ds_cache::MIB, 0),
            "nothing decoded into the grid cache"
        );
        assert_eq!(
            partial.grid_cache_stats(),
            (0, 0),
            "no grid loaded, no grid miss"
        );
    }
    // Four indexes built, then reused; the messages were fetched once.
    let index = partial.point_index_cache_metrics();
    assert_eq!((index.hits, index.misses), (4, 4));
    assert!(index.bytes > 4 * 1000, "{}", index.bytes);
    let messages = partial.message_cache_metrics();
    assert_eq!((messages.hits, messages.misses), (4, 4));
    assert!(partial
        .source
        .point_index
        .as_ref()
        .unwrap()
        .may_read(&q_key()));
}

/// Without a compressed-message cache a field the decoded-grid cache keeps
/// is read again from it at no cost, so it takes the full decode; a window
/// the grid cache cannot hold reads through the index.
#[test]
fn only_grids_the_cache_would_keep_skip_the_index_without_a_message_cache() {
    let source = source("q", "150 mb", &vec![fixture(); 4]);
    let points = points();
    let expected = query(&view(&source, 0, 0, 0), "q", 150.0, &points);

    let kept = view(&source, 16, 0, 64);
    assert_eq!(query(&kept, "q", 150.0, &points), expected);
    assert_eq!(kept.point_field_counts(), (0, 4));
    assert_eq!(kept.grid_cache_utilization().2, 4);
    let before = kept.storage_bytes_read();
    assert_eq!(query(&kept, "q", 150.0, &points), expected);
    assert_eq!(
        kept.storage_bytes_read(),
        before,
        "served from the grid cache"
    );

    // Four 0.25° grids do not fit 8 MiB: the window bypasses the cache.
    let bypassing = view(&source, 16, 0, 8);
    let before = bypassing.storage_bytes_read();
    assert_eq!(query(&bypassing, "q", 150.0, &points), expected);
    assert_eq!(bypassing.point_field_counts(), (4, 0));
    assert_eq!(bypassing.grid_cache_utilization().2, 0);
    assert_eq!(
        bypassing.storage_bytes_read() - before,
        4 * fixture().len() as u64,
        "one read per field"
    );
}

/// #1308 acceptance: a window the grid cache cannot hold is read around
/// it, and each such read counts as a bypass, not a miss, so a meteogram's
/// one-pass read stays out of the map workload's hit ratio. It inserts
/// nothing and evicts nothing.
#[test]
fn a_window_read_around_the_grid_cache_counts_bypasses_not_misses() {
    let source = source("q", "150 mb", &vec![fixture(); 4]);
    let points = points();
    let expected = query(&view(&source, 0, 0, 0), "q", 150.0, &points);
    // No point index: the full decode, of four 0.25° grids that do not fit
    // 8 MiB.
    let bypassing = view(&source, 0, 0, 8);
    assert_eq!(query(&bypassing, "q", 150.0, &points), expected);
    let metrics = bypassing.grid_cache_metrics();
    assert_eq!((metrics.hits, metrics.misses), (0, 0));
    assert_eq!(metrics.churn.bypasses, 4);
    assert_eq!(
        (metrics.churn.inserted_bytes, metrics.churn.evictions()),
        (0, 0)
    );
}

/// A decoded grid a map left in the cache serves its field as it is.
#[test]
fn resident_grids_are_used_as_they_are() {
    let source = source("q", "150 mb", &vec![fixture(); 3]);
    let engine = view(&source, 16, 64, 64);
    let catalog = engine.catalog();
    let run = catalog.latest_run().unwrap();
    let keys = GribEngine::keys_at_level(
        catalog.parameter_keys(&run.reference_time).unwrap(),
        Some(150.0),
    );
    let first = run.steps.values().next().unwrap();
    engine.fetch_grid(first, "q", &keys).unwrap();
    assert_eq!(engine.grid_cache_utilization().2, 1);
    assert_eq!(engine.grid_cache_stats(), (0, 1));
    let points = points();
    let expected = query(&view(&source, 0, 0, 0), "q", 150.0, &points);
    assert_eq!(query(&engine, "q", 150.0, &points), expected);
    assert_eq!(engine.point_field_counts(), (2, 1));
    assert_eq!(engine.point_index_cache_metrics().misses, 2);
    assert_eq!(engine.grid_cache_utilization().2, 1);
    // One hit for the resident grid; the two fields read through the index
    // add no miss.
    assert_eq!(engine.grid_cache_stats(), (1, 1));
}

/// The grid cache counts what the full path would: a hit for a resident
/// grid and a miss for each field decoded in full, here a payload the index
/// cannot decode, which is then kept; nothing for a field read through the
/// index.
#[test]
fn the_grid_cache_counts_only_grids_found_or_decoded() {
    let good = fixture();
    let source = source("q", "150 mb", &[good.clone(), cut_payload(&good), good]);
    let points = points();
    let full = view(&source, 0, 64, 64);
    let partial = view(&source, 16, 64, 64);
    let expected = query(&full, "q", 150.0, &points);
    assert_eq!(query(&partial, "q", 150.0, &points), expected);
    assert_eq!(partial.point_field_counts(), (2, 1));
    assert_eq!(partial.grid_cache_stats(), (0, 1));
    assert_eq!(
        partial.grid_cache_utilization().2,
        1,
        "the full decode is kept"
    );
    assert_eq!(query(&partial, "q", 150.0, &points), expected);
    assert_eq!(partial.point_field_counts(), (4, 2));
    assert_eq!(partial.grid_cache_stats(), (1, 1));
    // The full path counts the same for that field, and its own for the rest.
    assert_eq!(query(&full, "q", 150.0, &points), expected);
    assert_eq!(full.grid_cache_stats(), (3, 3));
}

/// Simple packing (or any layout the index cannot read) is decoded in full
/// from the bytes already read, never fetched twice, and the parameter then
/// goes to the full decode directly.
#[test]
fn other_packings_are_read_once_then_decoded_directly() {
    let messages: Vec<_> = (0..6)
        .map(|step| message(0, 280.0 + step as f32, [0, 2, 4, 6], 100, 85000))
        .collect();
    let source = source("TMP", "850 mb", &messages);
    let points = ["POINT(0.3 0.6)".to_owned(), "POINT(0.9 0.1)".to_owned()];
    let expected = query(&view(&source, 0, 0, 0), "TMP", 850.0, &points);
    let engine = view(&source, 16, 0, 0);
    let before = engine.storage_bytes_read();
    assert_eq!(query(&engine, "TMP", 850.0, &points), expected);
    assert_eq!(
        engine.storage_bytes_read() - before,
        6 * messages[0].len() as u64
    );
    assert_eq!(engine.point_field_counts(), (0, 6));
    let key = ParameterKey {
        param: "TMP".into(),
        levtype: "pl".into(),
        level: Some(850),
    };
    let index = engine.source.point_index.as_ref().unwrap();
    assert!(!index.may_read(&key));
    assert_eq!(engine.point_index_cache_metrics().misses, 0);
    assert_eq!(query(&engine, "TMP", 850.0, &points), expected);
}

/// The unreadable mark lasts until the catalog next publishes: a new step
/// may pack the parameter differently, so it is tried again, and marked
/// again on its next unreadable message.
#[test]
fn publishing_the_catalog_forgets_unreadable_parameters() {
    let messages: Vec<_> = (0..2)
        .map(|step| message(0, 280.0 + step as f32, [0, 2, 4, 6], 100, 85000))
        .collect();
    let source = source("TMP", "850 mb", &messages);
    let engine = view(&source, 16, 0, 0);
    let key = ParameterKey {
        param: "TMP".into(),
        levtype: "pl".into(),
        level: Some(850),
    };
    let index = engine.source.point_index.as_ref().unwrap();
    let points = ["POINT(0.3 0.6)".to_owned()];
    query(&engine, "TMP", 850.0, &points);
    assert!(!index.may_read(&key));

    source.write(
        "f002",
        &[
            ("TMP", "850 mb", message(0, 282.0, [0, 2, 4, 6], 100, 85000)),
            ("TAIL", "surface", message(0, 0.0, [0; 4], 1, 0)),
        ],
        2,
    );
    engine.scan_once().unwrap();
    assert_eq!(engine.catalog().latest_run().unwrap().steps.len(), 3);
    assert!(index.may_read(&key), "a published catalog clears the mark");
    let series = query(&engine, "TMP", 850.0, &points);
    assert_eq!(series[0].0.len(), 3);
    assert!(!index.may_read(&key), "learned again from the new run");
}

/// An unset `point_index_cache_mb` follows `message_cache_mb` (#1333): the
/// indexes of a window of GFS 0.25° template 5.3 messages, about 115 KB for
/// 0.81 MB each, fit the index cache whenever the messages fit the message
/// cache. A size set, 0 included, is kept; with neither set, 16 MiB.
#[test]
fn an_unset_point_index_cache_holds_the_indexes_of_the_cached_messages() {
    const MESSAGE: u64 = 810_000;
    const INDEX: u64 = 115_000;
    let source = TestSource::new();
    // The budgets as a config file sets them.
    let capacities = |budgets: serde_json::Value| {
        let mut config = serde_json::json!({
            "data_path": source.dir.to_str().unwrap(),
            "index_format": "wgrib2", "index_suffix": ".idx",
        });
        let fields = config.as_object_mut().unwrap();
        fields.extend(budgets.as_object().unwrap().clone());
        let config: GribConfig = serde_json::from_value(config).unwrap();
        let engine = GribEngine::new("budgets", &config).unwrap();
        (
            engine.point_index_cache_metrics().capacity_bytes,
            engine.message_cache_metrics().capacity_bytes,
        )
    };
    let mib = ds_cache::MIB;
    let (index, message) = capacities(serde_json::json!({ "message_cache_mb": 1024 }));
    assert_eq!(message, 1024 * mib);
    assert!(
        index / INDEX >= message / MESSAGE,
        "{} indexes for {} messages",
        index / INDEX,
        message / MESSAGE
    );
    for (budgets, expected) in [
        // Never below 16 MiB, whatever the message cache.
        (serde_json::json!({ "message_cache_mb": 64 }), 16),
        (serde_json::json!({}), 16),
        (
            serde_json::json!({ "message_cache_mb": 1024, "point_index_cache_mb": 16 }),
            16,
        ),
        (
            serde_json::json!({ "message_cache_mb": 1024, "point_index_cache_mb": 0 }),
            0,
        ),
    ] {
        assert_eq!(capacities(budgets.clone()).0, expected * mib, "{budgets}");
    }
}

/// A payload the index cannot decode (Section 7 cut short, which the full
/// decode zero-fills) is answered by the full decode; a missing file is a
/// null in both.
#[test]
fn broken_messages_answer_as_the_full_decode_does() {
    let good = fixture();
    let source = source(
        "q",
        "150 mb",
        &[good.clone(), cut_payload(&good), good.clone()],
    );
    let full = view(&source, 0, 0, 0);
    let partial = view(&source, 16, 0, 0);
    std::fs::remove_file(source.dir.join("f002.grib2")).unwrap();
    let points = points();
    let expected = query(&full, "q", 150.0, &points);
    assert!(expected.iter().all(|(values, _, _)| values[2].is_none()));
    assert!(expected.iter().any(|(values, _, _)| values[1].is_some()));
    assert_eq!(query(&partial, "q", 150.0, &points), expected);
    // The good field through the index, the cut one decoded in full.
    assert_eq!(partial.point_field_counts(), (1, 1));
    assert!(partial
        .source
        .point_index
        .as_ref()
        .unwrap()
        .may_read(&q_key()));
}

/// `message` with its last 1000 payload bytes dropped and Section 7 and
/// the indicator saying so: the full decode zero-fills the missing values,
/// the index refuses the payload.
fn cut_payload(message: &[u8]) -> Vec<u8> {
    let mut short = message[..message.len() - 4 - 1000].to_vec();
    short.extend(b"7777");
    // Walk the sections from the end of the indicator to Section 7.
    let mut s7 = 16;
    while short[s7 + 4] != 7 {
        s7 += u32::from_be_bytes(short[s7..s7 + 4].try_into().unwrap()) as usize;
    }
    let s7_len = u32::from_be_bytes(short[s7..s7 + 4].try_into().unwrap()) - 1000;
    short[s7..s7 + 4].copy_from_slice(&s7_len.to_be_bytes());
    let total = short.len() as u64;
    short[8..16].copy_from_slice(&total.to_be_bytes());
    short
}

/// A repeated 41-step point query with every message in the compressed
/// cache, the point index against the full decode. Any GRIB2 message
/// serves; a 0.1° CCSDS field shows the difference:
/// `MC_CCSDS_REPLAY=ifs01-2t.grib2 cargo test -p engine-grib --release
/// point_index_replay -- --ignored --nocapture`. Prints measurements; no
/// thresholds.
#[test]
#[ignore = "needs MC_CCSDS_REPLAY; prints measurements"]
fn point_index_replay() {
    let path = std::env::var("MC_CCSDS_REPLAY").expect("MC_CCSDS_REPLAY");
    let bytes = std::fs::read(&path).unwrap();
    let steps = 41;
    let source = source("q", "150 mb", &vec![bytes.clone(); steps]);
    let points = [
        "POINT(24.96 60.32)".to_owned(),
        "POINT(21.0 52.2)".to_owned(),
        "POINT(139.8 35.6)".to_owned(),
    ];
    let budget = (bytes.len() as u64 * steps as u64).div_ceil(ds_cache::MIB) + 16;
    let mut timings = Vec::new();
    let mut answers = Vec::new();
    for (name, index_mb) in [("full", 0), ("partial", 16)] {
        let engine = view(&source, index_mb, budget, 256);
        let start = Instant::now();
        let first = query(&engine, "q", 150.0, &points);
        let cold = start.elapsed();
        let mut warm = Vec::new();
        for _ in 0..5 {
            let start = Instant::now();
            assert_eq!(query(&engine, "q", 150.0, &points), first);
            warm.push(start.elapsed().as_secs_f64() * 1e3);
        }
        warm.sort_by(f64::total_cmp);
        timings.push(format!(
            "{name}: cold {:.0} ms, warm median {:.1} ms (min {:.1}, max {:.1}), fields {:?}, grid cache {:?}",
            cold.as_secs_f64() * 1e3,
            warm[2],
            warm[0],
            warm[4],
            engine.point_field_counts(),
            engine.grid_cache_utilization(),
        ));
        answers.push(first);
    }
    assert_eq!(answers[0], answers[1], "identical responses");
    eprintln!("{path}: {steps} steps, {} points", points.len());
    for line in timings {
        eprintln!("  {line}");
    }
}
