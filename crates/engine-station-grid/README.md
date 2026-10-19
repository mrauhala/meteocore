# engine-station-grid

Grids station observations on the fly and serves the fields through WMS
1.3.0, OGC API - Maps, OGC API - Tiles and OGC API - EDR
(`engine_type = "station-grid"`).

## What it solves

Weather stations report at points, but a temperature or wind map needs a
value in every cell. This derived collection builds that field on the fly
from another collection's station observations: it interpolates one time
slice onto a regular latitude/longitude grid. No grid is stored on disk;
late or corrected reports change the field when it is next computed.

## How it works

- **Source and slice.** `source` is a CSV, BUFR or PostGIS stations
  collection in the same config, whatever APIs it lists. A slice is one EDR
  area query to it over the grid widened by the search radius, for the
  window `[t - max_age, t]`, ends included. Each station gives its latest
  finite value in the window; a station without one is left out.
- **Interpolation.** Each cell is estimated at its centre from at most
  `max_neighbours` stations within `search_radius_km`, by great-circle
  distance on the sphere, so stations either side of ±180° or around a pole
  are neighbours. A cell with no station in reach is nodata; a station
  within 1 mm of a cell centre gives the cell its value.
  `idw` weights `d^-power`. `adw` weights `exp(-d / correlation_length)^power`
  times an angular term, so a station in a direction no other covers gets up
  to three times the weight of one in a cluster. `nearest` takes the nearest
  station's value, for categorical fields.
- **Grid and time.** CRS84 over `bbox`, `resolution` adjusted so whole cells
  cover it; `west > east` crosses the antimeridian and `[-180, s, 180, n]`
  goes round the Earth. Values are `f32` in the source's units. Analysis
  times fall every `cadence` across the source's temporal extent, on
  multiples of `cadence` counted from 1970-01-01T00:00Z (on the hour for
  `PT1H`); the latest 10 000 are kept.
- **Computing and caching.** A grid is computed once per collection,
  parameter, analysis time and revision of its input, on one thread of a
  pool of its own, and kept in a process-wide cache; concurrent requests
  share one computation, which first reserves its grid's bytes plus 4 MiB
  from an admission budget. A request that does not fit, or whose grid is
  not ready by the request deadline, gets a 503; the computation goes on and
  serves the retry. A partial grid is never served.
- **Poll and revisions.** Every `poll_interval_secs`, on the background poll
  runtime, the engine rebuilds the time axis, re-reads the latest
  `precompute` times of every parameter and computes those not cached, one
  at a time, leaving half the admission budget to requests. A changed slice
  fingerprint is a revision: that time's grid is computed again and the
  content version moves, which re-keys the rendered map caches. The version
  is never 0, so maps for an explicit `TIME` revalidate after 60 s.

## What it serves

| Trait | APIs | Notes |
|---|---|---|
| `MapEngine` | WMS, Maps, Tiles | One layer per parameter; without a parameter name, the first |
| `EdrEngine` | EDR | `position`, `area`, `radius`, `cube` |

**Maps.** A requested instant (WMS `TIME`, Maps/Tiles `datetime`) draws the
latest analysis time not after it, or the earliest when it precedes them
all; without one, the latest. With no analysis time yet a tile is empty.
Each pixel takes the grid cell under it, through a coarse projection grid
for projected CRSs. Layer titles read like "temperature (ADW analysis)".

**EDR.** Every query samples the cached grid the maps draw, at the cell
under each point, so a position query and WMS
[GetFeatureInfo](../api-wms/README.md#getfeatureinfo) answer the value the
map shows, not an estimate at the exact point.

- Without `datetime` a query reads the latest analysis time; an instant
  must be one (else 404); an interval reads every one inside it, at most 96
  analyses (times × parameters) per query (else 400).
- `parameter-name` ignores case; an unknown name is a 400. `position` takes
  `POINT(lon lat)`; off the grid is a 404.
- `area` is a CRS84 grid over the polygon's bounding box at the analysis
  resolution, at most 256 cells per axis, null outside the polygon.
  `radius` is answered as an area. `cube` returns the analysis cells whose
  centres lie in the bbox, or `resolution-x`/`-y` evenly spaced positions.
  An area, radius or cube response holds at most 1 M values (else 400).
- No `locations` or `trajectory` query, instances, items or vertical axis.

Every API's collection description ends with a note on the source, method
and grid. `/health` shows the collection degraded until its first analysis
time. Of the `station_grid_*` metrics, `station_grid_stations` counts the
stations behind each parameter's latest analysis.

## Configuration

```toml
id = "weather-analysis"
engine_type = "station-grid"
apis = ["wms", "maps", "tiles", "edr"]   # only these four

[station_grid]
source = "weather"
parameters = ["temperature", "wind_speed"]
grid = { bbox = [19.0, 59.0, 32.0, 71.0], resolution = 0.05 }
method = "adw"
cadence = "PT1H"
```

In `config.toml` the table is `[collections.station_grid]`. Styles come
from `[wms]` as for any map collection, in the source's units. The runnable
example is
[`collections.d/weather-analysis.toml`](../../collections.d/weather-analysis.toml),
over the `weather` CSV collection.

| Field | Default | Meaning |
|---|---|---|
| `source` | – | Id of a CSV, BUFR or PostGIS stations collection; not this one |
| `parameters` | – | Source parameters to grid, at least one, no repeats; one layer each |
| `grid.bbox`, `grid.resolution` | – | `[west, south, east, north]` and cell size, in degrees |
| `grid.crs` | `"EPSG:4326"` | Must be CRS84: `EPSG:4326`, `CRS:84`, `OGC:CRS84` or the OGC CRS84 URI |
| `method` | `"idw"` | `idw`, `adw` or `nearest` |
| `power` | 2 (`idw`), 4 (`adw`) | IDW distance power or ADW decay exponent; not for `nearest` |
| `correlation_length_km` | `search_radius_km` | ADW decay length; `adw` only |
| `max_neighbours` | `8` | Stations per cell, 1 to 64 |
| `search_radius_km` | `150` | Above 0, at most 10 000 |
| `max_age` | `"PT1H"` | Oldest observation used, ISO 8601 |
| `cadence` | – | Analysis time step, ISO 8601. Required: no station engine lists its instants |
| `poll_interval_secs` | `60` | Poll period, above 0; under 5 runs as 5 |
| `precompute` | `3` | Latest times per parameter computed ahead and re-checked, 0 to 24 |

An unknown key, or a `source` naming this collection, fails the config load;
an out-of-range value fails only this collection. A grid has at most
16 777 216 cells. One grid (4 bytes a cell) must fit the grid cache and one
computation half the admission budget, or the collection fails to load.
Every station grid shares the cache (`MC_STATION_GRID_CACHE_MB`, default
256 MiB), the budget (`MC_STATION_GRID_MEMORY_MB`, default 512 MiB) and the
pool (`MC_STATION_GRID_WORKERS` grids at once, at most 64; default half the
CPUs, between 1 and 4).

## Limits

- CRS84 analysis grids and the three methods only. No correction for
  station elevation, no land/sea mask, no station exclusions or quality
  flags, no interpolation in time. Zoomed in past `resolution`, the nearest
  cell sampling shows cells as blocks.
- A CSV source answers an area of at most 500 stations, so a grid whose
  search box holds more fails to compute.
- Late reports are found within one poll only for the latest `precompute`
  times; an older time is re-checked when its grid is next computed. A
  revision re-keys the map tiles of every time.
- A reload rebuilds a grid whose config or source changed, and every grid
  over a CSV source (CSV engines are always rebuilt).

## Testing

```sh
cargo test -p engine-station-grid
python3 crates/engine-station-grid/tests/reference/idw_adw_reference.py
```

Unit tests in `src/` cover the grid geometry, the neighbour search and the
methods. `tests/integration.rs` grids `testdata/weather.csv` through the CSV
engine (maps in three CRSs, every EDR query type, bad configs) and in-memory
sources (`170,10,-170,20` across the antimeridian, a polar EPSG:3035 view,
late reports, a miss past the deadline). The Python script is an independent
IDW/ADW implementation; the test pins its output to 1e-4 °C.

Contributor rules for this crate are in [CLAUDE.md](CLAUDE.md).
