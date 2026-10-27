# engine-grib

Serves GRIB2 numerical weather prediction (NWP) forecasts through OGC API -
EDR, OGC API - Maps, OGC API - Tiles and WMS 1.3.0.

## What it solves

NWP centres publish each model run as one large GRIB2 file per forecast
step, each holding hundreds of fields, often with a small index sidecar
that gives every message's byte offset. One field at one step should not
cost a whole-file download. This engine builds its catalogue of runs, steps
and fields from the sidecars alone and fetches single messages with
byte-range reads when a request needs them. One collection (or one per
level type) exposes every parameter; each retained run is an EDR instance.

## How it works

- **Discovery.** A poll loop on the server's background runtime lists the
  sidecars: for a remote source, the runs of today and yesterday (UTC)
  from `prefix_pattern`, newest first, until `max_runs` runs have files;
  for `data_path`, one fixed prefix. Older runs are re-listed hourly to
  catch late steps. Temporary names and hidden folders are skipped. New
  sidecars are fetched eight at a time; the data file is the index path
  with `index_suffix` replaced by `data_suffix`, and files of one run and
  step are merged.
- **Metadata.** Each scan reads the headers of up to 32 messages, eight at
  a time, without unpacking values: the WMO triple, grid geometry and u/v
  frame give labels, units and the spatial extent.
- **Reads.** A query fetches one message by byte range and decodes it to an
  `f32` grid, kept in an LRU cache (`grid_cache_mb`). An optional cache of
  compressed messages (`message_cache_mb`) saves the download on a
  decoded-grid miss. Both caches are per source, shared by its level views.
- **Long EDR windows.** An EDR query (position, trajectory, area, radius,
  cube) whose fields, at the grid's decoded size, add up to more than
  `grid_cache_mb` reads them without keeping them: inserting them would
  evict its own fields before any reuse, and every grid the maps keep
  there. It still uses a grid already in the cache, waits for one another
  request is decoding, and fills the message cache. A window that fits is
  cached as before, so a repeat is all hits. To make repeats of long
  windows cheap, set `message_cache_mb`: a compressed GFS 0.25° message is
  about 0.6 MB against 4 MB decoded, an IFS 0.1° one about 1.3 MB against
  26 MB.
- **Point reads of CCSDS fields.** A position query on a template 5.42
  field (ECMWF IFS and AIFS) decodes only the blocks of 4096 values that
  hold its points' neighbours, bit for bit the full decode's values. The
  first read of a message decodes it once, building no grid, and indexes
  where each block starts (about 1.2 KB at 0.25°, 6.5 KB at 0.1°;
  `point_index_cache_mb`). A grid already in the cache is used as it is;
  nothing is inserted. Other packings, bitmaps, and fields the grid cache
  would keep while no message cache is set are decoded in full. Without a
  message cache, or one too small for the query's messages, the query also
  fetches only bytes: one range GET of an indexed message's needed blocks,
  else a prefix predicted from an indexed message of the same parameter,
  plus at most one continuation. About 15 % of a message at 60°N, 80 % at
  34°S, a few kilobytes once indexed.
- **Point reads of GFS fields.** Template 5.3 (complex packing with
  spatial differencing, all of GFS 0.25°) reads alike, by grid rows: every
  value is a running sum of the differences before it, so the index keeps
  the decoder's state at each row's first value and the message's group
  descriptors: about 110 KB per message at 0.25°, against 1–6 KB for a
  CCSDS index. A point then decodes two rows, bit for bit the full decode's
  values. Partial fetches: a prefix
  through the point's last value, about 24 % of a message at 60°N, 39 % at
  35°N and 78 % at 34°S, then one range GET of the point's rows, 1.3–2.3 KB.
  A field with a bitmap (soil and other land fields) places its values
  through the bitmap, which precedes the data: it always reads a prefix,
  about 55 % of SOILW at 60°N.
- **Sampling.** Points, paths and map pixels are interpolated bilinearly in
  `f64`, then converted to display units; map tiles store `f32`. Projected
  output CRSs map pixels through a coarse projection grid. An EDR query
  reads each field once, sampling every point from it, up to 16 fields at a
  time: as many as keep 256 MiB of decoded values in flight, at least four.
  A 0.25° global grid (4 MB a field) reads 16 at a time, a 0.1° one (26 MB)
  ten. Before the grid's geometry is known, four.
- **Units.** The source unit comes from a built-in subset of WMO Code
  Table 4.2 plus ECMWF local numbers (cloud cover, land-sea mask,
  precipitation and snow in metres of water). Display conversions: K → °C
  (temperature differences stay K), Pa → hPa, kg m⁻² → mm, kg m⁻² s⁻¹ →
  mm/h (× 3600, precipitation rates such as `PRATE` and ECMWF `tprate`),
  metres of water → mm, proportion → %; other units pass unchanged.
  Geopotential (`z`, `GP`) stays in m² s⁻²: its height is another
  quantity. An unknown triple is labelled by its short name, unitless.
  A rate accumulated over a time interval (template 4.8 or 4.11, Code
  Table 4.10 type 1) is the amount, as eccodes reads it: AIFS `tp`, `cp`
  and `sf` (0/1/52, 0/1/37, 0/1/53) are Total precipitation, Convective
  precipitation and Snowfall water equivalent in kg m⁻² → mm, while
  IFS `tprate`, the same 0/1/52 unprocessed, stays a rate in mm/h.

## What it serves

`GribEngine` implements `EdrEngine`, `MapEngine` and
`ds_core::wind::WindSource`. Allowed `apis`: `edr`, `wms`, `maps`, `tiles`.
The [EDR status page](../api-edr/README.md#per-engine-query-type-matrix)
compares engines.

| API | Query | Notes |
|---|---|---|
| EDR | `position` (also `MULTIPOINT`) | `PointSeries`; one `VerticalProfile` per step when several levels are selected |
| EDR | `area`, `radius` | native grid nodes over the polygon's bounding box; nodes outside the polygon are null |
| EDR | `trajectory` | WKT `LINESTRING`, `Z`, `M`, `ZM`, densified at the grid spacing |
| EDR | `cube` | pressure and model-level views only; `[t, z, y, x]`, nearest native node |
| EDR | `/instances` | one per retained run |
| WMS, Maps, Tiles | raster | one layer per parameter (`parameter-name`; WMS `{collection}/{parameter}`); WMS GetFeatureInfo runs the EDR position query |

- **EDR time.** `datetime` matches steps exactly: an instant without a step
  is a 404, an interval takes every step inside it. Without `datetime`, a
  position returns every step of the latest run, an area, radius or cube
  its last step. Runs carrying every named parameter are preferred. A
  pinned `reference_time` never falls back to another run.
- **EDR parameters.** Without `parameter-name`, a pressure or model view
  returns all its parameters, a single-level view the products of the
  selected steps, and a collection without `level_types` only their
  near-surface products.
- **Map time.** `time` snaps to the nearest step of the newest run whose
  valid-time range covers it; a time outside every run is an error (400 in
  Maps and Tiles). No time draws the newest run's last step. A parameter
  present at only some steps (hour-window aggregates, maxima) has its own
  time axis and renders from the run and step that carry it.
- **Levels.** On pressure and model views, EDR `z` and Maps/Tiles
  `elevation` (WMS `ELEVATION`) select levels exactly. A map without one
  draws the selected run's largest level value.
- **Wind.** The engine reports each u/v message's triple, level and earth-
  or grid-relative flag. The server's shared wrapper derives speed (`10si`,
  `WIND`, …) on every API and direction in EDR, unless the collection sets
  `derive_wind = false`.
- **Geopotential height.** A view with geopotential (0/3/4) in m² s⁻² and
  no geopotential height (0/3/5) at that level gets one derived, z ÷ g₀ in
  gpm, on every API: `z` → `gh`, wgrib2 `GP` → `HGT`. On a single-level
  view `z` is the surface geopotential and `gh` the terrain height. The
  ECMWF pressure levels ship `gh` and get no second one.
  `derive_geopotential_height = false` opts out.
- **Relative humidity and dew point.** Where a view has temperature (0/0/0)
  and a dew point (0/0/6), relative humidity (0/1/1) or, on a pressure
  view, specific humidity (0/1/0) at a level, but not the other quantity,
  it is derived on every API from the served units: `2t`/`2d` → `2r`;
  pressure-level `t`/`q` → `r` and `dpt`, from each level's pressure;
  wgrib2 `TMP` with `DPT`, `RH` or `SPFH` → `RH` or `DPT`. Relative
  humidity is over liquid water. GFS's 2 m `RH` and `DPT` are native and
  get no second one; specific humidity at a single level gives nothing (it
  would need the surface pressure). `derive_humidity = false` opts out.

## Configuration

```toml
[[collections]]
id = "ecmwf-ifs"
title = "ECMWF IFS Global Forecast"
description = "IFS deterministic forecast (0.25 deg) from ECMWF open data"
engine_type = "grib"
apis = ["edr", "wms", "maps", "tiles"]

[collections.grib]
endpoint = "https://s3.eu-central-1.amazonaws.com"
bucket = "ecmwf-forecasts"
prefix_pattern = "%Y%m%d/{run}z/ifs/0p25/oper/"
max_runs = 1
level_types = ["single", "pressure"]
parameters = ["2t", "10u", "10v", "msl", "tp", "t", "z"]
```

| Field | Default | Meaning |
|---|---|---|
| `data_path` | – | Local directory, or `s3://` / `http(s)://` URL, holding data files and sidecars. Excludes `endpoint`/`bucket`. An `s3://` URL uses AWS credentials from the environment. |
| `endpoint`, `bucket` | – | Remote store, both required. Requests are unsigned, so the bucket must allow anonymous reads. An AWS or CloudFerro endpoint is read as S3, any other as plain HTTP. |
| `prefix_pattern` | – | Required with `endpoint`: strftime date fields plus `{run}` (each run hour, two digits). `%H` and braces other than `{run}` are rejected. With `data_path`: optional literal sub-prefix. |
| `run_hours` | `[0, 6, 12, 18]` | Hours substituted into `{run}`, each 0–23. |
| `index_format` | `"ecmwf-json"` | `"ecmwf-json"` (JSON lines, `_offset`/`_length`) or `"wgrib2"` (colon-separated text). |
| `index_suffix` | `".index"` | Suffix of index files. |
| `data_suffix` | `".grib2"` | Replaces `index_suffix` to name the data file; `""` for GFS names. |
| `filename_contains` | – | Keep only index paths containing this substring. |
| `parameters` | all | Parameter filter; set it for wgrib2 sources (a GFS 0.25° file holds about 700 messages). A wgrib2 base name (`APCP`) also selects its window keys (`APCP_acc_6h`). |
| `level_types` | – | Non-empty list of `"single"`, `"pressure"`, `"model"`. |
| `poll_interval_secs` | `600` | Poll cadence, > 0. |
| `max_runs` | none | Keep the newest N runs; unset keeps every run found. |
| `time_window` | none | ISO 8601 duration relative to the current time: `-PT12H` keeps steps valid in the last 12 h, `P5D` in the next 5 days. |
| `grid_cache_mb` | `256` | Decoded-grid cache per source; `0` disables it. An EDR query over more fields than it holds reads around it. |
| `message_cache_mb` | `0` | Compressed-message cache per source; `0` disables it. |
| `point_index_cache_mb` | `16` | Point-index cache per source, one shard, for CCSDS and template 5.3 fields; `0` decodes every position field in full. An index is 1–6 KB for CCSDS and about 110 KB for GFS 0.25°, so 16 MiB holds about 145 GFS messages. |

With `level_types`, the collection id names a source, and the server
registers one view per enabled family once data for it is found:

| View | Fields | Vertical axis |
|---|---|---|
| `{id}-single` | surface, height above ground, MSL, named levels (tropopause, cloud ceiling, …) | none |
| `{id}-pressure` | isobaric (`pl`) | pressure, hPa |
| `{id}-model` | model, hybrid and sigma levels (`ml`) | level number |

Without it, one collection has no vertical axis and serves each short name
at one canonical level: the lowest height above ground up to 100 m, then
surface, then mean sea level. `/metrics` reports `grid_cache_*`,
`grib_message_cache_*` and `grib_point_index_cache_*` per source, and
`grib_point_fields_total` by `path`: `partial` through the point index,
`full` from a decoded grid. `grib_point_fetches_total` and
`grib_point_fetch_bytes_total` count the point path's GETs and bytes by
`mode`: `whole`, `prefix`, `range` or `continuation`.

Runnable examples read public buckets
([`ecmwf-ifs`](../../collections.d/ecmwf-ifs.toml),
[`ecmwf-aifs`](../../collections.d/ecmwf-aifs.toml),
[`noaa-gfs`](../../collections.d/noaa-gfs.toml) with wgrib2) or committed
fixtures ([`split-levels`](../../testdata/grib-local/split-levels.toml),
[`derived-wind`](../../testdata/grib-arpege-wind/derived-wind.toml)).
`server --auto-collections <DIR>` serves `.grib2`/`.grb2` files that have
`.index` or `.idx` sidecars.

## Limits

- GRIB2 on regular lat/lon grids (template 3.0) only. Gaussian, rotated
  and projected grids and staggered scan modes are rejected.
- Packing: simple (5.0), complex (5.2, 5.3), CCSDS (5.42), run-length
  (5.200). JPEG 2000 (5.40), PNG (5.41) and others are not supported.
- The engine never builds indexes: a data file without a sidecar is not
  seen. Forecast steps are whole hours. An index is read once per path, so
  a file rewritten in place under the same name is not picked up.
- Without `max_runs` or `time_window` no run is evicted. A parameter
  dropped from later runs stays listed until its last run is evicted.
- Soil, isentropic and fractional levels belong to no level view; model
  levels are not converted to height or pressure.
- wgrib2 hour-window accumulations and averages are keyed by duration
  (`APCP_acc_6h`, `DSWRF_avg_6h`) at the window end, not divided by its
  length. A key repeated in one index serves its first record and logs one
  summary warning per scan.
- No EDR `locations` query. An area is answered at native resolution, up
  to 1 000 000 values (steps × levels × cells × parameters), else a 400. A
  cube bbox crossing the antimeridian (west > east) is a 400.

## Testing

```bash
cargo test -p engine-grib
cargo test -p server --test grib_cube_edr --test grib_parameter_times
cargo test -p engine-grib -- --ignored --nocapture  # latency replays, timings
cargo run --release -p engine-grib --example bench_grid_cache  # grid memory
```

- `testdata/grib-local`: one global 0.25° ECMWF message (specific humidity
  at 150 hPa) with its JSON-lines index.
- `testdata/gfs`: real GFS wgrib2 sidecars (f000, f003, f006, f384) only.
- `testdata/grib-arpege-wind`: a Météo-France ARPEGE subset with 10 m u/v
  and native speed and direction, to check derived wind.
- `src/test_support` builds synthetic messages.

Building needs CMake, a C compiler and libclang: `libaec-sys` compiles
libaec for CCSDS unpacking. Contributor notes: [CLAUDE.md](CLAUDE.md).
