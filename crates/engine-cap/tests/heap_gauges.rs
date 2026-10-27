//! `CapEngine::memory`'s estimates against the heap the parts hold, measured
//! with a counting allocator (#1307): the geocode lookup, the WIS2
//! accumulator and the catalog, each within 25 %. One test: every thread's
//! allocations count, so nothing may run alongside.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use ds_core::config::{CapConfig, Wis2Config};
use ds_core::feature_engine::FeatureEngine;
use ds_core::heap::Counting;
use ds_wis2::{parse_notification, Notification, Payload, PayloadSource, Resolved};
use engine_cap::wis2::hint_from_bytes;
use engine_cap::CapEngine;

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const TOLERANCE: f64 = 0.25;

fn fixture(name: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/wis2-fixtures")
        .join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn notification(bytes: &[u8]) -> Notification {
    let v: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    let topic = v["topic"].as_str().unwrap();
    parse_notification(topic, &serde_json::to_vec(&v["message"]).unwrap()).unwrap()
}

fn config(geocode: bool) -> CapConfig {
    CapConfig {
        data_path: None,
        feed_url: None,
        poll_interval_secs: 60,
        language: None,
        status_filter: vec!["Actual".to_string()],
        default_ttl: None,
        circle_segments: 64,
        geocode_geometry: geocode.then(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../testdata/cap/emma-fi.geojson")
                .to_string_lossy()
                .into_owned()
        }),
        geocode_property: "code".to_string(),
        geocode_value_name: geocode.then(|| "EMMA_ID".to_string()),
        feed_allowlist: Vec::new(),
        wis2: Some(Wis2Config {
            topics: vec![
                "cache/a/wis2/eu-eumetnet-warnings/data/core/weather/advisories-warnings".into(),
            ],
            ..Wis2Config::default()
        }),
        retention_grace: "PT1H".to_string(),
        max_alerts: 10_000,
        geometry_links: true,
        bbox_fallback: false,
    }
}

fn resolved(n: Notification, bytes: Option<Vec<u8>>) -> Resolved {
    Resolved {
        notification: n,
        payload: bytes.map(|bytes| Payload {
            bytes: bytes.into(),
            source: PayloadSource::Downloaded("https://cache/x.xml".into()),
            media_type: Some("application/xml".into()),
            verified: None,
        }),
    }
}

/// Inside the MK alert's validity, and before the FI alert's end.
const NOW: DateTime<Utc> = DateTime::from_timestamp(1_789_202_410, 0).unwrap(); // 2026-09-12T08:20:10Z

/// Bytes live through the allocator now.
fn live() -> f64 {
    Counting::live() as f64
}

fn check(part: &str, estimate: u64, held: f64) {
    let estimate = estimate as f64;
    let error = (estimate - held) / held;
    println!(
        "{part}: held {held} bytes, estimated {estimate} ({:+.1} %)",
        100.0 * error
    );
    assert!(
        error.abs() <= TOLERANCE,
        "{part}: the estimate {estimate} is {:.1} % off the {held} bytes held",
        100.0 * error
    );
}

#[test]
fn memory_estimates_match_the_heap_held() {
    let mk_xml = String::from_utf8(fixture("meteoalarm-mk-alert.xml")).unwrap();
    let fi_xml = String::from_utf8(fixture("meteoalarm-fi-alert.xml")).unwrap();
    let mk_notification = fixture("meteoalarm-mk-notification.json");
    let mk_area = fixture("meteoalarm-mk-area.geojson");
    const MK_IDENTIFIER: &str = "2.49.0.0.807.0.MK.120926101959.8054";
    const FI_IDENTIFIER: &str = "2.49.0.0.246.0.FI.260913145444.hTwsi2Z20lXyoHTJB1og";

    // The geocode lookup: the same engine built with and without it.
    let before = live();
    let plain = CapEngine::new(&config(false), "cap-heap").unwrap();
    let without = live() - before;
    assert_eq!(plain.memory().geocode_bytes, None);
    drop(plain);
    let before = live();
    let engine = CapEngine::new(&config(true), "cap-heap").unwrap();
    let with = live() - before;
    check(
        "geocode",
        engine.memory().geocode_bytes.unwrap(),
        with - without,
    );

    // The accumulator: 400 MeteoAlarm MK alerts with their zone polygon
    // hint, 400 FMI alerts (three languages, geocodes the lookup resolves)
    // and 200 deletions of unknown objects, each its own data_id. Inputs
    // are built inside the measured loop and freed there: what stays is
    // what the accumulator keeps.
    let source = engine.wis2_source().unwrap();
    let before = live();
    for i in 0..400 {
        let mut n = notification(&mk_notification);
        n.id = format!("{:08x}-97c9-4dc1-a99a-e2ed3ec2d09f", i);
        n.data_id = format!("eu-eumetnet-warnings/meteoalarm/mk/{i:06}-b262dfd9-01af-438b-8149");
        let hint = hint_from_bytes(&n, &mk_area).unwrap();
        let xml = mk_xml.replace(MK_IDENTIFIER, &format!("{MK_IDENTIFIER}.{i}"));
        source.apply_with_hint(
            resolved(n, Some(xml.into_bytes())),
            Some((1, 0, hint)),
            "cap-heap",
            NOW,
        );
    }
    for i in 0..400 {
        let mut n = notification(&mk_notification);
        n.id = format!("{:08x}-fi00-4dc1-a99a-e2ed3ec2d09f", i);
        n.data_id = format!("eu-eumetnet-warnings/meteoalarm/fi/{i:06}-19a060e9-01af-438b");
        let xml = fi_xml.replace(FI_IDENTIFIER, &format!("{FI_IDENTIFIER}.{i}"));
        source.apply_with_hint(resolved(n, Some(xml.into_bytes())), None, "cap-heap", NOW);
    }
    for i in 0..200 {
        let mut n = notification(&mk_notification);
        n.id = format!("{:08x}-de1e-4dc1-a99a-e2ed3ec2d09f", i);
        n.data_id = format!("eu-eumetnet-warnings/meteoalarm/gone/{i:06}-b262dfd9-01af");
        source.apply_with_hint(resolved(n, None), None, "cap-heap", NOW);
    }
    let accumulator = live() - before;
    assert_eq!(source.len(), 800);

    // The catalog: one rebuild replaces the empty catalog the engine
    // started with (whose estimate is added back) with 2 000 area records.
    let empty_catalog = engine.memory().catalog_bytes as f64;
    let before = live();
    engine.refresh_with(|| NOW).unwrap();
    let catalog = live() - before + empty_catalog;
    assert_eq!(engine.feature_count(), 2_000);

    let memory = engine.memory();
    check(
        "accumulator",
        memory.accumulator_bytes.unwrap(),
        accumulator,
    );
    check("catalog", memory.catalog_bytes, catalog);
}
