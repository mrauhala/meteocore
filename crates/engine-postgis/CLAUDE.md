# engine-postgis crate — Claude Instructions

PostGIS/TimescaleDB observation engine (`EdrEngine` + `FeatureEngine`).
Read the root `CLAUDE.md` first — Critical Rule 8 (SQL safety) is enforced
in CI by `scripts/check_sql_safety.sh`: keep `SELECT` keywords in plain
`String` literals, never inside `format!`; no string concatenation onto SQL
literals; no `push_str(variable)`.

## Prerequisites & schema shapes

- PostgreSQL ≥ 13 + PostGIS ≥ 3.0. TimescaleDB is a supported *deployment*
  choice (hypertables plan well), but the engine never branches on it.
- Three schema shapes via `observations.shape`: `long` (EAV), `wide`
  (column-per-parameter), `per_parameter` (table-per-parameter; one fan-out
  query per param).

## Location modes (#433/#439 — derived, not flagged)

The `[postgis.stations]` table is optional; locations can be derived from
the observations table's own geometry (`observations.geom_col`;
per_parameter inherits or overrides per table):

- stations present, no obs geom → **stations-only** (original behavior):
  the whole registry is advertised, data or not. Live
  `ST_DWithin`/`ST_Within` SQL for position/area.
- stations present + obs geom → **orphan fallback (mode B)**: membership =
  the windowed observation reporters; the stations table supplies only
  metadata (label/properties/authoritative geometry) for the registered
  subset. Reporters without a stations row are bare orphans (`label = id`).
  **A registered-but-silent station is NOT advertised** — use stations-only
  mode to advertise the full registry.
- no stations + obs geom → **observations-only (mode A)**: every location
  derived from the obs table (windowed reporters, bare).
- no stations + no obs geom → hard config error.

Derivation rules (hard-won from production timeouts):

- Orphan/derived locations use `SELECT DISTINCT ON (station_fk)`; for
  `per_parameter`, **one query per table, NOT a single `UNION`** — a UNION
  of N multi-second scans runs as one statement and blows a read-only
  role's `statement_timeout` (#435). Deduped by id in Rust
  (`DerivedLocations`); each query fetches `max_locations + 1` rows and the
  merged list stops at `max_locations` (below).
- The derivation is **time-windowed by default**
  (`observations.locations_window`, ISO 8601 duration, default 24 h;
  `"all"` = full history) — restricts the scan to recent hypertable chunks
  (~0.1 s vs >5 s full-history, #438) and advertises only
  currently-reporting stations. A climate-style collection sets
  `locations_window = "all"` and needs a role `statement_timeout` big
  enough (or a pre-materialized locations table).
- In modes A/B, position = nearest-by-haversine and area = exact
  point-in-polygon (#671), both **in-memory** from the cached location set.
  The per-request observation fetch (`WHERE station_fk = $1`, or
  `= ANY($1)` on the set-based area path, #1062) is identical in every
  mode.
- Scaling for very large tables: pre-materialize a distinct `(id, geom)`
  view and wire it as the `stations` block, and/or raise
  `metadata_refresh_secs`.

## Security & connection rules

- **DSN via env var only:** `[postgis].dsn_env` names an env var; a literal
  `postgres://` URL in TOML is rejected unless `MC_ALLOW_INLINE_DB_URL=1`.
- **TLS is deferred (#110 follow-up):** v1 passes `NoTls`; `sslmode=` is
  parsed but not applied; a startup WARN fires when a non-loopback DSN
  lacks `sslmode=require`. Reach the DB over private network/VPN/loopback.
- Every identifier: `ds_core::config::is_valid_sql_identifier` at load +
  `security::quote_ident` at emit. Every value: `$N` bind.
  `stations.where_clause` is config-time only, validated against a
  blocklist (DML/DDL verbs, `UNION`/`EXECUTE`/`CALL`/`PERFORM`, `;`,
  comments) — for richer filtering, create a SQL VIEW.
- **Session limits come from the role, not the engine:**
  `statement_timeout`, `lock_timeout`, `default_transaction_read_only` are
  set via `ALTER ROLE meteocore_ro SET ...` (crate README). The engine uses
  `RecyclingMethod::Fast` and issues no `SET` on checkout, so **a superuser
  DSN or an unconfigured role bypasses those limits entirely** — the
  role-setup SQL is operationally mandatory, not optional.
- **Per-URL pool** shared across collections on the same
  `(host, port, db, user, sslmode)` tuple; first-caller-wins on size;
  `HARD_POOL_CAP = 32`. Across incremental reloads (#574) a reused engine
  keeps its pool, and `PoolRegistry::seed` pre-registers those pools into
  each load so rebuilt collections on the same DSN join them — DSN sharing
  is a standing invariant, not load-time-only.

## Station-list cap (`[postgis] max_locations`, #1061)

- One cap for every station shape: default
  `ds_core::config::POSTGIS_DEFAULT_MAX_LOCATIONS` (200 000), validated at
  load to `1..=POSTGIS_MAX_LOCATIONS_CEILING` (1 000 000); rejected on the
  events shape. Never a silent cut: every location query fetches
  `max_locations + 1` rows (`query::location_fetch_rows`), and one past the cap
  keeps the first `max_locations` in query order and sets
  `CollectionMeta::locations_cut` (`LocationSet::of_station_rows` for the
  stations table, `DerivedLocations::merge_table` for each per-table
  observation query). Exactly `max_locations` is complete. Mode B
  (`orphan_fallback`): a cut reporter list is `LocationsCut::Stations`; a cut
  registry is `LocationsCut::Registry` only when some reporter found no
  registry row, else nothing served is missing and nothing is flagged.
  `fetch_*_rows` call those helpers and the tests call the same ones: keep
  the cap logic in them, not inline in the async fetch.
- The `LIMIT` is an integer literal from the validated `usize`, NOT a `$N`
  bind: Postgres plans a bound LIMIT in a generic plan as 10 % of the rows,
  and after five executions of these per-refresh prepared statements that
  could switch the `DISTINCT ON` scan to a plan chosen for a tenth of its
  rows — the statement-timeout failure class of #435/#438. Keep it a literal.
- Truncation is visible: `PostgisEngine::live_health()` folds it into
  `LiveHealth::LocationsTruncated` (degraded, `LocationsCut::reason` names
  the cap and which read; an unreachable DB wins), the
  `postgis_locations_truncated{collection}` gauge, a Grafana stat, and a
  WARN on the refresh that first cuts the list. Every `/health` string comes
  from `LiveHealth::degraded_reason`.
- A cut list is still served, so it is **live-only**: the load/reload
  snapshot keeps it `ready` (`admin::postgis_load_status`, degraded only for
  an unreachable DB) and `effective_health` overlays the cut. Recording it in
  the snapshot made a later outage show the cut instead (an `Unreachable`
  verdict keeps a recorded error) and would fail reload's zero-`ready` guard
  when every collection is cut.
- Sizing is measured, not guessed: `metadata::tests::footprint` counts the
  snapshot's heap (≈ 613 B a station with three properties; the README has
  the table and the /locations, Features, refresh and mask-budget costs). If
  a new snapshot field grows per station, rerun it with `--nocapture` and
  update the README numbers; the test fails past 1 KiB a station.

## Metadata cache & health (#110, #441, #445)

- `ArcSwap<CollectionMeta>` holds stations, parameter descriptors, temporal
  extent, spatial bbox. Synchronous bootstrap at construction, then
  `poll_loop` refreshes every `metadata_refresh_secs` (default 300) on the
  background runtime (spawned at boot AND on reload; `shutdown()` on
  reload). A failed refresh WARNs and keeps the previous snapshot — never
  empties the cache, and does NOT flip health.
- **Features `filterables` = the configured `stations.property_cols` ∪ the
  names the cached rows carry** (#1024). `MetadataCache::new(cfg)` seeds them
  at construction, so an empty stations table or a failed first refresh
  (database unreachable at boot) still accepts `territory=FI`: 200 with zero
  features, never a 400 that lasts until the first row loads. Every stations
  row carries every property column, so the union adds nothing once data is
  in. The observations-only mode and the events shape have no stations table
  and seed nothing; in mode B the orphans carry no properties, so a filter
  matches registered stations only. Snapshot assembly is
  `CollectionMeta::build`, pure and unit-tested without a database; keep new
  per-refresh derivations there, not in `refresh`.
- **Live ping:** `poll_loop` runs `SELECT 1` every 30 s (2 s deadline) on a
  **dedicated** connection (not the shared pool — a busy pool must not
  masquerade as DB-unreachable), flipping the collection ready⇄degraded.
  `engine.live_health()` is the `/health` authority: the ping's verdict plus
  a truncated station list (above); the handler overrides the boot snapshot
  with it for postgis collections (`admin::apply_postgis_live_health`), while
  a `failed` collection (couldn't construct) has no engine and keeps its boot
  status.
- Metrics: `postgis_up{collection}`, `postgis_locations_truncated{collection}`,
  `postgis_pool_*{pool_key}` gauges, `postgis_metadata_refresh_seconds`, and
  `postgis_{metadata_refreshes,metadata_refresh_failures,pings,
  ping_failures}_total` counters (process-global with
  rebaseline-on-reset delta tracking, since engines reset on reload — keeps
  `rate()` working). Per-query histograms are deferred (#444).

## Data-shape rules

- **Response budget model (#498):** the gate on query size is
  `MAX_RESPONSE_VALUES` (500k values ≈ stations × parameters × timesteps),
  charged as rows arrive — NOT a flat station cap. Its purpose: "many
  stations × one timestep" and "one station × a year of 10-min data" both
  fit; "everything × everything" is a 400 whose message names the numbers
  and the dimension to narrow. Every breached cap is
  `DataServerError::QueryTooLarge` → HTTP 400, never `Engine` (the API
  layer hides that behind an opaque 500 — that misread a too-large polygon
  as a server fault in production).
- Supporting caps: stations-in-polygon `MAX_STATIONS_IN_POLYGON` =
  200 001, one past a 200 000-station ceiling (#1062 raised it from 10 001,
  which made "every station at 06 UTC" a 400 on a 20 000-station network).
  It is a memory guard, well above any network: the value budget does not
  bound per-station memory, because one value a station is still a
  coverage a station. A request holds about 300 bytes a station before any
  row (≈ 60 MB at the ceiling) and then a coverage per station with data,
  of the order of a kilobyte each before serialisation. Fan-out
  `MAX_AREA_QUERIES` 20k statements (stations × params for `per_parameter`),
  **fan-out path only**; locations `LIMIT max_locations + 1` (above);
  nearest-station `LIMIT 1`.
- **Two area paths (#1062), one result** but for the two edge cases
  below. `query_area` picks with
  `area_path`: **set-based** for a `datetime` window of at most
  `SET_BASED_MAX_WINDOW` (provisional 1 h: an instant, each instant of a
  `datetime` list, which the API queries one by one) and for a longer
  window the fan-out could not run (over `MAX_AREA_QUERIES`, so the value
  budget decides); the **fan-out** otherwise and without `datetime`. The
  threshold is a window, not stations × steps: both paths pay per station,
  so what decides is the steps per station, and the engine does not know a
  collection's cadence. Tune it from the timing table `tests/db.rs` prints.
  - **Risk of the long-window routing:** a long window over more stations
    than the fan-out may take used to be the statement-count 400 before
    any SQL ran. Set-based, it is one `ORDER BY <fk>, <time> LIMIT
    budget plus one` statement, and its 400 is cheap only when the planner
    stops at the LIMIT: an ordered scan of the `(<fk>, <time> DESC)` index
    (`= ANY` on its leading column) under an incremental sort. Without that
    index, or with a bitmap scan and a full sort, it reads every row in the
    window and can reach the role's `statement_timeout`, an opaque 500. The
    timing test prints each path's time to that 400 for a week over 20 000
    stations, and the statement's `EXPLAIN ANALYZE`.
  - Set-based (`build_area_set`, `run_area_set`): one statement per
    observation table (`long`/`wide` one, `per_parameter` one per
    parameter) with `<fk> = ANY($1)` bound to the station ids (one shared
    `SqlParam::SharedTextArray`), rows led by `<fk>::text AS mc_station`,
    `ORDER BY <fk>, <time>`, `LIMIT $N` rewritten to the remaining budget
    plus one before each statement. Streamed (`query_raw`) and charged per
    row; the crossing row is the 400. One connection and one
    `fanout_limiter` permit, so the pool headroom holds across both paths.
  - The polygon test is unchanged: SQL `ST_Within` in stations-only mode,
    the cached set in modes A/B; both paths take the same station list,
    each id once (`distinct_stations`: a stations view without a unique id
    would otherwise make the fan-out answer a station twice).
  - Where the paths differ (documented on `run_area_set`): set-based has no
    per-station row cap (the budget bounds the whole response), so a
    station over `MAX_OBSERVATION_ROWS` in the window is served there and
    is the fan-out's 400; and set-based matches rows to stations by
    `<fk>::text`, so a `citext`-like key whose text spells an id
    differently from the request drops those rows with a WARN, where the
    fan-out keeps them.
  - **One decoder for every path** (`RowDecoder`): the single-station,
    fan-out and set-based statements select the same columns in the same
    order (`StationSelect` in `query.rs`), set-based rows offset by one
    (`first_observation_column`), decoded by position into `StationRows`,
    coverages assembled in station-list order. Keep it that way: the DB
    tests pin both paths to identical values in identical order.
- Observation `LIMIT` is a **bind** (`LIMIT $N`, `SqlParam::Int`,
  `BuiltQuery::{row_limit,set_row_limit}`), not a literal: single-station
  paths (position/location) rewrite it to the remaining budget per query
  (long series work); area fan-out queries pin it to
  `MAX_OBSERVATION_ROWS` 10_001 per station×parameter so the concurrent
  transient buffer stays ≤ width × cap. `values_per_row` weighs `wide`
  rows (one row = one value per selected column) against the budget.
- **Area fan-out is concurrent** (#115): `stream::iter().buffered(n)`,
  `n = clamp(pool max_size − 2, 1, 16)`, AND a **process-global
  per-pool-key semaphore** (`fanout_limiter`) holds that width across ALL
  concurrent area requests — the pool is shared per-DSN across
  collections, and per-request widths alone let two simultaneous fan-outs
  jointly drain it. Permit acquired before the pooled connection, always
  in that order (no circular wait). One pooled connection per in-flight
  station, station order preserved, first error cancels the rest. Never
  make it unbounded and never re-serialize it — 8k stations × ~2.4 ms
  sequential was ~20 s. A 200 can never exceed the value budget; transient
  pre-400 DB work may overshoot by ≤ width × `MAX_OBSERVATION_ROWS` rows
  (documented on `run_area_fanout`).
- **Time-zone columns:** `time_col_tz` is required when `time_col` is
  `timestamp without time zone`. The WHERE clause wraps the BIND
  (`$N AT TIME ZONE '<tz>'`) so the column index stays usable; the SELECT
  list wraps the COLUMN to emit `timestamptz`.
- No data in window → `LocationNotFound` → 404 (an empty PointSeries fails
  CoverageJSON validation).
- Supported `pg_type`s for `property_cols`: `bool`, `int2/4/8`, `float4/8`,
  `text`/`varchar`/`bpchar`/`name`, NULL. Others (arrays, json, enums,
  numeric, timestamp-typed properties) are rejected at refresh time.
- Do not `map_err(|_| …)` away the underlying Postgres error — surfacing
  the real error is tracked in #436; swallowing it hid a production
  misconfiguration.

- Event-source detection coverage (#621) is `observations.coverage_bbox`,
  optional and independently validated. `EventSource::covers` checks full
  containment; absent config returns unknown. Never substitute `extent_bbox`
  or observed strike positions: neither guarantees detection coverage.

## Database tests (`tests/db.rs`, #1062)

- The only tests that run SQL. They read `MC_TEST_POSTGIS_DSN` (a
  superuser: they create the `postgis`/`timescaledb` extensions and their
  own `mc1062_*` schemas, dropped first) and skip with a printed line
  without it, unless `MC_TEST_POSTGIS_REQUIRED=1`, which fails instead.
  CI's `.github/workflows/postgis-db.yml` runs them against a
  `timescale/timescaledb-ha:pg17` service container with both set, on PRs
  touching engine-postgis, its path dependencies (ds-core, ds-poll,
  ds-cache), api-edr, `Cargo.toml`, `Cargo.lock` or the workflow, and on
  every main push. `cargo test` elsewhere skips them.
- They go through the public load path (`PostgisEngineConfig::resolve`,
  `PoolRegistry`, `PostgisEngine::new`, `refresh_metadata`) and the area
  query (`query_area_on` forces a path), one shape per test: `long`
  stations-only, `wide` mode B with a naive-UTC column, `per_parameter`
  mode A. 20 000 synthetic stations; nulls, missing rows and stations
  without data in a window are deliberate.
- The timing test prints both paths by stations × steps on a TimescaleDB
  hypertable, each path's time to the value-budget 400 for a week over
  20 000 stations, and that set-based statement's `EXPLAIN ANALYZE`. It
  never fails on a number; without TimescaleDB it prints that it skipped.
  Run with `-- --nocapture --test-threads=1`.
