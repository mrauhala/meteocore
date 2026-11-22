//! The startup load (#317): the server serves HTTP once its socket binds and
//! its config validates, and builds its collections behind it, each
//! published to the live registries the moment it is built
//! ([`admin::publish_load`]). A slow collection holds only itself.
//!
//! - Each configured collection is built on its own
//!   ([`admin::load_for_startup`]), as a task on the poll runtime that runs
//!   the blocking load inside `block_in_place`, as a reload runs on that
//!   runtime: never on a request worker (root CLAUDE.md Critical Rules 6
//!   and 7), and without holding a poll worker while it blocks.
//! - At most `[server] startup_concurrency` (default 4) build at once: a
//!   load mostly waits on its source (LISTs, index and header GETs, a PVOL
//!   or HDF5 scan), each with its own bounded concurrency inside, so a
//!   handful at once loads a config of tens of collections in a few of its
//!   slowest builds, while the boot's requests to one object store stay a
//!   small multiple of one load's (a throttling store answers more of them
//!   with 503 SlowDown) and so does its memory peak (a PVOL bootstrap, a
//!   GRIB index). A source stuck for minutes holds one slot.
//! - A derived collection (nowcast, station grid, period products) waits
//!   for the collections of lower load passes its sources name
//!   ([`dependencies`]), then reads their engines from the loads that built
//!   them ([`admin::BuiltSources`]), with the errors one load gives when a
//!   source is missing or failed. Collections that can register the same
//!   id (a radar site `{base}-{nod}` and a collection named so) publish in
//!   the order one load registers them, so the same one wins.
//! - `/health` lists every configured collection from the first request,
//!   `loading` until it is built ([`loading_health`]); `/health?loaded`
//!   is 503 until the load settles or its readiness grace passes.
//! - Each publish holds `reload_lock`, briefly; the load does not hold it
//!   between them, so the memory governor's valve keeps running. Reloads
//!   wait for the load instead ([`StartupLoad::is_loading`]): the admin
//!   endpoint answers 409, the watcher waits and applies what changed
//!   after, the recovery loop and the governor's re-plan skip.

use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ds_core::config::CollectionConfig;
use engine_postgis::pool::PoolRegistry;
use tokio::sync::watch;
use tracing::error;

use crate::admin::{
    self, AcceptedLoad, AdminState, BuiltSources, CollectionHealth, CollectionStatus, LoadResult,
    ServerState,
};

/// The startup load's state on [`ServerState`]. `Default` is a server with
/// no startup load running (tests, which build their registries in one
/// load).
pub struct StartupLoad {
    /// `true` while the load and the steps after it run.
    loading: watch::Sender<bool>,
    started: Instant,
    /// `[server] startup_readiness_grace_secs`; `None` (0) waits for every
    /// collection.
    grace: Option<Duration>,
}

impl Default for StartupLoad {
    fn default() -> Self {
        Self {
            loading: watch::channel(false).0,
            started: Instant::now(),
            grace: None,
        }
    }
}

impl StartupLoad {
    /// A startup load about to run, its readiness grace `grace_secs` (0:
    /// none).
    pub(crate) fn loading(grace_secs: u64) -> Self {
        Self {
            loading: watch::channel(true).0,
            started: Instant::now(),
            grace: (grace_secs > 0).then(|| Duration::from_secs(grace_secs)),
        }
    }

    /// Whether the startup load still runs: some configured collection is
    /// not built yet, or the steps after the load have not run.
    pub(crate) fn is_loading(&self) -> bool {
        *self.loading.borrow()
    }

    /// Wait until the startup load and the steps after it are done (at
    /// once without one).
    pub(crate) async fn settled(&self) {
        let mut loading = self.loading.subscribe();
        let _ = loading.wait_for(|loading| !*loading).await;
    }

    /// The startup load and the steps after it are done.
    pub(crate) fn settle(&self) {
        self.loading.send_replace(false);
    }

    /// [`Self::settle`] when dropped, so a panic in the steps after the load
    /// refuses no reload for good.
    pub(crate) fn settles_on_drop(&self) -> impl Drop + '_ {
        struct Settles<'a>(&'a StartupLoad);
        impl Drop for Settles<'_> {
            fn drop(&mut self) {
                self.0.settle();
            }
        }
        Settles(self)
    }

    /// Whether the load has run longer than its readiness grace: from then
    /// on `/health?loaded` answers 200 with collections still loading.
    pub(crate) fn grace_elapsed(&self) -> bool {
        self.grace
            .is_some_and(|grace| self.started.elapsed() >= grace)
    }
}

/// `/health` before any collection is built: one `loading` entry per
/// configured collection, in the config's order. Each gives way to what its
/// load reports ([`admin::publish_load`]).
pub(crate) fn loading_health(collections: &[CollectionConfig]) -> Vec<CollectionHealth> {
    collections
        .iter()
        .map(|c| CollectionHealth {
            id: c.id.clone(),
            engine_type: c.engine_type.clone(),
            status: CollectionStatus::Loading,
            error: None,
        })
        .collect()
}

/// The ids a derived collection reads another collection's engine by.
fn named_sources(collection: &CollectionConfig) -> Vec<&str> {
    let mut named: Vec<&str> = Vec::new();
    if let Some(n) = &collection.nowcast {
        named.push(&n.source);
        named.extend(
            [&n.lightning_source, &n.impact_source, &n.radar_source]
                .into_iter()
                .flatten()
                .map(String::as_str),
        );
    }
    if let Some(g) = &collection.station_grid {
        named.push(&g.source);
        named.extend(
            [&g.elevation_source, &g.land_mask_source]
                .into_iter()
                .flatten()
                .map(String::as_str),
        );
    }
    if let Some(p) = &collection.period {
        named.push(&p.source);
    }
    named
}

/// Whether `id` is one config `of` registers beside its own: a GRIB level
/// view `{id}-single`, a radar site `{id}-{nod}`.
fn derived_from(id: &str, of: &str) -> bool {
    id.strip_prefix(of)
        .and_then(|rest| rest.strip_prefix('-'))
        .is_some_and(|rest| !rest.is_empty())
}

/// Whether a radar site of the polar-volume source `source` may take the id
/// of `other`, a configured collection: `{source}-{nod}`, a `nod` being
/// alphanumeric.
fn site_may_collide(source: &CollectionConfig, other: &CollectionConfig) -> bool {
    source.engine_type == "odim-volume"
        && other
            .id
            .strip_prefix(source.id.as_str())
            .and_then(|rest| rest.strip_prefix('-'))
            .is_some_and(|nod| !nod.is_empty() && nod.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// For each collection, the indices of the collections whose loads it
/// waits for:
///
/// - Of a lower load pass ([`admin::load_pass`]), the one its sources
///   name, or for an id one config registers several of (a GRIB level view
///   `{id}-single`, a radar site `{id}-{nod}`), each whose id and a `-`
///   begin it. A lower pass only, as in one load, where a derived
///   collection sees the engines its earlier passes built. A name no
///   collection can register waits for nothing, and its collection fails
///   as in one load.
/// - A polar-volume source and a collection named like one of its radar
///   sites: the one one load registers later waits for the other, a higher
///   pass for a lower, else the later declared for the earlier, so the
///   same one keeps the id ([`admin::publish_load`]).
///
/// Every edge points to an earlier collection in (pass, declaration)
/// order, so no two wait for each other.
pub(crate) fn dependencies(collections: &[CollectionConfig]) -> Vec<Vec<usize>> {
    let order = |index: usize| (admin::load_pass(&collections[index].engine_type), index);
    collections
        .iter()
        .enumerate()
        .map(|(at, collection)| {
            let pass = admin::load_pass(&collection.engine_type);
            let named = named_sources(collection);
            collections
                .iter()
                .enumerate()
                .filter(|&(index, c)| {
                    let source = admin::load_pass(&c.engine_type) < pass
                        && named
                            .iter()
                            .any(|name| *name == c.id || derived_from(name, &c.id));
                    let collides = index != at
                        && order(index) < order(at)
                        && (site_may_collide(c, collection) || site_may_collide(collection, c));
                    source || collides
                })
                .map(|(index, _)| index)
                .collect()
        })
        .collect()
}

/// Builds one collection of the startup load: [`admin::load_for_startup`],
/// or a test's hook around it.
pub(crate) type Loader = Arc<
    dyn Fn(
            &ServerState,
            &AcceptedLoad,
            &CollectionConfig,
            &BuiltSources,
            &Mutex<PoolRegistry>,
        ) -> LoadResult
        + Send
        + Sync,
>;

/// The server's loader.
pub(crate) fn loader() -> Loader {
    Arc::new(admin::load_for_startup)
}

/// How the startup load ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Settled {
    /// Every collection was built or failed.
    Loaded { elapsed: Duration },
    /// The server shut down first: loads still running publish nothing.
    ShutDown,
}

/// Marks a collection's load settled when dropped, a panic included, so
/// no collection waits for it forever.
struct SettleOnDrop(Arc<watch::Sender<bool>>);

impl Drop for SettleOnDrop {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

/// Fail the `loading` entry of `id` with `error`, as its load would.
fn fail_loading(state: &ServerState, id: &str, error: &str) {
    let mut health = state.health.write().unwrap_or_else(|e| e.into_inner());
    for h in health
        .iter_mut()
        .filter(|h| h.id == id && h.status == CollectionStatus::Loading)
    {
        h.status = CollectionStatus::Failed;
        h.error = Some(error.to_owned());
    }
    admin::update_health_gauges(&health);
}

/// Build and publish every collection of the accepted config (#317), at
/// most `concurrency` at once, as the module documents, and return once
/// each is built or failed, or the server shuts down. Run it on the poll
/// runtime.
pub(crate) async fn load(state: AdminState, loader: Loader, concurrency: usize) -> Settled {
    let started = Instant::now();
    let Some(config) = state
        .accepted_load
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    else {
        return Settled::Loaded {
            elapsed: started.elapsed(),
        };
    };
    let collections = config.collections().to_vec();
    let waits_for = dependencies(&collections);
    let settled: Vec<Arc<watch::Sender<bool>>> = collections
        .iter()
        .map(|_| Arc::new(watch::channel(false).0))
        .collect();
    let permits = Arc::new(tokio::sync::Semaphore::new(concurrency.max(1)));
    let built = Arc::new(Mutex::new(BuiltSources::default()));
    let pools = Arc::new(Mutex::new(PoolRegistry::new()));

    let mut tasks = tokio::task::JoinSet::new();
    for (index, collection) in collections.into_iter().enumerate() {
        let sources: Vec<watch::Receiver<bool>> = waits_for[index]
            .iter()
            .map(|&source| settled[source].subscribe())
            .collect();
        let settles = SettleOnDrop(settled[index].clone());
        let (state, config, loader) = (state.clone(), config.clone(), loader.clone());
        let (permits, built, pools) = (permits.clone(), built.clone(), pools.clone());
        tasks.spawn_on(
            async move {
                let _settles = settles;
                for mut source in sources {
                    // Each sender lives until its task ends, and settles first.
                    let _ = source.wait_for(|settled| *settled).await;
                }
                let Ok(_permit) = permits.acquire_owned().await else {
                    return;
                };
                if state.recovery_shutdown.is_shutdown() {
                    return;
                }
                let sources = built.lock().unwrap_or_else(|e| e.into_inner()).clone();
                // Blocking, like a reload's load: off the poll runtime's
                // workers for its whole length, which `block_in_place`
                // hands to another thread, keeping the runtime context the
                // storage bridge and the engines' own spawns use.
                let part = tokio::task::block_in_place(|| {
                    std::panic::catch_unwind(AssertUnwindSafe(|| {
                        loader(&state, &config, &collection, &sources, &pools)
                    }))
                });
                // Under the reload lock, briefly: no memory governor step
                // resizes caches meanwhile, and shutdown, which takes the
                // lock for good, finds no publish half done and lets none
                // start after it.
                let _publishing = state.reload_lock.lock().await;
                if state.recovery_shutdown.is_shutdown() {
                    return;
                }
                match part {
                    Ok(part) => {
                        let mut built = built.lock().unwrap_or_else(|e| e.into_inner());
                        admin::publish_load(&state, &collection, part, &mut built);
                    }
                    Err(_) => {
                        error!("Collection '{}': its load panicked", collection.id);
                        fail_loading(&state, &collection.id, "the collection's load panicked");
                    }
                }
            },
            crate::poll_runtime(),
        );
    }

    let shutdown = state.recovery_shutdown.wait();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            joined = tasks.join_next() => match joined {
                None => break,
                Some(Ok(())) => {}
                Some(Err(e)) => error!("A collection load ended abnormally: {e}"),
            },
            _ = &mut shutdown => {
                // A running load cannot be stopped; it ends on its own and
                // publishes nothing.
                tasks.detach_all();
                return Settled::ShutDown;
            }
        }
    }
    // A load that ended without publishing (a panic outside it) leaves no
    // collection loading.
    let unsettled: Vec<String> = state
        .health
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|h| h.status == CollectionStatus::Loading)
        .map(|h| h.id.clone())
        .collect();
    for id in unsettled {
        fail_loading(&state, &id, "the collection's load ended without a result");
    }
    Settled::Loaded {
        elapsed: started.elapsed(),
    }
}

/// What the server does once its startup load settled: the empty-start
/// guard, decided over what every configured collection became.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// This many collections loaded, ready or degraded: serve them.
    Serve { loaded: usize },
    /// None was configured: a legitimately empty server, which a reload can
    /// fill.
    Empty,
    /// Every collection failed, and each is a remote radar source the
    /// recovery loop retries.
    RetryRadar,
    /// Every configured collection failed: exit 1.
    Exit,
}

/// The empty-start guard over the settled health of `collections`.
pub(crate) fn verdict(collections: &[CollectionConfig], health: &[CollectionHealth]) -> Verdict {
    let loaded = health
        .iter()
        .filter(|h| h.status != CollectionStatus::Failed)
        .count();
    if loaded > 0 {
        Verdict::Serve { loaded }
    } else if collections.is_empty() {
        Verdict::Empty
    } else if admin::can_start_failed_radar(collections) {
        Verdict::RetryRadar
    } else {
        Verdict::Exit
    }
}

#[cfg(test)]
#[path = "startup_tests.rs"]
mod tests;
