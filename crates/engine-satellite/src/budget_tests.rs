//! Which fetch budget each satellite download takes (#1011, #1055), and the
//! time bound on a poll's ingest (#1055), over a store whose GET bodies
//! stall past the request budget, on a paused clock
//! ([`ds_storage::test_util`]).

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use ds_core::config::{SatelliteConfig, SatelliteProductConfig};
use ds_storage::object_store::path::Path as ObjectPath;
use ds_storage::test_util::{run_stalled, StalledBody};
use ds_storage::DataStore;

use super::{SatelliteEngine, Source, FRAMES, INGEST_BUDGET, MAX_INGEST_PER_POLL};

/// A GOES-19 ABI band 13 crop, scan start 2026-09-25 (day 268) 19:00:19.
const C13: &str = "OR_ABI-L2-CMIPF-M6C13_G19_s20262681900199_e20262681909519_c20262681909592.nc";

/// The fixture under the name of a scan starting `minute` minutes after
/// 19:00.
fn scan(minute: u32) -> String {
    C13.replace("s20262681900199", &format!("s202626819{minute:02}199"))
}

fn scan_time(minute: u32) -> DateTime<Utc> {
    format!("2026-09-25T19:{minute:02}:00Z").parse().unwrap()
}

fn fixture() -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/goes19-abi")
            .join(C13),
    )
    .unwrap()
}

/// A GOES-R engine over `stalled`, listed like a local mirror.
fn engine(stalled: &Arc<StalledBody>) -> Arc<SatelliteEngine> {
    let dir = tempfile::tempdir().unwrap();
    let config = SatelliteConfig {
        provider: "goes-r".into(),
        data_path: Some(dir.path().to_string_lossy().into_owned()),
        endpoint: None,
        bucket: None,
        time_window: None,
        poll_interval_secs: 60,
        composites: Vec::new(),
        products: vec![SatelliteProductConfig {
            parameter: "ir_10_3".into(),
            title: "IR 10.3 µm brightness temperature".into(),
            unit: "K".into(),
            product: "ABI-L2-CMIPF".into(),
            band: Some(13),
            variable: "CMI".into(),
        }],
    };
    let mut engine = SatelliteEngine::new("budget", &config).unwrap();
    engine.source = Source::Directory {
        store: DataStore::new(stalled.clone()),
        base: ObjectPath::from(""),
    };
    Arc::new(engine)
}

/// The scan times the catalog holds.
fn held(engine: &SatelliteEngine) -> Vec<DateTime<Utc>> {
    engine.catalog.load().frames[0].keys().copied().collect()
}

/// #1011, #1055: the poll ingests a new scan under the background budget,
/// so a body stalled past the request budget still lands. Downloading an
/// evicted scan again, from the poll or for a request, takes the request
/// budget: a request may be waiting on it.
#[test]
fn ingest_takes_the_background_budget_and_refetches_the_request_budget() {
    let stalled = StalledBody::with_objects([(scan(0), fixture())]);
    let engine = engine(&stalled);
    let poll = Arc::clone(&engine);
    run_stalled(&stalled, move || poll.poll_once());
    assert_eq!(stalled.stalled(), 1);
    assert_eq!(
        held(&engine),
        [scan_time(0)],
        "the stalled scan is ingested"
    );

    let time = scan_time(0);
    let key = engine.frame_key(0, time);
    let paths = engine.catalog.load().frames[0][&time].clone();
    let evict = || {
        FRAMES.remove(&key);
    };

    evict();
    let poll = Arc::clone(&engine);
    let held = paths.clone();
    let reingested = run_stalled(&stalled, move || poll.reingest(0, time, &held));
    assert!(!reingested, "the re-download gave up at the request budget");
    assert_eq!(stalled.stalled(), 2);

    evict();
    let request = Arc::clone(&engine);
    let fetched = run_stalled(&stalled, move || request.frame(0, time, &paths.paths).err());
    let error = fetched
        .expect("the request-time fetch times out")
        .to_string();
    assert!(error.contains("timed out after 30s"), "{error}");
    assert_eq!(stalled.stalled(), 3);
}

/// #1055: a poll starts no ingest once [`INGEST_BUDGET`] is spent, so a
/// source that stalls every body holds the catalog for the budget and one
/// ingest, not for every new scan. The scans left over are ingested by the
/// next poll, newest first.
#[test]
fn a_poll_stops_ingesting_when_its_budget_is_spent() {
    // Each stalled ingest takes 60 s: the budget lets two start.
    assert_eq!(
        INGEST_BUDGET.as_secs(),
        2 * ds_storage::test_util::STALL.as_secs()
    );
    let minutes = [0, 10, 20, 30];
    assert_eq!(minutes.len(), MAX_INGEST_PER_POLL);
    let stalled =
        StalledBody::with_objects(minutes.iter().map(|&minute| (scan(minute), fixture())));
    let engine = engine(&stalled);

    let poll = Arc::clone(&engine);
    run_stalled(&stalled, move || poll.poll_once());
    assert_eq!(stalled.stalled(), 2, "no ingest starts after the budget");
    assert_eq!(
        held(&engine),
        [scan_time(20), scan_time(30)],
        "the newest two"
    );

    let poll = Arc::clone(&engine);
    run_stalled(&stalled, move || poll.poll_once());
    assert_eq!(stalled.stalled(), 4);
    assert_eq!(held(&engine), minutes.map(scan_time));
}
