//! Parameterized SQL builders.
//!
//! Every emitted SELECT ends with a hard row cap (a caller-supplied
//! `LIMIT $N` bind for observations — see [`MAX_RESPONSE_VALUES`] for the
//! budget model — `LIMIT 50001` for locations, `LIMIT 10001` for the
//! stations-in-polygon prefilter, `LIMIT 1` for the nearest-station
//! position query). Every identifier goes through [`quote_ident`]; every
//! value is bound as `$1..$n`. The builders never interpolate request
//! data into SQL — only config-derived identifiers from the validated
//! [`PostgisEngineConfig`] are inlined, and those have already passed the
//! whitelist regex at config load.
//!
//! Builders return a [`BuiltQuery`] that carries the SQL text, the bound
//! parameters as a testable [`SqlParam`] enum, and an optional
//! `parameter` hint the caller uses to route per-query result streams
//! back into the right domain-model coverage ranges (important for the
//! `per_parameter` shape, where a single EDR location query fans out into
//! one DB query per requested parameter).

use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::config::PostgisEngineConfig;
use crate::schema::{
    EventsShape, LongShape, ObservationSchema, PerParameterShape, QualifiedTable, WideShape,
};
use crate::security::{quote_ident, QuoteError};

/// Maximum rows returned by a locations list query. 50_001 covers the
/// real-world deployment range (FMI ~10k stations, MET Office ~5k, NOAA
/// COOP ~30k). Operators with larger networks should narrow via
/// `stations.where`. The 50_001th row signals the cap is breached so the
/// engine can warn — see #110 metadata refresh status.
pub const MAX_LOCATIONS: usize = 50_001;

/// Per-query row cap for one station×parameter query inside an **area**
/// fan-out. Area queries run concurrently, so each in-flight query needs
/// its own bound to keep the transient row buffer small (worst case ≈
/// fan-out width × this). Single-station paths (position/location) are
/// NOT bound by this — their LIMIT comes from the remaining
/// [`MAX_RESPONSE_VALUES`] budget, which is what allows long time series
/// for one station.
pub const MAX_OBSERVATION_ROWS: usize = 10001;

/// Total response budget: observation values (station × parameter ×
/// timestep cells) one request may return, across all fan-out queries.
/// This is THE cap that matters — it scales each request dimension
/// against the others, so "many stations × one timestep" and "one
/// station × a year of 10-min data" both fit, while "everything ×
/// everything" is rejected with the numbers. ~500k values ≈ 20–35 MB of
/// CoverageJSON. Breach ⇒ `QueryTooLarge` ⇒ HTTP 400.
pub const MAX_RESPONSE_VALUES: usize = 500_000;

/// Sanity ceiling on stations matched by an area polygon (the prefilter
/// fetches one extra row to detect the breach). NOT the real gate — the
/// response budget and the query-count bound are — this only bounds the
/// prefilter buffer and the per-request station Vec. Live `fmi-obs`
/// advertises ~8.3k stations, so a whole-extent polygon fits.
pub const MAX_STATIONS_IN_POLYGON: usize = 10_001;

/// Ceiling on SQL queries one area request may fan out (stations ×
/// requested parameters for the `per_parameter` shape; stations × 1 for
/// `long`/`wide`). Bounds total DB work independently of row counts —
/// 8k stations × 1 parameter is fine, 8k × 6 parameters is not; narrow
/// `parameter-name` instead.
pub const MAX_AREA_QUERIES: usize = 20_000;

/// Default search radius for nearest-station position queries (25 km).
pub const DEFAULT_POSITION_RADIUS_M: f64 = 25_000.0;

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("identifier error: {0}")]
    Identifier(#[from] QuoteError),
    #[error("unknown parameter '{0}' — not in the validated config parameter list")]
    UnknownParameter(String),
    #[error("polygon WKT must not be empty")]
    EmptyPolygonWkt,
    #[error("radius must be positive, got {0}")]
    InvalidRadius(f64),
    #[error("this query requires a stations table, but the collection has none")]
    NoStations,
    #[error("observations-derived locations require an observations geometry column")]
    NoObservationGeom,
}

/// Typed bind parameter. Kept as a small sum so unit tests can assert
/// value-level correctness without a live DB; converted to
/// `tokio_postgres::types::ToSql` at execution time by the engine layer.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlParam {
    Text(String),
    Float(f64),
    /// `LIMIT $N` binds (Postgres treats a bound LIMIT as bigint).
    Int(i64),
    Timestamp(DateTime<Utc>),
    TextArray(Vec<String>),
}

impl SqlParam {
    /// View the parameter as a tokio-postgres `ToSql` reference.
    /// Engine-layer callers collect these into a `Vec<&(dyn ToSql + Sync)>`
    /// for `client.query(&sql, &params)`.
    pub fn as_sql(&self) -> &(dyn tokio_postgres::types::ToSql + Sync) {
        match self {
            SqlParam::Text(s) => s,
            SqlParam::Float(f) => f,
            SqlParam::Int(i) => i,
            SqlParam::Timestamp(t) => t,
            SqlParam::TextArray(v) => v,
        }
    }
}

/// Convenience for the engine layer: turn `Vec<SqlParam>` into a slice of
/// `&(dyn ToSql + Sync)` refs suitable for `query(&sql, &params)`.
pub fn params_as_refs(params: &[SqlParam]) -> Vec<&(dyn tokio_postgres::types::ToSql + Sync)> {
    params.iter().map(|p| p.as_sql()).collect()
}

/// A finished, bind-safe query.
///
/// `parameter` is `Some` for per_parameter shape queries so the caller
/// can multiplex multiple queries' rows into the right coverage keys.
#[derive(Debug, Clone, PartialEq)]
pub struct BuiltQuery {
    pub sql: String,
    pub params: Vec<SqlParam>,
    pub parameter: Option<String>,
    /// Observation values one returned row contributes to the response
    /// budget: 1 for `long`/`per_parameter` (one value per row), the
    /// requested-parameter count for `wide` (one row carries a value per
    /// selected column).
    pub values_per_row: usize,
    /// Index into `params` of the `LIMIT $N` bind, when the query has a
    /// caller-adjustable row limit. The engine layer rewrites this bind to
    /// the remaining response budget before executing (sequential
    /// single-station paths), or leaves the builder's value (area fan-out).
    pub limit_param_idx: Option<usize>,
}

impl BuiltQuery {
    fn new(sql: String, params: Vec<SqlParam>) -> Self {
        Self {
            sql,
            params,
            parameter: None,
            values_per_row: 1,
            limit_param_idx: None,
        }
    }

    fn with_parameter(mut self, parameter: impl Into<String>) -> Self {
        self.parameter = Some(parameter.into());
        self
    }

    /// Record which bind is the LIMIT and how rows weigh against the
    /// response budget.
    fn with_row_limit(mut self, limit_param_idx: usize, values_per_row: usize) -> Self {
        self.limit_param_idx = Some(limit_param_idx);
        self.values_per_row = values_per_row;
        self
    }

    /// The LIMIT bind's current row value (`None` when the query has no
    /// adjustable limit).
    pub fn row_limit(&self) -> Option<usize> {
        let idx = self.limit_param_idx?;
        match self.params.get(idx) {
            Some(SqlParam::Int(v)) => Some(*v as usize),
            _ => None,
        }
    }

    /// Rewrite the LIMIT bind. No-op for queries without one.
    pub fn set_row_limit(&mut self, rows: usize) {
        if let Some(idx) = self.limit_param_idx {
            self.params[idx] = SqlParam::Int(rows as i64);
        }
    }
}

// ─── locations ──────────────────────────────────────────────────────────────

/// List all stations advertised by the collection.
///
/// No bind parameters — the optional `where_clause` is a config-time
/// constant the config loader already validated as a non-empty string
/// (identifier whitelist is not applied to the WHERE body; see plan doc
/// B3 "only pre-validated `stations.where`").
pub fn build_locations(cfg: &PostgisEngineConfig) -> Result<BuiltQuery, BuildError> {
    let s = cfg
        .location_source
        .stations()
        .ok_or(BuildError::NoStations)?;
    let id = quote_ident(&s.id_col)?;
    let label = quote_ident(&s.label_col)?;
    let geom = quote_ident(&s.geom_col)?;
    let table = fq_table(&s.table)?;

    let mut props = String::new();
    for col in &s.property_cols {
        props.push_str(", ");
        props.push_str(&quote_ident(col)?);
    }

    let mut sql = String::from("SELECT ");
    sql.push_str(&format!(
        "{id}::text AS id, \
         {label} AS label, \
         ST_Y({geom}) AS lat, \
         ST_X({geom}) AS lon{props} \
         FROM {table}"
    ));
    // SAFETY: `where_clause` is inlined verbatim — it CANNOT be
    // parameterised with `$N` because its role is to contribute SQL
    // fragments (identifiers, operators), not values. The string is
    // validated at config load time by
    // `ds_core::config::validate_stations_where_clause`, which rejects
    // `;`, comments (`--`/`/*`/`*/`), and whole-word DML/DDL/exfil
    // verbs (`drop`/`delete`/`update`/`insert`/`truncate`/`alter`/
    // `create`/`grant`/`revoke`/`copy`/`union`/`execute`/`call`/
    // `perform`/`select`/`from`). The CI tripwire
    // `scripts/check_sql_safety.sh` does NOT see this call site
    // (`push_str(&format!(...))` with no verb in the template). For any
    // filter logic beyond simple `col OP value AND col OP value`,
    // create a Postgres VIEW and point `stations.table` at it —
    // that's the documented extension path.
    if let Some(w) = s.where_clause.as_deref() {
        if !w.trim().is_empty() {
            sql.push_str(&format!(" WHERE {w}"));
        }
    }
    sql.push_str(&format!(" ORDER BY {id} LIMIT {MAX_LOCATIONS}"));

    Ok(BuiltQuery::new(sql, vec![]))
}

/// Derive the location list from the observations table(s)' own geometry —
/// used when there is no stations table (mode A) or to fill orphan stations
/// (mode B). Returns **one query per observation table**, each yielding one row
/// per distinct `station_fk` with its coords; the caller runs them, dedups by id
/// across tables (first wins), and assembles `label = id` + empty properties
/// (orphans carry no station metadata).
///
/// One query per table — NOT a single `UNION` — is load-bearing: each per-table
/// `DISTINCT ON` scan is ~seconds over a large hypertable, and a read-only role
/// commonly caps `statement_timeout` (nexus `meteocore_ro` = 5s), which a 6-table
/// union blows in one statement. Splitting keeps every statement small; the
/// cross-table dedup the `UNION`'s outer `DISTINCT ON (id)` used to do now happens
/// in the caller (`fetch_observation_rows`).
///
/// `DISTINCT ON (<fk>)` picks one geometry per station — stations are assumed
/// spatially stable; if a station's geometry ever drifts, the first row by `<fk>`
/// wins. `WHERE <geom> IS NOT NULL` drops rows that cannot be placed. When
/// `since` is `Some`, an `AND <time_col> >= <since>` filter restricts each scan
/// to recent hypertable chunks (the default — keeps `DISTINCT ON` cheap on huge
/// tables and advertises only currently-reporting stations); `None` scans full
/// history (`observations.locations_window = "all"`). Each query is capped at
/// [`MAX_LOCATIONS`].
pub fn build_locations_from_observations(
    cfg: &PostgisEngineConfig,
    since: Option<DateTime<Utc>>,
) -> Result<Vec<BuiltQuery>, BuildError> {
    match &cfg.observations {
        ObservationSchema::Long(l) => Ok(vec![obs_locations_query(
            &l.table,
            &l.station_fk_col,
            l.geom_col.as_deref().ok_or(BuildError::NoObservationGeom)?,
            &l.time_col,
            l.time_col_tz.as_deref(),
            since,
        )?]),
        ObservationSchema::Wide(w) => Ok(vec![obs_locations_query(
            &w.table,
            &w.station_fk_col,
            w.geom_col.as_deref().ok_or(BuildError::NoObservationGeom)?,
            &w.time_col,
            w.time_col_tz.as_deref(),
            since,
        )?]),
        ObservationSchema::PerParameter(pp) => {
            let mut queries = Vec::with_capacity(pp.tables.len());
            for t in &pp.tables {
                let geom = t.geom_col.as_deref().ok_or(BuildError::NoObservationGeom)?;
                queries.push(obs_locations_query(
                    &t.table,
                    &t.station_fk_col,
                    geom,
                    &t.time_col,
                    t.time_col_tz.as_deref(),
                    since,
                )?);
            }
            Ok(queries)
        }
        // Events have no station concept — nothing to derive.
        ObservationSchema::Events(_) => Err(BuildError::NoStations),
    }
}

/// One standalone observations-derived `(id, lat, lon)` query for a single table,
/// with an optional recent-window time filter, capped at [`MAX_LOCATIONS`].
fn obs_locations_query(
    table: &QualifiedTable,
    station_fk_col: &str,
    geom_col: &str,
    time_col: &str,
    time_col_tz: Option<&str>,
    since: Option<DateTime<Utc>>,
) -> Result<BuiltQuery, BuildError> {
    let table_fq = fq_table(table)?;
    let fk = quote_ident(station_fk_col)?;
    let geom = quote_ident(geom_col)?;
    let mut params: Vec<SqlParam> = Vec::new();
    // SELECT kept in a plain literal; the format! templates below interpolate
    // only already-quoted identifiers and contain no flagged SQL verb.
    let mut sql = String::from("SELECT ");
    sql.push_str(&format!(
        "DISTINCT ON ({fk}) {fk}::text AS id, \
         ST_Y({geom}) AS lat, \
         ST_X({geom}) AS lon \
         FROM {table_fq} \
         WHERE {geom} IS NOT NULL"
    ));
    // Recent-window filter: only stations seen since `since`. Restricts the scan
    // to recent hypertable chunks (chunk exclusion) so the DISTINCT ON stays
    // cheap. We wrap the *bind* (not the column) for naive-UTC columns — see
    // `time_filter_rhs` — so the column index/chunk-pruning stays usable.
    if let Some(ts) = since {
        let time = quote_ident(time_col)?;
        let rhs = time_filter_rhs("$1", time_col_tz);
        sql.push_str(&format!(" AND {time} >= {rhs}"));
        params.push(SqlParam::Timestamp(ts));
    }
    sql.push_str(&format!(" ORDER BY {fk} LIMIT {MAX_LOCATIONS}"));
    Ok(BuiltQuery::new(sql, params))
}

// ─── position (nearest station) ────────────────────────────────────────────

/// Nearest station within `radius_m` metres of `(lon, lat)`. Used to turn
/// an EDR position query into a station-keyed location query.
pub fn build_position(
    cfg: &PostgisEngineConfig,
    lon: f64,
    lat: f64,
    radius_m: f64,
) -> Result<BuiltQuery, BuildError> {
    if !radius_m.is_finite() || radius_m <= 0.0 {
        return Err(BuildError::InvalidRadius(radius_m));
    }
    let s = cfg
        .location_source
        .stations()
        .ok_or(BuildError::NoStations)?;
    let id = quote_ident(&s.id_col)?;
    let label = quote_ident(&s.label_col)?;
    let geom = quote_ident(&s.geom_col)?;
    let table = fq_table(&s.table)?;

    let mut props = String::new();
    for col in &s.property_cols {
        props.push_str(", ");
        props.push_str(&quote_ident(col)?);
    }

    let mut sql = String::from("SELECT ");
    sql.push_str(&format!(
        "{id}::text AS id, \
         {label} AS label, \
         ST_Y({geom}) AS lat, \
         ST_X({geom}) AS lon{props}, \
         ST_Distance({geom}::geography, ST_SetSRID(ST_MakePoint($1, $2), 4326)::geography) AS dist_m \
         FROM {table}"
    ));
    sql.push_str(&format!(
        " WHERE ST_DWithin({geom}::geography, ST_SetSRID(ST_MakePoint($1, $2), 4326)::geography, $3)"
    ));
    // SAFETY: see validate_stations_where_clause.
    if let Some(w) = s.where_clause.as_deref() {
        if !w.trim().is_empty() {
            sql.push_str(&format!(" AND ({w})"));
        }
    }
    sql.push_str(" ORDER BY dist_m LIMIT 1");

    Ok(BuiltQuery::new(
        sql,
        vec![
            SqlParam::Float(lon),
            SqlParam::Float(lat),
            SqlParam::Float(radius_m),
        ],
    ))
}

// ─── locations inside polygon (area prefilter) ─────────────────────────────

/// Prefilter stations by bbox + exact `ST_Within`. Returned stations
/// become the fan-out set for `build_location` — the caller iterates and
/// runs one observation query per station so the per-query `LIMIT 10001`
/// cap protects each station independently.
pub fn build_stations_in_polygon(
    cfg: &PostgisEngineConfig,
    polygon_wkt: &str,
) -> Result<BuiltQuery, BuildError> {
    if polygon_wkt.trim().is_empty() {
        return Err(BuildError::EmptyPolygonWkt);
    }
    let s = cfg
        .location_source
        .stations()
        .ok_or(BuildError::NoStations)?;
    let id = quote_ident(&s.id_col)?;
    let label = quote_ident(&s.label_col)?;
    let geom = quote_ident(&s.geom_col)?;
    let table = fq_table(&s.table)?;

    let mut props = String::new();
    for col in &s.property_cols {
        props.push_str(", ");
        props.push_str(&quote_ident(col)?);
    }

    let mut sql = String::from("SELECT ");
    sql.push_str(&format!(
        "{id}::text AS id, \
         {label} AS label, \
         ST_Y({geom}) AS lat, \
         ST_X({geom}) AS lon{props} \
         FROM {table} \
         WHERE {geom} && ST_GeomFromText($1, 4326) \
         AND ST_Within({geom}, ST_GeomFromText($1, 4326))"
    ));
    // SAFETY: see validate_stations_where_clause.
    if let Some(w) = s.where_clause.as_deref() {
        if !w.trim().is_empty() {
            sql.push_str(&format!(" AND ({w})"));
        }
    }
    sql.push_str(&format!(" ORDER BY {id} LIMIT {MAX_STATIONS_IN_POLYGON}"));

    Ok(BuiltQuery::new(
        sql,
        vec![SqlParam::Text(polygon_wkt.to_string())],
    ))
}

// ─── observation queries (shape-aware) ─────────────────────────────────────

/// Build observation queries for a single station.
///
/// Returns `Vec<BuiltQuery>` because the `per_parameter` shape fans out
/// into one query per requested parameter. For `long` and `wide` shapes
/// the result is a single-element vector.
///
/// `source_keys` are the *engine-internal* source_keys (as resolved in
/// `PostgisEngineConfig::parameters`), not the advertised parameter names.
/// When empty, the caller wants all configured parameters — the builder
/// expands it to every parameter in the mapping.
///
/// `row_limit` is the initial `LIMIT $N` bind on every emitted query
/// (rows, not values). The engine layer picks it per path: area fan-out
/// queries keep [`MAX_OBSERVATION_ROWS`]; sequential single-station paths
/// rewrite the bind to the remaining [`MAX_RESPONSE_VALUES`] budget via
/// [`BuiltQuery::set_row_limit`] before each execution.
pub fn build_location(
    cfg: &PostgisEngineConfig,
    station_id: &str,
    time_range: Option<(DateTime<Utc>, DateTime<Utc>)>,
    source_keys: &[&str],
    row_limit: usize,
) -> Result<Vec<BuiltQuery>, BuildError> {
    let effective: Vec<&str> = if source_keys.is_empty() {
        cfg.parameters
            .iter()
            .map(|p| p.source_key.as_str())
            .collect()
    } else {
        source_keys.to_vec()
    };

    match &cfg.observations {
        ObservationSchema::Long(l) => {
            let q = build_location_long(l, station_id, time_range, &effective, row_limit)?;
            Ok(vec![q])
        }
        ObservationSchema::Wide(w) => {
            let q = build_location_wide(w, station_id, time_range, &effective, row_limit)?;
            Ok(vec![q])
        }
        ObservationSchema::PerParameter(pp) => {
            build_location_per_parameter(pp, station_id, time_range, &effective, row_limit)
        }
        // Events have no stations; the engine layer rejects station-keyed
        // queries before reaching the builder.
        ObservationSchema::Events(_) => Err(BuildError::NoStations),
    }
}

// ─── events (non-station event tables) ─────────────────────────────────────

/// Events-shape area query (#113): one statement fetching every event inside
/// the polygon × time window. Each returned row is one event —
/// `(time, lon, lat, <one column per requested source_key>)` — so
/// `values_per_row` is the requested-column count for the response budget.
/// `ORDER BY time DESC, <id>` keeps results deterministic (newest first, id
/// tiebreak on equal timestamps); `LIMIT $4` is the budget in rows.
///
/// The `::double precision` cast on every parameter column also normalises
/// `numeric`-typed columns (e.g. a `numeric(3,1)` accuracy radius) to `f64`
/// at the SQL layer.
pub fn build_events_area(
    shape: &EventsShape,
    polygon_wkt: &str,
    time_range: (DateTime<Utc>, DateTime<Utc>),
    source_keys: &[&str],
    row_limit: usize,
) -> Result<BuiltQuery, BuildError> {
    if polygon_wkt.trim().is_empty() {
        return Err(BuildError::EmptyPolygonWkt);
    }
    let table = fq_table(&shape.table)?;
    let time_col = quote_ident(&shape.time_col)?;
    let geom = quote_ident(&shape.geom_col)?;
    let id = quote_ident(&shape.id_col)?;
    let time_select = time_select_expr(&time_col, shape.time_col_tz.as_deref());
    let tz = shape.time_col_tz.as_deref();

    let mut projection = format!("{time_select} AS time, ST_X({geom}) AS lon, ST_Y({geom}) AS lat");
    for key in source_keys {
        // source_key == column name == row alias (identifier-validated at
        // config load and at engine resolve).
        let col = quote_ident(key)?;
        projection.push_str(&format!(", {col}::double precision AS {col}"));
    }

    let rhs_lo = time_filter_rhs("$1", tz);
    let rhs_hi = time_filter_rhs("$2", tz);
    let mut sql = String::from("SELECT ");
    sql.push_str(&format!(
        "{projection} \
         FROM {table} \
         WHERE {time_col} >= {rhs_lo} AND {time_col} <= {rhs_hi} \
         AND {geom} && ST_GeomFromText($3, 4326) \
         AND ST_Intersects({geom}, ST_GeomFromText($3, 4326)) \
         ORDER BY {time_col} DESC, {id} LIMIT $4"
    ));
    let params = vec![
        SqlParam::Timestamp(time_range.0),
        SqlParam::Timestamp(time_range.1),
        SqlParam::Text(polygon_wkt.to_string()),
        SqlParam::Int(row_limit as i64),
    ];
    Ok(BuiltQuery::new(sql, params).with_row_limit(3, source_keys.len().max(1)))
}

/// Whether an events window selects the optional per-event attribute columns
/// (#616). Only the `EventSource` join reads them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowAttrs {
    Include,
    Omit,
}

/// Events-shape strike fetch for the map layer (#504): every event in the
/// half-open window `(start, end]` across the WHOLE table extent — the map
/// path caches one window per timestep and filters per tile in memory, so
/// there is no spatial predicate here. `ORDER BY time DESC, id` so a
/// truncated fetch keeps the NEWEST strikes (the map-relevant ones); the
/// caller reverses to ascending for paint order.
///
/// `attrs` decides whether the optional per-event attribute columns are
/// selected. The two callers want opposite things from the same window: the
/// map splat needs only time + coords (`Omit`), while the `EventSource` join
/// needs the attributes (`Include`). Sharing the builder without the switch
/// would make the per-frame map query pay for three columns it never reads.
pub fn build_events_window(
    shape: &EventsShape,
    time_range: (DateTime<Utc>, DateTime<Utc>),
    row_limit: usize,
    attrs: WindowAttrs,
) -> Result<BuiltQuery, BuildError> {
    let table = fq_table(&shape.table)?;
    let time_col = quote_ident(&shape.time_col)?;
    let geom = quote_ident(&shape.geom_col)?;
    let id = quote_ident(&shape.id_col)?;
    let time_select = time_select_expr(&time_col, shape.time_col_tz.as_deref());
    let tz = shape.time_col_tz.as_deref();

    let rhs_lo = time_filter_rhs("$1", tz);
    let rhs_hi = time_filter_rhs("$2", tz);
    // Optional attribute columns (#616). Aliased to fixed names so the
    // decoder never has to know the operator's column naming; every
    // identifier goes through quote_ident (Critical Rule 8).
    //
    // Cast to `double precision` for the same reason the EDR parameter path
    // does: these columns are `smallint` in one deployment and `numeric` or
    // `real` in the next. Without the cast a typed decode of the wrong width
    // returns None, and "this network doesn't report polarity" is exactly the
    // answer a misconfiguration would forge. One numeric type in, one decode.
    let mut attr_cols = String::new();
    if attrs == WindowAttrs::Include {
        for (col, alias) in [
            (&shape.cloud_indicator_col, "cloud_indicator"),
            (&shape.peak_current_col, "peak_current"),
            (&shape.multiplicity_col, "multiplicity"),
        ] {
            if let Some(name) = col {
                attr_cols.push_str(&format!(
                    ", {}::double precision AS {alias}",
                    quote_ident(name)?
                ));
            }
        }
    }
    let mut sql = String::from("SELECT ");
    sql.push_str(&format!(
        "{time_select} AS time, ST_X({geom}) AS lon, ST_Y({geom}) AS lat{attr_cols} \
         FROM {table} \
         WHERE {time_col} > {rhs_lo} AND {time_col} <= {rhs_hi} \
         ORDER BY {time_col} DESC, {id} LIMIT $3"
    ));
    let params = vec![
        SqlParam::Timestamp(time_range.0),
        SqlParam::Timestamp(time_range.1),
        SqlParam::Int(row_limit as i64),
    ];
    Ok(BuiltQuery::new(sql, params).with_row_limit(2, 1))
}

fn build_location_long(
    shape: &LongShape,
    station_id: &str,
    time_range: Option<(DateTime<Utc>, DateTime<Utc>)>,
    source_keys: &[&str],
    row_limit: usize,
) -> Result<BuiltQuery, BuildError> {
    let table = fq_table(&shape.table)?;
    let station_fk = quote_ident(&shape.station_fk_col)?;
    let time_col = quote_ident(&shape.time_col)?;
    let param_col = quote_ident(&shape.param_col)?;
    let value_col = quote_ident(&shape.value_col)?;
    let time_select = time_select_expr(&time_col, shape.time_col_tz.as_deref());
    let tz = shape.time_col_tz.as_deref();

    let mut params: Vec<SqlParam> = vec![SqlParam::Text(station_id.to_string())];
    let mut sql = String::from("SELECT ");
    sql.push_str(&format!(
        "{time_select} AS time, \
         {param_col} AS parameter, \
         {value_col}::double precision AS value \
         FROM {table} \
         WHERE {station_fk} = $1"
    ));

    if let Some((t0, t1)) = time_range {
        let rhs_lo = time_filter_rhs("$2", tz);
        let rhs_hi = time_filter_rhs("$3", tz);
        sql.push_str(&format!(
            " AND {time_col} >= {rhs_lo} AND {time_col} <= {rhs_hi}"
        ));
        params.push(SqlParam::Timestamp(t0));
        params.push(SqlParam::Timestamp(t1));
    }

    sql.push_str(&format!(" AND {param_col} = ANY(${})", params.len() + 1));
    params.push(SqlParam::TextArray(
        source_keys.iter().map(|s| s.to_string()).collect(),
    ));

    sql.push_str(&format!(" ORDER BY {time_col} LIMIT ${}", params.len() + 1));
    params.push(SqlParam::Int(row_limit as i64));
    let limit_idx = params.len() - 1;

    Ok(BuiltQuery::new(sql, params).with_row_limit(limit_idx, 1))
}

fn build_location_wide(
    shape: &WideShape,
    station_id: &str,
    time_range: Option<(DateTime<Utc>, DateTime<Utc>)>,
    source_keys: &[&str],
    row_limit: usize,
) -> Result<BuiltQuery, BuildError> {
    let table = fq_table(&shape.table)?;
    let station_fk = quote_ident(&shape.station_fk_col)?;
    let time_col = quote_ident(&shape.time_col)?;
    let time_select = time_select_expr(&time_col, shape.time_col_tz.as_deref());
    let tz = shape.time_col_tz.as_deref();

    // Build the projection list from source_keys by looking up columns.
    let mut projection = format!("{time_select} AS time");
    for key in source_keys {
        let column = shape
            .columns
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, c)| c.as_str())
            .ok_or_else(|| BuildError::UnknownParameter((*key).to_string()))?;
        let col_quoted = quote_ident(column)?;
        let alias = quote_ident(key)?;
        projection.push_str(&format!(", {col_quoted}::double precision AS {alias}"));
    }

    let mut params: Vec<SqlParam> = vec![SqlParam::Text(station_id.to_string())];
    let mut sql = String::from("SELECT ");
    sql.push_str(&format!(
        "{projection} \
         FROM {table} \
         WHERE {station_fk} = $1"
    ));
    if let Some((t0, t1)) = time_range {
        let rhs_lo = time_filter_rhs("$2", tz);
        let rhs_hi = time_filter_rhs("$3", tz);
        sql.push_str(&format!(
            " AND {time_col} >= {rhs_lo} AND {time_col} <= {rhs_hi}"
        ));
        params.push(SqlParam::Timestamp(t0));
        params.push(SqlParam::Timestamp(t1));
    }

    sql.push_str(&format!(" ORDER BY {time_col} LIMIT ${}", params.len() + 1));
    params.push(SqlParam::Int(row_limit as i64));
    let limit_idx = params.len() - 1;

    // One wide row carries a value per selected column — weigh it so
    // against the response budget.
    Ok(BuiltQuery::new(sql, params).with_row_limit(limit_idx, source_keys.len().max(1)))
}

fn build_location_per_parameter(
    shape: &PerParameterShape,
    station_id: &str,
    time_range: Option<(DateTime<Utc>, DateTime<Utc>)>,
    source_keys: &[&str],
    row_limit: usize,
) -> Result<Vec<BuiltQuery>, BuildError> {
    let mut queries = Vec::with_capacity(source_keys.len());

    for key in source_keys {
        let table = shape
            .tables
            .iter()
            .find(|t| t.parameter == *key)
            .ok_or_else(|| BuildError::UnknownParameter((*key).to_string()))?;

        let table_fq = fq_table(&table.table)?;
        let station_fk = quote_ident(&table.station_fk_col)?;
        let time_col = quote_ident(&table.time_col)?;
        let value_col = quote_ident(&table.value_col)?;
        let time_select = time_select_expr(&time_col, table.time_col_tz.as_deref());
        let tz = table.time_col_tz.as_deref();

        let mut params: Vec<SqlParam> = vec![SqlParam::Text(station_id.to_string())];
        let mut sql = String::from("SELECT ");
        sql.push_str(&format!(
            "{time_select} AS time, \
             {value_col}::double precision AS value \
             FROM {table_fq} \
             WHERE {station_fk} = $1"
        ));
        if let Some((t0, t1)) = time_range {
            let rhs_lo = time_filter_rhs("$2", tz);
            let rhs_hi = time_filter_rhs("$3", tz);
            sql.push_str(&format!(
                " AND {time_col} >= {rhs_lo} AND {time_col} <= {rhs_hi}"
            ));
            params.push(SqlParam::Timestamp(t0));
            params.push(SqlParam::Timestamp(t1));
        }
        sql.push_str(&format!(" ORDER BY {time_col} LIMIT ${}", params.len() + 1));
        params.push(SqlParam::Int(row_limit as i64));
        let limit_idx = params.len() - 1;
        queries.push(
            BuiltQuery::new(sql, params)
                .with_parameter(key.to_string())
                .with_row_limit(limit_idx, 1),
        );
    }

    Ok(queries)
}

// ─── helpers ───────────────────────────────────────────────────────────────

fn fq_table(t: &QualifiedTable) -> Result<String, BuildError> {
    Ok(format!(
        "{}.{}",
        quote_ident(&t.schema)?,
        quote_ident(&t.table)?
    ))
}

/// Expression used in SELECT list to emit a `timestamptz`-typed value
/// regardless of the underlying column type.
///
/// - `timestamptz` column (tz=None): returns the column unchanged
/// - `timestamp without time zone` (tz=Some("UTC")): `col AT TIME ZONE 'UTC'`
///   — anchors the naive timestamp to UTC, yielding a timestamptz
fn time_select_expr(time_col: &str, tz: Option<&str>) -> String {
    match tz {
        None => time_col.to_string(),
        Some(tz) => format!("({time_col} AT TIME ZONE '{}')", escape_sql_literal(tz)),
    }
}

/// Wrap a bind placeholder for use against a `time_col` in a WHERE clause.
///
/// - `timestamptz` column (tz=None): returns the bind placeholder unchanged;
///   `DateTime<Utc>` ⇄ timestamptz pairs up cleanly.
/// - `timestamp without time zone` (tz=Some("UTC")): wraps as
///   `($N AT TIME ZONE '<tz>')`. The bind stays `DateTime<Utc>` (timestamptz);
///   Postgres converts it to `timestamp` at query time, so comparisons are
///   `timestamp >= timestamp` and the underlying btree/BRIN index on the
///   column is preserved (wrapping the column instead would disable it).
fn time_filter_rhs(bind: &str, tz: Option<&str>) -> String {
    match tz {
        None => bind.to_string(),
        Some(tz) => format!("({bind} AT TIME ZONE '{}')", escape_sql_literal(tz)),
    }
}

/// Escape a single-quoted SQL literal value. Only used for timezone
/// strings (which have already passed an IANA-like regex at config load);
/// values never come from user requests.
fn escape_sql_literal(s: &str) -> String {
    s.replace('\'', "''")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ValidatedParameter;
    use crate::schema::{
        LocationSource, LongShape, ObservationSchema, PerParameterShape, PerParameterTable,
        QualifiedTable, StationsMapping, WideShape,
    };
    use chrono::TimeZone;

    fn t(y: i32, mo: u32, d: u32, h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, 0, 0).unwrap()
    }

    // Test helpers build [`PostgisEngineConfig`] directly instead of going
    // through `resolve()` — that path (env var resolution, MC_ALLOW_INLINE
    // opt-in, parameter cross-refs) is exercised in `config::tests` with
    // an `EnvGuard` serialization mutex. Avoiding env-var mutation here
    // prevents a race with config::tests when cargo runs tests in
    // parallel within one binary.

    fn public(t_name: &str) -> QualifiedTable {
        QualifiedTable {
            schema: "public".into(),
            table: t_name.into(),
        }
    }

    fn param(
        name: &str,
        label: &str,
        unit: &str,
        observed: &str,
        source: &str,
    ) -> ValidatedParameter {
        ValidatedParameter {
            name: name.into(),
            label: label.into(),
            unit: unit.into(),
            observed_property: observed.into(),
            source_key: source.into(),
        }
    }

    fn mk_cfg(
        stations: StationsMapping,
        observations: ObservationSchema,
        parameters: Vec<ValidatedParameter>,
    ) -> PostgisEngineConfig {
        PostgisEngineConfig {
            dsn: "postgres://test@localhost/x".into(),
            dsn_was_literal: false,
            pool_size: 4,
            pool_label: None,
            metadata_refresh_secs: 300,
            location_source: LocationSource::Stations(stations),
            observations,
            parameters,
            locations_window: None,
            events_default_window: None,
            events_extent_bbox: None,
        }
    }

    /// A config with no stations table (`LocationSource::Observations`, mode A).
    fn mk_cfg_obs_only(observations: ObservationSchema) -> PostgisEngineConfig {
        PostgisEngineConfig {
            dsn: "postgres://test@localhost/x".into(),
            dsn_was_literal: false,
            pool_size: 4,
            pool_label: None,
            metadata_refresh_secs: 300,
            location_source: LocationSource::Observations,
            observations,
            parameters: vec![param("t2m", "t", "°C", "t2m", "t2m")],
            locations_window: None,
            events_default_window: None,
            events_extent_bbox: None,
        }
    }

    fn nexus_per_parameter_cfg() -> PostgisEngineConfig {
        mk_cfg(
            StationsMapping {
                table: public("stations"),
                id_col: "wigos_id".into(),
                label_col: "name".into(),
                geom_col: "the_geom".into(),
                property_cols: vec!["territory".into()],
                where_clause: None,
            },
            ObservationSchema::PerParameter(PerParameterShape {
                tables: vec![
                    PerParameterTable {
                        parameter: "air_temperature".into(),
                        table: public("airtemperature"),
                        station_fk_col: "wigos_id".into(),
                        time_col: "time".into(),
                        time_col_tz: Some("UTC".into()),
                        value_col: "value".into(),
                        geom_col: Some("the_geom".into()),
                    },
                    PerParameterTable {
                        parameter: "wind_speed".into(),
                        table: public("wind_speed"),
                        station_fk_col: "wigos_id".into(),
                        time_col: "time".into(),
                        time_col_tz: Some("UTC".into()),
                        value_col: "value".into(),
                        geom_col: Some("the_geom".into()),
                    },
                ],
            }),
            vec![
                param(
                    "air_temperature",
                    "2 m air temperature",
                    "°C",
                    "air_temperature",
                    "air_temperature",
                ),
                param(
                    "wind_speed",
                    "10 m wind speed",
                    "m/s",
                    "wind_speed",
                    "wind_speed",
                ),
            ],
        )
    }

    fn long_cfg() -> PostgisEngineConfig {
        mk_cfg(
            StationsMapping {
                table: public("stations"),
                id_col: "fmisid".into(),
                label_col: "name".into(),
                geom_col: "geom".into(),
                property_cols: vec![],
                where_clause: Some("active = true".into()),
            },
            ObservationSchema::Long(LongShape {
                table: public("observations"),
                station_fk_col: "fmisid".into(),
                time_col: "obstime".into(),
                time_col_tz: None,
                param_col: "param_name".into(),
                value_col: "value".into(),
                geom_col: None,
            }),
            vec![param("t2m", "temp", "°C", "t2m", "t2m")],
        )
    }

    fn wide_cfg() -> PostgisEngineConfig {
        mk_cfg(
            StationsMapping {
                table: public("stations"),
                id_col: "station_id".into(),
                label_col: "name".into(),
                geom_col: "geom".into(),
                property_cols: vec![],
                where_clause: None,
            },
            ObservationSchema::Wide(WideShape {
                table: public("synop"),
                station_fk_col: "station_id".into(),
                time_col: "valid_time".into(),
                time_col_tz: None,
                geom_col: None,
                columns: vec![
                    ("t2m".into(), "temp_celsius".into()),
                    ("rh".into(), "humidity_pct".into()),
                ],
            }),
            vec![
                param("t2m", "temp", "°C", "t2m", "t2m"),
                param("rh", "rh", "%", "rh", "rh"),
            ],
        )
    }

    #[test]
    fn locations_sql_quotes_identifiers_and_caps_rows() {
        let cfg = nexus_per_parameter_cfg();
        let q = build_locations(&cfg).unwrap();
        assert!(q.params.is_empty());
        assert!(q.sql.contains("\"wigos_id\""));
        assert!(q.sql.contains("\"territory\""));
        assert!(q.sql.contains("ST_Y(\"the_geom\")"));
        assert!(q.sql.contains("FROM \"public\".\"stations\""));
        assert!(q.sql.contains(&format!("LIMIT {MAX_LOCATIONS}")));
        assert!(!q.sql.contains(" WHERE ")); // no where_clause in this cfg
    }

    #[test]
    fn locations_sql_inlines_where_when_set() {
        let cfg = long_cfg();
        let q = build_locations(&cfg).unwrap();
        assert!(q.sql.contains(" WHERE active = true"));
    }

    #[test]
    fn obs_locations_per_parameter_one_query_per_table() {
        let cfg = nexus_per_parameter_cfg(); // both tables carry geom_col
        let qs = build_locations_from_observations(&cfg, None).unwrap();
        // One standalone query per table — NOT a single UNION (a UNION of N
        // multi-second per-table scans blows a read-only role's statement_timeout).
        assert_eq!(qs.len(), 2);
        for q in &qs {
            assert!(q.params.is_empty());
            assert!(q.sql.starts_with("SELECT DISTINCT ON (\"wigos_id\")"));
            assert!(q.sql.contains("ST_Y(\"the_geom\")"));
            assert!(q.sql.contains("ST_X(\"the_geom\")"));
            assert!(q.sql.contains("WHERE \"the_geom\" IS NOT NULL"));
            assert!(q.sql.contains(&format!("LIMIT {MAX_LOCATIONS}")));
            assert!(!q.sql.contains("UNION")); // no cross-table union in SQL
                                               // Orphans carry no station metadata columns.
            assert!(!q.sql.contains("\"territory\""));
            assert!(!q.sql.contains("AS label"));
        }
        assert!(qs[0].sql.contains("FROM \"public\".\"airtemperature\""));
        assert!(qs[1].sql.contains("FROM \"public\".\"wind_speed\""));
    }

    #[test]
    fn obs_locations_recent_window_filter() {
        let cfg = nexus_per_parameter_cfg(); // tables are time_col_tz = "UTC"
        let since = t(2026, 6, 15, 0);
        let qs = build_locations_from_observations(&cfg, Some(since)).unwrap();
        for q in &qs {
            // Recent-window filter present; the bind (not the column) is wrapped
            // for the naive-UTC column so the index/chunk-pruning stays usable.
            assert!(q.sql.contains("AND \"time\" >= ($1 AT TIME ZONE 'UTC')"));
            assert!(q
                .sql
                .contains(&format!("ORDER BY \"wigos_id\" LIMIT {MAX_LOCATIONS}")));
            assert_eq!(q.params, vec![SqlParam::Timestamp(since)]);
        }
        // None ⇒ no time filter, no bind (full-history path).
        let qs_all = build_locations_from_observations(&cfg, None).unwrap();
        for q in &qs_all {
            assert!(!q.sql.contains("AT TIME ZONE"));
            assert!(q.params.is_empty());
        }
    }

    #[test]
    fn obs_locations_recent_window_long_and_wide() {
        let since = t(2026, 6, 15, 0);
        // Long, naive-UTC column ⇒ bind wrapped with AT TIME ZONE.
        let long = mk_cfg(
            StationsMapping {
                table: public("stations"),
                id_col: "wigos_id".into(),
                label_col: "name".into(),
                geom_col: "the_geom".into(),
                property_cols: vec![],
                where_clause: None,
            },
            ObservationSchema::Long(LongShape {
                table: public("obs"),
                station_fk_col: "wigos_id".into(),
                time_col: "obstime".into(),
                time_col_tz: Some("UTC".into()),
                param_col: "param".into(),
                value_col: "value".into(),
                geom_col: Some("the_geom".into()),
            }),
            vec![param("t2m", "t", "°C", "t2m", "t2m")],
        );
        let ql = build_locations_from_observations(&long, Some(since)).unwrap();
        assert_eq!(ql.len(), 1);
        assert!(ql[0]
            .sql
            .contains("AND \"obstime\" >= ($1 AT TIME ZONE 'UTC')"));
        assert_eq!(ql[0].params, vec![SqlParam::Timestamp(since)]);

        // Wide, timestamptz column (tz = None) ⇒ bind used bare, no AT TIME ZONE.
        let wide = mk_cfg(
            StationsMapping {
                table: public("stations"),
                id_col: "station_id".into(),
                label_col: "name".into(),
                geom_col: "geom".into(),
                property_cols: vec![],
                where_clause: None,
            },
            ObservationSchema::Wide(WideShape {
                table: public("synop"),
                station_fk_col: "station_id".into(),
                time_col: "valid_time".into(),
                time_col_tz: None,
                geom_col: Some("the_geom".into()),
                columns: vec![("t2m".into(), "temp_celsius".into())],
            }),
            vec![param("t2m", "t", "°C", "t2m", "t2m")],
        );
        let qw = build_locations_from_observations(&wide, Some(since)).unwrap();
        assert_eq!(qw.len(), 1);
        assert!(qw[0].sql.contains("AND \"valid_time\" >= $1"));
        assert!(!qw[0].sql.contains("AT TIME ZONE"));
        assert_eq!(qw[0].params, vec![SqlParam::Timestamp(since)]);
    }

    #[test]
    fn obs_locations_per_parameter_inherits_shared_geom_col() {
        // A per_parameter table with NO own geom_col must inherit the shared
        // `observations.geom_col` (the nexus fmi-obs pattern). Exercise the full
        // lowering path (ObservationSchema::from_config) → SQL builder, not a
        // pre-populated shape, so the inheritance is actually under test.
        use ds_core::config::{PostgisObservationTable, PostgisObservationsConfig};
        let raw = PostgisObservationsConfig {
            shape: "per_parameter".into(),
            table: None,
            station_fk_col: Some("wigos_id".into()),
            time_col: Some("time".into()),
            time_col_tz: Some("UTC".into()),
            param_col: None,
            value_col: Some("value".into()),
            geom_col: Some("the_geom".into()), // shared default
            locations_window: None,
            id_col: None,
            cloud_indicator_col: None,
            peak_current_col: None,
            multiplicity_col: None,
            default_datetime: None,
            extent_bbox: None,
            columns: vec![],
            tables: vec![PostgisObservationTable {
                parameter: "t2m".into(),
                table: "public.airtemperature".into(),
                station_fk_col: None,
                time_col: None,
                time_col_tz: None,
                value_col: None,
                geom_col: None, // <-- no own geom; must inherit the default
            }],
        };
        let observations = ObservationSchema::from_config(&raw).unwrap();
        let cfg = mk_cfg_obs_only(observations);
        let qs = build_locations_from_observations(&cfg, None).unwrap();
        assert_eq!(qs.len(), 1);
        assert!(qs[0].sql.contains("ST_Y(\"the_geom\")"));
        assert!(qs[0].sql.contains("WHERE \"the_geom\" IS NOT NULL"));
    }

    #[test]
    fn obs_locations_long_distinct_on_fk_quotes_and_caps() {
        let cfg = mk_cfg(
            StationsMapping {
                table: public("stations"),
                id_col: "wigos_id".into(),
                label_col: "name".into(),
                geom_col: "the_geom".into(),
                property_cols: vec![],
                where_clause: None,
            },
            ObservationSchema::Long(LongShape {
                table: public("obs"),
                station_fk_col: "wigos_id".into(),
                time_col: "time".into(),
                time_col_tz: None,
                param_col: "param".into(),
                value_col: "value".into(),
                geom_col: Some("the_geom".into()),
            }),
            vec![param("t2m", "t", "°C", "t2m", "t2m")],
        );
        let qs = build_locations_from_observations(&cfg, None).unwrap();
        assert_eq!(qs.len(), 1);
        let q = &qs[0];
        assert!(q.sql.starts_with("SELECT DISTINCT ON (\"wigos_id\")"));
        assert!(q.sql.contains("\"wigos_id\"::text AS id"));
        assert!(q.sql.contains("WHERE \"the_geom\" IS NOT NULL"));
        assert!(q
            .sql
            .contains(&format!("ORDER BY \"wigos_id\" LIMIT {MAX_LOCATIONS}")));
        assert!(!q.sql.contains(" UNION "));
    }

    #[test]
    fn obs_locations_errors_without_geom() {
        let cfg = long_cfg(); // LongShape.geom_col == None
        let err = build_locations_from_observations(&cfg, None).unwrap_err();
        assert!(matches!(err, BuildError::NoObservationGeom));
    }

    #[test]
    fn station_builders_error_without_stations_table() {
        // A LocationSource::Observations cfg has no stations mapping — the
        // station-only builders must surface NoStations rather than panic.
        let cfg = mk_cfg_obs_only(ObservationSchema::Long(LongShape {
            table: public("obs"),
            station_fk_col: "wigos_id".into(),
            time_col: "time".into(),
            time_col_tz: None,
            param_col: "param".into(),
            value_col: "value".into(),
            geom_col: Some("the_geom".into()),
        }));
        assert!(matches!(
            build_locations(&cfg).unwrap_err(),
            BuildError::NoStations
        ));
        assert!(matches!(
            build_position(&cfg, 24.9, 60.2, 5000.0).unwrap_err(),
            BuildError::NoStations
        ));
        assert!(matches!(
            build_stations_in_polygon(&cfg, "POLYGON((0 0,1 0,1 1,0 1,0 0))").unwrap_err(),
            BuildError::NoStations
        ));
        // ...but the observations-derived builder works in this mode.
        assert!(build_locations_from_observations(&cfg, None).is_ok());
    }

    #[test]
    fn position_binds_lon_lat_radius_and_limits_to_one() {
        let cfg = long_cfg();
        let q = build_position(&cfg, 24.9, 60.2, 5000.0).unwrap();
        assert_eq!(
            q.params,
            vec![
                SqlParam::Float(24.9),
                SqlParam::Float(60.2),
                SqlParam::Float(5000.0),
            ]
        );
        assert!(q.sql.contains("ST_DWithin("));
        assert!(q.sql.contains("ST_MakePoint($1, $2)"));
        assert!(q.sql.contains("$3)"));
        assert!(q.sql.contains("AND (active = true)"));
        assert!(q.sql.ends_with(" LIMIT 1"));
    }

    #[test]
    fn position_rejects_non_positive_radius() {
        let cfg = long_cfg();
        assert!(matches!(
            build_position(&cfg, 0.0, 0.0, 0.0),
            Err(BuildError::InvalidRadius(_))
        ));
        assert!(matches!(
            build_position(&cfg, 0.0, 0.0, -1.0),
            Err(BuildError::InvalidRadius(_))
        ));
    }

    #[test]
    fn stations_in_polygon_binds_wkt_once_and_caps_501() {
        let cfg = nexus_per_parameter_cfg();
        let q = build_stations_in_polygon(&cfg, "POLYGON((0 0,10 0,10 10,0 10,0 0))").unwrap();
        assert_eq!(
            q.params,
            vec![SqlParam::Text("POLYGON((0 0,10 0,10 10,0 10,0 0))".into())]
        );
        // WKT bind is reused (same $1 in bbox prefilter and ST_Within).
        assert_eq!(q.sql.matches("$1").count(), 2);
        assert!(q.sql.contains(&format!("LIMIT {MAX_STATIONS_IN_POLYGON}")));
    }

    #[test]
    fn stations_in_polygon_rejects_empty_wkt() {
        let cfg = nexus_per_parameter_cfg();
        assert!(matches!(
            build_stations_in_polygon(&cfg, "   "),
            Err(BuildError::EmptyPolygonWkt)
        ));
    }

    #[test]
    fn location_long_with_time_range_has_three_binds() {
        let cfg = long_cfg();
        let q = build_location(
            &cfg,
            "1001",
            Some((t(2026, 4, 1, 0), t(2026, 4, 1, 12))),
            &["t2m"],
            MAX_OBSERVATION_ROWS,
        )
        .unwrap();
        assert_eq!(q.len(), 1);
        let q = &q[0];
        assert_eq!(
            q.params,
            vec![
                SqlParam::Text("1001".into()),
                SqlParam::Timestamp(t(2026, 4, 1, 0)),
                SqlParam::Timestamp(t(2026, 4, 1, 12)),
                SqlParam::TextArray(vec!["t2m".into()]),
                SqlParam::Int(MAX_OBSERVATION_ROWS as i64),
            ]
        );
        assert!(q.sql.contains("\"param_name\" = ANY($4)"));
        // The row cap is a bind, not a literal — the engine can rewrite it
        // to the remaining response budget.
        assert!(q.sql.contains("LIMIT $5"));
        assert_eq!(q.limit_param_idx, Some(4));
        assert_eq!(q.row_limit(), Some(MAX_OBSERVATION_ROWS));
        assert_eq!(q.values_per_row, 1);
    }

    #[test]
    fn location_long_without_time_range_drops_time_binds() {
        let cfg = long_cfg();
        let q = build_location(&cfg, "1001", None, &["t2m"], MAX_OBSERVATION_ROWS).unwrap();
        let q = &q[0];
        assert_eq!(
            q.params,
            vec![
                SqlParam::Text("1001".into()),
                SqlParam::TextArray(vec!["t2m".into()]),
                SqlParam::Int(MAX_OBSERVATION_ROWS as i64),
            ]
        );
        assert!(q.sql.contains("\"param_name\" = ANY($2)"));
        assert!(!q.sql.contains(">= $"));
    }

    #[test]
    fn location_wide_projects_only_requested_columns() {
        let cfg = wide_cfg();
        let q = build_location(&cfg, "s1", None, &["t2m"], MAX_OBSERVATION_ROWS).unwrap();
        let q = &q[0];
        assert!(q
            .sql
            .contains("\"temp_celsius\"::double precision AS \"t2m\""));
        assert!(!q.sql.contains("humidity_pct"));
    }

    #[test]
    fn location_wide_weighs_rows_by_selected_column_count() {
        // One wide row carries a value per selected column, so it must
        // charge the response budget accordingly.
        let cfg = wide_cfg();
        let q = build_location(&cfg, "s1", None, &["t2m", "rh"], MAX_OBSERVATION_ROWS).unwrap();
        assert_eq!(q[0].values_per_row, 2);
    }

    #[test]
    fn set_row_limit_rewrites_the_limit_bind_only() {
        let cfg = long_cfg();
        let mut q = build_location(&cfg, "1001", None, &["t2m"], MAX_OBSERVATION_ROWS)
            .unwrap()
            .remove(0);
        let n_params = q.params.len();
        q.set_row_limit(42);
        assert_eq!(q.row_limit(), Some(42));
        assert_eq!(q.params.len(), n_params);
        // Non-limit binds untouched.
        assert_eq!(q.params[0], SqlParam::Text("1001".into()));
    }

    #[test]
    fn location_wide_unknown_parameter_errors() {
        let cfg = wide_cfg();
        let err = build_location(&cfg, "s1", None, &["does_not_exist"], MAX_OBSERVATION_ROWS)
            .unwrap_err();
        assert!(matches!(err, BuildError::UnknownParameter(_)));
    }

    #[test]
    fn location_per_parameter_fans_out_one_query_per_param() {
        let cfg = nexus_per_parameter_cfg();
        let qs = build_location(
            &cfg,
            "0-146-0-1001",
            None,
            &["air_temperature", "wind_speed"],
            MAX_OBSERVATION_ROWS,
        )
        .unwrap();
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[0].parameter.as_deref(), Some("air_temperature"));
        assert_eq!(qs[1].parameter.as_deref(), Some("wind_speed"));
        assert!(qs[0].sql.contains("FROM \"public\".\"airtemperature\""));
        assert!(qs[1].sql.contains("FROM \"public\".\"wind_speed\""));
    }

    #[test]
    fn location_per_parameter_emits_time_tz_conversion_when_set() {
        // nexus time_col_tz = "UTC" → SELECT list wraps in AT TIME ZONE 'UTC'.
        let cfg = nexus_per_parameter_cfg();
        let qs =
            build_location(&cfg, "s", None, &["air_temperature"], MAX_OBSERVATION_ROWS).unwrap();
        assert!(qs[0].sql.contains("(\"time\" AT TIME ZONE 'UTC') AS time"));
    }

    #[test]
    fn location_long_without_tz_emits_bare_column() {
        let cfg = long_cfg();
        let q = build_location(&cfg, "s", None, &["t2m"], MAX_OBSERVATION_ROWS).unwrap();
        assert!(q[0].sql.contains("\"obstime\" AS time"));
        assert!(!q[0].sql.contains("AT TIME ZONE"));
    }

    #[test]
    fn empty_source_keys_expands_to_all_configured_parameters() {
        let cfg = nexus_per_parameter_cfg();
        let qs = build_location(&cfg, "s", None, &[], MAX_OBSERVATION_ROWS).unwrap();
        assert_eq!(qs.len(), 2); // both air_temperature and wind_speed
    }

    #[test]
    fn per_parameter_unknown_parameter_errors() {
        let cfg = nexus_per_parameter_cfg();
        let err = build_location(&cfg, "s", None, &["not_a_real_param"], MAX_OBSERVATION_ROWS)
            .unwrap_err();
        assert!(matches!(err, BuildError::UnknownParameter(_)));
    }

    // ---- events ------------------------------------------------------------

    fn lightning_shape() -> EventsShape {
        lightning_shape_with(None, None)
    }

    /// A shape with the optional attribute columns declared, so the SQL
    /// builder's behaviour with and without them is both testable.
    fn lightning_shape_with(cloud: Option<&str>, current: Option<&str>) -> EventsShape {
        EventsShape {
            cloud_indicator_col: cloud.map(str::to_string),
            peak_current_col: current.map(str::to_string),
            multiplicity_col: None,
            table: QualifiedTable {
                schema: "public".into(),
                table: "lightning".into(),
            },
            time_col: "time".into(),
            time_col_tz: Some("UTC".into()),
            geom_col: "the_geom".into(),
            id_col: "id".into(),
        }
    }

    #[test]
    fn events_area_sql_shape_and_binds() {
        let q = build_events_area(
            &lightning_shape(),
            "POLYGON((21 59,29 59,29 66,21 66,21 59))",
            (t(2026, 7, 11, 17), t(2026, 7, 11, 18)),
            &["peak_current", "multiplicity"],
            125_001,
        )
        .unwrap();

        // Projection: timestamptz time + coords + one cast column per key.
        assert!(q.sql.contains("(\"time\" AT TIME ZONE 'UTC') AS time"));
        assert!(q.sql.contains("ST_X(\"the_geom\") AS lon"));
        assert!(q.sql.contains("ST_Y(\"the_geom\") AS lat"));
        assert!(q
            .sql
            .contains("\"peak_current\"::double precision AS \"peak_current\""));
        assert!(q
            .sql
            .contains("\"multiplicity\"::double precision AS \"multiplicity\""));
        // Time filter wraps the BINDS (index-preserving), not the column.
        assert!(q.sql.contains("\"time\" >= ($1 AT TIME ZONE 'UTC')"));
        assert!(q.sql.contains("\"time\" <= ($2 AT TIME ZONE 'UTC')"));
        // Polygon test: bbox operator prefilter + exact intersects, one bind.
        assert!(q.sql.contains("\"the_geom\" && ST_GeomFromText($3, 4326)"));
        assert!(q
            .sql
            .contains("ST_Intersects(\"the_geom\", ST_GeomFromText($3, 4326))"));
        // Deterministic newest-first ordering with id tiebreak; bound LIMIT.
        assert!(q.sql.contains("ORDER BY \"time\" DESC, \"id\" LIMIT $4"));

        assert_eq!(q.params.len(), 4);
        assert_eq!(
            q.params[2],
            SqlParam::Text("POLYGON((21 59,29 59,29 66,21 66,21 59))".into())
        );
        assert_eq!(q.params[3], SqlParam::Int(125_001));
        assert_eq!(q.limit_param_idx, Some(3));
        assert_eq!(q.values_per_row, 2);
    }

    #[test]
    fn events_area_without_tz_uses_bare_binds() {
        let mut shape = lightning_shape();
        shape.time_col_tz = None;
        let q = build_events_area(
            &shape,
            "POLYGON((0 0,1 0,1 1,0 1,0 0))",
            (t(2026, 7, 11, 17), t(2026, 7, 11, 18)),
            &["peak_current"],
            10,
        )
        .unwrap();
        assert!(q.sql.contains("\"time\" >= $1 AND \"time\" <= $2"));
        assert!(!q.sql.contains("AT TIME ZONE"));
        assert_eq!(q.values_per_row, 1);
    }

    #[test]
    fn events_area_rejects_empty_wkt() {
        let err = build_events_area(
            &lightning_shape(),
            "  ",
            (t(2026, 7, 11, 17), t(2026, 7, 11, 18)),
            &["peak_current"],
            10,
        )
        .unwrap_err();
        assert!(matches!(err, BuildError::EmptyPolygonWkt));
    }

    #[test]
    fn events_station_builders_reject_events_shape() {
        let cfg = mk_cfg_obs_only(ObservationSchema::Events(lightning_shape()));
        let err = build_location(&cfg, "s", None, &["peak_current"], 10).unwrap_err();
        assert!(matches!(err, BuildError::NoStations));
        let err = build_locations_from_observations(&cfg, None).unwrap_err();
        assert!(matches!(err, BuildError::NoStations));
    }

    #[test]
    fn events_window_selects_declared_attribute_columns_cast() {
        let shape = lightning_shape_with(Some("cloud_indicator"), Some("peak_current"));
        let q = build_events_window(
            &shape,
            (t(2026, 7, 11, 17), t(2026, 7, 11, 18)),
            1000,
            WindowAttrs::Include,
        )
        .unwrap();

        // Cast, not a bare column: the source may be smallint, numeric or
        // real, and only the cast makes one decode cover all three.
        assert!(q
            .sql
            .contains("\"cloud_indicator\"::double precision AS cloud_indicator"));
        assert!(q
            .sql
            .contains("\"peak_current\"::double precision AS peak_current"));
        // Undeclared column is absent entirely, not selected as NULL.
        assert!(!q.sql.contains("multiplicity"));
        // Half-open window and newest-first truncation are unchanged.
        assert!(q.sql.contains("\"time\" > ($1 AT TIME ZONE 'UTC')"));
        assert!(q.sql.contains("ORDER BY \"time\" DESC"));
    }

    #[test]
    fn events_window_for_the_map_omits_attributes_even_when_declared() {
        // The map splat shares this builder but reads none of them; selecting
        // three unread columns per strike on a per-frame query is pure cost.
        let shape = lightning_shape_with(Some("cloud_indicator"), Some("peak_current"));
        let q = build_events_window(
            &shape,
            (t(2026, 7, 11, 17), t(2026, 7, 11, 18)),
            1000,
            WindowAttrs::Omit,
        )
        .unwrap();
        assert!(!q.sql.contains("cloud_indicator"));
        assert!(!q.sql.contains("peak_current"));
        assert!(q.sql.contains("ST_X(\"the_geom\") AS lon"));
    }

    #[test]
    fn events_window_neutralizes_an_injecting_attribute_identifier() {
        // Names are rejected at config load (`check_identifier`), so this can
        // only be reached by constructing an EventsShape directly. Even then
        // `quote_ident` escapes rather than concatenates: the payload becomes
        // ONE quoted identifier that the DB rejects as unknown, never a second
        // statement. Defense in depth, per Critical Rule 8.
        let shape = lightning_shape_with(Some("bad\"; DROP TABLE x --"), None);
        let q = build_events_window(
            &shape,
            (t(2026, 7, 11, 17), t(2026, 7, 11, 18)),
            1000,
            WindowAttrs::Include,
        )
        .unwrap();
        assert!(
            q.sql.contains("\"bad\"\"; DROP TABLE x --\""),
            "the embedded quote must be doubled, not closed: {}",
            q.sql
        );
        // The statement still ends at the single LIMIT bind — nothing escaped.
        assert_eq!(q.sql.matches("SELECT").count(), 1);
    }
}
