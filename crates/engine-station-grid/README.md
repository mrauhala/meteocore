# engine-station-grid

Grids station observations on the fly and serves the fields through WMS
1.3.0, OGC API - Maps, OGC API - Tiles and OGC API - EDR, and how well each
station fits the field, its leave-one-out residual, through OGC API -
Features (`engine_type = "station-grid"`).

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
  `PT1H`). Without `cadence` they are the source's own observation
  instants: every distinct time a CSV file or a BUFR store holds,
  off-hour reports included, read from the source's snapshot and not
  listed in its own metadata; a PostGIS source lists none and needs a
  `cadence`. The latest 10 000 are kept.
- **Leave-one-out residuals.** With each grid, every station of the slice
  is estimated at its own position from the other stations, through the
  same per-target estimate a grid cell gets (same method, neighbours and
  radius); its residual is that estimate minus its value. One neighbour
  search per station, cached with the grid.
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
  at a time, leaving half the admission budget to requests. It then
  re-reads at most 8 older times whose grid is still cached, newest first
  and round-robin across cycles. A changed slice fingerprint is a revision:
  that time's grid is computed again and the content version moves, which
  re-keys the rendered map caches. A request whose own computation finds a
  revision keeps the grid under the new revision, so the next request hits
  the cache. The version is never 0, so maps for an explicit `TIME`
  revalidate after 60 s.
- **Corrections.** A terrain, a land/sea mask and station exclusions; see
  [Corrections](#corrections).

## What it serves

| Trait | APIs | Notes |
|---|---|---|
| `MapEngine` | WMS, Maps, Tiles | One layer per parameter; without a parameter name, the first |
| `EdrEngine` | EDR | `position`, `area`, `radius`, `cube` |
| `FeatureEngine` | Features | Leave-one-out residuals, with `features` in `apis` |

**Maps.** A requested instant (WMS `TIME`, Maps/Tiles `datetime`) draws the
latest analysis time not after it, or the earliest when it precedes them
all; without one, the latest. With no analysis time yet a tile is empty.
Each pixel takes the grid cell under it, through a coarse projection grid
for projected CRSs. Layer titles read like "temperature (ADW analysis)".

**EDR.** Every query samples the cached grid the maps draw, at the cell
under each point, so a position query and WMS
[GetFeatureInfo](../api-wms/README.md#getfeatureinfo) answer the value the
map shows, not an estimate at the exact point. A cell the land/sea mask
leaves out is null there as it is transparent on the map.

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

**Features.** `/collections/<id>/items` is one Point per station,
parameter and analysis time, with `parameter`, `time` (the analysis time),
`value` (the station's value in the slice), `estimate` (the analysis there
from the other stations), `residual` (`estimate − value`) and `neighbours`
(the stations behind the estimate). `estimate` and `residual` are null for
a station with no other in reach. The largest `|residual|` marks a station
its neighbours do not support: a faulty sensor, a wrong position, or a
local effect the network cannot resolve.

- Without `datetime` the latest analysis time; an instant selects it if it
  is an analysis time, else nothing; an interval selects every analysis
  time inside it, ends included. At most 96 analyses (times × parameters)
  per request, else 400. A `parameter` or `time` filter narrows the
  analyses read.
- `sortby` takes `residual`, `estimate`, `value`, `neighbours`, `time` and
  `parameter`; every property is a filter. Ids are
  `<parameter>@<time>@<lon>,<lat>`, with `~<n>` for further stations at
  one position.
- A request never waits for a computation: an analysis not computed yet is
  a 503 with `Retry-After: 1` while the pool computes it, and the retry is
  served from the cache. The poll keeps the latest `precompute` analysis
  times ready, so a request without `datetime` is served at once.
- No EDR `items`: the collection's EDR data is the grid.

Every API's collection description ends with a note on the source, method
and grid. `/health` shows the collection degraded until its first analysis
time. Of the `station_grid_*` metrics, `station_grid_stations` counts the
stations behind each parameter's latest analysis, `station_grid_loo_rmse`
is the RMSE of its leave-one-out residuals in the parameter's units, and
`station_grid_loo_residuals` counts the stations that have one.

## Configuration

```toml
id = "weather-analysis"
engine_type = "station-grid"
apis = ["wms", "maps", "tiles", "edr", "features"]   # only these five

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
| `parameters` | – | Source parameters to grid, at least one, no repeats; one layer each. A name, or a table `{ name, lapse_rate, land_mask }` ([Corrections](#corrections)) |
| `grid.bbox`, `grid.resolution` | – | `[west, south, east, north]` and cell size, in degrees |
| `grid.crs` | `"EPSG:4326"` | Must be CRS84: `EPSG:4326`, `CRS:84`, `OGC:CRS84` or the OGC CRS84 URI |
| `method` | `"idw"` | `idw`, `adw` or `nearest` |
| `power` | 2 (`idw`), 4 (`adw`) | IDW distance power or ADW decay exponent; not for `nearest` |
| `correlation_length_km` | `search_radius_km` | ADW decay length; `adw` only |
| `max_neighbours` | `8` | Stations per cell, 1 to 64 |
| `search_radius_km` | `150` | Above 0, at most 10 000 |
| `max_age` | `"PT1H"` | Oldest observation used, ISO 8601 |
| `cadence` | – | Analysis time step, ISO 8601. Unset: the source's observation instants; required over PostGIS, which lists none |
| `poll_interval_secs` | `60` | Poll period, above 0; under 5 runs as 5 |
| `precompute` | `3` | Latest times per parameter computed ahead and re-checked, 0 to 24 |
| `elevation_source` | – | Id of a raster collection holding terrain heights; see [Corrections](#corrections) |
| `elevation_parameter` | – | Its parameter, when it has several |
| `land_mask_source` | – | Id of a raster collection holding a land/sea mask |
| `land_mask_parameter` | – | Its parameter, when it has several |
| `exclude_where` | – | Station property filter: a table, or a list of tables |

An unknown key, or a `source`, `elevation_source` or `land_mask_source`
naming this collection, fails the config load; an out-of-range value fails
only this collection. A grid has at most 16 777 216 cells. One grid (4 bytes
a cell), with its terrain (4 bytes a cell of the widened grid) and mask (1
byte a cell), must fit the grid cache and one computation half the
admission budget, or the collection fails to load.
Every station grid shares the cache (`MC_STATION_GRID_CACHE_MB`, default
256 MiB), the budget (`MC_STATION_GRID_MEMORY_MB`, default 512 MiB) and the
pool (`MC_STATION_GRID_WORKERS` grids at once, at most 64; default half the
CPUs, between 1 and 4).

## Corrections

```toml
[station_grid]
source = "obs-synop"
parameters = [
    "air_temperature",                               # BUFR: -6.5 K/km by default
    { name = "dew_point_temperature", lapse_rate = -2.0 },
    { name = "wind_speed", land_mask = false },
]
elevation_source = "terrain"        # a raster collection of heights
land_mask_source = "land"           # a raster collection of land fractions
exclude_where = { exclude_from_gridding = true }
grid = { bbox = [19.0, 59.0, 32.0, 71.0], resolution = 0.05 }
cadence = "PT1H"
```

- **Terrain and mask.** Other collections of the same config, named by id:
  any non-derived raster collection (GeoTIFF or COG, Zarr, NetCDF, GRIB,
  QueryData, satellite, ODIM composite), whatever APIs it lists. A missing,
  derived or non-raster collection fails the grid at load. The poll samples
  each once per engine through the collection's map rendering, CRS84 at
  one value per analysis cell centre; the terrain over the grid widened by
  whole cells to the search radius, for stations outside the grid. Until
  both are sampled (a remote collection still polling) the collection has no
  analysis time: maps are empty and EDR answers 404, never an uncorrected or
  unmasked value, and `/health` says what it waits for. The poll retries
  every 5 s. They are not sampled again: a new terrain needs the grid
  rebuilt, by a reload that rebuilds the terrain collection or by a
  restart.
- **Units.** Heights must state `m`, `km`, `ft` or `gpm` (read as metres);
  a mask `1`, `proportion` or no unit (inside from 0.5) or `%` (inside from
  50). Anything else, or a collection of several parameters without
  `elevation_parameter` / `land_mask_parameter`, fails the grid at load
  (for a remote collection, when first sampled: it then stays degraded).
- **Lapse rate.** A corrected parameter's station values are reduced to sea
  level, `v − Γ·z_station`, interpolated, and each cell gets `Γ·z_cell`
  back. Γ is the parameter's `lapse_rate` in K/km; without one it is −6.5
  where the source's own metadata says the parameter is air temperature (a
  CF standard name, as BUFR's built-in `air_temperature`, or an observed
  property the source states, as a PostGIS `observed_property`), and no
  correction otherwise, whatever the name. CSV states neither, so a CSV
  temperature sets `lapse_rate`. `lapse_rate = 0` turns the default off.
  The unit must be K, °C or °F. `elevation_source` with no corrected
  parameter fails the grid.
- **Station heights.** The height the source states for the station, the
  `elevation` property of its station inventory (BUFR's; a PostGIS
  registry's `property_cols` column or view alias named `elevation`, in
  metres), else the terrain at its position, bilinear between the sampled
  cells. A stated height more than 100 m from the terrain is logged once per
  station; a station with neither is left out of corrected parameters and
  logged once. Over terrain nodata (sea in many DEMs) a corrected cell is
  nodata.
- **Mask.** Cells outside the mask, or over its nodata, are nodata in every
  parameter, before the grid is cached, so maps and EDR agree; a parameter
  with `land_mask = false` keeps its estimate from every station. Stations
  are never filtered by the mask.
- **Exclusions.** A station of the source's station inventory (its Features
  items) matching a rule is left out of every analysis: each property a rule
  names equals one of its values, as the Features `/items` property filter
  compares them; a list of tables is several rules. CSV stations carry only
  `name`, BUFR stations `name`, `wigos_station_identifier` and `elevation`,
  PostGIS stations their `property_cols`.
- **Revisions.** The poll re-reads the inventory when the source's data
  version moves. Stations matched by position (to 1e-6°), as the area
  answer carries no station id. A slice's fingerprint covers the stations
  left out and the heights used, so a station newly excluded, or a height
  newly stated, is a revision like a late report: found within one poll for
  the latest `precompute` times, and for an older time whose grid is cached
  by the round-robin re-checks.
- **Residuals.** A corrected parameter's leave-one-out estimate is formed
  from the other stations' sea-level values and put back at the station's
  own height, so its residual is in the observed values and a station on a
  ridge is not an outlier for being high. An excluded station and one
  without a height have no residual; the mask removes none.

## Limits

- CRS84 analysis grids and the three methods only. No quality-flag filter,
  no interpolation in time. Zoomed in past `resolution`, the nearest cell
  sampling shows cells as blocks.
- The terrain is sampled at the analysis resolution, so a cell's height, and
  a station's without a stated one, are the terrain at that resolution.
  A station beyond the widened terrain grid with no stated height is left
  out of corrected parameters.
- A CSV source answers an area of at most 500 stations, so a grid whose
  search box holds more fails to compute.
- Without a lapse-rate correction a residual is in the field's own space,
  so a station on a hill reads as an outlier in a temperature field.
- Late reports are found within one poll for the latest `precompute`
  times. Older times whose grid is cached are re-read 8 a cycle, so with K
  of them a late report is found within ⌈K / 8⌉ polls. An older time whose
  grid has left the cache is not re-read: its next request reads the
  source, but map tiles already rendered from it stay cached until the
  content version next moves. A revision re-keys the map tiles of every
  time.
- A reload rebuilds a grid whose config, source, terrain or mask changed,
  and every grid over a CSV source (CSV engines are always rebuilt).

## Testing

```sh
cargo test -p engine-station-grid
python3 crates/engine-station-grid/tests/reference/idw_adw_reference.py
python3 crates/engine-station-grid/tests/reference/loo_reference.py
python3 crates/engine-station-grid/tests/reference/lapse_rate_reference.py
```

Unit tests in `src/` cover the grid geometry, the neighbour search and the
methods. `tests/integration.rs` grids `testdata/weather.csv` through the CSV
engine (maps in three CRSs, every EDR query type, bad configs) and in-memory
sources (`170,10,-170,20` across the antimeridian, a polar EPSG:3035 view,
late reports, a miss past the deadline). The Python script is an independent
IDW/ADW implementation; the test pins its output to 1e-4 °C.
`tests/residuals.rs` pins the leave-one-out residuals to
`loo_reference.py`, which applies that implementation to each slice
without the station, to 1e-9: the fixture's RMSE and worst-fitting station,
and an outlier in a synthetic lattice. It also covers the Features rules:
`datetime`, the 96-analysis cap, paging, sorting, filters, ids, and a
request that does not wait.
`src/engine/revision_tests.rs` covers revisions found by a request, the
round-robin re-checks of older cached times, and analysis times from the
source's own instants.
`tests/corrections.rs` covers the corrections over in-memory collections: a
two-station valley and the residuals of a four-station one, both pinned to
`lapse_rate_reference.py`; stated and terrain heights; the mask in maps
and EDR; per-parameter opt-outs; exclusions, also found by a re-check of an
older time; a terrain not yet sampled; the `170,10,-170,20` seam; and every
config error. The server's `station_grid_corrections_tests` wire GeoTIFF
terrain and mask fixtures (`testdata/station-grid-terrain`) through
`load_collections`.

Contributor rules for this crate are in [CLAUDE.md](CLAUDE.md).
