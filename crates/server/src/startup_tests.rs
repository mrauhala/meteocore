//! The startup load (#317) over real loads of fixture collections, with a
//! test's hook holding chosen loads back: HTTP serves while they load, each
//! collection is published once built, a derived collection waits for its
//! source, a reload is refused and a watched change waits until the load
//! settles, and colliding ids keep the owner one load gives them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Instant;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use super::*;
use crate::admin::AdminState;

fn testdata(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

/// A config file in `dir` whose `[[collections]]` are `body`.
fn config_file(dir: &Path, body: &str) -> PathBuf {
    config_file_with(dir, "", body)
}

/// [`config_file`] with `server` lines in `[server]`.
fn config_file_with(dir: &Path, server: &str, body: &str) -> PathBuf {
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        format!("[server]\nhost = \"127.0.0.1\"\nport = 8000\n{server}\n{body}"),
    )
    .unwrap();
    path
}

/// A geotiff radar over the TM35FIN fixture, a nowcast's raster source.
fn radar(id: &str) -> String {
    format!(
        r#"
[[collections]]
id = "{id}"
title = "{id}"
description = "Radar"
engine_type = "geotiff"
data_path = {data:?}
apis = ["wms"]
[collections.geotiff]
filename_template = "radar_tm35_%Y%m%dT%H%MZ.tif"
parameter = "reflectivity"
unit = "dBZ"
"#,
        data = testdata("radar-tm35fin")
    )
}

/// A polar-volume network over a copy of the two-sweep fixture in `dir`:
/// one radar site, `{id}-test`.
fn pvol(dir: &Path, id: &str) -> String {
    let volumes = dir.join(format!("{id}-volumes"));
    std::fs::create_dir_all(&volumes).unwrap();
    std::fs::copy(testdata("pvol-cold-batch.h5"), volumes.join("pvol.h5")).unwrap();
    format!(
        r#"
[[collections]]
id = "{id}"
title = "{id}"
description = "Volumes"
engine_type = "odim-volume"
apis = ["edr", "wms", "maps", "tiles", "features"]
data_path = {volumes:?}
[collections.odim]
prewarm_sweeps = 0
"#
    )
}

/// A nowcast over `source`, its beam geometry from `radar_source`.
fn nowcast(id: &str, source: &str, radar_source: Option<&str>) -> String {
    let radar = radar_source
        .map(|r| format!("radar_source = \"{r}\"\n"))
        .unwrap_or_default();
    format!(
        r#"
[[collections]]
id = "{id}"
title = "{id}"
description = "Nowcast"
engine_type = "nowcast"
apis = ["wms", "features"]
[collections.nowcast]
source = "{source}"
horizon = "PT1H"
{radar}"#
    )
}

/// A CSV station collection over the weather fixture, or over a file that
/// does not exist.
fn csv(id: &str, present: bool) -> String {
    let data = if present {
        testdata("weather.csv")
    } else {
        testdata("no-such-weather.csv")
    };
    format!(
        r#"
[[collections]]
id = "{id}"
title = "{id}"
description = "Stations"
engine_type = "csv"
data_path = {data:?}
apis = ["edr", "features"]
"#
    )
}

/// A station grid over `source`, the fixtures' south-west Finland.
fn grid(id: &str, source: &str) -> String {
    format!(
        r#"
[[collections]]
id = "{id}"
title = "{id}"
description = "Analysed."
engine_type = "station-grid"
apis = ["edr", "wms"]
[collections.station_grid]
source = "{source}"
parameters = ["temperature"]
grid = {{ bbox = [21.0, 60.0, 23.0, 62.0], resolution = 0.05 }}
search_radius_km = 60
cadence = "PT1H"
precompute = 1
"#
    )
}

/// The server's loader, which records the order loads start in and holds
/// the load of each `gated` id until its gate is sent to, or dropped.
struct Gated {
    loader: Loader,
    started: Arc<Mutex<Vec<String>>>,
    gates: HashMap<String, mpsc::Sender<()>>,
}

fn gated(ids: &[&str], panics: &[&str]) -> Gated {
    let started = Arc::new(Mutex::new(Vec::new()));
    let mut gates = HashMap::new();
    let mut waits = HashMap::new();
    for id in ids {
        let (gate, wait) = mpsc::channel::<()>();
        gates.insert(id.to_string(), gate);
        waits.insert(id.to_string(), Mutex::new(wait));
    }
    let panics: Vec<String> = panics.iter().map(|id| id.to_string()).collect();
    let log = started.clone();
    let loader: Loader = Arc::new(move |state, config, collection, built, pools| {
        log.lock().unwrap().push(collection.id.clone());
        if let Some(wait) = waits.get(&collection.id) {
            let _ = wait.lock().unwrap().recv();
        }
        if panics.contains(&collection.id) {
            panic!("a test load panics");
        }
        admin::load_for_startup(state, config, collection, built, pools)
    });
    Gated {
        loader,
        started,
        gates,
    }
}

impl Gated {
    fn release(&self, id: &str) {
        self.gates[id].send(()).unwrap();
    }

    fn started(&self) -> Vec<String> {
        self.started.lock().unwrap().clone()
    }
}

/// The state `main` serves before its startup load, over `body`.
fn booting(dir: &Path, body: &str) -> AdminState {
    crate::watcher::tests::build_state_at(&config_file(dir, body), true)
}

/// Stop the poll loops the load spawned, which outlive the test otherwise.
fn stop(state: &AdminState) {
    macro_rules! stop {
        ($($field:ident),*) => {$(
            for engine in state.$field.read().unwrap().iter() {
                engine.shutdown();
            }
        )*};
    }
    stop!(
        geotiff_engines,
        odim_volume_engines,
        nowcast_engines,
        station_grid_engines
    );
}

fn start(state: &AdminState, loader: Loader) -> tokio::task::JoinHandle<Settled> {
    crate::poll_runtime().spawn(load(state.clone(), loader, 4))
}

/// The health entries one load of the config at `config_path` gives, on
/// the poll runtime as the server loads.
async fn one_load(config_path: &Path) -> (Vec<CollectionHealth>, admin::LoadResult) {
    let (config, _) =
        ds_core::config::ServerConfig::from_file(config_path.to_str().unwrap()).unwrap();
    let result = crate::poll_runtime()
        .spawn(async move {
            tokio::task::block_in_place(|| {
                admin::load_collections(
                    &ds_render::StyleContext::with_builtins(),
                    &config.collections,
                    &[],
                    "http://x",
                    false,
                    &Default::default(),
                    crate::memory_caches::RenderCacheSizes::mb(
                        0,
                        ds_core::config::DEFAULT_RENDERED_CACHE_MB,
                    ),
                    admin::ReusableCaches::default(),
                    admin::EngineReuse::default(),
                )
            })
        })
        .await
        .unwrap();
    (result.health.clone(), result)
}

/// Each entry's id, whether it failed, and a failure's error: what a load
/// decides, as against a ready or degraded entry, which a poll the load
/// spawned may move on.
fn outcomes(health: &[CollectionHealth]) -> Vec<(String, bool, Option<String>)> {
    let mut out: Vec<_> = health
        .iter()
        .map(|h| {
            let failed = h.status == CollectionStatus::Failed;
            (
                h.id.clone(),
                failed,
                failed.then(|| h.error.clone()).flatten(),
            )
        })
        .collect();
    out.sort();
    out
}

async fn wait_until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn get(router: &axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let response = router
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 10_000_000)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

fn health_router(state: &AdminState) -> axum::Router {
    axum::Router::new().route(
        "/health",
        axum::routing::get(admin::health_handler).with_state(state.clone()),
    )
}

fn status_of<'a>(health: &'a serde_json::Value, id: &str) -> &'a str {
    health["collections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .unwrap_or_else(|| panic!("no /health entry for {id}: {health}"))["status"]
        .as_str()
        .unwrap()
}

fn error_of(state: &AdminState, id: &str) -> Option<String> {
    state
        .health
        .read()
        .unwrap()
        .iter()
        .find(|h| h.id == id)
        .and_then(|h| h.error.clone())
}

/// A collection whose load does not end holds only itself: the server
/// answers, the fast collection is served and `/health` lists the slow one
/// `loading` with 200 (503 only for a client asking to wait, `loaded`), a
/// reload is refused; once the slow one is built it is served too.
#[tokio::test]
async fn a_slow_collection_holds_only_itself() {
    let dir = tempfile::tempdir().unwrap();
    let state = booting(
        dir.path(),
        &format!("{}{}", csv("slow", true), csv("fast", true)),
    );
    let health = health_router(&state);
    let edr = api_edr::router(state.edr.clone());

    // Before the load: every collection listed, loading.
    let (status, body) = get(&health, "/health").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "loading");
    assert_eq!(status_of(&body, "slow"), "loading");
    assert_eq!(status_of(&body, "fast"), "loading");

    let loads = gated(&["slow"], &[]);
    let load = start(&state, loads.loader.clone());
    wait_until("the fast collection", || {
        state.edr.load().engines.contains_key("fast")
    })
    .await;

    let (status, body) = get(&health, "/health").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "loading");
    assert_eq!(status_of(&body, "fast"), "ready");
    assert_eq!(status_of(&body, "slow"), "loading");
    let (status, body) = get(&health, "/health?loaded").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["loaded"], "1 still loading");
    assert_eq!(get(&health, "/health?loaded=false").await.0, StatusCode::OK);
    assert_eq!(
        get(&health, "/health?loaded=soon").await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(get(&edr, "/collections/fast").await.0, StatusCode::OK);
    assert_eq!(
        get(&edr, "/collections/slow").await.0,
        StatusCode::NOT_FOUND
    );
    assert!(state.features.load().engines.contains_key("fast"));
    assert_eq!(
        admin::HealthCounts::of(&admin::effective_health(&state)).loading,
        1
    );

    // A reload would race the load: refused at once.
    let reload = admin::serve_reload(&state, |_| unreachable!("no reload runs"))
        .await
        .unwrap_err();
    assert_eq!(reload.0, StatusCode::CONFLICT);
    assert!(reload.1 .0["error"]
        .as_str()
        .unwrap()
        .contains("still running"));

    loads.release("slow");
    assert!(matches!(load.await.unwrap(), Settled::Loaded { .. }));
    assert_eq!(get(&edr, "/collections/slow").await.0, StatusCode::OK);
    let (status, body) = get(&health, "/health?loaded").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "healthy");
    assert_eq!(body["loaded"], "settled");
    // In the config's order, as one load lists them.
    let ids: Vec<&str> = body["collections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["slow", "fast"]);
    assert_eq!(state.engine_handles.read().unwrap().len(), 2);
}

/// A station grid waits for its station source's load, then reads the
/// engine that load published; a grid over a source that failed fails
/// with the error one load gives it.
#[tokio::test]
async fn a_derived_collection_waits_for_its_source() {
    let dir = tempfile::tempdir().unwrap();
    let body = format!(
        "{}{}{}{}",
        grid("grid", "weather"),
        csv("weather", true),
        grid("orphan", "missing"),
        csv("missing", false),
    );
    let state = booting(dir.path(), &body);
    let loads = gated(&["weather"], &[]);
    let load = start(&state, loads.loader.clone());
    wait_until("the failed source", || error_of(&state, "orphan").is_some()).await;
    // The grid has not started: its source is still loading.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!loads.started().contains(&"grid".to_string()));
    assert_eq!(
        state.health.read().unwrap()[0].status,
        CollectionStatus::Loading
    );

    loads.release("weather");
    load.await.unwrap();
    let started = loads.started();
    let at = |id: &str| started.iter().position(|s| s == id).unwrap();
    assert!(at("weather") < at("grid"), "{started:?}");
    assert!(at("missing") < at("orphan"), "{started:?}");

    let health = state.health.read().unwrap().clone();
    let entry = |id: &str| health.iter().find(|h| h.id == id).unwrap().clone();
    // Built over the published source: degraded until its poll, already
    // spawned, analyses the first time.
    assert!(matches!(
        entry("grid").status,
        CollectionStatus::Degraded | CollectionStatus::Ready
    ));
    assert!(state.edr.load().engines.contains_key("grid"));
    assert!(state.wms.load().styles.contains_key("grid/temperature"));

    // The errors and statuses one load of the same config gives.
    let (config, _) =
        ds_core::config::ServerConfig::from_file(config_file(dir.path(), &body).to_str().unwrap())
            .unwrap();
    let one = admin::load_collections(
        &ds_render::StyleContext::with_builtins(),
        &config.collections,
        &[],
        "http://x",
        false,
        &Default::default(),
        crate::memory_caches::RenderCacheSizes::mb(0, ds_core::config::DEFAULT_RENDERED_CACHE_MB),
        admin::ReusableCaches::default(),
        admin::EngineReuse::default(),
    );
    for id in ["weather", "orphan", "missing"] {
        let once = one.health.iter().find(|h| h.id == id).unwrap();
        assert_eq!(
            (entry(id).status, entry(id).error),
            (once.status, once.error.clone()),
            "{id}"
        );
    }
    assert_eq!(entry("orphan").status, CollectionStatus::Failed);
    stop(&state);
}

/// A load that panics fails its collection; the others load, and the
/// collections waiting for it fail as over any failed source.
#[tokio::test]
async fn a_panicking_load_fails_its_collection_only() {
    let dir = tempfile::tempdir().unwrap();
    let state = booting(
        dir.path(),
        &format!(
            "{}{}{}",
            csv("broken", true),
            csv("fine", true),
            grid("grid", "broken")
        ),
    );
    let loads = gated(&[], &["broken"]);
    assert!(matches!(
        start(&state, loads.loader.clone()).await.unwrap(),
        Settled::Loaded { .. }
    ));
    let health = state.health.read().unwrap().clone();
    let status = |id: &str| health.iter().find(|h| h.id == id).unwrap().status;
    assert_eq!(status("broken"), CollectionStatus::Failed);
    assert_eq!(status("fine"), CollectionStatus::Ready);
    assert_eq!(status("grid"), CollectionStatus::Failed);
    assert_eq!(
        error_of(&state, "broken").as_deref(),
        Some("the collection's load panicked")
    );
}

/// Shutdown ends the wait at once; a load still running then publishes
/// nothing.
#[tokio::test]
async fn shutdown_abandons_a_load_still_running() {
    let dir = tempfile::tempdir().unwrap();
    let state = booting(dir.path(), &csv("slow", true));
    let loads = gated(&["slow"], &[]);
    let load = start(&state, loads.loader.clone());
    wait_until("the slow load to start", || !loads.started().is_empty()).await;
    state.recovery_shutdown.shutdown();
    assert_eq!(load.await.unwrap(), Settled::ShutDown);
    // As `main` shuts down: the reload lock for good.
    let _shut = state.reload_lock.lock().await;
    loads.release("slow");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(state.edr.load().engines.is_empty());
    assert_eq!(
        state.health.read().unwrap()[0].status,
        CollectionStatus::Loading
    );
}

/// The load waits only for lower passes, and only for collections a source
/// can name: by id, or by the prefix of the ids one config registers
/// several of.
#[test]
fn dependencies_follow_named_sources_of_lower_passes() {
    let config: ds_core::config::ServerConfig = toml::from_str(&format!(
        "[server]\nhost = \"127.0.0.1\"\nport = 8000\n{}{}{}{}{}",
        csv("weather", true),
        grid("grid", "weather"),
        r#"
[[collections]]
id = "radar"
title = "radar"
description = "d"
engine_type = "odim-volume"
[collections.odim]
endpoint = "https://example.com"
bucket = "radar"

[[collections]]
id = "radar-fivih-nowcast"
title = "n"
description = "d"
engine_type = "nowcast"
apis = ["wms"]
[collections.nowcast]
source = "radar-fivih"
radar_source = "radar"
impact_source = "grid"

[[collections]]
id = "rain"
title = "rain"
description = "d"
engine_type = "period"
apis = ["wms"]
[collections.period]
source = "grid"
windows = ["PT1H"]
"#,
        csv("rad", true),
        "",
    ))
    .unwrap();
    let ids: Vec<&str> = config.collections.iter().map(|c| c.id.as_str()).collect();
    let deps: Vec<Vec<&str>> = dependencies(&config.collections)
        .into_iter()
        .map(|d| d.into_iter().map(|i| ids[i]).collect())
        .collect();
    assert_eq!(
        ids,
        [
            "weather",
            "grid",
            "radar",
            "radar-fivih-nowcast",
            "rain",
            "rad"
        ]
    );
    // A nowcast's impact source is a station grid, a higher pass: it does
    // not wait (and fails as in one load); `rad` is no prefix of
    // `radar-fivih` short of its dash.
    assert_eq!(
        deps,
        [
            vec![],
            vec!["weather"],
            vec![],
            vec!["radar"],
            vec!["grid"],
            vec![]
        ]
    );
}

#[test]
fn every_collection_failed_exits_once_loading_settles() {
    let config: ds_core::config::ServerConfig = toml::from_str(&format!(
        "[server]\nhost = \"127.0.0.1\"\nport = 8000\n{}{}",
        csv("a", true),
        csv("b", true)
    ))
    .unwrap();
    let entry = |id: &str, status| CollectionHealth {
        id: id.into(),
        engine_type: "csv".into(),
        status,
        error: None,
    };
    use CollectionStatus::{Degraded, Failed};
    assert_eq!(
        verdict(
            &config.collections,
            &[entry("a", Failed), entry("b", Failed)]
        ),
        Verdict::Exit
    );
    assert_eq!(
        verdict(
            &config.collections,
            &[entry("a", Failed), entry("b", Degraded)]
        ),
        Verdict::Serve { loaded: 1 }
    );
    assert_eq!(verdict(&[], &[]), Verdict::Empty);
    let radar: ds_core::config::ServerConfig = toml::from_str(
        r#"
[server]
host = "127.0.0.1"
port = 8000
[[collections]]
id = "radar"
title = "radar"
description = "d"
engine_type = "odim"
[collections.odim]
endpoint = "https://example.com"
bucket = "radar"
"#,
    )
    .unwrap();
    assert_eq!(
        verdict(&radar.collections, &[entry("radar", Failed)]),
        Verdict::RetryRadar
    );
}

/// `/health?loaded` waits for the load only as long as its readiness
/// grace: then 200 with collections still loading, saying so.
#[tokio::test]
async fn readiness_waits_for_the_load_only_as_long_as_its_grace() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_file_with(
        dir.path(),
        "startup_readiness_grace_secs = 1",
        &format!("{}{}", csv("slow", true), csv("fast", true)),
    );
    let state = crate::watcher::tests::build_state_at(&path, true);
    let health = health_router(&state);
    let loads = gated(&["slow"], &[]);
    let load = start(&state, loads.loader.clone());
    let (status, body) = get(&health, "/health?loaded").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    wait_until("the grace and the fast collection", || {
        state.startup.grace_elapsed() && state.edr.load().engines.contains_key("fast")
    })
    .await;
    let (status, body) = get(&health, "/health?loaded").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["loaded"], "grace elapsed, 1 still loading");
    assert_eq!(status_of(&body, "slow"), "loading");
    loads.release("slow");
    load.await.unwrap();

    // 0: no grace, every collection waited for.
    assert!(!StartupLoad::loading(0).grace_elapsed());
    assert!(!StartupLoad::default().grace_elapsed());
}

/// A collections.d change made while the load runs is applied once it
/// settles, against the config the load built: the slow collection's engine
/// is reused, the added collection is built.
#[tokio::test]
async fn a_watched_change_during_the_load_is_applied_once_it_settles() {
    let dir = tempfile::tempdir().unwrap();
    let collections_d = dir.path().join("collections.d");
    std::fs::create_dir(&collections_d).unwrap();
    std::fs::write(
        collections_d.join("slow.toml"),
        csv("slow", true).replace("[[collections]]\n", ""),
    )
    .unwrap();
    let path = config_file_with(
        dir.path(),
        "collections_dir = \"collections.d\"",
        &csv("fast", true),
    );
    let state = crate::watcher::tests::build_state_at(&path, true);
    let loads = gated(&["slow"], &[]);
    let load = start(&state, loads.loader.clone());
    crate::watcher::spawn_config_watcher(
        state.clone(),
        Some(collections_d.canonicalize().unwrap()),
        None,
        Duration::from_millis(50),
    )
    .unwrap();
    wait_until("the fast collection", || {
        state.edr.load().engines.contains_key("fast")
    })
    .await;
    std::fs::write(
        collections_d.join("added.toml"),
        csv("added", true).replace("[[collections]]\n", ""),
    )
    .unwrap();
    // Well past the debounce: the reload waits for the load.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(!state.edr.load().engines.contains_key("added"));
    assert!(state.startup.is_loading());

    loads.release("slow");
    load.await.unwrap();
    // As `main` does once the steps after the load have run.
    state.startup.settle();
    wait_until("the added collection", || {
        state.edr.load().engines.contains_key("added")
    })
    .await;
    assert!(state.edr.load().engines.contains_key("slow"));
    assert!(state.edr.load().engines.contains_key("fast"));
    assert!(state
        .health
        .read()
        .unwrap()
        .iter()
        .all(|h| h.status == CollectionStatus::Ready));
}

/// A radar site `{pvol}-test` and a collection named so: whichever one
/// load registers first keeps the id however the loads finish, and a
/// loser serving nothing leaves no engine behind. A site declared after
/// the collection fails as one load fails it.
#[tokio::test]
async fn a_colliding_id_keeps_the_owner_one_load_gives_it() {
    // The collection first: the radar network waits for it.
    let dir = tempfile::tempdir().unwrap();
    let body = format!("{}{}", csv("pvol-test", true), pvol(dir.path(), "pvol"));
    let state = booting(dir.path(), &body);
    let loads = gated(&["pvol-test"], &[]);
    let load = start(&state, loads.loader.clone());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(loads.started(), ["pvol-test"]);
    loads.release("pvol-test");
    load.await.unwrap();
    let health = state.health.read().unwrap().clone();
    let (once, _) = one_load(&config_file(dir.path(), &body)).await;
    assert_eq!(outcomes(&health), outcomes(&once));
    assert!(state.features.load().engines.contains_key("pvol-test"));
    assert!(!state.wms.load().engines.contains_key("pvol-test"));
    stop(&state);

    // The network first: the collection waits, and fails.
    let dir = tempfile::tempdir().unwrap();
    let body = format!("{}{}", pvol(dir.path(), "pvol"), csv("pvol-test", true));
    let state = booting(dir.path(), &body);
    let loads = gated(&["pvol"], &[]);
    let load = start(&state, loads.loader.clone());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(loads.started(), ["pvol"]);
    loads.release("pvol");
    load.await.unwrap();
    let health = state.health.read().unwrap().clone();
    let entries = |health: &[CollectionHealth]| -> Vec<_> {
        health
            .iter()
            .filter(|h| h.id == "pvol-test")
            .map(|h| (h.engine_type.clone(), h.status, h.error.clone()))
            .collect()
    };
    let expected = [
        ("odim-volume".to_string(), CollectionStatus::Ready, None),
        (
            "csv".to_string(),
            CollectionStatus::Failed,
            Some(
                "id collides with radar site 'test' of network 'pvol', registered first"
                    .to_string(),
            ),
        ),
    ];
    assert_eq!(entries(&health), expected);
    let served = |state: &AdminState| {
        (
            state.wms.load().engines.contains_key("pvol-test"),
            state.edr.load().collections["pvol-test"]
                .engine_type
                .clone(),
            state.features.load().engines.contains_key("pvol-test"),
        )
    };
    assert_eq!(served(&state), (true, "odim-volume".to_string(), false));
    assert!(!state
        .engine_handles
        .read()
        .unwrap()
        .contains_key("pvol-test"));
    // One load, and so a reload of the same config, decide it alike.
    let (once, _) = one_load(&config_file(dir.path(), &body)).await;
    assert_eq!(outcomes(&health), outcomes(&once));
    let reloaded = state.clone();
    crate::poll_runtime()
        .spawn(async move { admin::do_reload(&reloaded).map(|_| ()) })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entries(&state.health.read().unwrap()), expected);
    assert_eq!(served(&state), (true, "odim-volume".to_string(), false));
    stop(&state);
}

/// Nowcasts over a radar raster and over a radar site, with a radar
/// network's beam geometry, wait for their sources' loads and end as one
/// load of the same config ends them.
#[tokio::test]
async fn nowcasts_over_radar_and_radar_site_sources_load_as_in_one_load() {
    let dir = tempfile::tempdir().unwrap();
    let body = format!(
        "{}{}{}{}{}",
        nowcast("nc", "radar", Some("pvol")),
        nowcast("site-nc", "pvol-test", None),
        nowcast("orphan-nc", "no-such-radar", None),
        radar("radar"),
        pvol(dir.path(), "pvol"),
    );
    let state = booting(dir.path(), &body);
    let loads = gated(&["radar", "pvol"], &[]);
    let load = start(&state, loads.loader.clone());
    wait_until("the orphan to fail", || {
        error_of(&state, "orphan-nc").is_some()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = loads.started();
    assert!(
        !started.contains(&"nc".to_string()) && !started.contains(&"site-nc".to_string()),
        "{started:?}"
    );
    loads.release("radar");
    loads.release("pvol");
    load.await.unwrap();

    let health = state.health.read().unwrap().clone();
    let (once, _) = one_load(&config_file(dir.path(), &body)).await;
    assert_eq!(outcomes(&health), outcomes(&once));
    let failed = |id: &str| {
        health
            .iter()
            .any(|h| h.id == id && h.status == CollectionStatus::Failed)
    };
    assert!(
        !failed("nc") && failed("orphan-nc"),
        "{:?}",
        outcomes(&health)
    );
    assert!(state.wms.load().engines.contains_key("nc"));
    assert!(state.features.load().engines.contains_key("nc"));
    stop(&state);
}

/// A collection's live status never moves its `loading` entry: the engine
/// is listed a moment before the entry is replaced.
#[tokio::test]
async fn a_loading_entry_ignores_its_engines_live_status() {
    let dir = tempfile::tempdir().unwrap();
    let body = format!("{}{}", csv("weather", true), grid("grid", "weather"));
    let state = booting(dir.path(), &body);
    let (_, built) = one_load(&config_file(dir.path(), &body)).await;
    let grid = built.station_grid_engines[0].clone();
    grid.poll_once();
    assert!(grid.has_data());
    state.station_grid_engines.write().unwrap().push(grid);
    let entry = admin::effective_health(&state)
        .into_iter()
        .find(|h| h.id == "grid")
        .unwrap();
    assert_eq!(entry.status, CollectionStatus::Loading);
}

/// While collections load, a catalog's short or missing policy is
/// `no-cache`; data policies stay, and nothing changes once it settles.
#[tokio::test]
async fn catalogs_are_not_cached_while_collections_load() {
    let dir = tempfile::tempdir().unwrap();
    let state = booting(dir.path(), &csv("a", true));
    let router = |state: &AdminState| {
        let with = |policy: Option<&'static str>| {
            axum::routing::get(move || async move {
                match policy {
                    Some(policy) => {
                        ([(axum::http::header::CACHE_CONTROL, policy)], "x").into_response()
                    }
                    None => "x".into_response(),
                }
            })
        };
        axum::Router::new()
            .route(
                "/short",
                with(Some(ds_core::http_cache::CACHE_CONTROL_SHORT)),
            )
            .route("/none", with(None))
            .route("/tile", with(Some("public, max-age=86400, immutable")))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                admin::loading_cache_control,
            ))
    };
    use axum::response::IntoResponse;
    let policy = |router: axum::Router, uri: &'static str| async move {
        router
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .headers()
            .get(axum::http::header::CACHE_CONTROL)
            .map(|v| v.to_str().unwrap().to_owned())
    };
    let app = router(&state);
    assert_eq!(
        policy(app.clone(), "/short").await.as_deref(),
        Some("no-cache")
    );
    assert_eq!(
        policy(app.clone(), "/none").await.as_deref(),
        Some("no-cache")
    );
    assert_eq!(
        policy(app.clone(), "/tile").await.as_deref(),
        Some("public, max-age=86400, immutable")
    );
    state.startup.settle();
    assert_eq!(
        policy(app.clone(), "/short").await.as_deref(),
        Some(ds_core::http_cache::CACHE_CONTROL_SHORT)
    );
    assert_eq!(policy(app, "/none").await, None);
}

/// The plan at boot, before any collection is built, in `auto`: a
/// process-wide entry it sizes down is lowered (the render budget, 10 % of
/// the total), one it would grow keeps its size (the meta-tile cache, which
/// the full plan grows at this total), and an engine's entries keep theirs
/// for the plan once the load settles.
///
/// Runs in a child process: the process-wide entries must be its alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_boot_plan_only_lowers_process_wide_sizes() {
    const CHILD: &str = "MC_TEST_STARTUP_BOOT_PLAN_CHILD";
    const NAME: &str = "startup::tests::the_boot_plan_only_lowers_process_wide_sizes";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture"])
            .env(CHILD, "1")
            .status()
            .unwrap();
        assert!(status.success(), "{NAME} failed in its child process");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = config_file_with(
        dir.path(),
        "[memory]\nmode = \"auto\"\ntotal_mb = 8192",
        &radar("radar"),
    );
    let state = crate::watcher::tests::build_state_at(&path, true);
    // An engine's entries, built already (by a test, a metric read).
    let _ = engine_geotiff::decode_budget::metrics();
    let _ = engine_geotiff::decoded_chunk_cache_metrics();
    let report = crate::memory_plan::lower_at_boot(&state);
    let plan = report.plan.as_ref().expect("a total");
    let rows = crate::cache_plan::snapshot(&state);
    let row = |id: &str| rows.iter().find(|r| r.entry.id == id).unwrap();
    assert_eq!(
        row("render_budget").usage.capacity_bytes,
        819 * ds_cache::MIB
    );
    let metatile = row("metatile_cache");
    assert!(plan.grant(&metatile.entry).unwrap().bytes > metatile.configured_bytes);
    assert_eq!(metatile.usage.capacity_bytes, metatile.configured_bytes);
    let engine_scoped: Vec<_> = rows
        .iter()
        .filter(|r| r.entry.scope != ds_cache::plan::Scope::Process)
        .collect();
    assert!(!engine_scoped.is_empty());
    for row in engine_scoped {
        assert_eq!(
            row.usage.capacity_bytes, row.configured_bytes,
            "{}",
            row.entry.id
        );
    }
    for row in &rows {
        assert!(
            row.usage.capacity_bytes <= row.configured_bytes,
            "{} grew",
            row.entry.id
        );
    }
}

/// A radar network and the collections a site of it may collide with wait
/// in the order one load registers them: a lower pass first, then the
/// earlier declared. An id with more than one segment after the network's
/// is no site's.
#[test]
fn colliding_ids_wait_in_the_order_one_load_registers_them() {
    let dir = tempfile::tempdir().unwrap();
    let config: ds_core::config::ServerConfig = toml::from_str(&format!(
        "[server]\nhost = \"127.0.0.1\"\nport = 8000\n{}{}{}{}{}",
        csv("net-a", true),
        pvol(dir.path(), "net"),
        csv("net-b", true),
        csv("net-b-c", true),
        nowcast("net-n", "other", None),
    ))
    .unwrap();
    assert_eq!(
        dependencies(&config.collections),
        [vec![], vec![0], vec![1], vec![], vec![1]]
    );
}

/// After its grace the readiness check passes only once some collection is
/// served: a pod whose first loads are all slow, or failed, serves only
/// 404s.
#[tokio::test]
async fn readiness_after_the_grace_waits_for_a_collection_to_serve() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_file_with(
        dir.path(),
        "startup_readiness_grace_secs = 1",
        &format!("{}{}", csv("slow", true), csv("broken", false)),
    );
    let state = crate::watcher::tests::build_state_at(&path, true);
    let health = health_router(&state);
    let loads = gated(&["slow"], &[]);
    let load = start(&state, loads.loader.clone());
    wait_until("the grace and the broken collection", || {
        state.startup.grace_elapsed() && error_of(&state, "broken").is_some()
    })
    .await;
    let (status, body) = get(&health, "/health?loaded").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["loaded"], "1 loading, none ready");
    assert!(state.edr.load().engines.is_empty());
    loads.release("slow");
    load.await.unwrap();
    let (status, body) = get(&health, "/health?loaded").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["loaded"], "settled");
}

/// Each publish fits the plan again without applying it: the valve then
/// finds a grant for a cache a collection registered after boot, its size
/// still the configured one.
///
/// Runs in a child process: the plan registry must be its alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_publish_refits_the_plan_for_the_valve_without_applying_it() {
    const CHILD: &str = "MC_TEST_STARTUP_REFIT_CHILD";
    const NAME: &str =
        "startup::tests::each_publish_refits_the_plan_for_the_valve_without_applying_it";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture"])
            .env(CHILD, "1")
            .status()
            .unwrap();
        assert!(status.success(), "{NAME} failed in its child process");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = config_file_with(
        dir.path(),
        "[memory]\nmode = \"auto\"\ntotal_mb = 1024",
        &radar("radar"),
    );
    let state = crate::watcher::tests::build_state_at(&path, true);
    let boot = crate::memory_plan::lower_at_boot(&state);
    let tile_cache = |rows: &[ds_cache::plan::Row]| {
        rows.iter()
            .find(|r| r.entry.id == "tile_cache" && r.entry.scope.collection() == Some("radar"))
            .cloned()
    };
    assert!(tile_cache(&crate::cache_plan::snapshot(&state)).is_none());
    let loads = gated(&[], &[]);
    start(&state, loads.loader.clone()).await.unwrap();
    let row = tile_cache(&crate::cache_plan::snapshot(&state)).expect("registered by the load");
    assert!(boot.plan.as_ref().unwrap().grant(&row.entry).is_none());
    let report = crate::memory_plan::last().unwrap();
    assert!(
        report.plan.as_ref().unwrap().grant(&row.entry).is_some(),
        "refitted"
    );
    assert_eq!(
        row.usage.capacity_bytes, row.configured_bytes,
        "not applied"
    );
    assert!(report.changes.is_empty());
    stop(&state);
}
