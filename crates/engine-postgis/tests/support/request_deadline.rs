//! #1500: a statement still running at the request deadline is
//! `DeadlineExceeded` at the deadline, the database cancels it, and its
//! connection is discarded rather than handed to the next request.
//!
//! Each fixture's view calls a function that sleeps 20 s for a marked row
//! (value or peak current 999) and returns at once for any other. A query
//! whose window holds a marked row runs past a 1.5 s deadline; the backends
//! running it are read from `pg_stat_activity` while it runs, and must be
//! gone within seconds of the answer. A canceled statement that went back
//! to the pool would leave its backend connected, idle; one that was not
//! canceled would leave it active until the sleep ends.
use super::*;

/// The request deadline the engine calls run under.
const DEADLINE: Duration = Duration::from_millis(1500);
/// How long the tests wait for the statements to start, and for their
/// backends to end after the deadline.
const WAIT: Duration = Duration::from_secs(5);

/// `call` on a blocking thread under a request deadline [`DEADLINE`] away,
/// as the query runtime runs an engine call.
fn under_deadline<T: Send + 'static>(
    call: impl FnOnce() -> T + Send + 'static,
) -> tokio::task::JoinHandle<T> {
    tokio::task::spawn_blocking(move || {
        let _deadline = ds_core::deadline::enter(Some(Instant::now() + DEADLINE));
        call()
    })
}

/// The backends, other than `client`'s, running a statement that reads
/// `view` of `schema`, as the engine names it.
async fn running(client: &tokio_postgres::Client, schema: &str, view: &str) -> Vec<i32> {
    let name = format!(
        "{}.{}",
        quote_ident(schema).expect("schema identifier"),
        quote_ident(view).expect("view identifier")
    );
    client
        .query(
            "SELECT pid FROM pg_catalog.pg_stat_activity \
             WHERE state = 'active' AND pid <> pg_backend_pid() AND position($1 in query) > 0",
            &[&name],
        )
        .await
        .expect("pg_stat_activity")
        .iter()
        .map(|row| row.get(0))
        .collect()
}

/// The backends of the statements on `schema`'s `view` once `n` of them
/// run, or those seen when `call` ends first (its answer then says why).
async fn wait_running<T>(
    client: &tokio_postgres::Client,
    (schema, view): (&str, &str),
    n: usize,
    call: &tokio::task::JoinHandle<T>,
) -> Vec<i32> {
    let until = Instant::now() + WAIT;
    loop {
        let pids = running(client, schema, view).await;
        if pids.len() >= n || call.is_finished() || Instant::now() >= until {
            return pids;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Wait until none of `pids` is connected any more.
async fn wait_gone(client: &tokio_postgres::Client, pids: &[i32]) {
    let until = Instant::now() + WAIT;
    loop {
        let left: Vec<i32> = client
            .query(
                "SELECT pid FROM pg_catalog.pg_stat_activity WHERE pid = ANY($1)",
                &[&pids],
            )
            .await
            .expect("pg_stat_activity")
            .iter()
            .map(|row| row.get(0))
            .collect();
        if left.is_empty() {
            return;
        }
        assert!(
            Instant::now() < until,
            "backends {left:?} outlived the deadline: a canceled, discarded connection ends"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn assert_deadline<T: std::fmt::Debug>(result: Result<T, DataServerError>, started: Instant) {
    let elapsed = started.elapsed();
    assert!(
        matches!(result, Err(DataServerError::DeadlineExceeded)),
        "{result:?}"
    );
    assert!(
        elapsed < DEADLINE + Duration::from_secs(2),
        "answered after {elapsed:?}, not at the deadline"
    );
}

const EVENTS: &str = "
DROP SCHEMA IF EXISTS __S__ CASCADE;
CREATE SCHEMA __S__;
CREATE TABLE __S__.base (
    id bigint NOT NULL,
    time timestamp without time zone NOT NULL,
    geom geometry(Point, 4326) NOT NULL,
    peak_current real
);
CREATE FUNCTION __S__.slow(g geometry, ka real) RETURNS geometry
    LANGUAGE plpgsql VOLATILE AS $$
    BEGIN IF ka = 999 THEN PERFORM pg_sleep(20); END IF; RETURN g; END $$;
CREATE VIEW __S__.strikes AS
    SELECT id, time, __S__.slow(geom, peak_current) AS geom, peak_current FROM __S__.base;
";

fn events_config(schema: &str) -> PostgisConfig {
    let mut cfg = config(
        None,
        PostgisObservationsConfig {
            table: Some(format!("{schema}.strikes")),
            time_col: Some("time".into()),
            time_col_tz: Some("UTC".into()),
            geom_col: Some("geom".into()),
            id_col: Some("id".into()),
            extent_bbox: Some([0.0, 0.0, 2.0, 2.0]),
            ..observations("events")
        },
    );
    cfg.parameters = vec![PostgisParameterConfig {
        name: "peak_current".into(),
        label: "Peak current".into(),
        unit: "kA".into(),
        observed_property: None,
        source_key: None,
        valid_range: None,
        missing_values: vec![],
    }];
    cfg
}

/// The events area statement: one connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_request_deadline_cancels_an_events_statement() {
    let Some(dsn) = dsn() else {
        return;
    };
    let schema = "mc1500_events";
    let client = connect(&dsn).await;
    {
        let _setup = SETUP.lock().await;
        exec(&client, schema, "CREATE EXTENSION IF NOT EXISTS postgis;").await;
        exec(&client, schema, EVENTS).await;
    }
    // Empty while the refresh validates the view.
    let engine = engine("mc1500-events", &events_config(schema)).await;
    exec(
        &client,
        schema,
        "INSERT INTO __S__.base VALUES
         (1, '2026-01-10 10:30:00', ST_SetSRID(ST_MakePoint(1, 1), 4326), 999),
         (2, '2026-01-10 08:30:00', ST_SetSRID(ST_MakePoint(1, 1), 4326), 5);",
    )
    .await;
    let at = |s: &str| -> DateTime<Utc> { s.parse().unwrap() };
    let slow = (at("2026-01-10T10:00:00Z"), at("2026-01-10T11:00:00Z"));
    let fast = (at("2026-01-10T08:00:00Z"), at("2026-01-10T09:00:00Z"));
    const POLYGON: &str = "POLYGON((0 0,2 0,2 2,0 2,0 0))";

    let started = Instant::now();
    let call = under_deadline({
        let engine = Arc::clone(&engine);
        move || engine.query_area_on(None, POLYGON, Some(slow), None)
    });
    let pids = wait_running(&client, (schema, "strikes"), 1, &call).await;
    assert_deadline(call.await.expect("area task"), started);
    assert_eq!(pids.len(), 1, "the statement ran before the deadline");
    wait_gone(&client, &pids).await;

    // The pool serves the next request, under a deadline too.
    let answer = under_deadline(move || engine.query_area_on(None, POLYGON, Some(fast), None))
        .await
        .expect("area task")
        .expect("the next statement runs");
    match answer {
        CoverageResponse::Collection(coverages) => assert_eq!(coverages.len(), 1),
        CoverageResponse::Single(_) => panic!("an events area answers a collection"),
    }
    exec(&client, schema, "DROP SCHEMA __S__ CASCADE;").await;
}

const STATIONS_ABC: &str = "
DROP SCHEMA IF EXISTS __S__ CASCADE;
CREATE SCHEMA __S__;
CREATE TABLE __S__.stations (
    id text PRIMARY KEY,
    name text NOT NULL,
    geom geometry(Point, 4326) NOT NULL
);
INSERT INTO __S__.stations VALUES
    ('a', 'A', ST_SetSRID(ST_MakePoint(1.0, 1), 4326)),
    ('b', 'B', ST_SetSRID(ST_MakePoint(1.1, 1), 4326)),
    ('c', 'C', ST_SetSRID(ST_MakePoint(1.2, 1), 4326));
CREATE TABLE __S__.base (
    station_id text NOT NULL,
    obstime timestamptz NOT NULL,
    param text NOT NULL,
    value double precision
);
CREATE FUNCTION __S__.slow(v double precision) RETURNS double precision
    LANGUAGE plpgsql VOLATILE AS $$
    BEGIN IF v = 999 THEN PERFORM pg_sleep(20); END IF; RETURN v; END $$;
CREATE VIEW __S__.obs AS
    SELECT station_id, obstime, param, __S__.slow(value) AS value FROM __S__.base;
";

/// The area fan-out: a connection per station, all running at the
/// deadline, all canceled and discarded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_request_deadline_cancels_every_fanout_statement() {
    let Some(dsn) = dsn() else {
        return;
    };
    let schema = "mc1500_fanout";
    let client = connect(&dsn).await;
    {
        let _setup = SETUP.lock().await;
        exec(&client, schema, "CREATE EXTENSION IF NOT EXISTS postgis;").await;
        exec(&client, schema, STATIONS_ABC).await;
    }
    let mut cfg = long_config(schema);
    // Its own fan-out limiter, whatever width another test gave the DSN's.
    cfg.pool_label = Some("mc1500-fanout".into());
    // Empty while the refresh reads the temporal extent through the view.
    let engine = engine("mc1500-fanout", &cfg).await;
    exec(
        &client,
        schema,
        "INSERT INTO __S__.base
         SELECT s, '2026-01-01T06:00:00Z'::timestamptz - h * interval '1 hour', 't2m',
                CASE WHEN h = 0 THEN 999 ELSE 1 END
         FROM unnest(ARRAY['a', 'b', 'c']) AS s, generate_series(0, 1) AS h;",
    )
    .await;
    const POLYGON: &str = "POLYGON((0 0,2 0,2 2,0 2,0 0))";
    let parameters = || Some(vec!["t2m".to_string()]);

    let started = Instant::now();
    let call = under_deadline({
        let engine = Arc::clone(&engine);
        move || {
            engine.query_area_on(
                Some(AreaPath::FanOut),
                POLYGON,
                Some((t0(), t0())),
                parameters().as_deref(),
            )
        }
    });
    let pids = wait_running(&client, (schema, "obs"), 3, &call).await;
    assert_deadline(call.await.expect("area task"), started);
    assert_eq!(
        pids.len(),
        3,
        "a statement per station ran before the deadline"
    );
    wait_gone(&client, &pids).await;

    let earlier = t0() - hours(1);
    let answer = under_deadline(move || {
        engine.query_area_on(
            Some(AreaPath::FanOut),
            POLYGON,
            Some((earlier, earlier)),
            parameters().as_deref(),
        )
    })
    .await
    .expect("area task")
    .expect("the next statements run");
    match answer {
        CoverageResponse::Collection(coverages) => assert_eq!(coverages.len(), 3),
        CoverageResponse::Single(_) => panic!("an area answers a collection"),
    }
    exec(&client, schema, "DROP SCHEMA __S__ CASCADE;").await;
}
