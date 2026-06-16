//! `PostgisEngine` — implements the `ds_core::edr_engine::EdrEngine` trait on top
//! of a [`deadpool_postgres`] pool and a [`MetadataCache`].
//!
//! DB work is async; the trait is sync. The bridge is
//! `tokio::task::block_in_place(|| Handle::current().block_on(..))` — safe
//! from axum handlers because axum runs on a multi-thread runtime. The
//! engine never calls `block_on` from an async context without first
//! entering `block_in_place`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use deadpool_postgres::Pool;
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::feature::Bbox;
use ds_core::model::{
    CoverageResponse, DomainDescription, Location, NdArray, ParameterDescription, QueryResult,
};
use tokio::sync::watch;
use tokio_postgres::Row;

use crate::config::PostgisEngineConfig;
use crate::health::{Health, HealthSnapshot, HealthStatus};
use crate::metadata::{CollectionMeta, MetadataCache};
use crate::query::{
    build_location, build_position, build_stations_in_polygon, params_as_refs, BuiltQuery,
    DEFAULT_POSITION_RADIUS_M, MAX_OBSERVATION_ROWS, MAX_STATIONS_IN_POLYGON,
};
use crate::schema::ObservationSchema;

/// Time-ordered observation values for a single parameter. Exists to
/// keep the Clippy `type_complexity` lint happy without papering over
/// the intent.
type ParamSeries = Vec<(DateTime<Utc>, Option<f64>)>;

/// `SELECT 1` health-ping cadence — the `/health` reachability probe (#110).
const PING_INTERVAL: Duration = Duration::from_secs(30);
/// Per-ping deadline; a slower response counts as unreachable.
const PING_TIMEOUT: Duration = Duration::from_secs(2);

/// Collection-scoped engine. Cheap to clone (Arcs inside); axum handlers
/// hold it behind `Arc<dyn EdrEngine>` as is done for every engine.
pub struct PostgisEngine {
    collection_id: String,
    config: Arc<PostgisEngineConfig>,
    pool: Arc<Pool>,
    cache: Arc<MetadataCache>,
    /// Live health (DB-reachability) + metrics counters; updated by `poll_loop`.
    health: Health,
    /// `<user>@<host>:<port>/<db>` (or `pool_label`) — the `/metrics` pool label.
    pool_key_label: String,
    /// Stops the background metadata-refresh loop on reload.
    shutdown_tx: watch::Sender<()>,
    /// The version-0 receiver retained from `watch::channel`. `poll_loop` clones
    /// this rather than calling `shutdown_tx.subscribe()` — a fresh `subscribe()`
    /// starts at the channel's *current* version and would miss a `shutdown()`
    /// that fired before the spawned loop began (a rapid-reload race).
    shutdown_rx: watch::Receiver<()>,
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
        let (shutdown_tx, shutdown_rx) = watch::channel(());
        Self {
            collection_id,
            config,
            pool,
            cache: Arc::new(MetadataCache::new_empty()),
            health: Health::new(),
            pool_key_label,
            shutdown_tx,
            shutdown_rx,
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

    /// Authoritative live `/health` status — `None` until the first ping has
    /// run (so the caller keeps the boot snapshot rather than the optimistic
    /// seed), then `Some(Ready/Degraded)`.
    pub fn live_health(&self) -> Option<HealthStatus> {
        self.health.live_status()
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
    /// `/admin/collections/reload` path. Records the duration + outcome for
    /// `/metrics` (a failure does NOT flip `/health` — the ping owns that).
    pub async fn refresh_metadata(&self) -> Result<(), DataServerError> {
        let start = Instant::now();
        let result = self
            .cache
            .refresh(&self.config, &self.pool)
            .await
            .map_err(|e| DataServerError::Engine(format!("metadata refresh failed: {e}")));
        self.health.record_refresh(result.is_ok(), start.elapsed());
        result
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
    /// authority — flips `Ready`/`Degraded` on DB reachability) and a metadata
    /// refresh every `metadata_refresh_secs` (keeps the location list / extents /
    /// `locations_window` set current — a failed refresh keeps the previous
    /// snapshot, so a blip never empties the cache). Spawned on the dedicated
    /// background poll runtime (never a request worker, #221); exits on
    /// `shutdown()`. The metadata-refresh interval skips its first tick (boot
    /// already refreshed), but the ping fires immediately so `/health` is
    /// accurate within ~2 s of boot.
    pub async fn poll_loop(&self) {
        // Clone the retained version-0 receiver — NOT `subscribe()`, which would
        // start at the channel's current version and miss a `shutdown()` that
        // already fired before this loop ran (rapid-reload race).
        let mut shutdown_rx = self.shutdown_rx.clone();
        let mut refresh_iv = tokio::time::interval(Duration::from_secs(
            self.config.metadata_refresh_secs.max(1),
        ));
        refresh_iv.tick().await; // boot already refreshed — skip the immediate tick
        let mut ping_iv = tokio::time::interval(PING_INTERVAL); // first tick fires now

        loop {
            tokio::select! {
                _ = ping_iv.tick() => {
                    self.health.record_ping(self.ping().await);
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
                _ = shutdown_rx.changed() => {
                    tracing::info!(collection = %self.collection_id, "postgis: poll loop shutting down");
                    break;
                }
            }
        }
    }

    /// Signal [`poll_loop`] to stop (called on reload before the engine is
    /// replaced).
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(());
    }

    fn load_meta(&self) -> Arc<CollectionMeta> {
        self.cache.load()
    }
}

// ─── Engine trait ────────────────────────────────────────────────────────────

impl EdrEngine for PostgisEngine {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        Ok((*self.load_meta().locations).clone())
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
        vec![
            "locations".to_string(),
            "position".to_string(),
            "location".to_string(),
            "area".to_string(),
        ]
    }

    fn query_location(
        &self,
        location_id: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        let source_keys = resolve_source_keys(&self.config, parameters)?;
        let key_refs: Vec<&str> = source_keys.iter().map(String::as_str).collect();

        let queries = build_location(&self.config, location_id, datetime, &key_refs)
            .map_err(|e| DataServerError::Engine(format!("build_location: {e}")))?;

        let (lon, lat) = lookup_station_coords(&self.load_meta(), location_id)?;
        let rows_per_query = run_queries_sync(&self.pool, &queries)?;
        Ok(CoverageResponse::Single(assemble_query_result(
            &self.config,
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
            resolve_nearest_station(&self.pool, &self.config, lon, lat)?
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
        let source_keys = resolve_source_keys(&self.config, parameters)?;
        let key_refs: Vec<&str> = source_keys.iter().map(String::as_str).collect();

        // Observations-derived modes (A/B) select stations in-memory from the
        // cached set using the area's bounding box (a superset of exact
        // `ST_Within` — documented v1 simplification). Stations-only mode keeps
        // the live `ST_Within` prefilter.
        let stations = if self.config.location_source.uses_observations() {
            let bbox = area_to_bbox(coords)?;
            let meta = self.load_meta();
            // Stop at the cap + nothing more — mirrors the SQL path's
            // `LIMIT MAX_STATIONS_IN_POLYGON`, so a huge bbox over a dense
            // dataset never materialises an unbounded Vec just to error out.
            meta.locations
                .iter()
                .filter(|l| bbox.contains(l.longitude, l.latitude))
                .take(MAX_STATIONS_IN_POLYGON)
                .cloned()
                .collect::<Vec<Location>>()
        } else {
            let polygon_wkt = normalize_area_wkt(coords)?;
            run_stations_in_polygon_sync(&self.pool, &self.config, &polygon_wkt)?
        };
        if stations.is_empty() {
            return Ok(CoverageResponse::Collection(vec![]));
        }
        if stations.len() >= MAX_STATIONS_IN_POLYGON {
            return Err(DataServerError::Engine(format!(
                "area query hit the {} station cap — narrow the polygon or time range",
                MAX_STATIONS_IN_POLYGON - 1
            )));
        }

        let mut results = Vec::with_capacity(stations.len());
        for station in &stations {
            let queries = build_location(&self.config, &station.id, datetime, &key_refs)
                .map_err(|e| DataServerError::Engine(format!("build_location: {e}")))?;
            let rows_per_query = run_queries_sync(&self.pool, &queries)?;
            // A single-station gap inside an area window is expected
            // (sparse stations, retired sensors, missing parameter for
            // that station). Skip it rather than 404'ing the whole
            // request. Real errors still propagate.
            match assemble_query_result(
                &self.config,
                &station.id,
                station.longitude,
                station.latitude,
                &queries,
                rows_per_query,
            ) {
                Ok(qr) => results.push(qr),
                Err(DataServerError::LocationNotFound(_)) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(CoverageResponse::Collection(results))
    }
}

// ─── helpers ───────────────────────────────────────────────────────────────

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

fn run_queries_sync(pool: &Pool, queries: &[BuiltQuery]) -> Result<Vec<Vec<Row>>, DataServerError> {
    let pool = pool.clone();
    let queries = queries.to_vec();
    block_on_async(async move {
        let client = pool
            .get()
            .await
            .map_err(|e| DataServerError::Engine(format!("pool acquire failed: {e}")))?;
        let mut out = Vec::with_capacity(queries.len());
        for q in &queries {
            let refs = params_as_refs(&q.params);
            let rows = client
                .query(&q.sql, &refs)
                .await
                .map_err(|e| map_pg_error(e, q))?;
            if rows.len() >= MAX_OBSERVATION_ROWS {
                return Err(DataServerError::Engine(format!(
                    "result row count hit cap {MAX_OBSERVATION_ROWS}; narrow bbox or time range"
                )));
            }
            out.push(rows);
        }
        Ok(out)
    })
}

fn run_stations_in_polygon_sync(
    pool: &Pool,
    cfg: &PostgisEngineConfig,
    polygon_wkt: &str,
) -> Result<Vec<Location>, DataServerError> {
    let built = build_stations_in_polygon(cfg, polygon_wkt)
        .map_err(|e| DataServerError::Engine(format!("build_stations_in_polygon: {e}")))?;
    let pool = pool.clone();
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
    pool: &Pool,
    cfg: &PostgisEngineConfig,
    lon: f64,
    lat: f64,
) -> Result<String, DataServerError> {
    let built = build_position(cfg, lon, lat, DEFAULT_POSITION_RADIUS_M)
        .map_err(|e| DataServerError::Engine(format!("build_position: {e}")))?;
    let pool = pool.clone();
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

/// Reduce an EDR area `coords` value (a `west,south,east,north` bbox string or
/// a `POLYGON((...))` WKT, CRS84 lon/lat) to its bounding box. For a true
/// polygon this returns the enclosing box — the documented bbox-superset
/// behavior of the observations-derived area path. Antimeridian-crossing
/// polygons are not special-cased (v1).
fn area_to_bbox(coords: &str) -> Result<Bbox, DataServerError> {
    let s = coords.trim();
    let invalid = |m: String| {
        DataServerError::InvalidParameter(format!("cannot parse area coordinates: {m}"))
    };
    let is_polygon = s
        .get(..7)
        .is_some_and(|p| p.eq_ignore_ascii_case("POLYGON"));
    if !is_polygon {
        let parts: Vec<&str> = s.split(',').collect();
        if parts.len() == 4 {
            if let Ok(v) = parts
                .iter()
                .map(|p| p.trim().parse::<f64>())
                .collect::<Result<Vec<_>, _>>()
            {
                return Bbox::new(v[0], v[1], v[2], v[3])
                    .map_err(|e| DataServerError::InvalidParameter(format!("invalid bbox: {e}")));
            }
        }
        return Err(invalid(s.to_string()));
    }
    // POLYGON WKT: collect every numeric token (lon lat pairs), take min/max.
    let mut nums: Vec<f64> = Vec::new();
    for tok in s.split(|c: char| c == '(' || c == ')' || c == ',' || c.is_whitespace()) {
        let t = tok.trim();
        if t.is_empty() || t.eq_ignore_ascii_case("POLYGON") {
            continue;
        }
        nums.push(
            t.parse::<f64>()
                .map_err(|_| invalid(format!("bad coordinate '{t}'")))?,
        );
    }
    if nums.len() < 8 || !nums.len().is_multiple_of(2) {
        return Err(invalid("polygon needs at least 4 coordinate pairs".into()));
    }
    let (mut w, mut so, mut e, mut n) = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    for pair in nums.chunks_exact(2) {
        let (lon, lat) = (pair[0], pair[1]);
        w = w.min(lon);
        e = e.max(lon);
        so = so.min(lat);
        n = n.max(lat);
    }
    Bbox::new(w, so, e, n)
        .map_err(|e| DataServerError::InvalidParameter(format!("invalid polygon bbox: {e}")))
}

fn map_pg_error(e: tokio_postgres::Error, q: &BuiltQuery) -> DataServerError {
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

fn assemble_query_result(
    cfg: &PostgisEngineConfig,
    station_id: &str,
    lon: f64,
    lat: f64,
    queries: &[BuiltQuery],
    rows_per_query: Vec<Vec<Row>>,
) -> Result<QueryResult, DataServerError> {
    let total_rows: usize = rows_per_query.iter().map(|r| r.len()).sum();
    if total_rows == 0 {
        // Match CsvEngine: station exists but has no data in the queried
        // window ⇒ 404. Better than emitting an empty PointSeries (which
        // fails CoverageJSON schema validation).
        return Err(DataServerError::LocationNotFound(format!(
            "{station_id} (no data in time range)"
        )));
    }
    match &cfg.observations {
        ObservationSchema::Long(_) => {
            let rows = rows_per_query.into_iter().next().unwrap_or_default();
            assemble_long(cfg, station_id, lon, lat, rows)
        }
        ObservationSchema::Wide(_) => {
            let rows = rows_per_query.into_iter().next().unwrap_or_default();
            assemble_wide(cfg, lon, lat, rows)
        }
        ObservationSchema::PerParameter(_) => {
            assemble_per_parameter(cfg, lon, lat, queries, rows_per_query)
        }
    }
}

fn assemble_long(
    cfg: &PostgisEngineConfig,
    _station_id: &str,
    lon: f64,
    lat: f64,
    rows: Vec<Row>,
) -> Result<QueryResult, DataServerError> {
    // Long rows: (time, parameter, value). Group by param, collect times,
    // build per-param NdArray.
    let mut per_param: HashMap<String, ParamSeries> = HashMap::new();
    for row in &rows {
        let t: DateTime<Utc> = row
            .try_get("time")
            .map_err(|e| DataServerError::Engine(format!("decode time: {e}")))?;
        let p: String = row
            .try_get("parameter")
            .map_err(|e| DataServerError::Engine(format!("decode parameter: {e}")))?;
        let v: Option<f64> = row
            .try_get("value")
            .map_err(|e| DataServerError::Engine(format!("decode value: {e}")))?;
        per_param.entry(p).or_default().push((t, v));
    }

    // Union of all times across all parameters, sorted.
    let mut all_times: Vec<DateTime<Utc>> = per_param
        .values()
        .flat_map(|rows| rows.iter().map(|(t, _)| *t))
        .collect();
    all_times.sort_unstable();
    all_times.dedup();

    let domain = DomainDescription::PointSeries {
        x: lon,
        y: lat,
        t: all_times.clone(),
        z: None,
    };

    let mut param_descs = HashMap::new();
    let mut ranges = HashMap::new();
    for (source_key, rows) in per_param {
        let pname = source_key_to_param_name(cfg, &source_key);
        let desc = cfg
            .parameters
            .iter()
            .find(|p| p.name == pname)
            .map(|p| ParameterDescription {
                label: p.label.clone(),
                unit: p.unit.clone(),
                observed_property: p.observed_property.clone(),
            })
            .unwrap_or_else(|| ParameterDescription {
                label: pname.clone(),
                unit: String::new(),
                observed_property: pname.clone(),
            });
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

    Ok(QueryResult {
        domain,
        parameters: param_descs,
        ranges,
    })
}

fn assemble_wide(
    cfg: &PostgisEngineConfig,
    lon: f64,
    lat: f64,
    rows: Vec<Row>,
) -> Result<QueryResult, DataServerError> {
    // Wide rows: (time, <param_alias_1>::float8, <param_alias_2>::float8, ...).
    // The BuiltQuery SELECT projected each requested source_key as an alias
    // named after that source_key — walk `cfg.parameters` in order for the
    // column list, but we only know which were requested via the row's own
    // column schema. Iterate row columns to discover the set.
    // Empty rows are caught upstream by assemble_query_result and turned
    // into LocationNotFound — by the time we get here, `rows` is non-empty.
    let first = rows.first().expect("upstream guarantees non-empty rows");

    let time_col_idx = first
        .columns()
        .iter()
        .position(|c| c.name() == "time")
        .ok_or_else(|| DataServerError::Engine("wide row missing 'time' column".into()))?;

    // Other columns are parameter aliases.
    let param_keys: Vec<String> = first
        .columns()
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            if i == time_col_idx {
                None
            } else {
                Some(c.name().to_string())
            }
        })
        .collect();

    let mut times: Vec<DateTime<Utc>> = Vec::with_capacity(rows.len());
    let mut per_param: HashMap<String, Vec<Option<f64>>> = param_keys
        .iter()
        .map(|k| (k.clone(), Vec::with_capacity(rows.len())))
        .collect();

    for row in &rows {
        let t: DateTime<Utc> = row
            .try_get::<_, DateTime<Utc>>(time_col_idx)
            .map_err(|e| DataServerError::Engine(format!("decode time: {e}")))?;
        times.push(t);
        for key in &param_keys {
            let v: Option<f64> = row
                .try_get(key.as_str())
                .map_err(|e| DataServerError::Engine(format!("decode {key}: {e}")))?;
            per_param.get_mut(key).unwrap().push(v);
        }
    }

    let domain = DomainDescription::PointSeries {
        x: lon,
        y: lat,
        t: times.clone(),
        z: None,
    };

    let mut param_descs = HashMap::new();
    let mut ranges = HashMap::new();
    for key in &param_keys {
        let pname = source_key_to_param_name(cfg, key);
        let desc = cfg
            .parameters
            .iter()
            .find(|p| p.name == pname)
            .map(|p| ParameterDescription {
                label: p.label.clone(),
                unit: p.unit.clone(),
                observed_property: p.observed_property.clone(),
            })
            .unwrap_or_else(|| ParameterDescription {
                label: pname.clone(),
                unit: String::new(),
                observed_property: pname.clone(),
            });
        let values = per_param.remove(key).unwrap();
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

    Ok(QueryResult {
        domain,
        parameters: param_descs,
        ranges,
    })
}

fn assemble_per_parameter(
    cfg: &PostgisEngineConfig,
    lon: f64,
    lat: f64,
    queries: &[BuiltQuery],
    rows_per_query: Vec<Vec<Row>>,
) -> Result<QueryResult, DataServerError> {
    // Each query carries a `parameter` (source_key) tag. Each row is (time,
    // value). Union times across all queries.
    let mut per_param: HashMap<String, ParamSeries> = HashMap::new();
    for (q, rows) in queries.iter().zip(rows_per_query) {
        let Some(source_key) = q.parameter.as_ref() else {
            return Err(DataServerError::Engine(
                "per_parameter query missing parameter tag".into(),
            ));
        };
        let mut entries = Vec::with_capacity(rows.len());
        for row in &rows {
            let t: DateTime<Utc> = row
                .try_get("time")
                .map_err(|e| DataServerError::Engine(format!("decode time: {e}")))?;
            let v: Option<f64> = row
                .try_get("value")
                .map_err(|e| DataServerError::Engine(format!("decode value: {e}")))?;
            entries.push((t, v));
        }
        per_param.insert(source_key.clone(), entries);
    }

    let mut all_times: Vec<DateTime<Utc>> = per_param
        .values()
        .flat_map(|rows| rows.iter().map(|(t, _)| *t))
        .collect();
    all_times.sort_unstable();
    all_times.dedup();

    // Same invariant as wide: empty rows turned into LocationNotFound upstream.
    debug_assert!(!all_times.is_empty(), "upstream guarantees non-empty rows");

    let domain = DomainDescription::PointSeries {
        x: lon,
        y: lat,
        t: all_times.clone(),
        z: None,
    };

    let mut param_descs = HashMap::new();
    let mut ranges = HashMap::new();
    for (source_key, rows) in per_param {
        let pname = source_key_to_param_name(cfg, &source_key);
        let desc = cfg
            .parameters
            .iter()
            .find(|p| p.name == pname)
            .map(|p| ParameterDescription {
                label: p.label.clone(),
                unit: p.unit.clone(),
                observed_property: p.observed_property.clone(),
            })
            .unwrap_or_else(|| ParameterDescription {
                label: pname.clone(),
                unit: String::new(),
                observed_property: pname.clone(),
            });
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

    Ok(QueryResult {
        domain,
        parameters: param_descs,
        ranges,
    })
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

fn parse_coords(coords: &str) -> Result<(f64, f64), DataServerError> {
    let coords = coords.trim();
    if let Some(inner) = coords
        .strip_prefix("POINT(")
        .or_else(|| coords.strip_prefix("POINT ("))
        .and_then(|s| s.strip_suffix(')'))
    {
        let parts: Vec<&str> = inner.split_whitespace().collect();
        if parts.len() == 2 {
            let lon: f64 = parts[0].parse().map_err(|_| {
                DataServerError::InvalidParameter(format!("invalid longitude: {}", parts[0]))
            })?;
            let lat: f64 = parts[1].parse().map_err(|_| {
                DataServerError::InvalidParameter(format!("invalid latitude: {}", parts[1]))
            })?;
            return Ok((lon, lat));
        }
    }
    let parts: Vec<&str> = coords.split(',').collect();
    if parts.len() == 2 {
        let lon: f64 = parts[0].trim().parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("invalid longitude: {}", parts[0]))
        })?;
        let lat: f64 = parts[1].trim().parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("invalid latitude: {}", parts[1]))
        })?;
        return Ok((lon, lat));
    }
    Err(DataServerError::InvalidParameter(format!(
        "cannot parse coordinates: {coords}"
    )))
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
        };
        let keys = resolve_source_keys(&cfg, None).unwrap();
        assert_eq!(keys, vec!["TEMP", "WIND"]);

        let keys = resolve_source_keys(&cfg, Some(&["t2m".into()])).unwrap();
        assert_eq!(keys, vec!["TEMP"]);

        assert!(resolve_source_keys(&cfg, Some(&["unknown".into()])).is_err());
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
            feature_stations: std::sync::Arc::new(stations),
            locations: std::sync::Arc::new(locations),
            station_idx: std::sync::Arc::new(station_idx),
            parameters: std::sync::Arc::new(HashMap::new()),
            temporal_extent: None,
            spatial_extent: None,
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

    #[test]
    fn area_to_bbox_parses_bbox_string() {
        let b = area_to_bbox("10,40,30,50").unwrap();
        assert_eq!((b.west, b.south, b.east, b.north), (10.0, 40.0, 30.0, 50.0));
        assert!(b.contains(20.0, 45.0));
        assert!(!b.contains(35.0, 45.0));
    }

    #[test]
    fn area_to_bbox_reduces_polygon_to_bounding_box() {
        // Triangle — bbox is its enclosing rectangle.
        let b = area_to_bbox("POLYGON((0 0,10 0,5 8,0 0))").unwrap();
        assert_eq!((b.west, b.south, b.east, b.north), (0.0, 0.0, 10.0, 8.0));
        assert!(b.contains(5.0, 4.0));
        assert!(!b.contains(20.0, 4.0));
        // Case-insensitive WKT keyword.
        let lc = area_to_bbox("polygon((0 0,10 0,5 8,0 0))").unwrap();
        assert_eq!(
            (lc.west, lc.south, lc.east, lc.north),
            (0.0, 0.0, 10.0, 8.0)
        );
    }

    #[test]
    fn area_to_bbox_rejects_garbage() {
        assert!(area_to_bbox("nonsense").is_err());
        assert!(area_to_bbox("1,2,3").is_err());
        assert!(area_to_bbox("POLYGON((0 0,1 1))").is_err()); // <4 pairs
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
