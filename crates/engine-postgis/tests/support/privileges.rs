//! Advisory role diagnostics exercised through the public engine load path.
use super::*;
use futures::FutureExt;
use std::io::Write;
use std::sync::Mutex;
use tracing::instrument::WithSubscriber;

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Logs {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
    async fn capture<F: std::future::Future>(&self, future: F) -> F::Output {
        let writer = self.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        future.with_subscriber(subscriber).await
    }
    fn count(&self, message: &str) -> usize {
        self.text().matches(message).count()
    }
}

async fn fixture(schema: &str, roles: &[&str]) -> Option<tokio_postgres::Client> {
    let client = connect(&dsn()?).await;
    let _setup = SETUP.lock().await;
    client
        .batch_execute("CREATE EXTENSION IF NOT EXISTS postgis")
        .await
        .unwrap();
    exec(
        &client,
        schema,
        "DROP SCHEMA IF EXISTS __S__ CASCADE; CREATE SCHEMA __S__",
    )
    .await;
    for role in roles {
        let name = quote_ident(role).unwrap();
        client
            .batch_execute(&format!(
                "DROP ROLE IF EXISTS {name}; CREATE ROLE {name} INHERIT"
            ))
            .await
            .unwrap();
    }
    exec(&client, schema, "
        CREATE TABLE __S__.stations (id text, name text, geom geometry(Point,4326));
        INSERT INTO __S__.stations VALUES ('a','A','SRID=4326;POINT(1 45)');
        CREATE TABLE __S__.obs (station_id text, obstime timestamptz, alternate_time timestamptz,
                               param text, value float8, extra text);
        INSERT INTO __S__.obs VALUES ('a','2026-01-01 06:00:00+00','2026-01-01 06:00:00+00','t2m',12,'unmapped');
    ").await;
    for role in roles {
        let name = quote_ident(role).unwrap();
        exec(&client, schema, &format!("GRANT USAGE ON SCHEMA __S__ TO {name}; GRANT SELECT ON ALL TABLES IN SCHEMA __S__ TO {name}")).await;
    }
    Some(client)
}

async fn cleanup(client: &tokio_postgres::Client, schema: &str, roles: &[&str]) {
    exec(client, schema, "DROP SCHEMA __S__ CASCADE").await;
    for role in roles {
        client
            .batch_execute(&format!("DROP ROLE {}", quote_ident(role).unwrap()))
            .await
            .unwrap();
    }
}

/// Startup SET ROLE makes the effective role differ from the superuser login,
/// including on replacement connections after a failed audit is discarded.
async fn role_engine(schema: &str, role: Option<&str>) -> Arc<PostgisEngine> {
    let mut cfg = PostgisEngineConfig::resolve(&long_config(schema)).unwrap();
    let parsed: tokio_postgres::Config = cfg.dsn.parse().unwrap();
    let mut options = parsed.get_options().unwrap_or_default().to_string();
    if let Some(role) = role {
        options.push_str(&format!(" -c role={role}"));
    }
    options.push_str(" -c default_transaction_read_only=on -c statement_timeout=750ms");
    if cfg.dsn.starts_with("postgres://") || cfg.dsn.starts_with("postgresql://") {
        let encoded: String = options.bytes().map(|b| format!("%{b:02X}")).collect();
        cfg.dsn = format!(
            "{}{}options={encoded}",
            cfg.dsn,
            if cfg.dsn.contains('?') { '&' } else { '?' }
        );
    } else {
        cfg.dsn.push_str(&format!(
            " options='{}'",
            options.replace('\\', "\\\\").replace('\'', "\\'")
        ));
    }
    let pool = PoolRegistry::new()
        .get_or_create(&cfg.dsn, NonZeroU32::new(1).unwrap())
        .unwrap();
    Arc::new(PostgisEngine::new(schema, Arc::new(cfg), pool))
}

const COMPLETE: &str = "postgis: privilege audit completed";
const UNCHECKED: &str = "postgis: privilege audit unchecked";
const WRITES: &str = "postgis: database role has write privileges";
const SUPERUSER: &str = "postgis: database role is a superuser";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audits_views_and_every_parameter_relation_including_public_grants() {
    let schema = "mc1148_views";
    let roles = ["mc1148_views_reader"];
    let Some(client) = fixture(schema, &roles).await else {
        return;
    };
    exec(
        &client,
        schema,
        "
        CREATE VIEW __S__.station_view AS SELECT * FROM __S__.stations;
        CREATE MATERIALIZED VIEW __S__.first_parameter AS SELECT * FROM __S__.obs;
        CREATE VIEW __S__.second_parameter AS SELECT * FROM __S__.obs;
        GRANT SELECT ON ALL TABLES IN SCHEMA __S__ TO mc1148_views_reader;
        GRANT INSERT ON __S__.station_view TO PUBLIC;
        GRANT UPDATE ON __S__.second_parameter TO mc1148_views_reader;
    ",
    )
    .await;
    let initial = role_engine(schema, Some(roles[0])).await;
    let mut raw = per_parameter_config(schema);
    let mut station_mapping = stations(schema);
    station_mapping.table = format!("{schema}.station_view");
    raw.stations = Some(station_mapping);
    raw.observations.station_fk_col = Some("station_id".into());
    raw.observations.time_col = Some("obstime".into());
    raw.observations.geom_col = None;
    raw.observations.tables[0].table = format!("{schema}.first_parameter");
    raw.observations.tables[1].table = format!("{schema}.second_parameter");
    let mut cfg = PostgisEngineConfig::resolve(&raw).unwrap();
    cfg.dsn = initial.config().dsn.clone();
    let engine = PostgisEngine::new(schema, Arc::new(cfg), initial.pool().clone());
    let logs = Logs::default();
    logs.capture(engine.refresh_metadata()).await.unwrap();
    let text = logs.text();
    assert_eq!(logs.count(COMPLETE), 1, "{text}");
    assert_eq!(logs.count(UNCHECKED), 0, "{text}");
    assert_eq!(logs.count(WRITES), 2, "{text}");
    assert!(text
        .lines()
        .any(|line| line.contains(COMPLETE) && line.contains("relations=3")));
    for (relation, privilege) in [("station_view", "INSERT"), ("second_parameter", "UPDATE")] {
        assert!(
            text.lines().any(|line| line.contains(WRITES)
                && line.contains(&format!("relation={relation}"))
                && line.contains(privilege)),
            "{text}"
        );
    }
    assert_eq!(engine.get_locations().unwrap().len(), 1);
    cleanup(&client, schema, &roles).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn effective_privilege_matrix_and_read_only_audit() {
    let schema = "mc1148_matrix";
    let roles = [
        "mc1148_reader",
        "mc1148_owner",
        "mc1148_group",
        "mc1148_inherited",
        "mc1148_columns",
        "mc1148_insert",
    ];
    let Some(client) = fixture(schema, &roles).await else {
        return;
    };
    exec(
        &client,
        schema,
        "
        GRANT DELETE ON __S__.obs TO mc1148_group;
        GRANT mc1148_group TO mc1148_inherited;
        GRANT INSERT(extra), UPDATE(extra) ON __S__.obs TO mc1148_columns;
        GRANT INSERT ON __S__.obs TO mc1148_insert;
        ALTER TABLE __S__.obs OWNER TO mc1148_owner;
    ",
    )
    .await;
    for (role, expected) in [
        (Some("mc1148_reader"), vec![]),
        (Some("mc1148_owner"), vec!["INSERT", "UPDATE", "DELETE"]),
        (Some("mc1148_inherited"), vec!["DELETE"]),
        (Some("mc1148_columns"), vec!["INSERT", "UPDATE"]),
        (Some("mc1148_insert"), vec!["INSERT"]),
        (None, vec!["INSERT", "UPDATE", "DELETE"]),
    ] {
        let engine = role_engine(schema, role).await;
        let logs = Logs::default();
        logs.capture(engine.refresh_metadata()).await.unwrap();
        assert_eq!(logs.count(COMPLETE), 1);
        assert_eq!(logs.count(UNCHECKED), 0, "{}", logs.text());
        assert_eq!(logs.count(SUPERUSER), usize::from(role.is_none()));
        let text = logs.text();
        let writes: Vec<_> = text.lines().filter(|line| line.contains(WRITES)).collect();
        if expected.is_empty() {
            assert!(
                writes.is_empty(),
                "extra readable columns must be quiet: {text}"
            );
            assert!(!text.contains("WARN"), "{text}");
        } else {
            assert_eq!(writes.len(), if role.is_none() { 2 } else { 1 }, "{text}");
            let obs = writes
                .iter()
                .find(|line| line.contains("relation=obs"))
                .unwrap();
            for permission in expected {
                assert!(obs.contains(permission), "{obs}");
            }
            assert!(
                obs.contains("schema=mc1148_matrix") && obs.contains("collection=mc1148_matrix")
            );
            if let Some(role) = role {
                assert!(obs.contains(&format!("role={role}")), "{obs}");
            }
            if role == Some("mc1148_owner") {
                assert!(obs.contains("owns_relation=true"));
            }
        }
        let session = engine.pool().get().await.unwrap();
        assert_eq!(
            session
                .query_one("SHOW statement_timeout", &[])
                .await
                .unwrap()
                .get::<_, String>(0),
            "750ms"
        );
        assert_eq!(
            session
                .query_one("SHOW transaction_read_only", &[])
                .await
                .unwrap()
                .get::<_, String>(0),
            "on"
        );
        // Column grants must be found even when table-level write checks fail.
        if role == Some("mc1148_columns") {
            let row = session
                .query_one(
                    "SELECT has_table_privilege($1, 'INSERT'), has_table_privilege($1, 'UPDATE')",
                    &[&format!("{schema}.obs")],
                )
                .await
                .unwrap();
            assert!(!row.get::<_, bool>(0) && !row.get::<_, bool>(1));
        }
        drop(session);
        assert_eq!(engine.get_locations().unwrap().len(), 1);
        assert_eq!(engine.health_snapshot().refresh_failures, 0);
    }
    let row = client
        .query_one("SELECT count(*), min(value) FROM mc1148_matrix.obs", &[])
        .await
        .unwrap();
    assert_eq!(row.get::<_, i64>(0), 1);
    assert_eq!(row.get::<_, f64>(1), 12.0);
    cleanup(&client, schema, &roles).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_relations_warn_once_and_rebuilds_recheck() {
    let schema = "mc1148_dedup";
    let roles = ["mc1148_shared"];
    let Some(client) = fixture(schema, &roles).await else {
        return;
    };
    exec(
        &client,
        schema,
        "GRANT UPDATE ON __S__.obs TO mc1148_shared",
    )
    .await;
    let first = role_engine(schema, Some(roles[0])).await;
    let second = Arc::new(PostgisEngine::new(
        "second",
        Arc::new(first.config().clone()),
        first.pool().clone(),
    ));
    let logs = Logs::default();
    logs.capture(async {
        let (one, two) = tokio::join!(first.refresh_metadata(), second.refresh_metadata());
        one.unwrap();
        two.unwrap();
    })
    .await;
    assert_eq!(logs.count(COMPLETE), 2);
    assert_eq!(logs.count(WRITES), 1, "{}", logs.text());
    exec(
        &client,
        schema,
        "REVOKE UPDATE ON __S__.obs FROM mc1148_shared; GRANT DELETE ON __S__.obs TO mc1148_shared",
    )
    .await;
    logs.capture(async {
        first.refresh_metadata().await.unwrap();
        second.refresh_metadata().await.unwrap();
        first
            .query_location("a", Some((t0(), t0())), Some(&["t2m".into()]), None, None)
            .unwrap();
    })
    .await;
    assert_eq!(
        logs.count(COMPLETE),
        2,
        "no audits on periodic refresh or requests"
    );
    assert_eq!(logs.count(WRITES), 1);
    let mut changed = first.config().clone();
    let engine_postgis::ObservationSchema::Long(mapping) = &mut changed.observations else {
        unreachable!()
    };
    mapping.time_col = "alternate_time".into();
    let mut registry = PoolRegistry::new();
    registry.seed(&changed.dsn, first.pool().clone()).unwrap();
    let reused = registry
        .get_or_create(&changed.dsn, NonZeroU32::new(1).unwrap())
        .unwrap();
    let rebuilt = PostgisEngine::new("rebuilt", Arc::new(changed), reused);
    logs.capture(rebuilt.refresh_metadata()).await.unwrap();
    assert_eq!(logs.count(COMPLETE), 3);
    assert_eq!(
        logs.count(WRITES),
        2,
        "newly detected DELETE must warn: {}",
        logs.text()
    );
    assert!(logs.text().lines().any(|line| line.contains(WRITES)
        && line.contains("collection=rebuilt")
        && line.contains("DELETE")));
    cleanup(&client, schema, &roles).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unchecked_audit_is_advisory_and_retries_on_successful_ping() {
    let schema = "mc1148_unchecked";
    let roles = ["mc1148_unchecked_reader"];
    let Some(client) = fixture(schema, &roles).await else {
        return;
    };
    let engine = role_engine(schema, Some(roles[0])).await;
    let logs = Logs::default();
    // Simulate catalog checks being disallowed while normal data reads work.
    // Restore the database-wide function ACL even if an assertion panics.
    client
        .batch_execute(
            "REVOKE EXECUTE ON FUNCTION pg_catalog.has_any_column_privilege(oid,text) FROM PUBLIC",
        )
        .await
        .unwrap();
    let result = std::panic::AssertUnwindSafe(logs.capture(async {
        engine.refresh_metadata().await.unwrap();
        engine.refresh_metadata().await.unwrap();
        assert_eq!(logs.count(UNCHECKED), 1, "{}", logs.text());
        assert_eq!(logs.count(COMPLETE), 0);
        assert_eq!(engine.health_snapshot().refresh_failures, 0);
        assert_eq!(engine.get_locations().unwrap().len(), 1);
        assert!(engine
            .query_location("a", Some((t0(), t0())), None, None, None)
            .is_ok());
    }))
    .catch_unwind()
    .await;
    client
        .batch_execute(
            "GRANT EXECUTE ON FUNCTION pg_catalog.has_any_column_privilege(oid,text) TO PUBLIC",
        )
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pool_wait_has_a_deadline_without_failing_metadata() {
    let schema = "mc1148_deadline";
    let roles = ["mc1148_deadline_reader"];
    let Some(client) = fixture(schema, &roles).await else {
        return;
    };
    exec(&client, schema, "CREATE VIEW __S__.slow AS SELECT obs.* FROM __S__.obs, LATERAL pg_sleep(0.5); GRANT SELECT ON __S__.slow TO mc1148_deadline_reader").await;
    let initial = role_engine(schema, Some(roles[0])).await;
    let session = initial.pool().get().await.unwrap();
    let pid: i32 = session
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    drop(session);
    let mut cfg = initial.config().clone();
    let engine_postgis::ObservationSchema::Long(mapping) = &mut cfg.observations else {
        unreachable!()
    };
    mapping.table.table = "slow".into();
    let engine = Arc::new(PostgisEngine::new(
        schema,
        Arc::new(cfg),
        initial.pool().clone(),
    ));
    let logs = Logs::default();
    let refresh = tokio::spawn({
        let engine = engine.clone();
        let logs = logs.clone();
        async move { logs.capture(engine.refresh_metadata()).await }
    });
    // Queue for the sole pooled connection while temporal metadata is reading
    // the slow view. We receive it before the audit's pool acquisition.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let row = client.query_one("SELECT state = 'active' AND query LIKE '%MIN(%' FROM pg_stat_activity WHERE pid = $1", &[&pid]).await.unwrap();
            if row.get::<_, bool>(0) { break; }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await.unwrap();
    let held = engine.pool().get().await.unwrap();
    let start = Instant::now();
    tokio::time::timeout(Duration::from_secs(4), refresh)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(start.elapsed() < Duration::from_secs(4));
    assert_eq!(logs.count(UNCHECKED), 1, "{}", logs.text());
    assert_eq!(logs.count(COMPLETE), 0);
    assert_eq!(engine.health_snapshot().refresh_failures, 0);
    assert_eq!(engine.get_locations().unwrap().len(), 1);
    drop(held);
    logs.capture(engine.refresh_metadata()).await.unwrap();
    assert_eq!(logs.count(COMPLETE), 1);
    cleanup(&client, schema, &roles).await;
}
