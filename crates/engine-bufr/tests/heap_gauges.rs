//! The BUFR collection's memory estimates (`BufrEngine::store_memory`, the
//! `bufr_store_bytes`, `bufr_snapshot_bytes` and `bufr_deletion_index_bytes`
//! gauges, #1307) against the heap each part holds, measured with a
//! counting allocator. One test: every thread's allocations count.

use chrono::{DateTime, Duration, TimeZone, Utc};
use ds_core::config::{BufrConfig, Wis2Config};
use ds_core::heap::Counting;
use engine_bufr::decode::{xy_from_code, Element};
use engine_bufr::{BufrEngine, ObsReport};

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Bytes `f` leaves allocated.
fn held(f: impl FnOnce()) -> f64 {
    let before = Counting::live();
    f();
    (Counting::live() - before) as f64
}

fn check(name: &str, estimate: u64, held: f64) {
    let estimate = estimate as f64;
    println!(
        "{name}: held {held} bytes, estimated {estimate} ({:+.1} %)",
        100.0 * (estimate - held) / held
    );
    assert!(
        (estimate - held).abs() <= 0.25 * held,
        "{name}: estimate {estimate} is more than 25 % off the {held} bytes held"
    );
}

/// A SYNOP report: temperature, dew point, pressures, wind, cloud, visibility.
fn report(station: usize, time: DateTime<Utc>) -> ObsReport {
    let elements = [
        ("012101", 280.15),
        ("012103", 275.0),
        ("010004", 101_200.0),
        ("010051", 101_350.0),
        ("011001", 230.0),
        ("011002", 6.5),
        ("020010", 75.0),
        ("020001", 25_000.0),
    ]
    .map(|(code, value)| Element {
        xy: xy_from_code(code).unwrap(),
        value,
        period_hours: None,
    });
    ObsReport {
        station_id: format!("0-20000-0-{:05}", 2000 + station),
        name: Some(format!("STATION {station} AWS")),
        lat: -60.0 + (station % 120) as f64,
        lon: -170.0 + (station / 120) as f64 * 4.0,
        elevation: Some(12.0),
        time,
        elements: elements.to_vec(),
    }
}

#[test]
fn store_snapshot_and_deletion_index_estimates_match_the_heap_they_hold() {
    let now = Utc.with_ymd_and_hms(2026, 10, 27, 12, 0, 0).unwrap();
    let config = BufrConfig {
        data_path: None,
        wis2: Some(Wis2Config {
            topics: vec![
                "cache/a/wis2/+/data/core/weather/surface-based-observations/synop".into(),
            ],
            ..Wis2Config::default()
        }),
        poll_interval_secs: 60,
        retention: "PT24H".into(),
        max_stations: 50_000,
        stale_after: "PT2H".into(),
        position_radius_km: 25.0,
        builtin_parameters: true,
        parameters: vec![],
        map: None,
    };
    let engine = BufrEngine::new(&config, "obs-synop").unwrap();
    let empty = engine.store_memory();

    // 4 000 stations reporting hourly for a day, in arrival order: each
    // hour's round of the network, oldest first.
    let stations = 4_000;
    let reports: Vec<ObsReport> = (0..24)
        .flat_map(|h| {
            let t = now - Duration::hours(23 - h);
            (0..stations).map(move |s| report(s, t))
        })
        .collect();

    let store = held(|| assert_eq!(engine.ingest_reports(&reports, now), reports.len()));
    let snapshot = held(|| engine.snapshot_if_dirty());
    let memory = engine.store_memory();
    check("store", memory.store_bytes, store);
    check(
        "snapshot",
        memory.snapshot_bytes - empty.snapshot_bytes,
        snapshot,
    );

    // The deletion index of a day's firehose: 20 000 bulletins, each with a
    // ~100-character data_id, producing one to three stations' rows.
    let index = held(|| {
        for i in 0..20_000usize {
            let data_id = format!(
                "wis2/de-dwd-gts-to-wis2/data/core/weather/surface-based-observations/synop/SNDL{:02}_EDZW_{:06}",
                i % 99,
                i
            );
            let t = now - Duration::hours((i % 24) as i64);
            let keys = (0..1 + i % 3)
                .map(|k| (format!("0-20000-0-{:05}", 2000 + (i * 3 + k) % stations), t))
                .collect();
            engine.record_deletion_keys(data_id, keys);
        }
    });
    let (entries, bytes) = engine.store_memory().deletion_index.unwrap();
    assert_eq!(entries, 20_000);
    check("deletion index", bytes, index);
}
