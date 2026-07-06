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
  role's `statement_timeout` (#435). Deduped by id in Rust; each capped at
  `MAX_LOCATIONS`.
- The derivation is **time-windowed by default**
  (`observations.locations_window`, ISO 8601 duration, default 24 h;
  `"all"` = full history) — restricts the scan to recent hypertable chunks
  (~0.1 s vs >5 s full-history, #438) and advertises only
  currently-reporting stations. A climate-style collection sets
  `locations_window = "all"` and needs a role `statement_timeout` big
  enough (or a pre-materialized locations table).
- In modes A/B, position = nearest-by-haversine and area = bbox test,
  both **in-memory** from the cached location set (polygon area returns its
  bbox superset, not exact `ST_Within`). The per-request observation fetch
  (`WHERE station_fk = $1`) is identical in every mode.
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
  `HARD_POOL_CAP = 32`. Per-load only (no reuse across reloads in v1).

## Metadata cache & health (#110, #441, #445)

- `ArcSwap<CollectionMeta>` holds stations, parameter descriptors, temporal
  extent, spatial bbox. Synchronous bootstrap at construction, then
  `poll_loop` refreshes every `metadata_refresh_secs` (default 300) on the
  background runtime (spawned at boot AND on reload; `shutdown()` on
  reload). A failed refresh WARNs and keeps the previous snapshot — never
  empties the cache, and does NOT flip health.
- **Live ping:** `poll_loop` runs `SELECT 1` every 30 s (2 s deadline) on a
  **dedicated** connection (not the shared pool — a busy pool must not
  masquerade as DB-unreachable), flipping the collection ready⇄degraded.
  The ping is the `/health` authority: the handler overrides the boot
  snapshot with `engine.health_status()` for postgis collections, while a
  `failed` collection (couldn't construct) has no engine and keeps its boot
  status.
- Metrics: `postgis_up{collection}`, `postgis_pool_*{pool_key}` gauges,
  `postgis_metadata_refresh_seconds`, and
  `postgis_{metadata_refreshes,metadata_refresh_failures,pings,
  ping_failures}_total` counters (process-global with
  rebaseline-on-reset delta tracking, since engines reset on reload — keeps
  `rate()` working). Per-query histograms are deferred (#444).

## Data-shape rules

- Row caps (non-configurable invariants): locations `LIMIT 50_001`,
  per-observation-query `LIMIT 10_001`, stations-in-polygon prefilter
  `LIMIT 501`, nearest-station `LIMIT 1`.
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
