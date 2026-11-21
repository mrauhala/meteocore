//! A PVOL source's per-site collections follow its catalog after the load
//! (#1462): the recovery loop registers a radar site a poll finds, local
//! source or remote, with the engine and caches reused, and never drops a
//! site whose volumes left the catalog: `/health` lists that site
//! `Degraded` until they return (#1511). Over the committed polar volume
//! `testdata/pvol-cold-batch.h5` (site `test`), patched to other sites.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use engine_odim::PolarVolumeEngine;
use tower::ServiceExt;

use super::{recover_radar_once, AdminState, CollectionStatus, RecoveryOutcome, RADAR_SITE_ABSENT};

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

/// The status and the error `/health` gives `id`, if it lists it.
async fn health_of(state: &AdminState, id: &str) -> Option<(String, Option<String>)> {
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
        .map(|c| {
            (
                c["status"].as_str().unwrap().to_string(),
                c["error"].as_str().map(str::to_string),
            )
        })
}

/// The status `/health` gives `id`, if it lists it.
async fn health_status(state: &AdminState, id: &str) -> Option<String> {
    health_of(state, id).await.map(|(status, _)| status)
}

/// What `/health` lists a site whose volumes left the catalog as, on every
/// path (#1511).
fn absent() -> Option<(String, Option<String>)> {
    Some(("degraded".into(), Some(RADAR_SITE_ABSENT.into())))
}

/// Whether the recovery loop's own check would start a pass for a pending
/// radar source, over the load snapshot and over the live health `/health`
/// reports alike.
fn pending(state: &AdminState) -> bool {
    let collections: Vec<_> = state
        .last_collections
        .read()
        .unwrap()
        .values()
        .cloned()
        .collect();
    let snapshot = state.health.read().unwrap().clone();
    super::has_pending_radar(&collections, &snapshot)
        || super::has_pending_radar(&collections, &super::effective_health(state))
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
/// retention) keeps its collection, which `/health` lists `Degraded` while
/// they are away (#1511): its absence alone starts no pass, and a pass
/// another site starts registers it again as it was, title and layer styles
/// included, still `Degraded`. Back, it is `Ready` with no pass. An explicit
/// reload still registers only what the catalog holds and lists the absent
/// site as the recovery path does, and the recovery loop registers the site
/// again once it returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_site_that_leaves_the_catalog_keeps_its_collection() {
    const BASE: &str = "pvol-late-aged";
    let gone = format!("{BASE}-tes2");
    let (root, volumes) = volumes_dir();
    deliver(&volumes, "test");
    deliver(&volumes, "tes2");
    let (state, engine) = pvol_server(root.path(), BASE, &volumes);
    assert!(served(&state, &gone) && ready(&state, &gone));
    assert_eq!(health_of(&state, &gone).await, Some(("ready".into(), None)));
    let title = state.edr.load().collections[&gone].title.clone();
    let style = format!("{gone}/DBZH");
    assert!(state.wms.load().styles.contains_key(&style));

    std::fs::remove_file(volumes.join("tes2.h5")).unwrap();
    polled(&engine, &["test"]).await;
    assert_eq!(health_of(&state, &gone).await, absent());
    assert_eq!(health_status(&state, BASE).await.as_deref(), Some("ready"));
    // The snapshot keeps the registered site `Ready`, so neither the
    // zero-`Ready` reload guard nor the loop's back-off counts it, and the
    // loop starts no pass for it, pass after pass.
    assert!(served(&state, &gone) && ready(&state, &gone));
    assert!(!pending(&state));
    for _ in 0..3 {
        assert_eq!(recover_radar_once(&state).unwrap(), RecoveryOutcome::Idle);
    }

    // Another radar starts reporting: the pass that registers it keeps the
    // absent one, `Degraded` as before, and is no pending pass itself.
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
    assert_eq!(health_of(&state, &gone).await, absent());
    assert!(!pending(&state));
    assert_eq!(recover_radar_once(&state).unwrap(), RecoveryOutcome::Idle);

    // Back, it is registered already and `Ready` again.
    deliver(&volumes, "tes2");
    polled(&engine, &["tes2", "tes3", "test"]).await;
    assert_eq!(health_of(&state, &gone).await, Some(("ready".into(), None)));
    assert_eq!(recover_radar_once(&state).unwrap(), RecoveryOutcome::Idle);

    // An explicit reload registers what the catalog holds, as before, and
    // lists the site it drops as the recovery path lists the one it keeps.
    std::fs::remove_file(volumes.join("tes2.h5")).unwrap();
    polled(&engine, &["tes3", "test"]).await;
    assert_eq!(health_of(&state, &gone).await, absent());
    let accepted = state.accepted_load.read().unwrap().clone().unwrap();
    super::apply_load(&state, accepted, false).unwrap();
    assert!(!served(&state, &gone));
    assert_eq!(health_of(&state, &gone).await, absent());
    assert!(!pending(&state));
    assert_eq!(recover_radar_once(&state).unwrap(), RecoveryOutcome::Idle);
    // A pass another site starts keeps the dropped site's entry, and does
    // not back off for it.
    deliver(&volumes, "tes4");
    polled(&engine, &["tes3", "tes4", "test"]).await;
    assert_eq!(
        recover_radar_once(&state).unwrap(),
        RecoveryOutcome::Recovered
    );
    assert!(served(&state, &format!("{BASE}-tes4")) && !served(&state, &gone));
    assert_eq!(health_of(&state, &gone).await, absent());
    assert!(!pending(&state));
    assert_eq!(recover_radar_once(&state).unwrap(), RecoveryOutcome::Idle);
    deliver(&volumes, "tes2");
    polled(&engine, &["tes2", "tes3", "tes4", "test"]).await;
    // Not `Ready` before the pass that serves it again.
    assert_eq!(health_of(&state, &gone).await, absent());
    assert_eq!(
        recover_radar_once(&state).unwrap(),
        RecoveryOutcome::Recovered
    );
    assert!(served(&state, &gone) && ready(&state, &gone));
    assert_eq!(health_of(&state, &gone).await, Some(("ready".into(), None)));
    engine.shutdown();
}

/// The site's layer in WMS GetCapabilities: the `<Layer>` element whose
/// title is `title`, children included.
fn site_layer(capabilities: &str, title: &str) -> String {
    let at = capabilities
        .find(&format!("<Title>{title}</Title>"))
        .unwrap_or_else(|| panic!("no layer {title} in {capabilities}"));
    let start = capabilities[..at].rfind("<Layer").unwrap();
    let (mut depth, mut at) = (0, start);
    loop {
        let close = at + capabilities[at..].find("</Layer>").unwrap();
        match capabilities[at..].find("<Layer").map(|open| at + open) {
            Some(open) if open < close => {
                depth += 1;
                at = open + "<Layer".len();
            }
            _ => {
                depth -= 1;
                at = close + "</Layer>".len();
                if depth == 0 {
                    return capabilities[start..at].to_string();
                }
            }
        }
    }
}

/// A request's status, `x-cache` header and body.
async fn get(app: &axum::Router, uri: &str) -> (axum::http::StatusCode, Option<String>, String) {
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri(uri)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let cache = response
        .headers()
        .get("x-cache")
        .map(|v| v.to_str().unwrap().to_string());
    let bytes = axum::body::to_bytes(response.into_body(), 10_000_000)
        .await
        .unwrap();
    (status, cache, String::from_utf8_lossy(&bytes).into_owned())
}

/// What a kept site serves while its volumes are away, as
/// `crates/server/CLAUDE.md` describes it (#1511): one WMS layer under the
/// site's id with the default style only, no quantity children and no time
/// dimension; the error tile in WMS GetMap and a 400 in Maps and Tiles; in
/// EDR its metadata, no location and a 404 for its data. Its quantity
/// layers, their time dimension and its data come back with its volumes,
/// with no reload and no pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kept_site_serves_one_layer_until_its_volumes_return() {
    use axum::http::StatusCode;
    const BASE: &str = "pvol-kept-served";
    let gone = format!("{BASE}-tes2");
    let title = "Radar volumes — tes2";
    let (root, volumes) = volumes_dir();
    deliver(&volumes, "test");
    deliver(&volumes, "tes2");
    let (state, engine) = pvol_server(root.path(), BASE, &volumes);
    let wms = api_wms::router(state.wms.clone());
    let edr = api_edr::router(state.edr.clone());
    let shared = api_common::shared::router(api_common::shared::SharedApi::new(
        "",
        vec![
            Arc::new(api_maps::MapsBlock::new(state.maps.clone())),
            Arc::new(api_tiles::TilesBlock::new(state.tiles.clone())),
        ],
        vec![],
    ));
    let children = [
        format!("<Name>{gone}/DBZH</Name>"),
        format!("<Name>{gone}/VRAD</Name>"),
        format!("<Name>{gone}/CELLS</Name>"),
    ];
    let layer = || async {
        let (status, _, capabilities) = get(&wms, "/?SERVICE=WMS&REQUEST=GetCapabilities").await;
        assert_eq!(status, StatusCode::OK);
        site_layer(&capabilities, title)
    };
    let get_map = |layer: &str| {
        format!(
            "/?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS={layer}&STYLES=&CRS=CRS:84\
             &BBOX=24,60,25,61&WIDTH=64&HEIGHT=32&FORMAT=image/png&TRANSPARENT=TRUE"
        )
    };
    let map = format!("/collections/{gone}/map?bbox=24,60,25,61&width=64&height=32&f=image%2Fpng");
    let tile = format!("/collections/{gone}/map/tiles/WebMercatorQuad/8/145/73?f=image%2Fpng");
    let position = format!("/collections/{gone}/position?coords=POINT(24.5%2060.3)");
    let location = format!("/collections/{gone}/locations/tes2");
    let serves_its_data = || async {
        let site = layer().await;
        assert!(!site.contains(&format!("<Name>{gone}</Name>")), "{site}");
        for child in &children {
            assert!(site.contains(child), "{child} in {site}");
        }
        assert!(site.contains("<Dimension name=\"time\""), "{site}");
        for layer in [gone.clone(), format!("{gone}/DBZH")] {
            let (status, cache, _) = get(&wms, &get_map(&layer)).await;
            assert_eq!(status, StatusCode::OK);
            assert_ne!(cache.as_deref(), Some("ERROR"), "{layer}");
        }
        for uri in [&map, &tile] {
            assert_eq!(get(&shared, uri).await.0, StatusCode::OK, "{uri}");
        }
        for uri in [&position, &location] {
            assert_eq!(get(&edr, uri).await.0, StatusCode::OK, "{uri}");
        }
    };
    serves_its_data().await;

    std::fs::remove_file(volumes.join("tes2.h5")).unwrap();
    polled(&engine, &["test"]).await;
    assert_eq!(health_of(&state, &gone).await, absent());
    let site = layer().await;
    assert!(
        site.starts_with(&format!(
            "<Layer queryable=\"1\" opaque=\"1\"><Name>{gone}</Name>"
        )),
        "{site}"
    );
    for child in &children {
        assert!(!site.contains(child), "{child} in {site}");
    }
    assert!(!site.contains("<Dimension"), "{site}");
    assert_eq!(site.matches("<Style>").count(), 1, "{site}");
    assert!(site.contains("<Style><Name>default</Name>"), "{site}");
    for layer in [gone.clone(), format!("{gone}/DBZH")] {
        let (status, cache, _) = get(&wms, &get_map(&layer)).await;
        assert_eq!(
            (status, cache.as_deref()),
            (StatusCode::OK, Some("ERROR")),
            "{layer}"
        );
    }
    for uri in [&map, &tile] {
        assert_eq!(get(&shared, uri).await.0, StatusCode::BAD_REQUEST, "{uri}");
    }
    let (status, _, metadata) = get(&edr, &format!("/collections/{gone}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(metadata.contains(title), "{metadata}");
    let (status, _, locations) = get(&edr, &format!("/collections/{gone}/locations")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(locations.contains("\"features\":[]"), "{locations}");
    for uri in [&position, &location] {
        assert_eq!(get(&edr, uri).await.0, StatusCode::NOT_FOUND, "{uri}");
    }
    assert_eq!(recover_radar_once(&state).unwrap(), RecoveryOutcome::Idle);

    deliver(&volumes, "tes2");
    polled(&engine, &["tes2", "test"]).await;
    serves_its_data().await;
    assert_eq!(health_of(&state, &gone).await, Some(("ready".into(), None)));
    assert_eq!(recover_radar_once(&state).unwrap(), RecoveryOutcome::Idle);
    engine.shutdown();
}

/// A mock S3 endpoint: bucket `pvol` lists `test.h5`, and `tes2.h5` while
/// `tes2` holds, each a volume of its site; bucket `denied` answers 403.
async fn s3_endpoint(tes2: Arc<std::sync::atomic::AtomicBool>) -> String {
    use axum::{
        body::Body,
        http::{Method, Response, StatusCode},
    };
    let app = axum::Router::new().fallback(axum::routing::any(
        move |request: axum::extract::Request| {
            let tes2 = tes2.load(std::sync::atomic::Ordering::SeqCst);
            async move {
                let path = request.uri().path();
                if !path.starts_with("/pvol") {
                    return Response::builder()
                        .status(StatusCode::FORBIDDEN)
                        .body(Body::empty())
                        .unwrap();
                }
                let nods: &[&str] = if tes2 { &["tes2", "test"] } else { &["test"] };
                if request
                    .uri()
                    .query()
                    .is_some_and(|q| q.contains("list-type"))
                {
                    let contents: String = nods
                        .iter()
                        .map(|nod| {
                            format!(
                                "<Contents><Key>{nod}.h5</Key>\
                                 <LastModified>2026-09-16T10:00:00Z</LastModified>\
                                 <Size>{}</Size></Contents>",
                                volume_of(nod).len()
                            )
                        })
                        .collect();
                    return Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "application/xml")
                        .body(Body::from(format!(
                            "<?xml version=\"1.0\"?><ListBucketResult \
                             xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
                             <Name>pvol</Name><IsTruncated>false</IsTruncated>\
                             {contents}</ListBucketResult>"
                        )))
                        .unwrap();
                }
                let Some(nod) = nods.iter().find(|nod| path == format!("/pvol/{nod}.h5")) else {
                    return Response::builder()
                        .status(StatusCode::NOT_FOUND)
                        .body(Body::empty())
                        .unwrap();
                };
                let bytes = volume_of(nod);
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-length", bytes.len())
                    .body(if request.method() == Method::HEAD {
                        Body::empty()
                    } else {
                        Body::from(bytes)
                    })
                    .unwrap()
            }
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

/// A server over one remote PVOL source `base` in `bucket` at `endpoint`,
/// its poll loop running every second when its engine loaded, and a CSV
/// station collection, so a load with the source failed is accepted.
fn remote_pvol_server(
    root: &Path,
    base: &str,
    endpoint: &str,
    bucket: &str,
) -> (AdminState, Option<Arc<PolarVolumeEngine>>) {
    let csv = root.join("weather.csv");
    std::fs::write(&csv, include_str!("../../../testdata/weather.csv")).unwrap();
    let config_path = root.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[server]
host = "127.0.0.1"
port = 8000

[[collections]]
id = "{base}-stations"
title = "Stations"
description = "test"
data_path = {csv:?}
apis = ["edr", "features"]

[[collections]]
id = "{base}"
title = "Radar volumes"
description = "test"
engine_type = "odim-volume"
apis = ["edr", "wms", "maps", "tiles", "3dtiles", "features"]

[collections.odim]
endpoint = "{endpoint}"
bucket = "{bucket}"
prefix_pattern = ""
prewarm_sweeps = 0
poll_interval_secs = 1
"#,
            csv = csv.to_str().unwrap()
        ),
    )
    .unwrap();
    let state = crate::watcher::tests::build_state(&config_path);
    let engine = state.odim_volume_engines.read().unwrap().first().cloned();
    if let Some(engine) = &engine {
        let poller = engine.clone();
        crate::poll_runtime().spawn(async move { poller.poll_loop().await });
    }
    (state, engine)
}

/// A remote source's site whose volumes leave the bucket is kept and
/// `Degraded` as a local one is, and alone starts no recovery pass, while a
/// remote source that never loaded still does (#1511).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_kept_site_starts_no_pass_and_a_pending_remote_source_does() {
    use std::sync::atomic::{AtomicBool, Ordering};
    const BASE: &str = "pvol-kept-remote";
    let gone = format!("{BASE}-tes2");
    let tes2 = Arc::new(AtomicBool::new(true));
    let endpoint = s3_endpoint(tes2.clone()).await;
    let root = tempfile::tempdir().unwrap();
    let (state, engine) = remote_pvol_server(root.path(), BASE, &endpoint, "pvol");
    let engine = engine.expect("the remote source loads");
    assert!(served(&state, &gone) && ready(&state, &gone));
    assert_eq!(health_of(&state, &gone).await, Some(("ready".into(), None)));

    tes2.store(false, Ordering::SeqCst);
    polled(&engine, &["test"]).await;
    assert_eq!(health_of(&state, &gone).await, absent());
    assert!(served(&state, &gone));
    assert!(!pending(&state));
    for _ in 0..3 {
        assert_eq!(recover_radar_once(&state).unwrap(), RecoveryOutcome::Idle);
    }

    tes2.store(true, Ordering::SeqCst);
    polled(&engine, &["tes2", "test"]).await;
    assert_eq!(health_of(&state, &gone).await, Some(("ready".into(), None)));
    assert_eq!(recover_radar_once(&state).unwrap(), RecoveryOutcome::Idle);
    engine.shutdown();

    // A remote source whose first scan failed is pending: its own entry is
    // `Failed`, and every pass retries it, backing off.
    const PENDING: &str = "pvol-pending-remote";
    let root = tempfile::tempdir().unwrap();
    let (state, engine) = remote_pvol_server(root.path(), PENDING, &endpoint, "denied");
    assert!(engine.is_none());
    assert_eq!(
        health_status(&state, PENDING).await.as_deref(),
        Some("failed")
    );
    assert!(pending(&state));
    assert_eq!(
        recover_radar_once(&state).unwrap(),
        RecoveryOutcome::Pending
    );
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
