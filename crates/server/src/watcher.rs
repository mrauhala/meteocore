//! Filesystem watcher for `collections_dir` (issue #318).
//!
//! When enabled (`[server] watch_collections_dir = true`), watches the
//! per-collection config directory and triggers an atomic reload — the **same**
//! path as `POST /admin/collections/reload` ([`do_reload`]) — whenever a
//! `.toml` file is added, edited, or removed, so operators don't have to call
//! reload by hand.
//!
//! Events are coalesced over a debounce window (an editor's write/rename/delete
//! burst becomes one reload). The watcher and the reload run on the dedicated
//! background runtime ([`crate::poll_runtime`]), never the request-serving
//! workers, and a failed reload (e.g. a half-written or invalid file) leaves the
//! live registry intact — exactly like the manual reload's guard.

use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::{RecursiveMode, Watcher};
use tracing::{info, warn};

use crate::admin::{do_reload, AdminState, ReloadError};

/// Start watching `dir` (non-recursive) and auto-reload on `.toml` changes.
///
/// Best-effort: returns the `notify` error if the watcher can't be created or
/// can't start watching, so the caller can log it and continue running without
/// auto-reload rather than fail to boot.
pub fn spawn_collections_watcher(
    state: AdminState,
    dir: PathBuf,
    debounce: Duration,
) -> notify::Result<()> {
    // Unbounded so the notify callback (on notify's own thread) never blocks;
    // `send` is synchronous and callable from any thread.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    let mut watcher =
        notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
            Ok(event) if is_toml_event(&event) => {
                // Ignore send errors — a closed receiver just means the task ended.
                let _ = tx.send(());
            }
            Ok(_) => {}
            Err(e) => warn!("collections_dir watcher event error: {e}"),
        })?;
    watcher.watch(&dir, RecursiveMode::NonRecursive)?;

    info!(
        "Watching collections_dir '{}' for changes (debounce {}ms)",
        dir.display(),
        debounce.as_millis()
    );
    if debounce.is_zero() {
        warn!(
            "watch_debounce_ms = 0: event coalescing is disabled — a reload fires \
             per raw filesystem event (an editor save can trigger several)"
        );
    }

    crate::poll_runtime().spawn(async move {
        // Hold the watcher (and thus its event thread + the sender) alive for
        // the lifetime of this task, which runs for the life of the process.
        let _watcher = watcher;
        while rx.recv().await.is_some() {
            // Debounce: keep absorbing events until `debounce` elapses with no
            // new one, so an editor's write/rename/delete burst is one reload.
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(debounce) => break,
                    ev = rx.recv() => {
                        if ev.is_none() {
                            return; // channel closed — watcher dropped
                        }
                        // Another event within the window — reset the timer.
                    }
                }
            }

            // Serialize against a concurrent admin reload; wait rather than skip
            // so a change is never silently dropped. `do_reload` blocks (sync
            // load) but runs here on the background runtime, off request workers.
            let _guard = state.reload_lock.lock().await;
            match do_reload(&state) {
                Ok(o) => info!(
                    "collections_dir change applied: {} ready, {} degraded of {} configured",
                    o.ready, o.degraded, o.configured
                ),
                Err(ReloadError::ConfigRead(e)) => warn!(
                    "collections_dir change ignored — config invalid, keeping current \
                     collections: {e}"
                ),
                Err(ReloadError::NoReadyCollections { configured }) => warn!(
                    "collections_dir change ignored — 0 of {configured} collections loaded, \
                     keeping current collections"
                ),
            }
        }
    });

    Ok(())
}

/// Whether an event touches a `.toml`-ish file — covers `.toml`,
/// `.toml.disabled` (enable/disable renames), and editor temp/rename artifacts
/// (`*.toml.swp`, `*.toml~`). Combined with the debounce window this keeps
/// unrelated directory churn from reloading while still catching every real
/// change; a stray non-toml file in the dir is ignored.
fn is_toml_event(event: &notify::Event) -> bool {
    event.paths.iter().any(|p| {
        Path::new(p)
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.contains(".toml"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{EventKind, ModifyKind};
    use notify::Event;

    fn event_for(path: &str) -> Event {
        Event {
            kind: EventKind::Modify(ModifyKind::Any),
            paths: vec![PathBuf::from(path)],
            attrs: Default::default(),
        }
    }

    #[test]
    fn toml_changes_trigger() {
        assert!(is_toml_event(&event_for("/c.d/radar.toml")));
        assert!(is_toml_event(&event_for("/c.d/radar.toml.disabled")));
        // Editor artifacts still contain ".toml" — fine, debounce coalesces.
        assert!(is_toml_event(&event_for("/c.d/.radar.toml.swp")));
    }

    #[test]
    fn non_toml_changes_ignored() {
        assert!(!is_toml_event(&event_for("/c.d/README.md")));
        assert!(!is_toml_event(&event_for("/c.d/notes.txt")));
    }

    // -- End-to-end: a collections.d add/remove auto-reloads the registry -----

    use crate::admin::{load_collections, CollectionStatus, ReusableCaches, ServerState};
    use arc_swap::ArcSwap;
    use ds_core::config::ServerConfig;
    use std::fs;
    use std::sync::{Arc, RwLock};

    /// Build a live `AdminState` over `config_path`, mirroring `main.rs`.
    fn build_state(config_path: &Path) -> AdminState {
        let (config, _) = ServerConfig::from_file(config_path.to_str().unwrap()).unwrap();
        let result = load_collections(
            &config.collections,
            &config.style_bundles,
            &config.server.base_url(),
            config.server.trust_proxy_headers,
            config.server.metatile_cache_mb,
            ReusableCaches::default(),
        );
        Arc::new(ServerState {
            edr: Arc::new(ArcSwap::from_pointee(result.edr_state)),
            features: Arc::new(ArcSwap::from_pointee(result.features_state)),
            wms: Arc::new(ArcSwap::from_pointee(result.wms_state)),
            maps: Arc::new(ArcSwap::from_pointee(result.maps_state)),
            tiles: Arc::new(ArcSwap::from_pointee(result.tiles_state)),
            tiles_3d: Arc::new(ArcSwap::from_pointee(result.tiles_3d_state)),
            config_path: config_path.to_str().unwrap().to_string(),
            health: RwLock::new(result.health),
            geotiff_engines: RwLock::new(result.geotiff_engines),
            querydata_engines: RwLock::new(result.querydata_engines),
            grib_engines: RwLock::new(result.grib_engines),
            zarr_engines: RwLock::new(result.zarr_engines),
            odim_engines: RwLock::new(result.odim_engines),
            odim_volume_engines: RwLock::new(result.odim_volume_engines),
            postgis_engines: RwLock::new(result.postgis_engines),
            reload_lock: tokio::sync::Mutex::new(()),
            admin_token: None,
        })
    }

    fn ready_has(state: &AdminState, id: &str) -> bool {
        state
            .health
            .read()
            .unwrap()
            .iter()
            .any(|h| h.id == id && h.status == CollectionStatus::Ready)
    }

    /// Poll up to ~10s for collection `id` to reach the `present` ready-state.
    async fn wait_ready(state: &AdminState, id: &str, present: bool) -> bool {
        for _ in 0..200 {
            if ready_has(state, id) == present {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    fn collection_toml(id: &str, csv: &Path) -> String {
        format!(
            "id = \"{id}\"\ntitle = \"{id}\"\ndescription = \"{id}\"\ndata_path = \"{}\"\n",
            csv.display()
        )
    }

    #[tokio::test]
    async fn watcher_reloads_on_collections_dir_add_and_remove() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let cdir = root.join("collections.d");
        fs::create_dir(&cdir).unwrap();

        // A tiny CSV collection is the cheapest "Ready" engine.
        let csv = root.join("data.csv");
        fs::write(
            &csv,
            "location,latitude,longitude,time,temperature\n\
             A,60.0,25.0,2026-01-01T00:00:00Z,1.0\n",
        )
        .unwrap();

        let config_path = root.join("config.toml");
        fs::write(
            &config_path,
            "[server]\nhost = \"127.0.0.1\"\nport = 8000\n\
             collections_dir = \"collections.d\"\nwatch_collections_dir = true\n",
        )
        .unwrap();
        fs::write(cdir.join("a.toml"), collection_toml("a", &csv)).unwrap();

        let state = build_state(&config_path);
        assert!(
            ready_has(&state, "a"),
            "initial collection 'a' must be ready"
        );
        assert!(!ready_has(&state, "b"));

        let watch_dir = cdir.canonicalize().unwrap();
        spawn_collections_watcher(state.clone(), watch_dir, Duration::from_millis(50)).unwrap();

        // Add a second collection → it must auto-register.
        fs::write(cdir.join("b.toml"), collection_toml("b", &csv)).unwrap();
        assert!(
            wait_ready(&state, "b", true).await,
            "collection 'b' must appear after its file is added"
        );
        assert!(ready_has(&state, "a"), "'a' must still be present");

        // Remove it → it must auto-deregister.
        fs::remove_file(cdir.join("b.toml")).unwrap();
        assert!(
            wait_ready(&state, "b", false).await,
            "collection 'b' must disappear after its file is removed"
        );
    }
}
