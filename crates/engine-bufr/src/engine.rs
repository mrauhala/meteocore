//! `BufrEngine`: `EdrEngine` (locations / position / area / radius) +
//! `FeatureEngine` (station = Point feature) over the in-memory store.

use std::collections::HashMap;
use std::ops::Bound;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use ds_core::config::{BufrConfig, Wis2Config};
use ds_core::datetime::parse_iso8601_duration;
use ds_core::error::DataServerError;
use ds_core::feature::{
    check_mask_budget, parse_area_coords, parse_point_coords, sort_features, DatetimeInterval,
    Feature, FeaturePage, FeatureQuery,
};
use ds_core::feature_engine::FeatureEngine;
use ds_core::geo::great_circle_distance_m;
use ds_core::health::{LiveStatus, WarmupCause};
use ds_core::model::{
    CoverageResponse, DomainDescription, LocationSnapshot, NdArray, ParameterDescription,
    QueryResult, ValuePrecision,
};
use ds_core::state::{
    collection_key, hours_minutes, StateError, StateStore, StateWriter, WriteOutcome, WritePolicy,
};
use ds_poll::{FirstTick, Shutdown};

use crate::decode::{is_nil_bulletin, DecodeError, Decoder, ObsReport};
use crate::health::Health;
use crate::metadata::Snapshot;
use crate::params::ParameterTable;
use crate::persist;
use crate::source::LocalSource;
use crate::store::{Ingest, ObsStore, StationInfo};
use crate::wis2::{WarmupClock, Wis2Source};
use ds_core::station_map::StationMapSettings;
use ds_render::station_symbols::{StationLayers, StationSymbols};

/// Values (stations × parameters × timesteps) one response may carry. The
/// only bound on an `area` query's size (#1062): the stations it may touch
/// are those of the in-memory store, itself bounded by `max_stations`, so a
/// whole-network instant fits however many stations report.
pub const MAX_RESPONSE_VALUES: usize = 500_000;
/// How often the metadata snapshot is rebuilt when the store changed.
const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(10);
/// How often expired rows / surplus stations are pruned.
const PRUNE_INTERVAL: Duration = Duration::from_secs(60);

pub(crate) enum Source {
    Local(Mutex<LocalSource>),
    Wis2(Box<Wis2Source>),
}

/// What one [`BufrEngine::ingest_bytes_keyed`] call did.
#[derive(Debug, Default)]
pub struct IngestOutcome {
    /// `(station_id, time)` of every report inserted or replaced.
    pub keys: Vec<(String, DateTime<Utc>)>,
    /// Reports decoded before the retention-window / ingest step — "the
    /// decoder understood this centre's payload" even when nothing landed.
    pub decoded_reports: usize,
    /// Per-message decode failures (a concatenated bulletin keeps going).
    pub failed: Vec<DecodeError>,
}

/// Estimated heap the collection holds outside every cache (#1307), for the
/// `bufr_*_bytes` gauges. Kept when the store changes (the store and
/// snapshot figures at each snapshot rebuild), so reading it costs nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StoreMemory {
    /// The report store, as of the last snapshot rebuild.
    pub store_bytes: u64,
    /// The published metadata snapshot.
    pub snapshot_bytes: u64,
    /// WIS2 mode: the `rel=deletion` index's `(data_ids, bytes)`; `None`
    /// for a `data_path` source.
    pub deletion_index: Option<(u64, u64)>,
}

pub struct BufrEngine {
    collection_id: String,
    pub(crate) table: Arc<ParameterTable>,
    decoder: Decoder,
    pub(crate) store: RwLock<ObsStore>,
    pub(crate) snapshot: ArcSwap<Snapshot>,
    dirty: AtomicBool,
    version: AtomicU64,
    source: Source,
    poll_interval: Duration,
    position_radius_m: f64,
    shutdown: Shutdown,
    pub health: Health,
    /// WIS2 mode with a state store: the store snapshot's key and its write
    /// policy (#1002). `None` = no persistence (`data_path` mode re-reads
    /// its files instead).
    state: Option<Mutex<StateWriter>>,
    /// The station-symbol map layers (`[bufr.map]`, #1204).
    pub(crate) map: StationLayers,
}

impl BufrEngine {
    /// Build the engine and, in `data_path` mode, do a best-effort initial
    /// scan so local fixtures serve immediately (a failing source starts
    /// `Degraded`; the poll loop retries).
    pub fn new(config: &BufrConfig, collection_id: &str) -> Result<Self, DataServerError> {
        Self::new_with_state(config, collection_id, None)
    }

    /// [`Self::new`] with the server's state store (`[server] state_dir`,
    /// #1002). In WIS2 mode the store is restored from the state store's
    /// `<collection_id>.bufr` snapshot — rows older than the current
    /// `retention` pruned, columns mapped onto the current parameter table,
    /// the warm-up restarted when the snapshot is older than `warmup` — and
    /// snapshotted back while the poll loop runs. A missing, unreadable or
    /// corrupt snapshot is a cold start (logged), never an error. A
    /// `data_path` source re-reads its files and ignores the store.
    pub fn new_with_state(
        config: &BufrConfig,
        collection_id: &str,
        state: Option<Arc<dyn StateStore>>,
    ) -> Result<Self, DataServerError> {
        let retention = parse_iso8601_duration(&config.retention)?;
        let map = StationLayers::new(StationMapSettings::resolve(config.map.as_ref()).map_err(
            |e| {
                DataServerError::Config(format!(
                    "Collection '{collection_id}': invalid [bufr.map] {e}"
                ))
            },
        )?);
        let table = Arc::new(ParameterTable::build(
            config.builtin_parameters,
            &config.parameters,
        ));
        let source = match (&config.data_path, &config.wis2) {
            (Some(path), None) => {
                Source::Local(Mutex::new(LocalSource::new(collection_id, path)?))
            }
            (None, Some(w)) => Source::Wis2(Box::new(Wis2Source::new(
                w.clone(),
                parse_iso8601_duration(&config.stale_after)?,
                wis2_warmup(w, retention)?,
            ))),
            _ => {
                return Err(DataServerError::Config(format!(
                    "Collection '{collection_id}': bufr requires exactly one of data_path or [bufr.wis2]"
                )))
            }
        };
        let snapshot = ArcSwap::from_pointee(Snapshot::empty(&table, &map));
        let mut engine = BufrEngine {
            collection_id: collection_id.to_string(),
            table,
            decoder: Decoder::new(),
            store: RwLock::new(ObsStore::new(retention, config.max_stations)),
            snapshot,
            dirty: AtomicBool::new(false),
            version: AtomicU64::new(0),
            source,
            poll_interval: Duration::from_secs(config.poll_interval_secs.max(1)),
            position_radius_m: config.position_radius_km * 1000.0,
            shutdown: Shutdown::new(),
            health: Health::new(),
            state: None,
            map,
        };
        // Local mode: best-effort initial scan so fixtures serve at once.
        // WIS2 mode: nothing to load until the broker delivers — the
        // pipeline starts in `poll_loop` (the constructor runs on the
        // request runtime; see crates/ds-wis2/CLAUDE.md) — except what a
        // state snapshot restores, served at once.
        if matches!(engine.source, Source::Local(_)) {
            engine.scan_once();
        }
        let mut restored_age = None;
        if let (Some(store), Source::Wis2(w)) = (state, &engine.source) {
            let mut writer = StateWriter::new(
                store,
                collection_key(collection_id, persist::KIND),
                // A changed store at most every five minutes, checked on
                // every `SNAPSHOT_INTERVAL` tick (`shutdown()` flushes the
                // rest); an unchanged one every quarter of the warm-up.
                WritePolicy::for_warmup(w.warmup().to_std().unwrap_or_default()),
            );
            restored_age = engine.restore_state(&mut writer, Utc::now());
            engine.state = Some(Mutex::new(writer));
        }
        engine.rebuild_snapshot();
        if let (Some(age), Some(writer)) = (restored_age, &engine.state) {
            // The state store holds what was restored: no rewrite until the
            // store changes or the refresh interval runs out.
            writer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .mark_restored(engine.state_revision(), age, Instant::now());
        }
        Ok(engine)
    }

    /// Restore the store from the snapshot `writer` manages (#1002). Returns
    /// the snapshot's age when the store now holds what it held; `None` for
    /// a cold start (no snapshot, or an unreadable or rejected one —
    /// logged, never an error) and after a long outage, whose restarted
    /// warm-up is not in the state store yet (written at the first tick).
    ///
    /// A snapshot written longer than `warmup` before `now` means the
    /// server was down that long: whatever the feed published meanwhile is
    /// missing for good (observations are not republished). Its rows are
    /// restored, but the warm-up restarts — the clock is cleared, so it
    /// starts again when the subscription comes up, and `/health` says
    /// "warming up after a long outage" until it ends.
    fn restore_state(&self, writer: &mut StateWriter, now: DateTime<Utc>) -> Option<Duration> {
        let Source::Wis2(src) = &self.source else {
            return None;
        };
        let id = &self.collection_id;
        let at = writer.describe();
        let bytes = match writer.load() {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                tracing::info!("[{id}] bufr/wis2: no state snapshot at {at} — cold start");
                return None;
            }
            Err(e) => {
                tracing::warn!(
                    "[{id}] bufr/wis2: cannot read state snapshot {at}: {e} — cold start"
                );
                return None;
            }
        };
        let decoded = match persist::decode(&bytes, id, &self.table) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(
                    "[{id}] bufr/wis2: state snapshot {at} rejected ({e}) — cold start; the \
                     next write replaces it"
                );
                return None;
            }
        };
        let age = now - decoded.written_at;
        let long_outage = age > src.warmup();
        // A snapshot from the future (clock skew) is as good as new.
        let age_std = age.to_std().unwrap_or_default();
        let clock = if long_outage {
            WarmupClock {
                filling_since: None,
                cause: WarmupCause::LongOutage,
            }
        } else {
            decoded.clock
        };
        let (reports, stations, dropped) = self.install(src, clock, decoded.stations, now);
        tracing::info!(
            "[{id}] bufr/wis2: restored {reports} report(s) at {stations} station(s) from state \
             snapshot {at} written {} ago ({dropped} past retention or max_stations{})",
            hours_minutes(age_std),
            dropped_columns(&decoded.columns_dropped)
        );
        if long_outage {
            tracing::warn!(
                "[{id}] bufr/wis2: the state snapshot is older than warmup ({}): reports \
                 published during the outage are missing — the warm-up restarts and /health \
                 reports degraded until it ends",
                hours_minutes(src.warmup().to_std().unwrap_or_default())
            );
            return None;
        }
        Some(age_std)
    }

    /// Replace the store with `stations` (rows already in this engine's
    /// column layout) and set the warm-up clock: a snapshot restore and a
    /// take-over alike. The current `retention` and `max_stations` apply,
    /// as a prune tick would. Returns `(reports, stations, rows dropped)`.
    fn install(
        &self,
        src: &Wis2Source,
        clock: WarmupClock,
        stations: Vec<(StationInfo, persist::Rows)>,
        now: DateTime<Utc>,
    ) -> (usize, usize, usize) {
        src.set_warmup_clock(clock);
        let mut store = self.store.write().unwrap_or_else(|e| e.into_inner());
        store.clear();
        for (info, rows) in stations {
            store.restore_station(info, rows);
        }
        let (rows_dropped, _) = store.prune(now);
        (store.row_count(), store.station_count(), rows_dropped)
    }

    /// Take over the report store of the engine this one replaces (#1036).
    /// A reload that rebuilds a WIS2 collection builds this engine first,
    /// and it restores only the last periodic snapshot, while the replaced
    /// engine keeps receiving until the reload is accepted. The server
    /// calls this after `predecessor.shutdown()`, which stops its ingest,
    /// and before this engine's poll loop starts: every report the replaced
    /// engine held moves over in memory, mapped onto this engine's
    /// parameter table and pruned to its `retention` and `max_stations` as
    /// a restore is, with the warm-up clock, the deletion index and the
    /// version. A state store is not involved; this engine's next write
    /// puts the store back in it. A no-op unless both engines are WIS2-fed.
    pub fn take_over(&self, predecessor: &BufrEngine) {
        let (Source::Wis2(src), Source::Wis2(prev)) = (&self.source, &predecessor.source) else {
            return;
        };
        let now = Utc::now();
        let carried = {
            let store = predecessor.store.read().unwrap_or_else(|e| e.into_inner());
            persist::carry_over(
                &store,
                &predecessor.table,
                &self.table,
                prev.warmup_clock(),
                now,
            )
        };
        let (reports, stations, dropped) = self.install(src, carried.clock, carried.stations, now);
        src.take_deletions(prev);
        // Continue the replaced engine's version (the content version and
        // the Features ETag), so none it handed out matches what this
        // engine serves.
        self.version.fetch_max(
            predecessor.version.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.rebuild_snapshot();
        tracing::info!(
            "[{}] bufr/wis2: took over {reports} report(s) at {stations} station(s) from the \
             engine this reload replaced ({dropped} past retention or max_stations{})",
            self.collection_id,
            dropped_columns(&carried.columns_dropped)
        );
    }

    /// What the state snapshot's write policy compares: moves whenever the
    /// store changed (every change is followed by a metadata rebuild,
    /// which bumps `version`) or the warm-up clock started. Both counters
    /// only grow, so their sum moves when either does.
    fn state_revision(&self) -> u64 {
        let clock = match &self.source {
            Source::Wis2(w) => w.clock_changes(),
            Source::Local(_) => 0,
        };
        self.version.load(Ordering::Relaxed).wrapping_add(clock)
    }

    /// Write the store's state snapshot when its write policy says so
    /// ([`WritePolicy::for_warmup`]; `force` always writes). Never fails the
    /// caller: a failed write keeps the previous snapshot and is logged, as
    /// a WARN at most every [`WritePolicy::WARN_INTERVAL`]. A forced write,
    /// the shutdown flush, is logged at INFO with its duration and size: it
    /// is what a container's stop timeout has to cover (#1043). The encode
    /// reads the store under its read lock — requests keep reading; the only
    /// writer is this engine's own poll loop, which is the caller (the
    /// shutdown flush may wait for the loop's last write, and its duration
    /// includes that wait).
    ///
    /// Returns what the write policy did, `None` without a state store (and
    /// in `data_path` mode, which never persists).
    pub(crate) fn write_state(&self, force: bool) -> Option<WriteOutcome> {
        let (Some(writer), Source::Wis2(src)) = (&self.state, &self.source) else {
            return None;
        };
        let started = Instant::now();
        let mut writer = writer.lock().unwrap_or_else(|e| e.into_inner());
        let mut reports = 0;
        let outcome = writer.write_if_due(self.state_revision(), Instant::now(), force, || {
            let store = self.store.read().unwrap_or_else(|e| e.into_inner());
            reports = store.row_count();
            persist::encode(
                &self.collection_id,
                &store,
                &self.table,
                src.warmup_clock(),
                Utc::now(),
            )
            .map_err(StateError::Encode)
        });
        let id = &self.collection_id;
        match &outcome {
            WriteOutcome::NotDue => {}
            WriteOutcome::Written { bytes } if force => tracing::info!(
                "[{id}] bufr/wis2: state snapshot flushed in {:.1?}: {reports} report(s), \
                 {bytes} bytes to {}",
                started.elapsed(),
                writer.describe()
            ),
            WriteOutcome::Written { bytes } => tracing::debug!(
                "[{id}] bufr/wis2: state snapshot written: {reports} report(s), {bytes} bytes \
                 to {}",
                writer.describe()
            ),
            WriteOutcome::Failed { error, warn: true } => tracing::warn!(
                "[{id}] bufr/wis2: cannot write state snapshot {}: {error} — the previous one \
                 stays; retrying every {} min, this warning repeats at most every {} min",
                writer.describe(),
                WritePolicy::WRITE_INTERVAL.as_secs() / 60,
                WritePolicy::WARN_INTERVAL.as_secs() / 60
            ),
            WriteOutcome::Failed { error, warn: false } => {
                tracing::debug!("[{id}] bufr/wis2: state snapshot write failed again: {error}")
            }
        }
        Some(outcome)
    }

    /// Hand the station map layers their style facts (#1204): each
    /// parameter layer's midpoint, which the overlap order measures from.
    /// The server calls this whenever it wires the collection to a map API,
    /// built or reused, since a reload can change the styles alone.
    pub fn set_station_symbols(&self, symbols: StationSymbols) {
        self.map.set_symbols(symbols);
    }

    /// Whether this collection is fed by a WIS2 subscription.
    pub fn is_wis2(&self) -> bool {
        matches!(self.source, Source::Wis2(_))
    }

    /// WIS2 broker / pipeline counters for `/metrics` (`None` unless WIS2
    /// mode and the pipeline has started).
    pub fn wis2_status(&self) -> Option<ds_wis2::StatusSnapshot> {
        match &self.source {
            Source::Wis2(w) => w.status_snapshot(),
            Source::Local(_) => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn source(&self) -> &Source {
        &self.source
    }

    /// The WIS2 subscription config (WIS2 mode only).
    pub fn wis2_config(&self) -> Option<&Wis2Config> {
        match &self.source {
            Source::Wis2(w) => Some(w.config()),
            Source::Local(_) => None,
        }
    }

    pub fn collection_id(&self) -> &str {
        &self.collection_id
    }

    /// Runtime health: `None` until the source has been probed once (boot
    /// snapshot stands), then Ready / Degraded from the source's state.
    pub fn live_health(&self) -> Option<LiveStatus> {
        self.live_health_at(Utc::now())
    }

    fn live_health_at(&self, now: DateTime<Utc>) -> Option<LiveStatus> {
        match &self.source {
            Source::Local(_) => self.health.local_status(),
            Source::Wis2(w) => Some(w.live_status(&self.health, self.gauges().1 as u64, now)),
        }
    }

    /// Whether the first scan succeeded (boot readiness; `Local` mode).
    pub fn is_loaded(&self) -> bool {
        match &self.source {
            Source::Local(_) => {
                self.health.is_probed()
                    && self.health.scan_failures_total.load(Ordering::Relaxed)
                        < self.health.scans_total.load(Ordering::Relaxed).max(1)
            }
            Source::Wis2(_) => self.health.is_probed(),
        }
    }

    /// `(stations, reports)` currently held.
    pub fn gauges(&self) -> (usize, usize) {
        let s = self.snapshot.load();
        (s.stations.len(), s.report_count)
    }

    /// Estimated heap of the store, its snapshot and the WIS2 deletion
    /// index (#1307). O(1): the figures kept when they last changed.
    pub fn store_memory(&self) -> StoreMemory {
        let snapshot = self.snapshot.load();
        StoreMemory {
            store_bytes: snapshot.store_bytes,
            snapshot_bytes: snapshot.bytes,
            deletion_index: match &self.source {
                Source::Wis2(w) => Some(w.deletion_index()),
                Source::Local(_) => None,
            },
        }
    }

    /// Newest report time across stations (for `/health` data age).
    pub fn latest_report(&self) -> Option<DateTime<Utc>> {
        self.snapshot.load().temporal_extent.map(|(_, b)| b)
    }

    /// One scan of the local source: fetch new files, decode, ingest. Files
    /// are ingested as each fetch chunk lands, so a backlog is never held in
    /// memory whole.
    fn scan_once(&self) {
        let Source::Local(src) = &self.source else {
            return;
        };
        let now = Utc::now();
        let mut ingested = 0usize;
        let mut src = src.lock().unwrap_or_else(|e| e.into_inner());
        let result = src.scan(|f| {
            self.health.files_total.fetch_add(1, Ordering::Relaxed);
            ingested += self.ingest_bytes(&f.bytes, &f.path, now);
        });
        match result {
            Ok(0) => self.health.record_scan(true),
            Ok(files) => {
                self.health.record_scan(true);
                tracing::info!(
                    "[{}] bufr: {files} new file(s), {ingested} report(s) ingested from '{}'",
                    self.collection_id,
                    src.label()
                );
            }
            Err(e) => {
                self.health.record_scan(false);
                tracing::warn!(
                    "[{}] bufr: scan of '{}' failed: {e}",
                    self.collection_id,
                    src.label()
                );
            }
        }
    }

    /// Decode one BUFR byte stream and ingest its reports. Returns the number
    /// of reports inserted or replaced. Shared by the local scan and the
    /// WIS2 source.
    pub fn ingest_bytes(&self, bytes: &[u8], label: &str, now: DateTime<Utc>) -> usize {
        self.ingest_bytes_keyed(bytes, label, now)
            .map(|o| o.keys.len())
            .unwrap_or(0)
    }

    /// [`Self::ingest_bytes`] with the full outcome: the `(station_id, time)`
    /// of every report inserted or replaced — the WIS2 source remembers them
    /// per `data_id` so a later `rel=deletion` can withdraw exactly those
    /// rows — plus how many reports decoded and the per-message failures
    /// (counted here, returned for the caller to log). `Err` only when the
    /// bytes hold no BUFR message at all and are not a NIL bulletin: that
    /// is an empty outcome, counted in `Health::nil_bulletins_total`.
    pub fn ingest_bytes_keyed(
        &self,
        bytes: &[u8],
        label: &str,
        now: DateTime<Utc>,
    ) -> Result<IngestOutcome, DecodeError> {
        if is_nil_bulletin(bytes) {
            // The centre had no report to send: its own ingest result, not
            // a decode failure, so a NIL-sending centre neither moves the
            // failure counter nor the WIS2 failure WARNs (#1043).
            self.health
                .nil_bulletins_total
                .fetch_add(1, Ordering::Relaxed);
            tracing::debug!("[{}] bufr: '{label}' is a NIL bulletin", self.collection_id);
            return Ok(IngestOutcome::default());
        }
        let decoded = match self.decoder.decode(bytes) {
            Ok(d) => d,
            Err(e) => {
                // No BUFR magic at all — the whole object is not a message.
                self.health.record_decode_failure(e.kind());
                tracing::debug!("[{}] bufr: '{label}' not decoded: {e}", self.collection_id);
                return Err(e);
            }
        };
        // Per-message failures: the other messages of a concatenated file
        // still ingest below.
        for e in &decoded.failed {
            self.health.record_decode_failure(e.kind());
            tracing::debug!("[{}] bufr: '{label}' failed: {e}", self.collection_id);
        }
        self.health
            .subsets_skipped_total
            .fetch_add(decoded.skipped.len() as u64, Ordering::Relaxed);
        let mut keys = Vec::new();
        {
            let mut store = self.store.write().unwrap_or_else(|e| e.into_inner());
            // Shut down: nothing more lands in the store (see `shutdown`).
            let reports = if self.shutdown.is_shutdown() {
                &[][..]
            } else {
                &decoded.reports[..]
            };
            for r in reports {
                match store.ingest(r, &self.table, now) {
                    Ingest::Inserted => {
                        self.health
                            .reports_ingested_total
                            .fetch_add(1, Ordering::Relaxed);
                        keys.push((r.station_id.clone(), r.time));
                    }
                    Ingest::Replaced => {
                        self.health
                            .reports_replaced_total
                            .fetch_add(1, Ordering::Relaxed);
                        keys.push((r.station_id.clone(), r.time));
                    }
                    Ingest::OutOfWindow => {
                        self.health
                            .reports_out_of_window_total
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        if !keys.is_empty() {
            self.dirty.store(true, Ordering::Release);
        }
        Ok(IngestOutcome {
            keys,
            decoded_reports: decoded.reports.len(),
            failed: decoded.failed,
        })
    }

    /// Ingest already decoded reports as a payload's are, no decoder
    /// involved. Returns how many landed. For the heap-gauge test (#1307),
    /// which needs a network of thousands of stations no fixture carries;
    /// not part of the engine's API.
    #[doc(hidden)]
    pub fn ingest_reports(&self, reports: &[ObsReport], now: DateTime<Utc>) -> usize {
        let landed = {
            let mut store = self.store.write().unwrap_or_else(|e| e.into_inner());
            reports
                .iter()
                .filter(|r| store.ingest(r, &self.table, now) != Ingest::OutOfWindow)
                .count()
        };
        if landed > 0 {
            self.dirty.store(true, Ordering::Release);
        }
        landed
    }

    /// Remember the rows `data_id` produced in the WIS2 deletion index, as
    /// an accepted notification's are; a no-op for a `data_path` source.
    /// For the heap-gauge test (#1307); not part of the engine's API.
    #[doc(hidden)]
    pub fn record_deletion_keys(&self, data_id: String, keys: Vec<(String, DateTime<Utc>)>) {
        if let Source::Wis2(w) = &self.source {
            w.record_produced(data_id, keys);
        }
    }

    /// Withdraw specific reports (WIS2 `rel=deletion`). Returns how many
    /// existed.
    pub fn remove_reports(&self, keys: &[(String, DateTime<Utc>)]) -> usize {
        let mut store = self.store.write().unwrap_or_else(|e| e.into_inner());
        if self.shutdown.is_shutdown() {
            return 0;
        }
        let n = keys.iter().filter(|(id, t)| store.remove(id, *t)).count();
        if n > 0 {
            self.dirty.store(true, Ordering::Release);
        }
        n
    }

    fn rebuild_snapshot(&self) {
        let version = self.version.fetch_add(1, Ordering::Relaxed) + 1;
        let snap = {
            let store = self.store.read().unwrap_or_else(|e| e.into_inner());
            Snapshot::build(&store, version, &self.table, &self.map)
        };
        self.snapshot.store(Arc::new(snap));
    }

    fn prune(&self) {
        let (rows, stations) = {
            let mut store = self.store.write().unwrap_or_else(|e| e.into_inner());
            store.prune(Utc::now())
        };
        if rows > 0 || stations > 0 {
            self.dirty.store(true, Ordering::Release);
            tracing::debug!(
                "[{}] bufr: pruned {rows} report(s), {stations} station(s)",
                self.collection_id
            );
        }
    }

    /// Background loop (poll runtime): feed the store from the source
    /// (periodic scans, or the WIS2 pipeline started here), prune, and
    /// rebuild the metadata snapshot when the store changed. Exits on
    /// `shutdown()`.
    pub async fn poll_loop(&self) {
        match &self.source {
            Source::Local(_) => self.local_loop().await,
            Source::Wis2(w) => w.run(self, &self.shutdown).await,
        }
        tracing::info!("[{}] bufr: poll loop shutting down", self.collection_id);
    }

    async fn local_loop(&self) {
        let mut scan = self.shutdown.ticker(self.poll_interval, FirstTick::Skip);
        let mut snap = self.shutdown.ticker(SNAPSHOT_INTERVAL, FirstTick::Skip);
        let mut prune = self.shutdown.ticker(PRUNE_INTERVAL, FirstTick::Skip);
        loop {
            tokio::select! {
                biased;
                _ = self.shutdown.wait() => break,
                _ = scan.tick() => self.scan_once(),
                _ = prune.tick() => self.prune(),
                _ = snap.tick() => self.snapshot_if_dirty(),
            }
        }
    }

    /// Rebuild the snapshot when the store changed since the last build.
    /// Public for the heap-gauge test (#1307) only.
    #[doc(hidden)]
    pub fn snapshot_if_dirty(&self) {
        if self.dirty.swap(false, Ordering::AcqRel) {
            self.rebuild_snapshot();
        }
    }

    pub(crate) fn prune_now(&self) {
        self.prune();
    }

    pub(crate) const SNAPSHOT_INTERVAL: Duration = SNAPSHOT_INTERVAL;
    pub(crate) const PRUNE_INTERVAL: Duration = PRUNE_INTERVAL;

    /// Signal the poll loop to stop; the store takes no report from here
    /// on (#1036). With a state store, also flush the snapshot (#1002): a
    /// graceful restart loses nothing, and the snapshot's `written_at`
    /// records when the server went down.
    pub fn shutdown(&self) {
        {
            // Under the write lock, which every ingest holds while it
            // checks the signal: an ingest lands either before it or not at
            // all, never after the flush below or a successor's take-over.
            let _store = self.store.write().unwrap_or_else(|e| e.into_inner());
            self.shutdown.shutdown();
        }
        self.write_state(true);
    }

    // ---- query helpers -------------------------------------------------

    /// The table columns `parameter-name` selects, through the shared rule:
    /// an unknown name or an empty list is a 400 naming the valid ones, and
    /// matching ignores case (#666, #966).
    fn selected_params(
        &self,
        parameters: Option<&[String]>,
    ) -> Result<Vec<usize>, DataServerError> {
        let names = self.table.names();
        let available: Vec<&str> = names.iter().map(String::as_str).collect();
        Ok(
            ds_core::edr_engine::select_parameters(parameters, &available)?
                .into_iter()
                .filter_map(|name| self.table.index_of(name))
                .collect(),
        )
    }

    /// Build one station's `PointSeries` coverage. `budget` is decremented
    /// by the values emitted; exhausting it is a `QueryTooLarge`.
    fn station_series(
        &self,
        store: &ObsStore,
        station_id: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        params: &[usize],
        budget: &mut usize,
    ) -> Result<Option<QueryResult>, DataServerError> {
        let Some(series) = store.get(station_id) else {
            return Ok(None);
        };
        let rows: Vec<(&DateTime<Utc>, &Box<[f32]>)> = match datetime {
            Some((start, end)) => series.rows.range(start..=end).collect(),
            None => series.rows.iter().collect(),
        };
        if rows.is_empty() {
            return Ok(None);
        }
        let needed = rows.len() * params.len();
        if needed > *budget {
            return Err(DataServerError::QueryTooLarge(format!(
                "response would carry more than {MAX_RESPONSE_VALUES} values — narrow the \
                 datetime window, the area or the parameter list"
            )));
        }
        *budget -= needed;
        let times: Vec<DateTime<Utc>> = rows.iter().map(|(t, _)| **t).collect();
        let mut ranges = HashMap::with_capacity(params.len());
        let mut descs = HashMap::with_capacity(params.len());
        for &pi in params {
            let p = &self.table.params[pi];
            let values: Vec<Option<f64>> = rows
                .iter()
                .map(|(_, r)| {
                    let v = r[pi];
                    (!v.is_nan()).then_some(round_stored(v))
                })
                .collect();
            ranges.insert(
                p.name.clone(),
                NdArray {
                    shape: vec![times.len()],
                    axis_names: vec!["t".to_string()],
                    values,
                    precision: ValuePrecision::F64,
                },
            );
            descs.insert(
                p.name.clone(),
                ParameterDescription {
                    label: p.label.clone(),
                    unit: p.unit.clone(),
                    observed_property: p.observed_property.clone(),
                    standard_name: p.standard_name.clone(),
                },
            );
        }
        Ok(Some(QueryResult {
            domain: DomainDescription::PointSeries {
                x: series.info.lon,
                y: series.info.lat,
                t: times,
                z: None,
            },
            parameters: descs,
            ranges,
        }))
    }
}

/// The resolved `[bufr.wis2] warmup`: as configured, else the shorter of
/// `retention` and [`ds_core::config::DEFAULT_WIS2_WARMUP`] (`PT24H`,
/// CAP's default). The store is complete only once it holds a full
/// retention window, but waiting for that kept a collection with a 7-day
/// retention `degraded` for about a week after a cold start; a retention
/// under a day still warms up for its own window. An explicit `warmup`
/// keeps its value, even longer than a day or than `retention`.
fn wis2_warmup(
    w: &Wis2Config,
    retention: chrono::Duration,
) -> Result<chrono::Duration, DataServerError> {
    let warmup = w.warmup_duration()?;
    Ok(match w.warmup {
        Some(_) => warmup,
        None => warmup.min(retention),
    })
}

/// The restore log's note on columns whose values were dropped (empty
/// when none were).
fn dropped_columns(columns: &[String]) -> String {
    if columns.is_empty() {
        String::new()
    } else {
        format!(
            "; column(s) no longer configured as written, values dropped: {}",
            columns.join(", ")
        )
    }
}

/// Whether `station` has at least one report INSIDE `interval`: the one
/// `datetime` rule of Features `/items` and EDR `/locations` (#682, #932).
/// `[first_report, last_report]` overlapping the interval is only the cheap
/// prefilter (hourly SYNOP leaves gaps a narrow window can fall into); the
/// rows themselves decide, one BTreeMap range probe. A reversed interval
/// matches nothing (the API layers reject one; `range` would panic on it).
fn has_report_in(store: &ObsStore, station: &StationInfo, interval: &DatetimeInterval) -> bool {
    if let (Some(start), Some(end)) = (interval.start, interval.end) {
        if start > end {
            return false;
        }
    }
    let overlaps = interval
        .start
        .is_none_or(|start| station.last_report >= start)
        && interval.end.is_none_or(|end| station.first_report <= end);
    overlaps
        && store.get(&station.id).is_some_and(|series| {
            let lo = interval.start.map_or(Bound::Unbounded, Bound::Included);
            let hi = interval.end.map_or(Bound::Unbounded, Bound::Included);
            series.rows.range((lo, hi)).next().is_some()
        })
}

/// Rows are `f32` (BUFR values carry at most ~7 significant digits); widen
/// back to the decimal the producer encoded rather than the binary
/// expansion (`290.12`, not `290.1199951171875`).
pub(crate) fn round_stored(v: f32) -> f64 {
    ((v as f64) * 1e5).round() / 1e5
}

// ---------------------------------------------------------------------------
// EdrEngine
// ---------------------------------------------------------------------------

impl ds_core::edr_engine::EdrEngine for BufrEngine {
    fn get_locations(&self) -> Result<Arc<LocationSnapshot>, DataServerError> {
        Ok(Arc::clone(&self.snapshot.load().locations))
    }

    /// `/locations?datetime=` (#932): stations with a report inside one of
    /// the intervals, the Features `datetime` rule (`has_report_in`). Holds
    /// the store's read lock while the API filters the list, as one
    /// `get_features` does.
    fn location_time_filter<'a>(
        &'a self,
        intervals: &'a [DatetimeInterval],
    ) -> Option<ds_core::edr_engine::LocationFilter<'a>> {
        let snap = self.snapshot.load_full();
        let store = self.store.read().unwrap_or_else(|e| e.into_inner());
        Some(Box::new(move |location| {
            snap.index
                .get(location.id.as_str())
                .and_then(|&i| snap.stations.get(i))
                .is_some_and(|station| {
                    intervals
                        .iter()
                        .any(|interval| has_report_in(&store, station, interval))
                })
        }))
    }

    fn query_location(
        &self,
        location_id: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        let params = self.selected_params(parameters)?;
        let store = self.store.read().unwrap_or_else(|e| e.into_inner());
        if store.get(location_id).is_none() {
            return Err(DataServerError::LocationNotFound(location_id.to_string()));
        }
        let mut budget = MAX_RESPONSE_VALUES;
        match self.station_series(&store, location_id, datetime, &params, &mut budget)? {
            Some(q) => Ok(CoverageResponse::Single(q)),
            None => Err(DataServerError::LocationNotFound(format!(
                "{location_id} (no data in time range)"
            ))),
        }
    }

    fn query_position(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        // `parse_point_coords` returns (lat, lon).
        let (lat, lon) = parse_point_coords(coords)?;
        let snap = self.snapshot.load();
        let mut best: Option<(f64, &str)> = None;
        for s in snap.stations.iter() {
            let d = great_circle_distance_m(lon, lat, s.lon, s.lat);
            if d <= self.position_radius_m && best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, &s.id));
            }
        }
        let Some((_, id)) = best else {
            return Err(DataServerError::LocationNotFound(format!(
                "no station within {:.0} m of ({lon}, {lat})",
                self.position_radius_m
            )));
        };
        let id = id.to_string();
        drop(snap);
        self.query_location(&id, datetime, parameters, z, reference_time)
    }

    fn query_area(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        let polygon = parse_area_coords(coords)?;
        let snap = self.snapshot.load();
        check_mask_budget(snap.stations.len(), &polygon)?;
        let inside: Vec<&str> = snap
            .stations
            .iter()
            .filter(|s| polygon.contains(s.lon, s.lat))
            .map(|s| &*s.id)
            .collect();
        let params = self.selected_params(parameters)?;
        let store = self.store.read().unwrap_or_else(|e| e.into_inner());
        let mut budget = MAX_RESPONSE_VALUES;
        let mut out = Vec::with_capacity(inside.len());
        for id in inside {
            if let Some(q) = self.station_series(&store, id, datetime, &params, &mut budget)? {
                out.push(q);
            }
        }
        Ok(CoverageResponse::Collection(out))
    }

    fn get_parameters(&self) -> Vec<String> {
        self.table.names()
    }

    fn get_parameter_descriptions(&self) -> HashMap<String, ParameterDescription> {
        self.table.descriptions()
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        self.snapshot.load().temporal_extent
    }

    /// Every distinct report time held, from the snapshot (so at most 10 s
    /// behind the store), for a station grid without a `cadence` (#1206).
    /// Not advertised: `get_available_times` stays `None`.
    fn observation_instants(&self) -> Option<Arc<[DateTime<Utc>]>> {
        Some(Arc::clone(&self.snapshot.load().instants))
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        self.snapshot.load().spatial_extent
    }

    fn supported_query_types(&self) -> Vec<String> {
        ["locations", "position", "area", "radius"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    /// One `PointSeries` per station, at the station's decoded position
    /// (the snapshot's `get_locations` entry): EDR GeoJSON names each (#929).
    fn serves_station_series(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// FeatureEngine
// ---------------------------------------------------------------------------

const SORTABLES: &[&str] = &["last_report", "first_report", "report_count", "name"];

impl FeatureEngine for BufrEngine {
    fn filterables(&self) -> ds_core::feature::FilterableProperties {
        static NAMES: std::sync::LazyLock<ds_core::feature::FilterableProperties> =
            std::sync::LazyLock::new(|| {
                Arc::new(
                    [
                        "name",
                        "wigos_station_identifier",
                        "elevation",
                        "first_report",
                        "last_report",
                        "report_count",
                    ]
                    .into_iter()
                    .map(String::from)
                    .collect(),
                )
            });
        NAMES.clone()
    }

    fn get_features(&self, query: &FeatureQuery) -> Result<FeaturePage, DataServerError> {
        let snap = self.snapshot.load();
        // `datetime`: a station matches when it has at least one report
        // inside the interval (`has_report_in`). One read lock for the whole
        // filter.
        let store = query
            .datetime
            .as_ref()
            .map(|_| self.store.read().unwrap_or_else(|e| e.into_inner()));
        let mut features: Vec<Feature> = snap
            .features
            .iter()
            .zip(snap.stations.iter())
            .filter(|(_, s)| match &query.bbox {
                Some(b) => b.contains(s.lon, s.lat),
                None => true,
            })
            .filter(|(_, s)| match (&query.datetime, &store) {
                (Some(dt), Some(store)) => has_report_in(store, s, dt),
                _ => true,
            })
            .filter(|(f, _)| ds_core::feature::matches_property_filters(f, &query.property_filters))
            .map(|(f, _)| f.clone())
            .collect();
        drop(store);
        sort_features(&mut features, &query.sortby);
        let number_matched = features.len();
        let offset = query.offset.min(number_matched);
        let end = offset.saturating_add(query.limit).min(number_matched);
        let page: Vec<Feature> = features[offset..end].to_vec();
        let number_returned = page.len();
        Ok(FeaturePage {
            features: page,
            number_matched,
            number_returned,
            next_offset: (end < number_matched).then_some(end),
        })
    }

    fn get_feature(&self, feature_id: &str) -> Result<Feature, DataServerError> {
        let snap = self.snapshot.load();
        snap.index
            .get(feature_id)
            .map(|&i| snap.features[i].clone())
            .ok_or_else(|| DataServerError::FeatureNotFound(feature_id.to_string()))
    }

    fn feature_count(&self) -> usize {
        self.snapshot.load().features.len()
    }

    fn sortables(&self) -> &[&'static str] {
        SORTABLES
    }

    fn spatial_extent(&self) -> Option<[f64; 4]> {
        self.snapshot.load().spatial_extent
    }

    fn temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        self.snapshot.load().temporal_extent
    }

    fn data_version(&self) -> u64 {
        self.snapshot.load().version
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use ds_core::config::BufrConfig;
    use ds_core::feature::{DatetimeInterval, FeatureQuery};
    use ds_core::feature_engine::FeatureEngine;

    fn fixtures_dir() -> String {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/bufr-synop")
            .to_string_lossy()
            .into_owned()
    }

    fn report(id: &str, t: DateTime<Utc>) -> crate::decode::ObsReport {
        crate::decode::ObsReport {
            station_id: id.into(),
            name: None,
            lat: 60.0,
            lon: 25.0,
            elevation: None,
            time: t,
            elements: Vec::new(),
        }
    }

    #[test]
    fn features_datetime_requires_a_report_inside_the_interval() {
        let cfg = BufrConfig {
            data_path: Some(fixtures_dir()),
            wis2: None,
            poll_interval_secs: 60,
            retention: "P36500D".to_string(),
            max_stations: 50_000,
            stale_after: "PT2H".to_string(),
            position_radius_km: 25.0,
            builtin_parameters: true,
            parameters: Vec::new(),
            map: None,
        };
        let e = BufrEngine::new(&cfg, "bufr-gap").unwrap();
        // One station reporting at 08:00 and 09:00 only.
        let h = |hh: u32, mm: u32| Utc.with_ymd_and_hms(2026, 9, 12, hh, mm, 0).unwrap();
        {
            let mut store = e.store.write().unwrap();
            store.ingest(&report("gap-station", h(8, 0)), &e.table, h(9, 0));
            store.ingest(&report("gap-station", h(9, 0)), &e.table, h(9, 0));
        }
        e.rebuild_snapshot();
        let mut query = FeatureQuery {
            property_filters: vec![("wigos_station_identifier".into(), "gap-station".into())],
            limit: 1,
            ..Default::default()
        };
        let page = e.get_features(&query).unwrap();
        assert_eq!(page.number_matched, 1);
        assert_eq!(page.features[0].id, "gap-station");
        // The station is the EDR location of its id (#1143).
        assert_eq!(
            page.features[0].related,
            [ds_core::feature::Related::edr_location("gap-station")]
        );
        query.offset = 1;
        assert_eq!(e.get_features(&query).unwrap().number_returned, 0);
        query
            .property_filters
            .push(("name".into(), "no such station".into()));
        assert_eq!(e.get_features(&query).unwrap().number_matched, 0);

        let matched = |start: Option<DateTime<Utc>>, end: Option<DateTime<Utc>>| {
            e.get_features(&FeatureQuery {
                datetime: Some(DatetimeInterval { start, end }),
                ..FeatureQuery::default()
            })
            .unwrap()
            .features
            .iter()
            .any(|f| f.id == "gap-station")
        };
        // A window in the gap between the two reports overlaps
        // [first_report, last_report] but holds no report: no match.
        assert!(!matched(Some(h(8, 15)), Some(h(8, 45))));
        // Windows containing a report (bounds inclusive) do match.
        assert!(matched(Some(h(8, 0)), Some(h(8, 30))));
        assert!(matched(Some(h(8, 30)), Some(h(9, 0))));
        assert!(matched(Some(h(8, 30)), None));
        assert!(matched(None, Some(h(8, 0))));
        // Entirely outside: no match.
        assert!(!matched(Some(h(9, 1)), None));
        assert!(!matched(None, Some(h(7, 59))));
    }

    /// The distinct report times a station grid without a `cadence` takes
    /// as its analysis times (#1206): every instant the store holds, an
    /// off-hour report included, from the snapshot, so a report shows
    /// after the rebuild and a withdrawn one is gone after the next. The
    /// public metadata does not list them.
    #[test]
    fn observation_instants_follow_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = BufrConfig {
            data_path: Some(dir.path().to_string_lossy().into_owned()),
            wis2: None,
            poll_interval_secs: 60,
            retention: "P36500D".to_string(),
            max_stations: 50_000,
            stale_after: "PT2H".to_string(),
            position_radius_km: 25.0,
            builtin_parameters: true,
            parameters: Vec::new(),
            map: None,
        };
        let e = BufrEngine::new(&cfg, "bufr-instants").unwrap();
        assert_eq!(e.observation_instants().as_deref(), Some(&[][..]));
        let h = |hh: u32, mm: u32| Utc.with_ymd_and_hms(2026, 9, 12, hh, mm, 0).unwrap();
        {
            let mut store = e.store.write().unwrap();
            for (id, t) in [
                ("a", h(9, 0)),
                ("b", h(9, 0)),
                ("a", h(8, 0)),
                ("ship:XYZ", h(9, 20)),
            ] {
                store.ingest(&report(id, t), &e.table, h(9, 30));
            }
        }
        assert_eq!(
            e.observation_instants().as_deref(),
            Some(&[][..]),
            "the snapshot, not the store"
        );
        e.rebuild_snapshot();
        assert_eq!(
            e.observation_instants().as_deref(),
            Some(&[h(8, 0), h(9, 0), h(9, 20)][..])
        );
        assert_eq!(e.remove_reports(&[("ship:XYZ".to_string(), h(9, 20))]), 1);
        e.rebuild_snapshot();
        assert_eq!(
            e.observation_instants().as_deref(),
            Some(&[h(8, 0), h(9, 0)][..])
        );
        assert_eq!(e.get_available_times(), None);
    }

    /// The memory figures (#1307) are the snapshot's: kept at each rebuild,
    /// as the store stood then; a `data_path` source has no deletion index.
    #[test]
    fn store_memory_is_kept_at_each_snapshot_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = BufrConfig {
            data_path: Some(dir.path().to_string_lossy().into_owned()),
            wis2: None,
            poll_interval_secs: 60,
            retention: "P36500D".to_string(),
            max_stations: 50_000,
            stale_after: "PT2H".to_string(),
            position_radius_km: 25.0,
            builtin_parameters: true,
            parameters: Vec::new(),
            map: None,
        };
        let e = BufrEngine::new(&cfg, "bufr-memory").unwrap();
        let empty = e.store_memory();
        assert_eq!((empty.store_bytes, empty.deletion_index), (0, None));
        assert!(empty.snapshot_bytes > 0, "the map layers' metadata");
        let h = |hh: u32| Utc.with_ymd_and_hms(2026, 9, 12, hh, 0, 0).unwrap();
        {
            let mut store = e.store.write().unwrap();
            for (id, t) in [("a", h(8)), ("a", h(9)), ("b", h(9))] {
                store.ingest(&report(id, t), &e.table, h(9));
            }
        }
        assert_eq!(e.store_memory(), empty, "nothing until the rebuild");
        e.rebuild_snapshot();
        let held = e.store_memory();
        assert_eq!(
            held.store_bytes,
            e.store.read().unwrap().heap_bytes(),
            "the store as of the rebuild"
        );
        assert!(held.snapshot_bytes > empty.snapshot_bytes);
        assert_eq!(e.remove_reports(&[("b".to_string(), h(9))]), 1);
        e.rebuild_snapshot();
        assert!(e.store_memory().store_bytes < held.store_bytes);
    }

    // ---- area: the value budget is the only bound (#1062) ------------------

    /// 20 000 stations on a 200 × 100 grid (in the South Pacific, clear of
    /// the fixtures), each reporting air temperature and station pressure at
    /// `times`, in an engine whose `data_path` is an empty directory.
    fn network(times: &[DateTime<Utc>]) -> (BufrEngine, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = BufrConfig {
            data_path: Some(dir.path().to_string_lossy().into_owned()),
            wis2: None,
            poll_interval_secs: 60,
            retention: "P36500D".to_string(),
            max_stations: 50_000,
            stale_after: "PT2H".to_string(),
            position_radius_km: 25.0,
            builtin_parameters: true,
            parameters: Vec::new(),
            map: None,
        };
        let e = BufrEngine::new(&cfg, "bufr-network").unwrap();
        {
            let mut store = e.store.write().unwrap();
            for n in 0..20_000u32 {
                for &time in times {
                    let report = crate::decode::ObsReport {
                        lon: -170.0 + f64::from(n % 200) * 0.1,
                        lat: -60.0 + f64::from(n / 200) * 0.1,
                        elements: vec![
                            crate::decode::Element {
                                xy: crate::decode::XY { x: 12, y: 101 },
                                value: 273.15 + f64::from(n % 30),
                                period_hours: None,
                            },
                            crate::decode::Element {
                                xy: crate::decode::XY { x: 10, y: 4 },
                                value: 100_000.0 + f64::from(n % 50),
                                period_hours: None,
                            },
                        ],
                        ..report(&format!("0-20000-0-{n:05}"), time)
                    };
                    assert_ne!(store.ingest(&report, &e.table, time), Ingest::OutOfWindow);
                }
            }
        }
        e.rebuild_snapshot();
        (e, dir)
    }

    const NETWORK_AREA: &str = "POLYGON((-171 -61,-149 -61,-149 -49,-171 -49,-171 -61))";

    /// Every station's value at one instant is a basic observation request:
    /// past the old 10 001-station ceiling, the 40 000 values fit the budget
    /// and every station is in the answer.
    #[test]
    fn area_instant_over_twenty_thousand_stations_returns_every_station() {
        let t0 = Utc.with_ymd_and_hms(2026, 9, 12, 6, 0, 0).unwrap();
        let (e, _dir) = network(&[t0]);
        let params = ["air_temperature".to_string(), "pressure".to_string()];
        let CoverageResponse::Collection(coverages) = e
            .query_area(NETWORK_AREA, Some((t0, t0)), Some(&params), None, None)
            .unwrap()
        else {
            panic!("an area query answers a collection");
        };
        assert_eq!(coverages.len(), 20_000);
        let mut points = std::collections::HashSet::new();
        for c in &coverages {
            let DomainDescription::PointSeries { x, y, t, .. } = &c.domain else {
                panic!("a station answers a PointSeries");
            };
            assert_eq!(t, &vec![t0]);
            assert_eq!(c.ranges.len(), 2);
            points.insert((x.to_bits(), y.to_bits()));
        }
        assert_eq!(points.len(), 20_000, "every station, each once");
    }

    /// The same polygon over a week of two reports with every parameter is
    /// 880 000 values: the 400 names the value budget, not a station count.
    #[test]
    fn area_over_the_value_budget_is_a_400_naming_it() {
        let t0 = Utc.with_ymd_and_hms(2026, 9, 12, 6, 0, 0).unwrap();
        let t1 = Utc.with_ymd_and_hms(2026, 9, 12, 7, 0, 0).unwrap();
        let (e, _dir) = network(&[t0, t1]);
        assert!(e.table.params.len() * 2 * 20_000 > MAX_RESPONSE_VALUES);
        let week = (t1 - chrono::Duration::days(7), t1);
        match e.query_area(NETWORK_AREA, Some(week), None, None, None) {
            Err(DataServerError::QueryTooLarge(msg)) => {
                assert!(msg.contains("500000 values"), "message: {msg}");
            }
            other => panic!("expected the value budget's QueryTooLarge, got {other:?}"),
        }
    }

    // ---- state snapshots and warm-up (#1002) -----------------------------

    use crate::decode::{Element, XY};
    use crate::wis2::WarmupClock;
    use ds_core::config::Wis2Config;
    use ds_core::edr_engine::EdrEngine;
    use ds_core::state::{FileStateStore, StateStore};

    const ID: &str = "obs-wis2";

    fn wis2_config(retention: &str, warmup: Option<&str>) -> BufrConfig {
        BufrConfig {
            data_path: None,
            wis2: Some(Wis2Config {
                topics: vec![
                    "cache/a/wis2/se-smhi/data/core/weather/surface-based-observations/synop"
                        .into(),
                ],
                warmup: warmup.map(String::from),
                ..Wis2Config::default()
            }),
            poll_interval_secs: 60,
            retention: retention.into(),
            max_stations: 100,
            stale_after: "PT2H".into(),
            position_radius_km: 25.0,
            builtin_parameters: true,
            parameters: Vec::new(),
            map: None,
        }
    }

    fn file_store(dir: &std::path::Path) -> Option<Arc<dyn StateStore>> {
        Some(Arc::new(FileStateStore::new(dir)))
    }

    fn wis2(e: &BufrEngine) -> &Wis2Source {
        match e.source() {
            Source::Wis2(w) => w,
            Source::Local(_) => panic!("not a WIS2 engine"),
        }
    }

    /// `hours` before the top of the hour (fixed per test run, so a test
    /// crossing an hour boundary still sees one timeline).
    fn hours_ago(hours: i64) -> DateTime<Utc> {
        static TOP: std::sync::LazyLock<DateTime<Utc>> = std::sync::LazyLock::new(|| {
            let now = Utc::now().timestamp();
            DateTime::from_timestamp(now - now.rem_euclid(3600), 0).unwrap()
        });
        *TOP - chrono::Duration::hours(hours)
    }

    /// Ingest one report (air temperature `kelvin`) as a payload would,
    /// and publish it, as the poll loop's snapshot tick would.
    fn put(e: &BufrEngine, station: &str, time: DateTime<Utc>, kelvin: f64) {
        let report = crate::decode::ObsReport {
            elements: vec![Element {
                xy: XY { x: 12, y: 101 },
                value: kelvin,
                period_hours: None,
            }],
            name: Some(format!("Station {station}")),
            ..report(station, time)
        };
        let outcome = e
            .store
            .write()
            .unwrap()
            .ingest(&report, &e.table, Utc::now());
        assert_ne!(outcome, Ingest::OutOfWindow);
        e.dirty.store(true, Ordering::Release);
        e.snapshot_if_dirty();
    }

    /// `(station, time, air temperature bits)` of every row held.
    fn rows(e: &BufrEngine) -> Vec<(String, DateTime<Utc>, u32)> {
        let store = e.store.read().unwrap();
        let mut out: Vec<_> = store
            .stations()
            .flat_map(|s| {
                s.rows
                    .iter()
                    .map(|(t, r)| (s.info.id.to_string(), *t, r[0].to_bits()))
            })
            .collect();
        out.sort();
        out
    }

    fn snapshot_file(dir: &std::path::Path) -> std::path::PathBuf {
        dir.join(format!("{ID}.bufr.state"))
    }

    fn decode_file(dir: &std::path::Path, e: &BufrEngine) -> persist::Decoded {
        let bytes = std::fs::read(snapshot_file(dir)).unwrap();
        persist::decode(&bytes, ID, &e.table).unwrap()
    }

    #[test]
    fn shutdown_flushes_the_store_and_the_next_build_restores_it() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = wis2_config("PT24H", None);
        let a = BufrEngine::new_with_state(&cfg, ID, file_store(dir.path())).unwrap();
        assert_eq!(a.gauges(), (0, 0), "nothing to restore: a cold start");
        put(&a, "A", hours_ago(2), 281.35);
        put(&a, "A", hours_ago(1), 282.15);
        put(&a, "B", hours_ago(0), 270.0);
        let filling_since = hours_ago(30);
        wis2(&a).mark_filling(filling_since);
        assert!(!snapshot_file(dir.path()).exists());
        a.shutdown();
        assert!(snapshot_file(dir.path()).is_file());

        let b = BufrEngine::new_with_state(&cfg, ID, file_store(dir.path())).unwrap();
        assert_eq!(b.gauges(), (2, 3), "served at once, before any broker");
        assert_eq!(rows(&b), rows(&a));
        assert_eq!(b.latest_report(), Some(hours_ago(0)));
        let ids: Vec<String> = b
            .get_locations()
            .unwrap()
            .iter()
            .map(|l| l.id.clone())
            .collect();
        assert_eq!(ids, vec!["A", "B"]);
        assert_eq!(
            wis2(&b).warmup_clock(),
            WarmupClock {
                filling_since: Some(filling_since),
                cause: WarmupCause::ColdStart,
            },
            "the warm-up clock travels with the store"
        );
        // The state store holds what was restored: nothing to write until
        // the store changes (or the refresh interval runs out).
        let writer = b.state.as_ref().unwrap().lock().unwrap();
        assert!(!writer.is_due(b.state_revision(), Instant::now(), false));
        drop(writer);
        put(&b, "C", hours_ago(0), 280.0);
        let writer = b.state.as_ref().unwrap().lock().unwrap();
        assert!(writer.is_due(b.state_revision(), Instant::now(), false));
    }

    /// #1036: a reload that rebuilds the collection builds the new engine
    /// while the old one still receives, so the new one restores only the
    /// last periodic snapshot. The old engine's shutdown ends its ingest,
    /// and the new one takes over the rest in memory, under its own
    /// parameter table: nothing that arrived after that write is lost, and
    /// the new engine's next write, which replaces the old one's shutdown
    /// flush, holds it.
    #[test]
    fn a_rebuild_takes_over_what_arrived_after_the_last_periodic_write() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = wis2_config("P36500D", None);
        let old = BufrEngine::new_with_state(&cfg, ID, file_store(dir.path())).unwrap();
        wis2(&old).mark_filling(hours_ago(30));
        put(&old, "A", hours_ago(1), 281.35);
        // The periodic write, then more reports.
        old.write_state(false);
        put(&old, "A", hours_ago(0), 282.15);
        put(&old, "B", hours_ago(0), 270.0);

        // The reload builds the successor first, with a column added.
        let changed = BufrConfig {
            parameters: vec![ds_core::config::BufrParameterConfig {
                name: "new_column".into(),
                descriptors: vec!["012102".into()],
                unit: "K".into(),
                label: None,
                observed_property: None,
                period_hours: None,
            }],
            ..cfg.clone()
        };
        let new = BufrEngine::new_with_state(&changed, ID, file_store(dir.path())).unwrap();
        assert_eq!(new.gauges(), (1, 1), "the periodic snapshot only");

        // Accepted: the old engine is shut down, then the new one takes over.
        old.shutdown();
        new.take_over(&old);
        assert_eq!(new.gauges(), (2, 3));
        assert_eq!(rows(&new), rows(&old));
        assert!(
            new.data_version() > old.data_version(),
            "no version the old engine handed out names the new content"
        );
        assert_eq!(
            wis2(&new).warmup_clock(),
            WarmupClock {
                filling_since: Some(hours_ago(30)),
                cause: WarmupCause::ColdStart,
            }
        );

        // The old engine takes nothing more after its shutdown.
        let fixture = std::fs::read(format!(
            "{}/synop_se-smhi_20260912T0800Z.bufr",
            fixtures_dir()
        ))
        .unwrap();
        assert_eq!(old.ingest_bytes(&fixture, "late", Utc::now()), 0);
        assert_eq!(old.gauges(), (2, 3));

        // The new engine's first report moves it; its write holds it all.
        assert!(new.ingest_bytes(&fixture, "next", Utc::now()) > 0);
        new.snapshot_if_dirty();
        new.write_state(false);
        let restored = BufrEngine::new_with_state(&changed, ID, file_store(dir.path())).unwrap();
        assert_eq!(restored.gauges(), new.gauges());
        assert_eq!(rows(&restored), rows(&new));
    }

    #[test]
    fn restore_prunes_what_the_current_retention_and_max_stations_reject() {
        let dir = tempfile::tempdir().unwrap();
        let a = BufrEngine::new_with_state(&wis2_config("P2D", None), ID, file_store(dir.path()))
            .unwrap();
        put(&a, "A", hours_ago(40), 280.0);
        put(&a, "A", hours_ago(30), 281.0);
        put(&a, "A", hours_ago(1), 282.0);
        put(&a, "OLD", hours_ago(30), 283.0);
        put(&a, "NEW", hours_ago(0), 284.0);
        a.shutdown();

        // Retention shortened to a day since the snapshot: rows past it go,
        // and a station left with none goes too.
        let b = BufrEngine::new_with_state(&wis2_config("PT24H", None), ID, file_store(dir.path()))
            .unwrap();
        assert_eq!(b.gauges(), (2, 2));
        let held: Vec<_> = rows(&b).into_iter().map(|(s, t, _)| (s, t)).collect();
        assert_eq!(
            held,
            vec![("A".into(), hours_ago(1)), ("NEW".into(), hours_ago(0))]
        );
        let store = b.store.read().unwrap();
        assert_eq!(store.get("A").unwrap().info.first_report, hours_ago(1));
        drop(store);

        // max_stations as well: the most recently seen station stays.
        let c = BufrEngine::new_with_state(
            &BufrConfig {
                max_stations: 1,
                ..wis2_config("P2D", None)
            },
            ID,
            file_store(dir.path()),
        )
        .unwrap();
        assert_eq!(c.gauges(), (1, 1));
        assert!(c.store.read().unwrap().get("NEW").is_some());
    }

    #[test]
    fn corrupt_snapshot_is_a_cold_start_that_the_next_write_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = wis2_config("PT24H", None);
        std::fs::write(snapshot_file(dir.path()), b"\x1f\x8bnot really gzip").unwrap();
        let e = BufrEngine::new_with_state(&cfg, ID, file_store(dir.path())).unwrap();
        assert_eq!(e.gauges(), (0, 0));
        assert_eq!(wis2(&e).warmup_clock(), WarmupClock::default());
        put(&e, "A", hours_ago(0), 280.0);
        // The first tick writes (nothing was restored), replacing the file.
        e.write_state(false);
        assert_eq!(decode_file(dir.path(), &e).stations.len(), 1);

        // Another collection's snapshot under this key is rejected as well.
        let other = BufrEngine::new_with_state(&cfg, "other", None).unwrap();
        put(&other, "Z", hours_ago(0), 280.0);
        let bytes = persist::encode(
            "other",
            &other.store.read().unwrap(),
            &other.table,
            WarmupClock::default(),
            Utc::now(),
        )
        .unwrap();
        std::fs::write(snapshot_file(dir.path()), bytes).unwrap();
        let f = BufrEngine::new_with_state(&cfg, ID, file_store(dir.path())).unwrap();
        assert_eq!(f.gauges(), (0, 0));
    }

    /// Every write is attempted and fails, the first one WARNs, and the
    /// engine serves on. The outcomes are checked, so a write that was
    /// never attempted fails the test (#1043).
    #[test]
    fn unwritable_state_dir_never_fails_the_engine() {
        let dir = tempfile::tempdir().unwrap();
        // A regular file where the state directory should be.
        let blocker = dir.path().join("state");
        std::fs::write(&blocker, b"").unwrap();
        let e = BufrEngine::new_with_state(&wis2_config("PT24H", None), ID, file_store(&blocker))
            .unwrap();
        put(&e, "A", hours_ago(0), 280.0);
        // Nothing was written yet: the periodic write is due, and fails.
        assert!(matches!(
            e.write_state(false),
            Some(WriteOutcome::Failed {
                error: StateError::Io(_),
                warn: true
            })
        ));
        // The retry waits for the write interval; the shutdown flush does
        // not, and fails without a second warning.
        assert!(matches!(e.write_state(false), Some(WriteOutcome::NotDue)));
        assert!(matches!(
            e.write_state(true),
            Some(WriteOutcome::Failed {
                error: StateError::Io(_),
                warn: false
            })
        ));
        e.shutdown();
        assert_eq!(e.gauges(), (1, 1));
        assert!(blocker.is_file());
    }

    #[test]
    fn data_path_collections_ignore_the_state_store() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = BufrConfig {
            data_path: Some(fixtures_dir()),
            wis2: None,
            retention: "P36500D".into(),
            ..wis2_config("PT24H", None)
        };
        let e = BufrEngine::new_with_state(&cfg, ID, file_store(dir.path())).unwrap();
        assert!(e.gauges().1 > 0, "fixtures scanned");
        assert!(e.state.is_none());
        assert!(e.write_state(true).is_none());
        e.shutdown();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    fn warming(received: u64, cause: WarmupCause) -> Option<LiveStatus> {
        Some(LiveStatus::WarmingUp {
            received,
            items: "reports",
            cause,
        })
    }

    /// A broker session that is up and delivering, and a decoded report.
    fn connected(e: &BufrEngine) -> Arc<ds_wis2::Status> {
        let status = Arc::new(ds_wis2::Status::new());
        status.set_connected();
        status.set_subscribed();
        status.record_accepted(0);
        wis2(e).set_status(status.clone());
        e.health.mark_probed();
        status
    }

    /// The default `warmup` is the shorter of `retention` and a day; an
    /// explicit one keeps its value, whatever the retention.
    #[test]
    fn warmup_defaults_to_the_shorter_of_retention_and_a_day() {
        let warmup = |retention: &str, warmup: Option<&str>| {
            let e = BufrEngine::new(&wis2_config(retention, warmup), ID).unwrap();
            wis2(&e).warmup()
        };
        let hours = chrono::Duration::hours;
        // A retention longer than a day: a day.
        assert_eq!(warmup("P7D", None), hours(24));
        assert_eq!(warmup("PT36H", None), hours(24));
        // A day or shorter: the retention window itself.
        assert_eq!(warmup("PT24H", None), hours(24));
        assert_eq!(warmup("PT6H", None), hours(6));
        // Explicit: kept, shorter or longer than the default.
        assert_eq!(warmup("P7D", Some("PT2H")), hours(2));
        assert_eq!(warmup("P7D", Some("P7D")), hours(7 * 24));
        assert_eq!(warmup("PT6H", Some("PT12H")), hours(12));
    }

    /// The default drives health: with a 7-day retention a cold start is
    /// `Ready` a day after the subscription came up, not a week later, and
    /// a snapshot written more than a day ago restarts the warm-up.
    #[test]
    fn a_week_of_retention_warms_up_for_a_day_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = wis2_config("P7D", None);
        let a = BufrEngine::new(&cfg, ID).unwrap();
        connected(&a);
        put(&a, "A", hours_ago(30), 280.0);
        let t0 = Utc::now();
        wis2(&a).mark_filling(t0);
        let later = |h| t0 + chrono::Duration::hours(h);
        assert_eq!(
            a.live_health_at(later(23)),
            warming(1, WarmupCause::ColdStart)
        );
        assert_eq!(a.live_health_at(later(25)), Some(LiveStatus::Ready));

        let warm_since = Utc::now() - chrono::Duration::hours(48);
        let restore = |written_hours_ago: i64| {
            let bytes = persist::encode(
                ID,
                &a.store.read().unwrap(),
                &a.table,
                WarmupClock {
                    filling_since: Some(warm_since),
                    cause: WarmupCause::ColdStart,
                },
                Utc::now() - chrono::Duration::hours(written_hours_ago),
            )
            .unwrap();
            FileStateStore::new(dir.path())
                .save(&format!("{ID}.bufr"), &bytes)
                .unwrap();
            let e = BufrEngine::new_with_state(&cfg, ID, file_store(dir.path())).unwrap();
            assert_eq!(e.gauges(), (1, 1), "rows inside retention are kept");
            wis2(&e).warmup_clock()
        };
        // Down for 20 hours: the clock is kept.
        assert_eq!(restore(20).filling_since, Some(warm_since));
        // Down for 30 hours: longer than the warm-up, which restarts.
        assert_eq!(
            restore(30),
            WarmupClock {
                filling_since: None,
                cause: WarmupCause::LongOutage,
            }
        );
    }

    #[test]
    fn cold_start_reports_warming_up_until_the_warmup_after_subscription() {
        // No state store: every start is cold.
        let e = BufrEngine::new(&wis2_config("PT24H", Some("PT1H")), ID).unwrap();
        let status = connected(&e);
        put(&e, "A", hours_ago(0), 280.0);
        let t0 = Utc::now();
        // Subscribed but the clock not started yet (the next tick does).
        assert_eq!(e.live_health_at(t0), warming(1, WarmupCause::ColdStart));
        assert_eq!(
            e.live_health_at(t0).unwrap().degraded_reason().as_deref(),
            Some("warming up after cold start: 1 reports received")
        );
        wis2(&e).mark_filling(t0);
        let minutes = |m| t0 + chrono::Duration::minutes(m);
        assert_eq!(
            e.live_health_at(minutes(59)),
            warming(1, WarmupCause::ColdStart)
        );
        // A broker blip inside degrade_after_secs does not end the warm-up.
        status.set_disconnected();
        assert_eq!(
            e.live_health_at(minutes(59)),
            warming(1, WarmupCause::ColdStart)
        );
        assert_eq!(e.live_health_at(minutes(61)), Some(LiveStatus::Ready));
        status.set_connected();
        status.set_subscribed();
        assert_eq!(e.live_health_at(minutes(61)), Some(LiveStatus::Ready));
        // A reconnect does not restart the clock.
        wis2(&e).mark_filling(minutes(70));
        assert_eq!(wis2(&e).warmup_clock().filling_since, Some(t0));
    }

    /// The server was down longer than `warmup`: the restored reports are
    /// served, but the warm-up restarts — what the feed published meanwhile
    /// is missing for good — and that is written back at once.
    #[test]
    fn a_snapshot_older_than_the_warmup_keeps_its_rows_but_restarts_the_warm_up() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = wis2_config("PT24H", Some("PT1H"));
        let a = BufrEngine::new(&cfg, ID).unwrap();
        put(&a, "A", hours_ago(3), 280.0);
        put(&a, "A", hours_ago(2), 281.0);
        let warm_since = Utc::now() - chrono::Duration::hours(30);
        let write = |written_at: DateTime<Utc>| {
            let bytes = persist::encode(
                ID,
                &a.store.read().unwrap(),
                &a.table,
                WarmupClock {
                    filling_since: Some(warm_since),
                    cause: WarmupCause::ColdStart,
                },
                written_at,
            )
            .unwrap();
            FileStateStore::new(dir.path())
                .save(&format!("{ID}.bufr"), &bytes)
                .unwrap();
        };

        // Last written 50 minutes ago: inside the warm-up, the clock is
        // kept, so it is ready at once.
        write(Utc::now() - chrono::Duration::minutes(50));
        let fresh = BufrEngine::new_with_state(&cfg, ID, file_store(dir.path())).unwrap();
        connected(&fresh);
        assert_eq!(fresh.live_health(), Some(LiveStatus::Ready));
        assert_eq!(wis2(&fresh).warmup_clock().filling_since, Some(warm_since));

        // Last written two hours ago: rows kept, warm-up restarted.
        write(Utc::now() - chrono::Duration::hours(2));
        let b = BufrEngine::new_with_state(&cfg, ID, file_store(dir.path())).unwrap();
        assert_eq!(b.gauges(), (1, 2));
        assert_eq!(
            wis2(&b).warmup_clock(),
            WarmupClock {
                filling_since: None,
                cause: WarmupCause::LongOutage,
            }
        );
        connected(&b);
        assert_eq!(b.live_health(), warming(2, WarmupCause::LongOutage));
        assert_eq!(
            b.live_health().unwrap().degraded_reason().as_deref(),
            Some("warming up after a long outage: 2 reports received")
        );
        // Written at the first tick, so a restart during the restarted
        // warm-up keeps warming with the same wording.
        b.write_state(false);
        assert_eq!(
            decode_file(dir.path(), &b).clock.cause,
            WarmupCause::LongOutage
        );
        let c = BufrEngine::new_with_state(&cfg, ID, file_store(dir.path())).unwrap();
        connected(&c);
        assert_eq!(c.live_health(), warming(2, WarmupCause::LongOutage));
    }

    #[test]
    fn a_started_clock_is_written_before_any_report_arrives() {
        let dir = tempfile::tempdir().unwrap();
        let e = BufrEngine::new_with_state(&wis2_config("PT24H", None), ID, file_store(dir.path()))
            .unwrap();
        e.write_state(false);
        assert_eq!(decode_file(dir.path(), &e).clock.filling_since, None);
        let revision = e.state_revision();
        let t0 = hours_ago(0);
        wis2(&e).mark_filling(t0);
        assert_ne!(e.state_revision(), revision);
        e.write_state(true);
        assert_eq!(decode_file(dir.path(), &e).clock.filling_since, Some(t0));
    }
}
