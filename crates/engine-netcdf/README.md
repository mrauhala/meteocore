# engine-netcdf

Serves gridded CF-NetCDF forecasts and analyses through WMS 1.3.0, OGC API -
Maps, OGC API - Tiles and OGC API - EDR (`engine_type = "netcdf"`).

## What it solves

Weather models such as MetCoOp MEPS publish each run as a family of
CF-NetCDF files, often one per lead time, each with many variables and
over 100 MB. Some publishers expose them only on an HTTP file server that
cannot be listed, or only through OPeNDAP, as thredds.met.no serves MEPS
since 2026-10-22. This engine serves such a family as map layers and EDR
queries in place: a request reads one 2-D field, and no file is converted
or downloaded whole.

## How it works

- **One collection is one file family.** Every file carries the
  collection's variables on one grid. The first file opened fixes the
  layout (variables, grid, levels, units); a later file that differs is
  skipped with a WARN.
- **Discovery.** A local `data_path` is listed non-recursively: names
  matching `filename_template`, else every `*.nc`, skipping temporary
  upload names. An `http(s)://` `data_path` is probed by name: each poll
  HEADs the first lead (the sentinel) of every run in `time_window` not
  seen yet, and the other leads once it appears. With a `catalog_url`, the
  server's THREDDS catalogs answer instead of the HEADs (below). Each poll
  opens new and changed files and keeps them open: the newest `max_runs`
  runs, at most `max_files` files. Requests never open a file.
- **Time comes from the file, never its name:** valid times from the
  `time` axis, the model run from `forecast_reference_time`. Files without
  a run form one timeline without runs.
- **A request reads one field:** one variable at one step and level, by
  hyperslab, decoded to `f32` (NaN for nodata, north-up) into a field
  cache shared by every NetCDF collection, then resampled to the output
  grid. Concurrent renders of one cold field decode it once.
- **Remote files are read by HTTP range.** Opening a file reads its first
  1 MiB plus a few 64 KiB blocks of its chunk indexes; a field is one GET
  of its compressed chunk. Both are cached. Every GET is conditional on
  the version opened: a request that finds the file replaced gets a 503,
  and a later poll opens the new version. At most 8 requests go to one
  host at once, over every collection on it; polls use at most 2 of them.
  A host answering 429 gets fewer: the 8 halve per 429 and grow back by
  one per quiet 30 s, and its `Retry-After` pauses every collection on it
  (ds-storage's host back-off).
  A retried request's log line names its collection, host and file
  (ds-storage's `storage` span).
- **OPeNDAP** (`access = "opendap"`, #1264). Remote files read through a
  DAP2 server, `data_path` its directory (`…/thredds/dodsC/…`), found
  through the THREDDS catalogs (`catalog_url`, required) by their
  `OpenDAP` service. Opening a file reads its `.dds` and `.das` (the
  variables, shapes and attributes, which build the same layout a NetCDF
  file does) and one `.dods` of its scalars and 1-D coordinates: three
  requests, about 158 KB for a MEPS sfc file. A field is one `.dods` of
  its exact hyperslab (time, level, the whole y and x), decoded from XDR
  in its stored type: 4.06 MB for a MEPS field, since the server sends it
  uncompressed (THREDDS ignores `Accept-Encoding` there), against a 1.5 MB
  compressed chunk read by range. The reply is kept in the chunk cache,
  single-flight, as a chunk is. An EDR or GetFeatureInfo query reads only
  the window of each field it samples (#1171): one `.dods` of the cells
  around its points or area cells, a 2 × 2 window of a MEPS field being
  428 bytes against 4.06 MB (0.10 s against 0.25 s from a server's
  network, 2026-10-24), so a 67-lead position series is about 29 KB per
  variable instead of 272 MB. Windows are kept in the chunk cache under
  their own constraints; the values are the ones the whole field gives. A
  window over half the field reads the field whole, and a field a render,
  a prewarm or another query left cached whole is sampled whole, without a
  request. Maps read whole fields. Replies stream chunked, without
  `Content-Length`, and are read whole; every request takes the host's
  request limit and back-off. Nothing is HEADed: a file's version is its
  catalog entry's, and a file the catalog shows changed is opened again.
  THREDDS sends no `Last-Modified` with DAP replies, so a file replaced
  between polls is found by the poll only; a 404 on a read is a file gone,
  and a server that sends `Last-Modified` has each reply checked against
  the `.dds`'s, both as a replaced file is handled. No DAP4, and no
  OPeNDAP for local files.
- **THREDDS catalogs** (`catalog_url`, #1170). The template fields still
  name the files to pick; the `catalog.xml` of each run directory a poll
  checks says which exist, with a size and a modification time. A poll
  walks to the directory from `catalog_url` through the `catalogRef`s
  leading to the runs in `time_window` only, at most 64 catalogs, and
  reads it directly afterwards. A file settles on the catalog's size and
  date as it would on a HEAD's, and a run is published as by probing. A
  catalog rounds sizes (`167.8 Mbytes`), so each file listed is HEADed
  once for its exact size, which the range reads check. Catalogs are
  read whole although THREDDS streams them chunked, without
  `Content-Length`, at most 16 MiB each. Catalogs and HEADs take the
  host's request limit and back-off like every request.
- **Run and time selection.** A pinned run is exactly that run (404 when
  absent). Otherwise a map draws from the newest run whose steps span the
  requested time, at the nearest step; without a time, the newest run's
  last step.

## What it serves

| Trait | APIs | Notes |
|---|---|---|
| `MapEngine` | WMS, Maps, Tiles | One layer per variable (WMS child layers `<collection>/<variable>` when there are several, Maps/Tiles `parameter-name=<variable>`), and one per per-step amount (below); a request naming no variable draws the first. Runs are WMS `DIM_REFERENCE_TIME` values. |
| `EdrEngine` | EDR, WMS GetFeatureInfo | `position` (one or several points), `area`, `radius`, `trajectory` (`LINESTRING`, `Z`, `M`, `ZM`) and `cube`; each run is an instance. No `locations`. GetFeatureInfo reads the run, step and level the map drew. |
| `WindSource` | all of the above, through the server's derived-wind wrapper | Facts for the wind speed derived from u/v components (below). |

**Data it reads:** NetCDF-4 (HDF5) and classic CDF-1/2/5 files. A
variable has `(y, x)` as its last two axes: 1-D latitude/longitude, or
1-D projected x/y in metres or kilometres with a CF `grid_mapping`
(below), or 2-D latitude/longitude (a curvilinear grid, below); a
projected grid is described in its own CRS, as WKT2, by the collection's
`meteocore:nativeGrid` member (#1237). It needs a
time axis; an ensemble axis is read at `member`; every other axis has
size 1, except one pressure axis. Rows stored south to north are drawn
north-up. CF `scale_factor`/`add_offset`, `_FillValue`, `missing_value`,
NaN, and netCDF's default float fill (when no `_FillValue` is set) are
applied.

**Grid mappings.** `lambert_conformal_conic` (one or two standard
parallels, on a sphere or WGS84), `geostationary` (x/y may also be a scan
angle in radians or microradians), `polar_stereographic` (scaled by
`standard_parallel` or `scale_factor_at_projection_origin`; given both,
they must agree), `stereographic` at any origin (scaled by
`scale_factor_at_projection_origin`), `lambert_azimuthal_equal_area` and
`transverse_mercator`. The mapping must state its earth figure; the last
four are computed on it, a sphere or any ellipsoid, such as a polar NWP
grid's sphere, NSIDC's Hughes 1980 or Airy 1830 (#1192). Projected x/y in
kilometres are scaled to metres, and so are the false easting and
northing, which CF states in the coordinates' units (#1194). A
grid that holds a pole, such as an Arctic polar stereographic grid,
advertises an extent reaching the pole, and one across the antimeridian
an extent `west > east`.

**Curvilinear grids** (#1174). A variable whose CF `coordinates`
attribute names 2-D `latitude(y, x)` and `longitude(y, x)` variables,
known by their `standard_name` or their `degrees_north`/`degrees_east`
units, never their names, and whose `(y, x)` have no 1-D
latitude/longitude, nor projected x/y with a `grid_mapping`, is on a
curvilinear grid, as ocean, sea-ice and regional products are. Where a
file has both, the axes win and the 2-D arrays are not read: MEPS
carries them beside its Lambert axes. Each file's two arrays are read
whole when it is opened: they are its grid, and a later file whose
nodes differ (by a thousandth of a node's spacing) is skipped. The
collection builds a k-d tree over the nodes once (17 bytes per node).
Every point is then placed by an inverse lookup, made only at a map's
coarse projection grid nodes and once per EDR point: the nearest node,
then the fractional position inside a cell around it, so maps
interpolate smoothly between lookups. Both work on the sphere, so the
antimeridian and the poles need nothing special. Values are sampled as
on any grid (bilinear, the nearest cell at a nodata neighbour, the half
cell past the outer cell edges); farther off the grid is empty, never the
edge cell drawn on. The extent comes from the outer cell edges, reaching
a pole the grid holds. Rows are drawn as stored. The grid has no CRS:
`native_crs` is `curvilinear`, with no `storageCrs` and no native grid
description. Grid-relative wind components give nothing on it (the axes'
directions on the ground are unknown); earth-relative ones give speed and
direction.

**Display units.** Each variable's CF `units`, in any spelling, picks the
unit it is served in: K as °C, Pa as hPa, kg m-2 as mm, kg m-2 s-1 as mm/h;
geopotential's m2 s-2 as stored. CF's dimensionless `1` is served as % when
the variable's `standard_name` is a fraction: `relative_humidity`, or a name ending in
`_area_fraction` or `_area_fraction_in_atmosphere_layer`, such as
`cloud_area_fraction`. Any other unit, such as m/s or the `1` of
`specific_humidity`, is served as stored. Values are sampled from the stored
field and converted after, so a map pixel, an EDR value and GetFeatureInfo
at one point agree. Layers without a `[[wms.parameters]]` style take the
built-in default their name and served unit select.

**Pressure levels.** A dimension whose coordinate has pressure units
(`Pa`, `hPa`, `mbar`, …) and is not a parametric coordinate is the
collection's vertical axis, in hPa, bottom first. WMS `ELEVATION` and
Maps/Tiles `elevation` must name a level exactly; a map without one draws
the bottom level. EDR `z` drops levels the collection lacks (a 400 when
none remain); without `z` it reads every level. A collection has one
vertical axis, so a pressure-level family (MEPS `pl`) is a collection
apart from its surface family (MEPS `sfc`).

**EDR.**

- Without `datetime`, a position query is every step of the newest run,
  and an area is the step a map without `TIME` draws. With `datetime`,
  the newest run with a step in the window, preferring one that also
  spans the window's start; a window without a step is a 404.
- Points and area cells are projected onto the grid once, and sampled and
  converted as the map draws them. A point off the grid is null; a polygon
  outside the grid's extent is a 404.
- An area is a CRS84 grid over the polygon's bounding box at the source's
  mean cell size, at most 256 cells per axis; cells outside the polygon
  are null. `radius` is answered as an area.
- A trajectory is densified along great circles to about one sample per
  source cell (`ds_core::trajectory`), and each sample is read as a
  position query reads a point. A 2-D or Z path takes a position query's
  steps, one coverage per step; an M or ZM path reads the run that covers
  its earliest time, each sample at that run's nearest step. On pressure
  levels a Z path's samples take the nearest level, and a 2-D or M path
  gives one coverage per `z` level.
- A cube is a CRS84 grid over the bbox with a `t` axis (an area's steps)
  and, on pressure levels, a `z` axis (`z`'s levels, or every level). Its
  native cells are an area's without the 256 cap, bounded by the
  1 million value budget; `resolution-x`/`-y` place that many positions
  from edge to edge, and `resolution-z` positions take the nearest level.
  Every cell is sampled as a position query samples a point. A bbox off
  the grid is a 404.
- Each field is read once per query for all points, four at a time, and
  is not added to the field cache. A trajectory or cube reads only the
  fields it samples, and a field it samples nowhere on the grid is not
  read. Through OPeNDAP the read is the window of the field the query
  samples (see OPeNDAP above); the query cap still counts whole fields.

**Derived wind.** Components are identified by CF `standard_name`
(`x_wind`/`y_wind` and `grid_eastward_wind`/`grid_northward_wind` are
grid-relative, `eastward_wind`/`northward_wind` earth-relative), or, on a
variable without one, by a known name such as `u`/`v` (frame unknown).
`x_wind_10m` and `y_wind_10m` give `wind_speed_10m`; `x_wind_pl` and
`y_wind_pl` give `wind_speed_pl` per level. Speed is served on every API.
Direction is EDR-only and needs earth-relative components, or
grid-relative ones on a latitude/longitude grid. `derive_wind = false`
opts out.

**Derived geopotential height.** A variable whose `standard_name` is
`geopotential`, in m2 s-2, gives its height ÷ g₀ in gpm on every API,
unless the collection has a `geopotential_height` at that level: MEPS's
`geopotential_pl` gives `geopotential_height_pl`, per level.
`derive_geopotential_height = false` opts out.

**Derived relative humidity and dew point.** Variables identified by CF
`standard_name` at one level give the quantity the collection lacks there,
named by swapping the `air_temperature` token: `air_temperature` with
`dew_point_temperature` gives the relative humidity over liquid water, in
%; with `relative_humidity` the dew point, in the temperature's °C (MEPS's
`air_temperature_2m` → `dew_point_temperature_2m`, `air_temperature_pl` →
`dew_point_temperature_pl` per level); with `specific_humidity` on the
pressure axis both, from each level's pressure. `derive_humidity = false`
opts out.

**Per-step amounts.** A variable named `<name>_acc` in kg m-2 or mm,
MET Norway's accumulation since the run start (MEPS
`precipitation_amount_acc`, and `rainfall_`, `snowfall_` and
`graupelfall_amount_acc`), also gives `<name>`, the amount of each step:
the accumulation at the step less the accumulation at the run's step
before, clamped at zero, so a decreasing accumulation is 0. A run's first
step has no step before it and is its own accumulation (zero at the run
start). It is in mm, with the accumulation's standard name, and its title
names the step, `Precipitation amount (1 h accumulation)` for MEPS, or the
shortest and longest when the steps differ, as in a run missing a lead. It
has the accumulation's runs, steps and levels on every API, and takes the
per-step precipitation style (`precipitation`, 0–50 mm). Each of the two
fields is sampled and converted, and the samples differenced, so a map
pixel and an EDR value agree. In an EDR trajectory each sample takes its
own step's step before, both sampled at the sample (an `M` path's, in the
run the path reads); a cube is an area's. Each lead's field is read once: a
map's previous step is the next step's step before, from the field cache,
and an EDR query reads each lead once whatever needs it, the query cap
counting the steps before. A lead arriving late
inside a run, or removed from it, changes the amount after it and moves
the content version. Not derived: an `_acc` in another unit (clamping
would corrupt a signed accumulation such as net radiation), or one whose
`<name>` the files have. `derive_per_step = false` opts out.

## Configuration

```toml
id = "meps-netcdf-local"
engine_type = "netcdf"
apis = ["edr", "wms", "maps", "tiles"]   # default ["edr"]

[netcdf]
data_path = "testdata/meps-netcdf"
filename_template = "meps_sfc_{lead}_%Y%m%dT{run}Z.nc"
variables = ["air_temperature_2m", "x_wind_10m", "y_wind_10m"]
max_runs = 4
```

In `config.toml` the table is `[collections.netcdf]`. Unknown keys, and
URL-only fields on a local `data_path`, are a load error.

| Field | Default | Meaning |
|---|---|---|
| `data_path` | required | Local directory, or an `http(s)://` URL read by range (or an OPeNDAP directory with `access = "opendap"`) |
| `filename_template` | every `*.nc` | strftime name with `{run}` (run hour) and `{lead}` (two-digit lead); required for a URL |
| `variables` | every variable on the first file's grid | Variables served, in order; the first is the default layer |
| `titles` | `long_name`, else the name | Layer titles by variable name (a table) |
| `member` | `0` | Ensemble member read from files with an ensemble axis |
| `max_runs` | `4` | Newest model runs kept |
| `max_files` | `1024` | Most files kept open, over all runs |
| `poll_interval_secs` | `60` | Poll period in seconds |
| `prefix_pattern` | none | URL only: strftime path of one run's files, with `{run}` |
| `run_hours` | required | URL only: hours (0–23) runs start at |
| `leads` | required with `{lead}` | URL only: leads to probe; the first is the sentinel |
| `time_window` | required | URL only: how far back runs are probed, such as `-PT12H` |
| `min_age` | `PT10M` | URL only: a file is used once two polls see it unchanged, or at once when this old |
| `run_grace` | `PT30M` | URL only: a run missing leads is served without them after this |
| `prewarm_variables` | `[]` | URL only: variables whose newest-run fields each poll fetches ahead (bottom level) |
| `derive_per_step` | `true` | Serve `<name>`, the per-step amount of each `<name>_acc` accumulation in kg m-2 or mm (above) |
| `catalog_url` | none | URL only: the THREDDS `catalog.xml` of `data_path`'s directory (`…/thredds/catalog/<dir>/catalog.xml` for `…/thredds/fileServer/<dir>/` or `…/thredds/dodsC/<dir>/`). The catalogs say which files the template names exist, in place of a HEAD per file; the template fields stay required |
| `access` | `"http"` | URL only: `"http"` reads files by HTTP range; `"opendap"` through OPeNDAP (DAP2), `data_path` the OPeNDAP directory, and needs a `catalog_url` and a `data_path` without a query |

Runnable examples: `collections.d/meps-netcdf-local.toml` (MEPS surface
sample), `collections.d/meps-netcdf-pl-local.toml` (pressure-level
sample) and `collections.d/netcdf-curvilinear-local.toml` (a synthetic
curvilinear grid around the North Pole whose fields are each node's own
column and row). `meps-netcdf-opendap.toml.disabled` and
`meps-netcdf-pl-opendap.toml.disabled` in the same directory read these
families from MET Norway's public thredds.met.no through OPeNDAP, as it
serves MEPS since 2026-10-22, and state their traffic.
`meps-netcdf-thredds.toml.disabled`, `meps-netcdf-pl-thredds.toml.disabled`
and `meps-netcdf-thredds-catalog.toml.disabled` read them by HTTP range
from its fileServer, by HEAD probing or through its catalogs, which now
answers 403 for MEPS.

Process-wide environment variables, shared by every NetCDF collection:

| Variable | Default | Bounds |
|---|---|---|
| `MC_NETCDF_FIELD_CACHE_MB` | `256` | Decoded fields (~4 MB per MEPS field) |
| `MC_NETCDF_CHUNK_CACHE_MB` | `512` | Compressed chunks of remote files read by range (~1.5 MB per MEPS field), and the `.dods` replies of fields read through OPeNDAP (~4.06 MB) and of the windows EDR queries read (428 bytes at a point) |
| `MC_NETCDF_BLOCK_CACHE_MB` | `64` | 64 KiB metadata blocks of remote files read by range; at least 8 |
| `MC_NETCDF_READ_MEMORY_MB` | `1024` | Buffers of field reads in progress; a read that does not fit is a 503 |
| `MC_NETCDF_EDR_QUERY_MB` | `544` | Decoded bytes one EDR query may read (steps × levels × variables read × one field; a trajectory counts the (step, level) fields its path reads, a cube the levels its `z` positions take; a per-step amount adds its accumulation at the step before each step), checked before any read; over it a 400. Fits one variable, or a derived wind speed (both components), over a 67-lead MEPS run |

## Limits

- Only a pressure axis may have more than one level. A variable with
  several height-above-ground or hybrid model levels is skipped; listed
  in `variables`, it makes the file unservable.
- Every file must hold all the collection's variables on its grid; a
  file missing one is skipped. More than one ensemble member per
  collection is not supported.
- Not supported grids: other mappings (such as
  `rotated_latitude_longitude`), and a Lambert conic on an ellipsoid other
  than WGS84. A curvilinear grid has at most 16.8 million nodes (4096²),
  and its 2-D latitude/longitude are read whole each time a file is
  opened.
- Grid-relative wind components on a projected grid give speed only, and
  nothing on a geostationary or azimuthal equal-area grid, whose axes are
  not orthogonal on the ground, nor on a curvilinear grid, whose axes'
  directions are unknown. `valid_range` is not applied; non-standard
  calendars are refused.
- A cold remote field slower than the render deadline fails that render
  with a 503; the fetch still completes into the cache for the next one.
- `catalog_url` reads THREDDS catalogs whose tree mirrors the file server's
  (THREDDS `datasetScan`), datasets by their `urlPath` attribute and the
  catalog's `HTTPServer` service (`OpenDAP` with `access = "opendap"`);
  `access` elements are not read.
- OPeNDAP: DAP2 only; a dataset's variables are its top-level arrays,
  scalars and Grids (structures and sequences are not read); string
  variables are not read. A file read through OPeNDAP keeps no block
  cache: opened again (a reload, a closed file kept again), it reads its
  metadata again.
- Each kept file holds its parsed metadata in memory (about 820 KB for a
  221-variable MEPS surface file read by range, 254 KB through OPeNDAP)
  and, if local, one file descriptor.

## Testing

```bash
cargo test -p engine-netcdf
cargo test -p server --test netcdf_edr   # EDR responses vs. the schemas
```

Fixtures under `testdata/`, each with a README and the script that built
it, or its source: `netcdf-thredds-catalog/` (MET Norway's THREDDS catalog
of one MEPS day directory, as served, for the `catalog_url` tests);
`netcdf-opendap/` (what thredds.met.no's OPeNDAP served for two MEPS
files: DDS, DAS, the coordinates reply and a 4 × 4 hyperslab, for the
DAP2 format tests; the OPeNDAP collection tests serve the MEPS samples
through a local DAP2 server and compare them with the files read locally);
`meps-netcdf/` and `meps-netcdf-pl/` (real MEPS surface and
pressure-level files cropped to 11 × 10 points, two runs);
`netcdf-global/` (synthetic global grid without runs, packed and filled
values); `netcdf-pressure-pa/` (synthetic Pa pressure axis, earth-relative
wind); `netcdf-remote/` (synthetic MEPS-like files, served by a local HTTP
test server for the range-read tests); `netcdf-projections/` (synthetic
polar stereographic, LAEA, transverse Mercator and oblique stereographic
grids, the last on a sphere with x/y and its false origin in kilometres,
whose fields are
each cell's own projected x/y, which `tests/grid_mappings.rs` checks
against PROJ's `cs2cs`). `tests/trajectory_cube.rs` checks every
trajectory and cube value against a position query at the same point,
time, level and run. `tests/live_thredds.rs` reads a few
MEPS files from thredds.met.no and is ignored by default
(`cargo test -p engine-netcdf --test live_thredds -- --ignored`).
Contributor notes are in [CLAUDE.md](CLAUDE.md).
