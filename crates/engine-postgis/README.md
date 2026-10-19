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
| `long` | Row per value: station, time, parameter, value | `locations`, `position`, `area`, `radius` | Station inventory | Vector (MVT); raster with the map layers | [Station layer](#station-map-layers) per parameter |
| `wide` | Row per station and time, column per parameter | same | same | same | same |
| `per_parameter` | Table per parameter: station, time, value | same | same | same | same |
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
valid_range = [-80.0, 60.0]  # optional, inclusive [min, max]
missing_values = [-32766]    # optional, at most 16 codes
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

### Missing values and valid ranges

Some sources store a code such as `-32766` instead of NULL. A value outside
a parameter's `valid_range`, or equal to one of its `missing_values`, reads
as NULL. Config load requires finite numbers, `min <= max` and at most 16
codes.

- EDR series keep the row's time with a null value; an event keeps its
  place with a null value.
- The station map layers and a
  [`station-grid`](../engine-station-grid/README.md) collection over this
  one use each station's latest valid value.
- The location list, the temporal extent and the value budget still count
  the row. Features properties come from the stations table and are not
  filtered, nor are the event attributes a nowcast reads.

The test runs in SQL on the value cast to `double precision`. A `real`
column's `-999.9` is therefore not the TOML `-999.9`: list the code as it
reads back in `double precision`, or exclude it with `valid_range`. NaN
falls outside every range.

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

### Station map layers

A station collection that lists `wms` or `maps` in `apis`, or has a
`[postgis.map]` section, draws each parameter as a map layer: every cached
station is a disc coloured by its latest value. In WMS, a collection of
several parameters has a layer `<collection>/<parameter>` each, such as
`surface-obs/t2m`, and `<collection>` draws the first; a single parameter
is the layer `<collection>`. Maps and Tiles select one with
`parameter-name`, by default the first. Listed with `tiles`, the layers are
raster tiles beside the station vector tiles. A collection that lists only
`tiles` and has no `[postgis.map]` serves vector tiles alone.

```toml
[postgis.map]            # optional; the defaults are shown
max_age = "PT1H"         # ISO 8601, at most P7D
time_step = "PT10M"      # whole seconds, PT1M to PT6H
marker_size = 6          # disc diameter in pixels, 1 to 64
draw_missing = false

[wms]                    # styled like any raster layer
colormap = "temperature"

[[wms.parameters]]
name = "t2m"
min = -30.0
max = 30.0
```

`[postgis.map]` needs `wms`, `maps` or `tiles` in `apis` and is rejected
on the events shape. An unknown key in it is a load error.

- A frame at `TIME` shows each station's latest valid value in
  `[TIME - max_age, TIME]`. A station without one is left out, or with
  `draw_missing = true` drawn in the palette's `nodata_color` (transparent
  unless the palette sets one).
- The layers advertise `time_step` steps over the newest six hours of data,
  updated on refresh; without `TIME` they draw the newest step. A `TIME`
  floors to its step, counted from 1970-01-01T00:00Z.
- Where discs overlap, the value farthest from the middle of the layer's
  default style range (its `min` and `max`, else the palette's end stops)
  is on top. Stations without a value lie under the rest.
- Rows can arrive late or be corrected. With the layers on, each metadata
  refresh fingerprints the station list and the rows the advertised frames
  read, and a change redraws the collection's cached frames. Responses to
  an explicit `TIME` therefore revalidate rather than being `immutable`.
  A late row older than the advertised frames' windows is not detected.
- A frame is one set-based read over every cached station, cached
  (`MC_STATION_FRAME_CACHE_MB`, default 32) so the tiles of one map share
  it. A frame that would read over 2 000 000 rows (every station's rows
  within `max_age`) fails with an error that names `max_age`.
- WMS GetFeatureInfo does not answer these layers (`queryable="0"`): a
  marker's latest value within `max_age` is not what an EDR position query
  returns.

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

Without a `[wms]` palette the layer takes the built-in `lightning_age`
style, 0–60 minutes, which fits the default one-hour window; for another
window, set `[wms] max` to its length in minutes. A map frame draws the
events of the `default_datetime` window ending at `TIME` (floored to the
minute) as discs of 2-pixel radius, valued by age in minutes. Advertised
times are one-minute steps over the newest six hours of data; without `TIME`
the frame follows the newest step, updated on refresh. Each window is one
read, cached (`MC_LIGHTNING_STRIKE_CACHE_MB`, default 32) and capped at the
newest 200 000 events.

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
  With `edr` in `apis`, each station links (`rel="related"`) to its EDR
  location.

A response holds at most 500 000 values (NULLs and filtered values
included) and an area at most 200 000 stations. Windows up to 12 hours run
one statement per observation table. Longer windows and no `datetime` run
one per station (and parameter, for `per_parameter`), each at most 10 000
rows; past 20 000 statements a window runs per table again and no
`datetime` is refused. Each breach is a 400 saying what to narrow.
Formats: [api-edr](../api-edr/README.md),
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
  refresh and ping counters, `lightning_strike_cache_*` (event maps) and
  `station_frame_cache_*` (station map layers).

## Limits

- No native TLS: connections are plaintext and `sslmode=require` fails. Use
  loopback, a private network, a VPN or an SSH tunnel.
- No interpolation between stations (a
  [`station-grid`](../engine-station-grid/README.md) collection over this
  one grids them), no trajectory, cube or corridor, no vertical axis, no
  forecast instances.
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
