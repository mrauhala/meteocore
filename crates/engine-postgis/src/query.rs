//! Parameterized SQL builders.
//!
//! Every emitted SELECT ends with a hard row cap (`LIMIT 10001` for
//! observations, `LIMIT 1001` for locations, `LIMIT 501` for the
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
use crate::schema::{LongShape, ObservationSchema, PerParameterShape, QualifiedTable, WideShape};
use crate::security::{quote_ident, QuoteError};

/// Maximum rows returned by a locations list query. 50_001 covers the
/// real-world deployment range (FMI ~10k stations, MET Office ~5k, NOAA
/// COOP ~30k). Operators with larger networks should narrow via
/// `stations.where`. The 50_001th row signals the cap is breached so the
/// engine can warn — see #110 metadata refresh status.
pub const MAX_LOCATIONS: usize = 50_001;

/// Maximum rows returned per observation query. Exceeded ⇒ engine returns
/// a `Truncated` error that the HTTP layer maps to 413.
pub const MAX_OBSERVATION_ROWS: usize = 10001;

/// Maximum stations returned by the area-prefilter query. `CsvEngine`
/// caps area results at 500 stations; 501 here lets the engine tell if
/// the cap was breached.
pub const MAX_STATIONS_IN_POLYGON: usize = 501;

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
}

impl BuiltQuery {
    fn new(sql: String, params: Vec<SqlParam>) -> Self {
        Self {
            sql,
            params,
            parameter: None,
        }
    }

    fn with_parameter(mut self, parameter: impl Into<String>) -> Self {
        self.parameter = Some(parameter.into());
        self
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
pub fn build_location(
    cfg: &PostgisEngineConfig,
    station_id: &str,
    time_range: Option<(DateTime<Utc>, DateTime<Utc>)>,
    source_keys: &[&str],
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
            let q = build_location_long(l, station_id, time_range, &effective)?;
            Ok(vec![q])
        }
        ObservationSchema::Wide(w) => {
            let q = build_location_wide(w, station_id, time_range, &effective)?;
            Ok(vec![q])
        }
        ObservationSchema::PerParameter(pp) => {
            build_location_per_parameter(pp, station_id, time_range, &effective)
        }
    }
}

fn build_location_long(
    shape: &LongShape,
    station_id: &str,
    time_range: Option<(DateTime<Utc>, DateTime<Utc>)>,
    source_keys: &[&str],
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

    sql.push_str(&format!(
        " ORDER BY {time_col} LIMIT {MAX_OBSERVATION_ROWS}"
    ));

    Ok(BuiltQuery::new(sql, params))
}

fn build_location_wide(
    shape: &WideShape,
    station_id: &str,
    time_range: Option<(DateTime<Utc>, DateTime<Utc>)>,
    source_keys: &[&str],
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

    sql.push_str(&format!(
        " ORDER BY {time_col} LIMIT {MAX_OBSERVATION_ROWS}"
    ));

    Ok(BuiltQuery::new(sql, params))
}

fn build_location_per_parameter(
    shape: &PerParameterShape,
    station_id: &str,
    time_range: Option<(DateTime<Utc>, DateTime<Utc>)>,
    source_keys: &[&str],
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
        sql.push_str(&format!(
            " ORDER BY {time_col} LIMIT {MAX_OBSERVATION_ROWS}"
        ));
        queries.push(BuiltQuery::new(sql, params).with_parameter(key.to_string()));
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
            ]
        );
        assert!(q.sql.contains("\"param_name\" = ANY($4)"));
        assert!(q.sql.contains(&format!("LIMIT {MAX_OBSERVATION_ROWS}")));
    }

    #[test]
    fn location_long_without_time_range_drops_time_binds() {
        let cfg = long_cfg();
        let q = build_location(&cfg, "1001", None, &["t2m"]).unwrap();
        let q = &q[0];
        assert_eq!(
            q.params,
            vec![
                SqlParam::Text("1001".into()),
                SqlParam::TextArray(vec!["t2m".into()]),
            ]
        );
        assert!(q.sql.contains("\"param_name\" = ANY($2)"));
        assert!(!q.sql.contains(">= $"));
    }

    #[test]
    fn location_wide_projects_only_requested_columns() {
        let cfg = wide_cfg();
        let q = build_location(&cfg, "s1", None, &["t2m"]).unwrap();
        let q = &q[0];
        assert!(q
            .sql
            .contains("\"temp_celsius\"::double precision AS \"t2m\""));
        assert!(!q.sql.contains("humidity_pct"));
    }

    #[test]
    fn location_wide_unknown_parameter_errors() {
        let cfg = wide_cfg();
        let err = build_location(&cfg, "s1", None, &["does_not_exist"]).unwrap_err();
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
        let qs = build_location(&cfg, "s", None, &["air_temperature"]).unwrap();
        assert!(qs[0].sql.contains("(\"time\" AT TIME ZONE 'UTC') AS time"));
    }

    #[test]
    fn location_long_without_tz_emits_bare_column() {
        let cfg = long_cfg();
        let q = build_location(&cfg, "s", None, &["t2m"]).unwrap();
        assert!(q[0].sql.contains("\"obstime\" AS time"));
        assert!(!q[0].sql.contains("AT TIME ZONE"));
    }

    #[test]
    fn empty_source_keys_expands_to_all_configured_parameters() {
        let cfg = nexus_per_parameter_cfg();
        let qs = build_location(&cfg, "s", None, &[]).unwrap();
        assert_eq!(qs.len(), 2); // both air_temperature and wind_speed
    }

    #[test]
    fn per_parameter_unknown_parameter_errors() {
        let cfg = nexus_per_parameter_cfg();
        let err = build_location(&cfg, "s", None, &["not_a_real_param"]).unwrap_err();
        assert!(matches!(err, BuildError::UnknownParameter(_)));
    }
}
