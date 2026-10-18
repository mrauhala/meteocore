# engine-csv

Serves station observations from one CSV file through OGC API - EDR and
OGC API - Features.

## What it solves

Point observations often come as a flat table: one row per station and
time, one column per measured quantity. This engine publishes such a file
without conversion. EDR clients get a time series for one station, or for
every station in an area or radius. Features clients get the station
inventory as points.

## How it works

- `CsvDataStore::load` reads the whole file into memory at startup and
  indexes the rows by location, and by location and time.
- The first four columns are fixed by position: location, latitude,
  longitude, time. Every further column is a parameter named by its header.
- `CsvEngine::new` builds the station inventory once, in the order the
  stations first appear in the file, along with the EDR location list and
  the observation time span. `/locations`, Features `/items` and the
  Features temporal extent are served from these without scanning the rows.
- A station sits at the latitude and longitude of its first row.
- Every EDR result is a `PointSeries` per station, without a vertical axis.
  Because each one sits at a listed location, the EDR layer answers
  `locations` and `radius` in EDR GeoJSON as well as CoverageJSON, and
  those and `area` in CSV.
- The file is static. There is no poll loop: an explicit reload
  (`POST /admin/collections/reload`) re-reads it.

## What it serves

| API | Query | Result |
|---|---|---|
| EDR | `locations` (`/locations`, `/locations/{locId}`) | the station list; one station's series in the `datetime` window |
| EDR | `area` (WKT `POLYGON` or `west,south,east,north`) | one series per station inside the area that has data in the window |
| EDR | `radius` | the circle as a 64-vertex geodesic polygon, then answered as `area` |
| EDR | `items` | the station features below, whenever the collection lists `edr` |
| Features | `/items`, `/items/{featureId}` | one Point per station, properties `name`, `latitude`, `longitude` |

- EDR `datetime` selects rows whose time falls in the window, both ends
  included. On `/locations` and on Features `/items` it keeps the stations
  with at least one row in the interval.
- `parameter-name` matches names ignoring ASCII case. An unknown name is a
  400 naming the valid parameters, checked before any data is read. A blank
  value selects every parameter.
- A requested station with no row in the window is a 404. An area or radius
  leaves such stations out and is a 404 only when no station inside has
  data.
- Features accepts `bbox` (station point inside the box), `datetime`,
  equality filters on `name`, `latitude` and `longitude`, and
  `limit`/`offset`. The feature id is the location value.
- Parameter metadata: the label is the column name with `_` as a space.
  Units come from a built-in table keyed by exact column name:
  `temperature` °C, `humidity` %, `wind_speed` m/s, `pressure` hPa,
  `precipitation` mm. Any other column has no unit.

The EDR status page compares this with the other engines:
[Per-engine query-type matrix](../api-edr/README.md#per-engine-query-type-matrix).

## File format

```csv
location,latitude,longitude,time,temperature,humidity,wind_speed
Helsinki,60.1699,24.9384,2026-01-01T00:00:00Z,-2.5,85,3.2
Helsinki,60.1699,24.9384,2026-01-01T01:00:00Z,-2.7,86,
```

- Comma-separated, with a header row and standard CSV quoting. Every row
  must have as many fields as the header.
- The first four columns are read by position; their header names are not
  checked.
- `location` is any string. It is the station id in both APIs.
- `latitude` and `longitude` are WGS84 decimal degrees. They are not
  range-checked.
- `time` is RFC 3339 with a mandatory UTC offset: `Z`, `UTC`, `+02:00` or
  `+0200`, and a space may replace the `T`. It is stored as UTC. A
  timestamp without an offset fails the load. A file that mixes offsets
  still loads, with one warning per load giving the row count per offset.
- A parameter cell that does not parse as a number, such as an empty cell,
  is null.

A malformed row fails the whole load. The collection is then reported as
failed in `/health`, and the other collections load as usual.

## Configuration

There is no engine-specific table; the collection's top-level fields are
enough.

```toml
[[collections]]
id = "weather"
title = "Finnish Weather Observations"
description = "Hourly weather observations from Finnish weather stations"
engine_type = "csv"                 # the default engine_type
data_path = "testdata/weather.csv"  # required; a local file
apis = ["edr", "features"]          # default ["edr"]
```

- `data_path` is a local file path. A relative path resolves against the
  server's working directory.
- `apis` accepts only `edr` and `features`. Any other API makes the
  collection fail to load.
- The runnable example is
  [`collections.d/weather.toml`](../../collections.d/weather.toml).
- `server --auto-collections <DIR>` creates one such collection, with EDR
  and Features, per `.csv` file in a directory that holds no QueryData,
  GRIB, GeoTIFF or HDF5 files.

## Limits

- Local files only. The whole file is held in memory.
- Rows added to the file are served only after a reload.
- One position per station, from its first row. A later row with other
  coordinates does not move the station in `/locations`, Features or area
  matching. A series takes the coordinates of its first row in the window.
- If two rows share a location and time, the later row's values are served.
- Units come only from the built-in table and cannot be configured.
- An `area` or `radius` that contains more than 500 stations is a 400,
  counted before the `datetime` filter.
- No `position`, `cube` or `trajectory` query, no vertical axis and no
  model runs (instances).
- No Features `sortby` (400).

## Testing

```sh
cargo test -p engine-csv
cargo bench -p engine-csv   # criterion benchmarks over testdata/weather.csv
```

The unit tests live in `src/loader.rs` (time parsing, offset reporting) and
`src/engine.rs` (paging, filters, `datetime`, parameter selection, area and
radius). They use two fixtures:

- `tests/fixtures/station_history.csv`: five rows, three stations, one of
  which moves and one of which has no second hour.
- `testdata/weather.csv` at the workspace root: hourly observations from
  193 Finnish weather stations for January 2026, about 143 000 rows.
