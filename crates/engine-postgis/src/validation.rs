//! Database-side mapping checks, run before publishing metadata, never on an
//! HTTP request. Generic geometry declarations need a bounded sample plus a
//! check on every returned geometry; a sample alone cannot certify a view.

use std::time::Duration;

use tokio_postgres::{Client, Row};

use crate::config::PostgisEngineConfig;
use crate::schema::{ObservationSchema, QualifiedTable};
use crate::security::quote_ident;

pub(crate) const SCHEMA_ERROR: &str =
    "database schema does not match collection configuration (see server logs)";
pub(crate) const PENDING_ERROR: &str = "database schema validation pending";
const VALIDATION_TIMEOUT: Duration = Duration::from_secs(5);
const SAMPLE_ROWS: usize = 128;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ValidationError {
    #[error("{0}")]
    Mismatch(String),
    #[error("{0}")]
    Database(#[from] tokio_postgres::Error),
    #[error("validation connection closed")]
    Closed,
    #[error("database schema validation timed out")]
    Timeout,
}

/// A dedicated connection bounds acquisition, catalog and view/sample work
/// together. Driving it inline ensures timeout drops the socket, rather than
/// returning a pooled connection with validation still running on it.
pub(crate) async fn validate(cfg: &PostgisEngineConfig) -> Result<(), ValidationError> {
    let work = async {
        let (client, connection) = tokio_postgres::connect(&cfg.dsn, tokio_postgres::NoTls).await?;
        tokio::pin!(connection);
        tokio::select! {
            result = validate_on(&client, cfg) => result,
            result = &mut connection => {
                result?;
                Err(ValidationError::Closed)
            }
        }
    };
    tokio::time::timeout(VALIDATION_TIMEOUT, work)
        .await
        .map_err(|_| ValidationError::Timeout)?
}

async fn validate_on<'a>(
    client: &Client,
    cfg: &'a PostgisEngineConfig,
) -> Result<(), ValidationError> {
    // Keep a stricter role timeout. A server-side limit also stops view work
    // if the outer deadline closes the connection while PostgreSQL is busy.
    client.query_one(
        "SELECT set_config('statement_timeout', LEAST(COALESCE(NULLIF(EXTRACT(EPOCH FROM current_setting('statement_timeout')::interval) * 1000, 0), 5000), 5000)::bigint::text, false)", &[],
    ).await?;
    let mut times = Vec::new();
    let mut geometries = Vec::new();
    if let Some(stations) = cfg.location_source.stations() {
        geometries.push((
            &stations.table,
            stations.geom_col.as_str(),
            stations.where_clause.as_deref(),
        ));
    }
    let mut observation = |table, time, tz, geom: Option<&'a str>| {
        times.push((table, time, tz));
        if cfg.location_source.uses_observations() {
            if let Some(geom) = geom {
                geometries.push((table, geom, None));
            }
        }
    };
    match &cfg.observations {
        ObservationSchema::Long(s) => observation(
            &s.table,
            &s.time_col,
            s.time_col_tz.as_deref(),
            s.geom_col.as_deref(),
        ),
        ObservationSchema::Wide(s) => observation(
            &s.table,
            &s.time_col,
            s.time_col_tz.as_deref(),
            s.geom_col.as_deref(),
        ),
        ObservationSchema::PerParameter(s) => {
            for t in &s.tables {
                observation(
                    &t.table,
                    &t.time_col,
                    t.time_col_tz.as_deref(),
                    t.geom_col.as_deref(),
                );
            }
        }
        ObservationSchema::Events(s) => {
            times.push((&s.table, &s.time_col, s.time_col_tz.as_deref()));
            geometries.push((&s.table, s.geom_col.as_str(), None));
        }
    }
    for (table, column, tz) in times {
        let (kind, _) = column_type(client, table, column).await?;
        if !matches!(
            (kind.as_str(), tz),
            ("timestamp", Some(_)) | ("timestamptz", None)
        ) {
            return Err(mismatch(
                table,
                column,
                "timestamp requires time_col_tz; timestamptz requires it to be absent",
            ));
        }
        if let Some(tz) = tz {
            // Ask the same PostgreSQL operator used by data queries. This also
            // accepts abbreviations/POSIX zones, without changing session TZ.
            if let Err(e) = client
                .query_one("SELECT TIMESTAMP '2000-01-01' AT TIME ZONE $1", &[&tz])
                .await
            {
                if e.code() == Some(&tokio_postgres::error::SqlState::INVALID_PARAMETER_VALUE) {
                    return Err(mismatch(
                        table,
                        column,
                        &format!("invalid time_col_tz {tz:?}: {e}"),
                    ));
                }
                return Err(e.into());
            }
        }
    }
    for (table, column, filter) in geometries {
        let (kind, typmod) = column_type(client, table, column).await?;
        if kind != "geometry" {
            return Err(mismatch(table, column, "expected PostGIS geometry"));
        }
        let declaration = if typmod >= 0 {
            Some(
                client
                    .query_one(
                        "SELECT postgis_typmod_type($1), postgis_typmod_srid($1)",
                        &[&typmod],
                    )
                    .await?,
            )
        } else {
            // Legacy AddGeometryColumn mappings use CHECK constraints instead
            // of typmods. PostGIS exposes the constraints it recognizes here;
            // an unconstrained view still reports GEOMETRY / unknown SRID.
            client.query_opt(
                "SELECT type, srid FROM geometry_columns WHERE f_table_schema = $1 AND f_table_name = $2 AND f_geometry_column = $3",
                &[&table.schema, &table.table, &column],
            ).await?
        };
        if let Some(row) = declaration {
            let shape: String = row.get(0);
            let srid: i32 = row.get(1);
            let shape = shape.to_ascii_uppercase();
            let shape = shape.trim_end_matches(['Z', 'M']);
            if !matches!(shape, "POINT" | "GEOMETRY") || (srid != 0 && srid != 4326) {
                return Err(mismatch(
                    table,
                    column,
                    &format!("expected Point/4326, declared {shape}/{srid}"),
                ));
            }
            if shape == "POINT" && srid == 4326 {
                continue; // enforced by the database, even when empty
            }
        }
        // Limit BEFORE examining geometry: NULL/empty-only tables must not
        // force a history scan looking for a non-empty row. Views may do more
        // work internally, so the outer connection deadline is also essential.
        let geom = quote_ident(column).map_err(|e| ValidationError::Mismatch(e.to_string()))?;
        let schema =
            quote_ident(&table.schema).map_err(|e| ValidationError::Mismatch(e.to_string()))?;
        let name =
            quote_ident(&table.table).map_err(|e| ValidationError::Mismatch(e.to_string()))?;
        let mut sql = String::from("SELECT ");
        sql.push_str(&geometry_check(&geom));
        sql.push_str(" FROM (SELECT ");
        sql.push_str(&format!("{geom} FROM {schema}.{name}"));
        // SAFETY: the same config-time, blocklist-validated fragment used by
        // build_locations. It defines which registry rows this source uses.
        if let Some(filter) = filter {
            sql.push_str(&format!(" WHERE {filter}"));
        }
        sql.push_str(&format!(" LIMIT {SAMPLE_ROWS}) AS sample"));
        for row in client.query(&sql, &[]).await? {
            if !geometry_row_valid(&row) {
                return Err(mismatch(
                    table,
                    column,
                    "sample contains a non-Point or non-4326 geometry",
                ));
            }
        }
    }
    Ok(())
}

async fn column_type(
    client: &Client,
    table: &QualifiedTable,
    column: &str,
) -> Result<(String, i32), ValidationError> {
    // pg_attribute includes tables, hypertables, views and materialized views.
    // Follow domains to their base type and inherit their typmod if necessary.
    let row = client.query_opt(
        "WITH RECURSIVE types AS (
            SELECT a.atttypid AS oid, a.atttypmod AS typmod, 0 AS depth
            FROM pg_catalog.pg_attribute a
            JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
            JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
            WHERE n.nspname = $1 AND c.relname = $2 AND a.attname = $3
              AND a.attnum > 0 AND NOT a.attisdropped
            UNION ALL
            SELECT t.typbasetype, CASE WHEN types.typmod >= 0 THEN types.typmod ELSE t.typtypmod END, depth + 1
            FROM types JOIN pg_catalog.pg_type t ON t.oid = types.oid
            WHERE t.typtype = 'd' AND depth < 16
        ) SELECT t.typname::text, types.typmod FROM types
          JOIN pg_catalog.pg_type t ON t.oid = types.oid
          WHERE t.typtype <> 'd'",
        &[&table.schema, &table.table, &column],
    ).await?;
    row.map(|r| (r.get(0), r.get(1)))
        .ok_or_else(|| mismatch(table, column, "mapped column not found"))
}

fn mismatch(table: &QualifiedTable, column: &str, reason: &str) -> ValidationError {
    ValidationError::Mismatch(format!(
        "{}.{}.{}: {reason}",
        table.schema, table.table, column
    ))
}

/// `geom` is already quoted by the caller. Empty/NULL geometries retain the
/// existing decoder behavior (events skip them, station decodes can fail).
pub(crate) fn geometry_check(geom: &str) -> String {
    format!("({geom} IS NULL OR (ST_GeometryType({geom}) = 'ST_Point' AND (ST_IsEmpty({geom}) OR ST_SRID({geom}) = 4326)))")
}

/// Avoid calling ST_X/Y on non-points: the boolean at the end of the row
/// reports the schema error before any coordinates are used.
pub(crate) fn coordinates(geom: &str) -> String {
    format!("CASE WHEN ST_GeometryType({geom}) = 'ST_Point' THEN ST_Y({geom}) END AS lat, CASE WHEN ST_GeometryType({geom}) = 'ST_Point' THEN ST_X({geom}) END AS lon")
}

/// Every geometry-producing query appends this boolean LAST, so a mapped
/// property/parameter with the same name cannot shadow the validation field.
pub(crate) fn geometry_row_valid(row: &Row) -> bool {
    row.try_get::<_, bool>(row.len() - 1).unwrap_or(false)
}
