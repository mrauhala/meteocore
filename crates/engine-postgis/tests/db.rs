//! Database-backed tests of the PostGIS engine's EDR area query (#1062).
//!
//! They need PostgreSQL with PostGIS, and TimescaleDB for the timing table.
//! `MC_TEST_POSTGIS_DSN` names the database (a superuser, since the tests
//! create extensions and schemas), e.g.
//! `postgres://postgres:postgres@localhost:5432/postgres`. Without it every
//! test prints that it skipped and passes, unless `MC_TEST_POSTGIS_REQUIRED=1`,
//! which CI sets (`.github/workflows/postgis-db.yml`) so a missing database
//! fails there instead of skipping silently.
//!
//! Each test drops and rebuilds its own schema of synthetic data and reaches
//! the engine through its public load path: `PostgisEngineConfig::resolve`
//! (the DSN comes from the same environment variable), `PoolRegistry`,
//! `PostgisEngine::new`, `refresh_metadata`, then the area query. Run them
//! one at a time so the timing table measures one query at a time:
//!
//! ```sh
//! MC_TEST_POSTGIS_DSN=postgres://… cargo test -p engine-postgis --test db -- --nocapture --test-threads=1
//! ```

use std::collections::HashSet;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeDelta, Utc};
use ds_core::config::{
    PostgisConfig, PostgisObservationColumn, PostgisObservationTable, PostgisObservationsConfig,
    PostgisParameterConfig, PostgisStationsConfig,
};
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::model::{CoverageResponse, DomainDescription, QueryResult};
use engine_postgis::engine::AreaPath;
use engine_postgis::pool::PoolRegistry;
use engine_postgis::query::{
    build_area_set, params_as_refs, MAX_AREA_QUERIES, MAX_RESPONSE_VALUES, SET_BASED_MAX_WINDOW,
};
use engine_postgis::security::quote_ident;
use engine_postgis::{PostgisEngine, PostgisEngineConfig};
use tokio_postgres::NoTls;

const DSN_ENV: &str = "MC_TEST_POSTGIS_DSN";
const REQUIRED_ENV: &str = "MC_TEST_POSTGIS_REQUIRED";

/// Stations on a 200 × 100 grid, 0.1° apart, from (0°, 40°).
const STATIONS: usize = 20_000;
/// A polygon around every station.
const NETWORK: &str = "POLYGON((-1 39,21 39,21 51,-1 51,-1 39))";
/// A polygon around 2 500 of them: longitudes 1.0–5.9, latitudes 41.0–45.9.
const BLOCK: &str = "POLYGON((0.95 40.95,5.95 40.95,5.95 45.95,0.95 45.95,0.95 40.95))";

/// One DDL step at a time, so two tests run in parallel never race on
/// `CREATE EXTENSION`.
static SETUP: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The DSN, or `None` (printed) when the database tests are not configured.
fn dsn() -> Option<String> {
    match std::env::var(DSN_ENV) {
        Ok(dsn) if !dsn.trim().is_empty() => Some(dsn),
        _ if std::env::var(REQUIRED_ENV).is_ok_and(|v| v == "1") => {
            panic!("{REQUIRED_ENV}=1 but {DSN_ENV} is not set: the database tests must not skip")
        }
        _ => {
            println!("skipped: set {DSN_ENV} to a PostGIS database to run the engine-postgis database tests");
            None
        }
    }
}

/// A setup connection, retried for a minute while a fresh service container
/// finishes starting. The DSN itself is never printed: it holds a password.
async fn connect(dsn: &str) -> tokio_postgres::Client {
    let mut last = None;
    for _ in 0..60 {
        match tokio_postgres::connect(dsn, NoTls).await {
            Ok((client, connection)) => {
                tokio::spawn(async move {
                    if let Err(e) = connection.await {
                        eprintln!("setup connection closed: {e}");
                    }
                });
                return client;
            }
            Err(e) => {
                last = Some(e);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
    panic!("cannot connect to the database in {DSN_ENV}: {last:?}");
}

/// Run `sql` with every `__S__` replaced by the quoted `schema`. The
/// statements are fixed test fixtures; the schema name is a constant.
async fn exec(client: &tokio_postgres::Client, schema: &str, sql: &str) {
    let quoted = quote_ident(schema).expect("schema identifier");
    let sql = sql.replace("__S__", &quoted);
    if let Err(e) = client.batch_execute(&sql).await {
        panic!("setup of {schema} failed: {e:?}");
    }
}

/// `2026-01-01T06:00:00Z`: the instant every station reports at.
fn t0() -> DateTime<Utc> {
    "2026-01-01T06:00:00Z".parse().unwrap()
}

fn hours(h: i64) -> TimeDelta {
    TimeDelta::hours(h)
}

/// The stations table and the per-station hourly source rows every shape is
/// filled from: 18 hours ending at `t0()` (h = 0 is `t0()`).
///
/// - `t2m` is NULL where `(n + h) % 11 = 0`: a row with a null value.
/// - `rh` is NULL where `(n + h) % 7 = 0`; the `long` and `per_parameter`
///   shapes leave those rows out, so a station misses `rh` at that hour.
/// - Stations with `n % 13 = 0` have no row 1 to 3 hours before `t0()`, so
///   a window over just those hours leaves them out of the answer.
///
/// Every station reports at `t0()`. Over the 18 hours the two parameters
/// carry about 660 000 values, past the 500 000-value budget.
const STATIONS_AND_SOURCE: &str = "
DROP SCHEMA IF EXISTS __S__ CASCADE;
CREATE SCHEMA __S__;
CREATE TABLE __S__.stations (
    id text PRIMARY KEY,
    name text NOT NULL,
    n integer NOT NULL,
    geom geometry(Point, 4326) NOT NULL
);
INSERT INTO __S__.stations (id, name, n, geom)
SELECT 's' || lpad(n::text, 5, '0'), 'Station ' || n, n,
       ST_SetSRID(ST_MakePoint(((n - 1) % 200) * 0.1::float8,
                               40 + ((n - 1) / 200) * 0.1::float8), 4326)
FROM generate_series(1, 20000) AS n;
CREATE INDEX ON __S__.stations USING gist (geom);
CREATE TABLE __S__.src AS
SELECT s.id AS station_id, s.geom, t.h, t.time,
       (CASE WHEN (s.n + t.h) % 11 = 0 THEN NULL ELSE (s.n % 97) + t.h * 0.5 END)::float8 AS t2m,
       (CASE WHEN (s.n + t.h) % 7 = 0 THEN NULL ELSE 50 + (s.n % 41) + t.h END)::float8 AS rh
FROM __S__.stations s
CROSS JOIN (
    SELECT h, TIMESTAMPTZ '2026-01-01 06:00:00+00' - h * INTERVAL '1 hour' AS time
    FROM generate_series(0, 17) AS h
) t
WHERE NOT (s.n % 13 = 0 AND t.h BETWEEN 1 AND 3);
";

/// `long`, stations-only: the polygon test runs in SQL.
const LONG: &str = "
CREATE TABLE __S__.obs (
    station_id text NOT NULL,
    obstime timestamptz NOT NULL,
    param text NOT NULL,
    value double precision
);
INSERT INTO __S__.obs
SELECT station_id, time, 't2m', t2m FROM __S__.src
UNION ALL
SELECT station_id, time, 'rh', rh FROM __S__.src WHERE rh IS NOT NULL;
CREATE INDEX ON __S__.obs (station_id, obstime DESC);
DROP TABLE __S__.src;
ANALYZE;
";

/// `wide`, mode B (stations table + observation geometry), with a
/// `timestamp without time zone` column holding UTC.
const WIDE: &str = "
CREATE TABLE __S__.synop (
    station_id text NOT NULL,
    valid_time timestamp NOT NULL,
    temp_c double precision,
    humidity double precision,
    geom geometry(Point, 4326)
);
INSERT INTO __S__.synop
SELECT station_id, time AT TIME ZONE 'UTC', t2m, rh, geom FROM __S__.src;
CREATE INDEX ON __S__.synop (station_id, valid_time DESC);
DROP TABLE __S__.src;
ANALYZE;
";

/// `per_parameter`, mode A (no stations table, locations from the
/// observation tables' geometry).
const PER_PARAMETER: &str = "
CREATE TABLE __S__.obs_t2m (
    wigos_id text NOT NULL,
    time timestamptz NOT NULL,
    value double precision,
    the_geom geometry(Point, 4326)
);
CREATE TABLE __S__.obs_rh (LIKE __S__.obs_t2m);
INSERT INTO __S__.obs_t2m SELECT station_id, time, t2m, geom FROM __S__.src;
INSERT INTO __S__.obs_rh SELECT station_id, time, rh, geom FROM __S__.src WHERE rh IS NOT NULL;
CREATE INDEX ON __S__.obs_t2m (wigos_id, time DESC);
CREATE INDEX ON __S__.obs_rh (wigos_id, time DESC);
DROP TABLE __S__.src;
DROP TABLE __S__.stations;
ANALYZE;
";

fn parameters() -> Vec<PostgisParameterConfig> {
    [
        ("t2m", "Air temperature", "°C"),
        ("rh", "Relative humidity", "%"),
    ]
    .into_iter()
    .map(|(name, label, unit)| PostgisParameterConfig {
        name: name.into(),
        label: label.into(),
        unit: unit.into(),
        observed_property: None,
        source_key: None,
        valid_range: None,
        missing_values: vec![],
    })
    .collect()
}

fn observations(shape: &str) -> PostgisObservationsConfig {
    PostgisObservationsConfig {
        shape: shape.into(),
        table: None,
        station_fk_col: None,
        time_col: None,
        time_col_tz: None,
        param_col: None,
        value_col: None,
        geom_col: None,
        locations_window: None,
        columns: vec![],
        tables: vec![],
        id_col: None,
        cloud_indicator_col: None,
        peak_current_col: None,
        default_datetime: None,
        extent_bbox: None,
        coverage_bbox: None,
    }
}

fn stations(schema: &str) -> PostgisStationsConfig {
    PostgisStationsConfig {
        table: format!("{schema}.stations"),
        id_col: "id".into(),
        label_col: "name".into(),
        geom_col: "geom".into(),
        property_cols: vec![],
        where_clause: None,
    }
}

fn config(
    stations: Option<PostgisStationsConfig>,
    observations: PostgisObservationsConfig,
) -> PostgisConfig {
    PostgisConfig {
        dsn_env: DSN_ENV.into(),
        pool_size: Some(16),
        pool_label: None,
        metadata_refresh_secs: None,
        max_locations: None,
        stations,
        observations,
        parameters: parameters(),
        map: None,
    }
}

fn long_config(schema: &str) -> PostgisConfig {
    config(
        Some(stations(schema)),
        PostgisObservationsConfig {
            table: Some(format!("{schema}.obs")),
            station_fk_col: Some("station_id".into()),
            time_col: Some("obstime".into()),
            param_col: Some("param".into()),
            value_col: Some("value".into()),
            ..observations("long")
        },
    )
}

fn wide_config(schema: &str) -> PostgisConfig {
    config(
        Some(stations(schema)),
        PostgisObservationsConfig {
            table: Some(format!("{schema}.synop")),
            station_fk_col: Some("station_id".into()),
            time_col: Some("valid_time".into()),
            time_col_tz: Some("UTC".into()),
            geom_col: Some("geom".into()),
            locations_window: Some("all".into()),
            columns: vec![
                PostgisObservationColumn {
                    parameter: "t2m".into(),
                    column: "temp_c".into(),
                },
                PostgisObservationColumn {
                    parameter: "rh".into(),
                    column: "humidity".into(),
                },
            ],
            ..observations("wide")
        },
    )
}

fn per_parameter_config(schema: &str) -> PostgisConfig {
    let table = |parameter: &str, table: &str| PostgisObservationTable {
        parameter: parameter.into(),
        table: format!("{schema}.{table}"),
        station_fk_col: None,
        time_col: None,
        time_col_tz: None,
        value_col: None,
        geom_col: None,
    };
    config(
        None,
        PostgisObservationsConfig {
            station_fk_col: Some("wigos_id".into()),
            time_col: Some("time".into()),
            value_col: Some("value".into()),
            geom_col: Some("the_geom".into()),
            locations_window: Some("all".into()),
            tables: vec![table("t2m", "obs_t2m"), table("rh", "obs_rh")],
            ..observations("per_parameter")
        },
    )
}

/// Build the engine the way the server does and load its metadata.
async fn engine(id: &str, cfg: &PostgisConfig) -> Arc<PostgisEngine> {
    let validated = PostgisEngineConfig::resolve(cfg).expect("resolve the test config");
    let size = NonZeroU32::new(validated.pool_size).expect("pool size");
    let pool = PoolRegistry::new()
        .get_or_create(&validated.dsn, size)
        .expect("pool");
    let engine = Arc::new(PostgisEngine::new(id, Arc::new(validated), pool));
    engine.refresh_metadata().await.expect("metadata refresh");
    engine
}

/// The area query on `path` (`None`: the engine's choice), on a blocking
/// thread as the EDR executor runs it.
async fn area(
    engine: &Arc<PostgisEngine>,
    path: Option<AreaPath>,
    coords: &str,
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
    parameters: Option<&[&str]>,
) -> Result<Vec<QueryResult>, DataServerError> {
    let engine = Arc::clone(engine);
    let coords = coords.to_string();
    let parameters: Option<Vec<String>> =
        parameters.map(|p| p.iter().map(|s| s.to_string()).collect());
    tokio::task::spawn_blocking(move || {
        match engine.query_area_on(path, &coords, window, parameters.as_deref())? {
            CoverageResponse::Collection(coverages) => Ok(coverages),
            CoverageResponse::Single(coverage) => Ok(vec![coverage]),
        }
    })
    .await
    .expect("area query task")
}

/// A coverage as comparable data: position, time axis, and every parameter's
/// unit and values, sorted by name.
#[derive(Debug, PartialEq)]
struct Coverage {
    x: f64,
    y: f64,
    t: Vec<DateTime<Utc>>,
    ranges: Vec<(String, String, Vec<Option<f64>>)>,
}

fn comparable(coverages: &[QueryResult]) -> Vec<Coverage> {
    coverages
        .iter()
        .map(|c| {
            let DomainDescription::PointSeries { x, y, t, .. } = &c.domain else {
                panic!("a station answers a PointSeries, got {:?}", c.domain);
            };
            let mut ranges: Vec<_> = c
                .ranges
                .iter()
                .map(|(name, nd)| {
                    let unit = c.parameters[name].unit.clone();
                    assert_eq!(nd.values.len(), t.len(), "{name}: one value per time");
                    (name.clone(), unit, nd.values.clone())
                })
                .collect();
            ranges.sort_by(|a, b| a.0.cmp(&b.0));
            Coverage {
                x: *x,
                y: *y,
                t: t.clone(),
                ranges,
            }
        })
        .collect()
}

fn point(x: f64, y: f64) -> (u64, u64) {
    (x.to_bits(), y.to_bits())
}

/// The fixture's station number `n` at `(x, y)`: the inverse of the grid in
/// `STATIONS_AND_SOURCE`.
fn station_number(x: f64, y: f64) -> i64 {
    let column = (x / 0.1).round() as i64;
    let row = ((y - 40.0) / 0.1).round() as i64;
    let n = row * 200 + column + 1;
    assert!(
        (1..=STATIONS as i64).contains(&n),
        "({x}, {y}) is no station"
    );
    n
}

/// `station_number` inverts the fixture's grid for every station, with the
/// same float arithmetic the SQL uses (`int * 0.1::float8`, `40 + …`). Needs
/// no database.
#[test]
fn fixture_grid_inverts_to_station_numbers() {
    for n in 1..=STATIONS as i64 {
        let x = ((n - 1) % 200) as f64 * 0.1;
        let y = 40.0 + ((n - 1) / 200) as f64 * 0.1;
        assert_eq!(station_number(x, y), n);
    }
    let without_rh = (1..=STATIONS as i64)
        .filter(|&n| expected_at_t0(Shape::Long, n).len() == 1)
        .count();
    assert_eq!(without_rh, STATIONS / 7);
}

/// The observation shape under test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Long,
    Wide,
    PerParameter,
}

/// A station's ranges at `t0()` (h = 0), sorted by name, from the fixture's
/// formulas: `t2m` is `n % 97`, null where `n % 11 = 0`; `rh` is
/// `50 + n % 41`, null where `n % 7 = 0`. That null `rh` is a NULL in the
/// `wide` column and a missing row in `long` and `per_parameter`. A
/// `per_parameter` table without a row in the window is still a range of
/// nulls, but a `long` key without rows has no range at all.
fn expected_at_t0(shape: Shape, n: i64) -> Vec<(String, Vec<Option<f64>>)> {
    let t2m = (n % 11 != 0).then_some((n % 97) as f64);
    let rh = (n % 7 != 0).then_some((50 + n % 41) as f64);
    let mut ranges = Vec::new();
    if rh.is_some() || shape != Shape::Long {
        ranges.push(("rh".to_string(), vec![rh]));
    }
    ranges.push(("t2m".to_string(), vec![t2m]));
    ranges
}

/// What #1062 asks of every shape, on one dataset:
///
/// 1. every station's two parameters at one instant: a 200 with all 20 000
///    stations (the old 10 000-station ceiling made it a 400), each with
///    the fixture's values;
/// 2. the same request over a week: the 400 that names the value budget;
/// 3. the set-based and fan-out paths: the same values in the same order.
async fn check_shape(engine: &Arc<PostgisEngine>, shape: Shape) {
    let both: &[&str] = &["t2m", "rh"];
    let locations: HashSet<(u64, u64)> = engine
        .get_locations()
        .unwrap()
        .iter()
        .map(|l| point(l.longitude, l.latitude))
        .collect();
    assert_eq!(
        locations.len(),
        STATIONS,
        "the fixture's stations are loaded"
    );

    // 1. One instant over the whole network.
    let instant = Some((t0(), t0()));
    let started = Instant::now();
    let coverages = area(engine, None, NETWORK, instant, Some(both))
        .await
        .expect("an instant over every station fits the value budget");
    println!(
        "  every station at one instant: {} coverages in {} ms",
        coverages.len(),
        started.elapsed().as_millis()
    );
    assert_eq!(coverages.len(), STATIONS, "every station is in the answer");
    let mut without_rh = 0;
    let answered: HashSet<(u64, u64)> = comparable(&coverages)
        .iter()
        .map(|c| {
            assert_eq!(c.t, vec![t0()]);
            let n = station_number(c.x, c.y);
            let values: Vec<(String, Vec<Option<f64>>)> = c
                .ranges
                .iter()
                .map(|(name, _, values)| (name.clone(), values.clone()))
                .collect();
            assert_eq!(values, expected_at_t0(shape, n), "{shape:?}, station {n}");
            without_rh += usize::from(c.ranges.len() == 1);
            point(c.x, c.y)
        })
        .collect();
    assert_eq!(answered, locations, "each station once, at its location");
    // `long` answers `t2m` alone for the 2 857 stations with n % 7 = 0.
    let expected_without_rh = if shape == Shape::Long {
        STATIONS / 7
    } else {
        0
    };
    assert_eq!(
        without_rh, expected_without_rh,
        "{shape:?}: stations without rh"
    );

    // 2. The same request over a week is past the value budget, whichever
    // path the engine takes and on both paths.
    let week = Some((t0() - TimeDelta::days(7), t0()));
    for path in [None, Some(AreaPath::SetBased), Some(AreaPath::FanOut)] {
        match area(engine, path, NETWORK, week, Some(both)).await {
            Err(DataServerError::QueryTooLarge(msg)) => {
                let named = msg.contains(&format!("{MAX_RESPONSE_VALUES}-value budget"))
                    || (path == Some(AreaPath::FanOut) && msg.contains("SQL queries"));
                assert!(named, "{path:?}: {msg}");
                if path.is_none() {
                    assert!(
                        msg.contains(&format!("{MAX_RESPONSE_VALUES}-value budget")),
                        "the engine's own choice names the value budget: {msg}"
                    );
                }
            }
            Ok(c) => panic!(
                "{path:?}: a week is past the value budget, got {} coverages",
                c.len()
            ),
            Err(e) => panic!("{path:?}: expected QueryTooLarge, got {e:?}"),
        }
    }

    // 3. Both paths on the same data: identical values, identical order.
    let windows = [
        ("an instant", instant),
        ("three hours", Some((t0() - hours(3), t0()))),
        (
            "hours some stations skip",
            Some((t0() - hours(3), t0() - hours(1))),
        ),
        ("no datetime", None),
    ];
    for (what, window) in windows {
        for parameters in [None, Some(&["rh"][..]), Some(both)] {
            let set = area(engine, Some(AreaPath::SetBased), BLOCK, window, parameters)
                .await
                .unwrap_or_else(|e| panic!("set-based, {what}, {parameters:?}: {e:?}"));
            let fan = area(engine, Some(AreaPath::FanOut), BLOCK, window, parameters)
                .await
                .unwrap_or_else(|e| panic!("fan-out, {what}, {parameters:?}: {e:?}"));
            assert!(
                !set.is_empty(),
                "{what}, {parameters:?}: the block has data"
            );
            assert!(set.len() <= 2_500);
            let (set, fan) = (comparable(&set), comparable(&fan));
            assert_eq!(
                set.len(),
                fan.len(),
                "{what}, {parameters:?}: coverage count"
            );
            for (i, (s, f)) in set.iter().zip(&fan).enumerate() {
                assert_eq!(s, f, "{what}, {parameters:?}: coverage {i}");
            }
        }
    }
    // The skipped hours really leave stations out (n % 13 = 0), on both paths.
    let skipping = area(
        engine,
        None,
        BLOCK,
        Some((t0() - hours(3), t0() - hours(1))),
        Some(both),
    )
    .await
    .unwrap();
    assert!(skipping.len() < 2_500, "{} stations", skipping.len());
}

async fn run_shape(schema: &str, shape: Shape, shape_sql: &str, cfg: fn(&str) -> PostgisConfig) {
    let Some(dsn) = dsn() else {
        return;
    };
    let client = connect(&dsn).await;
    {
        let _setup = SETUP.lock().await;
        exec(&client, schema, "CREATE EXTENSION IF NOT EXISTS postgis;").await;
        exec(&client, schema, STATIONS_AND_SOURCE).await;
        exec(&client, schema, shape_sql).await;
    }
    println!("{schema}:");
    let engine = engine(schema, &cfg(schema)).await;
    check_shape(&engine, shape).await;
    exec(&client, schema, "DROP SCHEMA __S__ CASCADE;").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn long_shape_stations_only() {
    run_shape("mc1062_long", Shape::Long, LONG, long_config).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wide_shape_with_observation_geometry() {
    run_shape("mc1062_wide", Shape::Wide, WIDE, wide_config).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn per_parameter_shape_without_stations_table() {
    run_shape(
        "mc1062_per_parameter",
        Shape::PerParameter,
        PER_PARAMETER,
        per_parameter_config,
    )
    .await;
}

/// A `long` hypertable of 20 000 stations × 73 ten-minute steps (12 hours):
/// one parameter, chunks of 6 hours, the recommended `(station, time)` index.
const TIMING: &str = "
CREATE TABLE __S__.obs (
    station_id text NOT NULL,
    obstime timestamptz NOT NULL,
    param text NOT NULL,
    value double precision
);
SELECT create_hypertable('__S__.obs', 'obstime', chunk_time_interval => INTERVAL '6 hours');
INSERT INTO __S__.obs
SELECT s.id, TIMESTAMPTZ '2026-01-01 12:00:00+00' - m * INTERVAL '10 minutes', 't2m',
       (s.n % 97) + m * 0.01
FROM __S__.stations s CROSS JOIN generate_series(0, 72) AS m;
CREATE INDEX ON __S__.obs (station_id, obstime DESC);
DROP TABLE __S__.src;
ANALYZE;
";

/// The faster of two timed runs, in milliseconds, and the coverage count.
async fn time_path(
    engine: &Arc<PostgisEngine>,
    path: AreaPath,
    coords: &str,
    window: (DateTime<Utc>, DateTime<Utc>),
) -> (u128, usize) {
    let mut best = u128::MAX;
    let mut count = 0;
    for _ in 0..2 {
        let started = Instant::now();
        let coverages = area(engine, Some(path), coords, Some(window), None)
            .await
            .unwrap_or_else(|e| panic!("{path:?}: {e:?}"));
        best = best.min(started.elapsed().as_millis());
        count = coverages.len();
    }
    (best, count)
}

/// Both paths timed on a TimescaleDB hypertable by stations × steps, the
/// numbers `SET_BASED_MAX_WINDOW` is tuned from, then each path's time to the
/// value-budget 400 for a week over every station and the set-based
/// statement's `EXPLAIN ANALYZE`. Prints them; never fails on a timing.
/// Skips (printed) where TimescaleDB is not installed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timing_set_based_and_fan_out_on_a_hypertable() {
    let Some(dsn) = dsn() else {
        return;
    };
    let schema = "mc1062_timing";
    let client = connect(&dsn).await;
    {
        let _setup = SETUP.lock().await;
        if let Err(e) = client
            .batch_execute("CREATE EXTENSION IF NOT EXISTS timescaledb;")
            .await
        {
            println!("timing table skipped: TimescaleDB is not available ({e})");
            return;
        }
        exec(&client, schema, "CREATE EXTENSION IF NOT EXISTS postgis;").await;
        exec(&client, schema, STATIONS_AND_SOURCE).await;
        exec(&client, schema, TIMING).await;
    }
    let engine = engine(schema, &long_config(schema)).await;
    let end: DateTime<Utc> = "2026-01-01T12:00:00Z".parse().unwrap();

    println!(
        "\nArea query on a TimescaleDB hypertable, 10-minute steps, one parameter \
         (fastest of two runs; set-based runs up to a {} min window):",
        SET_BASED_MAX_WINDOW.num_minutes()
    );
    println!(
        "{:>8} {:>6} {:>8} {:>14} {:>12} {:>10}",
        "stations", "steps", "values", "set-based ms", "fan-out ms", "fan/set"
    );
    for rows in [2usize, 10, 25, 100] {
        let stations = rows * 200;
        let north = 40.0 + rows as f64 * 0.1 - 0.05;
        let coords =
            format!("POLYGON((-0.05 39.95,19.95 39.95,19.95 {north},-0.05 {north},-0.05 39.95))");
        // Warm the cache for these stations before timing either path.
        let _ = area(
            &engine,
            Some(AreaPath::SetBased),
            &coords,
            Some((end - hours(12), end)),
            None,
        )
        .await;
        for steps in [1i64, 7, 37, 73] {
            let values = stations * steps as usize;
            if values > MAX_RESPONSE_VALUES || stations > MAX_AREA_QUERIES {
                println!(
                    "{stations:>8} {steps:>6} {values:>8} {:>14} {:>12} {:>10}",
                    "over budget", "-", "-"
                );
                continue;
            }
            let window = (end - TimeDelta::minutes(10 * (steps - 1)), end);
            let (set_ms, set_n) = time_path(&engine, AreaPath::SetBased, &coords, window).await;
            let (fan_ms, fan_n) = time_path(&engine, AreaPath::FanOut, &coords, window).await;
            assert_eq!(set_n, fan_n, "both paths answer the same stations");
            println!(
                "{stations:>8} {steps:>6} {values:>8} {set_ms:>14} {fan_ms:>12} {:>10.2}",
                fan_ms as f64 / set_ms.max(1) as f64
            );
        }
    }

    // Time to the 400 for a week over every station: 20 000 × 73 = 1.46 M
    // values, about three times the budget. The engine runs a long window
    // set-based when the fan-out could not take it (more than
    // MAX_AREA_QUERIES statements), and there the 400 is cheap only if the
    // database stops at the statement's LIMIT: an ordered scan of the
    // `(station, time)` index under an incremental sort, not a sort of every
    // row in the window. The plan printed below shows which one it chose.
    let week = (end - TimeDelta::days(7), end);
    println!("\nTime to the value-budget 400, a week over {STATIONS} stations:");
    for path in [AreaPath::SetBased, AreaPath::FanOut] {
        let started = Instant::now();
        let outcome = area(&engine, Some(path), NETWORK, Some(week), None).await;
        let ms = started.elapsed().as_millis();
        match outcome {
            Err(DataServerError::QueryTooLarge(msg)) => {
                assert!(
                    msg.contains(&format!("{MAX_RESPONSE_VALUES}-value budget")),
                    "{path:?}: {msg}"
                );
                println!("{:>12} {ms:>8} ms", format!("{path:?}"));
            }
            Ok(c) => panic!(
                "{path:?}: a week is past the value budget, got {} coverages",
                c.len()
            ),
            Err(e) => panic!("{path:?}: expected QueryTooLarge, got {e:?}"),
        }
    }
    let cfg = PostgisEngineConfig::resolve(&long_config(schema)).expect("resolve the test config");
    let ids: Arc<Vec<String>> = Arc::new(
        engine
            .get_locations()
            .unwrap()
            .iter()
            .map(|l| l.id.clone())
            .collect(),
    );
    let statements = build_area_set(&cfg, &ids, Some(week), &["t2m"], MAX_RESPONSE_VALUES + 1)
        .expect("build the set-based statement");
    for q in &statements {
        let explain = ["EXPLAIN (ANALYZE, BUFFERS) ", q.sql.as_str()].concat();
        match client
            .query(explain.as_str(), &params_as_refs(&q.params))
            .await
        {
            Ok(rows) => {
                println!("\nPlan of the set-based statement for that week:");
                for row in rows {
                    let line: String = row.get(0);
                    println!("  {line}");
                }
            }
            // Diagnostics only: the table above stands without it.
            Err(e) => println!("EXPLAIN of the set-based statement failed: {e}"),
        }
    }
    exec(&client, schema, "DROP SCHEMA __S__ CASCADE;").await;
}

/// The station map layers' fixture (#1063): three stations 2° apart, `t2m`
/// rows around the 06:00 frame, in a `long` table and a `wide` one with a
/// naive UTC time column.
///
/// - `fresh` reports at 05:30 and 05:50 (its value at 06:00 is 2.0) and at
///   06:10, after the frame.
/// - `stale` last reported at 04:30, more than an hour before 06:00.
/// - `nulls` reported only a NULL `t2m`, at 05:55.
const STATION_MAP: &str = "
DROP SCHEMA IF EXISTS __S__ CASCADE;
CREATE SCHEMA __S__;
CREATE TABLE __S__.stations (
    id text PRIMARY KEY,
    name text NOT NULL,
    geom geometry(Point, 4326) NOT NULL
);
INSERT INTO __S__.stations VALUES
    ('fresh', 'Fresh', ST_SetSRID(ST_MakePoint(1.05, 1.05), 4326)),
    ('stale', 'Stale', ST_SetSRID(ST_MakePoint(3.05, 3.05), 4326)),
    ('nulls', 'Nulls', ST_SetSRID(ST_MakePoint(5.05, 5.05), 4326));
CREATE TABLE __S__.obs (
    station_id text NOT NULL,
    obstime timestamptz NOT NULL,
    param text NOT NULL,
    value double precision
);
INSERT INTO __S__.obs VALUES
    ('fresh', '2026-01-01 05:30:00+00', 't2m', 1.0),
    ('fresh', '2026-01-01 05:50:00+00', 't2m', 2.0),
    ('fresh', '2026-01-01 06:10:00+00', 't2m', 99.0),
    ('stale', '2026-01-01 04:30:00+00', 't2m', 7.0),
    ('nulls', '2026-01-01 05:55:00+00', 't2m', NULL);
CREATE INDEX ON __S__.obs (station_id, obstime DESC);
CREATE TABLE __S__.synop (
    station_id text NOT NULL,
    valid_time timestamp NOT NULL,
    temp_c double precision,
    humidity double precision,
    geom geometry(Point, 4326)
);
INSERT INTO __S__.synop
SELECT o.station_id, o.obstime AT TIME ZONE 'UTC', o.value, 50, s.geom
FROM __S__.obs o JOIN __S__.stations s ON s.id = o.station_id;
CREATE INDEX ON __S__.synop (station_id, valid_time DESC);
ANALYZE;
";

/// The `t2m` layer at `at` over 0°–10° in 100 × 100 CRS:84 pixels, on a
/// blocking thread as the render executor draws it.
async fn station_frame(engine: &Arc<PostgisEngine>, at: &str) -> ds_core::map_engine::RasterTile {
    use ds_core::map_engine::{MapEngine, OutputCrs};
    let engine = Arc::clone(engine);
    let time: DateTime<Utc> = at.parse().unwrap();
    tokio::task::spawn_blocking(move || {
        engine.get_raster_tile(
            [0.0, 0.0, 10.0, 10.0],
            100,
            100,
            Some(time),
            &OutputCrs::Wgs84,
            Some("t2m"),
            None,
            None,
        )
    })
    .await
    .expect("render task")
    .expect("station frame")
}

/// The value at the pixel holding a fixture station: (1.05, 1.05) is column
/// 10, row 89; (3.05, 3.05) column 30, row 69; (5.05, 5.05) column 50, row 49.
/// The value-filter fixture's stations stand in the same places.
fn at_station(tile: &ds_core::map_engine::RasterTile, station: &str) -> Option<f64> {
    let (col, row) = match station {
        "fresh" | "ok1" => (10, 89),
        "stale" | "ok2" => (30, 69),
        "nulls" | "bad" => (50, 49),
        other => panic!("no fixture station {other}"),
    };
    tile.values.value_at(row * 100 + col)
}

/// What #1063 asks of a station layer, on one shape:
///
/// 1. a frame draws each station's latest value within `max_age` before it
///    (`fresh` 2.0, not its later 99.0), and no marker for a station
///    without one (`stale`, `nulls`);
/// 2. the content version holds over a refresh that found nothing new, and
///    a late row moves it, after which the frame shows that row;
/// 3. with `draw_missing`, stations without a value are nodata markers.
async fn check_station_map(schema: &str, cfg: PostgisConfig, late_row: &str) {
    use ds_core::map_engine::MapEngine;
    let Some(dsn) = dsn() else {
        return;
    };
    let client = connect(&dsn).await;
    {
        let _setup = SETUP.lock().await;
        exec(&client, schema, "CREATE EXTENSION IF NOT EXISTS postgis;").await;
        exec(&client, schema, STATION_MAP).await;
    }
    let symbols = || {
        engine_postgis::StationSymbols::new(std::collections::HashMap::from([(
            "t2m".to_string(),
            0.0,
        )]))
    };
    let layer = engine(schema, &cfg).await;
    layer.set_station_symbols(symbols());
    layer.refresh_metadata().await.expect("refresh");
    assert_eq!(
        layer.resolve_time(Some("2026-01-01T06:00:30Z".parse().unwrap()), None),
        Some(t0()),
        "an explicit TIME resolves to its 10-minute step"
    );

    // 1. The frame at 06:00.
    let tile = station_frame(&layer, "2026-01-01T06:00:00Z").await;
    assert_eq!(at_station(&tile, "fresh"), Some(2.0));
    assert_eq!(at_station(&tile, "stale"), None, "older than max_age");
    assert_eq!(at_station(&tile, "nulls"), None, "only a null value");
    let later = station_frame(&layer, "2026-01-01T06:10:00Z").await;
    assert_eq!(at_station(&later, "fresh"), Some(99.0));

    // 2. Late data.
    let version = layer.content_version();
    assert_ne!(version, 0);
    layer.refresh_metadata().await.expect("refresh");
    assert_eq!(
        layer.content_version(),
        version,
        "a refresh that found nothing new keeps the version"
    );
    exec(&client, schema, late_row).await;
    layer.refresh_metadata().await.expect("refresh");
    assert_ne!(
        layer.content_version(),
        version,
        "a late row moves the version"
    );
    let tile = station_frame(&layer, "2026-01-01T06:00:00Z").await;
    assert_eq!(
        at_station(&tile, "stale"),
        Some(8.0),
        "the late row is drawn"
    );

    // 3. Nodata markers.
    let mut cfg = cfg;
    cfg.map = Some(ds_core::config::StationMapConfig {
        draw_missing: Some(true),
        ..Default::default()
    });
    let with_missing = engine(schema, &cfg).await;
    with_missing.set_station_symbols(symbols());
    let tile = station_frame(&with_missing, "2026-01-01T06:00:00Z").await;
    assert_eq!(at_station(&tile, "fresh"), Some(2.0));
    assert_eq!(
        at_station(&tile, "nulls"),
        Some(engine_postgis::NODATA_MARKER)
    );
    exec(&client, schema, "DROP SCHEMA __S__ CASCADE;").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn station_map_long_shape() {
    let schema = "mc1063_long";
    check_station_map(
        schema,
        long_config(schema),
        "INSERT INTO __S__.obs VALUES ('stale', '2026-01-01 05:45:00+00', 't2m', 8.0);",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn station_map_wide_shape() {
    let schema = "mc1063_wide";
    check_station_map(
        schema,
        wide_config(schema),
        "INSERT INTO __S__.synop SELECT 'stale', TIMESTAMP '2026-01-01 05:45:00', 8.0, 50, geom \
         FROM __S__.stations WHERE id = 'stale';",
    )
    .await;
}

/// The value-filter fixture (#1210): three stations where the station map's
/// stand, `t2m` and `rh` rows from 05:20 to 05:50 in every station shape's
/// tables: a `long` table, a `wide` one with a naive UTC column, and one
/// table per parameter. With [`filtered_parameters`], `t2m` keeps
/// `[-80, 60]` without `-32766` and `-9.5`, and `rh` drops `999`.
///
/// - `ok1` (1.05, 1.05): `t2m` 60 at 05:20 (the range's top, kept), 1 at
///   05:30, the sentinel -9.5 at 05:50: its latest valid `t2m` is 1.
/// - `ok2` (3.05, 3.05): `t2m` 2 at 05:30, 75 at 05:50 (out of range): its
///   latest valid `t2m` is 2. `rh` is the sentinel 999 at 05:30 and -32766
///   at 05:50, which is no `rh` sentinel and kept.
/// - `bad` (5.05, 5.05): `t2m` the sentinel -32766 at 05:40 and NaN at 05:45
///   (outside every range): no valid `t2m`.
const VALUE_FILTERS: &str = "
DROP SCHEMA IF EXISTS __S__ CASCADE;
CREATE SCHEMA __S__;
CREATE TABLE __S__.stations (
    id text PRIMARY KEY,
    name text NOT NULL,
    geom geometry(Point, 4326) NOT NULL
);
INSERT INTO __S__.stations VALUES
    ('ok1', 'Ok 1', ST_SetSRID(ST_MakePoint(1.05, 1.05), 4326)),
    ('ok2', 'Ok 2', ST_SetSRID(ST_MakePoint(3.05, 3.05), 4326)),
    ('bad', 'Bad', ST_SetSRID(ST_MakePoint(5.05, 5.05), 4326));
CREATE TABLE __S__.src (
    station_id text NOT NULL,
    obstime timestamptz NOT NULL,
    t2m double precision,
    rh double precision
);
INSERT INTO __S__.src VALUES
    ('ok1', '2026-01-01 05:20:00+00', 60, 40),
    ('ok1', '2026-01-01 05:30:00+00', 1, 50),
    ('ok1', '2026-01-01 05:50:00+00', -9.5, 60),
    ('ok2', '2026-01-01 05:30:00+00', 2, 999),
    ('ok2', '2026-01-01 05:50:00+00', 75, -32766),
    ('bad', '2026-01-01 05:40:00+00', -32766, 70),
    ('bad', '2026-01-01 05:45:00+00', 'NaN', 70);
CREATE TABLE __S__.obs (
    station_id text NOT NULL,
    obstime timestamptz NOT NULL,
    param text NOT NULL,
    value double precision
);
INSERT INTO __S__.obs
SELECT station_id, obstime, 't2m', t2m FROM __S__.src
UNION ALL
SELECT station_id, obstime, 'rh', rh FROM __S__.src;
CREATE INDEX ON __S__.obs (station_id, obstime DESC);
CREATE TABLE __S__.synop (
    station_id text NOT NULL,
    valid_time timestamp NOT NULL,
    temp_c double precision,
    humidity double precision,
    geom geometry(Point, 4326)
);
INSERT INTO __S__.synop
SELECT o.station_id, o.obstime AT TIME ZONE 'UTC', o.t2m, o.rh, s.geom
FROM __S__.src o JOIN __S__.stations s ON s.id = o.station_id;
CREATE INDEX ON __S__.synop (station_id, valid_time DESC);
CREATE TABLE __S__.obs_t2m (
    wigos_id text NOT NULL,
    time timestamptz NOT NULL,
    value double precision,
    the_geom geometry(Point, 4326)
);
CREATE TABLE __S__.obs_rh (LIKE __S__.obs_t2m);
INSERT INTO __S__.obs_t2m
SELECT o.station_id, o.obstime, o.t2m, s.geom
FROM __S__.src o JOIN __S__.stations s ON s.id = o.station_id;
INSERT INTO __S__.obs_rh
SELECT o.station_id, o.obstime, o.rh, s.geom
FROM __S__.src o JOIN __S__.stations s ON s.id = o.station_id;
DROP TABLE __S__.src;
ANALYZE;
";

/// [`parameters`] with the value-filter fixture's filters.
fn filtered_parameters() -> Vec<PostgisParameterConfig> {
    let mut parameters = parameters();
    parameters[0].valid_range = Some(vec![-80.0, 60.0]);
    parameters[0].missing_values = vec![-32766.0, -9.5];
    parameters[1].missing_values = vec![999.0];
    parameters
}

/// `2026-01-01T<hh:mm>:00Z`.
fn on_jan_1(hh_mm: &str) -> DateTime<Utc> {
    format!("2026-01-01T{hh_mm}:00Z").parse().unwrap()
}

/// The value-filter fixture's station at longitude `x`.
fn filter_station(x: f64) -> &'static str {
    match (x * 100.0).round() as i64 {
        105 => "ok1",
        305 => "ok2",
        505 => "bad",
        _ => panic!("({x}, …) is no value-filter fixture station"),
    }
}

/// A station's series over 05:00–06:00 with the filters: its times, then
/// the `rh` and `t2m` values. Every row keeps its time; a rejected value is
/// null there, as a NULL is.
fn expected_filtered(station: &str) -> (Vec<DateTime<Utc>>, Vec<Vec<Option<f64>>>) {
    match station {
        "ok1" => (
            vec![on_jan_1("05:20"), on_jan_1("05:30"), on_jan_1("05:50")],
            vec![
                vec![Some(40.0), Some(50.0), Some(60.0)],
                vec![Some(60.0), Some(1.0), None],
            ],
        ),
        "ok2" => (
            vec![on_jan_1("05:30"), on_jan_1("05:50")],
            vec![vec![None, Some(-32766.0)], vec![Some(2.0), None]],
        ),
        "bad" => (
            vec![on_jan_1("05:40"), on_jan_1("05:45")],
            vec![vec![Some(70.0), Some(70.0)], vec![None, None]],
        ),
        other => panic!("no value-filter fixture station {other}"),
    }
}

/// A coverage's times and its ranges' values, sorted by name.
fn series(c: &Coverage) -> (Vec<DateTime<Utc>>, Vec<Vec<Option<f64>>>) {
    (
        c.t.clone(),
        c.ranges
            .iter()
            .map(|(_, _, values)| values.clone())
            .collect(),
    )
}

/// The location query for `station`, on a blocking thread as the EDR
/// executor runs it.
async fn location_series(
    engine: &Arc<PostgisEngine>,
    station: &str,
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> Coverage {
    let engine = Arc::clone(engine);
    let id = station.to_string();
    let response = tokio::task::spawn_blocking(move || {
        engine
            .query_location(&id, window, None, None, None)
            .unwrap_or_else(|e| panic!("location {id}: {e:?}"))
    })
    .await
    .expect("location task");
    let CoverageResponse::Single(coverage) = response else {
        panic!("a location answers one coverage");
    };
    comparable(&[coverage]).remove(0)
}

/// What #1210 asks of a station shape, on the value-filter fixture:
///
/// 1. EDR: a value outside `valid_range` or equal to a sentinel is null in
///    the series, which keeps its time, on the location query and on both
///    area paths; without filters the same rows are served as stored;
/// 2. the station map layer draws each station's latest VALID value and no
///    marker for a station without one;
/// 3. a station grid over the collection, as the server wires one, never
///    sees a filtered value: two stations behind the analysis, every cell
///    between their valid values.
async fn check_value_filters(schema: &str, cfg: fn(&str) -> PostgisConfig) {
    let Some(dsn) = dsn() else {
        return;
    };
    let client = connect(&dsn).await;
    {
        let _setup = SETUP.lock().await;
        exec(&client, schema, "CREATE EXTENSION IF NOT EXISTS postgis;").await;
        exec(&client, schema, VALUE_FILTERS).await;
    }
    let mut filtered = cfg(schema);
    filtered.parameters = filtered_parameters();
    let pg = engine(schema, &filtered).await;
    let window = Some((on_jan_1("05:00"), on_jan_1("06:00")));

    // 1. EDR series.
    for station in ["ok1", "ok2", "bad"] {
        let coverage = location_series(&pg, station, window).await;
        assert_eq!(
            series(&coverage),
            expected_filtered(station),
            "{schema}: location {station}"
        );
    }
    for path in [AreaPath::SetBased, AreaPath::FanOut] {
        let coverages = area(
            &pg,
            Some(path),
            "POLYGON((0 0,10 0,10 10,0 10,0 0))",
            window,
            Some(&["t2m", "rh"][..]),
        )
        .await
        .unwrap_or_else(|e| panic!("{schema} {path:?}: {e:?}"));
        let coverages = comparable(&coverages);
        assert_eq!(coverages.len(), 3, "{schema} {path:?}");
        for c in &coverages {
            let station = filter_station(c.x);
            assert_eq!(
                series(c),
                expected_filtered(station),
                "{schema} {path:?}: {station}"
            );
        }
    }
    let raw = engine(schema, &cfg(schema)).await;
    let ok1 = location_series(&raw, "ok1", window).await;
    assert_eq!(
        series(&ok1).1[1],
        vec![Some(60.0), Some(1.0), Some(-9.5)],
        "{schema}: without filters a sentinel is served as stored"
    );
    let ok2 = location_series(&raw, "ok2", window).await;
    assert_eq!(
        series(&ok2).1,
        vec![
            vec![Some(999.0), Some(-32766.0)],
            vec![Some(2.0), Some(75.0)]
        ],
        "{schema}: without filters"
    );

    // 2. The station map frame at 06:00 (max_age PT1H).
    pg.set_station_symbols(engine_postgis::StationSymbols::new(
        std::collections::HashMap::from([("t2m".to_string(), 0.0)]),
    ));
    pg.refresh_metadata().await.expect("refresh");
    let tile = station_frame(&pg, "2026-01-01T06:00:00Z").await;
    assert_eq!(at_station(&tile, "ok1"), Some(1.0), "{schema}: past -9.5");
    assert_eq!(at_station(&tile, "ok2"), Some(2.0), "{schema}: past 75");
    assert_eq!(
        at_station(&tile, "bad"),
        None,
        "{schema}: no valid value, no marker"
    );

    // 3. A station grid over the collection, analysis at 05:50.
    let source: Arc<dyn EdrEngine> = pg.clone();
    let grid_config = ds_core::config::StationGridConfig {
        source: schema.into(),
        parameters: vec!["t2m".into()],
        elevation_source: None,
        elevation_parameter: None,
        land_mask_source: None,
        land_mask_parameter: None,
        exclude_where: Default::default(),
        grid: ds_core::config::StationGridGeometry {
            crs: "EPSG:4326".into(),
            bbox: [0.0, 0.0, 6.0, 6.0],
            resolution: 0.5,
        },
        method: "idw".into(),
        power: None,
        correlation_length_km: None,
        max_neighbours: 8,
        search_radius_km: 1000.0,
        max_age: "PT1H".into(),
        cadence: Some("PT10M".into()),
        poll_interval_secs: 60,
        precompute: 1,
    };
    let grid_id = format!("{schema}_grid");
    let analysis = on_jan_1("05:50");
    let (stations, values) = tokio::task::spawn_blocking(move || {
        let grid = engine_station_grid::StationGridEngine::new(
            &grid_id,
            &grid_config.source,
            source,
            &grid_config,
        )
        .expect("station grid over the postgis collection");
        let response = grid
            .query_area("0,0,6,6", Some((analysis, analysis)), None, None, None)
            .expect("grid area");
        let stations = grid.station_counts();
        grid.shutdown();
        let result = match response {
            CoverageResponse::Single(result) => result,
            CoverageResponse::Collection(mut results) => results.remove(0),
        };
        (stations, result.ranges["t2m"].values.clone())
    })
    .await
    .expect("grid task");
    assert_eq!(
        stations,
        vec![("t2m".to_string(), Some(2))],
        "{schema}: `bad` has no valid value and stays out of the analysis"
    );
    let cells: Vec<f64> = values.into_iter().flatten().collect();
    assert!(!cells.is_empty(), "{schema}: the grid has values");
    for v in &cells {
        assert!(
            (1.0 - 1e-4..=2.0 + 1e-4).contains(v),
            "{schema}: a cell of {v} is outside the valid values 1 and 2"
        );
    }
    exec(&client, schema, "DROP SCHEMA __S__ CASCADE;").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn value_filters_long_shape() {
    check_value_filters("mc1210_long", long_config).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn value_filters_wide_shape() {
    check_value_filters("mc1210_wide", wide_config).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn value_filters_per_parameter_shape() {
    check_value_filters("mc1210_per_parameter", per_parameter_config).await;
}

/// An events table whose `real` peak current stores 0 for "not measured".
const VALUE_FILTER_EVENTS: &str = "
DROP SCHEMA IF EXISTS __S__ CASCADE;
CREATE SCHEMA __S__;
CREATE TABLE __S__.strikes (
    id bigint PRIMARY KEY,
    time timestamptz NOT NULL,
    geom geometry(Point, 4326) NOT NULL,
    peak_current real
);
INSERT INTO __S__.strikes VALUES
    (1, '2026-01-01 05:10:00+00', ST_SetSRID(ST_MakePoint(1, 1), 4326), 12.5),
    (2, '2026-01-01 05:20:00+00', ST_SetSRID(ST_MakePoint(2, 2), 4326), 0),
    (3, '2026-01-01 05:30:00+00', ST_SetSRID(ST_MakePoint(3, 3), 4326), 900),
    (4, '2026-01-01 05:40:00+00', ST_SetSRID(ST_MakePoint(4, 4), 4326), NULL);
CREATE INDEX ON __S__.strikes (time);
ANALYZE;
";

/// #1210 on the events shape: every event stays in the answer; a peak
/// current outside `[-300, 300]` or equal to the sentinel 0 is null, as a
/// NULL is.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn value_filters_events_shape() {
    let Some(dsn) = dsn() else {
        return;
    };
    let schema = "mc1210_events";
    let client = connect(&dsn).await;
    {
        let _setup = SETUP.lock().await;
        exec(&client, schema, "CREATE EXTENSION IF NOT EXISTS postgis;").await;
        exec(&client, schema, VALUE_FILTER_EVENTS).await;
    }
    let mut cfg = config(
        None,
        PostgisObservationsConfig {
            table: Some(format!("{schema}.strikes")),
            time_col: Some("time".into()),
            geom_col: Some("geom".into()),
            id_col: Some("id".into()),
            extent_bbox: Some([0.0, 0.0, 10.0, 10.0]),
            ..observations("events")
        },
    );
    cfg.parameters = vec![PostgisParameterConfig {
        name: "peak_current".into(),
        label: "Peak current".into(),
        unit: "kA".into(),
        observed_property: None,
        source_key: None,
        valid_range: Some(vec![-300.0, 300.0]),
        missing_values: vec![0.0],
    }];
    let engine = engine(schema, &cfg).await;
    let events = area(
        &engine,
        None,
        "POLYGON((0 0,10 0,10 10,0 10,0 0))",
        Some((on_jan_1("05:00"), on_jan_1("06:00"))),
        None,
    )
    .await
    .expect("events area");
    let values: Vec<(Option<DateTime<Utc>>, Option<f64>)> = events
        .iter()
        .map(|c| {
            let DomainDescription::Point { t, .. } = &c.domain else {
                panic!("an event is a Point, got {:?}", c.domain);
            };
            (*t, c.ranges["peak_current"].values[0])
        })
        .collect();
    assert_eq!(
        values,
        vec![
            (Some(on_jan_1("05:40")), None),
            (Some(on_jan_1("05:30")), None),
            (Some(on_jan_1("05:20")), None),
            (Some(on_jan_1("05:10")), Some(12.5)),
        ],
        "newest first; out of range, sentinel and NULL are all null"
    );
    exec(&client, schema, "DROP SCHEMA __S__ CASCADE;").await;
}

const PICK_EVENTS: &str = "
DROP SCHEMA IF EXISTS __S__ CASCADE;
CREATE SCHEMA __S__;
CREATE TABLE __S__.strikes (
    id bigint PRIMARY KEY,
    time timestamptz NOT NULL,
    geom geometry(Point, 4326) NOT NULL,
    peak_current real,
    cloud smallint
);
INSERT INTO __S__.strikes VALUES
    (1, '2026-01-01 05:30:00+00', ST_SetSRID(ST_MakePoint(1.002, 1.008), 4326), -12.5, 0),
    (2, '2026-01-01 05:40:00+00', ST_SetSRID(ST_MakePoint(1.005, 1.005), 4326), 900, 1),
    (3, '2026-01-01 05:00:00+00', ST_SetSRID(ST_MakePoint(1.003, 1.004), 4326), 5, 0),
    (4, '2026-01-01 05:50:00+00', ST_SetSRID(ST_MakePoint(1.055, 1.055), 4326), 6, 0),
    (5, '2026-01-01 06:01:00+00', ST_SetSRID(ST_MakePoint(1.005, 1.005), 4326), 7, 0),
    (6, '2026-01-01 05:45:00+00', ST_SetSRID(ST_MakePoint(1.007, 1.002), 4326), NULL, NULL),
    (7, '2026-01-01 05:55:00+00', ST_SetSRID(ST_MakePoint(-179.998, 1.005), 4326), 8, 1);
CREATE INDEX ON __S__.strikes (time);
ANALYZE;
";

/// #1100: WMS GetFeatureInfo on the lightning layer picks the strikes whose
/// disc a frame draws over the pixel: the frame's half-open window
/// `(TIME - PT1H, TIME]` (05:00 out, 06:01 out), on 0.01° pixels a disc of
/// radius 2 around each strike's own pixel (a click on its edge pixel finds
/// it, one just past does not), newest first, `limit` kept and every strike
/// counted, each with its time, id, filtered parameter and `type`; a pixel
/// beside the antimeridian finds the strike stored at -179.998 through the
/// region's copy.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn events_pick_shape() {
    use ds_core::feature::PropertyValue;
    use ds_core::feature_pick::{FeaturePick, PickMap};
    use ds_core::map_engine::{MapEngine, OutputCrs};
    let Some(dsn) = dsn() else {
        return;
    };
    let schema = "mc1100_events";
    let client = connect(&dsn).await;
    {
        let _setup = SETUP.lock().await;
        exec(&client, schema, "CREATE EXTENSION IF NOT EXISTS postgis;").await;
        exec(&client, schema, PICK_EVENTS).await;
    }
    let mut cfg = config(
        None,
        PostgisObservationsConfig {
            table: Some(format!("{schema}.strikes")),
            time_col: Some("time".into()),
            geom_col: Some("geom".into()),
            id_col: Some("id".into()),
            cloud_indicator_col: Some("cloud".into()),
            peak_current_col: Some("peak_current".into()),
            extent_bbox: Some([-180.0, 0.0, 180.0, 10.0]),
            ..observations("events")
        },
    );
    cfg.parameters = vec![PostgisParameterConfig {
        name: "peak_current".into(),
        label: "Peak current".into(),
        unit: "kA".into(),
        observed_property: None,
        source_key: None,
        valid_range: Some(vec![-300.0, 300.0]),
        missing_values: vec![],
    }];
    let engine = engine(schema, &cfg).await;
    assert!(engine.layer_picks_features("lightning_age"));

    // Pixel (i, j) of a 10 × 10 CRS:84 map of 0.01° pixels from `west`°E,
    // 1.0°N, centred as api-wms centres it; TIME floors to 06:00.
    let output_crs = OutputCrs::Wgs84;
    let pick_on = |west: f64, (i, j): (u32, u32), limit: usize| {
        let bbox = [west, 1.0, west + 0.1, 1.1];
        let (lon, lat) = output_crs.project_node(
            bbox,
            (f64::from(i) + 0.5) / 10.0,
            (f64::from(j) + 0.5) / 10.0,
        );
        FeaturePick {
            parameter: Some("lightning_age"),
            map: PickMap {
                output_crs: &output_crs,
                wgs84_bbox: bbox,
                width: 10,
                height: 10,
            },
            pixel: (i, j),
            point: (ds_core::geo::wrap_lon(lon), lat),
            time: Some("2026-01-01T06:00:30Z".parse().unwrap()),
            reference_time: None,
            z: None,
            limit,
        }
    };
    let picked_ids = |picked: &ds_core::feature_pick::PickedFeatures| -> Vec<String> {
        picked.features.iter().map(|f| f.id.clone()).collect()
    };
    // Strikes 1, 2 and 6 lie in pixel (0, 9), 1.00..1.01°E, 1.00..1.01°N.
    let pixel = |west: f64, limit: usize| pick_on(west, (0, 9), limit);
    let picked = engine.pick_features(&pixel(1.0, 2)).expect("pick");
    assert_eq!(picked.number_matched, 3, "strikes 2, 6 and 1");
    let ids: Vec<&str> = picked.features.iter().map(|f| f.id.as_str()).collect();
    assert_eq!(ids, ["6", "2"], "newest first, the limit kept");
    let all = engine.pick_features(&pixel(1.0, 10)).expect("pick");
    let strike = all.features.iter().find(|f| f.id == "1").expect("strike 1");
    assert_eq!(
        strike.properties,
        vec![
            (
                "time".to_string(),
                PropertyValue::String("2026-01-01T05:30:00Z".into())
            ),
            ("peak_current".to_string(), PropertyValue::Float(-12.5)),
            (
                "type".to_string(),
                PropertyValue::String("cloud-to-ground".into())
            ),
        ],
        "a parameter reads peak_current, so no peak_current_ka"
    );
    let two = all.features.iter().find(|f| f.id == "2").unwrap();
    assert_eq!(
        two.properties[1].1,
        PropertyValue::Null,
        "900 kA is out of range"
    );
    assert_eq!(
        two.properties[2].1,
        PropertyValue::String("intra-cloud".into())
    );
    match &*strike.geometry {
        ds_core::feature::Geometry::Point { x, y } => {
            assert!((x - 1.002).abs() < 1e-9 && (y - 1.008).abs() < 1e-9)
        }
        other => panic!("{other:?}"),
    }

    // Before the first strike's window: nothing, an empty answer.
    let mut early = pixel(1.0, 10);
    early.time = Some("2026-01-01T04:00:00Z".parse().unwrap());
    let none = engine.pick_features(&early).expect("pick");
    assert_eq!(none.number_matched, 0);
    assert!(none.features.is_empty());

    // Strike 4 lies in pixel (5, 4). Its disc reaches two pixels across,
    // one diagonally: (7, 4) and (5, 2) are its edge pixels, (7, 5) and
    // (8, 4) just past it.
    for (pixel, found) in [
        ((5, 4), true),
        ((7, 4), true),
        ((5, 2), true),
        ((6, 5), true),
        ((7, 5), false),
        ((8, 4), false),
    ] {
        let picked = engine
            .pick_features(&pick_on(1.0, pixel, 10))
            .expect("pick");
        let expected: Vec<String> = if found { vec!["4".into()] } else { Vec::new() };
        assert_eq!(picked_ids(&picked), expected, "{pixel:?}");
    }
    // The strike stored at -179.998 lies in 180.00..180.01°E, pixel (5, 9)
    // of the map from 179.95°E. A click on pixel (4, 9), 179.99..180.00°E,
    // centred east of 180°, finds it through the region's copy.
    for (pixel, expected) in [((5, 9), vec!["7"]), ((4, 9), vec!["7"]), ((2, 9), vec![])] {
        let picked = engine
            .pick_features(&pick_on(179.95, pixel, 10))
            .expect("pick");
        assert_eq!(picked_ids(&picked), expected, "{pixel:?}");
    }
    exec(&client, schema, "DROP SCHEMA __S__ CASCADE;").await;
}

#[path = "support/schema_validation.rs"]
mod schema_validation;

#[path = "support/privileges.rs"]
mod privileges;

#[path = "support/indexes.rs"]
mod indexes;

#[path = "support/temporal_extent.rs"]
mod temporal_extent;
