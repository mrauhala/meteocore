//! The CAP engine: implements `FeatureEngine` (one feature per alert area) and
//! `MapEngine` (severity-shaded polygon fills) over a poll-and-swap catalog.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use ds_poll::{FirstTick, Shutdown};
use futures::stream::{FuturesOrdered, StreamExt};

use ds_core::config::{CapConfig, Wis2Config, CAP_WIS2_MIN_POLL_INTERVAL_SECS};
use ds_core::datetime::parse_iso8601_duration;
use ds_core::error::DataServerError;
use ds_core::feature::{Bbox, Feature, FeaturePage, FeatureQuery};
use ds_core::health::{LiveStatus, WarmupCause};
use ds_core::map_engine::{MapEngine, OutputCrs, RasterInfo, RasterTile};
use ds_core::state::{
    collection_key, StateError, StateStore, StateWriter, WriteOutcome, WritePolicy,
};
use ds_render::rasterize::{fill_polygon, Combine};
use ds_wis2::{Fetcher, Resolved, Status as Wis2Status, StatusSnapshot as Wis2StatusSnapshot};

use crate::catalog::{BuildConfig, Catalog, CatalogStore};
use crate::parser::CapAreaHint;
use crate::persist;
use crate::source::{Source, SourceLoad};
use crate::supersede::{accepts_status, resolve_references, MessageKey};
use crate::wis2::{Wis2CapSource, Wis2SourceConfig};

/// The single render parameter advertised by a CAP collection (one layer = the
/// alert set, shaded by severity code 0–4).
pub const CAP_PARAMETER: &str = "severity";

/// Upper bound on areas rasterized into one tile (pathological-input guard).
const MAX_RENDER_RECORDS: usize = 50_000;
/// Cap on remembered superseded identifiers (`cap_alerts_superseded_total`
/// dedup set); beyond it the set resets and a very old identifier could be
/// counted again — acceptable versus unbounded growth.
const MAX_SUPERSEDED_IDS: usize = 100_000;
/// WIS2 mode: back-off between attempts to (re)start the broker pipeline
/// after it ended on its own.
const WIS2_RESPAWN_DELAY: Duration = Duration::from_secs(30);
/// WIS2 mode: how often the catalog is rebuilt when the accumulator is dirty.
/// Also the floor between two rebuilds — every rebuild advances `as_of`, the
/// TIME-less WMS cache key (see [`MapEngine::resolve_time`] below), so two
/// catalogs must never share one second. The forced `poll_interval_secs`
/// rebuild is folded into this cadence (it only marks a rebuild due), and
/// config validation keeps `poll_interval_secs` at or above it.
const WIS2_DIRTY_REBUILD: Duration = Duration::from_secs(CAP_WIS2_MIN_POLL_INTERVAL_SECS);
/// WIS2 mode: `rel=geometry` hint downloads in flight at once. MeteoAlarm
/// publishes one notification per alert × info × area, so a multi-area
/// document is a burst of downloads — overlapped up to this many (the
/// fetcher's own concurrency cap), applied in arrival order; beyond it the
/// pipeline channel backs up and the broker queues, as before.
const WIS2_HINT_INFLIGHT: usize = 8;
/// WIS2 mode with a state store (#1000): a changed accumulator is
/// snapshotted at most this often. Checked on every [`WIS2_DIRTY_REBUILD`]
/// tick; `shutdown()` flushes the rest.
const SNAPSHOT_WRITE_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// An unchanged accumulator is rewritten at least this often — or every
/// quarter of a shorter `warmup`, never more often than
/// [`SNAPSHOT_WRITE_INTERVAL`] — so a restore can tell from the snapshot's
/// `written_at` how long the server was down ([`snapshot_policy`]).
const SNAPSHOT_REFRESH_INTERVAL: Duration = Duration::from_secs(3600);
/// A failing snapshot write (read-only or full disk) is retried every
/// [`SNAPSHOT_WRITE_INTERVAL`] but WARNs at most this often.
const SNAPSHOT_WARN_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// What `LiveStatus::WarmingUp` counts.
const WARMUP_ITEMS: &str = "alerts";

/// CAP alert engine. Polls a local directory or web feed, parses CAP v1.2
/// documents into a [`Catalog`], and swaps it atomically.
pub struct CapEngine {
    catalog: Arc<CatalogStore>,
    source: Arc<Source>,
    build_cfg: BuildConfig,
    collection_id: String,
    poll_interval: Duration,
    /// Edge-triggered stop signal for `poll_loop` (shared lifecycle, #481);
    /// safe even when `shutdown()` races ahead of the loop's spawn.
    shutdown: Shutdown,
    /// Set once the first `refresh()` succeeds — even with **zero** records. A
    /// healthy CAP source can legitimately have no active alerts, so "loaded"
    /// (not "non-empty") is the readiness signal; see [`Self::is_loaded`].
    loaded: AtomicBool,
    refresh_failed: AtomicBool,
    /// WIS2 mode only: subscription config (the pipeline is started from
    /// `poll_loop`, never from the constructor — see `crates/ds-wis2/CLAUDE.md`).
    wis2: Option<Wis2Runtime>,
    /// Alerts withdrawn by an Update/Cancel chain, cumulative over rebuilds
    /// (`cap_alerts_superseded_total`). Counts each identifier once per
    /// withdrawal: `superseded_ids` holds the set as of the last rebuild and
    /// only newly withdrawn identifiers increment the counter — a cancelled
    /// alert that lingers in the source until eviction is not re-counted on
    /// every rebuild.
    superseded: std::sync::atomic::AtomicU64,
    superseded_ids: std::sync::Mutex<std::collections::HashSet<MessageKey>>,
    /// WIS2 mode with a state store: the accumulator snapshot's key and its
    /// write policy (#1000). `None` = no persistence.
    snapshots: Option<Mutex<StateWriter>>,
}

/// WIS2-mode state shared between `poll_loop` and the health/metrics readers.
struct Wis2Runtime {
    config: Wis2Config,
    /// Broker/pipeline counters; swapped in by `poll_loop` once the pipeline
    /// exists (`None` until then ⇒ "connecting").
    status: ArcSwap<Option<Arc<Wis2Status>>>,
    degrade_after: Duration,
    /// `[cap.wis2] warmup`: how long after the accumulator began filling
    /// from empty `live_health` reports `WarmingUp` (#1000).
    warmup: chrono::Duration,
}

impl CapEngine {
    /// Construct the engine and attempt a best-effort initial load. Construction
    /// never fails on an empty/unreachable source — the collection starts empty
    /// (degraded) and the poll loop fills it in, matching the file-backed
    /// raster engines.
    pub fn new(config: &CapConfig, collection_id: &str) -> Result<Self, DataServerError> {
        Self::new_with_state(config, collection_id, None)
    }

    /// [`Self::new`] with the server's state store (`[server] state_dir`,
    /// #1000). In WIS2 mode the accumulator is restored from the store's
    /// `<collection_id>.cap` snapshot before the first catalog build —
    /// alerts that expired meanwhile dropped, the warm-up restarted when the
    /// snapshot is older than `warmup` — and snapshotted back while the poll
    /// loop runs. A missing, unreadable or corrupt snapshot is a cold start
    /// (logged), never an error. Directory/feed sources re-read their source
    /// and ignore the store.
    pub fn new_with_state(
        config: &CapConfig,
        collection_id: &str,
        state: Option<Arc<dyn StateStore>>,
    ) -> Result<Self, DataServerError> {
        let default_ttl = match &config.default_ttl {
            Some(s) => Some(parse_iso8601_duration(s)?),
            None => None,
        };
        let (source, wis2) = match &config.wis2 {
            Some(w) => {
                let src = Arc::new(Wis2CapSource::new(Wis2SourceConfig {
                    label: collection_id.to_string(),
                    status_filter: config.status_filter.clone(),
                    retention_grace: parse_iso8601_duration(&config.retention_grace)?,
                    max_alerts: config.max_alerts.max(1),
                    geometry_links: config.geometry_links,
                    bbox_fallback: config.bbox_fallback,
                    default_ttl,
                }));
                (
                    Source::Wis2 {
                        source: src,
                        topics: w.topics.clone(),
                    },
                    Some(Wis2Runtime {
                        config: w.clone(),
                        status: ArcSwap::from_pointee(None),
                        degrade_after: Duration::from_secs(w.degrade_after_secs.max(1)),
                        warmup: w.warmup_duration()?,
                    }),
                )
            }
            None => (
                Source::build(
                    config.data_path.as_deref(),
                    config.feed_url.as_deref(),
                    &config.feed_allowlist,
                )?,
                None,
            ),
        };
        // Load the optional geocode → geometry lookup once (static reference
        // data). A misconfigured path is a hard error — it's local config, unlike
        // the pollable source.
        let geocode_lookup = match &config.geocode_geometry {
            Some(path) => {
                let lk = crate::geocode::GeocodeLookup::load(
                    path,
                    &config.geocode_property,
                    config.geocode_value_name.as_deref(),
                )?;
                tracing::info!(
                    "[{collection_id}] cap: loaded {} geocode zone(s) from '{path}'",
                    lk.len()
                );
                Some(Arc::new(lk))
            }
            None => None,
        };
        let build_cfg = BuildConfig {
            language: config.language.clone(),
            status_filter: config
                .status_filter
                .iter()
                .map(|s| s.trim().to_ascii_lowercase())
                .collect(),
            default_ttl,
            circle_segments: config.circle_segments,
            geocode_lookup,
        };

        let catalog = Arc::new(ArcSwap::from_pointee(Catalog::empty(
            CAP_PARAMETER,
            Utc::now(),
        )));
        let mut restored = false;
        let snapshots = match (state, source.wis2(), &wis2) {
            (Some(store), Some(src), Some(w)) => {
                let mut writer = StateWriter::new(
                    store,
                    collection_key(collection_id, persist::KIND),
                    snapshot_policy(w.warmup),
                );
                restored = restore_snapshot(
                    src,
                    &mut writer,
                    collection_id,
                    w.warmup,
                    Utc::now(),
                    Instant::now(),
                );
                Some(Mutex::new(writer))
            }
            _ => None,
        };
        let engine = CapEngine {
            catalog,
            source: Arc::new(source),
            build_cfg,
            collection_id: collection_id.to_string(),
            poll_interval: Duration::from_secs(config.poll_interval_secs.max(1)),
            shutdown: Shutdown::new(),
            loaded: AtomicBool::new(false),
            refresh_failed: AtomicBool::new(false),
            wis2,
            superseded: std::sync::atomic::AtomicU64::new(0),
            superseded_ids: std::sync::Mutex::new(std::collections::HashSet::new()),
            snapshots,
        };

        // Best-effort initial load (so local fixtures populate immediately).
        // WIS2 mode has nothing to load until the broker delivers — unless a
        // snapshot was restored, which is served right away. Either way it
        // stays `Degraded("connecting to WIS2 broker")` until the session is
        // up (see `poll_loop`).
        if restored {
            if let Err(e) = engine.refresh() {
                tracing::warn!("[{collection_id}] cap/wis2: restored catalog build failed: {e}");
            }
        }
        if engine.wis2.is_none() {
            if let Err(e) = engine.refresh() {
                tracing::warn!(
                    "[{collection_id}] cap: initial load from {} failed: {e} (will retry on poll)",
                    engine.source.label()
                );
            }
        }
        Ok(engine)
    }

    /// Whether this collection is fed by a WIS2 subscription.
    pub fn is_wis2(&self) -> bool {
        self.wis2.is_some()
    }

    /// The WIS2 accumulator (WIS2 mode only) — lets tests feed notifications
    /// without a broker.
    pub fn wis2_source(&self) -> Option<&Arc<Wis2CapSource>> {
        self.source.wis2()
    }

    /// Live health: latest acquisition outcome for directory/feed sources,
    /// or broker session readiness in WIS2 mode.
    /// A disconnect shorter than `degrade_after_secs` is not reported — the
    /// last catalog keeps serving and the session resumes with its backlog.
    /// A WIS2 accumulator that began filling from empty less than
    /// `[cap.wis2] warmup` ago is `WarmingUp` (#1000): the standing
    /// warnings return only as the producers republish them.
    pub fn live_health(&self) -> Option<LiveStatus> {
        self.live_health_at(Utc::now())
    }

    fn live_health_at(&self, now: DateTime<Utc>) -> Option<LiveStatus> {
        let Some(w) = self.wis2.as_ref() else {
            return Some(
                if self.refresh_failed.load(Ordering::Relaxed) || !self.is_loaded() {
                    LiveStatus::Degraded {
                        reason: "CAP source refresh failed",
                    }
                } else {
                    LiveStatus::Ready
                },
            );
        };
        let status = w.status.load();
        let Some(status) = status.as_ref() else {
            return Some(LiveStatus::Degraded {
                reason: "connecting to WIS2 broker",
            });
        };
        let snap = status.snapshot();
        if !snap.connected {
            return Some(match snap.disconnected_for_secs {
                Some(secs) if secs >= w.degrade_after.as_secs() => LiveStatus::Degraded {
                    reason: "WIS2 broker disconnected",
                },
                // Never connected yet, or a short blip inside the grace period.
                // A blip does not end a warm-up: it stays `WarmingUp`.
                Some(_) => {
                    if self.is_loaded() {
                        self.warming_up(w, now).unwrap_or(LiveStatus::Ready)
                    } else {
                        LiveStatus::Degraded {
                            reason: "connecting to WIS2 broker",
                        }
                    }
                }
                None => LiveStatus::Degraded {
                    reason: "connecting to WIS2 broker",
                },
            });
        }
        if !snap.subscribed {
            return Some(LiveStatus::Degraded {
                reason: "WIS2 subscription not acknowledged",
            });
        }
        // Valid warnings are being evicted to stay under `max_alerts`
        // (#805): the map and `/items` are missing some of them.
        if self
            .source
            .wis2()
            .is_some_and(|src| src.stats.over_capacity.load(Ordering::Relaxed))
        {
            return Some(LiveStatus::Degraded {
                reason: "over max_alerts: valid warnings evicted",
            });
        }
        if !self.is_loaded() {
            return Some(LiveStatus::Degraded {
                reason: "waiting for first WIS2 catalog build",
            });
        }
        Some(self.warming_up(w, now).unwrap_or(LiveStatus::Ready))
    }

    /// `WarmingUp` while the accumulator is inside `[cap.wis2] warmup` of
    /// an empty start or of a restore after a long outage (#1000), else
    /// `None`. The clock starts when the subscription first comes up and
    /// survives restarts through the snapshot; a recent snapshot whose fill
    /// began long ago is ready at once.
    fn warming_up(&self, w: &Wis2Runtime, now: DateTime<Utc>) -> Option<LiveStatus> {
        let src = self.source.wis2()?;
        let warm = src
            .filling_since()
            .and_then(|t| t.checked_add_signed(w.warmup))
            .is_some_and(|end| now >= end);
        (!warm).then(|| LiveStatus::WarmingUp {
            received: src.len() as u64,
            items: WARMUP_ITEMS,
            cause: src.warmup_cause(),
        })
    }

    /// WIS2 broker/pipeline counters for `/metrics` (`None` unless WIS2 mode
    /// and the pipeline has started).
    pub fn wis2_status(&self) -> Option<Wis2StatusSnapshot> {
        let w = self.wis2.as_ref()?;
        let guard = w.status.load();
        guard.as_ref().as_ref().map(|s| s.snapshot())
    }

    /// WIS2 accumulator counters (`None` unless WIS2 mode). Values:
    /// `(documents_ingested, documents_rejected, deletions, hints_attached,
    /// hints_rejected, evicted, evicted_valid, alerts_held)`.
    pub fn wis2_source_stats(&self) -> Option<[u64; 8]> {
        let src = self.source.wis2()?;
        let st = &src.stats;
        Some([
            st.documents_ingested.load(Ordering::Relaxed),
            st.documents_rejected.load(Ordering::Relaxed),
            st.deletions.load(Ordering::Relaxed),
            st.hints_attached.load(Ordering::Relaxed),
            st.hints_rejected.load(Ordering::Relaxed),
            st.evicted.load(Ordering::Relaxed),
            st.evicted_valid.load(Ordering::Relaxed),
            src.len() as u64,
        ])
    }

    /// Number of alert areas in the current catalog.
    pub fn record_count(&self) -> usize {
        self.snapshot().records.len()
    }

    /// Collection id (for logging / health).
    pub fn collection_id(&self) -> &str {
        &self.collection_id
    }

    /// Whether at least one load has succeeded (regardless of record count).
    /// This is the health-readiness signal: a reachable source with **zero**
    /// active alerts is `Ready`, not `Degraded` — only a never-yet-successful
    /// load (e.g. an unreachable feed at startup) is `Degraded`.
    pub fn is_loaded(&self) -> bool {
        self.loaded.load(Ordering::Relaxed)
    }

    /// Fetch + parse the source and swap in a fresh catalog (advancing `as_of`
    /// so the TIME-less "now" view tracks expiry). Keeps the previous snapshot
    /// on an I/O failure so a transient outage doesn't blank the alerts.
    pub fn refresh(&self) -> Result<(), DataServerError> {
        self.refresh_with(Utc::now)
    }

    /// [`Self::refresh`] with an explicit clock — lets tests work with
    /// captured documents whose validity has long expired. The clock is read
    /// once before the load (the WIS2 accumulator's eviction instant) and
    /// again after it for the catalog's `as_of`, so `as_of` keeps following
    /// data *acquisition*: a slow feed fetch must not judge expiries against
    /// a clock that predates the data.
    pub fn refresh_with(&self, clock: impl Fn() -> DateTime<Utc>) -> Result<(), DataServerError> {
        let load = self.source.load_at(clock());
        self.publish_load(load, clock())
    }

    fn publish_load(
        &self,
        load: Result<SourceLoad, DataServerError>,
        as_of: DateTime<Utc>,
    ) -> Result<(), DataServerError> {
        let load = match load {
            Ok(load) => load,
            Err(e) => {
                // Keep the warning data but advance the render/metadata clock:
                // failed acquisition must not freeze TIME-less expiry or caches.
                self.catalog.store(Arc::new(self.snapshot().at_time(as_of)));
                self.refresh_failed.store(true, Ordering::Relaxed);
                return Err(e);
            }
        };
        let failed_documents = load.failed_documents;
        let usable = failed_documents == 0 || !load.alerts.is_empty();
        let alerts = load
            .alerts
            .into_iter()
            .filter(|a| accepts_status(a, &self.build_cfg.status_filter))
            .collect();
        // WIS2 resolves at ingest using pubtime as well as CAP identity. Running
        // old Update references again here would withdraw a later reissue.
        let (alerts, withdrawn) = if self.is_wis2() {
            (alerts, Vec::new())
        } else {
            resolve_references(alerts)
        };
        let superseded = {
            let mut seen = self
                .superseded_ids
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            // Union, not replace: an identifier stays "already counted" even
            // when the chain link that withdrew it drops out of the loaded
            // set for a rebuild (bounded so a long-lived directory source
            // cannot grow it without limit).
            if seen.len() > MAX_SUPERSEDED_IDS {
                seen.clear();
            }
            withdrawn
                .into_iter()
                .filter(|id| seen.insert(id.clone()))
                .count()
        };
        self.superseded
            .fetch_add(superseded as u64, Ordering::Relaxed);
        let catalog = Catalog::build(
            &alerts,
            &self.build_cfg,
            &self.collection_id,
            CAP_PARAMETER,
            as_of,
        );
        tracing::info!(
            "[{}] cap: loaded {} alert area(s) ({} geocode-only, {} newly superseded) from {}",
            self.collection_id,
            catalog.records.len(),
            catalog.geocode_only_count,
            superseded,
            self.source.label()
        );
        self.catalog.store(Arc::new(catalog));
        if usable {
            self.loaded.store(true, Ordering::Relaxed);
        }
        self.refresh_failed
            .store(failed_documents > 0, Ordering::Relaxed);
        if failed_documents > 0 {
            Err(DataServerError::Engine(format!("CAP refresh: {failed_documents} document(s) unavailable; retained last good copies")))
        } else {
            Ok(())
        }
    }

    /// Run the poll loop on the background runtime. Exits on [`Self::shutdown`].
    ///
    /// Directory/feed: a fixed-cadence refresh. WIS2: starts the broker
    /// pipeline (must happen here — this is the only entry point guaranteed
    /// to run on `poll_runtime()`), applies every resolved notification to
    /// the accumulator, rebuilds the catalog at most every
    /// [`WIS2_DIRTY_REBUILD`] when something changed, and forces a rebuild
    /// every `poll_interval_secs` so `as_of` advances and expired alerts
    /// drop out of the "now" view while the feed is quiet.
    pub async fn poll_loop(&self) {
        match &self.wis2 {
            None => {
                let mut ticker = self.shutdown.ticker(self.poll_interval, FirstTick::Skip);
                while ticker.tick().await {
                    if let Err(e) = self.refresh() {
                        tracing::warn!("[{}] cap: poll refresh failed: {e}", self.collection_id);
                    }
                }
            }
            Some(w) => self.wis2_loop(w).await,
        }
        tracing::info!("[{}] cap: poll loop shutting down", self.collection_id);
    }

    /// WIS2 mode. The broker pipeline is (re)started here; if it ever ends on
    /// its own (a non-transient subscriber error, a closed channel) it is
    /// respawned after [`WIS2_RESPAWN_DELAY`] rather than leaving the
    /// collection frozen — an unchanged-config reload reuses this engine, so
    /// nothing else would restart it short of a process restart. While the
    /// pipeline is down `live_health()` reports the last status it wrote,
    /// which after a disconnect is `Degraded`.
    async fn wis2_loop(&self, w: &Wis2Runtime) {
        let Some(source) = self.source.wis2().cloned() else {
            return;
        };
        let label: Arc<str> = Arc::from(self.collection_id.as_str());
        let mut dirty_ticker = self.shutdown.ticker(WIS2_DIRTY_REBUILD, FirstTick::Skip);
        let mut forced_ticker = self
            .shutdown
            .ticker(self.poll_interval.max(WIS2_DIRTY_REBUILD), FirstTick::Skip);
        let mut first_build_pending = true;
        let mut rebuild_due = false;
        // Notifications whose hint download is in flight, in arrival order
        // (kept across a session respawn — nothing about them depends on the
        // broker session).
        let mut pending = FuturesOrdered::new();
        'session: loop {
            let shutdown = Arc::new(Shutdown::new());
            let mut pipeline =
                match ds_wis2::spawn_pipeline(&w.config, &self.collection_id, shutdown.clone()) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::error!(
                            "[{}] cap/wis2: cannot start subscription: {e} — retrying in {}s",
                            self.collection_id,
                            WIS2_RESPAWN_DELAY.as_secs()
                        );
                        if !self.shutdown.sleep(WIS2_RESPAWN_DELAY).await {
                            break 'session;
                        }
                        continue 'session;
                    }
                };
            w.status.store(Arc::new(Some(pipeline.status.clone())));
            let fetcher = pipeline.fetcher.clone();
            loop {
                // `biased`: the rebuild tickers sit ahead of the message arms
                // so a QoS-1 backlog replay (the channel continuously ready)
                // cannot starve them — a ticker fires at most every 5 s, so
                // checking it first costs nothing in the steady state.
                tokio::select! {
                    biased;
                    _ = self.shutdown.wait() => {
                        shutdown.shutdown();
                        break 'session;
                    }
                    _ = dirty_ticker.tick() => {
                        let subscribed = pipeline.status.is_subscribed();
                        if subscribed {
                            // Starts the warm-up clock of an empty start.
                            source.mark_filling(Utc::now());
                        }
                        if source.take_dirty() || rebuild_due || (first_build_pending && subscribed) {
                            first_build_pending = false;
                            rebuild_due = false;
                            if let Err(e) = self.refresh() {
                                tracing::warn!("[{}] cap: rebuild failed: {e}", self.collection_id);
                            }
                        }
                        // At most every five minutes (see `snapshot_policy`);
                        // a blocking store call is fine on the poll runtime,
                        // which is the only place this loop runs.
                        self.write_snapshot(false);
                    }
                    _ = forced_ticker.tick() => {
                        // Folded into the dirty cadence so two rebuilds can
                        // never land in the same second.
                        rebuild_due = true;
                    }
                    Some((r, hint)) = pending.next(), if !pending.is_empty() => {
                        source.apply_with_hint(r, hint, &label, Utc::now());
                    }
                    r = pipeline.receiver.recv(), if pending.len() < WIS2_HINT_INFLIGHT => {
                        match r {
                            Some(r) => pending.push_back(resolve_for_apply(
                                source.clone(),
                                fetcher.clone(),
                                label.clone(),
                                r,
                            )),
                            None => {
                                tracing::warn!(
                                    "[{}] cap/wis2: pipeline ended — restarting in {}s",
                                    self.collection_id,
                                    WIS2_RESPAWN_DELAY.as_secs()
                                );
                                // Mark the session down so /health degrades
                                // while we wait, then respawn.
                                pipeline.status.set_disconnected();
                                shutdown.shutdown();
                                if !self.shutdown.sleep(WIS2_RESPAWN_DELAY).await {
                                    break 'session;
                                }
                                continue 'session;
                            }
                        }
                    }
                }
            }
        }
        // The pipeline's own Shutdown is private to this loop, so a reload can
        // never leave a subscriber behind.
    }

    /// Alerts withdrawn by Update/Cancel chains since boot — at rebuild
    /// (every source mode) plus at ingest (WIS2 accumulator).
    pub fn superseded_total(&self) -> u64 {
        let at_ingest = self
            .source
            .wis2()
            .map(|s| s.stats.superseded.load(Ordering::Relaxed))
            .unwrap_or(0);
        self.superseded.load(Ordering::Relaxed) + at_ingest
    }

    /// Signal the poll loop to stop. With a state store, also flush the
    /// snapshot (#1000): a graceful restart loses nothing, and the
    /// snapshot's `written_at` records when the server went down.
    pub fn shutdown(&self) {
        self.shutdown.shutdown();
        self.write_snapshot(true);
    }

    /// Write the accumulator snapshot when its write policy says so
    /// ([`snapshot_policy`]; `force` always writes). Never fails the caller:
    /// a failed write keeps the previous snapshot and is logged, as a WARN
    /// at most every [`SNAPSHOT_WARN_INTERVAL`].
    fn write_snapshot(&self, force: bool) {
        let (Some(writer), Some(src)) = (&self.snapshots, self.source.wis2()) else {
            return;
        };
        let mut writer = writer.lock().unwrap_or_else(|e| e.into_inner());
        let mut alerts = 0;
        let outcome = writer.write_if_due(src.revision(), Instant::now(), force, || {
            let state = src.export();
            alerts = state.entries.len();
            persist::encode(&self.collection_id, state, Utc::now())
                .map_err(|e| StateError::Encode(e.to_string()))
        });
        match outcome {
            WriteOutcome::NotDue => {}
            WriteOutcome::Written { bytes } => tracing::debug!(
                "[{}] cap/wis2: state snapshot written: {alerts} alert(s), {bytes} bytes to {}",
                self.collection_id,
                writer.describe()
            ),
            WriteOutcome::Failed { error, warn: true } => tracing::warn!(
                "[{}] cap/wis2: cannot write state snapshot {}: {error} — the previous one \
                 stays; retrying every {} min, this warning repeats at most every {} min",
                self.collection_id,
                writer.describe(),
                SNAPSHOT_WRITE_INTERVAL.as_secs() / 60,
                SNAPSHOT_WARN_INTERVAL.as_secs() / 60
            ),
            WriteOutcome::Failed { error, warn: false } => tracing::debug!(
                "[{}] cap/wis2: state snapshot write failed again: {error}",
                self.collection_id
            ),
        }
    }

    fn snapshot(&self) -> arc_swap::Guard<Arc<Catalog>> {
        self.catalog.load()
    }
}

/// The snapshot write policy of a collection with `[cap.wis2] warmup`
/// (#1000): a changed accumulator at most every
/// [`SNAPSHOT_WRITE_INTERVAL`], an unchanged one at least every quarter of
/// the warm-up, clamped to [`SNAPSHOT_WRITE_INTERVAL`] ..=
/// [`SNAPSHOT_REFRESH_INTERVAL`]. The refresh keeps `written_at` close to
/// the server's last breath even when the feed is quiet, so the
/// long-outage check in [`restore_snapshot`] errs by at most that much.
fn snapshot_policy(warmup: chrono::Duration) -> WritePolicy {
    let quarter = warmup.to_std().unwrap_or(Duration::ZERO) / 4;
    WritePolicy {
        min_interval: SNAPSHOT_WRITE_INTERVAL,
        refresh_interval: quarter.clamp(SNAPSHOT_WRITE_INTERVAL, SNAPSHOT_REFRESH_INTERVAL),
        warn_interval: SNAPSHOT_WARN_INTERVAL,
    }
}

/// Restore `src` from the snapshot `writer` manages (#1000). `true` when a
/// snapshot was restored; a missing, unreadable or rejected one is a cold
/// start, logged, never an error.
///
/// A snapshot written longer than `warmup` before `now` means the server
/// was down that long: whatever the feed published meanwhile is missing
/// until it is republished. Its alerts are restored, but the warm-up
/// restarts — the clock is cleared, so it starts again when the
/// subscription comes up, and `/health` says "warming up after a long
/// outage" until it ends.
fn restore_snapshot(
    src: &Wis2CapSource,
    writer: &mut StateWriter,
    collection_id: &str,
    warmup: chrono::Duration,
    now: DateTime<Utc>,
    now_instant: Instant,
) -> bool {
    let at = writer.describe();
    let bytes = match writer.load() {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            tracing::info!("[{collection_id}] cap/wis2: no state snapshot at {at} — cold start");
            return false;
        }
        Err(e) => {
            tracing::warn!(
                "[{collection_id}] cap/wis2: cannot read state snapshot {at}: {e} — cold start"
            );
            return false;
        }
    };
    let mut decoded = match persist::decode(&bytes, collection_id) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(
                "[{collection_id}] cap/wis2: state snapshot {at} rejected ({e}) — cold start; \
                 the next write replaces it"
            );
            return false;
        }
    };
    let age = now - decoded.written_at;
    let long_outage = age > warmup;
    if long_outage {
        decoded.state.filling_since = None;
        decoded.state.warmup_cause = WarmupCause::LongOutage;
    }
    let s = src.restore(decoded.state, now);
    if !long_outage {
        // The store holds what was restored: no rewrite until it changes
        // or the refresh interval runs out. After a long outage the
        // restarted warm-up is not in the store yet: write it at once.
        writer.mark_restored(
            src.revision(),
            age.to_std().unwrap_or(Duration::ZERO),
            now_instant,
        );
    }
    tracing::info!(
        "[{collection_id}] cap/wis2: restored {} alert(s) from state snapshot {at} written {} \
         ago ({} expired meanwhile, {} filtered, {} hint(s) dropped by config, {} tombstone(s))",
        s.alerts,
        hours_minutes(age),
        s.expired,
        s.filtered,
        s.hints_dropped,
        s.tombstones
    );
    if long_outage {
        tracing::warn!(
            "[{collection_id}] cap/wis2: the state snapshot is older than warmup ({}): \
             alerts published during the outage are missing until republished — the \
             warm-up restarts and /health reports degraded until it ends",
            hours_minutes(warmup)
        );
    }
    true
}

/// `26h05m` (negative ⇒ `0h00m`).
fn hours_minutes(d: chrono::Duration) -> String {
    let minutes = d.num_minutes().max(0);
    format!("{}h{:02}m", minutes / 60, minutes % 60)
}

/// The network half of one WIS2 notification (hint download), paired with
/// the notification so the loop can apply them in arrival order once the
/// download settles.
async fn resolve_for_apply(
    source: Arc<Wis2CapSource>,
    fetcher: Arc<Fetcher>,
    label: Arc<str>,
    r: Resolved,
) -> (Resolved, Option<(usize, usize, CapAreaHint)>) {
    let hint = source.resolve_hint(&r, &fetcher, &label).await;
    (r, hint)
}

// ---------------------------------------------------------------------------
// FeatureEngine
// ---------------------------------------------------------------------------

impl ds_core::feature_engine::FeatureEngine for CapEngine {
    fn filterables(&self) -> ds_core::feature::FilterableProperties {
        self.snapshot().filterables.clone()
    }

    fn get_features(&self, query: &FeatureQuery) -> Result<FeaturePage, DataServerError> {
        let cat = self.snapshot();

        // Candidate indices: spatial index when a bbox is set (excludes
        // null-geometry areas, which can't intersect a bbox), else every area.
        let mut indices: Vec<usize> = match &query.bbox {
            Some(bbox) => cat.query_bbox(bbox),
            None => (0..cat.records.len()).collect(),
        };

        // Active-window (datetime) filter.
        if let Some(dt) = &query.datetime {
            indices.retain(|&i| cat.records[i].window.intersects(dt.start, dt.end));
        }

        indices.retain(|&i| {
            ds_core::feature::matches_property_values(
                &cat.records[i].properties,
                &query.property_filters,
            )
        });
        let number_matched = indices.len();
        let offset = query.offset.min(number_matched);
        let end = offset.saturating_add(query.limit).min(number_matched);
        let features: Vec<Feature> = indices[offset..end]
            .iter()
            .map(|&i| to_feature(&cat.records[i]))
            .collect();
        let number_returned = features.len();
        let next_offset = (end < number_matched).then_some(end);

        Ok(FeaturePage {
            features,
            number_matched,
            number_returned,
            next_offset,
        })
    }

    fn get_feature(&self, feature_id: &str) -> Result<Feature, DataServerError> {
        let cat = self.snapshot();
        cat.get(feature_id)
            .map(to_feature)
            .ok_or_else(|| DataServerError::FeatureNotFound(feature_id.to_string()))
    }

    fn feature_count(&self) -> usize {
        self.snapshot().records.len()
    }

    fn spatial_extent(&self) -> Option<[f64; 4]> {
        self.snapshot().spatial_extent
    }

    fn temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        self.snapshot().temporal_extent()
    }

    fn data_version(&self) -> u64 {
        self.snapshot().data_version
    }
}

fn to_feature(rec: &crate::catalog::AreaRecord) -> Feature {
    Feature {
        // IDs are domain values. The API encodes them when building URLs;
        // pre-encoding here breaks both direct lookup and advertised links.
        id: rec.id.clone(),
        geometry: Arc::clone(&rec.geometry),
        properties: Arc::clone(&rec.properties),
    }
}

// ---------------------------------------------------------------------------
// MapEngine
// ---------------------------------------------------------------------------

impl MapEngine for CapEngine {
    fn default_time(&self) -> Option<DateTime<Utc>> {
        Some(self.snapshot().as_of)
    }

    /// The instant a render is keyed on. `None` (a TIME-less request) means
    /// "now" — the snapshot's `as_of`, exactly what [`Self::get_raster_tile`]
    /// substitutes — so the no-TTL rendered/meta-tile caches key the default
    /// view on the catalog actually rendered and follow every rebuild
    /// (root `CLAUDE.md`, "Adding a new engine" step 7; #507). An explicit
    /// TIME is rendered as-is (active-at-instant, no snapping).
    fn resolve_time(
        &self,
        time: Option<DateTime<Utc>>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        Some(time.unwrap_or_else(|| self.snapshot().as_of))
    }

    /// An alert set is revised in place: a warning published at 10:00 is
    /// active at 09:00 too, so every tile already rendered for an explicit
    /// `TIME=09:00` is wrong from then on. The catalog's `data_version`
    /// (content only — never `as_of`) keys the rendered caches, so a changed
    /// alert set gets fresh tiles and an unchanged rebuild keeps them.
    fn content_version(&self) -> u64 {
        // `0` is reserved for "never revised"; the hash is non-zero in
        // practice but the contract must hold by construction.
        self.snapshot().data_version.max(1)
    }

    fn get_raster_tile(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        output_crs: &OutputCrs,
        _parameter: Option<&str>,
        _z: Option<f64>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        let cat = self.snapshot();
        // None ⇒ "now" (the snapshot's as_of); an explicit TIME selects that instant.
        let t = time.unwrap_or(cat.as_of);

        let (w, h) = (width as usize, height as usize);
        let mut values: Vec<Option<f64>> = vec![None; w.saturating_mul(h)];

        // Prefilter to areas whose bbox intersects the (WGS84) request rectangle.
        // A degenerate request bbox (zero-area / non-finite) asks for no region,
        // so the tile is empty — never a full-catalog render (the API layer
        // already rejects such bboxes; this is the engine's own safety net).
        let [query_west, query_east] = crs84_query_lons(bbox[0], bbox[2]);
        let candidates: Vec<usize> = match Bbox::new(query_west, bbox[1], query_east, bbox[3]) {
            Ok(b) => cat.query_bbox(&b),
            Err(_) => {
                return Ok(RasterTile {
                    width,
                    height,
                    values: values.into(),
                })
            }
        };

        // A viewport reaching past ±180° (a Maps bbox crossing the
        // antimeridian arrives unwrapped, #828) shows an area at −175° at
        // 185°: draw each area at the world copies that fall in view.
        // `world_to_fraction` is linear in `lon − west` there, so shifting the
        // view by −k draws the area shifted by +k. A projected output wraps
        // longitude in its own forward transform and needs no copies.
        let shifts: &[f64] = match output_crs {
            OutputCrs::Projected { .. } => &[0.0],
            _ => &[0.0, 360.0, -360.0],
        };

        let mut rendered = 0usize;
        for &i in &candidates {
            let rec = &cat.records[i];
            if !rec.window.active_at(t) {
                continue;
            }
            for &k in shifts {
                let in_view = |[w, _, e, _]: [f64; 4]| w + k <= bbox[2] && e + k >= bbox[0];
                if k != 0.0 && !rec.bbox.is_some_and(in_view) {
                    continue;
                }
                let view = [bbox[0] - k, bbox[1], bbox[2] - k, bbox[3]];
                let px = ds_core::geo::geometry_to_pixels(
                    &rec.geometry,
                    view,
                    width,
                    height,
                    output_crs,
                );
                for poly in &px.polygons {
                    fill_polygon(
                        &mut values,
                        width,
                        height,
                        &poly.exterior,
                        &poly.holes,
                        rec.severity_code,
                        Combine::Max, // higher severity wins on overlap, order-independent
                    );
                }
            }
            rendered += 1;
            if rendered >= MAX_RENDER_RECORDS {
                tracing::warn!(
                    "[{}] cap: render capped at {MAX_RENDER_RECORDS} areas",
                    self.collection_id
                );
                break;
            }
        }

        Ok(RasterTile {
            width,
            height,
            values: values.into(),
        })
    }

    fn raster_info(&self) -> RasterInfo {
        // Cheap clone of the prebuilt snapshot — no recomputation (#211); the
        // cost is O(times), bounded by the 256-entry TIME cap.
        (*self.snapshot().info).clone()
    }
}

/// The CRS84 `[west, east]` the catalog is queried with for a request
/// viewport's longitudes. The catalog is indexed in −180…180, while a map
/// viewport may reach past ±180° (#828): such a span is wrapped back into
/// the domain, where it becomes a box crossing the antimeridian
/// (`west > east`) that [`Catalog::query_bbox`] splits at the seam. A
/// viewport of a whole turn or more queries every longitude. Non-finite
/// values pass through for `Bbox::new` to reject.
fn crs84_query_lons(west: f64, east: f64) -> [f64; 2] {
    let in_domain = |lon: f64| (-180.0..=180.0).contains(&lon);
    if !(west.is_finite() && east.is_finite()) || in_domain(west) && in_domain(east) {
        [west, east]
    } else if east - west >= 360.0 {
        [-180.0, 180.0]
    } else {
        [ds_core::geo::wrap_lon(west), ds_core::geo::wrap_lon(east)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::HintPart;
    use crate::wis2::test_support::resolved;
    use ds_core::feature_engine::FeatureEngine;
    use ds_core::state::FileStateStore;

    fn store(dir: &std::path::Path) -> Option<Arc<dyn StateStore>> {
        Some(Arc::new(FileStateStore::new(dir)))
    }

    fn wis2_config(warmup: Option<&str>) -> CapConfig {
        let cfg: CapConfig = serde_json::from_value(serde_json::json!({"data_path": "."})).unwrap();
        CapConfig {
            data_path: None,
            wis2: Some(Wis2Config {
                warmup: warmup.map(String::from),
                ..Wis2Config::default()
            }),
            ..cfg
        }
    }

    /// A broker session that is connected and subscribed (tests run no
    /// poll loop).
    fn subscribe(engine: &CapEngine) {
        let status = Arc::new(Wis2Status::new());
        status.set_connected();
        status.set_subscribed();
        engine
            .wis2
            .as_ref()
            .unwrap()
            .status
            .store(Arc::new(Some(status)));
    }

    /// A Severe alert valid until 2099 (so a wall-clock restore keeps it).
    fn alert(identifier: &str) -> String {
        include_str!("../tests/fixtures/helsinki-flood.xml")
            .replace("urn:test:helsinki-flood-1", identifier)
            .replace(
                "<severity>Severe</severity>",
                "<severity>Severe</severity><expires>2099-01-01T00:00:00+00:00</expires>",
            )
    }

    fn push(engine: &CapEngine, identifier: &str, now: DateTime<Utc>) {
        let hint = CapAreaHint::single(
            HintPart::Feature(0),
            ds_core::feature::Geometry::Polygon {
                exterior: vec![[24.8, 60.1], [25.2, 60.1], [25.0, 60.3], [24.8, 60.1]],
                holes: Vec::new(),
            },
            "notification",
        );
        engine.wis2_source().unwrap().apply_with_hint(
            resolved(&format!("d-{identifier}"), 0, Some(alert(identifier))),
            Some((0, 0, hint)),
            "test",
            now,
        );
    }

    fn warming(received: u64) -> Option<LiveStatus> {
        Some(LiveStatus::WarmingUp {
            received,
            items: WARMUP_ITEMS,
            cause: WarmupCause::ColdStart,
        })
    }

    fn warming_after_outage(received: u64) -> Option<LiveStatus> {
        Some(LiveStatus::WarmingUp {
            received,
            items: WARMUP_ITEMS,
            cause: WarmupCause::LongOutage,
        })
    }

    #[test]
    fn cold_start_reports_warming_up_until_the_warmup_after_subscription() {
        let engine = CapEngine::new(&wis2_config(Some("PT2H")), "cap-wis2").unwrap();
        let t0 = Utc::now();
        assert_eq!(
            engine.live_health_at(t0),
            Some(LiveStatus::Degraded {
                reason: "connecting to WIS2 broker"
            })
        );
        subscribe(&engine);
        assert_eq!(
            engine.live_health_at(t0),
            Some(LiveStatus::Degraded {
                reason: "waiting for first WIS2 catalog build"
            })
        );
        push(&engine, "a", t0);
        push(&engine, "b", t0);
        engine.refresh_with(|| t0).unwrap();
        // Subscribed and built, but the poll loop has not started the clock
        // yet: still warming, never a premature `Ready`.
        assert_eq!(engine.live_health_at(t0), warming(2));
        engine.wis2_source().unwrap().mark_filling(t0);
        // A later reconnect does not restart the clock.
        engine
            .wis2_source()
            .unwrap()
            .mark_filling(t0 + chrono::Duration::hours(1));
        assert_eq!(
            engine.live_health_at(t0 + chrono::Duration::minutes(119)),
            warming(2)
        );
        assert_eq!(
            engine.live_health_at(t0 + chrono::Duration::hours(2)),
            Some(LiveStatus::Ready)
        );
        assert_eq!(
            engine
                .live_health_at(t0)
                .and_then(|s| s.degraded_reason())
                .as_deref(),
            Some("warming up after cold start: 2 alerts received")
        );
        // A broker blip inside `degrade_after_secs` keeps serving, but does
        // not end the warm-up: it stays warming, then ready once warm.
        let status = engine.wis2.as_ref().unwrap().status.load_full();
        (*status).as_ref().unwrap().set_disconnected();
        assert_eq!(
            engine.live_health_at(t0 + chrono::Duration::minutes(30)),
            warming(2)
        );
        assert_eq!(
            engine.live_health_at(t0 + chrono::Duration::hours(3)),
            Some(LiveStatus::Ready)
        );
    }

    #[test]
    fn snapshot_round_trips_through_shutdown_and_the_constructor() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = wis2_config(None);
        let now = Utc::now();
        let a = CapEngine::new_with_state(&cfg, "cap-wis2", store(dir.path())).unwrap();
        assert!(!a.is_loaded(), "nothing to restore: a cold start");
        push(&a, "a", now);
        push(&a, "b", now);
        // The fill began three days ago: past the default 24 h warm-up.
        a.wis2_source()
            .unwrap()
            .mark_filling(now - chrono::Duration::days(3));
        a.refresh_with(|| now).unwrap();
        a.shutdown();
        let file = dir.path().join("cap-wis2.cap.state");
        assert!(file.is_file());

        // Rebuilt (restart or reload): served at once, same content.
        let b = CapEngine::new_with_state(&cfg, "cap-wis2", store(dir.path())).unwrap();
        assert!(b.is_loaded(), "a restored catalog is published at build");
        assert_eq!(b.feature_count(), 2);
        assert_eq!(b.data_version(), a.data_version());
        let geometry = |e: &CapEngine| {
            let page = e.get_features(&FeatureQuery::default()).unwrap();
            page.features
                .iter()
                .map(|f| {
                    (
                        f.id.clone(),
                        f.geometry.bbox(),
                        f.properties
                            .get("geometry_source")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(geometry(&b), geometry(&a));
        subscribe(&b);
        assert_eq!(b.live_health(), Some(LiveStatus::Ready), "restored ⇒ ready");

        // A snapshot taken mid warm-up keeps warming after the restart.
        a.wis2_source().unwrap().restore(
            crate::wis2::AccumulatorState {
                filling_since: Some(now - chrono::Duration::hours(1)),
                ..a.wis2_source().unwrap().export()
            },
            now,
        );
        a.write_snapshot(true);
        let c = CapEngine::new_with_state(&cfg, "cap-wis2", store(dir.path())).unwrap();
        subscribe(&c);
        assert_eq!(c.live_health(), warming(2));
    }

    /// The server was down longer than `warmup`: the restored alerts are
    /// served, but the warm-up restarts — what the feed published meanwhile
    /// is missing until it is republished.
    #[test]
    fn a_snapshot_older_than_the_warmup_keeps_its_alerts_but_restarts_the_warm_up() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = wis2_config(Some("PT24H"));
        let now = Utc::now();
        let a = CapEngine::new(&cfg, "cap-wis2").unwrap();
        push(&a, "a", now);
        push(&a, "b", now);
        // Warm long ago.
        a.wis2_source()
            .unwrap()
            .mark_filling(now - chrono::Duration::days(3));
        let save_written = |written_at: DateTime<Utc>| {
            let bytes =
                persist::encode("cap-wis2", a.wis2_source().unwrap().export(), written_at).unwrap();
            FileStateStore::new(dir.path())
                .save("cap-wis2.cap", &bytes)
                .unwrap();
        };

        // Last written 23 h ago: inside the warm-up, the clock is kept.
        save_written(now - chrono::Duration::hours(23));
        let fresh = CapEngine::new_with_state(&cfg, "cap-wis2", store(dir.path())).unwrap();
        subscribe(&fresh);
        assert_eq!(fresh.live_health(), Some(LiveStatus::Ready));

        // Last written 25 h ago: alerts kept, warm-up restarted.
        save_written(now - chrono::Duration::hours(25));
        let b = CapEngine::new_with_state(&cfg, "cap-wis2", store(dir.path())).unwrap();
        assert!(b.is_loaded(), "the restored alerts are served at once");
        assert_eq!(b.feature_count(), 2);
        let src = b.wis2_source().unwrap();
        assert_eq!(
            src.filling_since(),
            None,
            "the clock restarts at subscription"
        );
        assert_eq!(src.warmup_cause(), WarmupCause::LongOutage);
        subscribe(&b);
        assert_eq!(b.live_health_at(now), warming_after_outage(2));
        assert_eq!(
            b.live_health_at(now)
                .and_then(|s| s.degraded_reason())
                .as_deref(),
            Some("warming up after a long outage: 2 alerts received")
        );
        // The poll loop starts the clock when the subscription is up.
        src.mark_filling(now);
        assert_eq!(
            b.live_health_at(now + chrono::Duration::hours(23)),
            warming_after_outage(2)
        );
        assert_eq!(
            b.live_health_at(now + chrono::Duration::hours(24)),
            Some(LiveStatus::Ready)
        );

        // The restarted warm-up is written at once, not left to the next
        // change, so a restart right after it keeps warming for the same
        // reason instead of reading the old snapshot again.
        b.write_snapshot(false);
        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("cap-wis2.cap.state")).unwrap())
                .unwrap();
        assert_eq!(written["warmup_cause"], "long_outage");
        let c = CapEngine::new_with_state(&cfg, "cap-wis2", store(dir.path())).unwrap();
        subscribe(&c);
        assert_eq!(c.live_health_at(now), warming_after_outage(2));
    }

    #[test]
    fn snapshot_policy_refreshes_within_a_quarter_of_the_warmup() {
        let policy = |iso: &str| snapshot_policy(parse_iso8601_duration(iso).unwrap());
        assert_eq!(policy("PT24H").min_interval, SNAPSHOT_WRITE_INTERVAL);
        assert_eq!(
            policy("PT24H").refresh_interval,
            SNAPSHOT_REFRESH_INTERVAL,
            "capped at an hour"
        );
        assert_eq!(
            policy("PT2H").refresh_interval,
            Duration::from_secs(30 * 60)
        );
        assert_eq!(
            policy("PT10M").refresh_interval,
            SNAPSHOT_WRITE_INTERVAL,
            "never more often than the write interval"
        );
    }

    #[test]
    fn alerts_that_expired_while_down_are_not_restored() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = wis2_config(None);
        let a = CapEngine::new_with_state(&cfg, "cap-wis2", store(dir.path())).unwrap();
        let received = "2026-09-13T10:00:00Z".parse().unwrap();
        push(&a, "kept", received);
        let short = alert("gone").replace("2099-01-01", "2026-09-13");
        a.wis2_source().unwrap().apply_with_hint(
            resolved("d-gone", 0, Some(short)),
            None,
            "test",
            received,
        );
        assert_eq!(a.wis2_source().unwrap().len(), 2);
        a.shutdown();
        let b = CapEngine::new_with_state(&cfg, "cap-wis2", store(dir.path())).unwrap();
        assert_eq!(b.wis2_source().unwrap().len(), 1);
        assert_eq!(b.feature_count(), 1);
        let page = b.get_features(&FeatureQuery::default()).unwrap();
        assert_eq!(
            page.features[0].properties["identifier"].as_str(),
            Some("kept")
        );
    }

    #[test]
    fn corrupt_snapshot_is_a_cold_start_and_gets_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cap-wis2.cap.state");
        std::fs::write(&file, b"{\"format\": \"meteocore/cap-wis2-acc").unwrap();
        let cfg = wis2_config(None);
        let e = CapEngine::new_with_state(&cfg, "cap-wis2", store(dir.path())).unwrap();
        assert!(!e.is_loaded());
        assert_eq!(e.wis2_source().unwrap().len(), 0);
        subscribe(&e);
        e.refresh().unwrap();
        e.wis2_source().unwrap().mark_filling(Utc::now());
        assert_eq!(e.live_health(), warming(0));
        // The next write replaces the corrupt file with a valid snapshot.
        e.shutdown();
        let bytes = std::fs::read(&file).unwrap();
        assert!(persist::decode(&bytes, "cap-wis2").is_ok());
    }

    #[test]
    fn unwritable_state_dir_never_fails_the_engine() {
        let dir = tempfile::tempdir().unwrap();
        // A regular file where the state directory should be.
        let blocker = dir.path().join("state");
        std::fs::write(&blocker, b"").unwrap();
        let e = CapEngine::new_with_state(&wis2_config(None), "cap-wis2", store(&blocker)).unwrap();
        push(&e, "a", Utc::now());
        e.refresh().unwrap();
        e.shutdown();
        assert_eq!(e.feature_count(), 1);
        // Directory/feed sources ignore state_dir entirely.
        let cfg: CapConfig = serde_json::from_value(serde_json::json!({
            "data_path": concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures")
        }))
        .unwrap();
        let local = CapEngine::new_with_state(&cfg, "local", store(dir.path())).unwrap();
        local.shutdown();
        assert!(!dir.path().join("local.cap.state").exists());
    }

    #[test]
    fn failed_acquisition_advances_expiry_and_cache_time_without_changing_content() {
        use super::*;
        use ds_core::feature_engine::FeatureEngine;
        let cfg: CapConfig = serde_json::from_value(serde_json::json!({"data_path": "."})).unwrap();
        // A WIS2 constructor is network-free; publish the captured alert directly.
        let cfg = CapConfig {
            data_path: None,
            wis2: Some(Wis2Config::default()),
            ..cfg
        };
        let engine = CapEngine::new(&cfg, "test").unwrap();
        let before: DateTime<Utc> = "2026-09-13T10:00:00Z".parse().unwrap();
        let after = before + chrono::Duration::hours(2);
        let xml = include_str!("../tests/fixtures/helsinki-flood.xml").replace(
            "<severity>Severe</severity>",
            "<severity>Severe</severity><expires>2026-09-13T11:00:00+00:00</expires>",
        );
        engine
            .publish_load(
                Ok(SourceLoad {
                    alerts: crate::parser::parse_document(&xml).unwrap(),
                    failed_documents: 0,
                }),
                before,
            )
            .unwrap();
        let version = engine.content_version();
        let render = || {
            engine
                .get_raster_tile(
                    [24.8, 60.0, 25.2, 60.4],
                    16,
                    16,
                    None,
                    &OutputCrs::Wgs84,
                    None,
                    None,
                    None,
                )
                .unwrap()
        };
        assert!(render().values.iter_values().any(|v| v.is_some()));
        assert!(engine
            .publish_load(
                Err(DataServerError::Engine("source unavailable".into())),
                after
            )
            .is_err());
        assert!(!render().values.iter_values().any(|v| v.is_some()));
        assert_eq!(engine.resolve_time(None, None), Some(after));
        assert_eq!(engine.raster_info().times.last(), Some(&after));
        assert_eq!(engine.content_version(), version);
        assert_eq!(
            engine.feature_count(),
            1,
            "Features keeps the retained warning history"
        );
    }
}
