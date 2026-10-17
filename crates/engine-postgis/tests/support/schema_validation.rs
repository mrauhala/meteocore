//! Small, independent fixtures for database mapping validation (#1147).
use super::*;
use ds_core::events::EventSource;
use ds_core::feature_engine::FeatureEngine;
use engine_postgis::LiveHealth;

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
        CREATE TABLE __S__.stations (id text, name text, geom geometry(Point,4326));
        INSERT INTO __S__.stations VALUES ('a', 'A', ST_SetSRID(ST_Point(1,45),4326));
        CREATE TABLE __S__.obs (
            station_id text, obstime timestamptz, local_time timestamp,
            param text, value float8, geom geometry(Point,4326)
        );
        INSERT INTO __S__.obs VALUES ('a', '2026-01-01 06:00:00+00',
            '2026-01-01 08:00:00', 't2m', 12, ST_SetSRID(ST_Point(1,45),4326));
    ",
    )
    .await;
    Some(client)
}

fn unrefreshed(id: &str, config: PostgisEngineConfig) -> Arc<PostgisEngine> {
    let pool = PoolRegistry::new()
        .get_or_create(&config.dsn, NonZeroU32::new(2).unwrap())
        .unwrap();
    Arc::new(PostgisEngine::new(id, Arc::new(config), pool))
}

fn resolve(cfg: &PostgisConfig) -> PostgisEngineConfig {
    PostgisEngineConfig::resolve(cfg).unwrap()
}

fn dsn_option(dsn: &str, uri: &str, keyword: &str) -> String {
    if dsn.starts_with("postgres://") || dsn.starts_with("postgresql://") {
        format!("{dsn}{}{uri}", if dsn.contains('?') { '&' } else { '?' })
    } else {
        format!("{dsn} {keyword}")
    }
}

async fn mismatch(cfg: &PostgisConfig) -> Arc<PostgisEngine> {
    let engine = unrefreshed("invalid", resolve(cfg));
    let error = engine.refresh_metadata().await.unwrap_err().to_string();
    assert!(error.contains("schema does not match"), "{error}");
    assert!(
        !error.contains("postgres://") && !error.contains("mc1147"),
        "public diagnostics must be generic: {error}"
    );
    assert_eq!(engine.live_health(), Some(LiveHealth::SchemaInvalid));
    assert!(engine.get_locations().is_err());
    assert!(engine.get_feature("a").is_err());
    engine
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timestamp_mappings_and_non_utc_sessions() {
    let schema = "mc1147_times";
    let Some(client) = fixture(schema).await else {
        return;
    };
    let mut cfg = long_config(schema);
    for (column, zone) in [("obstime", None), ("local_time", Some("Europe/Helsinki"))] {
        cfg.observations.time_col = Some(column.into());
        cfg.observations.time_col_tz = zone.map(str::to_string);
        let mut resolved = resolve(&cfg);
        resolved.dsn = dsn_option(
            &resolved.dsn,
            "options=-c%20TimeZone%3DPacific%2FHonolulu",
            "options='-c TimeZone=Pacific/Honolulu'",
        );
        let engine = unrefreshed(column, resolved);
        engine.refresh_metadata().await.unwrap();
        let session = engine.pool().get().await.unwrap();
        let zone: String = session
            .query_one("SHOW TimeZone", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(zone, "Pacific/Honolulu");
        drop(session);
        assert_eq!(engine.get_temporal_extent(), Some((t0(), t0())));
        let actual = engine
            .query_location("a", Some((t0(), t0())), Some(&["t2m".into()]), None, None)
            .unwrap();
        let CoverageResponse::Single(actual) = actual else {
            panic!("single station")
        };
        assert_eq!(actual.ranges["t2m"].values, vec![Some(12.0)]);
        let DomainDescription::PointSeries { t, .. } = actual.domain else {
            panic!("station series")
        };
        assert_eq!(t, vec![t0()]);
    }
    cfg.observations.time_col_tz = None;
    mismatch(&cfg).await; // naive without timezone
    cfg.observations.time_col_tz = Some("Not/A_Real_Zone".into());
    mismatch(&cfg).await;
    cfg.observations.time_col = Some("obstime".into());
    cfg.observations.time_col_tz = Some("UTC".into());
    mismatch(&cfg).await;
    cfg.observations.time_col = Some("value".into());
    mismatch(&cfg).await;
    cfg.observations.time_col = Some("absent".into());
    mismatch(&cfg).await;
    exec(&client, schema, "DROP SCHEMA __S__ CASCADE").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_per_parameter_override_is_checked() {
    let schema = "mc1147_overrides";
    let Some(client) = fixture(schema).await else {
        return;
    };
    let mut cfg = per_parameter_config(schema);
    cfg.stations = Some(stations(schema));
    cfg.observations.station_fk_col = Some("station_id".into());
    cfg.observations.time_col = Some("obstime".into());
    cfg.observations.geom_col = Some("geom".into());
    for table in &mut cfg.observations.tables {
        table.table = format!("{schema}.obs");
    }
    cfg.observations.tables[1].time_col = Some("local_time".into());
    cfg.observations.tables[1].time_col_tz = Some("Europe/Helsinki".into());
    engine("valid-overrides", &cfg).await;
    cfg.observations.tables[1].time_col_tz = Some("Invalid/Timezone".into());
    mismatch(&cfg).await;
    cfg.observations.tables[1].time_col_tz = None;
    mismatch(&cfg).await; // first table remains valid
    cfg.observations.tables[1].time_col_tz = Some("Europe/Helsinki".into());
    exec(
        &client,
        schema,
        "ALTER TABLE __S__.obs ADD COLUMN bad_geom geometry(Point,3857)",
    )
    .await;
    cfg.observations.tables[1].geom_col = Some("bad_geom".into());
    mismatch(&cfg).await;
    exec(&client, schema, "DROP SCHEMA __S__ CASCADE").await;
}

fn event_config(schema: &str, table: &str) -> PostgisConfig {
    let mut cfg = config(
        None,
        PostgisObservationsConfig {
            table: Some(format!("{schema}.{table}")),
            time_col: Some("obstime".into()),
            geom_col: Some("geom".into()),
            id_col: Some("station_id".into()),
            ..observations("events")
        },
    );
    cfg.parameters.clear();
    cfg
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn geometry_declarations_views_and_empty_sources() {
    let schema = "mc1147_geometry";
    let Some(client) = fixture(schema).await else {
        return;
    };
    exec(&client, schema, "
        CREATE VIEW __S__.typed AS SELECT * FROM __S__.obs;
        CREATE VIEW __S__.untyped AS SELECT station_id, obstime, ST_Translate(geom,0,0) AS geom FROM __S__.obs;
        CREATE MATERIALIZED VIEW __S__.materialized AS SELECT * FROM __S__.untyped;
        CREATE VIEW __S__.empty AS SELECT * FROM __S__.untyped WHERE false;
        CREATE TABLE __S__.empty_typed (LIKE __S__.obs);
        CREATE TABLE __S__.generic (station_id text, obstime timestamptz, geom geometry);
        INSERT INTO __S__.generic VALUES ('null', now(), NULL), ('empty', now(), 'SRID=4326;POINT EMPTY');
        CREATE TABLE __S__.wrong_srid (station_id text, obstime timestamptz, geom geometry(Point,3857));
        CREATE TABLE __S__.wrong_shape (station_id text, obstime timestamptz, geom geometry(Polygon,4326));
        CREATE TABLE __S__.wrong_type (station_id text, obstime timestamptz, geom text);
        CREATE TABLE __S__.constraint_srid (station_id text, obstime timestamptz, geom geometry CHECK (ST_SRID(geom) = 3857));
        CREATE TABLE __S__.constraint_shape (station_id text, obstime timestamptz, geom geometry CHECK (GeometryType(geom) = 'POLYGON'));
        CREATE VIEW __S__.wrong_view AS SELECT station_id, obstime, ST_SetSRID(geom,3857) AS geom FROM __S__.obs;
    ").await;
    for table in [
        "obs",
        "typed",
        "untyped",
        "materialized",
        "empty",
        "empty_typed",
        "generic",
    ] {
        let engine = engine(table, &event_config(schema, table)).await;
        let events = engine
            .recent_events(t0() - hours(1), Utc::now(), 20)
            .unwrap();
        assert_eq!(
            events.len(),
            usize::from(matches!(
                table,
                "obs" | "typed" | "untyped" | "materialized"
            )),
            "{table}"
        );
    }
    for table in [
        "wrong_srid",
        "wrong_shape",
        "wrong_type",
        "wrong_view",
        "constraint_srid",
        "constraint_shape",
    ] {
        mismatch(&event_config(schema, table)).await;
    }
    // Registry and observation-derived geometry are both inspected.
    exec(&client, schema, "ALTER TABLE __S__.stations ALTER COLUMN geom TYPE geometry(Point,3857) USING ST_SetSRID(geom,3857)").await;
    mismatch(&long_config(schema)).await;
    let mut cfg = long_config(schema);
    cfg.stations = None;
    cfg.observations.geom_col = Some("geom".into());
    cfg.observations.locations_window = Some("all".into());
    engine("observation-geometry", &cfg).await;
    cfg.observations.table = Some(format!("{schema}.wrong_srid"));
    mismatch(&cfg).await;
    exec(&client, schema, "DROP SCHEMA __S__ CASCADE").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn geometry_outside_the_sample_is_never_served() {
    let schema = "mc1147_sample";
    let Some(client) = fixture(schema).await else {
        return;
    };
    exec(&client, schema, "
        CREATE TABLE __S__.generic AS SELECT n::text AS station_id, TIMESTAMPTZ '2026-01-01 06:00:00+00' AS obstime, NULL::geometry AS geom FROM generate_series(1,128) n;
        INSERT INTO __S__.generic VALUES ('bad', '2026-01-01 06:00:00+00', 'SRID=3857;POINT(1 45)');
    ").await;
    let cfg = event_config(schema, "generic");
    let engine = engine("sample-bound", &cfg).await;
    assert!(engine.recent_events(t0() - hours(1), t0(), 1000).is_err());
    assert_eq!(engine.live_health(), Some(LiveHealth::SchemaInvalid));
    // An unknown declaration also stays guarded when new data arrives later.
    exec(&client, schema, "TRUNCATE __S__.generic").await;
    engine.refresh_metadata().await.unwrap();
    exec(&client, schema, "INSERT INTO __S__.generic VALUES ('bad', '2026-01-01 06:00:00+00', 'SRID=4326;LINESTRING(1 45,2 46)')").await;
    assert!(engine.recent_events(t0() - hours(1), t0(), 10).is_err());
    assert_eq!(engine.live_health(), Some(LiveHealth::SchemaInvalid));
    exec(&client, schema, "DROP SCHEMA __S__ CASCADE").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_successful_ping_cannot_clear_schema_errors() {
    let schema = "mc1147_health";
    let Some(client) = fixture(schema).await else {
        return;
    };
    let mut cfg = long_config(schema);
    cfg.observations.time_col_tz = Some("UTC".into());
    let engine = mismatch(&cfg).await;
    let task = tokio::spawn({
        let engine = engine.clone();
        async move { engine.poll_loop().await }
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        while !engine.health_snapshot().probed || engine.health_snapshot().refresh_total < 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(engine.health_snapshot().ping_failures, 0);
    assert_eq!(engine.live_health(), Some(LiveHealth::SchemaInvalid));
    assert!(engine.query_location("a", None, None, None, None).is_err());
    exec(&client, schema, "ALTER TABLE __S__.obs ALTER COLUMN obstime TYPE timestamp USING obstime AT TIME ZONE 'UTC'").await;
    engine.refresh_metadata().await.unwrap();
    assert_eq!(engine.live_health(), Some(LiveHealth::Ready));
    assert_eq!(engine.get_locations().unwrap().len(), 1);
    engine.shutdown();
    task.await.unwrap();
    exec(&client, schema, "DROP SCHEMA __S__ CASCADE").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn validation_deadline_bounds_view_work() {
    let schema = "mc1147_deadline";
    let Some(client) = fixture(schema).await else {
        return;
    };
    exec(&client, schema, "CREATE VIEW __S__.slow AS SELECT station_id, obstime, geom::geometry AS geom FROM __S__.obs, LATERAL pg_sleep(15)").await;
    let engine = unrefreshed("slow-view", resolve(&event_config(schema, "slow")));
    let started = Instant::now();
    assert!(engine.refresh_metadata().await.is_err());
    assert!(started.elapsed() < Duration::from_secs(8));
    assert_eq!(engine.live_health(), Some(LiveHealth::ValidationPending));
    assert!(engine.recent_events(t0() - hours(1), t0(), 10).is_err());
    exec(&client, schema, "DROP SCHEMA __S__ CASCADE").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn database_down_at_boot_is_validated_on_recovery() {
    let schema = "mc1147_recovery";
    let Some(client) = fixture(schema).await else {
        return;
    };
    let mut config = resolve(&long_config(schema));
    let upstream: tokio_postgres::Config = config.dsn.parse().unwrap();
    let tokio_postgres::config::Host::Tcp(host) = &upstream.get_hosts()[0] else {
        panic!("test recovery proxy needs a TCP DSN")
    };
    let upstream_addr = (
        host.clone(),
        upstream.get_ports().first().copied().unwrap_or(5432),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener); // connection refused during initialization
                    // Rebuild the endpoint rather than appending host/port: libpq-style
                    // parsers can retain the original as a fallback in a multi-host DSN.
    let quote = |value: &str| format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"));
    let user = upstream.get_user().unwrap_or("postgres");
    let password = std::str::from_utf8(upstream.get_password().unwrap_or_default()).unwrap();
    config.dsn = format!(
        "host=127.0.0.1 port={} user={} password={} dbname={} sslmode=disable",
        addr.port(),
        quote(user),
        quote(password),
        quote(upstream.get_dbname().unwrap_or(user))
    );
    let engine = unrefreshed("recovered", config);
    assert!(engine.refresh_metadata().await.is_err());
    assert_eq!(engine.live_health(), Some(LiveHealth::ValidationPending));
    assert!(engine.query_location("a", None, None, None, None).is_err());
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    let proxy = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            let (mut incoming, _) = listener.accept().await.unwrap();
            let upstream_addr = upstream_addr.clone();
            connections.spawn(async move {
                let mut outgoing = tokio::net::TcpStream::connect(upstream_addr).await.unwrap();
                let _ = tokio::io::copy_bidirectional(&mut incoming, &mut outgoing).await;
            });
        }
    });
    let task = tokio::spawn({
        let engine = engine.clone();
        async move { engine.poll_loop().await }
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        while engine.live_health() != Some(LiveHealth::Ready) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(engine.get_locations().unwrap().len(), 1);
    assert!(engine
        .query_location("a", Some((t0(), t0())), None, None, None)
        .is_ok());
    engine.shutdown();
    task.await.unwrap();
    proxy.abort();
    exec(&client, schema, "DROP SCHEMA __S__ CASCADE").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registry_filter_and_returned_geometries_are_checked() {
    let schema = "mc1147_registry";
    let Some(client) = fixture(schema).await else {
        return;
    };
    exec(
        &client,
        schema,
        "
        ALTER TABLE __S__.stations ALTER COLUMN geom TYPE geometry;
        INSERT INTO __S__.stations VALUES ('excluded', 'Excluded', 'SRID=3857;POINT(1 45)');
    ",
    )
    .await;
    let mut cfg = long_config(schema);
    cfg.stations.as_mut().unwrap().where_clause = Some("id = 'a'".into());
    let engine = engine("filtered-registry", &cfg).await;
    assert_eq!(engine.get_locations().unwrap().len(), 1);
    // Keep the first 128 sample rows valid and put an incompatible station
    // after them: metadata decoding must still reject it atomically.
    exec(&client, schema, "
        TRUNCATE __S__.stations;
        INSERT INTO __S__.stations SELECT 'a', 'A', 'SRID=4326;POINT(1 45)'::geometry FROM generate_series(1,128);
        INSERT INTO __S__.stations VALUES ('a', 'A', 'SRID=3857;POINT(1 45)');
    ").await;
    assert!(engine.refresh_metadata().await.is_err());
    assert_eq!(engine.live_health(), Some(LiveHealth::SchemaInvalid));
    assert!(engine.get_locations().is_err());
    assert!(engine.get_features(&Default::default()).is_err());
    exec(&client, schema, "DROP SCHEMA __S__ CASCADE").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edr_and_maps_reject_new_incompatible_event_geometry() {
    use ds_core::map_engine::{MapEngine, OutputCrs};
    let schema = "mc1147_render";
    let Some(client) = fixture(schema).await else {
        return;
    };
    exec(
        &client,
        schema,
        "ALTER TABLE __S__.obs ALTER COLUMN geom TYPE geometry",
    )
    .await;
    let cfg = event_config(schema, "obs");
    let engine = engine("geometry-render", &cfg).await;
    // A non-point can satisfy the spatial predicate; it must never become a
    // Point coverage through its projected coordinates.
    exec(
        &client,
        schema,
        "UPDATE __S__.obs SET geom = 'SRID=4326;LINESTRING(1 45,2 46)'::geometry",
    )
    .await;
    assert!(engine
        .query_area(NETWORK, Some((t0(), t0())), None, None, None)
        .is_err());
    assert_eq!(engine.live_health(), Some(LiveHealth::SchemaInvalid));
    exec(
        &client,
        schema,
        "UPDATE __S__.obs SET geom = 'SRID=4326;POINT(1 45)'::geometry",
    )
    .await;
    engine.refresh_metadata().await.unwrap();
    exec(
        &client,
        schema,
        "UPDATE __S__.obs SET geom = 'SRID=3857;POINT(1 45)'::geometry",
    )
    .await;
    let render = {
        let engine = engine.clone();
        move || {
            engine.get_raster_tile(
                [0., 40., 20., 50.],
                16,
                16,
                Some(t0()),
                &OutputCrs::Wgs84,
                None,
                None,
                None,
            )
        }
    };
    assert!(tokio::task::spawn_blocking(render).await.unwrap().is_err());
    assert_eq!(engine.live_health(), Some(LiveHealth::SchemaInvalid));
    exec(&client, schema, "DROP SCHEMA __S__ CASCADE").await;
}
