# engine-postgis

MeteoCore engine that serves observations from PostgreSQL / PostGIS (optionally
TimescaleDB) through OGC API - EDR and OGC API - Features.

## Scope (v1)

**In:**

- `Engine` trait — `get_locations`, `query_position`, `query_location`, `query_area`.
- `FeatureEngine` trait — each station is one Feature; properties come from the
  mapped `property_cols`.
- Three observation-table shapes:
  - `long` — EAV (`param_col` + `value_col`)
  - `wide` — column-per-parameter
  - `per_parameter` — one table per parameter (typical for hypertables)

**Out (deferred):**

- No `MapEngine` — no WMS, Maps, or Tiles from this engine in v1. Sparse-point
  rendering needs interpolation and vector symbolization; that is a separate
  design.
- No observations-as-time-aware Features — a Feature is a station. The time
  series is delivered via EDR coverages.
- No `events` shape (geometry + time without a station FK, e.g. lightning
  strikes). Deferred past v0.2.
- No TimescaleDB code branching — no runtime extension detection, no CAGG
  selection, no hypertable-aware SQL. TimescaleDB is supported as a deployment
  choice; the emitted SQL plans well on both.
- No user-supplied `WHERE` fragments from HTTP. The only filter extension is
  the load-time constant `stations.where` string.
- No non-PostgreSQL backends (no MySQL, DuckDB, ODBC).
- No hot-reload of column renames or DSN changes — those require a restart.

See issue [#99](https://github.com/mrauhala/meteocore/issues/99) (epic) for the
full design, phased roadmap, and devil's-advocate resolutions.

## Prerequisites

- **PostgreSQL** ≥ 13
- **PostGIS** ≥ 3.0
- **TimescaleDB** — optional. Recommended for large observation tables, but the
  engine does not branch on it.

### TLS — not implemented in v1

The engine currently connects with `NoTls`. The `sslmode` field in the DSN is
parsed and used as part of the pool key, but the connection itself is **never
actually encrypted**. Real TLS wiring is tracked in
[#110](https://github.com/mrauhala/meteocore/issues/110).

Until #110 lands, make sure the database is reachable **only** over a private
network, VPN, loopback, or an SSH tunnel. A startup `WARN` is emitted for any
non-loopback pool whose DSN does not have `sslmode=require` — treat it as a
reminder that credentials are moving in plaintext, not as proof that they
aren't.

## Role SQL

Create a dedicated read-only role and hand MeteoCore a DSN that logs in as this
role. The engine runs a startup privilege self-check and **warns** (does not
abort) if the role can `INSERT` on any mapped table, so a misconfigured
superuser DSN will still start — the warning is your signal.

```sql
-- Replace <db>, <schema>, <password> and the mapped table list as needed.

CREATE ROLE meteocore_ro LOGIN PASSWORD '<password>' NOINHERIT;

GRANT CONNECT ON DATABASE <db> TO meteocore_ro;
GRANT USAGE   ON SCHEMA  <schema> TO meteocore_ro;

-- One GRANT SELECT per mapped table. Do NOT use
-- `GRANT SELECT ON ALL TABLES IN SCHEMA ...` — it re-grants on future tables
-- that you may not want the engine to see.
GRANT SELECT ON <schema>.stations           TO meteocore_ro;
GRANT SELECT ON <schema>.obs_t2m            TO meteocore_ro;
GRANT SELECT ON <schema>.obs_rh2m           TO meteocore_ro;
GRANT SELECT ON <schema>.obs_pressure       TO meteocore_ro;
GRANT SELECT ON <schema>.obs_wind_speed     TO meteocore_ro;
GRANT SELECT ON <schema>.obs_wind_direction TO meteocore_ro;
GRANT SELECT ON <schema>.obs_precipitation  TO meteocore_ro;

-- Enforce server-side resource limits on every session that logs in as
-- this role. THESE ARE NOT OPTIONAL — the engine does NOT set
-- statement_timeout / lock_timeout / default_transaction_read_only
-- itself (session state on a fresh `deadpool-postgres` client with
-- RecyclingMethod::Fast has none applied). A superuser DSN, or a role
-- without these ALTER ROLE statements, will run queries with no cap
-- and no read-only guard — runaway SQL can saturate the pool and
-- Postgres backends.
ALTER ROLE meteocore_ro SET statement_timeout = '5s';
ALTER ROLE meteocore_ro SET default_transaction_read_only = on;
ALTER ROLE meteocore_ro SET lock_timeout = '2s';
```

Hand the engine the DSN via an environment variable — never inline in
`config.toml`:

```bash
export FMI_OBS_DSN='postgres://meteocore_ro:<password>@db.example.com:5432/weather?sslmode=verify-full'
```

Inline DSNs in TOML are rejected at config load unless `MC_ALLOW_INLINE_DB_URL=1`
is set (dev ergonomics only, warns at startup).

## Recommended indexes

The engine runs a startup index self-check against `pg_index` and logs
prescriptive `CREATE INDEX` statements if the required indexes are missing. It
does not abort — you can start an empty database and add the indexes later.

### Always (required for hot-path performance)

```sql
-- Spatial lookups (bbox, nearest-station, ST_Within).
CREATE INDEX IF NOT EXISTS stations_geom_gix
  ON stations USING GIST (geom);

-- One-station time-slice scans. Compound, with time DESC for "latest N" queries.
-- Create this on EVERY observation table (per_parameter shape = once per table).
CREATE INDEX IF NOT EXISTS obs_t2m_station_time_idx
  ON obs_t2m (station_id, time DESC);
-- ... repeat for each obs_<param> table
```

### Vanilla PostgreSQL only

Add a BRIN index on `time` when observation tables are large (≫ 10M rows) and
area queries scan wide time windows. BRIN is ~1000× smaller than btree and is a
good fit for append-only, time-ordered inserts:

```sql
-- Vanilla PG only — DO NOT add this on a TimescaleDB hypertable.
CREATE INDEX IF NOT EXISTS obs_t2m_time_brin
  ON obs_t2m USING BRIN (time) WITH (pages_per_range = 32);
```

### TimescaleDB

**Skip BRIN — per-chunk btree is already optimal.**

Why: TimescaleDB partitions a hypertable by time into chunks. Each chunk gets
its own local btree on `time`, which is already tight because the chunk covers
a narrow time interval. A BRIN index on the hypertable parent is ignored by the
planner, and a per-chunk BRIN is strictly worse than the per-chunk btree. This
is a known footgun — people who port vanilla-PG index recipes to Timescale
sometimes add BRIN "for safety" and then wonder why their scans got slower.

If you enable native compression on a hypertable, also consider:

```sql
-- Optional, only if you compress chunks. Lets the planner decompress on demand
-- during index-only plans. The engine does not assert this setting.
ALTER TABLE obs_t2m
  SET (timescaledb.enable_transparent_decompression = on);
```

## Configuration

The example below is the `per_parameter` shape verified against a real
deployment (TimescaleDB 2.13.1 + PostGIS 3.4.1, six per-parameter weather
hypertables). All observation tables in that deployment use
`timestamp without time zone` columns storing UTC, so `time_col_tz = "UTC"` is
mandatory.

```toml
[[collections]]
id = "nexus-obs"
title = "Nexus Weather Observations"
description = "Finnish surface observations — temperature, humidity, pressure, wind, precipitation"
engine_type = "postgis"
apis = ["edr", "features"]

[collections.postgis]
# DSN resolved from this env var at startup. Inline URLs rejected.
dsn_env = "NEXUS_DSN"
# Optional; defaults to max(4, min(cpu_count * 2, 16)), hard-capped at 32.
# pool_size = 8
# metadata_refresh_secs = 300

[collections.postgis.stations]
table         = "public.stations"
id_col        = "station_id"
label_col     = "name"
geom_col      = "geom"                   # geometry(Point, 4326); SRID asserted at startup
property_cols = ["country", "elevation_m", "wmo_id"]
where         = "active = true"          # config-time constant, not user input

[collections.postgis.observations]
shape            = "per_parameter"
station_fk_col   = "station_id"
time_col         = "time"
# Mandatory because the obs tables use `timestamp without time zone`.
# Forbidden if time_col is `timestamptz`. IANA TZ names also accepted.
time_col_tz      = "UTC"

# One row per parameter table. Each table must carry station_fk_col and time_col.
[[collections.postgis.observations.tables]]
parameter = "t2m"
table     = "public.obs_t2m"
value_col = "value"

[[collections.postgis.observations.tables]]
parameter = "rh2m"
table     = "public.obs_rh2m"
value_col = "value"

[[collections.postgis.observations.tables]]
parameter = "pressure"
table     = "public.obs_pressure"
value_col = "value"

[[collections.postgis.observations.tables]]
parameter = "wind_speed"
table     = "public.obs_wind_speed"
value_col = "value"

[[collections.postgis.observations.tables]]
parameter = "wind_direction"
table     = "public.obs_wind_direction"
value_col = "value"

[[collections.postgis.observations.tables]]
parameter = "precipitation"
table     = "public.obs_precipitation"
value_col = "value"

# EDR parameter metadata — one entry per advertised parameter.
# `name` here must match `parameter` in observations.tables above.
[[collections.postgis.parameters]]
name              = "t2m"
label             = "2 m air temperature"
unit              = "°C"
observed_property = "air_temperature"

[[collections.postgis.parameters]]
name              = "rh2m"
label             = "2 m relative humidity"
unit              = "%"
observed_property = "relative_humidity"

[[collections.postgis.parameters]]
name              = "pressure"
label             = "Station pressure"
unit              = "hPa"
observed_property = "air_pressure"

[[collections.postgis.parameters]]
name              = "wind_speed"
label             = "10 m wind speed"
unit              = "m/s"
observed_property = "wind_speed"

[[collections.postgis.parameters]]
name              = "wind_direction"
label             = "10 m wind direction"
unit              = "°"
observed_property = "wind_from_direction"

[[collections.postgis.parameters]]
name              = "precipitation"
label             = "Hourly precipitation"
unit              = "mm"
observed_property = "precipitation_amount"
```

The other two shapes swap the `[collections.postgis.observations]` block:

- **long / EAV:** set `shape = "long"`, drop `[[observations.tables]]`, add
  `param_col` + `value_col` on `[observations]`, and list each `source_key`
  under `[[parameters]]`.
- **wide:** set `shape = "wide"`, drop `[[observations.tables]]`, and add
  `[[observations.columns]]` mapping `parameter → column`.

If none of the three shapes fit your schema, expose a Postgres `VIEW` that does.
The DSL deliberately does not support joins, computed expressions, or column
transforms.

## Events shape (non-station event data)

The fourth shape, `events` (#113), serves tables where each row is an
independent event with its own time and point geometry — no stations, no
interval. Reference dataset: a lightning-strike table
`(id, time, the_geom, multiplicity, peak_current, cloud_indicator, ellipse_major)`.

```toml
# collections.d/lightning.toml
id = "lightning"
title = "Lightning strikes"
engine_type = "postgis"
apis = ["edr"]

[postgis]
dsn_env = "MC_OBS_DSN"

[postgis.observations]
shape = "events"
table = "public.lightning"
time_col = "time"
time_col_tz = "UTC"           # mandatory for `timestamp without time zone`
geom_col = "the_geom"
id_col = "id"                 # ORDER BY time DESC, id tiebreak
default_datetime = "PT1H"     # window when the query has no datetime (default PT1H)
extent_bbox = [4.0, 54.0, 42.0, 72.0]   # REQUIRED: the only spatial-extent source; never ST_Extent

[[postgis.parameters]]
name = "peak_current"         # source_key defaults to name = the column name
label = "Peak current"
unit = "kA"

[[postgis.parameters]]
name = "multiplicity"
label = "Multiplicity"
unit = "1"
```

Behaviour:

- **EDR `area` only.** One SQL statement per request (time-range + polygon
  intersect, `ORDER BY time DESC, id`), no fan-out — the fan-out semaphore is
  not involved. `position`/`locations` return 400 pointing at the area query.
- **Response**: a CoverageJSON `CoverageCollection` of `Point` coverages — one
  per event, each with its own single-value `t` axis and 0-d scalar ranges.
  An empty window is a valid empty collection, not a 404.
- **No `datetime`** never means full history: the `default_datetime` window
  (ending "now") applies.
- The response-value budget charges `rows × selected parameters`; a breach is
  an HTTP 400 naming the numbers.
- Parameter columns are cast `::double precision` in SQL, so `smallint` /
  `numeric(p,s)` columns work without config.
- A `[postgis.stations]` block, `station_fk_col`, `locations_window`, or
  `columns`/`tables` entries are rejected for this shape at config load.
- Recommended index: `btree (time DESC, geom) INCLUDE (<parameter columns>)`
  plus the usual gist on the geometry column.

## Optional stations / orphan locations

The `[collections.postgis.stations]` block is **optional**. When the observation
tables carry their own point geometry (`observations.geom_col`, e.g. `the_geom`),
locations can be derived directly from the observations — useful when the
observed station set is wider than (or entirely absent from) a curated stations
registry. The mode is derived from what you configure, with no extra flag:

| `[postgis.stations]` | `observations.geom_col` | Mode | Behavior |
|---|---|---|---|
| present | absent | stations-only | Original behavior. The **whole registry** is advertised (labels + properties), with or without data. |
| present | present | **orphan fallback** | **Membership = stations reporting within the window** (same set as observations-only). Registered ones get their label/properties + authoritative geometry; reporters with no `stations` row are bare orphans (`label = id`, no properties). A **registered-but-silent station is not advertised** — use stations-only mode to advertise the full registry. |
| absent | present | **observations-only** | Every location derived from the obs geometry (windowed reporters). `label = id`, no properties. |
| absent | absent | — | Hard config error (nothing can be placed on a map). |

**Mode A — no stations table** (everything from the observations geometry):

```toml
[collections.postgis]
dsn_env = "OBS_DSN"

[collections.postgis.observations]
shape          = "per_parameter"
station_fk_col = "wigos_id"
time_col       = "time"
time_col_tz    = "UTC"
value_col      = "value"
geom_col       = "the_geom"     # geometry(Point, 4326) on every obs table

[[collections.postgis.observations.tables]]
parameter = "air_temperature"
table     = "public.airtemperature"
# ... more tables ...

[[collections.postgis.parameters]]
name  = "air_temperature"
label = "2 m air temperature"
unit  = "degC"
```

**Mode B — stations table + orphan fallback** (add `geom_col` to an existing
stations config; registered stations keep their `name`/properties, the rest fall
back to the obs geometry):

```toml
[collections.postgis.stations]
table      = "public.stations"
id_col     = "wigos_id"
label_col  = "name"
geom_col   = "the_geom"
property_cols = ["territory"]

[collections.postgis.observations]
shape          = "per_parameter"
station_fk_col = "wigos_id"
time_col       = "time"
time_col_tz    = "UTC"
value_col      = "value"
geom_col       = "the_geom"          # <-- enables orphan fallback
locations_window = "PT24H"           # optional; default 24h. "all" = full history
# ... tables / parameters as above ...
```

**How it performs.** The observation-derived location list is built with one
`SELECT DISTINCT ON (station_fk)` **per observation table** (deduped by id in
Rust — NOT a single `UNION`, which would run as one statement and blow a
read-only role's `statement_timeout`), each capped at `MAX_LOCATIONS` (50 001).
It runs only in the **background metadata refresh** (`metadata_refresh_secs`),
never on a request; the per-request observation fetch (`WHERE station_fk = $1`)
is identical in every mode, so request latency is unchanged.

Even one table's *full-history* `DISTINCT ON` can exceed a tight
`statement_timeout` on a large hypertable (the largest nexus table, ~13 M rows,
took >5 s). So the derivation is **time-windowed by default**:

- `observations.locations_window` — ISO 8601 duration (**default `"PT24H"`**),
  adds `AND time_col >= now() - window` so the scan only touches recent
  hypertable chunks (~0.1 s) and advertises only **currently-reporting**
  stations. This is the right default for live observations.
- `locations_window = "all"` — full history (a climate-style collection). Needs a
  role `statement_timeout` large enough to scan the whole table (the limits come
  from the role, not the engine), or a pre-materialized distinct-`(id, geom)`
  table wired as the `stations` block.

In modes A/B, `position` (nearest, by haversine) and `area` are answered
**in-memory** from the cached location set — an `area` polygon returns its
**bounding-box superset**, not exact `ST_Within` (documented v1 simplification);
stations-only mode keeps the live `ST_DWithin`/`ST_Within` SQL path.

## Startup behavior: error vs. warning

The engine distinguishes between conditions that make the collection
unusable (hard error — the whole config load fails) and conditions that are
suspicious but workable (WARN — the collection still loads).

### Hard errors (config load or collection init fails)

- `engine_type = "postgis"` but no `[postgis]` block present.
- `[postgis]` contains a literal `postgres://…` URL and `MC_ALLOW_INLINE_DB_URL`
  is not `1`.
- `dsn_env` names an env var that does not exist at startup or during reload.
- Any schema, table, or column identifier fails the regex
  `^[A-Za-z_][A-Za-z0-9_]{0,62}$`.
- `pool_size > 32`.
- Two collections share a pool tuple `(host, port, db, user, sslmode)` but
  specify different passwords.
- `stations.geom_col` is not `geometry(Point, 4326)` (SRID or geometry type
  mismatch is checked against `geometry_columns`).
- `time_col` is `timestamp without time zone` but `time_col_tz` is absent.
- `time_col` is `timestamptz` but `time_col_tz` is set.
- A `property_cols` entry has an unsupported type (only `text`, `varchar`,
  integer types, `real`, `double precision`, and `bool` are accepted).
- The initial `SELECT 1` ping fails past the 2 s deadline at boot.

### Warnings (collection loads, condition logged)

- Role has `INSERT`, `UPDATE`, or `DELETE` privilege on any mapped table
  (startup `has_table_privilege` check).
- Role can `SELECT` a column on a mapped table that is not in the mapping
  (`has_column_privilege` check) — suggests the DSN is broader than needed.
- `GIST(geom)` on `stations` is missing.
- Compound `btree (station_fk_col, time_col DESC)` on an observation table is
  missing.
- `MC_ALLOW_INLINE_DB_URL=1` is set (inline DSNs are a dev-only escape hatch).
- A non-loopback DSN has `sslmode` other than `require` — a reminder that
  credentials are moving in plaintext (TLS enforcement tracked in #110).
- Two collections on the same pool tuple specify different `pool_size` —
  **first-caller wins**, subsequent size mismatches logged at INFO.

## Operations

- **Health:** `/health` reports per-collection `ready | degraded | failed`.
  A 30 s background `SELECT 1` ping (2 s deadline) flips a loaded collection
  between `ready` and `degraded` on DB reachability — live, not boot-time-only.
  `failed` means the collection never finished loading (config/privilege). A
  metadata-refresh failure does NOT degrade health (the ping is the authority;
  the failure shows up in metrics + logs).
- **Metrics:** Prometheus series under the `postgis_*` prefix, scraped live —
  gauges `postgis_up{collection}` (1/0), `postgis_pool_{size,max_size,available,waiting}{pool_key}`
  (`size` = open connections, `max_size` = capacity, `available` = acquirable now),
  `postgis_metadata_refresh_seconds{collection}` (last refresh duration); and
  counters `postgis_{metadata_refreshes,metadata_refresh_failures,pings,ping_failures}_total{collection}`
  (process-global, delta-tracked so `rate()` works across reloads). Labels are
  bounded (collection / pool_key are config-time).
  Per-query histograms (`postgis_query_duration_seconds` / `rows_returned` /
  `query_errors_total`) are a follow-up — they need recording at the API layer,
  which calls engines generically through the trait.
- **Reload:** `POST /admin/collections/reload` re-reads the config and swaps
  engines atomically via `ArcSwap`. Shared pools carry over by identity across
  reloads; dropped URLs' pools close naturally.
- **Row cap:** every emitted SELECT carries `LIMIT 10001` (per-query,
  non-configurable). Location/station listings use `LIMIT 1001`. Hitting the
  ceiling returns HTTP 413 with a message asking the caller to narrow `bbox`
  or `datetime` — it is a protection invariant, not a policy knob.

## References

- Epic: [#99](https://github.com/mrauhala/meteocore/issues/99)
- This README: [#112](https://github.com/mrauhala/meteocore/issues/112)
- Workspace guide: `/CLAUDE.md` (architecture rules, config format, admin
  endpoints)
