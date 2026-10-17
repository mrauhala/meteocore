//! `PostgisEngine` — implements the `ds_core::edr_engine::EdrEngine` trait on top
//! of a [`deadpool_postgres`] pool and a [`MetadataCache`].
//!
//! DB work is async; the trait is sync. The bridge is
//! `tokio::task::block_in_place(|| Handle::current().block_on(..))` — safe
//! from axum handlers because axum runs on a multi-thread runtime. The
//! engine never calls `block_on` from an async context without first
//! entering `block_in_place`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::{StreamExt, TryStreamExt};

use chrono::{DateTime, Utc};
use deadpool_postgres::Pool;
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::feature::{check_mask_budget, parse_area_coords, QueryPolygon};
use ds_core::model::{
    CoverageResponse, DomainDescription, Location, LocationSnapshot, NdArray, ParameterDescription,
    QueryResult,
};
use ds_poll::Shutdown;
use tokio_postgres::Row;

use crate::config::PostgisEngineConfig;
use crate::health::{Health, HealthSnapshot, HealthStatus, LiveHealth};
use crate::metadata::{CollectionMeta, LocationsCut, MetadataCache};
use crate::query::{
    build_area_set, build_events_area, build_events_window, build_location, build_position,
    build_stations_in_polygon, first_observation_column, params_as_refs, BuiltQuery, WindowAttrs,
    DEFAULT_POSITION_RADIUS_M, MAX_AREA_QUERIES, MAX_OBSERVATION_ROWS, MAX_RESPONSE_VALUES,
    MAX_STATIONS_IN_POLYGON, SET_BASED_MAX_WINDOW,
};
use crate::schema::{EventsShape, ObservationSchema};

/// Time-ordered observation values for a single parameter. Exists to
/// keep the Clippy `type_complexity` lint happy without papering over
/// the intent.
type ParamSeries = Vec<(DateTime<Utc>, Option<f64>)>;

/// How an area query reads its stations' observations (#1062). Not part of
/// the engine contract: [`PostgisEngine::query_area_on`] takes it so the
/// database tests can run both paths on one dataset.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AreaPath {
    /// One statement per observation table (`long` and `wide` one,
    /// `per_parameter` one per requested parameter) for every station in the
    /// polygon at once, rows ordered by station and time and charged against
    /// [`MAX_RESPONSE_VALUES`] as they stream in. For short windows: a single
    /// instant over a whole network is one statement, not one per station.
    SetBased,
    /// One statement per station (per station and parameter for
    /// `per_parameter`), at most 16 in flight, each capped at
    /// [`MAX_OBSERVATION_ROWS`] rows and the whole fan-out at
    /// [`MAX_AREA_QUERIES`] statements. For long windows, where each
    /// station's `(station, time)` index range scan is the cheap unit.
    FanOut,
}

/// `SELECT 1` health-ping cadence — the `/health` reachability probe (#110).
const PING_INTERVAL: Duration = Duration::from_secs(30);
/// Per-ping deadline; a slower response counts as unreachable.
const PING_TIMEOUT: Duration = Duration::from_secs(2);

const SCHEMA_PENDING: u8 = 0;
const SCHEMA_VALID: u8 = 1;
const SCHEMA_INVALID: u8 = 2;

/// Collection-scoped engine. Cheap to clone (Arcs inside); axum handlers
/// hold it behind `Arc<dyn EdrEngine>` as is done for every engine.
pub struct PostgisEngine {
    collection_id: String,
    config: Arc<PostgisEngineConfig>,
    pool: Arc<Pool>,
    cache: Arc<MetadataCache>,
    /// DB reachability and metrics counters, independent of schema validity.
    health: Health,
    schema_state: AtomicU8,
    refresh_lock: tokio::sync::Mutex<()>,
    privileges: crate::privileges::PrivilegeAudit,
    /// `<user>@<host>:<port>/<db>` (or `pool_label`) — the `/metrics` pool label.
    pool_key_label: String,
    /// Edge-triggered stop signal for `poll_loop` (shared lifecycle, #481);
    /// a `shutdown()` that fires before the spawned loop began (rapid-reload
    /// race) is still observed.
    shutdown: Shutdown,
}

impl std::fmt::Debug for PostgisEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgisEngine")
            .field("collection_id", &self.collection_id)
            .field("pool_max_size", &self.pool.status().max_size)
            .field("cache_version", &self.cache.load().version)
            .finish()
    }
}

impl PostgisEngine {
    pub fn new(
        collection_id: impl Into<String>,
        config: Arc<PostgisEngineConfig>,
        pool: Arc<Pool>,
    ) -> Self {
        let collection_id = collection_id.into();
        // Pool label for /metrics: explicit `pool_label`, else the derived
        // `<user>@<host>:<port>/<db>` pool key (password never included).
        let pool_key_label = config.pool_label.clone().unwrap_or_else(|| {
            crate::pool::normalize_dsn(&config.dsn)
                .map(|(_, key, _)| key.to_string())
                .unwrap_or_else(|_| "unknown".to_string())
        });
        // Seeded with what the config alone determines, so the parameter
        // descriptions (#1072) and a property filter on a configured station
        // column (#1024) are served before the first refresh lands.
        let cache = Arc::new(MetadataCache::new(&config));
        let privileges = crate::privileges::PrivilegeAudit::new(&config, &pool);
        Self {
            collection_id,
            config,
            pool,
            cache,
            health: Health::new(),
            schema_state: AtomicU8::new(SCHEMA_PENDING),
            refresh_lock: tokio::sync::Mutex::new(()),
            privileges,
            pool_key_label,
            shutdown: Shutdown::new(),
        }
    }

    pub fn collection_id(&self) -> &str {
        &self.collection_id
    }

    pub fn pool(&self) -> &Arc<Pool> {
        &self.pool
    }

    pub fn config(&self) -> &PostgisEngineConfig {
        &self.config
    }

    pub fn cache(&self) -> &MetadataCache {
        &self.cache
    }

    /// O(1) live health: schema validity, then reachability and inventory caps.
    /// A successful ping cannot clear a pending or invalid schema.
    pub fn live_health(&self) -> Option<LiveHealth> {
        match self.schema_state.load(Ordering::Acquire) {
            SCHEMA_INVALID => return Some(LiveHealth::SchemaInvalid),
            SCHEMA_PENDING => return Some(LiveHealth::ValidationPending),
            _ => {}
        }
        LiveHealth::from_parts(
            self.health.live_status(),
            self.locations_cut(),
            self.config.max_locations,
        )
    }

    /// Which read `max_locations` cut in the cached snapshot, if any (#1061).
    pub fn locations_cut(&self) -> Option<LocationsCut> {
        self.cache.load().locations_cut
    }

    /// Whether `max_locations` cut the cached snapshot (#1061): the
    /// `postgis_locations_truncated` gauge.
    pub fn locations_truncated(&self) -> bool {
        self.locations_cut().is_some()
    }

    /// Health + metrics-counter snapshot for the `/metrics` scrape.
    pub fn health_snapshot(&self) -> HealthSnapshot {
        self.health.snapshot()
    }

    /// `<user>@<host>:<port>/<db>` (or `pool_label`) — the `/metrics` pool label.
    pub fn pool_key_label(&self) -> &str {
        &self.pool_key_label
    }

    /// Run a one-shot metadata refresh. Used at construction and by the
    /// `/admin/collections/reload` path. Validates the schema before publishing
    /// metadata; transient failures retain the previous snapshot, while a
    /// confirmed mismatch blocks data access and degrades health.
    pub async fn refresh_metadata(&self) -> Result<(), DataServerError> {
        let start = Instant::now();
        let was_cut = self.locations_cut();
        let _refresh = self.refresh_lock.lock().await;
        let result = self.refresh_metadata_checked().await;
        // Advisory and independent of schema/metadata success. Completed audits
        // do no further I/O; unchecked audits retry without changing health.
        self.privileges.check(&self.collection_id, &self.pool).await;
        self.health.record_refresh(result.is_ok(), start.elapsed());
        let now_cut = self.locations_cut();
        if result.is_ok() && now_cut != was_cut {
            // Log the transitions only: `/health` and the gauge carry the
            // state while it lasts.
            match now_cut {
                Some(cut) => tracing::warn!(
                    collection = %self.collection_id,
                    max_locations = self.config.max_locations,
                    "postgis: {} (collection degraded)",
                    cut.reason(self.config.max_locations)
                ),
                None => tracing::info!(
                    collection = %self.collection_id,
                    max_locations = self.config.max_locations,
                    "postgis: station list fits max_locations again"
                ),
            }
        }
        result
    }

    // These errors reach public /health at boot: diagnostics belong in logs.
    async fn refresh_metadata_checked(&self) -> Result<(), DataServerError> {
        if let Err(e) = crate::validation::validate(&self.config).await {
            tracing::warn!(
                collection = %self.collection_id, error = ?e,
                "postgis: schema validation failed"
            );
            if matches!(e, crate::validation::ValidationError::Mismatch(_)) {
                self.schema_state.store(SCHEMA_INVALID, Ordering::Release);
            }
            self.reject_invalid_schema()?;
            return Err(DataServerError::Engine(
                "database schema validation unavailable (see server logs)".into(),
            ));
        }
        if let Err(e) = self.cache.refresh(&self.config, &self.pool).await {
            tracing::warn!(
                collection = %self.collection_id, error = %e,
                "postgis: metadata refresh failed"
            );
            if matches!(e, crate::metadata::MetadataError::GeometryMismatch) {
                self.schema_state.store(SCHEMA_INVALID, Ordering::Release);
                return Err(DataServerError::Engine(
                    crate::validation::SCHEMA_ERROR.into(),
                ));
            }
            return Err(DataServerError::Engine(
                "metadata refresh failed (database error; see server logs)".into(),
            ));
        }
        self.schema_state.store(SCHEMA_VALID, Ordering::Release);
        Ok(())
    }

    /// Requests only consult this atomic state; database validation runs on refresh.
    pub(crate) fn require_validated(&self) -> Result<(), DataServerError> {
        match self.schema_state.load(Ordering::Acquire) {
            SCHEMA_VALID => Ok(()),
            SCHEMA_INVALID => Err(DataServerError::Engine(
                crate::validation::SCHEMA_ERROR.into(),
            )),
            _ => Err(DataServerError::Engine(
                crate::validation::PENDING_ERROR.into(),
            )),
        }
    }

    /// A pending engine may still expose its empty, config-seeded inventory.
    pub(crate) fn reject_invalid_schema(&self) -> Result<(), DataServerError> {
        if self.schema_state.load(Ordering::Acquire) == SCHEMA_INVALID {
            return Err(DataServerError::Engine(
                crate::validation::SCHEMA_ERROR.into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn check_geometry_row(&self, row: &Row) -> Result<(), DataServerError> {
        if !crate::validation::geometry_row_valid(row) {
            self.schema_state.store(SCHEMA_INVALID, Ordering::Release);
            tracing::warn!(collection = %self.collection_id, "postgis: returned geometry is not a Point with SRID 4326");
        }
        self.reject_invalid_schema()
    }

    /// `SELECT 1` with a 2 s deadline — the `/health` reachability probe.
    ///
    /// Uses a **dedicated** connection (`tokio_postgres::connect`), NOT the shared
    /// pool: a busy pool (all connections checked out by request handlers) would
    /// otherwise make `pool.get()` time out and masquerade as DB unreachability,
    /// flipping a perfectly-healthy collection to `degraded`. The probe measures
    /// reachability only; pool saturation is observable via `postgis_pool_waiting`.
    /// `NoTls` matches the engine's connection (TLS is the remaining #110 work).
    async fn ping(&self) -> bool {
        let probe = async {
            // TODO(#110): when TLS lands, this connector MUST match the pool's
            // (currently both `NoTls`). If the ping stays `NoTls` against a
            // TLS-required server, it would be rejected while the pool works —
            // falsely degrading a healthy collection.
            let (client, conn) = tokio_postgres::connect(&self.config.dsn, tokio_postgres::NoTls)
                .await
                .ok()?;
            // Drive the connection INLINE (no `tokio::spawn`): if the outer
            // timeout drops this future, both the query and the connection driver
            // drop with it — a detached spawned task would instead leak one
            // orphan driver per timed-out ping until its socket closed.
            tokio::pin!(conn);
            let ok = tokio::select! {
                res = client.query_one("SELECT 1", &[]) => res.is_ok(),
                // The driver future only resolves if the connection errors/closes.
                _ = &mut conn => false,
            };
            Some(ok)
        };
        matches!(
            tokio::time::timeout(PING_TIMEOUT, probe).await,
            Ok(Some(true))
        )
    }

    /// Background loop: a `SELECT 1` ping every [`PING_INTERVAL`] (the `/health`
    /// reachability probe) and a metadata
    /// refresh every `metadata_refresh_secs` (keeps the location list / extents /
    /// `locations_window` set current — a failed refresh keeps the previous
    /// snapshot, so a blip never empties the cache). Spawned on the dedicated
    /// background poll runtime (never a request worker, #221); exits on
    /// `shutdown()`. The metadata-refresh interval skips its first tick (boot
    /// already refreshed), but the ping fires immediately so `/health` is
    /// accurate within ~2 s of boot.
    pub async fn poll_loop(&self) {
        // Two cadences in one task, so this keeps its own `select!` over the
        // shared [`Shutdown`] rather than using a single `PollTicker`.
        let mut refresh_iv = tokio::time::interval(Duration::from_secs(
            self.config.metadata_refresh_secs.max(1),
        ));
        refresh_iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        refresh_iv.tick().await; // boot already refreshed — skip the immediate tick
        let mut ping_iv = tokio::time::interval(PING_INTERVAL); // first tick fires now
        ping_iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                biased;
                _ = self.shutdown.wait() => {
                    tracing::info!(collection = %self.collection_id, "postgis: poll loop shutting down");
                    break;
                }
                _ = ping_iv.tick() => {
                    // Log only on a transition (not every tick) so log-based
                    // alerting sees DB down/recovery without spam.
                    let reachable = self.ping().await;
                    match self.health.record_ping(reachable) {
                        Some(HealthStatus::Degraded) => tracing::warn!(
                            collection = %self.collection_id,
                            "postgis: DB unreachable (health ping failed) — collection degraded"
                        ),
                        Some(HealthStatus::Ready) => tracing::info!(
                            collection = %self.collection_id,
                            "postgis: DB reachable again"
                        ),
                        None => {}
                    }
                    if reachable {
                        if self.schema_state.load(Ordering::Acquire) != SCHEMA_VALID {
                            let _ = self.refresh_metadata().await;
                        } else {
                            self.privileges.check(&self.collection_id, &self.pool).await;
                        }
                    }
                }
                _ = refresh_iv.tick() => {
                    if let Err(e) = self.refresh_metadata().await {
                        tracing::warn!(
                            collection = %self.collection_id,
                            error = %e,
                            "postgis: background metadata refresh failed — keeping the previous snapshot"
                        );
                    }
                }
            }
        }
    }

    /// Signal [`poll_loop`] to stop (called on reload before the engine is
    /// replaced).
    pub fn shutdown(&self) {
        self.shutdown.shutdown();
    }

    fn load_meta(&self) -> Arc<CollectionMeta> {
        self.cache.load()
    }

    /// Events-shape area query: one SQL statement over the event table, one
    /// `Point` coverage per returned event row. An empty window is a valid
    /// empty `CoverageCollection` (no strikes in the area is an answer, not
    /// a 404 — unlike a missing station).
    fn query_area_events(
        &self,
        shape: &EventsShape,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
    ) -> Result<CoverageResponse, DataServerError> {
        let source_keys = resolve_source_keys(&self.config, parameters)?;
        let key_refs: Vec<&str> = source_keys.iter().map(String::as_str).collect();
        let polygon_wkt = normalize_area_wkt(coords)?;

        // No `datetime` never means full history — fall back to the
        // configured window ending "now". `events_default_window` is always
        // Some for an events config (resolve sets it unconditionally); the
        // fallback shares the resolve-time constant so they cannot drift.
        let (t0, t1) = match datetime {
            Some(range) => range,
            None => {
                let window = self.config.events_default_window.unwrap_or_else(|| {
                    chrono::Duration::hours(crate::config::DEFAULT_EVENTS_WINDOW_HOURS)
                });
                let now = Utc::now();
                (now - window, now)
            }
        };

        let per_row = key_refs.len().max(1);
        // +1 sentinel row: crossing the budget is detected as "one more row
        // than fits" rather than silently truncating an over-budget window.
        let row_limit = MAX_RESPONSE_VALUES / per_row + 1;
        let built = build_events_area(shape, &polygon_wkt, (t0, t1), &key_refs, row_limit)
            .map_err(|e| DataServerError::Engine(format!("build_events_area: {e}")))?;

        let rows = run_single_query_sync(&self.pool, built)?;
        for row in &rows {
            self.check_geometry_row(row)?;
        }
        if rows.len() * per_row > MAX_RESPONSE_VALUES {
            return Err(events_budget_exceeded());
        }
        let events = decode_event_rows(&rows, &key_refs)?;
        Ok(CoverageResponse::Collection(assemble_event_coverages(
            &self.config,
            &key_refs,
            events,
        )))
    }
}

// ─── EventSource (#549): recent point events for cross-engine joins ─────────

impl ds_core::events::EventSource for PostgisEngine {
    fn covers(&self, bbox: [f64; 4]) -> Option<bool> {
        let [w, s, e, n] = self.config.events()?.coverage_bbox?;
        Some(bbox[0] >= w && bbox[1] >= s && bbox[2] <= e && bbox[3] <= n)
    }

    /// Events-shape window fetch for consumers in OTHER engines (the
    /// nowcast per-cell lightning join): the half-open window `(start,
    /// end]` across the whole table extent, ascending by time, NEWEST kept
    /// when `limit` truncates. Reuses the #504 window SQL and the engine's
    /// sync bridge — the ds-storage calling rules apply (multi-thread
    /// runtime worker only; the background poll runtime qualifies).
    fn recent_events(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<ds_core::events::EventPoint>, DataServerError> {
        self.require_validated()?;
        let Some(shape) = self.config.events() else {
            return Err(DataServerError::Engine(
                "recent_events requires an events-shape postgis collection".into(),
            ));
        };
        let built = build_events_window(shape, (start, end), limit, WindowAttrs::Include)
            .map_err(|e| DataServerError::Engine(format!("build_events_window: {e}")))?;
        let rows = run_single_query_sync(&self.pool, built)?;
        for row in &rows {
            self.check_geometry_row(row)?;
        }
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let time: DateTime<Utc> = row
                .try_get("time")
                .map_err(|e| DataServerError::Engine(format!("decode event time: {e}")))?;
            let lon: Option<f64> = row
                .try_get("lon")
                .map_err(|e| DataServerError::Engine(format!("decode event lon: {e}")))?;
            let lat: Option<f64> = row
                .try_get("lat")
                .map_err(|e| DataServerError::Engine(format!("decode event lat: {e}")))?;
            // Degenerate geometries (ST_X of POINT EMPTY) skip the event,
            // mirroring the EDR decode path.
            let (Some(lon), Some(lat)) = (lon, lat) else {
                continue;
            };
            // Attribute columns are optional AND nullable, so a missing
            // column and a NULL row both land on None — "not reported",
            // distinct from a reported zero.
            // All three are cast to `double precision` in SQL, so one decode
            // covers smallint / numeric / real source columns alike.
            let num = |name| row.try_get::<_, Option<f64>>(name).ok().flatten();
            let attrs = ds_core::events::EventAttrs {
                cloud_indicator: num("cloud_indicator").map(|v| v as i16),
                peak_current_ka: num("peak_current").map(|v| v as f32),
            };
            out.push(ds_core::events::EventPoint {
                time,
                lon,
                lat,
                attrs,
            });
        }
        // Window SQL orders newest-first (so truncation keeps the newest);
        // the trait contract is ascending.
        out.reverse();
        Ok(out)
    }
}

// ─── Engine trait ────────────────────────────────────────────────────────────

impl EdrEngine for PostgisEngine {
    fn get_locations(&self) -> Result<Arc<LocationSnapshot>, DataServerError> {
        self.reject_invalid_schema()?;
        Ok(Arc::clone(&self.load_meta().locations))
    }

    fn get_parameters(&self) -> Vec<String> {
        self.config
            .parameters
            .iter()
            .map(|p| p.name.clone())
            .collect()
    }

    fn get_parameter_descriptions(&self) -> HashMap<String, ParameterDescription> {
        (*self.load_meta().parameters).clone()
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        self.load_meta().temporal_extent
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        self.load_meta().spatial_extent
    }

    fn supported_query_types(&self) -> Vec<String> {
        if self.config.events().is_some() {
            // Events have no stations: no locations, no position (a point
            // has probability zero of hitting an event) — area only.
            return vec!["area".to_string(), "radius".to_string()];
        }
        vec![
            "locations".to_string(),
            "position".to_string(),
            "area".to_string(),
            "radius".to_string(),
        ]
    }

    /// Station shapes answer one `PointSeries` per station, at the cached
    /// location's coordinates: EDR GeoJSON names each (#929). Events are
    /// `Point` coverages with no station, so they stay CoverageJSON only.
    fn serves_station_series(&self) -> bool {
        self.config.events().is_none()
    }

    fn query_location(
        &self,
        location_id: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        reject_station_query_on_events(&self.config)?;
        self.require_validated()?;
        let source_keys = resolve_source_keys(&self.config, parameters)?;
        let key_refs: Vec<&str> = source_keys.iter().map(String::as_str).collect();

        // Single-station path: the whole response budget is this station's
        // to spend, so long time series work — the per-query LIMIT bind is
        // rewritten to the remaining budget as queries run.
        let queries = build_location(
            &self.config,
            location_id,
            datetime,
            &key_refs,
            MAX_RESPONSE_VALUES,
        )
        .map_err(|e| DataServerError::Engine(format!("build_location: {e}")))?;

        let (lon, lat) = lookup_station_coords(&self.load_meta(), location_id)?;
        let decoder = RowDecoder::new(&self.config, &key_refs, false)?;
        let rows_per_query = run_queries_budgeted_sync(&self.pool, queries.clone())?;
        Ok(CoverageResponse::Single(assemble_station(
            &decoder,
            location_id,
            lon,
            lat,
            &queries,
            rows_per_query,
        )?))
    }

    fn query_position(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        reject_station_query_on_events(&self.config)?;
        self.require_validated()?;
        let (lon, lat) = parse_coords(coords)?;
        // Observations-derived modes (A/B) resolve the nearest station in-memory
        // from the cached location set — a scan over a few thousand points is
        // far cheaper than a spatial query over the multi-million-row obs
        // hypertable, and it sees orphans uniformly. Stations-only mode keeps
        // the live `ST_DWithin` path.
        let station_id = if self.config.location_source.uses_observations() {
            nearest_in_memory(&self.load_meta(), lon, lat, DEFAULT_POSITION_RADIUS_M).ok_or_else(
                || {
                    DataServerError::LocationNotFound(format!(
                        "no station within {DEFAULT_POSITION_RADIUS_M:.0} m of ({lon}, {lat})"
                    ))
                },
            )?
        } else {
            resolve_nearest_station(self, &self.config, lon, lat)?
        };
        self.query_location(&station_id, datetime, parameters, z, reference_time)
    }

    fn query_area(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        self.query_area_on(None, coords, datetime, parameters)
    }
}

impl PostgisEngine {
    /// The EDR area query on `path`, or, with `None`, on the path
    /// [`EdrEngine::query_area`] takes: set-based for a `datetime` window of at
    /// most [`SET_BASED_MAX_WINDOW`] and for one the fan-out could not run
    /// (more than [`MAX_AREA_QUERIES`] statements), the per-station fan-out
    /// otherwise and without `datetime` (#1062). Both paths answer the same
    /// coverages in the same order on the same data, with the exceptions
    /// [`run_area_set`] lists; the choice is otherwise cost only.
    ///
    /// Not part of the engine contract: public only so the database tests
    /// can run both paths on one dataset. A forced path skips `area_path`'s
    /// rules, so a forced `SetBased` also reads a request without
    /// `datetime` in one statement. Events collections ignore `path`: they
    /// always run one statement.
    #[doc(hidden)]
    pub fn query_area_on(
        &self,
        path: Option<AreaPath>,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
    ) -> Result<CoverageResponse, DataServerError> {
        self.require_validated()?;
        if let ObservationSchema::Events(ev) = &self.config.observations {
            // Clone the shape so the borrow on `self.config` ends here —
            // `query_area_events` needs `&self` too.
            let ev = ev.clone();
            return self.query_area_events(&ev, coords, datetime, parameters);
        }
        let source_keys = resolve_source_keys(&self.config, parameters)?;
        let key_refs: Vec<&str> = source_keys.iter().map(String::as_str).collect();

        // Observations-derived modes (A/B) select stations in-memory from the
        // cached set with an exact point-in-polygon test (the bbox prefilter
        // is inside `QueryPolygon::contains`; #671). Stations-only mode keeps
        // the live `ST_Within` prefilter.
        let stations = if self.config.location_source.uses_observations() {
            let polygon = parse_area_coords(coords)?;
            let meta = self.load_meta();
            // Same cells × vertices bound the gridded engines apply: every
            // cached station is ray-cast against the ring before `take` can
            // stop it.
            check_mask_budget(meta.locations.len(), &polygon)?;
            stations_in_polygon(&meta.locations, &polygon)
        } else {
            let polygon_wkt = normalize_area_wkt(coords)?;
            run_stations_in_polygon_sync(self, &self.config, &polygon_wkt)?
        };
        if stations.is_empty() {
            return Ok(CoverageResponse::Collection(vec![]));
        }
        check_station_count(stations.len())?;
        let stations = distinct_stations(stations);
        let per_station = area_queries_per_station(&self.config, &key_refs);
        let path = path.unwrap_or_else(|| area_path(datetime, stations.len(), per_station));

        let results = match path {
            AreaPath::SetBased => run_area_set(
                &self.pool,
                &self.pool_key_label,
                &self.config,
                &stations,
                datetime,
                &key_refs,
            )?,
            AreaPath::FanOut => {
                enforce_area_query_count(stations.len(), per_station)?;
                run_area_fanout(
                    &self.pool,
                    &self.pool_key_label,
                    &self.config,
                    &stations,
                    datetime,
                    &key_refs,
                )?
            }
        };
        Ok(CoverageResponse::Collection(results))
    }
}

// ─── helpers ───────────────────────────────────────────────────────────────

/// The observations-derived area prefilter: cached stations inside the
/// polygon (exact point-in-polygon, boundary inclusive; #671). Stops at the
/// cap + nothing more — mirrors the SQL path's `LIMIT MAX_STATIONS_IN_POLYGON`,
/// so a huge polygon over a dense dataset never materialises an unbounded
/// Vec just to error out.
fn stations_in_polygon(locations: &[Location], polygon: &QueryPolygon) -> Vec<Location> {
    locations
        .iter()
        .filter(|l| polygon.contains(l.longitude, l.latitude))
        .take(MAX_STATIONS_IN_POLYGON)
        .cloned()
        .collect()
}

/// The prefilter's stations with each id once, at its first place. A
/// stations view without a unique id can list one station twice; both area
/// paths then read it once, so they answer the same coverages (#1062).
fn distinct_stations(mut stations: Vec<Location>) -> Vec<Location> {
    let keep: Vec<bool> = {
        let mut seen = HashSet::with_capacity(stations.len());
        stations
            .iter()
            .map(|s| seen.insert(s.id.as_str()))
            .collect()
    };
    if keep.iter().all(|&k| k) {
        return stations;
    }
    // `retain` visits every element once, in order.
    let mut keep = keep.into_iter();
    stations.retain(|_| keep.next().unwrap_or(true));
    stations
}

/// Both area prefilters (SQL `LIMIT`, in-memory `.take`) fetch one station
/// past the [`MAX_STATIONS_IN_POLYGON`] memory guard; a full batch means the
/// polygon matched more stations than it allows. The gate on a request's
/// size is the response-value budget (#1062): this only bounds the
/// prefilter buffer and the station id list. `QueryTooLarge` → HTTP 400.
fn check_station_count(matched: usize) -> Result<(), DataServerError> {
    if matched >= MAX_STATIONS_IN_POLYGON {
        return Err(DataServerError::QueryTooLarge(format!(
            "area query matched more than the {} station ceiling — narrow the polygon",
            MAX_STATIONS_IN_POLYGON - 1
        )));
    }
    Ok(())
}

/// The path an area query takes when the caller does not force one (#1062).
///
/// - No `datetime`: the fan-out. Every station's whole history is read, and
///   the per-station statements keep their row cap.
/// - A window of at most [`SET_BASED_MAX_WINDOW`] (an instant, each instant
///   of a `datetime` list): set-based, one statement per observation table.
/// - A longer window the fan-out could not run (more than
///   [`MAX_AREA_QUERIES`] statements): set-based, so the value budget decides
///   rather than the statement count.
/// - Any other window: the fan-out, whose parallel `(station, time)` index
///   scans win on long series.
fn area_path(
    datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
    stations: usize,
    queries_per_station: usize,
) -> AreaPath {
    let Some((start, end)) = datetime else {
        return AreaPath::FanOut;
    };
    if end.signed_duration_since(start) <= SET_BASED_MAX_WINDOW
        || stations.saturating_mul(queries_per_station) > MAX_AREA_QUERIES
    {
        AreaPath::SetBased
    } else {
        AreaPath::FanOut
    }
}

/// SQL queries one station contributes to an area fan-out: the
/// `per_parameter` shape runs one query per requested parameter; `long`
/// and `wide` run one query per station regardless of parameter count.
fn area_queries_per_station(cfg: &PostgisEngineConfig, source_keys: &[&str]) -> usize {
    match &cfg.observations {
        ObservationSchema::PerParameter(_) => source_keys.len().max(1),
        _ => 1,
    }
}

/// Bound a fan-out's statements before any of them runs: stations ×
/// per-station queries. This is the dimension the station count and the
/// parameter list multiply into — 8k stations × 1 parameter passes, 8k × 6
/// does not. The set-based path is not bound by it (#1062).
fn enforce_area_query_count(
    stations: usize,
    queries_per_station: usize,
) -> Result<(), DataServerError> {
    let total = stations.saturating_mul(queries_per_station);
    if total > MAX_AREA_QUERIES {
        return Err(DataServerError::QueryTooLarge(format!(
            "area query would fan out {stations} stations × {queries_per_station} parameter queries = {total} SQL queries (max {MAX_AREA_QUERIES}) — narrow the polygon or the parameter list"
        )));
    }
    Ok(())
}

/// The response-budget breach error. One message per shape family so the
/// dimensions named actually exist on the collection being queried.
fn budget_exceeded_for(dimensions: &str) -> DataServerError {
    DataServerError::QueryTooLarge(format!(
        "response would exceed the {MAX_RESPONSE_VALUES}-value budget ({dimensions}) — narrow the time range, the polygon, or the parameter list"
    ))
}

/// Station-shape budget breach (position/location/area fan-out paths).
fn budget_exceeded() -> DataServerError {
    budget_exceeded_for("stations × parameters × timesteps")
}

/// Events-shape budget breach — there are no stations or timesteps here,
/// the budget is rows × selected parameter columns.
fn events_budget_exceeded() -> DataServerError {
    budget_exceeded_for("events × parameters")
}

/// Station-keyed queries (position, location) are meaningless on an events
/// collection — reject with an actionable pointer at the area query.
fn reject_station_query_on_events(cfg: &PostgisEngineConfig) -> Result<(), DataServerError> {
    if cfg.events().is_some() {
        return Err(DataServerError::InvalidParameter(
            "this collection holds event data with no stations — use the area query".into(),
        ));
    }
    Ok(())
}

fn resolve_source_keys(
    cfg: &PostgisEngineConfig,
    parameters: Option<&[String]>,
) -> Result<Vec<String>, DataServerError> {
    // None or empty-slice both mean "all configured parameters".
    let requested = parameters.unwrap_or(&[]);
    if requested.is_empty() {
        return Ok(cfg
            .parameters
            .iter()
            .map(|p| p.source_key.clone())
            .collect());
    }
    let mut out = Vec::with_capacity(requested.len());
    for name in requested {
        let p = cfg
            .parameters
            .iter()
            .find(|p| &p.name == name)
            .ok_or_else(|| {
                DataServerError::InvalidParameter(format!("unknown parameter: {name}"))
            })?;
        out.push(p.source_key.clone());
    }
    Ok(out)
}

fn source_key_to_param_name(cfg: &PostgisEngineConfig, key: &str) -> String {
    cfg.parameters
        .iter()
        .find(|p| p.source_key == key)
        .map(|p| p.name.clone())
        .unwrap_or_else(|| key.to_string())
}

fn lookup_station_coords(
    meta: &CollectionMeta,
    location_id: &str,
) -> Result<(f64, f64), DataServerError> {
    let i = *meta
        .station_idx
        .get(location_id)
        .ok_or_else(|| DataServerError::LocationNotFound(location_id.to_string()))?;
    let station = &meta.locations[i];
    Ok((station.longitude, station.latitude))
}

/// Run one station's observation queries sequentially, spending the whole
/// [`MAX_RESPONSE_VALUES`] budget. Before each query the `LIMIT $N` bind is
/// rewritten to the remaining budget (in rows, +1 sentinel), so a breach is
/// detected at the row that crosses the line instead of after an unbounded
/// fetch — this is what lets a single station return a long time series.
fn run_queries_budgeted_sync(
    pool: &Pool,
    mut queries: Vec<BuiltQuery>,
) -> Result<Vec<Vec<Row>>, DataServerError> {
    let pool = pool.clone();
    block_on_async(async move {
        let client = pool
            .get()
            .await
            .map_err(|e| DataServerError::Engine(format!("pool acquire failed: {e}")))?;
        let mut remaining = MAX_RESPONSE_VALUES;
        let mut out = Vec::with_capacity(queries.len());
        for q in &mut queries {
            let per_row = q.values_per_row.max(1);
            q.set_row_limit(remaining / per_row + 1);
            let refs = params_as_refs(&q.params);
            let rows = client
                .query(&q.sql, &refs)
                .await
                .map_err(|e| map_pg_error(e, q))?;
            let values = rows.len() * per_row;
            if values > remaining {
                return Err(budget_exceeded());
            }
            remaining -= values;
            out.push(rows);
        }
        Ok(out)
    })
}

/// Process-global fan-out limiters, one per pool key (`<user>@<host>:
/// <port>/<db>` or `pool_label`). The connection pool is shared across
/// every collection on the same DSN, so the two-connection headroom must
/// hold across ALL concurrent area requests — a per-request width alone
/// lets two simultaneous fan-outs jointly drain the pool. Total in-flight
/// area stations per pool key ≤ `clamp(pool max_size − 2, 1, 16)`.
/// First-caller-wins on size (matching the pool registry's own rule);
/// entries persist across reloads, which is correct because the pool key
/// identifies the same upstream database.
static FANOUT_LIMITERS: std::sync::OnceLock<
    std::sync::Mutex<HashMap<String, Arc<tokio::sync::Semaphore>>>,
> = std::sync::OnceLock::new();

fn fanout_limiter(pool_key: &str, width: usize) -> Arc<tokio::sync::Semaphore> {
    let map = FANOUT_LIMITERS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    map.lock()
        .expect("fanout limiter mutex poisoned")
        .entry(pool_key.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(width)))
        .clone()
}

/// Concurrent bounded fan-out over an area's stations (#115). Width is the
/// pool size minus two-connection headroom (capped at 16, floor 1), and is
/// enforced *globally per pool key* via [`fanout_limiter`]: the pool is
/// shared across every collection on the same DSN, so no combination of
/// concurrent area requests may check out every connection and stall
/// unrelated single-station lookups. Each in-flight station holds one
/// semaphore permit, then one pooled connection (always in that order —
/// permit holders never wait on permits, so there is no circular wait);
/// results keep station order (`buffered`, not `buffer_unordered`) so
/// responses stay deterministic. Rows are charged against the shared
/// [`MAX_RESPONSE_VALUES`] budget as they arrive; each single query
/// additionally keeps the [`MAX_OBSERVATION_ROWS`] cap so the transient
/// row buffer is bounded by fan-out width × cap. The first error cancels
/// the remaining in-flight queries.
///
/// Budget semantics under concurrency: the accumulated total is checked
/// strictly after every query, so a 200 response can never exceed the
/// budget — but up to `width` in-flight stations may each fetch (and then
/// discard) one more capped query's rows after the crossing point, so the
/// *transient DB work* before the 400 can overshoot by ≤ width ×
/// [`MAX_OBSERVATION_ROWS`] rows. The pre-query `load` bail below keeps
/// queued stations from adding to that once a breach is visible.
fn run_area_fanout(
    pool: &Pool,
    pool_key: &str,
    config: &Arc<PostgisEngineConfig>,
    stations: &[Location],
    datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
    source_keys: &[&str],
) -> Result<Vec<QueryResult>, DataServerError> {
    let width = fanout_width(pool);
    let limiter = fanout_limiter(pool_key, width);
    let decoder = RowDecoder::new(config, source_keys, false)?;
    let decoder = &decoder;
    let spent = AtomicUsize::new(0);
    let pool = pool.clone();
    let results: Vec<Option<QueryResult>> = block_on_async(async {
        futures::stream::iter(stations.iter().map(|station| {
            let pool = pool.clone();
            let config = Arc::clone(config);
            let limiter = Arc::clone(&limiter);
            let spent = &spent;
            async move {
                let _permit = limiter
                    .acquire_owned()
                    .await
                    .map_err(|_| DataServerError::Engine("fanout limiter closed".into()))?;
                let queries = build_location(
                    &config,
                    &station.id,
                    datetime,
                    source_keys,
                    MAX_OBSERVATION_ROWS,
                )
                .map_err(|e| DataServerError::Engine(format!("build_location: {e}")))?;
                let client = pool
                    .get()
                    .await
                    .map_err(|e| DataServerError::Engine(format!("pool acquire failed: {e}")))?;
                let mut rows_per_query = Vec::with_capacity(queries.len());
                for q in &queries {
                    // A sibling already breached the budget — don't add
                    // more DB work to a request that is going to 400.
                    if spent.load(Ordering::Relaxed) > MAX_RESPONSE_VALUES {
                        return Err(budget_exceeded());
                    }
                    let refs = params_as_refs(&q.params);
                    let rows = client
                        .query(&q.sql, &refs)
                        .await
                        .map_err(|e| map_pg_error(e, q))?;
                    if rows.len() >= MAX_OBSERVATION_ROWS {
                        // Report VALUES, not rows: a wide-shape row carries
                        // one value per selected column.
                        return Err(DataServerError::QueryTooLarge(format!(
                            "station '{}' alone has {}+ values in the window — narrow the time range, or use the position/location query for long single-station series",
                            station.id,
                            (MAX_OBSERVATION_ROWS - 1) * q.values_per_row.max(1)
                        )));
                    }
                    let values = rows.len() * q.values_per_row.max(1);
                    if spent.fetch_add(values, Ordering::Relaxed) + values > MAX_RESPONSE_VALUES {
                        return Err(budget_exceeded());
                    }
                    rows_per_query.push(rows);
                }
                drop(client);
                // A single-station gap inside an area window is expected
                // (sparse stations, retired sensors, missing parameter for
                // that station). Skip it rather than 404'ing the whole
                // request. Real errors still propagate.
                match assemble_station(
                    decoder,
                    &station.id,
                    station.longitude,
                    station.latitude,
                    &queries,
                    rows_per_query,
                ) {
                    Ok(qr) => Ok(Some(qr)),
                    Err(DataServerError::LocationNotFound(_)) => Ok(None),
                    Err(e) => Err(e),
                }
            }
        }))
        .buffered(width)
        .try_collect()
        .await
    })?;
    Ok(results.into_iter().flatten().collect())
}

/// Area connections one pool key may hold at once: the pool size minus a
/// two-connection headroom, between 1 and 16.
fn fanout_width(pool: &Pool) -> usize {
    (pool.status().max_size.saturating_sub(2)).clamp(1, 16)
}

/// The set-based area path (#1062): every station in `stations` read by one
/// statement per observation table (`long`/`wide` one, `per_parameter` one
/// per parameter), run in turn on one pooled connection that holds one of the
/// pool key's [`fanout_limiter`] permits, so area requests together keep the
/// pool's headroom whichever path they take.
///
/// Rows stream in ordered by station and time and are charged against
/// [`MAX_RESPONSE_VALUES`] as they arrive: each statement's `LIMIT` is the
/// remaining budget in rows plus a sentinel row, so the server stops one row
/// past it, and the row that crosses the budget is the 400. Each row is
/// decoded into its station's series at once (no `Row` is kept), by the
/// same [`RowDecoder`] the fan-out uses, and the coverages are assembled in
/// `stations` order, so both paths answer the same coverages in the same
/// order. A station with no row in the window is left out, as in the
/// fan-out.
///
/// Where the two paths differ:
/// - No per-station row cap: that bounds the fan-out's concurrent buffers,
///   and here the budget bounds the whole response. A station with more
///   than [`MAX_OBSERVATION_ROWS`] rows in the window is served here and is
///   the fan-out's 400.
/// - Rows are matched to stations by `<fk>::text`. A foreign key whose
///   comparison ignores what its text keeps (`citext` ignores case) can
///   match `= ANY` under a spelling other than the requested id's; such
///   rows are skipped with a WARN, while the fan-out keeps them under the
///   requested id.
///
/// `stations` holds each id once (`distinct_stations`); a repeated id would
/// keep its first place.
///
/// Memory: besides the rows' series, a station costs about 200 bytes here
/// (the id list, the id index and a series slot), on top of the
/// prefilter's `Location` (see [`MAX_STATIONS_IN_POLYGON`]).
fn run_area_set(
    pool: &Pool,
    pool_key: &str,
    config: &PostgisEngineConfig,
    stations: &[Location],
    datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
    source_keys: &[&str],
) -> Result<Vec<QueryResult>, DataServerError> {
    let ids: Arc<Vec<String>> = Arc::new(stations.iter().map(|s| s.id.clone()).collect());
    let mut queries = build_area_set(config, &ids, datetime, source_keys, MAX_RESPONSE_VALUES)
        .map_err(|e| DataServerError::Engine(format!("build_area_set: {e}")))?;
    let decoder = RowDecoder::new(config, source_keys, true)?;
    // Position of each station id in `stations`, so the coverages come out
    // in the prefilter's order whatever order the database sorts ids in.
    let mut index: HashMap<&str, usize> = HashMap::with_capacity(stations.len());
    for (i, station) in stations.iter().enumerate() {
        index.entry(station.id.as_str()).or_insert(i);
    }
    let mut series: Vec<Option<StationRows>> = std::iter::repeat_with(|| None)
        .take(stations.len())
        .collect();
    let limiter = fanout_limiter(pool_key, fanout_width(pool));
    let pool = pool.clone();
    let unmatched = block_on_async(async {
        let _permit = limiter
            .acquire()
            .await
            .map_err(|_| DataServerError::Engine("fanout limiter closed".into()))?;
        let client = pool
            .get()
            .await
            .map_err(|e| DataServerError::Engine(format!("pool acquire failed: {e}")))?;
        let mut remaining = MAX_RESPONSE_VALUES;
        let mut unmatched = 0usize;
        for i in 0..queries.len() {
            let per_row = queries[i].values_per_row.max(1);
            queries[i].set_row_limit(remaining / per_row + 1);
            let q = &queries[i];
            let rows = client
                .query_raw(&q.sql, params_as_refs(&q.params))
                .await
                .map_err(|e| map_pg_error(e, q))?;
            futures::pin_mut!(rows);
            while let Some(row) = rows.try_next().await.map_err(|e| map_pg_error(e, q))? {
                if per_row > remaining {
                    return Err(budget_exceeded());
                }
                remaining -= per_row;
                let id: &str = row
                    .try_get(0)
                    .map_err(|e| DataServerError::Engine(format!("decode station: {e}")))?;
                let Some(&at) = index.get(id) else {
                    unmatched += 1;
                    continue;
                };
                let station = series[at].get_or_insert_with(|| decoder.station(&queries));
                decoder.push(station, q, &row)?;
            }
        }
        Ok::<_, DataServerError>(unmatched)
    })?;
    if unmatched > 0 {
        // `<fk>::text` of a row did not spell any requested id, as a column
        // type whose text form differs from its comparison (citext) would.
        tracing::warn!(
            pool_key,
            unmatched,
            "postgis: set-based area rows whose station id matched no requested station were skipped"
        );
    }

    let mut out = Vec::new();
    for (station, rows) in stations.iter().zip(series) {
        let Some(rows) = rows else {
            continue;
        };
        match decoder.finish(&station.id, station.longitude, station.latitude, rows) {
            Ok(qr) => out.push(qr),
            Err(DataServerError::LocationNotFound(_)) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

/// Run one built query on a pooled connection (sync bridge). Used by the
/// events area path — a single statement, no fan-out, no budget rewrite
/// (the LIMIT bind already carries the budget sentinel).
fn run_single_query_sync(pool: &Pool, built: BuiltQuery) -> Result<Vec<Row>, DataServerError> {
    let pool = pool.clone();
    block_on_async(async move {
        let client = pool
            .get()
            .await
            .map_err(|e| DataServerError::Engine(format!("pool acquire failed: {e}")))?;
        let refs = params_as_refs(&built.params);
        client
            .query(&built.sql, &refs)
            .await
            .map_err(|e| map_pg_error(e, &built))
    })
}

fn run_stations_in_polygon_sync(
    engine: &PostgisEngine,
    cfg: &PostgisEngineConfig,
    polygon_wkt: &str,
) -> Result<Vec<Location>, DataServerError> {
    let built = build_stations_in_polygon(cfg, polygon_wkt)
        .map_err(|e| DataServerError::Engine(format!("build_stations_in_polygon: {e}")))?;
    let pool = engine.pool.clone();
    let sql = built.sql;
    let params = built.params;
    block_on_async(async move {
        let client = pool
            .get()
            .await
            .map_err(|e| DataServerError::Engine(format!("pool acquire failed: {e}")))?;
        let refs = params_as_refs(&params);
        let rows = client
            .query(&sql, &refs)
            .await
            .map_err(|e| DataServerError::Engine(format!("stations_in_polygon failed: {e}")))?;

        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            engine.check_geometry_row(row)?;
            let id: String = row
                .try_get("id")
                .map_err(|e| DataServerError::Engine(format!("decode station id: {e}")))?;
            let label: String = row
                .try_get("label")
                .map_err(|e| DataServerError::Engine(format!("decode station label: {e}")))?;
            let lat: f64 = row
                .try_get("lat")
                .map_err(|e| DataServerError::Engine(format!("decode lat: {e}")))?;
            let lon: f64 = row
                .try_get("lon")
                .map_err(|e| DataServerError::Engine(format!("decode lon: {e}")))?;
            out.push(Location {
                id,
                label,
                latitude: lat,
                longitude: lon,
            });
        }
        Ok(out)
    })
}

fn resolve_nearest_station(
    engine: &PostgisEngine,
    cfg: &PostgisEngineConfig,
    lon: f64,
    lat: f64,
) -> Result<String, DataServerError> {
    let built = build_position(cfg, lon, lat, DEFAULT_POSITION_RADIUS_M)
        .map_err(|e| DataServerError::Engine(format!("build_position: {e}")))?;
    let pool = engine.pool.clone();
    let sql = built.sql;
    let params = built.params;
    block_on_async(async move {
        let client = pool
            .get()
            .await
            .map_err(|e| DataServerError::Engine(format!("pool acquire failed: {e}")))?;
        let refs = params_as_refs(&params);
        let opt = client
            .query_opt(&sql, &refs)
            .await
            .map_err(|e| DataServerError::Engine(format!("query_position failed: {e}")))?;
        let Some(row) = opt else {
            return Err(DataServerError::LocationNotFound(format!(
                "no station within {DEFAULT_POSITION_RADIUS_M:.0} m of ({lon}, {lat})"
            )));
        };
        engine.check_geometry_row(&row)?;
        let id: String = row
            .try_get("id")
            .map_err(|e| DataServerError::Engine(format!("decode station id: {e}")))?;
        Ok(id)
    })
}

// ─── in-memory station selection (observations-derived modes) ──────────────

/// Nearest cached location to `(lon, lat)` within `radius_m` metres, by
/// great-circle distance. Used by `query_position` in the observations-derived
/// modes (A/B) — a linear scan over the cached set (a few thousand points)
/// instead of a spatial query against the obs hypertable. Returns the
/// location id, or `None` when nothing is in range.
fn nearest_in_memory(meta: &CollectionMeta, lon: f64, lat: f64, radius_m: f64) -> Option<String> {
    let mut best: Option<(f64, &str)> = None;
    for loc in meta.locations.iter() {
        let d = haversine_m(lat, lon, loc.latitude, loc.longitude);
        if d <= radius_m && best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, loc.id.as_str()));
        }
    }
    best.map(|(_, id)| id.to_string())
}

/// Great-circle distance in metres (haversine). Mirrors PostGIS
/// `ST_Distance(::geography)` closely enough for nearest-station selection
/// (both are great-circle; not byte-identical).
fn haversine_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const EARTH_RADIUS_M: f64 = 6_371_000.0;
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_M * a.sqrt().atan2((1.0 - a).sqrt())
}

pub(crate) fn map_pg_error(e: tokio_postgres::Error, q: &BuiltQuery) -> DataServerError {
    // Keep the SQL template out of the client-facing message; only log it.
    tracing::warn!(
        sql = %q.sql,
        n_params = q.params.len(),
        error = %e,
        "postgis query failed"
    );
    DataServerError::Engine("database query failed".into())
}

// ─── row assembly ──────────────────────────────────────────────────────────

/// One station's observations decoded from its rows, before they become a
/// coverage. Rows are decoded as they arrive, so no `Row` outlives its
/// decoding.
#[derive(Debug, Default)]
struct StationRows {
    rows: usize,
    /// `long` and `per_parameter`: one `(time, value)` series per source key,
    /// in row order.
    series: HashMap<String, ParamSeries>,
    /// `wide`: the row times, and one value column per selected source key.
    times: Vec<DateTime<Utc>>,
    columns: Vec<Vec<Option<f64>>>,
}

/// Decodes observation rows into a station's series and assembles its
/// coverage. The single-station, fan-out and set-based statements select the
/// same columns in the same order, a set-based row led by the station id
/// (#1062), so every path fills a station through these same steps and the
/// same rows give the same coverage whichever path fetched them.
struct RowDecoder<'a> {
    cfg: &'a PostgisEngineConfig,
    /// The selected source keys in projection order: the `wide` value
    /// columns.
    keys: &'a [&'a str],
    /// Column index of the time, the first observation column.
    first: usize,
}

impl<'a> RowDecoder<'a> {
    fn new(
        cfg: &'a PostgisEngineConfig,
        keys: &'a [&'a str],
        set_based: bool,
    ) -> Result<Self, DataServerError> {
        if matches!(cfg.observations, ObservationSchema::Events(_)) {
            // Events never reach the station assembly path — their area
            // query assembles Point coverages via `assemble_event_coverages`.
            return Err(DataServerError::Engine(
                "events shape has no station-keyed assembly".into(),
            ));
        }
        Ok(Self {
            cfg,
            keys,
            first: first_observation_column(set_based),
        })
    }

    /// An empty station. `per_parameter` starts with a series per query, so a
    /// parameter whose table has no row in the window is a range of nulls;
    /// `wide` with a column per selected key.
    fn station(&self, queries: &[BuiltQuery]) -> StationRows {
        let mut station = StationRows::default();
        match &self.cfg.observations {
            ObservationSchema::PerParameter(_) => {
                for key in queries.iter().filter_map(|q| q.parameter.as_ref()) {
                    station.series.entry(key.clone()).or_default();
                }
            }
            ObservationSchema::Wide(_) => station.columns = vec![Vec::new(); self.keys.len()],
            ObservationSchema::Long(_) | ObservationSchema::Events(_) => {}
        }
        station
    }

    /// Decode one row of `query` into `station`.
    fn push(
        &self,
        station: &mut StationRows,
        query: &BuiltQuery,
        row: &Row,
    ) -> Result<(), DataServerError> {
        let i = self.first;
        let time: DateTime<Utc> = row
            .try_get(i)
            .map_err(|e| DataServerError::Engine(format!("decode time: {e}")))?;
        match &self.cfg.observations {
            ObservationSchema::Long(_) => {
                let key: String = row
                    .try_get(i + 1)
                    .map_err(|e| DataServerError::Engine(format!("decode parameter: {e}")))?;
                let value: Option<f64> = row
                    .try_get(i + 2)
                    .map_err(|e| DataServerError::Engine(format!("decode value: {e}")))?;
                station.series.entry(key).or_default().push((time, value));
            }
            ObservationSchema::Wide(_) => {
                station.times.push(time);
                for (k, column) in station.columns.iter_mut().enumerate() {
                    let value: Option<f64> = row.try_get(i + 1 + k).map_err(|e| {
                        DataServerError::Engine(format!("decode {}: {e}", self.keys[k]))
                    })?;
                    column.push(value);
                }
            }
            ObservationSchema::PerParameter(_) => {
                let Some(key) = query.parameter.as_deref() else {
                    return Err(DataServerError::Engine(
                        "per_parameter query missing parameter tag".into(),
                    ));
                };
                let value: Option<f64> = row
                    .try_get(i + 1)
                    .map_err(|e| DataServerError::Engine(format!("decode value: {e}")))?;
                match station.series.get_mut(key) {
                    Some(series) => series.push((time, value)),
                    None => {
                        station.series.insert(key.to_string(), vec![(time, value)]);
                    }
                }
            }
            ObservationSchema::Events(_) => {
                return Err(DataServerError::Engine(
                    "events shape has no station-keyed assembly".into(),
                ))
            }
        }
        station.rows += 1;
        Ok(())
    }

    /// The station's `PointSeries` coverage. No row in the window is a
    /// `LocationNotFound`: the single-station path's 404, a station the area
    /// paths leave out.
    fn finish(
        &self,
        station_id: &str,
        lon: f64,
        lat: f64,
        station: StationRows,
    ) -> Result<QueryResult, DataServerError> {
        if station.rows == 0 {
            // Match CsvEngine: station exists but has no data in the queried
            // window ⇒ 404. Better than emitting an empty PointSeries (which
            // fails CoverageJSON schema validation).
            return Err(DataServerError::LocationNotFound(format!(
                "{station_id} (no data in time range)"
            )));
        }
        match &self.cfg.observations {
            ObservationSchema::Wide(_) => Ok(wide_coverage(
                self.cfg,
                lon,
                lat,
                self.keys,
                station.times,
                station.columns,
            )),
            _ => Ok(series_coverage(self.cfg, lon, lat, station.series)),
        }
    }
}

/// Decode one station's rows, query by query, and assemble its coverage:
/// the single-station and fan-out paths.
fn assemble_station(
    decoder: &RowDecoder<'_>,
    station_id: &str,
    lon: f64,
    lat: f64,
    queries: &[BuiltQuery],
    rows_per_query: Vec<Vec<Row>>,
) -> Result<QueryResult, DataServerError> {
    let mut station = decoder.station(queries);
    for (query, rows) in queries.iter().zip(&rows_per_query) {
        for row in rows {
            decoder.push(&mut station, query, row)?;
        }
    }
    decoder.finish(station_id, lon, lat, station)
}

/// The descriptor of a configured parameter, or a bare one named after it.
fn parameter_description(cfg: &PostgisEngineConfig, pname: &str) -> ParameterDescription {
    cfg.parameters
        .iter()
        .find(|p| p.name == pname)
        .map(|p| ParameterDescription {
            label: p.label.clone(),
            unit: p.unit.clone(),
            observed_property: p.observed_property.clone(),
            standard_name: None,
        })
        .unwrap_or_else(|| ParameterDescription {
            label: pname.to_string(),
            unit: String::new(),
            observed_property: pname.to_string(),
            standard_name: None,
        })
}

// ─── event row assembly (events shape) ──────────────────────────────────────

/// One decoded event row: `(time, lon, lat)` plus one optional value per
/// requested source_key, in request order.
struct EventRow {
    time: DateTime<Utc>,
    lon: f64,
    lat: f64,
    values: Vec<Option<f64>>,
}

fn decode_event_rows(rows: &[Row], source_keys: &[&str]) -> Result<Vec<EventRow>, DataServerError> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let time: DateTime<Utc> = row
            .try_get("time")
            .map_err(|e| DataServerError::Engine(format!("decode event time: {e}")))?;
        // NULL geometries never pass the WHERE clause (`NULL && x` is NULL),
        // but ST_X/ST_Y are still NULL for degenerate geometries such as
        // `POINT EMPTY` — skip the row instead of failing the whole request.
        let lon: Option<f64> = row
            .try_get("lon")
            .map_err(|e| DataServerError::Engine(format!("decode event lon: {e}")))?;
        let lat: Option<f64> = row
            .try_get("lat")
            .map_err(|e| DataServerError::Engine(format!("decode event lat: {e}")))?;
        let (Some(lon), Some(lat)) = (lon, lat) else {
            continue;
        };
        let mut values = Vec::with_capacity(source_keys.len());
        for key in source_keys {
            let v: Option<f64> = row
                .try_get(*key)
                .map_err(|e| DataServerError::Engine(format!("decode event {key}: {e}")))?;
            values.push(v);
        }
        out.push(EventRow {
            time,
            lon,
            lat,
            values,
        });
    }
    Ok(out)
}

/// One `Point` coverage per event: single-value x/y/t axes and a 0-d scalar
/// range per parameter (`shape`/`axis_names` empty — the CoverageJSON scalar
/// form). Serialised as a `CoverageCollection` with `domainType: "Point"`.
fn assemble_event_coverages(
    cfg: &PostgisEngineConfig,
    source_keys: &[&str],
    events: Vec<EventRow>,
) -> Vec<QueryResult> {
    // Descriptor per requested key, resolved once — every event coverage
    // shares them (the API layer hoists parameters to collection level).
    let descs: Vec<(String, ParameterDescription)> = source_keys
        .iter()
        .map(|key| {
            let pname = source_key_to_param_name(cfg, key);
            let desc = parameter_description(cfg, &pname);
            (pname, desc)
        })
        .collect();

    events
        .into_iter()
        .map(|ev| {
            let mut parameters = HashMap::with_capacity(descs.len());
            let mut ranges = HashMap::with_capacity(descs.len());
            for ((pname, desc), v) in descs.iter().zip(&ev.values) {
                parameters.insert(pname.clone(), desc.clone());
                ranges.insert(
                    pname.clone(),
                    NdArray {
                        shape: vec![],
                        axis_names: vec![],
                        values: vec![*v],
                    },
                );
            }
            QueryResult {
                domain: DomainDescription::Point {
                    x: ev.lon,
                    y: ev.lat,
                    t: Some(ev.time),
                    z: None,
                },
                parameters,
                ranges,
            }
        })
        .collect()
}

/// `long` and `per_parameter`: one series per source key on the union of
/// their times, sorted; a key without a value at a time is null there, and of
/// two values at one time the later row wins.
fn series_coverage(
    cfg: &PostgisEngineConfig,
    lon: f64,
    lat: f64,
    per_param: HashMap<String, ParamSeries>,
) -> QueryResult {
    let mut all_times: Vec<DateTime<Utc>> = per_param
        .values()
        .flat_map(|rows| rows.iter().map(|(t, _)| *t))
        .collect();
    all_times.sort_unstable();
    all_times.dedup();

    let mut param_descs = HashMap::new();
    let mut ranges = HashMap::new();
    for (source_key, rows) in per_param {
        let pname = source_key_to_param_name(cfg, &source_key);
        let desc = parameter_description(cfg, &pname);
        let row_map: HashMap<DateTime<Utc>, Option<f64>> = rows.into_iter().collect();
        let values: Vec<Option<f64>> = all_times
            .iter()
            .map(|t| row_map.get(t).copied().unwrap_or(None))
            .collect();
        let n = values.len();
        ranges.insert(
            pname.clone(),
            NdArray {
                shape: vec![n],
                axis_names: vec!["t".into()],
                values,
            },
        );
        param_descs.insert(pname, desc);
    }

    QueryResult {
        domain: DomainDescription::PointSeries {
            x: lon,
            y: lat,
            t: all_times,
            z: None,
        },
        parameters: param_descs,
        ranges,
    }
}

/// `wide`: the row times as they came (ordered by time in SQL), and one
/// range per selected key.
fn wide_coverage(
    cfg: &PostgisEngineConfig,
    lon: f64,
    lat: f64,
    keys: &[&str],
    times: Vec<DateTime<Utc>>,
    columns: Vec<Vec<Option<f64>>>,
) -> QueryResult {
    let mut param_descs = HashMap::new();
    let mut ranges = HashMap::new();
    for (key, values) in keys.iter().zip(columns) {
        let pname = source_key_to_param_name(cfg, key);
        let desc = parameter_description(cfg, &pname);
        let n = values.len();
        ranges.insert(
            pname.clone(),
            NdArray {
                shape: vec![n],
                axis_names: vec!["t".into()],
                values,
            },
        );
        param_descs.insert(pname, desc);
    }

    QueryResult {
        domain: DomainDescription::PointSeries {
            x: lon,
            y: lat,
            t: times,
            z: None,
        },
        parameters: param_descs,
        ranges,
    }
}

// ─── sync ↔ async bridge ───────────────────────────────────────────────────

fn block_on_async<F, T>(fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(fut)),
        Err(_) => {
            // No current runtime — spin up a temporary one. Used in unit
            // tests outside tokio, never in production where axum always
            // provides one.
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build tokio runtime")
                .block_on(fut)
        }
    }
}

// ─── area-query WKT normalization ──────────────────────────────────────────

/// Accept either a WKT POLYGON directly (pass-through after light trim)
/// or a `west,south,east,north` bbox string (converted to a closed
/// POLYGON). Everything goes into `ST_GeomFromText` as a bind so no
/// request data reaches SQL as text.
fn normalize_area_wkt(coords: &str) -> Result<String, DataServerError> {
    let s = coords.trim();
    if s.starts_with("POLYGON") {
        return Ok(s.to_string());
    }
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() == 4 {
        let vals: Result<Vec<f64>, _> = parts.iter().map(|p| p.trim().parse::<f64>()).collect();
        if let Ok(v) = vals {
            return Ok(format!(
                "POLYGON(({w} {s_},{e} {s_},{e} {n},{w} {n},{w} {s_}))",
                w = v[0],
                s_ = v[1],
                e = v[2],
                n = v[3]
            ));
        }
    }
    Err(DataServerError::InvalidParameter(format!(
        "cannot parse area coordinates: {s}"
    )))
}

// ─── coord parsing (WKT POINT or "lon,lat") ────────────────────────────────

/// `(lon, lat)` of a WKT `POINT` or `lon,lat` pair: the shared ds-core
/// parser, so a direct engine call gets the same finite and range checks
/// as the HTTP boundary (#535). Note the order: ds-core returns
/// `(lat, lon)`.
fn parse_coords(coords: &str) -> Result<(f64, f64), DataServerError> {
    let (lat, lon) = ds_core::feature::parse_point_coords(coords)?;
    Ok((lon, lat))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_coords_wkt_point() {
        assert_eq!(parse_coords("POINT(24.9 60.2)").unwrap(), (24.9, 60.2));
        assert_eq!(parse_coords("POINT (24.9 60.2)").unwrap(), (24.9, 60.2));
    }

    #[test]
    fn parse_coords_comma() {
        assert_eq!(parse_coords("24.9,60.2").unwrap(), (24.9, 60.2));
        assert_eq!(parse_coords(" 24.9 , 60.2 ").unwrap(), (24.9, 60.2));
    }

    #[test]
    fn parse_coords_garbage_rejected() {
        assert!(parse_coords("garbage").is_err());
        assert!(parse_coords("POINT(24.9)").is_err());
    }

    /// Direct engine calls get the HTTP boundary's checks too (#535).
    #[test]
    fn parse_coords_rejects_non_finite_and_out_of_range() {
        for bad in [
            "POINT(NaN 60)",
            "POINT(24.9 inf)",
            "POINT(181 60)",
            "24.9,91",
            "-180.5,0",
        ] {
            assert!(parse_coords(bad).is_err(), "{bad}");
        }
        assert_eq!(parse_coords("POINT(-180 -90)").unwrap(), (-180.0, -90.0));
    }

    #[test]
    fn normalize_area_wkt_passes_polygon_through() {
        let wkt = "POLYGON((0 0,10 0,10 10,0 10,0 0))";
        assert_eq!(normalize_area_wkt(wkt).unwrap(), wkt);
    }

    #[test]
    fn normalize_area_wkt_converts_bbox_to_closed_polygon() {
        let out = normalize_area_wkt("0,0,10,10").unwrap();
        assert!(out.starts_with("POLYGON(("));
        // 5 points: 4 corners + closing repeat.
        assert_eq!(out.matches(',').count(), 4);
    }

    #[test]
    fn normalize_area_wkt_rejects_garbage() {
        assert!(normalize_area_wkt("not-coords").is_err());
        assert!(normalize_area_wkt("1,2,3").is_err()); // wrong arity
    }

    #[test]
    fn resolve_source_keys_expands_none_to_all() {
        use crate::config::ValidatedParameter;
        let cfg = PostgisEngineConfig {
            dsn: "postgres://x/y".into(),
            dsn_was_literal: false,
            pool_size: 4,
            pool_label: None,
            metadata_refresh_secs: 300,
            max_locations: ds_core::config::POSTGIS_DEFAULT_MAX_LOCATIONS,
            location_source: crate::schema::LocationSource::Stations(dummy_stations()),
            observations: dummy_long_obs(),
            parameters: vec![
                ValidatedParameter {
                    name: "t2m".into(),
                    label: "".into(),
                    unit: "°C".into(),
                    observed_property: "air_temperature".into(),
                    source_key: "TEMP".into(),
                },
                ValidatedParameter {
                    name: "ws".into(),
                    label: "".into(),
                    unit: "m/s".into(),
                    observed_property: "wind_speed".into(),
                    source_key: "WIND".into(),
                },
            ],
            locations_window: None,
            events_default_window: None,
            events_extent_bbox: None,
        };
        let keys = resolve_source_keys(&cfg, None).unwrap();
        assert_eq!(keys, vec!["TEMP", "WIND"]);

        let keys = resolve_source_keys(&cfg, Some(&["t2m".into()])).unwrap();
        assert_eq!(keys, vec!["TEMP"]);

        assert!(resolve_source_keys(&cfg, Some(&["unknown".into()])).is_err());
    }

    /// Build a full engine (lazy pool — no DB I/O until a query needs a
    /// connection) around the given config to exercise the EdrEngine trait
    /// surface. The events rejection paths bail before touching the pool.
    fn engine_with(config: PostgisEngineConfig) -> PostgisEngine {
        let mut registry = crate::pool::PoolRegistry::new();
        let pool = registry
            .get_or_create(
                "postgres://user:pw@127.0.0.1:1/db",
                std::num::NonZeroU32::new(2).unwrap(),
            )
            .expect("lazy pool");
        PostgisEngine::new("test", Arc::new(config), pool)
    }

    fn events_engine_config() -> PostgisEngineConfig {
        use crate::config::ValidatedParameter;
        use crate::schema::{EventsShape, QualifiedTable};
        PostgisEngineConfig {
            dsn: "postgres://user:pw@127.0.0.1:1/db".into(),
            dsn_was_literal: false,
            pool_size: 2,
            pool_label: None,
            metadata_refresh_secs: 300,
            max_locations: ds_core::config::POSTGIS_DEFAULT_MAX_LOCATIONS,
            location_source: crate::schema::LocationSource::None,
            observations: ObservationSchema::Events(EventsShape {
                coverage_bbox: None,
                table: QualifiedTable {
                    schema: "public".into(),
                    table: "lightning".into(),
                },
                time_col: "time".into(),
                time_col_tz: Some("UTC".into()),
                geom_col: "the_geom".into(),
                id_col: "id".into(),
                cloud_indicator_col: None,
                peak_current_col: None,
            }),
            parameters: vec![ValidatedParameter {
                name: "peak_current".into(),
                label: "Peak current".into(),
                unit: "kA".into(),
                observed_property: "peak_current".into(),
                source_key: "peak_current".into(),
            }],
            locations_window: None,
            events_default_window: Some(chrono::Duration::hours(1)),
            events_extent_bbox: Some([4.0, 54.0, 42.0, 72.0]),
        }
    }

    #[test]
    fn events_engine_advertises_area_and_radius() {
        let engine = engine_with(events_engine_config());
        assert_eq!(
            engine.supported_query_types(),
            vec!["area".to_string(), "radius".to_string()]
        );
    }

    /// EDR GeoJSON (#929) is offered for station series only: an events
    /// collection answers `Point` coverages with no station to name.
    #[test]
    fn only_station_shapes_serve_station_series() {
        use crate::config::ValidatedParameter;
        let stations = PostgisEngineConfig {
            dsn: "postgres://user:pw@127.0.0.1:1/db".into(),
            dsn_was_literal: false,
            pool_size: 2,
            pool_label: None,
            metadata_refresh_secs: 300,
            max_locations: ds_core::config::POSTGIS_DEFAULT_MAX_LOCATIONS,
            location_source: crate::schema::LocationSource::Stations(dummy_stations()),
            observations: dummy_long_obs(),
            parameters: vec![ValidatedParameter {
                name: "t2m".into(),
                label: "Air temperature".into(),
                unit: "°C".into(),
                observed_property: "air_temperature".into(),
                source_key: "TEMP".into(),
            }],
            locations_window: None,
            events_default_window: None,
            events_extent_bbox: None,
        };
        assert!(engine_with(stations).serves_station_series());
        assert!(!engine_with(events_engine_config()).serves_station_series());
    }

    #[test]
    fn events_engine_rejects_station_queries_with_400() {
        let engine = engine_with(events_engine_config());
        let err = engine
            .query_location("some-station", None, None, None, None)
            .unwrap_err();
        assert!(
            matches!(err, DataServerError::InvalidParameter(ref m) if m.contains("area")),
            "got: {err:?}"
        );
        let err = engine
            .query_position("POINT(25 61)", None, None, None, None)
            .unwrap_err();
        assert!(
            matches!(err, DataServerError::InvalidParameter(ref m) if m.contains("area")),
            "got: {err:?}"
        );
    }

    #[test]
    fn assemble_event_coverages_one_point_per_event() {
        use crate::config::ValidatedParameter;
        use crate::schema::{EventsShape, QualifiedTable};
        let cfg = PostgisEngineConfig {
            dsn: "postgres://x/y".into(),
            dsn_was_literal: false,
            pool_size: 4,
            pool_label: None,
            metadata_refresh_secs: 300,
            max_locations: ds_core::config::POSTGIS_DEFAULT_MAX_LOCATIONS,
            location_source: crate::schema::LocationSource::None,
            observations: ObservationSchema::Events(EventsShape {
                coverage_bbox: None,
                table: QualifiedTable {
                    schema: "public".into(),
                    table: "lightning".into(),
                },
                time_col: "time".into(),
                time_col_tz: Some("UTC".into()),
                geom_col: "the_geom".into(),
                id_col: "id".into(),
                cloud_indicator_col: None,
                peak_current_col: None,
            }),
            parameters: vec![ValidatedParameter {
                name: "peak_current".into(),
                label: "Peak current".into(),
                unit: "kA".into(),
                observed_property: "peak_current".into(),
                source_key: "peak_current".into(),
            }],
            locations_window: None,
            events_default_window: Some(chrono::Duration::hours(1)),
            events_extent_bbox: None,
        };
        let t0: DateTime<Utc> = "2026-07-11T17:00:00Z".parse().unwrap();
        let t1: DateTime<Utc> = "2026-07-11T17:05:00Z".parse().unwrap();
        let events = vec![
            EventRow {
                time: t0,
                lon: 25.0,
                lat: 61.0,
                values: vec![Some(-12.5)],
            },
            EventRow {
                time: t1,
                lon: 26.0,
                lat: 62.0,
                values: vec![None],
            },
        ];

        let out = assemble_event_coverages(&cfg, &["peak_current"], events);
        assert_eq!(out.len(), 2);

        match &out[0].domain {
            DomainDescription::Point { x, y, t, z } => {
                assert_eq!(*x, 25.0);
                assert_eq!(*y, 61.0);
                assert_eq!(*t, Some(t0));
                assert!(z.is_none());
            }
            other => panic!("expected Point domain, got {other:?}"),
        }
        // 0-d scalar ranges: empty shape/axis_names, exactly one value —
        // the CoverageJSON scalar NdArray form the API layer emits.
        let nd = &out[0].ranges["peak_current"];
        assert!(nd.shape.is_empty());
        assert!(nd.axis_names.is_empty());
        assert_eq!(nd.values, vec![Some(-12.5)]);
        assert_eq!(out[0].parameters["peak_current"].unit, "kA");
        // A null measurement stays null, not zero.
        assert_eq!(out[1].ranges["peak_current"].values, vec![None]);
    }

    fn meta_with(locations: Vec<Location>) -> CollectionMeta {
        let stations: Vec<crate::metadata::FeatureStation> = locations
            .iter()
            .map(|l| crate::metadata::FeatureStation {
                id: l.id.clone(),
                label: l.label.clone(),
                lat: l.latitude,
                lon: l.longitude,
                properties: std::sync::Arc::new(HashMap::new()),
            })
            .collect();
        let station_idx: HashMap<String, usize> = locations
            .iter()
            .enumerate()
            .map(|(i, l)| (l.id.clone(), i))
            .collect();
        CollectionMeta {
            filterables: ds_core::feature::property_names(
                stations
                    .iter()
                    .map(|s: &crate::metadata::FeatureStation| s.properties.as_ref()),
            ),
            feature_stations: std::sync::Arc::new(stations),
            locations: std::sync::Arc::new(LocationSnapshot::new(locations)),
            station_idx: std::sync::Arc::new(station_idx),
            parameters: std::sync::Arc::new(HashMap::new()),
            temporal_extent: None,
            spatial_extent: None,
            wms_times: std::sync::Arc::new(Vec::new()),
            locations_cut: None,
            version: 1,
        }
    }

    fn loc(id: &str, lon: f64, lat: f64) -> Location {
        Location {
            id: id.into(),
            label: id.into(),
            latitude: lat,
            longitude: lon,
        }
    }

    /// #671: the observations-derived area path is an exact polygon test,
    /// not the polygon's bbox — and a station exactly on the edge is in.
    #[test]
    fn stations_in_polygon_is_exact_and_boundary_inclusive() {
        let stations = vec![
            loc("inside", 10.5, 50.5),
            loc("bbox-only", 11.9, 51.9), // inside the bbox, outside the triangle
            loc("north-edge", 10.0, 52.0),
            loc("east-edge", 12.0, 50.0),
            loc("outside", 13.0, 53.0),
        ];
        let tri = parse_area_coords("POLYGON((10 50, 12 50, 10 52, 10 50))").unwrap();
        let found = stations_in_polygon(&stations, &tri);
        let ids: Vec<&str> = found.iter().map(|l| l.id.as_str()).collect();
        assert_eq!(ids, vec!["inside", "north-edge", "east-edge"]);
        let rect = parse_area_coords("10,50,12,52").unwrap();
        assert_eq!(stations_in_polygon(&stations, &rect).len(), 4);
    }

    #[test]
    fn station_cap_maps_to_query_too_large_not_engine_error() {
        // The guard sits at 200 000 stations (#1062), far above the old
        // 10 000 ceiling a 20 000-station network used to hit.
        const { assert!(MAX_STATIONS_IN_POLYGON > 20_000) };
        assert!(check_station_count(20_000).is_ok());
        // A full-ceiling batch stays under the limit untouched.
        assert!(check_station_count(MAX_STATIONS_IN_POLYGON - 1).is_ok());

        // A full sentinel batch signals the ceiling was breached →
        // QueryTooLarge (HTTP 400 with the message), never Engine
        // (opaque HTTP 500).
        match check_station_count(MAX_STATIONS_IN_POLYGON).unwrap_err() {
            DataServerError::QueryTooLarge(msg) => {
                assert!(msg.contains("narrow the polygon"), "message: {msg}")
            }
            other => panic!("expected QueryTooLarge, got: {other:?}"),
        }
    }

    /// #1062: a stations view without a unique id lists a station twice.
    /// The area paths read it once, at its first place, so the fan-out
    /// cannot answer it twice where the set-based path answers it once.
    #[test]
    fn area_stations_hold_each_id_once_at_its_first_place() {
        let stations = vec![
            loc("b", 1.0, 1.0),
            loc("a", 2.0, 2.0),
            loc("b", 3.0, 3.0),
            loc("c", 4.0, 4.0),
            loc("a", 5.0, 5.0),
        ];
        let kept: Vec<(String, f64)> = distinct_stations(stations)
            .into_iter()
            .map(|l| (l.id, l.longitude))
            .collect();
        assert_eq!(
            kept,
            vec![("b".into(), 1.0), ("a".into(), 2.0), ("c".into(), 4.0)]
        );

        // Without a repeat the list is untouched.
        let unique = vec![loc("x", 1.0, 1.0), loc("y", 2.0, 2.0)];
        let ids: Vec<String> = distinct_stations(unique)
            .into_iter()
            .map(|l| l.id)
            .collect();
        assert_eq!(ids, vec!["x", "y"]);
    }

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    /// #1062: an instant or a short window runs set-based whatever the
    /// station count; a long window keeps the fan-out while it can run it.
    #[test]
    fn short_windows_run_set_based() {
        let t0 = at("2026-01-01T06:00:00Z");
        let instant = Some((t0, t0));
        let hour = Some((t0, t0 + SET_BASED_MAX_WINDOW));
        let week = Some((t0, t0 + chrono::Duration::days(7)));
        for stations in [1, 50, 20_000, 30_000] {
            for per_station in [1, 2, 8] {
                assert_eq!(
                    area_path(instant, stations, per_station),
                    AreaPath::SetBased
                );
                assert_eq!(area_path(hour, stations, per_station), AreaPath::SetBased);
            }
        }
        // Just past the threshold, the fan-out takes what it can run.
        let longer = Some((t0, t0 + SET_BASED_MAX_WINDOW + chrono::Duration::seconds(1)));
        assert_eq!(area_path(longer, 100, 1), AreaPath::FanOut);
        assert_eq!(area_path(week, 8_000, 2), AreaPath::FanOut);
        assert_eq!(area_path(week, MAX_AREA_QUERIES, 1), AreaPath::FanOut);
    }

    /// #1062: a long window the fan-out cannot run goes set-based, so the
    /// value budget decides; without `datetime` the fan-out keeps its
    /// statement cap.
    #[test]
    fn long_windows_past_the_fan_out_cap_run_set_based() {
        let t0 = at("2026-01-01T06:00:00Z");
        let week = Some((t0, t0 + chrono::Duration::days(7)));
        assert_eq!(area_path(week, MAX_AREA_QUERIES + 1, 1), AreaPath::SetBased);
        assert_eq!(area_path(week, 20_000, 2), AreaPath::SetBased);
        assert_eq!(area_path(week, 2_501, 8), AreaPath::SetBased);
        assert_eq!(area_path(None, 1, 1), AreaPath::FanOut);
        assert_eq!(area_path(None, 30_000, 8), AreaPath::FanOut);
    }

    #[test]
    fn area_query_count_scales_stations_against_parameters() {
        // The parameter count multiplies into the bound: many stations ×
        // few parameters passes, the same stations × many parameters does
        // not — matching "number of parameters is a factor".
        assert!(enforce_area_query_count(8_276, 1).is_ok());
        assert!(enforce_area_query_count(8_276, 2).is_ok());
        match enforce_area_query_count(8_276, 6).unwrap_err() {
            DataServerError::QueryTooLarge(msg) => {
                assert!(msg.contains("49656"), "message should show the math: {msg}");
                assert!(msg.contains("parameter list"), "message: {msg}");
            }
            other => panic!("expected QueryTooLarge, got: {other:?}"),
        }
        // Exact boundary: MAX_AREA_QUERIES itself passes, +1 fails.
        assert!(enforce_area_query_count(MAX_AREA_QUERIES, 1).is_ok());
        assert!(enforce_area_query_count(MAX_AREA_QUERIES + 1, 1).is_err());
        // Overflow-safe.
        assert!(enforce_area_query_count(usize::MAX, 2).is_err());
    }

    #[test]
    fn budget_exceeded_is_query_too_large_with_actionable_message() {
        match budget_exceeded() {
            DataServerError::QueryTooLarge(msg) => {
                assert!(msg.contains("narrow the time range"), "message: {msg}");
            }
            other => panic!("expected QueryTooLarge, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn feature_property_filters_use_cached_properties_before_paging() {
        use ds_core::feature::{FeatureQuery, PropertyValue};
        use ds_core::feature_engine::FeatureEngine;
        let engine = engine_with(events_engine_config());
        let mut meta = meta_with(vec![
            loc("a", 10.0, 60.0),
            loc("b", 20.0, 60.0),
            loc("c", 30.0, 60.0),
        ]);
        for station in Arc::make_mut(&mut meta.feature_stations) {
            station.properties = Arc::new(HashMap::from([(
                "region".into(),
                PropertyValue::String(if station.id == "a" { "west" } else { "east" }.into()),
            )]));
        }
        meta.filterables = ds_core::feature::property_names(
            meta.feature_stations.iter().map(|s| s.properties.as_ref()),
        );
        engine.cache().store(meta);
        assert!(engine.filterables().contains("region"));
        let query = FeatureQuery {
            property_filters: vec![("region".into(), "east".into())],
            limit: 1,
            offset: 1,
            ..Default::default()
        };
        let page = engine.get_features(&query).unwrap();
        assert_eq!(page.number_matched, 2);
        assert_eq!(page.features[0].id, "c");
        assert_eq!(page.next_offset, None);
    }

    /// A stations-only engine whose stations table maps `property_cols`.
    fn stations_engine_config(property_cols: &[&str]) -> PostgisEngineConfig {
        PostgisEngineConfig {
            dsn: "postgres://user:pw@127.0.0.1:1/db".into(),
            dsn_was_literal: false,
            pool_size: 2,
            pool_label: None,
            metadata_refresh_secs: 300,
            max_locations: ds_core::config::POSTGIS_DEFAULT_MAX_LOCATIONS,
            location_source: crate::schema::LocationSource::Stations(
                crate::schema::StationsMapping {
                    property_cols: property_cols.iter().map(|c| (*c).to_string()).collect(),
                    ..dummy_stations()
                },
            ),
            observations: dummy_long_obs(),
            parameters: vec![],
            locations_window: None,
            events_default_window: None,
            events_extent_bbox: None,
        }
    }

    /// What the Features API needs for `?territory=FI` to answer 200 with
    /// zero features: the name is filterable and the engine pages an empty
    /// match set without error.
    fn assert_territory_filter_matches_nothing(engine: &PostgisEngine) {
        use ds_core::feature::FeatureQuery;
        use ds_core::feature_engine::FeatureEngine;
        assert!(
            engine.filterables().contains("territory"),
            "filterables: {:?}",
            engine.filterables()
        );
        let query = FeatureQuery {
            property_filters: vec![("territory".into(), "FI".into())],
            ..Default::default()
        };
        let page = engine.get_features(&query).unwrap();
        assert_eq!(page.number_matched, 0);
        assert_eq!(page.number_returned, 0);
        assert!(page.features.is_empty());
    }

    /// #1024: a configured station property is a valid filter before the
    /// first metadata refresh and after one that failed because the database
    /// was unreachable at boot, not a 400 until the first row arrives.
    #[tokio::test]
    async fn configured_property_filter_survives_a_failed_first_refresh() {
        let engine = engine_with(stations_engine_config(&["territory"]));
        assert_territory_filter_matches_nothing(&engine);

        // Nothing listens on port 1: the refresh fails and the seeded
        // snapshot stays in place.
        let refreshed =
            tokio::time::timeout(Duration::from_secs(30), engine.refresh_metadata()).await;
        assert!(
            matches!(refreshed, Ok(Err(_))),
            "a refresh against port 1 must fail, not succeed or hang"
        );
        assert_eq!(engine.health_snapshot().refresh_failures, 1);
        assert_eq!(engine.cache().load().version, 0);
        assert_territory_filter_matches_nothing(&engine);
    }

    /// #1024: a refresh that reads an empty stations table keeps the
    /// configured property columns filterable.
    #[test]
    fn configured_property_filter_is_valid_on_an_empty_stations_table() {
        let engine = engine_with(stations_engine_config(&["territory"]));
        let refreshed = CollectionMeta::build(
            engine.config(),
            crate::metadata::LocationSet::default(),
            None,
            1,
        );
        engine.cache().store(refreshed);
        assert_eq!(engine.cache().load().version, 1);
        assert_territory_filter_matches_nothing(&engine);
    }

    /// The EDR descriptions of `t2m` (labelled) and `ws` (no label, so the
    /// name stands in), as configured.
    fn assert_configured_parameters_described(engine: &PostgisEngine) {
        let params = engine.get_parameter_descriptions();
        assert_eq!(params.len(), 2, "descriptions: {params:?}");
        let t2m = &params["t2m"];
        assert_eq!(t2m.label, "Air temperature");
        assert_eq!(t2m.unit, "°C");
        assert_eq!(t2m.observed_property, "air_temperature");
        let ws = &params["ws"];
        assert_eq!(ws.label, "ws");
        assert_eq!(ws.unit, "m/s");
        assert_eq!(ws.observed_property, "wind_speed");
    }

    /// #1072: the parameter descriptions come from config alone, so they are
    /// served before the first metadata refresh and after one that failed
    /// because the database was unreachable at boot.
    #[tokio::test]
    async fn configured_parameters_are_described_before_any_refresh() {
        use crate::config::ValidatedParameter;
        let engine = engine_with(PostgisEngineConfig {
            parameters: vec![
                ValidatedParameter {
                    name: "t2m".into(),
                    label: "Air temperature".into(),
                    unit: "°C".into(),
                    observed_property: "air_temperature".into(),
                    source_key: "TEMP".into(),
                },
                ValidatedParameter {
                    name: "ws".into(),
                    label: "".into(),
                    unit: "m/s".into(),
                    observed_property: "wind_speed".into(),
                    source_key: "WIND".into(),
                },
            ],
            ..stations_engine_config(&[])
        });
        assert_configured_parameters_described(&engine);

        // Nothing listens on port 1: the refresh fails and the seeded
        // snapshot stays in place.
        let refreshed =
            tokio::time::timeout(Duration::from_secs(30), engine.refresh_metadata()).await;
        assert!(
            matches!(refreshed, Ok(Err(_))),
            "a refresh against port 1 must fail, not succeed or hang"
        );
        assert_eq!(engine.cache().load().version, 0);
        assert_configured_parameters_described(&engine);
    }

    /// #1072: the events shape's spatial extent is its configured
    /// `extent_bbox`, never read from the database, so it too is served
    /// before the first refresh.
    #[test]
    fn events_extent_is_served_before_any_refresh() {
        let engine = engine_with(events_engine_config());
        assert_eq!(engine.cache().load().version, 0);
        assert_eq!(engine.get_spatial_extent(), Some([4.0, 54.0, 42.0, 72.0]));
        assert_eq!(
            engine.get_parameter_descriptions()["peak_current"].unit,
            "kA"
        );
    }

    // ---- #1061: a station list cut at max_locations ------------------------

    fn station(id: &str) -> crate::metadata::FeatureStation {
        crate::metadata::FeatureStation {
            id: id.into(),
            label: format!("Station {id}"),
            lat: 60.0,
            lon: 25.0,
            properties: Arc::new(HashMap::from([(
                "territory".to_string(),
                ds_core::feature::PropertyValue::String("FI".into()),
            )])),
        }
    }

    fn served_ids(engine: &PostgisEngine) -> Vec<String> {
        use ds_core::feature::FeatureQuery;
        use ds_core::feature_engine::FeatureEngine;
        let edr: Vec<String> = engine
            .get_locations()
            .unwrap()
            .iter()
            .map(|l| l.id.clone())
            .collect();
        let features: Vec<String> = engine
            .get_features(&FeatureQuery::default())
            .unwrap()
            .features
            .into_iter()
            .map(|f| f.id)
            .collect();
        assert_eq!(edr, features, "/locations and Features serve one list");
        assert_eq!(engine.feature_count(), edr.len());
        edr
    }

    /// Stations-only mode: `build_locations` fetches `max_locations + 1`
    /// rows, and `fetch_station_rows` caps them with `of_station_rows`; a
    /// fourth row with a cap of three keeps the first three in query order,
    /// flags the list and degrades `/health`, before the first ping and with
    /// the database reachable.
    #[test]
    fn stations_only_list_past_the_cap_is_truncated_and_degraded() {
        let mut cfg = stations_engine_config(&["territory"]);
        cfg.max_locations = 3;
        let engine = engine_with(cfg);
        assert!(matches!(
            engine.config().location_source,
            crate::schema::LocationSource::Stations(_)
        ));

        let rows = ["a", "b", "c", "d"].map(station).to_vec();
        let set = crate::metadata::LocationSet::of_station_rows(engine.config(), rows);
        engine
            .cache()
            .store(CollectionMeta::build(engine.config(), set, None, 1));
        engine.schema_state.store(SCHEMA_VALID, Ordering::Release);

        assert!(engine.locations_truncated());
        assert_eq!(engine.locations_cut(), Some(LocationsCut::Stations));
        assert_eq!(served_ids(&engine), ["a", "b", "c"]);
        let cut = LiveHealth::LocationsTruncated {
            max_locations: 3,
            cut: LocationsCut::Stations,
        };
        assert_eq!(engine.live_health(), Some(cut));
        engine.health.record_ping(true);
        assert_eq!(engine.live_health(), Some(cut));
        // An unreachable database is the more urgent reason.
        engine.health.record_ping(false);
        assert_eq!(engine.live_health(), Some(LiveHealth::Unreachable));
    }

    /// Stations-only mode at and under the cap: no flag, and `/health`
    /// follows the ping alone.
    #[test]
    fn stations_only_list_within_the_cap_is_complete() {
        let mut cfg = stations_engine_config(&["territory"]);
        cfg.max_locations = 3;
        let engine = engine_with(cfg);
        for rows in [vec!["a", "b"], vec!["a", "b", "c"]] {
            let rows: Vec<_> = rows.into_iter().map(station).collect();
            let n = rows.len();
            let set = crate::metadata::LocationSet::of_station_rows(engine.config(), rows);
            engine
                .cache()
                .store(CollectionMeta::build(engine.config(), set, None, 1));
            engine.schema_state.store(SCHEMA_VALID, Ordering::Release);
            assert!(!engine.locations_truncated());
            assert_eq!(served_ids(&engine).len(), n);
            assert_eq!(
                engine.live_health(),
                None,
                "boot snapshot until the first ping"
            );
        }
        engine.health.record_ping(true);
        assert_eq!(engine.live_health(), Some(LiveHealth::Ready));
    }

    /// The observations-only mode (A), merged by the `merge_table` that
    /// `fetch_observation_rows` runs per table: two per-table result sets,
    /// ids deduplicated across them (first geometry wins), four distinct
    /// reporters against a cap of three. The first three are kept in query
    /// order and the in-memory position query no longer finds the fourth.
    #[test]
    fn observations_only_list_past_the_cap_is_truncated_and_degraded() {
        let mut cfg = stations_engine_config(&[]);
        cfg.location_source = crate::schema::LocationSource::Observations;
        cfg.max_locations = 3;
        let engine = engine_with(cfg);

        let tables = [
            vec![("a", 24.0, 60.0), ("b", 25.0, 61.0)],
            vec![("a", 0.0, 0.0), ("c", 26.0, 62.0), ("d", 27.0, 63.0)],
        ];
        let mut merged = crate::metadata::DerivedLocations::new(engine.config().max_locations);
        let mut stopped = false;
        for rows in &tables {
            let more = merged
                .merge_table(rows, |r| Ok(r.0.to_string()), |r| Ok((r.2, r.1)))
                .unwrap();
            if !more {
                stopped = true;
                break;
            }
        }
        assert!(stopped, "reading stops at the first station past the cap");
        let set = merged.finish();
        engine
            .cache()
            .store(CollectionMeta::build(engine.config(), set, None, 1));
        engine.schema_state.store(SCHEMA_VALID, Ordering::Release);

        assert!(engine.locations_truncated());
        assert_eq!(served_ids(&engine), ["a", "b", "c"]);
        // "a" keeps its first table's geometry.
        let locations = engine.get_locations().unwrap();
        assert_eq!(locations[0].longitude, 24.0);
        assert_eq!(locations.by_id("c").unwrap().latitude, 62.0);
        let meta = engine.load_meta();
        // Requests share the refresh's snapshot, never a copy (#1083).
        assert!(Arc::ptr_eq(&locations, &meta.locations));
        assert_eq!(
            nearest_in_memory(&meta, 26.0, 62.0, 1_000.0).as_deref(),
            Some("c")
        );
        assert_eq!(nearest_in_memory(&meta, 27.0, 63.0, 1_000.0), None);
        engine.health.record_ping(true);
        assert_eq!(
            engine.live_health(),
            Some(LiveHealth::LocationsTruncated {
                max_locations: 3,
                cut: LocationsCut::Stations,
            })
        );
    }

    #[test]
    fn nearest_in_memory_returns_closest_within_radius() {
        let meta = meta_with(vec![
            loc("helsinki", 24.94, 60.17),
            loc("espoo", 24.66, 60.21),
            loc("tampere", 23.76, 61.50),
        ]);
        // A point right next to Helsinki resolves to Helsinki.
        let id = nearest_in_memory(&meta, 24.95, 60.16, 25_000.0).unwrap();
        assert_eq!(id, "helsinki");
    }

    #[test]
    fn nearest_in_memory_none_when_all_outside_radius() {
        let meta = meta_with(vec![loc("tampere", 23.76, 61.50)]);
        // ~250 km away with a 25 km radius → nothing in range.
        assert!(nearest_in_memory(&meta, 24.95, 60.16, 25_000.0).is_none());
    }

    fn dummy_stations() -> crate::schema::StationsMapping {
        crate::schema::StationsMapping {
            table: crate::schema::QualifiedTable {
                schema: "public".into(),
                table: "stations".into(),
            },
            id_col: "id".into(),
            label_col: "name".into(),
            geom_col: "geom".into(),
            property_cols: vec![],
            where_clause: None,
        }
    }

    fn dummy_long_obs() -> crate::schema::ObservationSchema {
        crate::schema::ObservationSchema::Long(crate::schema::LongShape {
            table: crate::schema::QualifiedTable {
                schema: "public".into(),
                table: "obs".into(),
            },
            station_fk_col: "station_id".into(),
            time_col: "time".into(),
            time_col_tz: None,
            param_col: "param".into(),
            value_col: "value".into(),
            geom_col: None,
        })
    }
}
