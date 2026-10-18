//! The advertised temporal extent ends at now (#1139).
use super::privileges::Logs;
use super::*;
use ds_core::map_engine::MapEngine;
use engine_postgis::query::build_temporal_extent;

/// The warning, without the start the cleared message shares with it.
const FUTURE_WARNING: &str = "dated in the future: the advertised temporal extent ends at \
                              the latest observation not after now";
const FUTURE_CLEARED: &str = "no observations are dated in the future any more";

/// One station's rows, each in a `timestamptz` column and, as Helsinki local
/// time, in a naive one, both indexed: 10 000 hourly rows ending at `t0()`
/// (`value` 0), then rows an hour before (1) and an hour after (2) the
/// database's clock, and 13 days after it (3).
async fn fixture(schema: &str) -> Option<tokio_postgres::Client> {
    let client = connect(&dsn()?).await;
    let _setup = SETUP.lock().await;
    client
        .batch_execute("CREATE EXTENSION IF NOT EXISTS postgis")
        .await
        .unwrap();
    exec(
        &client,
        schema,
        "
        DROP SCHEMA IF EXISTS __S__ CASCADE;
        CREATE SCHEMA __S__;
        CREATE TABLE __S__.stations (
            id text PRIMARY KEY, name text NOT NULL, geom geometry(Point, 4326) NOT NULL
        );
        INSERT INTO __S__.stations VALUES ('a', 'A', ST_SetSRID(ST_Point(1, 45), 4326));
        CREATE TABLE __S__.obs (
            station_id text NOT NULL, obstime timestamptz NOT NULL,
            local_time timestamp NOT NULL, param text NOT NULL, value float8,
            geom geometry(Point, 4326) NOT NULL DEFAULT ST_SetSRID(ST_Point(1, 45), 4326)
        );
        INSERT INTO __S__.obs (station_id, obstime, local_time, param, value)
        SELECT 'a', t, t AT TIME ZONE 'Europe/Helsinki', 't2m', marker
        FROM (SELECT TIMESTAMPTZ '2026-01-01 06:00:00+00' - h * INTERVAL '1 hour', 0
              FROM generate_series(0, 9999) AS h
              UNION ALL
              VALUES (now() - INTERVAL '1 hour', 1),
                     (now() + INTERVAL '1 hour', 2),
                     (now() + INTERVAL '13 days', 3)) AS r(t, marker);
        CREATE INDEX ON __S__.obs (obstime);
        CREATE INDEX ON __S__.obs (local_time);
        ANALYZE __S__.obs;
    ",
    )
    .await;
    Some(client)
}

/// The `obstime` the fixture stored for the row whose `value` is `marker`.
async fn stored(client: &tokio_postgres::Client, schema: &str, marker: f64) -> DateTime<Utc> {
    let sql = "SELECT obstime FROM __S__.obs WHERE value = $1"
        .replace("__S__", &quote_ident(schema).expect("schema identifier"));
    client.query_one(&sql, &[&marker]).await.unwrap().get(0)
}

/// Whether Helsinki keeps one UTC offset from two hours before the
/// database's clock to two hours after it. In the autumn change's repeated
/// hour a naive local time is ambiguous, the documented limitation, so the
/// naive column's checks against rows an hour either side of now hold only
/// away from a change.
async fn helsinki_offset_steady(client: &tokio_postgres::Client) -> bool {
    client
        .query_one(
            "SELECT ((now() - INTERVAL '2 hours') AT TIME ZONE 'Europe/Helsinki') \
             + INTERVAL '4 hours' = ((now() + INTERVAL '2 hours') AT TIME ZONE 'Europe/Helsinki')",
            &[],
        )
        .await
        .unwrap()
        .get(0)
}

/// Both extent reads, unbounded and bounded at now, plan as `MIN`/`MAX`
/// index probes (a `Limit` over an index scan), never a scan of the table:
/// the aggregates and the bound take the bare column, a naive one too.
async fn assert_index_probes(client: &tokio_postgres::Client, cfg: &PostgisConfig, column: &str) {
    let cfg = PostgisEngineConfig::resolve(cfg).expect("resolve the test config");
    for not_after in [None, Some(Utc::now())] {
        let q = build_temporal_extent(&cfg, not_after)
            .expect("build the extent statement")
            .expect("a table to read");
        let explain = ["EXPLAIN ", q.sql.as_str()].concat();
        let rows = client
            .query(explain.as_str(), &params_as_refs(&q.params))
            .await
            .unwrap();
        let plan: Vec<String> = rows.iter().map(|row| row.get(0)).collect();
        let plan = plan.join("\n");
        assert!(
            plan.contains("Limit") && plan.contains("Index") && !plan.contains("Seq Scan"),
            "{column}, bound {not_after:?}:\n{plan}"
        );
    }
}

/// The engine as the server builds it, before its first refresh, so the
/// test captures that refresh's log.
fn unrefreshed(id: &str, cfg: &PostgisConfig) -> Arc<PostgisEngine> {
    let config = PostgisEngineConfig::resolve(cfg).expect("resolve the test config");
    let pool = PoolRegistry::new()
        .get_or_create(&config.dsn, NonZeroU32::new(2).unwrap())
        .unwrap();
    Arc::new(PostgisEngine::new(id, Arc::new(config), pool))
}

/// Rows an hour and 13 days ahead no longer stretch the extent of the
/// collection, and so of every location, into the future, where every query
/// is a 404: it ends at the row an hour before now, for a `timestamptz` and
/// a naive Helsinki column alike (a bound in the wrong zone misses that row
/// by hours), and for an events table. The refresh that first finds such
/// rows warns once, one that no longer finds them says so once, a row within
/// the grace is left out without a warning, and a table holding only future
/// rows advertises no extent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn temporal_extent_ends_at_now_despite_future_rows() {
    let schema = "mc1139_extent";
    let Some(client) = fixture(schema).await else {
        return;
    };
    // The row an hour before the database's clock, as stored.
    let recent = stored(&client, schema, 1.0).await;
    let past = Some((t0() - hours(9999), recent));
    let steady = helsinki_offset_steady(&client).await;
    let mut cfg = long_config(schema);
    for (column, zone) in [("obstime", None), ("local_time", Some("Europe/Helsinki"))] {
        cfg.observations.time_col = Some(column.into());
        cfg.observations.time_col_tz = zone.map(str::to_string);
        assert_index_probes(&client, &cfg, column).await;
        if zone.is_some() && !steady {
            println!("skipped the {column} extent: within two hours of a Helsinki clock change");
            continue;
        }
        let engine = unrefreshed(column, &cfg);
        let logs = Logs::default();
        for _ in 0..2 {
            logs.capture(engine.refresh_metadata()).await.unwrap();
            assert_eq!(engine.get_temporal_extent(), past, "{column}");
        }
        assert_eq!(
            logs.count(FUTURE_WARNING),
            1,
            "{column} warned once: {}",
            logs.text()
        );
    }

    // The events shape reads the same extent, and its newest TIME step is
    // that row's minute.
    let mut events = config(
        None,
        PostgisObservationsConfig {
            table: Some(format!("{schema}.obs")),
            time_col: Some("obstime".into()),
            geom_col: Some("geom".into()),
            id_col: Some("station_id".into()),
            ..observations("events")
        },
    );
    events.parameters.clear();
    let engine = unrefreshed("events", &events);
    engine.refresh_metadata().await.unwrap();
    assert_eq!(engine.get_temporal_extent(), past, "events");
    let last = recent.timestamp();
    let last_minute = DateTime::from_timestamp(last - last.rem_euclid(60), 0).unwrap();
    assert_eq!(engine.raster_info().times.last(), Some(&last_minute));

    cfg.observations.time_col = Some("obstime".into());
    cfg.observations.time_col_tz = None;
    let engine = unrefreshed("cleared", &cfg);
    let logs = Logs::default();
    logs.capture(engine.refresh_metadata()).await.unwrap();
    exec(
        &client,
        schema,
        "DELETE FROM __S__.obs WHERE obstime > now()",
    )
    .await;
    for _ in 0..2 {
        logs.capture(engine.refresh_metadata()).await.unwrap();
        assert_eq!(engine.get_temporal_extent(), past);
    }
    assert_eq!(logs.count(FUTURE_CLEARED), 1, "{}", logs.text());

    // Within the grace: bounded out of the extent, but not reported.
    exec(
        &client,
        schema,
        "INSERT INTO __S__.obs (station_id, obstime, local_time, param, value)
         SELECT 'a', t, t AT TIME ZONE 'Europe/Helsinki', 't2m', 4
         FROM (VALUES (now() + INTERVAL '5 minutes')) AS r(t);",
    )
    .await;
    logs.capture(engine.refresh_metadata()).await.unwrap();
    assert_eq!(engine.get_temporal_extent(), past, "a row within the grace");
    assert_eq!(logs.count(FUTURE_WARNING), 1, "{}", logs.text());
    assert_eq!(logs.count(FUTURE_CLEARED), 1, "{}", logs.text());

    exec(
        &client,
        schema,
        "DELETE FROM __S__.obs;
         INSERT INTO __S__.obs (station_id, obstime, local_time, param, value)
         SELECT 'a', t, t AT TIME ZONE 'Europe/Helsinki', 't2m', 3
         FROM (VALUES (now() + INTERVAL '13 days')) AS r(t);",
    )
    .await;
    logs.capture(engine.refresh_metadata()).await.unwrap();
    assert_eq!(engine.get_temporal_extent(), None, "only future rows");
    assert_eq!(logs.count(FUTURE_WARNING), 2, "{}", logs.text());
    exec(&client, schema, "DROP SCHEMA __S__ CASCADE").await;
}
