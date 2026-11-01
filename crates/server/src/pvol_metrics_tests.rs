//! PVOL source observability (#193) through a real load of the committed
//! polar volume `testdata/pvol-cold-batch.h5` (site `test`, 2026-09-16
//! 12:00 UTC) and a copy five minutes later: each radar site collection's
//! data age in `/health` and the data age gauges, and the source's scan
//! and catalog families.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};

const BASE: &str = "pvol-obs-193";
const SITE: &str = "pvol-obs-193-test";

/// The time of the newest volume: the fixture's copy five minutes on.
fn volume_time() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 16, 12, 5, 0).unwrap()
}

/// A server state over a PVOL source `base` holding the fixture and a copy
/// of it five minutes later, and the source's directory.
fn pvol_state(base: &str) -> (tempfile::TempDir, super::AdminState) {
    let dir = tempfile::tempdir().unwrap();
    let volumes = dir.path().join("volumes");
    std::fs::create_dir(&volumes).unwrap();
    let fixture: &[u8] = include_bytes!("../../../testdata/pvol-cold-batch.h5");
    std::fs::write(volumes.join("202609161200_test_PVOL.h5"), fixture).unwrap();
    // The `/what` `time` attribute, the file's only `120000`.
    let at = fixture.windows(6).position(|w| w == b"120000").unwrap();
    let mut later = fixture.to_vec();
    later[at..at + 6].copy_from_slice(b"120500");
    std::fs::write(volumes.join("202609161205_test_PVOL.h5"), later).unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "[server]\nhost = \"127.0.0.1\"\nport = 8000\n\
             [[collections]]\nid = \"{base}\"\ntitle = \"t\"\ndescription = \"d\"\n\
             data_path = \"{}\"\nengine_type = \"odim-volume\"\n\
             apis = [\"edr\", \"features\"]\n\
             [collections.odim]\nprewarm_sweeps = 0\n",
            volumes.display()
        ),
    )
    .unwrap();
    let state = crate::watcher::tests::build_state(&config_path);
    (dir, state)
}

/// `/metrics` as the server encodes it.
fn metrics_text() -> String {
    use prometheus::Encoder as _;
    let mut text = Vec::new();
    prometheus::TextEncoder::new()
        .encode(&super::REGISTRY.gather(), &mut text)
        .unwrap();
    String::from_utf8(text).unwrap()
}

/// Every series of `family` in `text` whose labels start with `labels`, by
/// the rest of their labels.
fn series_in(text: &str, family: &str, labels: &str) -> BTreeMap<String, f64> {
    let prefix = format!("{family}{{{labels}");
    text.lines()
        .filter_map(|line| {
            let rest = line.strip_prefix(prefix.as_str())?;
            let (labels, value) = rest.rsplit_once(' ')?;
            Some((labels.to_owned(), value.parse().unwrap()))
        })
        .collect()
}

/// Every series of `family` in `/metrics` whose labels start with
/// `collection="<id>"`, by the rest of their labels.
fn series(family: &str, id: &str) -> BTreeMap<String, f64> {
    series_in(&metrics_text(), family, &format!("collection=\"{id}\""))
}

/// The data age, `collection_has_data` and expected interval series of the
/// radar site, as a data age refresh from `state` sets them: each labelled
/// `alerting="false"`, so the freshness alerts leave the site out, and none
/// without that label. Each family stays one family, with one HELP and one
/// TYPE line. Under the refresh lock, as the other data age tests read
/// theirs: another test's refresh would otherwise remove the site's series
/// before they are read.
fn site_data_age(state: &super::AdminState) -> [Option<f64>; 3] {
    let mut reported = super::DATA_AGE_REPORTED
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    super::refresh_data_age_gauge_locked(state, &mut reported);
    let text = metrics_text();
    [
        "collection_data_age_seconds",
        "collection_has_data",
        "collection_expected_interval_seconds",
    ]
    .map(|family| {
        let site = format!("collection=\"{SITE}\"");
        assert_eq!(
            series_in(&text, family, &site),
            BTreeMap::new(),
            "{family}: a site series without alerting=\"false\""
        );
        for kind in ["HELP", "TYPE"] {
            let line = format!("# {kind} {family} ");
            let count = text.lines().filter(|l| l.starts_with(&line)).count();
            assert!(count <= 1, "{family}: {count} {kind} lines");
        }
        series_in(&text, family, &format!("alerting=\"false\",{site}"))
            .get("}")
            .copied()
    })
}

/// #193: a PVOL site collection reports its newest volume as its data age,
/// in `/health` and in the gauges, with its volumes' spacing, its series
/// labelled `alerting="false"` so that the freshness alerts
/// leave it out; once its volumes leave the catalog it has
/// `collection_has_data` 0 and no age, and a reload that removes the source
/// removes its series. The base id, the site inventory, has none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pvol_sites_report_their_newest_volume_as_data_age() {
    use axum::response::IntoResponse;
    let (dir, state) = pvol_state(BASE);

    let before = Utc::now();
    let response = super::health_handler(axum::extract::State(state.clone()))
        .await
        .into_response();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let [age, has_data, interval] = site_data_age(&state);
    let after = Utc::now();

    let ages = (before - volume_time()).num_seconds()..=(after - volume_time()).num_seconds();
    let entry = |id: &str| {
        json["collections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == id)
            .unwrap_or_else(|| panic!("no /health entry for {id}: {json}"))
            .clone()
    };
    let site = entry(SITE);
    let data_age = site["data_age_secs"].as_i64().unwrap();
    assert!(ages.contains(&data_age), "data_age_secs {data_age}");
    assert!(entry(BASE)["data_age_secs"].is_null(), "the site inventory");
    let gauge = age.expect("a collection_data_age_seconds series") as i64;
    assert!(ages.contains(&gauge), "collection_data_age_seconds {gauge}");
    assert_eq!(has_data, Some(1.0));
    assert_eq!(interval, Some(300.0), "the volumes' spacing");
    assert_eq!(
        series("collection_has_data", BASE),
        BTreeMap::new(),
        "the site inventory"
    );

    // The site's volumes leave the catalog: the source now scans an empty
    // directory.
    let empty = dir.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    let config = state.last_collections.read().unwrap()[BASE]
        .odim
        .clone()
        .unwrap();
    let drained = engine_odim::PolarVolumeEngine::new(BASE, empty.to_str(), &config).unwrap();
    *state.odim_volume_engines.write().unwrap() = vec![Arc::new(drained)];
    let [age, has_data, interval] = site_data_age(&state);
    assert_eq!(age, None, "no data, no age");
    assert_eq!(has_data, Some(0.0));
    assert_eq!(interval, None, "no volumes, no spacing");

    // A reload removes the source.
    state.odim_volume_engines.write().unwrap().clear();
    assert_eq!(site_data_age(&state), [None; 3]);
}

/// #193: the source's scans and catalog on `/metrics`: the construction
/// scan, the volumes it parsed, and the catalog's volumes and sites, every
/// outcome's series there from the first scrape.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pvol_sources_export_their_scans_and_catalog() {
    // A source of its own: the counters are process-wide.
    const BASE: &str = "pvol-scans-193";
    let (_dir, state) = pvol_state(BASE);
    super::refresh_metrics(&state);

    let scans = series("pvol_scans_total", BASE);
    assert_eq!(
        scans,
        [
            (",result=\"failed\"}".to_owned(), 0.0),
            (",result=\"ok\"}".to_owned(), 1.0)
        ]
        .into()
    );
    let volumes = series("pvol_scan_volumes_total", BASE);
    assert_eq!(
        volumes,
        [
            (",outcome=\"cached\"}".to_owned(), 0.0),
            (",outcome=\"parse_failed\"}".to_owned(), 0.0),
            (",outcome=\"parsed\"}".to_owned(), 2.0),
            (",outcome=\"read_failed\"}".to_owned(), 0.0),
        ]
        .into()
    );
    let duration = series("pvol_last_scan_duration_seconds", BASE)["}"];
    assert!(duration > 0.0, "{duration}");
    assert_eq!(series("pvol_catalog_volumes", BASE)["}"], 2.0);
    assert_eq!(series("pvol_catalog_sites", BASE)["}"], 1.0);
    // A local source reads nothing from remote storage.
    assert_eq!(series("storage_bytes_read_total", BASE), BTreeMap::new());

    // A second scrape adds nothing: the counters follow the engine's counts.
    super::refresh_metrics(&state);
    assert_eq!(series("pvol_scans_total", BASE), scans);
    assert_eq!(series("pvol_scan_volumes_total", BASE), volumes);
}
