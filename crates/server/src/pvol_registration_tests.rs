//! A PVOL source's per-site collections follow its catalog after the load
//! (#1462): the recovery loop registers a radar site a poll finds, local
//! source or remote, with the engine and caches reused, and never drops a
//! site whose volumes left the catalog. Over the committed polar volume
//! `testdata/pvol-cold-batch.h5` (site `test`), patched to other sites.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use engine_odim::PolarVolumeEngine;

use super::{recover_radar_once, AdminState, CollectionStatus, RecoveryOutcome};

/// The committed fixture as a volume of site `nod`, four characters: its
/// `/what/source` attribute is a fixed-length string that occurs once, and
/// its old-format HDF5 headers carry no checksums.
fn volume_of(nod: &str) -> Vec<u8> {
    let mut bytes = include_bytes!("../../../testdata/pvol-cold-batch.h5").to_vec();
    let (from, to) = (b"NOD:test", format!("NOD:{nod}"));
    assert_eq!(from.len(), to.len());
    let at = bytes.windows(from.len()).position(|w| w == from).unwrap();
    bytes[at..at + to.len()].copy_from_slice(to.as_bytes());
    bytes
}

/// A volume of `nod` dropped into `volumes` as an ingest does: written
/// under a hidden name, which no scan reads, then renamed into place.
fn deliver(volumes: &Path, nod: &str) {
    let hidden = volumes.join(format!(".{nod}.h5"));
    std::fs::write(&hidden, volume_of(nod)).unwrap();
    std::fs::rename(&hidden, volumes.join(format!("{nod}.h5"))).unwrap();
}

/// A server over one local PVOL source `base` reading `volumes`, with every
/// API a site serves, and the source's engine, its poll loop running every
/// second on the background runtime as `main` runs it.
fn pvol_server(root: &Path, base: &str, volumes: &Path) -> (AdminState, Arc<PolarVolumeEngine>) {
    let config_path = root.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[server]
host = "127.0.0.1"
port = 8000

[[collections]]
id = "{base}"
title = "Radar volumes"
description = "test"
engine_type = "odim-volume"
apis = ["edr", "wms", "maps", "tiles", "3dtiles", "features"]
data_path = {volumes:?}

[collections.odim]
prewarm_sweeps = 0
poll_interval_secs = 1
"#,
            volumes = volumes.to_str().unwrap()
        ),
    )
    .unwrap();
    let state = crate::watcher::tests::build_state(&config_path);
    let engine = state.odim_volume_engines.read().unwrap()[0].clone();
    let poller = engine.clone();
    crate::poll_runtime().spawn(async move { poller.poll_loop().await });
    (state, engine)
}

/// A temporary root with an empty `volumes` directory in it.
fn volumes_dir() -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let volumes = root.path().join("volumes");
    std::fs::create_dir(&volumes).unwrap();
    (root, volumes)
}

/// Wait for the poll to hold exactly the sites `nods`, sorted.
async fn polled(engine: &PolarVolumeEngine, nods: &[&str]) {
    let holds = || {
        engine
            .sites()
            .iter()
            .map(|(nod, _)| nod.clone())
            .eq(nods.iter().map(|n| n.to_string()))
    };
    tokio::time::timeout(Duration::from_secs(20), async {
        while !holds() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the poll holds {:?}, not {nods:?}", engine.sites()));
}

/// Whether every API a site serves registers `id`: EDR, WMS, Maps, Tiles
/// and 3D Tiles.
fn served(state: &AdminState, id: &str) -> bool {
    state.edr.load().engines.contains_key(id)
        && state.wms.load().engines.contains_key(id)
        && state.maps.load().engines.contains_key(id)
        && state.tiles.load().map_engines.contains_key(id)
        && state.tiles_3d.load().volume_engines.contains_key(id)
}

/// The status `/health` gives `id`, if it lists it.
async fn health_status(state: &AdminState, id: &str) -> Option<String> {
    use axum::response::IntoResponse;
    let response = super::health_handler(axum::extract::State(state.clone()))
        .await
        .into_response();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    json["collections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .map(|c| c["status"].as_str().unwrap().to_string())
}

fn ready(state: &AdminState, id: &str) -> bool {
    state
        .health
        .read()
        .unwrap()
        .iter()
        .any(|h| h.id == id && h.status == CollectionStatus::Ready)
}

/// A local source that loads over an empty directory is `Degraded` with no
/// site. Once a volume is dropped in, its poll finds the site and the
/// recovery loop registers the site's routes in every API, no reload: the
/// production outage the issue describes, where local drop directories were
/// empty at a restart and their sites stayed away until a manual reload.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_local_source_that_loads_empty_registers_its_first_site() {
    const BASE: &str = "pvol-late-empty";
    let site = format!("{BASE}-test");
    let (root, volumes) = volumes_dir();
    let (state, engine) = pvol_server(root.path(), BASE, &volumes);
    assert_eq!(
        health_status(&state, BASE).await.as_deref(),
        Some("degraded")
    );
    assert!(!served(&state, &site));
    assert_eq!(recover_radar_once(&state).unwrap(), RecoveryOutcome::Idle);

    deliver(&volumes, "test");
    polled(&engine, &["test"]).await;
    assert_eq!(
        recover_radar_once(&state).unwrap(),
        RecoveryOutcome::Recovered
    );
    assert!(served(&state, &site));
    assert!(state.features.load().engines.contains_key(BASE));
    assert_eq!(health_status(&state, &site).await.as_deref(), Some("ready"));
    assert_eq!(health_status(&state, BASE).await.as_deref(), Some("ready"));
    assert!(
        Arc::ptr_eq(&engine, &state.odim_volume_engines.read().unwrap()[0]),
        "the live engine, its catalog and poll loop, is reused"
    );
    // Registered once: the next pass has nothing to do.
    assert_eq!(recover_radar_once(&state).unwrap(), RecoveryOutcome::Idle);
    engine.shutdown();
}

/// A `Ready` source whose poll finds a second radar registers it on the next
/// pass, with the first site's engine and render caches reused: a tile of
/// the first site drawn before stays cached.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_ready_source_registers_a_site_that_starts_reporting() {
    const BASE: &str = "pvol-late-second";
    let (first, second) = (format!("{BASE}-test"), format!("{BASE}-tes2"));
    let (root, volumes) = volumes_dir();
    deliver(&volumes, "test");
    let (state, engine) = pvol_server(root.path(), BASE, &volumes);
    assert!(served(&state, &first) && ready(&state, &first));
    assert!(!served(&state, &second));

    let cache = state.wms.load().rendered_cache.clone();
    let key = ds_render::CacheKey {
        layer: format!("{first}/DBZH"),
        style: "default".into(),
        format: ds_render::ImageFormat::Png,
        crs: "EPSG:4326".into(),
        bbox: [0, 0, 1, 1],
        width: 1,
        height: 1,
        time: None,
        parameter: None,
        z: None,
        reference_time: None,
        content_version: 0,
        background: Default::default(),
    };
    cache.insert(
        key.clone(),
        ds_render::CachedRendered::new(axum::body::Bytes::from_static(b"warm")),
    );

    deliver(&volumes, "tes2");
    polled(&engine, &["tes2", "test"]).await;
    assert_eq!(
        recover_radar_once(&state).unwrap(),
        RecoveryOutcome::Recovered
    );
    assert!(served(&state, &second) && ready(&state, &second));
    assert!(served(&state, &first) && ready(&state, &first));
    assert!(state
        .wms
        .load()
        .styles
        .contains_key(&format!("{second}/DBZH")));
    assert!(Arc::ptr_eq(
        &engine,
        &state.odim_volume_engines.read().unwrap()[0]
    ));
    assert!(Arc::ptr_eq(&cache, &state.wms.load().rendered_cache));
    assert!(
        cache.get(&key).is_some(),
        "the first site's tiles stay warm"
    );
    assert_eq!(recover_radar_once(&state).unwrap(), RecoveryOutcome::Idle);
    engine.shutdown();
}

/// A site whose volumes leave the catalog (an outage longer than the
/// retention) keeps its collection: its absence alone starts no pass, and a
/// pass another site starts registers it again as it was, title and layer
/// styles included. Back, it needs nothing. An explicit reload still
/// registers only what the catalog holds, and the recovery loop registers
/// the site again once it returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_site_that_leaves_the_catalog_keeps_its_collection() {
    const BASE: &str = "pvol-late-aged";
    let gone = format!("{BASE}-tes2");
    let (root, volumes) = volumes_dir();
    deliver(&volumes, "test");
    deliver(&volumes, "tes2");
    let (state, engine) = pvol_server(root.path(), BASE, &volumes);
    assert!(served(&state, &gone) && ready(&state, &gone));
    let title = state.edr.load().collections[&gone].title.clone();
    let style = format!("{gone}/DBZH");
    assert!(state.wms.load().styles.contains_key(&style));

    std::fs::remove_file(volumes.join("tes2.h5")).unwrap();
    polled(&engine, &["test"]).await;
    assert_eq!(recover_radar_once(&state).unwrap(), RecoveryOutcome::Idle);
    assert!(served(&state, &gone) && ready(&state, &gone));

    // Another radar starts reporting: the pass that registers it keeps the
    // absent one.
    deliver(&volumes, "tes3");
    polled(&engine, &["tes3", "test"]).await;
    assert_eq!(
        recover_radar_once(&state).unwrap(),
        RecoveryOutcome::Recovered
    );
    assert!(served(&state, &format!("{BASE}-tes3")));
    assert!(served(&state, &gone) && ready(&state, &gone));
    assert_eq!(state.edr.load().collections[&gone].title, title);
    assert!(state.wms.load().styles.contains_key(&style));
    assert_eq!(health_status(&state, &gone).await.as_deref(), Some("ready"));
    assert_eq!(recover_radar_once(&state).unwrap(), RecoveryOutcome::Idle);

    // Back, it is registered already.
    deliver(&volumes, "tes2");
    polled(&engine, &["tes2", "tes3", "test"]).await;
    assert_eq!(recover_radar_once(&state).unwrap(), RecoveryOutcome::Idle);

    // An explicit reload registers what the catalog holds, as before.
    std::fs::remove_file(volumes.join("tes2.h5")).unwrap();
    polled(&engine, &["tes3", "test"]).await;
    let accepted = state.accepted_load.read().unwrap().clone().unwrap();
    super::apply_load(&state, accepted, false).unwrap();
    assert!(!served(&state, &gone));
    assert_eq!(
        health_status(&state, &gone).await.as_deref(),
        Some("degraded")
    );
    assert_eq!(recover_radar_once(&state).unwrap(), RecoveryOutcome::Idle);
    deliver(&volumes, "tes2");
    polled(&engine, &["tes2", "tes3", "test"]).await;
    assert_eq!(
        recover_radar_once(&state).unwrap(),
        RecoveryOutcome::Recovered
    );
    assert!(served(&state, &gone) && ready(&state, &gone));
    engine.shutdown();
}

/// A local source whose directory is missing still fails the load: no
/// engine, `Failed` health, no exception that would start the server with
/// every collection failed, and nothing for the recovery loop to retry. A
/// missing directory is a configuration error, not data still to come.
#[test]
fn a_local_source_without_its_directory_still_fails_the_load() {
    const BASE: &str = "pvol-late-missing";
    let (root, volumes) = volumes_dir();
    let missing = volumes.join("absent");
    let config_path = root.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "[server]\nhost = \"127.0.0.1\"\nport = 8000\n\
             [[collections]]\nid = \"{BASE}\"\ntitle = \"t\"\ndescription = \"d\"\n\
             engine_type = \"odim-volume\"\napis = [\"edr\", \"wms\"]\n\
             data_path = {missing:?}\n[collections.odim]\nprewarm_sweeps = 0\n",
            missing = missing.to_str().unwrap()
        ),
    )
    .unwrap();
    let state = crate::watcher::tests::build_state(&config_path);
    let health = state.health.read().unwrap().clone();
    let statuses: Vec<_> = health.iter().map(|h| (h.id.as_str(), h.status)).collect();
    assert_eq!(statuses, [(BASE, CollectionStatus::Failed)]);
    assert!(state.odim_volume_engines.read().unwrap().is_empty());
    let collections: Vec<_> = state
        .last_collections
        .read()
        .unwrap()
        .values()
        .cloned()
        .collect();
    assert!(
        !super::can_start_failed_radar(&collections),
        "startup refuses a server whose every collection failed"
    );
    assert!(!super::has_pending_radar(&collections, &health));
    assert_eq!(recover_radar_once(&state).unwrap(), RecoveryOutcome::Idle);
}
