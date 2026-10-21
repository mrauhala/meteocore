# engine-querydata

Serves FMI QueryData (`.sqd`) model output through OGC API - EDR, WMS,
OGC API - Maps and OGC API - Tiles.

## What it solves

QueryData is the binary data format of FMI's open-source SmartMet tools
(the newbase library). Model output, such as ECMWF or MEPS fields, is
converted into one `.sqd` file per model run. This engine serves a
directory of such files as they are, with no conversion or import step.
Each file is one model run, and the newest run is the default.

## How it works

- **Discovery.** A poll loop lists the collection's local directory every
  `poll_interval_secs` for `.sqd` files (any case), skipping hidden
  temporary names (`.name.sqd`).
- **Runs.** The last `max_runs` files in file-name order are retained. Each
  is keyed by the origin (analysis) time in its header. That time is the
  run's reference time: its EDR instance id and WMS `DIM_REFERENCE_TIME`.
- **Parsing.** The engine reads the text header (parameters, grid area,
  levels, timesteps, origin time), checks that the data block has the size
  the header implies, and memory-maps the file. Values are read straight
  from the mapping as little-endian `f32`; `32700` means missing.
- **Refresh.** A poll reuses every file it has already parsed (matched by
  path) and swaps the run set atomically only when the set of files
  changes. An empty or unreadable directory, or one holding only unloadable
  files, keeps the current runs.
- **Grids.** Lat/lon, rotated lat/lon, stereographic and Lambert conformal
  conic (LCC) areas, on the figures newbase computes them on: an LCC grid
  on the sphere whose radius the file states, a stereographic one on
  newbase's 6 371 220 m sphere, true at its true latitude. The grid transform and WGS84 extents are computed once per
  file (an extent reaches a pole the grid holds and is `west > east`
  across the antimeridian, #1194), and so is the native grid (#1237): the cells around the corner
  points, in the area's own CRS, which the collection's
  `meteocore:nativeGrid` member describes as WKT2 when it is not lat/lon.
- **Sampling.** Bilinear interpolation between the four surrounding grid
  points, falling back to the nearest point when one of them is missing.
  On a lat/lon grid that spans all 360° of longitude, the first column
  follows the last, so EDR and maps interpolate across the seam.
- **Map rendering.** A lat/lon grid under geographic (CRS:84, EPSG:4326) or
  Web Mercator output is sampled per pixel, since it needs no projection.
  Every other pairing takes its output-to-grid pixel map from the shared
  `ds_core::resample::SourceGrid`: a coarse projection grid, bounded by a
  projected grid's footprint. A global lat/lon grid's map is periodic in
  longitude, so polar and EPSG:3035 views stay registered.
- **Names.** EDR lists each parameter by its full descriptor, such as
  `2 Metre Temperature (2t)`, and also accepts the short name in the
  trailing parentheses (`2t`) in `parameter-name`; the coverage is keyed by
  the name asked for. The map APIs use the short name, or the full
  descriptor when there is none, so WMS GetFeatureInfo on `<collection>/2t`
  reads the same parameter.
- **Units.** A `.sqd` file has no unit strings. Each parameter's unit
  comes from its FMI parameter number, and values are served as stored:
  pressure (1) in hPa, temperature and dew point (4, 10) in °C, relative
  humidity and cloud covers (13, 79, 273–275) in %, wind direction (20)
  in °, wind speed, u, v and gusts (21, 23, 24, 417, 467) in m s-1, and
  precipitation amounts (50, 353, 354) in mm. Any other number reports no
  unit. EDR metadata and the map parameter list carry the unit, so the
  built-in default palettes, which key on name and unit, apply.

## What it serves

| API | Query | Result |
|---|---|---|
| EDR | `position` | a time series at the point (`PointSeries`) |
| EDR | `area` | a CRS84 `Grid` over the polygon's bounding box; cells outside the polygon are null |
| EDR | `radius` | the circle as a polygon, answered as `area` |
| EDR | `trajectory` | values along a WKT `LINESTRING` (also `Z`, `M`, `ZM`) |
| EDR | `/instances` | one instance per retained run |
| WMS, Maps, Tiles | map | one layer per parameter: WMS child layer `<collection>/<name>`, Maps and Tiles `parameter-name`; `wms_parameter` is the default |

- EDR without an instance, and WMS without `DIM_REFERENCE_TIME`, use the
  latest run. An EDR instance that is not retained is a 404; a WMS
  `DIM_REFERENCE_TIME` that is not retained is an `InvalidDimensionValue`
  error. Maps and Tiles always render the latest run.
- EDR `datetime` selects the run's timesteps inside the interval, both ends
  included; without it every timestep is returned. A window with no
  timestep is a 404.
- EDR `parameter-name` ignores case. An unknown name is a 400.
- The area grid uses the source's mean cell size, coarsened to at most 256
  cells per axis. A response holds at most 1 000 000 values across
  timesteps, cells and parameters; more is a 400. The grid has a `t` axis
  when more than one timestep is selected. A polygon whose bounding box
  misses the grid's extent is a 404.
- A trajectory is sampled about once per grid cell crossed, with the
  position query's interpolation. A `Z` coordinate is ignored.
- A map request draws the timestep nearest the requested time; without a
  time, the run's last timestep.
- WMS GetFeatureInfo reads values through the position query.
- Collection metadata (parameters, extents, timesteps) describes the latest
  run. Each instance lists its own valid times.
- `/health` reports `data_age_secs` as now minus the newest run's origin
  time, and `poll_age_secs` as the time since the engine loaded or a poll
  last found a loadable run.

Output formats and other engines' query types are on the EDR status page:
[Per-engine query-type matrix](../api-edr/README.md#per-engine-query-type-matrix).

### Derived wind

A u/v pair gains a derived wind speed on every API and, in EDR only, a wind
direction. Pairs are found by FMI parameter number (23 `WindUMS`,
24 `WindVMS`) or by short name in a known vocabulary, such as ECMWF
`10u`/`10v`. The derived parameters take that vocabulary's names:
`WindSpeedMS` and `WindDirection`, or `10si` and `10wdir`. QueryData does
not state the components' frame, so the engine follows newbase's
convention that u and v are relative to the grid. Direction is therefore
derived only on lat/lon areas; rotated lat/lon, stereographic and LCC areas
get speed only. `derive_wind = false` turns this off.

## Configuration

```toml
[[collections]]
id = "ecmwf-kenya"
title = "ECMWF Kenya Surface"
description = "ECMWF surface forecast for the Kenya region"
engine_type = "querydata"
data_path = "testdata/ecmwf-kenya"   # required: a local directory of .sqd files
apis = ["edr", "wms", "maps", "tiles"]
# derive_wind = false                # opt out of derived wind

[collections.querydata]              # optional; every field has a default
wms_parameter = "2t"                 # default: the file's first parameter
poll_interval_secs = 60              # default 30, minimum 1
max_runs = 4                         # default 4; 1 keeps only the latest run
```

- Without a `[wms]` block, each parameter layer takes the built-in default
  style its name and unit select, and the collection layer draws
  `wms_parameter` in that parameter's style.
- `data_path` is a local directory. A relative path resolves against the
  server's working directory.
- `wms_parameter` matches the full descriptor, the short name or the FMI
  parameter number, ignoring case. A value that matches nothing selects the
  first parameter.
- `apis` accepts `edr`, `wms`, `maps` and `tiles` (default `["edr"]`).
- Runnable examples:
  [`collections.d/ecmwf-kenya.toml`](../../collections.d/ecmwf-kenya.toml)
  (lat/lon) and
  [`collections.d/meps-northeurope.toml`](../../collections.d/meps-northeurope.toml)
  (LCC).
- `server --auto-collections <DIR>` turns `<DIR>`, or each of its immediate
  subdirectories, that holds `.sqd` files into one such collection with
  default settings, serving EDR, WMS, Maps and Tiles.

## Limits

- Local directories only; no object store or HTTP source.
- Binary, little-endian gridded files of format version 6 or later.
  Station (point) QueryData, text-mode data, big-endian files and other
  area types fail to load.
- Only the first vertical level is served. There is no vertical dimension.
- A parameter number without a known unit reports none, so unit-keyed
  default palettes do not match it: set its palette in `[wms]`. Maximum
  and minimum temperature (5, 6) and precipitation rate (49) are among
  them, since FMI's converters store each in more than one unit.
- The run window is chosen by file name, so names must sort in origin-time
  order, as timestamped names do. Of two files with the same origin time,
  the later name is kept.
- An unloadable file is skipped but still takes a place in the `max_runs`
  window.
- Files are memory-mapped and matched by path. Publish each run under a new
  name, written under a temporary name and then renamed. A file rewritten
  in place under the same name is not re-read.
- A directory with no loadable `.sqd` file at startup fails the collection
  in `/health`.
- No EDR `locations`, `cube` or `corridor` query.

## Testing

```sh
cargo test -p engine-querydata
# render timing, grid path against per-pixel projection:
cargo test --release -p engine-querydata meps_render_timing -- --ignored --nocapture
```

The tests are unit tests inside the crate:

- `src/parse.rs`: header parsing, grid points checked against PROJ,
  parameter lookup.
- `src/units.rs`: the parameter-number unit table.
- `src/engine.rs`: EDR position, area and trajectory, map tiles, runs and
  instances, skipping temporary names, data age.
- `src/engine/footprint_tests.rs`: the projected map path, world copies and
  the antimeridian.
- `src/engine/global_tests.rs`: a global lat/lon grid over the pole and
  across the seam.
- `src/engine/sampler_tests.rs`: the engine's render against the shared
  `ds_core::resample::SourceGrid` sampler.
- `src/engine/units_tests.rs`: units in EDR metadata, coverages and map
  parameters.
- `src/engine/wind_tests.rs`: derived wind, on fixture copies whose
  descriptors are renamed to u/v components.

Both fixtures are `qdcrop` subsets of FMI open data, described in
[`testdata/QUERYDATA_FIXTURES.md`](../../testdata/QUERYDATA_FIXTURES.md):

- `testdata/ecmwf-kenya/`: ECMWF lat/lon surface fields, 3 parameters,
  16 × 21 grid, 4 timesteps.
- `testdata/meps/`: MEPS LCC surface fields, 2 parameters, 3 timesteps.

Contributor notes: [CLAUDE.md](CLAUDE.md).
