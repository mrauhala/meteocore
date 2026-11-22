//! The startup load (#317) over real loads of fixture collections, with a
//! test's hook holding chosen loads back: HTTP serves while they load, each
//! collection is published once built, a derived collection waits for its
//! source, a reload is refused until the load settles.

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
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        format!("[server]\nhost = \"127.0.0.1\"\nport = 8000\n{body}"),
    )
    .unwrap();
    path
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
    for engine in state.station_grid_engines.read().unwrap().iter() {
        engine.shutdown();
    }
}

fn start(state: &AdminState, loader: Loader) -> tokio::task::JoinHandle<Settled> {
    crate::poll_runtime().spawn(load(state.clone(), loader))
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
    assert_eq!(
        get(&health, "/health?loaded").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
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
        .contains("still loading"));

    loads.release("slow");
    assert!(matches!(load.await.unwrap(), Settled::Loaded { .. }));
    assert_eq!(get(&edr, "/collections/slow").await.0, StatusCode::OK);
    let (status, body) = get(&health, "/health?loaded").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "healthy");
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
    state.startup.stop_publishing();
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
