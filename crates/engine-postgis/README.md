# engine-postgis

Publish observations already stored in PostgreSQL/PostGIS through MeteoCore's
OGC APIs. Clients can discover stations and retrieve measurements by location
and time without database access or a separate export pipeline.

The engine maps existing tables to collections and named parameters using
TOML. It reads measurements in place and refreshes cached station metadata in
the background. It does not ingest data, create tables, or require a
MeteoCore-specific database schema.

## Supported data and APIs

Choose the shape that matches your tables:

| `observations.shape` | Layout |
|---|---|
| `long` | One measurement per row: `station_id, time, parameter, value` |
| `wide` | One row per station/time: `station_id, time, temperature, humidity` |
| `per_parameter` | One table per parameter, each with station, time and value columns |
| `events` | Independent events with their own time and point geometry, such as lightning strikes |

| API | Station shapes (`long`, `wide`, `per_parameter`) | `events` |
|---|---|---|
| EDR | Locations, position, area, radius; time series per station | Area, radius; one point coverage per event |
| Features | Station inventory and configured properties | Not supported |
| Tiles | Station vector tiles (MVT); raster station-symbol tiles when the map layers are on | Raster event-age tiles |
| WMS / Maps | One station-symbol layer per parameter: each station a marker coloured by its value | Event-age layer, intended for lightning |

Station Features and vector tiles contain metadata, not measurements. Use EDR
for values, or the [station map layers](#station-map-layers) to see them on a
map. There is no station interpolation, trajectory/cube query or forecast-run
axis.

Requires PostgreSQL ≥ 13 and PostGIS ≥ 3.0. TimescaleDB hypertables work as
ordinary observation tables; the engine does not manage compression,
retention or continuous aggregates.

## Database setup

Create a dedicated read-only login, adapting the database, password and table
names. Set role-level limits for normal data queries:

```sql
CREATE ROLE meteocore_ro LOGIN PASSWORD '<password>' NOINHERIT;
GRANT CONNECT ON DATABASE weather TO meteocore_ro;
GRANT USAGE ON SCHEMA public TO meteocore_ro;
GRANT SELECT ON public.stations, public.observations TO meteocore_ro;
ALTER ROLE meteocore_ro SET default_transaction_read_only = on;
ALTER ROLE meteocore_ro SET statement_timeout = '5s';
ALTER ROLE meteocore_ro SET lock_timeout = '2s';

CREATE INDEX stations_geom_gix ON public.stations USING GIST (geom);
CREATE INDEX observations_station_time_idx
  ON public.observations (station_id, time DESC);
```

For `per_parameter`, grant and index every observation table. For events,
start with a B-tree on time and a GiST index on geometry. Check plans for
queries and metadata refreshes against your data, and adjust timeouts and
indexes accordingly. With registry-only locations, nearest-station SQL uses
`geom::geography`; its spatial index needs that expression, whereas area SQL
uses plain geometry.

At initialization and after a changed mapping is rebuilt, two advisory audits
use read-only catalog queries, each with a two-second deadline including pool
acquisition. They never scan data, run query plans, attempt writes or change
indexes/grants.

The privilege audit warns about superusers and effective `INSERT`, `UPDATE`
or `DELETE` on mapped relations, including inherited, ownership and column-level
grants. Extra readable columns are allowed. Keep the role settings above even
when no write grants are found: this is not a guarantee that the role cannot
modify the database.

Index diagnostics check station/time B-tree prefixes on every station
observation table, leading-time indexes for events, and live SQL spatial paths.
Valid ASC/DESC and longer composite keys are accepted; INCLUDE-only columns and
invalid indexes do not count. Geometry checks recognize 2D GiST/SP-GiST support.
Missing paths warn with quoted relation/column names and an index suggestion.
Views, potentially relevant partial/expression indexes (including geography),
unfamiliar operator classes, separate/reordered keys and alternative access
methods log **not assessed** when applicability is uncertain. Partitioned/TimescaleDB parent
indexes count; independent child/chunk index changes are not verified. Without
a matching parent index, coverage is not assessed. Materialized views are
inspected directly. These are bounded heuristics, not a
query-plan guarantee; investigate plans before adding indexes.

Failed audits log **unchecked** and retry on metadata refresh or a successful
health ping, without changing collection availability. Completed audits do no
further work on requests or refreshes. Shared pools deduplicate identical
findings; logs name the first collection that detected each one. Rebuild a
collection or restart to recheck changed grants/indexes. Diagnostics stay in
server logs.

Supply the DSN through the server process's environment:

```bash
export OBS_DSN='postgres://meteocore_ro:<password>@127.0.0.1:5432/weather?sslmode=disable'
```

**Native database TLS is not implemented.** Connections use `NoTls`:
`sslmode=require` fails to connect; the default `prefer` permits plaintext.
The example assumes a local database or the local end of an encrypted tunnel.
For remote access, provide a protected connection such as a VPN or SSH tunnel.
A non-loopback host logs a warning. Inline DSNs in TOML are rejected unless
`MC_ALLOW_INLINE_DB_URL=1` is set; keep credentials out of committed config.

## Station configuration

This collection maps `stations(station_id, name, geom, country)` and
`observations(station_id, time, parameter, value)`. Save it as
`collections.d/surface-obs.toml` with
`[server] collections_dir = "collections.d"` configured:

```toml
id = "surface-obs"
title = "Surface observations"
description = "Station air temperature observations"
engine_type = "postgis"
apis = ["edr", "features", "tiles"]

[postgis]
dsn_env = "OBS_DSN"

[postgis.stations]
table = "public.stations"
id_col = "station_id"
label_col = "name"
geom_col = "geom"
property_cols = ["country"]

[postgis.observations]
shape = "long"
table = "public.observations"
station_fk_col = "station_id"
time_col = "time"
param_col = "parameter"
value_col = "value"

[[postgis.parameters]]
name = "t2m"
source_key = "temperature"
label = "2 m air temperature"
unit = "degC"
observed_property = "air_temperature"
```

Here, clients request `t2m` and the database stores `temperature` in its
`parameter` column. Add a `[[postgis.parameters]]` entry for each advertised
parameter. `source_key` and `observed_property` default to `name`.

For inline configuration, put collection fields under `[[collections]]` and
prefix the remaining tables with `collections`, for example
`[collections.postgis]` and `[[collections.postgis.parameters]]`.

### Valid values and missing-value sentinels

Some sources store a code such as `-32766` instead of NULL when a value is
missing. Two optional parameter keys mark which stored values are
observations:

```toml
[[postgis.parameters]]
name = "t2m"
label = "2 m air temperature"
unit = "degC"
valid_range = [-80.0, 60.0]   # inclusive [min, max]
missing_values = [-32766]     # at most 16 sentinels
```

A value outside `valid_range` or equal to one of `missing_values` is served
as null, exactly like a NULL value:

- **EDR** location, position, area and radius series keep the row's time
  with a null value. On the events shape the event stays in the answer with
  a null value.
- **Latest-value reads** pass over it and use the station's latest valid
  value: the station map layers and a `station-grid` collection over this
  one. A station whose values in the window are all filtered is left out,
  as one with only NULLs is.
- Stations reporting only filtered values are still listed, and the
  temporal extent still counts their rows.

The test runs in SQL: the value, cast to `double precision`, must lie inside
the range and differ from every sentinel, each bound and the sentinel list a
bound parameter. Sentinels compare as `double precision`, so a `real` column
holding `-999.9` stores a number a little different from the `-999.9`
written in TOML; list such a code as it reads back in `double precision`, or
exclude it with `valid_range`. NaN lies outside every range. Both keys are
checked at config load: finite numbers, `min <= max`, at most 16 sentinels.
Features properties come from the stations table and are not affected. On
the events shape the filter applies to the EDR values only; the
`cloud_indicator_col` and `peak_current_col` attributes the nowcast's
lightning join reads are not parameters and are never filtered.

### Other station layouts

Keep the station and parameter definitions above and replace the observations
block with one of these. The mapping's `parameter` key matches `source_key`.

For a **wide** table with a `temperature` column:

```toml
[postgis.observations]
shape = "wide"
table = "public.observations"
station_fk_col = "station_id"
time_col = "time"
columns = [{ parameter = "temperature", column = "temperature" }]
```

For **one table per parameter**:

```toml
[postgis.observations]
shape = "per_parameter"
station_fk_col = "station_id"
time_col = "time"
value_col = "value"
tables = [{ parameter = "temperature", table = "public.obs_temperature" }]
```

Each `tables` entry may override the shared `station_fk_col`, `time_col`,
`time_col_tz`, `value_col` and `geom_col`. The advertised temporal extent comes
from the first configured table.

### Column requirements

- Use consistent text station keys and a unique registry ID. Non-empty
  geometry must contain Points with SRID 4326 (X = longitude, Y = latitude); prefer
  `geometry(Point, 4326)` to enforce this in the database.
- For `timestamptz`, omit `time_col_tz`. For `timestamp without time zone`,
  set it to the stored values' timezone, such as `"UTC"` or `"Europe/Helsinki"`.
  Initialization and metadata refresh check every resolved timestamp mapping
  and ask PostgreSQL to validate each timezone. Session timezone need not be UTC.
- Measurements are cast to `double precision`; NULL remains missing. Units
  describe stored values and do not perform conversion. Station properties
  support boolean, integer, real/double precision and text types, including
  NULL. Cast other property types in a view.
- Table names accept `schema.table` or a bare name in `public`. Identifiers
  must match `^[A-Za-z_][A-Za-z0-9_]{0,62}$`. Use a SQL view for joins,
  expressions, type conversions or coordinate transformations.

Database checks cover tables, hypertables, views and materialized views. Known
non-point geometry declarations or SRIDs other than 4326 are rejected, even on
empty sources. Generic geometry columns/views are allowed: validation examines
at most 128 rows per source, including NULL/empty rows, within a five-second
deadline for the whole collection. This is not a full data audit. Every returned
geometry is checked before its coordinates are used, including rows outside the
sample or inserted later. NULL/empty handling is unchanged. Use typed views to
make the contract explicit; the engine does not transform source geometries.

### Locations without a complete registry

Set `observations.geom_col` when observations carry their own points. For
`per_parameter`, every table must resolve a geometry column, either inherited
or set on the table entry.

| Registry | Observation geometry | Locations advertised |
|---|---|---|
| Present | Absent | All registry stations, with or without data |
| Absent | Present | Recent reporting stations; ID as label, no properties |
| Present | Present | Recent reporting stations, enriched from the registry where available |
| Absent | Absent | Invalid configuration |

“Recent” means `observations.locations_window`, default `"PT24H"`; use a
longer ISO 8601 duration or `"all"` for full history. This selects the cached
station inventory, not the measurement query's time range. Archive queries
still need their stations in that inventory. Full-history derivation can be
expensive; a maintained station table or materialized view avoids rescanning.

With observation geometry, position/area selection uses the cached locations
and exact polygon containment, including boundaries. Registry-only selection
uses live PostGIS queries; `ST_Within` excludes boundary points.

Optional `where = "active = true"` under `[postgis.stations]` filters the
registry using a fixed configuration fragment, never HTTP-supplied SQL. In
combined mode it only filters enrichment: excluded reporters can still appear
without registry metadata. Filter the observation source to restrict membership.

### Station map layers

List `wms` or `maps` in `apis`, or add a `[postgis.map]` section, to show a
station collection on a map; `tiles` then serves the layers as raster tiles
too. A collection that lists only `tiles`, without the section, serves its
station vector tiles alone, as before. Each configured parameter is a layer, `surface-obs/t2m` in WMS and
`parameter-name=t2m` in Maps and Tiles; the collection layer draws the first
parameter. A layer draws each cached station as a marker coloured by the
parameter's style, and its legend is that style's colormap legend, as for
raster layers. Style the layers like any raster layer, with `[wms]` and
`[[wms.parameters]]`; a `step` palette gives classes such as threshold
levels. `tiles` keeps serving the station vector tiles beside the raster
ones.

```toml
apis = ["edr", "features", "wms", "maps", "tiles"]

[postgis.map]            # optional; the defaults are shown
max_age = "PT1H"
time_step = "PT10M"
marker_size = 6
draw_missing = false

[wms]
colormap = "temperature"

[[wms.parameters]]
name = "t2m"
min = -30.0
max = 30.0
```

- **Value.** A frame at `TIME` shows each station's latest non-null value
  between `TIME - max_age` and `TIME`, both included. `max_age` is an ISO
  8601 duration of at most seven days. A station without one is left out;
  with `draw_missing = true` it is drawn in the palette's `nodata_color`
  (set it on a named `[[colormaps]]` palette; without one such markers are
  transparent). Null, NaN and infinite values count as no value, and so do
  values the parameter's `valid_range` or `missing_values` rejects.
- **Time.** The layers advertise steps of `time_step` over the latest six
  hours of data, updated on metadata refresh; without `TIME` they draw the
  latest step. A requested `TIME` floors to its step, counted from
  1970-01-01T00:00Z, so with the default `PT10M` both 10:00 and 10:07 draw
  the 10:00 frame. `time_step` is whole seconds from `PT1M` to `PT6H`.
  Older frames can be requested explicitly.
- **Markers.** Discs `marker_size` output pixels across (1–64), on a
  transparent background. Where markers overlap, **the most extreme value
  is on top**: markers are drawn in ascending distance from the middle of
  the layer's default style range (its `min` and `max`, else the palette's
  first and last stops), so the value farthest from it is drawn last.
  Markers without a value are drawn under every value.
- **Late data.** Observations for a past time can arrive late or be
  corrected. With the map layers on, each metadata refresh reads a count and
  hash sums of the rows the advertised frames read, from the first step's window start on; when they or
  the station list change, every cached frame of the collection is drawn
  again. Responses to an explicit `TIME` therefore revalidate instead of
  being `immutable`. A late row before that range is drawn once the
  collection's frames are drawn again for another change, or its cached
  frames are evicted.
- **Cost.** A frame is one statement per observation table: the set-based
  area statement over every cached station, the window and the parameter,
  served by the `(station, time)` index above. It may read 2 000 000 rows
  (stations × steps in `max_age`); past that the layer answers an error that
  names `max_age`. Frames are cached (`MC_STATION_FRAME_CACHE_MB`, default
  32 MiB, about 32 bytes a station), so the tiles of one map share a read.
- **GetFeatureInfo.** The layers are `queryable="0"`: a marker shows the
  latest value within `max_age`, which the EDR position query, an exact
  instant at the nearest station, does not answer.

## Event configuration

For `lightning(id, time, geom, peak_current)`, with UTC values in a timezone-less
timestamp column, use a separate collection file:

```toml
id = "lightning"
title = "Lightning strikes"
description = "Point events with measured peak current"
engine_type = "postgis"
apis = ["edr", "wms", "maps", "tiles"]

[postgis]
dsn_env = "OBS_DSN"
metadata_refresh_secs = 60

[postgis.observations]
shape = "events"
table = "public.lightning"
id_col = "id"
time_col = "time"
time_col_tz = "UTC"
geom_col = "geom"
default_datetime = "PT1H"
extent_bbox = [4.0, 54.0, 42.0, 72.0]

[[postgis.parameters]]
name = "peak_current"
label = "Peak current"
unit = "kA"

```

`extent_bbox` is required: it supplies the advertised extent as
`[west, south, east, north]`, without scanning event geometry. Parameter
`source_key` names the measurement column. Events have no registry or station
foreign key; `locations_window` and `max_locations` are invalid for this shape.

EDR returns events newest first. Without `datetime`, it queries
`default_datetime` ending now (default one hour). Event maps use that window
ending at TIME, floored to the minute, and draw two-pixel-radius discs colored
by **age in minutes**. With no `[wms]` palette the layer takes the built-in
`lightning_age` default, 0–60 minutes, which fits the default one-hour
window. For another window, set `[wms] max` to its length in minutes.
Advertised steps cover up to the latest six hours at one-minute intervals;
older frames can be requested explicitly. Omitted TIME follows the latest
advertised step, updated on metadata refresh; live clients should send TIME.

Map windows are cached (`MC_LIGHTNING_STRIKE_CACHE_MB`, default 32 MiB) and
keep at most the newest 200,000 events, warning on truncation. Increasing cache
capacity does not raise that limit.

For a nowcast `lightning_source`, optional `observations.cloud_indicator_col`
(0 = cloud-to-ground, 1 = intra-cloud) and `peak_current_col` (signed kA) supply
flash attributes independently. NULL means unknown; unknown currents do not
count in the polarity-share denominator. EDR still needs separate parameter
entries. Set `observations.coverage_bbox` only to a rectangle of guaranteed
detection, separate from `extent_bbox`; otherwise nowcast counts are null.
The cell's full footprint and 10 km attribution radius must fit within it.

## Queries and limits

Discover stations, then request measurements; substitute an ID and dates from
your data:

```bash
curl 'http://localhost:8000/edr/collections/surface-obs/locations?limit=100'
curl --get 'http://localhost:8000/edr/collections/surface-obs/locations/station-1' \
  --data-urlencode 'datetime=2026-10-16T00:00:00Z/2026-10-17T00:00:00Z' \
  --data-urlencode 'parameter-name=t2m'
curl --get 'http://localhost:8000/edr/collections/lightning/area' \
  --data-urlencode 'coords=POLYGON((24 59,26 59,26 61,24 61,24 59))'
```

A station position query selects the nearest station within **25 km**. An
instant matches that timestamp, not the nearest observation time. Without
`datetime`, station queries can read full history. Missing station data gives
404 for location/position; area/radius queries omit stations without data and
can return an empty collection. An empty event query is also valid.

CoverageJSON is the default. Station queries also support CSV; locations,
position and radius support EDR GeoJSON. Station Features support bbox,
paging and property filters, but reject `datetime` because the inventory has
no time dimension. See the [EDR](../api-edr/README.md) and
[Features](../api-features/README.md) references for full request/format rules.

EDR engine queries have a **500,000-value budget**, counting NULLs and values a
`valid_range` or `missing_values` filters, and station
areas may select at most 200,000 stations. Long-window or unbounded area
queries can also hit the 20,000-statement and 10,000-row-per-statement guards;
single-station queries do not have that row cap. Observation-derived polygon
selection is limited to 20 million cached-location × vertex tests (rectangles
are exempt). Budget breaches return HTTP 400 with narrowing guidance. Database
statement timeouts apply independently.

## Operations

| Setting under `[postgis]` | Default / behavior |
|---|---|
| `metadata_refresh_secs` | 300; refreshes cached locations and extents, and checks station map frames for late data |
| `pool_size` | Twice CPU parallelism, bounded to 4–16; explicit values 1–32 |
| `pool_label` | Optional pool metrics label |
| `max_locations` | 200,000; valid range 1–1,000,000 for station shapes |

A location list exceeding `max_locations` is served partially, with degraded
health and `postgis_locations_truncated = 1`. A cut registry in combined mode
can instead leave reporters without labels/properties. Memory follows actual
stations and their properties: the representative snapshot test stays below
1 KiB/station, or about 195 MiB at 200,000 stations. Refreshes hold old and new
snapshots plus database rows; responses need additional memory. Raising the
inventory cap does not raise query limits.

The advertised temporal extent, of the collection and of each location, runs
from the earliest observation to the latest one not after the metadata
refresh. Rows dated after the refresh are left out of it. When the latest of
them is more than 10 minutes ahead, the refresh that first finds them logs a
warning with its time, and the refresh that no longer finds them logs that
too. While such rows exist, and not every row is among them, a refresh runs
a second extent query, bounded at its own time. Both extent queries read
`MIN` and `MAX` of the bare time column, which an index led by that column
answers without scanning the table.

A `timestamp without time zone` column in a zone with daylight saving time
is bounded in its own local time. In the repeated autumn hour PostgreSQL
reads an ambiguous local time as standard time, so the rows of that hour's
first pass read back an hour late: the extent can then end up to an hour
after the refresh, and the warning can appear and clear once around the
change. A `timestamptz` column, or one holding UTC, avoids this.

A database outage keeps the previous snapshot; an initial outage registers a
degraded engine with no loaded stations. Database queries wait for successful
schema validation and metadata publication. `/health` combines a dedicated
`SELECT 1` probe every 30 seconds (two-second deadline), schema validation and
station-list truncation. Confirmed schema mismatches block data access and stay
degraded even when pings succeed. Pending/failed validation is retried on a
successful ping and on metadata refresh; recovery requires a successful refresh.
Detailed database diagnostics are logged, while public health errors stay generic.
Config resolution or pool construction failures leave the collection failed.
`/metrics` exposes `postgis_*` pool, reachability and refresh metrics, and
`lightning_strike_cache_*` for event maps and `station_frame_cache_*` for
station map layers.

`POST /admin/collections/reload` reuses unchanged engines and rebuilds changed
mappings. Pools are shared by `(host, port, database, user, sslmode)`; the first
pool's size wins and conflicting passwords are rejected. Changing an
environment-supplied DSN or password requires a server restart.
