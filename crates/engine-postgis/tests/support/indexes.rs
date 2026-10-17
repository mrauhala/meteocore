//! Catalog diagnostics through the public engine lifecycle. Run serially with
//! the database suite; the permission-denial test restores its catalog ACL.
use super::privileges::{cleanup, fixture, role_engine, Logs};
use super::*;
use futures::FutureExt;
use tracing::instrument::WithSubscriber;

const COMPLETE: &str = "postgis: index audit completed";
const MISSING: &str = "postgis: missing query index";
const UNKNOWN: &str = "postgis: query index not assessed";
const UNCHECKED: &str = "postgis: index audit unchecked";

async fn audit(engine: &PostgisEngine) -> Logs {
    let logs = Logs::default();
    logs.capture(engine.refresh_metadata()).await.unwrap();
    assert_eq!(logs.count(COMPLETE), 1, "{}", logs.text());
    assert_eq!(logs.count(UNCHECKED), 0, "{}", logs.text());
    assert_eq!(engine.health_snapshot().refresh_failures, 0);
    logs
}

fn has(logs: &Logs, message: &str, relation: &str, path: &str) -> bool {
    logs.text().lines().any(|line| {
        line.contains(message)
            && line.contains(&format!("relation={relation} "))
            && line.contains(path)
    })
}

async fn index_defs(client: &tokio_postgres::Client, schema: &str) -> Vec<String> {
    client
        .query(
            "SELECT indexdef FROM pg_indexes WHERE schemaname = $1 ORDER BY indexname",
            &[&schema],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn btree_prefixes_directions_expressions_and_invalid_indexes() {
    let schema = "mc1149_btree";
    let roles = ["mc1149_btree_reader"];
    let Some(client) = fixture(schema, &roles).await else {
        return;
    };
    for (definition, expected) in [
        ("", MISSING),
        ("(station_id ASC, obstime ASC)", "found"),
        (
            "(station_id DESC, obstime DESC, param) INCLUDE (value)",
            "found",
        ),
        ("(station_id ASC, obstime DESC)", "found"),
        ("(param, station_id, obstime)", "found"),
        ("(station_id, param, obstime)", "found"),
        ("(extra, station_id, obstime)", MISSING),
        ("(station_id) INCLUDE (obstime)", MISSING),
        ("(obstime, station_id)", UNKNOWN),
        (
            "(station_id); CREATE INDEX secondary ON __S__.obs (obstime)",
            UNKNOWN,
        ),
        ("(station_id, obstime) WHERE value > 0", UNKNOWN),
        ("(lower(station_id), obstime)", UNKNOWN),
        ("(station_id COLLATE \"C\", obstime)", UNKNOWN),
        ("(station_id, obstime, (value + 1))", "found"),
    ] {
        exec(
            &client,
            schema,
            "DROP INDEX IF EXISTS __S__.candidate; DROP INDEX IF EXISTS __S__.secondary",
        )
        .await;
        if !definition.is_empty() {
            exec(
                &client,
                schema,
                &format!("CREATE INDEX candidate ON __S__.obs {definition}"),
            )
            .await;
        }
        let before = index_defs(&client, schema).await;
        let engine = role_engine(schema, Some(roles[0])).await;
        let logs = audit(&engine).await;
        if expected == "found" {
            assert!(
                !has(&logs, MISSING, "obs", "station/time"),
                "{definition}: {}",
                logs.text()
            );
            assert!(
                !has(&logs, UNKNOWN, "obs", "station/time"),
                "{definition}: {}",
                logs.text()
            );
        } else {
            assert!(
                has(&logs, expected, "obs", "station/time"),
                "{definition}: {}",
                logs.text()
            );
            let other = if expected == MISSING {
                UNKNOWN
            } else {
                MISSING
            };
            assert!(!has(&logs, other, "obs", "station/time"), "{}", logs.text());
        }
        assert_eq!(
            before,
            index_defs(&client, schema).await,
            "audit must not change indexes"
        );
        let session = engine.pool().get().await.unwrap();
        assert_eq!(
            session
                .query_one("SHOW transaction_read_only", &[])
                .await
                .unwrap()
                .get::<_, String>(0),
            "on"
        );
        assert_eq!(
            session
                .query_one("SHOW statement_timeout", &[])
                .await
                .unwrap()
                .get::<_, String>(0),
            "750ms"
        );
    }
    // A real failed concurrent unique-index build leaves an invalid pg_index
    // entry. It must not count as a usable station/time access path.
    exec(
        &client,
        schema,
        "DROP INDEX __S__.candidate; INSERT INTO __S__.obs SELECT * FROM __S__.obs",
    )
    .await;
    assert!(client.batch_execute("CREATE UNIQUE INDEX CONCURRENTLY invalid_candidate ON mc1149_btree.obs (station_id, obstime)")
        .await.is_err());
    let valid: bool = client.query_one("SELECT indisvalid FROM pg_index WHERE indexrelid = 'mc1149_btree.invalid_candidate'::regclass", &[]).await.unwrap().get(0);
    assert!(!valid);
    let logs = audit(&*role_engine(schema, Some(roles[0])).await).await;
    assert!(
        has(&logs, MISSING, "obs", "station/time"),
        "{}",
        logs.text()
    );
    cleanup(&client, schema, &roles).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spatial_paths_and_in_memory_selection() {
    let schema = "mc1149_spatial";
    let roles = ["mc1149_spatial_reader"];
    let Some(client) = fixture(schema, &roles).await else {
        return;
    };
    for (definition, expected) in [
        ("", MISSING),
        ("USING gist (geom)", "found"),
        ("USING spgist (geom)", "found"),
        ("USING gist (geom) WHERE id = 'a'", UNKNOWN),
        ("USING gist (ST_Transform(geom, 3857))", UNKNOWN),
        ("USING gist (geom gist_geometry_ops_nd)", UNKNOWN),
        ("USING brin (geom)", UNKNOWN),
    ] {
        exec(
            &client,
            schema,
            "DROP INDEX IF EXISTS __S__.candidate; DROP INDEX IF EXISTS __S__.secondary",
        )
        .await;
        if !definition.is_empty() {
            exec(
                &client,
                schema,
                &format!("CREATE INDEX candidate ON __S__.stations {definition}"),
            )
            .await;
        }
        let logs = audit(&*role_engine(schema, Some(roles[0])).await).await;
        if expected == "found" {
            assert!(
                !has(&logs, MISSING, "stations", "spatial geometry"),
                "{}",
                logs.text()
            );
            assert!(
                !has(&logs, UNKNOWN, "stations", "spatial geometry"),
                "{}",
                logs.text()
            );
            // A geometry index cannot cover ST_DWithin(geom::geography, ...).
            assert!(
                has(&logs, MISSING, "stations", "nearest-station geography"),
                "{}",
                logs.text()
            );
        } else {
            assert!(
                has(&logs, expected, "stations", "spatial geometry"),
                "{}",
                logs.text()
            );
        }
    }
    exec(
        &client,
        schema,
        "CREATE INDEX geography_idx ON __S__.stations USING gist ((geom::geography))",
    )
    .await;
    let logs = audit(&*role_engine(schema, Some(roles[0])).await).await;
    assert!(
        has(&logs, UNKNOWN, "stations", "nearest-station geography"),
        "{}",
        logs.text()
    );
    assert!(!has(
        &logs,
        MISSING,
        "stations",
        "nearest-station geography"
    ));

    exec(&client, schema, "ALTER TABLE __S__.obs ADD COLUMN geom geometry(Point,4326); UPDATE __S__.obs SET geom = 'SRID=4326;POINT(1 45)'").await;
    let initial = role_engine(schema, Some(roles[0])).await;
    for stations_present in [true, false] {
        let mut raw = long_config(schema);
        raw.observations.geom_col = Some("geom".into());
        raw.observations.locations_window = Some("all".into());
        if !stations_present {
            raw.stations = None;
        }
        let mut cfg = PostgisEngineConfig::resolve(&raw).unwrap();
        cfg.dsn = initial.config().dsn.clone();
        let engine = PostgisEngine::new(schema, Arc::new(cfg), initial.pool().clone());
        let logs = audit(&engine).await;
        assert!(
            !logs.text().contains("relation=stations"),
            "cached spatial selection needs no station spatial check: {}",
            logs.text()
        );
    }
    cleanup(&client, schema, &roles).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_parameter_overrides_and_event_time_keys() {
    let schema = "mc1149_mappings";
    let roles = ["mc1149_mapping_reader"];
    let Some(client) = fixture(schema, &roles).await else {
        return;
    };
    exec(
        &client,
        schema,
        "
        CREATE INDEX first_idx ON __S__.obs (station_id, obstime);
        CREATE TABLE __S__.second (reporter text, valid_time timestamp, value float8);
        INSERT INTO __S__.second VALUES ('a','2026-01-01 06:00:00',45);
        GRANT SELECT ON __S__.second TO mc1149_mapping_reader;
    ",
    )
    .await;
    let initial = role_engine(schema, Some(roles[0])).await;
    let mut raw = per_parameter_config(schema);
    raw.stations = Some(stations(schema));
    raw.observations.station_fk_col = Some("station_id".into());
    raw.observations.time_col = Some("obstime".into());
    raw.observations.geom_col = None;
    raw.observations.tables[0].table = format!("{schema}.obs");
    raw.observations.tables[1].table = format!("{schema}.second");
    raw.observations.tables[1].station_fk_col = Some("reporter".into());
    raw.observations.tables[1].time_col = Some("valid_time".into());
    raw.observations.tables[1].time_col_tz = Some("UTC".into());
    let mut cfg = PostgisEngineConfig::resolve(&raw).unwrap();
    cfg.dsn = initial.config().dsn.clone();
    let engine = PostgisEngine::new(schema, Arc::new(cfg.clone()), initial.pool().clone());
    let logs = audit(&engine).await;
    assert!(!has(&logs, MISSING, "obs", "station/time"));
    assert!(
        has(&logs, MISSING, "second", "station/time"),
        "{}",
        logs.text()
    );
    assert!(
        logs.text().contains("\"reporter\", \"valid_time\""),
        "suggestion uses resolved names: {}",
        logs.text()
    );
    exec(
        &client,
        schema,
        "CREATE INDEX second_idx ON __S__.second (reporter DESC, valid_time ASC)",
    )
    .await;
    let rebuilt = PostgisEngine::new(schema, Arc::new(cfg), initial.pool().clone());
    let logs = audit(&rebuilt).await;
    assert!(!has(&logs, MISSING, "second", "station/time"));
    assert!(!has(&logs, UNKNOWN, "second", "station/time"));

    exec(&client, schema, "
        CREATE TABLE __S__.events (id text, time timestamp, geom geometry(Point,4326), t2m float8, rh float8);
        INSERT INTO __S__.events VALUES ('a','2026-01-01 06:00:00','SRID=4326;POINT(1 45)',12,45);
        GRANT SELECT ON __S__.events TO mc1149_mapping_reader;
        CREATE INDEX event_geom ON __S__.events USING gist (geom);
    ").await;
    let raw = config(
        None,
        PostgisObservationsConfig {
            table: Some(format!("{schema}.events")),
            id_col: Some("id".into()),
            time_col: Some("time".into()),
            time_col_tz: Some("UTC".into()),
            geom_col: Some("geom".into()),
            extent_bbox: Some([0.0, 40.0, 10.0, 50.0]),
            ..observations("events")
        },
    );
    for (keys, expected) in [
        ("(id, time)", MISSING),
        ("(time ASC, id DESC)", "found"),
        ("(time DESC) INCLUDE (id)", "found"),
    ] {
        exec(&client, schema, "DROP INDEX IF EXISTS __S__.event_time").await;
        exec(
            &client,
            schema,
            &format!("CREATE INDEX event_time ON __S__.events {keys}"),
        )
        .await;
        let mut cfg = PostgisEngineConfig::resolve(&raw).unwrap();
        // Fresh pool makes each fixture's diagnostic independently observable.
        let fresh = role_engine(schema, Some(roles[0])).await;
        cfg.dsn = fresh.config().dsn.clone();
        let engine = PostgisEngine::new(schema, Arc::new(cfg), fresh.pool().clone());
        let logs = audit(&engine).await;
        assert_eq!(
            has(&logs, MISSING, "events", "event time"),
            expected == MISSING,
            "{keys}: {}",
            logs.text()
        );
        assert!(
            !has(&logs, MISSING, "events", "spatial geometry"),
            "{}",
            logs.text()
        );
    }
    cleanup(&client, schema, &roles).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partition_parents_hypertables_views_and_materialized_views() {
    let schema = "mc1149_relations";
    let roles = ["mc1149_relation_reader"];
    let Some(client) = fixture(schema, &roles).await else {
        return;
    };
    exec(&client, schema, "
        CREATE TABLE __S__.partitioned (LIKE __S__.obs) PARTITION BY RANGE (obstime);
        CREATE TABLE __S__.child PARTITION OF __S__.partitioned FOR VALUES FROM (MINVALUE) TO (MAXVALUE);
        INSERT INTO __S__.partitioned SELECT * FROM __S__.obs;
        CREATE INDEX child_idx ON __S__.child (station_id, obstime);
        CREATE VIEW __S__.view_obs AS SELECT * FROM __S__.obs;
        CREATE MATERIALIZED VIEW __S__.materialized AS SELECT * FROM __S__.obs;
        CREATE INDEX materialized_idx ON __S__.materialized (station_id, obstime);
        GRANT SELECT ON ALL TABLES IN SCHEMA __S__ TO mc1149_relation_reader;
    ").await;
    for (table, expected) in [
        ("partitioned", UNKNOWN),
        ("view_obs", UNKNOWN),
        ("materialized", "found"),
    ] {
        let initial = role_engine(schema, Some(roles[0])).await;
        let mut cfg = initial.config().clone();
        let engine_postgis::ObservationSchema::Long(s) = &mut cfg.observations else {
            unreachable!()
        };
        s.table.table = table.into();
        let engine = PostgisEngine::new(schema, Arc::new(cfg), initial.pool().clone());
        let logs = audit(&engine).await;
        assert!(
            !has(&logs, MISSING, table, "station/time"),
            "{}",
            logs.text()
        );
        assert_eq!(
            has(&logs, UNKNOWN, table, "station/time"),
            expected == UNKNOWN,
            "{}",
            logs.text()
        );
    }
    exec(
        &client,
        schema,
        "CREATE INDEX parent_idx ON __S__.partitioned (station_id, obstime)",
    )
    .await;
    let initial = role_engine(schema, Some(roles[0])).await;
    let mut cfg = initial.config().clone();
    let engine_postgis::ObservationSchema::Long(s) = &mut cfg.observations else {
        unreachable!()
    };
    s.table.table = "partitioned".into();
    let logs = audit(&PostgisEngine::new(
        schema,
        Arc::new(cfg),
        initial.pool().clone(),
    ))
    .await;
    assert!(!has(&logs, MISSING, "partitioned", "station/time"));
    assert!(!has(&logs, UNKNOWN, "partitioned", "station/time"));

    client
        .batch_execute("CREATE EXTENSION IF NOT EXISTS timescaledb")
        .await
        .unwrap();
    exec(
        &client,
        schema,
        "ALTER TABLE __S__.obs ALTER COLUMN obstime SET NOT NULL",
    )
    .await;
    client
        .query_one(
            "SELECT create_hypertable('mc1149_relations.obs', 'obstime', migrate_data => true)",
            &[],
        )
        .await
        .unwrap();
    exec(
        &client,
        schema,
        "CREATE INDEX hypertable_idx ON __S__.obs (station_id, obstime DESC)",
    )
    .await;
    let logs = audit(&*role_engine(schema, Some(roles[0])).await).await;
    assert!(
        !has(&logs, MISSING, "obs", "station/time"),
        "{}",
        logs.text()
    );
    assert!(
        !has(&logs, UNKNOWN, "obs", "station/time"),
        "{}",
        logs.text()
    );
    cleanup(&client, schema, &roles).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_diagnostics_and_changed_mapping_rebuild() {
    let schema = "mc1149_reload";
    let roles = ["mc1149_reload_reader"];
    let Some(client) = fixture(schema, &roles).await else {
        return;
    };
    let first = role_engine(schema, Some(roles[0])).await;
    let second = PostgisEngine::new(
        "second",
        Arc::new(first.config().clone()),
        first.pool().clone(),
    );
    let logs = Logs::default();
    logs.capture(async {
        let (a, b) = tokio::join!(first.refresh_metadata(), second.refresh_metadata());
        a.unwrap();
        b.unwrap();
    })
    .await;
    assert_eq!(logs.count(COMPLETE), 2, "{}", logs.text());
    assert_eq!(
        logs.count(MISSING),
        3,
        "two spatial paths plus station/time, once each: {}",
        logs.text()
    );
    logs.capture(async {
        first.refresh_metadata().await.unwrap();
        second.refresh_metadata().await.unwrap();
        first
            .query_location("a", Some((t0(), t0())), None, None, None)
            .unwrap();
    })
    .await;
    assert_eq!(logs.count(COMPLETE), 2);
    assert_eq!(logs.count(MISSING), 3);
    let mut cfg = first.config().clone();
    let engine_postgis::ObservationSchema::Long(mapping) = &mut cfg.observations else {
        unreachable!()
    };
    mapping.time_col = "alternate_time".into();
    let mut registry = PoolRegistry::new();
    registry.seed(&cfg.dsn, first.pool().clone()).unwrap();
    let pool = registry
        .get_or_create(&cfg.dsn, NonZeroU32::new(1).unwrap())
        .unwrap();
    let rebuilt = PostgisEngine::new("rebuilt", Arc::new(cfg), pool);
    logs.capture(rebuilt.refresh_metadata()).await.unwrap();
    assert_eq!(logs.count(COMPLETE), 3);
    assert_eq!(
        logs.count(MISSING),
        4,
        "changed key mapping must be diagnosed: {}",
        logs.text()
    );
    assert!(logs.text().lines().any(|line| line.contains(MISSING)
        && line.contains("collection=rebuilt")
        && line.contains("alternate_time")));
    cleanup(&client, schema, &roles).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn denied_catalog_is_unchecked_and_retries_without_affecting_availability() {
    let schema = "mc1149_unchecked";
    let roles = ["mc1149_unchecked_reader"];
    let Some(client) = fixture(schema, &roles).await else {
        return;
    };
    let engine = role_engine(schema, Some(roles[0])).await;
    let logs = Logs::default();
    client
        .batch_execute("REVOKE SELECT ON pg_catalog.pg_index FROM PUBLIC")
        .await
        .unwrap();
    let result = std::panic::AssertUnwindSafe(logs.capture(async {
        engine.refresh_metadata().await.unwrap();
        engine.refresh_metadata().await.unwrap();
        assert_eq!(logs.count(UNCHECKED), 1, "{}", logs.text());
        assert_eq!(logs.count(COMPLETE), 0);
        assert_eq!(
            logs.count(MISSING),
            0,
            "failed checks must not invent missing indexes"
        );
        assert_eq!(
            logs.count("postgis: privilege audit completed"),
            1,
            "audits are independent"
        );
        assert_eq!(engine.health_snapshot().refresh_failures, 0);
        assert_eq!(engine.get_locations().unwrap().len(), 1);
        assert!(engine
            .query_location("a", Some((t0(), t0())), None, None, None)
            .is_ok());
    }))
    .catch_unwind()
    .await;
    client
        .batch_execute("GRANT SELECT ON pg_catalog.pg_index TO PUBLIC")
        .await
        .unwrap();
    result.unwrap();
    logs.capture(async {
        let task = tokio::spawn({
            let engine = engine.clone();
            async move { engine.poll_loop().await }.with_current_subscriber()
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            while logs.count(COMPLETE) == 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            engine.live_health(),
            Some(engine_postgis::LiveHealth::Ready)
        );
        engine.shutdown();
        task.await.unwrap();
    })
    .await;
    assert_eq!(logs.count(COMPLETE), 1);
    assert_eq!(logs.count(UNCHECKED), 1);
    assert!(!logs.text().contains("postgres://"));
    cleanup(&client, schema, &roles).await;
}
