//! The duplicate filter's heap estimate (`Dedup::heap_bytes`, the
//! `wis2_dedup_bytes` gauge, #1307) against the heap it holds, measured
//! with a counting allocator. One test: every thread's allocations count.

use chrono::{DateTime, Duration, TimeZone, Utc};
use ds_core::heap::Counting;
use ds_wis2::{Dedup, Notification};

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn notification(id: String, data_id: String, pubtime: DateTime<Utc>) -> Notification {
    Notification {
        topic: "cache/a/wis2/se-smhi/data/core/weather/surface-based-observations/synop".into(),
        centre_id: Some("se-smhi".into()),
        id,
        data_id,
        pubtime,
        datetime: None,
        start_datetime: None,
        end_datetime: None,
        geometry: None,
        integrity: None,
        content: None,
        links: vec![],
        metadata_id: None,
        wigos_station_identifier: None,
        global_cache: None,
        extra: Default::default(),
    }
}

/// A uuid-shaped message id.
fn uuid(i: usize) -> String {
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        i,
        i % 65_536,
        i % 4096,
        (i * 7) % 4096,
        i * 31
    )
}

#[test]
fn dedup_estimate_matches_the_heap_it_holds() {
    let start = Utc.with_ymd_and_hms(2026, 10, 27, 0, 0, 0).unwrap();
    // A firehose over two hours against the one-hour window, so it prunes
    // as it goes: 40 000 data objects, each arriving from six Global Caches
    // (the five later copies are duplicates), one in ten republished with a
    // newer pubtime (a re-queue).
    let objects = 40_000;
    let mut stream = Vec::new();
    for i in 0..objects {
        let data_id = format!(
            "wis2/se-smhi/data/core/weather/surface-based-observations/synop/WIGOS_0-20000-0-{:05}_20261027T{:04}00",
            i % 9_511,
            i % 2_400
        );
        let at = start + Duration::milliseconds(i as i64 * 180);
        for copy in 0..6 {
            stream.push((notification(uuid(i * 8 + copy), data_id.clone(), at), at));
        }
        if i % 10 == 0 {
            let later = at + Duration::seconds(1);
            stream.push((notification(uuid(i * 8 + 7), data_id, later), later));
        }
    }

    let before = Counting::live();
    let mut dedup = Dedup::new(Duration::hours(1));
    for (n, at) in &stream {
        dedup.accept_at(n, *at);
    }
    let held = (Counting::live() - before) as f64;
    let estimate = dedup.heap_bytes() as f64;
    println!(
        "dedup: {} data_ids, held {held} bytes, estimated {estimate} ({:+.1} %)",
        dedup.len(),
        100.0 * (estimate - held) / held
    );
    assert!(dedup.len() > 10_000, "the window holds a realistic set");
    assert!(
        (estimate - held).abs() <= 0.25 * held,
        "estimate {estimate} is more than 25 % off the {held} bytes held"
    );
}
