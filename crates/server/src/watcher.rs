//! Filesystem watcher for `collections_dir` (issue #318) and `colormaps_dir`
//! (issue #571).
//!
//! When enabled (`[server] watch_collections_dir = true`), watches the
//! per-collection config directory and, when set, the palette directory, and
//! triggers an atomic reload — the **same** path as
//! `POST /admin/collections/reload` ([`do_reload`]) — whenever a collection
//! `.toml` or a palette file is added, edited, or removed, so operators don't
//! have to call reload by hand.
//!
//! Events are coalesced over a debounce window (an editor's write/rename/delete
//! burst becomes one reload). The watcher and the reload run on the dedicated
//! background runtime ([`crate::poll_runtime`]), never the request-serving
//! workers, and a failed reload (e.g. a half-written or invalid file) leaves the
//! live registry intact — exactly like the manual reload's guard.

use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::event::{AccessKind, AccessMode, EventKind, ModifyKind};
use notify::{RecursiveMode, Watcher};
use tracing::{info, warn};

use crate::admin::{do_reload, AdminState, ReloadError};

/// Start watching the config directories (non-recursive) and auto-reload on
/// changes: `collections_dir` for `.toml`-ish files, `colormaps_dir` for any
/// file (see [`is_relevant_event`]). Both roots must be canonicalized by the
/// caller — event-path routing matches them by prefix. At least one root
/// should be `Some`; with both `None` the watcher watches nothing.
///
/// Best-effort: returns the `notify` error only if the watcher can't be
/// created or NO root can be watched, so the caller can log it and continue
/// running without auto-reload rather than fail to boot. With two roots, a
/// failure on one root disables auto-reload for it alone (logged WARN); the
/// other keeps working.
pub fn spawn_config_watcher(
    state: AdminState,
    collections_dir: Option<PathBuf>,
    colormaps_dir: Option<PathBuf>,
    debounce: Duration,
) -> notify::Result<()> {
    // Unbounded so the notify callback (on notify's own thread) never blocks;
    // `send` is synchronous and callable from any thread.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    let collections_root = collections_dir.clone();
    let colormaps_root = colormaps_dir.clone();
    let mut watcher =
        notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
            // A reload must fire only on a real content/structural change — NOT
            // on a read. `do_reload` re-opens every `.toml` and every palette
            // file to parse/fingerprint it, and notify's inotify mask always
            // includes `IN_OPEN`, so reacting to read-access would let a
            // reload's own file reads re-trigger the next reload — an infinite
            // self-sustaining loop (see [`is_read_only_event`]).
            Ok(event)
                if !is_read_only_event(&event.kind)
                    && is_relevant_event(
                        &event,
                        collections_root.as_deref(),
                        colormaps_root.as_deref(),
                    ) =>
            {
                // Ignore send errors — a closed receiver just means the task ended.
                let _ = tx.send(());
            }
            Ok(_) => {}
            Err(e) => warn!("config watcher event error: {e}"),
        })?;

    let mut roots: Vec<(&str, &PathBuf)> = Vec::new();
    if let Some(d) = &collections_dir {
        roots.push(("collections_dir", d));
    }
    // Skip an identical second root (colormaps_dir == collections_dir): one
    // watch already delivers both roles' events. A colormaps_dir NESTED inside
    // collections_dir still needs its own watch — a non-recursive watch sees
    // no events from subdirectories.
    if let Some(d) = &colormaps_dir {
        if collections_dir.as_ref() != Some(d) {
            roots.push(("colormaps_dir", d));
        }
    }
    // Register each root independently: a `?`-bail here would drop `watcher`,
    // which unregisters every already-successful watch — so a failure on one
    // root (an inotify watch/instance limit, a permission or removal race
    // since the boot-time canonicalize) would silently kill auto-reload for
    // the OTHER, perfectly watchable root too. Fail the whole spawn only when
    // no root can be watched at all.
    let mut last_err = None;
    let mut watching = 0usize;
    for (what, dir) in &roots {
        match watcher.watch(dir, RecursiveMode::NonRecursive) {
            Ok(()) => {
                watching += 1;
                info!(
                    "Watching {what} '{}' for changes (debounce {}ms)",
                    dir.display(),
                    debounce.as_millis()
                );
            }
            Err(e) => {
                warn!(
                    "cannot watch {what} '{}': {e} — auto-reload disabled for it",
                    dir.display()
                );
                last_err = Some(e);
            }
        }
    }
    if watching == 0 {
        // Zero roots watching: report the failure (or watch nothing quietly
        // when no root was configured) instead of spawning an idle task.
        return match last_err {
            Some(e) => Err(e),
            None => Ok(()),
        };
    }
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
                    "config directory change applied: {} ready, {} degraded of {} configured",
                    o.ready, o.degraded, o.configured
                ),
                Err(ReloadError::ConfigRead(e)) => warn!(
                    "config directory change ignored — config invalid, keeping current \
                     collections: {e}"
                ),
                Err(ReloadError::NoReadyCollections { configured }) => warn!(
                    "config directory change ignored — 0 of {configured} collections loaded, \
                     keeping current collections"
                ),
            }
        }
    });

    Ok(())
}

/// Whether an event touches a file the watcher cares about.
///
/// Anything under `colormaps_dir` counts — palettes span many extensions
/// (`.toml`/`.cpt`/`.pal`/`.txt`/`.clr`/`.sld`), `.disabled` renames must keep
/// triggering, and the debounce absorbs editor noise, so that root gets no
/// extension filter (#571). Every other path (a `collections_dir` event) must
/// be `.toml`-ish — covers `.toml`, `.toml.disabled` (enable/disable renames),
/// and editor temp/rename artifacts (`*.toml.swp`, `*.toml~`) — which keeps
/// unrelated directory churn from reloading while still catching every real
/// change; a stray non-toml file in that dir is ignored.
///
/// When one root is nested inside the other, a path under both is routed by
/// its NEAREST enclosing root, so the `.toml` filter holds for a
/// `collections_dir` nested inside `colormaps_dir` just as it does for
/// disjoint roots (identical roots keep the permissive palette rule — palette
/// files there must trigger).
fn is_relevant_event(
    event: &notify::Event,
    collections_dir: Option<&Path>,
    colormaps_dir: Option<&Path>,
) -> bool {
    event.paths.iter().any(|p| {
        let is_toml = || {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains(".toml"))
        };
        match (
            colormaps_dir.filter(|root| p.starts_with(root)),
            collections_dir.filter(|root| p.starts_with(root)),
        ) {
            // Under both (one root nested in the other, or identical): the
            // nearer — deeper — root's rule wins. Both roots are canonical,
            // so component count is a valid depth measure.
            (Some(cm), Some(cl)) => cm.components().count() >= cl.components().count() || is_toml(),
            (Some(_), None) => true,
            _ => is_toml(),
        }
    })
}

/// Whether an event is a *read* (or pure-metadata touch) that must NOT trigger a
/// reload.
///
/// `do_reload` re-opens and reads every collection `.toml` to re-parse the
/// config. `notify`'s inotify backend registers `IN_OPEN` in its watch mask
/// **unconditionally** (notify 8.x, `inotify.rs` `add_single_watch`), surfacing
/// it as [`EventKind::Access`]`(`[`AccessKind::Open`]`)`. Because the event
/// filter keys on the file *path* alone, reacting to that open made a reload's
/// own reads emit events that re-triggered the next reload — an infinite,
/// self-sustaining loop. It was invisible in `stat` (reads don't change
/// timestamps) and only fully provable on the production *read-only*
/// `collections_dir` mount, where `IN_OPEN` is the **only** event that can fire.
///
/// We therefore drop read-type accesses (`Open`/`Read`/read-`Close`) and
/// metadata-only touches (atime/chmod/chown), while keeping every genuine
/// change: create, remove, rename, data writes, and `IN_CLOSE_WRITE`
/// (`Access(Close(Write))`) — which `do_reload`'s read-only opens never produce.
fn is_read_only_event(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Access(AccessKind::Open(_))
            | EventKind::Access(AccessKind::Read)
            | EventKind::Access(AccessKind::Close(AccessMode::Read))
            | EventKind::Modify(ModifyKind::Metadata(_))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let cl = Some(Path::new("/c.d"));
        assert!(is_relevant_event(&event_for("/c.d/radar.toml"), cl, None));
        assert!(is_relevant_event(
            &event_for("/c.d/radar.toml.disabled"),
            cl,
            None
        ));
        // Editor artifacts still contain ".toml" — fine, debounce coalesces.
        assert!(is_relevant_event(
            &event_for("/c.d/.radar.toml.swp"),
            cl,
            None
        ));
    }

    #[test]
    fn non_toml_changes_ignored() {
        let cl = Some(Path::new("/c.d"));
        assert!(!is_relevant_event(&event_for("/c.d/README.md"), cl, None));
        assert!(!is_relevant_event(&event_for("/c.d/notes.txt"), cl, None));
    }

    #[test]
    fn colormaps_dir_triggers_on_any_extension() {
        // Under the palette root there is NO extension filter (#571): palettes
        // span .toml/.cpt/.pal/.txt/.clr/.sld and `.disabled` renames must
        // keep triggering.
        let cl = Some(Path::new("/c.d"));
        let cm = Some(Path::new("/cm.d"));
        assert!(is_relevant_event(&event_for("/cm.d/radar.pal"), cl, cm));
        assert!(is_relevant_event(&event_for("/cm.d/ramp.cpt"), cl, cm));
        assert!(is_relevant_event(&event_for("/cm.d/relief.txt"), cl, cm));
        assert!(is_relevant_event(
            &event_for("/cm.d/radar.pal.disabled"),
            cl,
            cm
        ));
        // Outside the palette root the .toml filter still applies.
        assert!(!is_relevant_event(&event_for("/c.d/notes.txt"), cl, cm));
        assert!(is_relevant_event(&event_for("/c.d/radar.toml"), cl, cm));
    }

    #[test]
    fn nested_roots_route_by_nearest_enclosing_root() {
        // colormaps_dir nested inside collections_dir: palette files trigger
        // unfiltered, the surrounding collections dir keeps the .toml filter.
        let cl = Some(Path::new("/c.d"));
        let cm = Some(Path::new("/c.d/cm"));
        assert!(is_relevant_event(&event_for("/c.d/cm/radar.pal"), cl, cm));
        assert!(is_relevant_event(&event_for("/c.d/a.toml"), cl, cm));
        assert!(!is_relevant_event(&event_for("/c.d/README.md"), cl, cm));

        // The INVERSE nesting (collections_dir inside colormaps_dir, e.g.
        // colormaps_dir = "."): collections events must NOT lose the .toml
        // filter just because they also sit under the palette root.
        let cm = Some(Path::new("/root"));
        let cl = Some(Path::new("/root/collections.d"));
        assert!(is_relevant_event(&event_for("/root/radar.pal"), cl, cm));
        assert!(is_relevant_event(
            &event_for("/root/collections.d/a.toml"),
            cl,
            cm
        ));
        assert!(!is_relevant_event(
            &event_for("/root/collections.d/README.md"),
            cl,
            cm
        ));

        // Identical roots: the permissive palette rule wins — palette files
        // in the shared dir must keep triggering.
        let both = Some(Path::new("/d"));
        assert!(is_relevant_event(&event_for("/d/radar.pal"), both, both));
    }

    #[test]
    fn read_only_events_are_dropped() {
        // The bug: notify's inotify mask always includes IN_OPEN, surfaced as
        // Access(Open). do_reload re-opens every .toml, so reacting to these
        // would self-trigger an infinite reload loop.
        assert!(is_read_only_event(&EventKind::Access(AccessKind::Open(
            AccessMode::Any
        ))));
        assert!(is_read_only_event(&EventKind::Access(AccessKind::Open(
            AccessMode::Read
        ))));
        // Open(Write) must ALSO be dropped — the `Open(_)` wildcard is
        // deliberate: a write-mode open is not evidence the data was committed
        // (the real signals are Close(Write)/Modify(Data)), so reloading on it
        // would be premature. Narrowing to `Open(Any | Read)` would regress this.
        assert!(is_read_only_event(&EventKind::Access(AccessKind::Open(
            AccessMode::Write
        ))));
        assert!(is_read_only_event(&EventKind::Access(AccessKind::Read)));
        assert!(is_read_only_event(&EventKind::Access(AccessKind::Close(
            AccessMode::Read
        ))));
        // atime/chmod/chown touches don't change config content.
        assert!(is_read_only_event(&EventKind::Modify(
            ModifyKind::Metadata(notify::event::MetadataKind::Any)
        )));
    }

    #[test]
    fn real_changes_are_kept() {
        use notify::event::{CreateKind, DataChange, RemoveKind, RenameMode};
        // Create / remove / rename / data write / IN_CLOSE_WRITE all survive.
        assert!(!is_read_only_event(&EventKind::Create(CreateKind::File)));
        assert!(!is_read_only_event(&EventKind::Remove(RemoveKind::File)));
        assert!(!is_read_only_event(&EventKind::Modify(ModifyKind::Name(
            RenameMode::Any
        ))));
        assert!(!is_read_only_event(&EventKind::Modify(ModifyKind::Data(
            DataChange::Any
        ))));
        assert!(!is_read_only_event(&EventKind::Modify(ModifyKind::Any)));
        // IN_CLOSE_WRITE — a real write closed; do_reload's read-only opens
        // never produce this, so keeping it is loop-safe.
        assert!(!is_read_only_event(&EventKind::Access(AccessKind::Close(
            AccessMode::Write
        ))));
        // Catch-alls stay permissive so a real change is never missed.
        assert!(!is_read_only_event(&EventKind::Any));
        assert!(!is_read_only_event(&EventKind::Other));
    }

    // -- End-to-end: a collections.d add/remove auto-reloads the registry -----

    use crate::admin::{
        load_collections, CollectionStatus, EngineReuse, ReusableCaches, ServerState,
    };
    use arc_swap::ArcSwap;
    use ds_core::config::ServerConfig;
    use std::fs;
    use std::sync::{Arc, RwLock};

    /// Build a live `AdminState` over `config_path`, mirroring `main.rs`.
    fn build_state(config_path: &Path) -> AdminState {
        let (config, _) = ServerConfig::from_file(config_path.to_str().unwrap()).unwrap();
        let style_ctx = ds_render::StyleContext::new(
            crate::colormaps::build_palette_registry(&config, config_path.parent())
                .expect("test config palette registry builds"),
        );
        let result = load_collections(
            &style_ctx,
            &config.collections,
            &config.style_bundles,
            &config.server.base_url(),
            config.server.trust_proxy_headers,
            config.server.metatile_cache_mb,
            ReusableCaches::default(),
            EngineReuse::default(),
        );
        Arc::new(ServerState {
            edr: Arc::new(ArcSwap::from_pointee(result.edr_state)),
            mcp: None,
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
            cap_engines: RwLock::new(result.cap_engines),
            postgis_engines: RwLock::new(result.postgis_engines),
            nowcast_engines: RwLock::new(result.nowcast_engines),
            reload_lock: tokio::sync::Mutex::new(()),
            admin_token: None,
            style_fingerprint: std::sync::atomic::AtomicU64::new(
                crate::colormaps::style_config_fingerprint(&config, config_path.parent()),
            ),
            last_collections: RwLock::new(
                config
                    .collections
                    .iter()
                    .map(|c| (c.id.clone(), c.clone()))
                    .collect(),
            ),
            engine_handles: RwLock::new(result.engines_by_id),
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
        spawn_config_watcher(
            state.clone(),
            Some(watch_dir),
            None,
            Duration::from_millis(50),
        )
        .unwrap();

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

    // -- End-to-end: a colormaps.d palette edit auto-reloads styles (#571) ----

    use std::sync::atomic::Ordering;

    /// Poll up to ~10s for the style fingerprint to move away from `old`.
    async fn wait_fingerprint_change(state: &AdminState, old: u64) -> bool {
        for _ in 0..200 {
            if state.style_fingerprint.load(Ordering::Relaxed) != old {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    #[tokio::test]
    async fn watcher_reloads_on_colormaps_dir_palette_edit() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let cmdir = root.join("colormaps.d");
        fs::create_dir(&cmdir).unwrap();
        fs::write(
            cmdir.join("ramp.toml"),
            "color_stops = [\n { value = 0.0, color = \"#000000\" },\n \
             { value = 1.0, color = \"#FFFFFF\" },\n]\n",
        )
        .unwrap();

        let csv = root.join("data.csv");
        fs::write(
            &csv,
            "location,latitude,longitude,time,temperature\n\
             A,60.0,25.0,2026-01-01T00:00:00Z,1.0\n",
        )
        .unwrap();

        // No collections_dir: this also exercises colormaps-only watching.
        let config_path = root.join("config.toml");
        fs::write(
            &config_path,
            format!(
                "[server]\nhost = \"127.0.0.1\"\nport = 8000\n\
                 colormaps_dir = \"colormaps.d\"\nwatch_collections_dir = true\n\n\
                 [[collections]]\n{}",
                collection_toml("a", &csv)
            ),
        )
        .unwrap();

        let state = build_state(&config_path);
        let fp0 = state.style_fingerprint.load(Ordering::Relaxed);

        let watch_dir = cmdir.canonicalize().unwrap();
        spawn_config_watcher(
            state.clone(),
            None,
            Some(watch_dir),
            Duration::from_millis(50),
        )
        .unwrap();

        // Edit the palette → a debounced reload must pick up the new bytes,
        // observable as a style-fingerprint change (the same do_reload path
        // also drops the rendered/meta-tile caches on it).
        fs::write(
            cmdir.join("ramp.toml"),
            "color_stops = [\n { value = 0.0, color = \"#FF0000\" },\n \
             { value = 1.0, color = \"#FFFFFF\" },\n]\n",
        )
        .unwrap();
        assert!(
            wait_fingerprint_change(&state, fp0).await,
            "palette edit must trigger a reload"
        );

        // A `.disabled` rename must retrigger too — no extension filter on
        // this root.
        let fp1 = state.style_fingerprint.load(Ordering::Relaxed);
        fs::rename(cmdir.join("ramp.toml"), cmdir.join("ramp.toml.disabled")).unwrap();
        assert!(
            wait_fingerprint_change(&state, fp1).await,
            "palette .disabled rename must trigger a reload"
        );
    }
}
