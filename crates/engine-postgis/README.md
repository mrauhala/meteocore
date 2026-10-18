# engine-postgis

Serves observations stored in PostgreSQL/PostGIS through MeteoCore's EDR,
Features, Tiles, WMS and Maps APIs.

## What it solves

Weather stations and lightning detectors often write straight into a
database. This engine publishes those tables as OGC collections without an
export pipeline, an ingest step or a MeteoCore-specific schema: TOML maps
existing tables and columns to collections and parameters, and the engine
reads them in place.

## How it works

- A background loop refreshes a metadata snapshot (locations, extents,
  station properties) every `metadata_refresh_secs`. Location lists,
  Features and extents come from it; values and events are read per
  request.
- Identifiers are validated at load and quoted; values are bind parameters.
- Before serving data, each refresh checks the mapped time and geometry
  columns in the database. Advisory privilege and index findings go to the
  log.
- Collections on the same `(host, port, database, user, sslmode)` share one
  connection pool.

| `observations.shape` | Layout | EDR | Features | Tiles | WMS, Maps |
|---|---|---|---|---|---|
| `long` | Row per value: station, time, parameter, value | `locations`, `position`, `area`, `radius` | Station inventory | Vector (MVT) | – |
| `wide` | Row per station and time, column per parameter | same | same | same | – |
| `per_parameter` | Table per parameter: station, time, value | same | same | same | – |
| `events` | Point events with own time and geometry (lightning) | `area`, `radius` | – | Raster | Age layer `lightning_age`, unit `min` |

## Configuration

### Database role and indexes

Use a dedicated read-only login. The engine sets no session limits itself:

```sql
CREATE ROLE meteocore_ro LOGIN PASSWORD '<password>' NOINHERIT;
GRANT CONNECT ON DATABASE weather TO meteocore_ro;
GRANT USAGE ON SCHEMA public TO meteocore_ro;
GRANT SELECT ON public.stations, public.observations TO meteocore_ro;
ALTER ROLE meteocore_ro SET default_transaction_read_only = on;
ALTER ROLE meteocore_ro SET statement_timeout = '5s';
ALTER ROLE meteocore_ro SET lock_timeout = '2s';

CREATE INDEX ON public.stations USING GIST (geom);
CREATE INDEX ON public.stations USING GIST ((geom::geography));
CREATE INDEX ON public.observations (station_id, time DESC);
```

Index every observation table on `(station, time)`, and an event table on
time and (GiST) geometry. The two registry indexes serve the registry-only
area (`geom`) and nearest-station (`geom::geography`) queries. A catalog
check logs a warning for each missing index.

Put the DSN in an environment variable
(`export OBS_DSN='postgres://meteocore_ro:…@127.0.0.1:5432/weather'`); a
literal URL in `dsn_env` is rejected unless `MC_ALLOW_INLINE_DB_URL=1`.

### Station collection

A file under `collections_dir` (inline: `[[collections]]` and
`[collections.postgis…]`). No shipped example under `collections.d/` uses
this engine.

```toml
id = "surface-obs"
title = "Surface observations"       # required, like description
description = "Hourly station observations"
engine_type = "postgis"
apis = ["edr", "features", "tiles"]

[postgis]
dsn_env = "OBS_DSN"

[postgis.stations]           # optional if observations.geom_col is set
table = "public.stations"    # schema.table, or a bare name in public
id_col = "station_id"
label_col = "name"
geom_col = "geom"
property_cols = ["country"]  # Features properties and filters
# where = "active = true"    # fixed filter: no ';', comments, subqueries

[postgis.observations]
shape = "long"
table = "public.observations"
station_fk_col = "station_id"
time_col = "time"
param_col = "parameter"
value_col = "value"

[[postgis.parameters]]       # at least one
name = "t2m"                 # what clients request
source_key = "temperature"   # value in param_col; default: name
label = "2 m air temperature"
unit = "degC"                # a label; no conversion
observed_property = "air_temperature"   # default: name
```

| `[postgis.observations]` field | Shapes | Meaning |
|---|---|---|
| `table` | `long`, `wide`, `events` | Required; `per_parameter` lists its tables under `tables` |
| `station_fk_col`, `time_col` | station shapes | Required; for `per_parameter`, defaults each table may override |
| `param_col` | `long` | Column holding the parameter's `source_key` |
| `value_col` | `long`, `per_parameter` | Value column (a default for `per_parameter` tables) |
| `columns` | `wide` | `[{ parameter = "<source_key>", column = "<col>" }]` |
| `tables` | `per_parameter` | `[{ parameter, table }]`; each may override `station_fk_col`, `time_col`, `time_col_tz`, `value_col`, `geom_col` |
| `time_col_tz` | all | Zone of a `timestamp without time zone` column, e.g. `"UTC"` |
| `geom_col` | all | Observation point geometry; required for `events` |
| `locations_window` | station shapes | Window for observation-derived locations: an ISO 8601 duration (default `"PT24H"`) or `"all"` |
| `id_col`, `extent_bbox` | `events` | Required: tiebreak column; advertised `[west, south, east, north]` |
| `default_datetime` | `events` | EDR window without `datetime`, and the map window; default `"PT1H"` |

Locations come from the registry, from `observations.geom_col`, or both. A
registry alone advertises every station, with or without data. Observation
geometry advertises the stations reporting within `locations_window`, with
the ID as label; with a registry too, registered reporters take its label,
properties and geometry, and `where` filters only that metadata. For
`per_parameter`, observation geometry counts only when every table has one.

### Column requirements

- Geometry is a Point with SRID 4326 (X = longitude); prefer
  `geometry(Point, 4326)`.
- `timestamptz` columns must not set `time_col_tz`; `timestamp without time
  zone` columns must. PostgreSQL validates the zone name.
- Values are cast to `double precision`; NULL stays missing.
- Registry rows need a non-NULL id, a non-NULL text label and a non-empty
  geometry; filter others out with `where` or a view.
- Station properties may be boolean, integer, real, double precision or text.
- Identifiers match `^[A-Za-z_][A-Za-z0-9_]{0,62}$`. Use a view for joins,
  casts or reprojection.

Each refresh first checks the time and geometry columns in the catalog
(five-second deadline). A geometry column not declared `Point, 4326` is
sampled (128 rows per source), and every geometry a query returns is checked
before use. A mismatch blocks data queries and degrades `/health` until a
refresh succeeds. A registry row or property column that breaks the rules
above fails the refresh, which keeps the previous snapshot.

### Event collection

Top-level fields and `[postgis]` as above, with
`apis = ["edr", "wms", "maps", "tiles"]` and no `[postgis.stations]`:

```toml
[postgis.observations]
shape = "events"
table = "public.lightning"
id_col = "id"
time_col = "time"
time_col_tz = "UTC"          # a timestamp without time zone column
geom_col = "geom"
extent_bbox = [4.0, 54.0, 42.0, 72.0]

[[postgis.parameters]]
name = "peak_current"        # source_key is the column name
label = "Peak current"
unit = "kA"
```

Style it with `[wms] colormap = "lightning_age"`, whose stops span 0–60
minutes, the default window. A map frame draws the events of the
`default_datetime` window ending at `TIME` (floored to the minute) as discs
of 2-pixel radius, valued by age in minutes. Advertised times are one-minute
steps over the newest six hours of data; without `TIME` the frame follows
the newest step, updated on refresh. Each window is one read, cached
(`MC_LIGHTNING_STRIKE_CACHE_MB`, default 32) and capped at the newest
200 000 events.

A nowcast `lightning_source` reads the same table. Optional
`cloud_indicator_col` (0 cloud-to-ground, 1 intra-cloud), `peak_current_col`
(signed kA) and `coverage_bbox` (where detection is guaranteed) feed it.

## Queries and budgets

- `position` picks the nearest station within 25 km. An instant `datetime`
  matches that timestamp exactly.
- `radius` runs as an area query over a circle polygon.
- Without `datetime`, station queries read full history and event queries
  the `default_datetime` window ending now. Events come newest first.
- No data in the window: 404 for `locations`/`position`; the station is left
  out of `area`/`radius`. An empty area is an empty collection.
- Registry-only areas use `ST_Within` (boundary excluded); observation-derived
  ones test the cached locations (boundary included).
- Features: `bbox`, paging and property filters; `datetime`/`sortby` are 400.

A response holds at most 500 000 values (NULLs included) and an area at most
200 000 stations. Windows up to 12 hours run one statement per observation
table. Longer windows and no `datetime` run one per station (and parameter,
for `per_parameter`), each at most 10 000 rows; past 20 000 statements a
window runs per table again and no `datetime` is refused. Each breach is a
400 saying what to narrow. Formats: [api-edr](../api-edr/README.md),
[api-features](../api-features/README.md).

## Operations

| `[postgis]` field | Default |
|---|---|
| `metadata_refresh_secs` | 300 |
| `pool_size` | 2 × CPUs, clamped to 4–16; at most 32 |
| `pool_label` | `<user>@<host>:<port>/<db>`, the pool metrics label |
| `max_locations` | 200 000; 1–1 000 000, not for `events` |

- A location list over `max_locations` is cut and served, `/health` reports
  degraded and `postgis_locations_truncated` is 1. A station costs under
  1 KiB of cache.
- `/health` also follows a `SELECT 1` ping every 30 s on its own connection
  and the schema check. A failed refresh keeps the previous snapshot; a
  database down at boot gives a degraded collection that recovers once the
  database answers.
- The temporal extent, of the collection and each location, ends at the
  latest observation not after the refresh (for `per_parameter`, in the
  first table); rows over 10 minutes ahead log a warning. A `timestamp
  without time zone` column in a daylight-saving zone can end it up to an
  hour late during the repeated autumn hour; `timestamptz` or UTC avoids
  that.
- Reload keeps unchanged collections' engines. Pools are shared: the first
  pool's size wins, and a second password for the same pool fails that
  collection.
- `/metrics`: `postgis_up`, `postgis_locations_truncated`, `postgis_pool_*`,
  refresh and ping counters, `lightning_strike_cache_*`.

## Limits

- No native TLS: connections are plaintext and `sslmode=require` fails. Use
  loopback, a private network, a VPN or an SSH tunnel.
- No interpolation between stations, no station map layers, no trajectory,
  cube or corridor, no vertical axis, no forecast instances.
- No DDL: the engine creates no tables or indexes. TimescaleDB hypertables
  are read as ordinary tables.

## Testing

```bash
cargo test -p engine-postgis    # unit tests, no database
MC_TEST_POSTGIS_DSN=postgres://… cargo test -p engine-postgis --test db -- --nocapture --test-threads=1
```

The database tests skip without `MC_TEST_POSTGIS_DSN` (they fail instead
with `MC_TEST_POSTGIS_REQUIRED=1`, as in CI, which runs them on
PostgreSQL 17 with PostGIS and TimescaleDB). They need a superuser to create
the extensions and their own schemas of synthetic stations. Contributor rules
are in [CLAUDE.md](CLAUDE.md).
