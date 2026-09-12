//! `BufrEngine`: `EdrEngine` (locations / position / area / radius) +
//! `FeatureEngine` (station = Point feature) over the in-memory store.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use ds_core::config::BufrConfig;
use ds_core::datetime::parse_iso8601_duration;
use ds_core::error::DataServerError;
use ds_core::feature::{
    check_mask_budget, parse_area_coords, parse_point_coords, sort_features, Feature, FeaturePage,
    FeatureQuery,
};
use ds_core::feature_engine::FeatureEngine;
use ds_core::geo::great_circle_distance_m;
use ds_core::health::LiveStatus;
use ds_core::model::{
    CoverageResponse, DomainDescription, Location, NdArray, ParameterDescription, QueryResult,
};
use ds_poll::{FirstTick, Shutdown};

use crate::decode::{DecodeError, Decoder};
use crate::health::Health;
use crate::metadata::Snapshot;
use crate::params::ParameterTable;
use crate::source::LocalSource;
use crate::store::{Ingest, ObsStore};

/// Stations an `area` query may touch (the postgis convention).
pub const MAX_STATIONS_IN_POLYGON: usize = 10_001;
/// Values (stations × parameters × timesteps) one response may carry.
pub const MAX_RESPONSE_VALUES: usize = 500_000;
/// How often the metadata snapshot is rebuilt when the store changed.
const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(10);
/// How often expired rows / surplus stations are pruned.
const PRUNE_INTERVAL: Duration = Duration::from_secs(60);

enum Source {
    Local(Mutex<LocalSource>),
}

pub struct BufrEngine {
    collection_id: String,
    table: Arc<ParameterTable>,
    decoder: Decoder,
    store: RwLock<ObsStore>,
    snapshot: ArcSwap<Snapshot>,
    dirty: AtomicBool,
    version: AtomicU64,
    source: Source,
    poll_interval: Duration,
    position_radius_m: f64,
    shutdown: Shutdown,
    pub health: Health,
}

impl BufrEngine {
    /// Build the engine and, in `data_path` mode, do a best-effort initial
    /// scan so local fixtures serve immediately (a failing source starts
    /// `Degraded`; the poll loop retries).
    pub fn new(config: &BufrConfig, collection_id: &str) -> Result<Self, DataServerError> {
        let retention = parse_iso8601_duration(&config.retention)?;
        let table = Arc::new(ParameterTable::build(
            config.builtin_parameters,
            &config.parameters,
        ));
        let source = match (&config.data_path, &config.wis2) {
            (Some(path), None) => Source::Local(Mutex::new(LocalSource::new(path)?)),
            (None, Some(_)) => {
                return Err(DataServerError::Config(format!(
                    "Collection '{collection_id}': [bufr.wis2] is not supported yet — use data_path"
                )))
            }
            _ => {
                return Err(DataServerError::Config(format!(
                    "Collection '{collection_id}': bufr requires exactly one of data_path or [bufr.wis2]"
                )))
            }
        };
        let engine = BufrEngine {
            collection_id: collection_id.to_string(),
            table,
            decoder: Decoder::new(),
            store: RwLock::new(ObsStore::new(retention, config.max_stations)),
            snapshot: ArcSwap::from_pointee(Snapshot::empty()),
            dirty: AtomicBool::new(false),
            version: AtomicU64::new(0),
            source,
            poll_interval: Duration::from_secs(config.poll_interval_secs.max(1)),
            position_radius_m: config.position_radius_km * 1000.0,
            shutdown: Shutdown::new(),
            health: Health::new(),
        };
        engine.scan_once();
        engine.rebuild_snapshot();
        Ok(engine)
    }

    pub fn collection_id(&self) -> &str {
        &self.collection_id
    }

    /// Runtime health: `None` until the source has been probed once (boot
    /// snapshot stands), then Ready / Degraded from the source's state.
    pub fn live_health(&self) -> Option<LiveStatus> {
        match &self.source {
            Source::Local(_) => self.health.local_status(),
        }
    }

    /// Whether the first scan succeeded (boot readiness).
    pub fn is_loaded(&self) -> bool {
        self.health.is_probed()
            && self.health.scan_failures_total.load(Ordering::Relaxed)
                < self.health.scans_total.load(Ordering::Relaxed).max(1)
    }

    /// `(stations, reports)` currently held.
    pub fn gauges(&self) -> (usize, usize) {
        let s = self.snapshot.load();
        (s.stations.len(), s.report_count)
    }

    /// Newest report time across stations (for `/health` data age).
    pub fn latest_report(&self) -> Option<DateTime<Utc>> {
        self.snapshot.load().temporal_extent.map(|(_, b)| b)
    }

    /// One scan of the local source: fetch new files, decode, ingest. Files
    /// are ingested as each fetch chunk lands, so a backlog is never held in
    /// memory whole.
    fn scan_once(&self) {
        let Source::Local(src) = &self.source;
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
        let decoded = match self.decoder.decode(bytes) {
            Ok(d) => d,
            Err(e) => {
                // No BUFR magic at all — the whole object is not a message.
                self.health
                    .decode_failures_total
                    .fetch_add(1, Ordering::Relaxed);
                tracing::debug!("[{}] bufr: '{label}' failed: {e}", self.collection_id);
                return 0;
            }
        };
        // Per-message failures: the other messages of a concatenated file
        // still ingest below.
        for e in &decoded.failed {
            match e {
                DecodeError::Unsupported(m) => {
                    self.health
                        .decode_unsupported_total
                        .fetch_add(1, Ordering::Relaxed);
                    tracing::debug!("[{}] bufr: '{label}' unsupported: {m}", self.collection_id);
                }
                e => {
                    self.health
                        .decode_failures_total
                        .fetch_add(1, Ordering::Relaxed);
                    tracing::debug!("[{}] bufr: '{label}' failed: {e}", self.collection_id);
                }
            }
        }
        self.health
            .subsets_skipped_total
            .fetch_add(decoded.skipped.len() as u64, Ordering::Relaxed);
        let mut n = 0usize;
        {
            let mut store = self.store.write().unwrap_or_else(|e| e.into_inner());
            for r in &decoded.reports {
                match store.ingest(r, &self.table, now) {
                    Ingest::Inserted => {
                        self.health
                            .reports_ingested_total
                            .fetch_add(1, Ordering::Relaxed);
                        n += 1;
                    }
                    Ingest::Replaced => {
                        self.health
                            .reports_replaced_total
                            .fetch_add(1, Ordering::Relaxed);
                        n += 1;
                    }
                    Ingest::OutOfWindow => {
                        self.health
                            .reports_out_of_window_total
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        if n > 0 {
            self.dirty.store(true, Ordering::Release);
        }
        n
    }

    fn rebuild_snapshot(&self) {
        let version = self.version.fetch_add(1, Ordering::Relaxed) + 1;
        let snap = {
            let store = self.store.read().unwrap_or_else(|e| e.into_inner());
            Snapshot::build(&store, version)
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

    /// Background loop (poll runtime): scan the source, prune, and rebuild
    /// the metadata snapshot when the store changed. Exits on `shutdown()`.
    pub async fn poll_loop(&self) {
        let mut scan = self.shutdown.ticker(self.poll_interval, FirstTick::Skip);
        let mut snap = self.shutdown.ticker(SNAPSHOT_INTERVAL, FirstTick::Skip);
        let mut prune = self.shutdown.ticker(PRUNE_INTERVAL, FirstTick::Skip);
        loop {
            tokio::select! {
                biased;
                _ = self.shutdown.wait() => break,
                _ = scan.tick() => self.scan_once(),
                _ = prune.tick() => self.prune(),
                _ = snap.tick() => {
                    if self.dirty.swap(false, Ordering::AcqRel) {
                        self.rebuild_snapshot();
                    }
                }
            }
        }
        tracing::info!("[{}] bufr: poll loop shutting down", self.collection_id);
    }

    pub fn shutdown(&self) {
        self.shutdown.shutdown();
    }

    // ---- query helpers -------------------------------------------------

    fn selected_params(&self, parameters: Option<&[String]>) -> Vec<usize> {
        match parameters {
            Some(req) => req.iter().filter_map(|p| self.table.index_of(p)).collect(),
            None => (0..self.table.len()).collect(),
        }
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
                },
            );
            descs.insert(
                p.name.clone(),
                ParameterDescription {
                    label: p.label.clone(),
                    unit: p.unit.clone(),
                    observed_property: p.observed_property.clone(),
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

/// Rows are `f32` (BUFR values carry at most ~7 significant digits); widen
/// back to the decimal the producer encoded rather than the binary
/// expansion (`290.12`, not `290.1199951171875`).
fn round_stored(v: f32) -> f64 {
    ((v as f64) * 1e5).round() / 1e5
}

// ---------------------------------------------------------------------------
// EdrEngine
// ---------------------------------------------------------------------------

impl ds_core::edr_engine::EdrEngine for BufrEngine {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        Ok(self.snapshot.load().locations.as_ref().clone())
    }

    fn query_location(
        &self,
        location_id: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        let params = self.selected_params(parameters);
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
        if inside.len() > MAX_STATIONS_IN_POLYGON {
            return Err(DataServerError::QueryTooLarge(format!(
                "{} stations inside the polygon (max {MAX_STATIONS_IN_POLYGON}) — narrow the area",
                inside.len()
            )));
        }
        let params = self.selected_params(parameters);
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

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        self.snapshot.load().spatial_extent
    }

    fn supported_query_types(&self) -> Vec<String> {
        ["locations", "position", "area", "radius"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }
}

// ---------------------------------------------------------------------------
// FeatureEngine
// ---------------------------------------------------------------------------

const SORTABLES: &[&str] = &["last_report", "first_report", "report_count", "name"];

impl FeatureEngine for BufrEngine {
    fn get_features(&self, query: &FeatureQuery) -> Result<FeaturePage, DataServerError> {
        let snap = self.snapshot.load();
        let mut features: Vec<Feature> = snap
            .features
            .iter()
            .zip(snap.stations.iter())
            .filter(|(_, s)| match &query.bbox {
                Some(b) => b.contains(s.lon, s.lat),
                None => true,
            })
            .filter(|(_, s)| match &query.datetime {
                // A station matches when it has at least one report inside
                // the interval.
                Some(dt) => {
                    dt.start.is_none_or(|start| s.last_report >= start)
                        && dt.end.is_none_or(|end| s.first_report <= end)
                }
                None => true,
            })
            .map(|(f, _)| f.clone())
            .collect();
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
