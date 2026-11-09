//! #1446: ordered statements sort by the table's own columns, and a
//! statement the database cancels for its `statement_timeout` is the
//! deadline, not a 500.
use super::schema_validation::dsn_option;
use super::*;
use ds_core::events::EventSource;
use ds_core::feature::{DatetimeInterval, FeatureQuery};
use ds_core::feature_pick::{FeaturePick, PickMap};
use ds_core::map_engine::{MapEngine, OutputCrs};
use engine_postgis::query::{
    build_events_area, build_events_pick, build_events_window, build_location, BuiltQuery,
    SqlParam, WindowAttrs,
};

/// A zone with daylight saving time. Its clocks went from 03:00 to 04:00
/// on 2026-03-29, so a naive `03:30` that day lies in the gap and reads
/// back as standard time, 01:30Z, after `04:10` (01:10Z). Only such a row
/// orders differently by the stored column than by its converted output,
/// which makes the column an `ORDER BY` names observable.
const ZONE: &str = "Europe/Helsinki";

/// Three strikes at one point, and a station reporting at the same three
/// naive times, `time` columns with no time zone. Stored order 1, 2, 3;
/// converted order 1, 3, 2. `peak_current` and `value` carry the id.
const NAIVE_GAP: &str = "
DROP SCHEMA IF EXISTS __S__ CASCADE;
CREATE SCHEMA __S__;
CREATE TABLE __S__.strikes (
    id bigint NOT NULL,
    time timestamp without time zone NOT NULL,
    geom geometry(Point, 4326) NOT NULL,
    peak_current real,
    PRIMARY KEY (time, id)
);
INSERT INTO __S__.strikes VALUES
    (1, '2026-03-29 02:50:00', ST_SetSRID(ST_MakePoint(1.002, 1.008), 4326), 1),
    (2, '2026-03-29 03:30:00', ST_SetSRID(ST_MakePoint(1.002, 1.008), 4326), 2),
    (3, '2026-03-29 04:10:00', ST_SetSRID(ST_MakePoint(1.002, 1.008), 4326), 3);
CREATE INDEX ON __S__.strikes USING GIST (geom);
CREATE TABLE __S__.stations (
    id text PRIMARY KEY,
    name text NOT NULL,
    geom geometry(Point, 4326) NOT NULL
);
INSERT INTO __S__.stations VALUES ('a', 'A', ST_SetSRID(ST_MakePoint(1, 1), 4326));
CREATE TABLE __S__.obs (
    station_id text NOT NULL,
    time timestamp without time zone NOT NULL,
    param text NOT NULL,
    value double precision
);
INSERT INTO __S__.obs SELECT 'a', time, 't2m', id FROM __S__.strikes;
CREATE INDEX ON __S__.obs (station_id, time DESC);
ANALYZE;
";

fn at(hh_mm: &str) -> DateTime<Utc> {
    format!("2026-03-29T{hh_mm}:00Z").parse().unwrap()
}

fn peak_current() -> PostgisParameterConfig {
    PostgisParameterConfig {
        name: "peak_current".into(),
        label: "Peak current".into(),
        unit: "kA".into(),
        observed_property: None,
        source_key: None,
        valid_range: None,
        missing_values: vec![],
    }
}

fn naive_events_config(schema: &str, table: &str) -> PostgisConfig {
    let mut cfg = config(
        None,
        PostgisObservationsConfig {
            table: Some(format!("{schema}.{table}")),
            time_col: Some("time".into()),
            time_col_tz: Some(ZONE.into()),
            geom_col: Some("geom".into()),
            id_col: Some("id".into()),
            extent_bbox: Some([0.0, 0.0, 2.0, 2.0]),
            ..observations("events")
        },
    );
    cfg.parameters = vec![peak_current()];
    cfg
}

/// The `Sort Key` and `Presorted Key` lines of `sql`'s plan, and the plan.
async fn sort_keys(
    client: &tokio_postgres::Client,
    sql: &str,
    params: &[SqlParam],
) -> (Vec<String>, String) {
    let plan: Vec<String> = client
        .query(&format!("EXPLAIN {sql}"), &params_as_refs(params))
        .await
        .unwrap_or_else(|e| panic!("EXPLAIN {sql}: {e}"))
        .iter()
        .map(|row| row.get(0))
        .collect();
    let keys = plan
        .iter()
        .filter(|l| l.contains("Sort Key") || l.contains("Presorted Key"))
        .cloned()
        .collect();
    (keys, plan.join("\n"))
}

/// Whether a sort key is the converted time: `AT TIME ZONE` as PostgreSQL
/// 16 and later print it, `timezone(...)` as earlier versions do.
fn sorts_by_a_conversion(keys: &[String]) -> bool {
    keys.iter().any(|key| {
        let key = key.to_lowercase();
        key.contains("time zone") || key.contains("timezone(")
    })
}

/// `q` sorts by the table's column, and the same statement with the bare
/// names #1446 replaced would sort by the converted output: the check sees
/// the difference on this server.
async fn assert_sorts_by_the_column(client: &tokio_postgres::Client, q: &BuiltQuery) {
    let (keys, plan) = sort_keys(client, &q.sql, &q.params).await;
    assert!(!sorts_by_a_conversion(&keys), "{}\n{plan}", q.sql);
    let bare = q.sql.replace("mc_events.", "").replace("mc_obs.", "");
    assert_ne!(bare, q.sql, "{}", q.sql);
    let (keys, plan) = sort_keys(client, &bare, &q.params).await;
    assert!(sorts_by_a_conversion(&keys), "{bare}\n{plan}");
}

/// #1446: on a naive `time` column, the events area, GetFeatureInfo pick,
/// map window and Features page all keep the newest strikes by the stored
/// column, the one the `WHERE` bounds and the `(time, id)` index read, so a
/// `LIMIT` keeps the same strike in each. The station location and
/// set-based statements sort by it too. No plan sorts by the converted
/// output column, which the bare `ORDER BY "time"` they had did.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn naive_time_orders_by_the_table_column() {
    let Some(dsn) = dsn() else {
        return;
    };
    let schema = "mc1446_order";
    let client = connect(&dsn).await;
    {
        let _setup = SETUP.lock().await;
        exec(&client, schema, "CREATE EXTENSION IF NOT EXISTS postgis;").await;
        exec(&client, schema, NAIVE_GAP).await;
    }
    let events = engine("mc1446-events", &naive_events_config(schema, "strikes")).await;
    let window = (at("00:00"), at("02:00"));

    // EDR area: every strike, newest by the stored column first. Each
    // coverage's value is its id.
    let coverages = area(
        &events,
        None,
        "POLYGON((0 0,2 0,2 2,0 2,0 0))",
        Some(window),
        Some(&["peak_current"]),
    )
    .await
    .expect("area");
    let ids: Vec<Option<f64>> = coverages
        .iter()
        .map(|c| c.ranges["peak_current"].values[0])
        .collect();
    assert_eq!(ids, [Some(3.0), Some(2.0), Some(1.0)]);

    // GetFeatureInfo: pixel (0, 9) of a 10 × 10 map of 0.01° pixels from
    // (1°E, 1°N) holds the strikes; the frame ending 01:10Z reads the stored
    // window (02:10, 04:10]. One kept: strike 3.
    let output_crs = OutputCrs::Wgs84;
    let pick = |limit| {
        let bbox = [1.0, 1.0, 1.1, 1.1];
        let (lon, lat) = output_crs.project_node(bbox, 0.05, 0.95);
        FeaturePick {
            parameter: Some("lightning_age"),
            map: PickMap {
                output_crs: &output_crs,
                wgs84_bbox: bbox,
                width: 10,
                height: 10,
            },
            pixel: (0, 9),
            point: (lon, lat),
            time: Some(at("01:10")),
            reference_time: None,
            z: None,
            limit,
        }
    };
    let picked_ids = |limit| {
        let picked = events.pick_features(&pick(limit)).expect("pick");
        assert_eq!(picked.number_matched, 3);
        picked
            .features
            .iter()
            .map(|f| f.id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(picked_ids(1), ["3"]);
    assert_eq!(picked_ids(3), ["3", "2", "1"]);

    // The map window (the nowcast's join reads it): the newest kept is 3.
    let newest = events.recent_events(window.0, window.1, 1).expect("window");
    assert_eq!(
        newest.iter().map(|e| e.time).collect::<Vec<_>>(),
        [at("01:10")]
    );

    // Features, aliased since #503: the same strike first.
    let page = feature_page(
        &events,
        FeatureQuery {
            datetime: Some(DatetimeInterval {
                start: Some(window.0),
                end: Some(window.1),
            }),
            limit: 1,
            ..Default::default()
        },
    )
    .await
    .expect("a page");
    assert_eq!(page.features[0].id, "3@2026-03-29T01:10:00Z");

    // The plans: no statement sorts by the converted time.
    let config = events.config();
    let shape = config.events().expect("an events shape");
    let wkt = "POLYGON((0 0,2 0,2 2,0 2,0 0))";
    let keys = ["peak_current"];
    for q in [
        build_events_area(shape, &config.parameters, wkt, window, &keys, 10).unwrap(),
        build_events_pick(shape, &config.parameters, wkt, window, &keys, 10).unwrap(),
        build_events_window(shape, window, 10, WindowAttrs::Include).unwrap(),
    ] {
        assert_sorts_by_the_column(&client, &q).await;
    }

    let mut cfg = long_config(schema);
    cfg.observations.time_col = Some("time".into());
    cfg.observations.time_col_tz = Some(ZONE.into());
    let stations = engine("mc1446-stations", &cfg).await;
    let config = stations.config();
    let ids = Arc::new(vec!["a".to_string()]);
    let statements = build_location(config, "a", Some(window), &["t2m"], 10)
        .unwrap()
        .into_iter()
        .chain(build_area_set(config, &ids, Some(window), &["t2m"], 10).unwrap());
    for q in statements {
        assert_sorts_by_the_column(&client, &q).await;
    }
    exec(&client, schema, "DROP SCHEMA __S__ CASCADE;").await;
}

/// #1446: a statement the role's `statement_timeout` cancels (SQLSTATE
/// 57014) answers `DeadlineExceeded`, the 503 (EDR: 504) every engine gives
/// a read past its time budget, not the opaque 500. The view's geometry
/// sleeps a second a row; it is empty while the refresh validates it, and
/// the strike lands after.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_statement_timeout_is_the_deadline() {
    let Some(dsn) = dsn() else {
        return;
    };
    let schema = "mc1446_timeout";
    let client = connect(&dsn).await;
    {
        let _setup = SETUP.lock().await;
        exec(&client, schema, "CREATE EXTENSION IF NOT EXISTS postgis;").await;
        exec(
            &client,
            schema,
            "
            DROP SCHEMA IF EXISTS __S__ CASCADE;
            CREATE SCHEMA __S__;
            CREATE TABLE __S__.base (
                id bigint NOT NULL,
                time timestamp without time zone NOT NULL,
                geom geometry(Point, 4326) NOT NULL,
                peak_current real
            );
            CREATE FUNCTION __S__.slow(g geometry) RETURNS geometry
                LANGUAGE plpgsql VOLATILE AS $$ BEGIN PERFORM pg_sleep(1); RETURN g; END $$;
            CREATE VIEW __S__.strikes AS
                SELECT id, time, __S__.slow(geom) AS geom, peak_current FROM __S__.base;
            ",
        )
        .await;
    }
    let mut resolved =
        PostgisEngineConfig::resolve(&naive_events_config(schema, "strikes")).expect("resolve");
    resolved.dsn = dsn_option(
        &resolved.dsn,
        "options=-c%20statement_timeout%3D300",
        "options='-c statement_timeout=300'",
    );
    let pool = PoolRegistry::new()
        .get_or_create(&resolved.dsn, NonZeroU32::new(2).unwrap())
        .expect("pool");
    let engine = Arc::new(PostgisEngine::new(
        "mc1446-timeout",
        Arc::new(resolved),
        pool,
    ));
    engine
        .refresh_metadata()
        .await
        .expect("an empty view validates within the timeout");
    exec(
        &client,
        schema,
        "INSERT INTO __S__.base VALUES \
         (1, '2026-03-29 02:30:00', ST_SetSRID(ST_MakePoint(1, 1), 4326), 5);",
    )
    .await;

    let window = (at("00:00"), at("02:00"));
    let started = Instant::now();
    let error = area(
        &engine,
        None,
        "POLYGON((0 0,2 0,2 2,0 2,0 0))",
        Some(window),
        None,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, DataServerError::DeadlineExceeded),
        "{error:?}"
    );
    let error = feature_page(
        &engine,
        FeatureQuery {
            datetime: Some(DatetimeInterval {
                start: Some(window.0),
                end: Some(window.1),
            }),
            limit: 10,
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, DataServerError::DeadlineExceeded),
        "{error:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the timeout, not the sleep, ended both"
    );
    exec(&client, schema, "DROP SCHEMA __S__ CASCADE;").await;
}
