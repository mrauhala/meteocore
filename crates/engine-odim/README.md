# engine-odim

Serves ODIM_H5 weather-radar data: 2-D composites (`engine_type = "odim"`)
and native polar volumes (`engine_type = "odim-volume"`).

## What it solves

European radar producers publish ODIM_H5, an HDF5 profile. A composite is
one projected grid per timestep, usually reflectivity. A polar volume is one
radar's stack of elevation sweeps, each carrying several moments
(reflectivity, Doppler velocity, dual-polarisation fields) in range ×
azimuth cells. This crate reads both directly from a local directory or an
object store, with the pure-Rust `hdf5-reader` (no `libhdf5`, no GDAL
preprocessing), and serves them through the map, EDR, Features and 3D Tiles
APIs. Polar data is resampled per request, so every sweep and moment stays
available.

## How it works

- **Catalog and poll.** A scan lists the source and builds a catalog of
  timestamped files. A background poll repeats it every
  `poll_interval_secs` and swaps the catalog atomically; a failed poll keeps
  the previous one. Temporary names (`.name.h5`, `*.tmp`, `*.part`) are
  skipped, and so are remote objects over 64 MiB. A polar source's catalog
  holds each volume's parsed metadata, never its pixels;
  `pvol_catalog_bytes` estimates its heap.
- **Composites (`OdimEngine`).** The filename template (or regex) gives
  each file's time. The newest file is decoded at startup for the grid, CRS
  and extent, so a source with no matching file fails to load. That grid is
  the collection's native grid (#1237), in the collection description's
  `meteocore:nativeGrid` member. Its CRS is an EPSG URI only when the
  projdef names GRS80 with exactly TM35FIN's (EPSG:3067) or ETRS89-LAEA
  Europe's (EPSG:3035) parameters, else its WKT2. The engine never claims a
  `storageCrs`. Decoded
  grids live in one byte-bounded LRU shared by all composite collections
  (`MC_ODIM_COMPOSITE_CACHE_MB`, default 2048, `0` disables).
- **Polar volumes (`PolarVolumeEngine`).** One source holds a radar
  network. The scan parses every `.h5` file's metadata (site, sweeps,
  moments) and groups volumes by the radar's ODIM node id (`NOD`). The
  server registers one collection per site, id `{id}-{nod}`, each served by
  a `PolarVolumeSiteView` over the shared catalog. At startup a remote
  source parses only the newest volume per radar; the first poll fills in
  the rest of the window.
- **Lazy polar pixels.** Pixel arrays are decoded per moment on first use
  into a shared byte-bounded cache (`MC_PVOL_PIXEL_CACHE_MB`, default
  1024). Each scan pre-decodes the default quantity, or the
  `prewarm_quantities`, of the lowest `prewarm_sweeps` sweeps of every
  volume it reads, so the first map of a new volume is a cache hit. The
  scan also records the byte ranges of the
  file each moment's decode reads, so a cold read fetches only those: for
  a base-sweep reflectivity map of a 20–40 MB volume, a few hundred
  kilobytes in four to seven GETs run at once. The whole file is read when
  a moment is not stored in chunks, or when the read would take more than
  ten ranges or more than half the file, as an all-quantity EDR profile
  does. An EDR query over several remote volumes reads the next three
  ahead while it samples one, by the same rule. A file in memory is
  decoded in place, copying only what the decoder reads. A request that
  reads several moments of one file reads it once, and concurrent requests
  that need the whole of the same remote volume share one download. A
  request waits for a download or decode that another request started only
  until its own deadline. A remote read the deadline cuts off goes on in
  the background, which decodes the moments the request wanted into the
  cache, at most four reads at once: the retry finds them there, or joins
  the download still running, instead of starting it again.
- **EDR read memory.** The volumes an EDR query (or a WMS GetFeatureInfo)
  reads are admitted by one process-wide budget, `MC_PVOL_READ_MEMORY_MB`
  (default 1024). Before a volume's file is read, the query reserves the
  bytes it fetches until they are decoded, and the moments it decodes, at
  four bytes a sample, until it moves on to the next volume; moments
  already cached cost nothing. A profile of every quantity of a 41 MB FMI
  volume (160 moments) reserves about 155 MB of moments plus the file. A
  query reading ahead reserves what each volume's read fetches, its byte
  ranges or the file, and its moments when the read starts, and reads no
  further ahead than the budget allows. A volume that does not fit is a
  503. Map, 3D Tiles and storm-cell reads are not counted.
- **Read errors.** A missing or undecodable polar volume reads as no data
  and is remembered as bad. A passed deadline or a storage failure fails
  the request, so no cache keeps an empty frame: a storage failure is a
  503, a deadline a 503 in the map and 3D Tiles APIs and a 504 in EDR. A
  volume the store refuses (401/403), or a local file the server may not
  read, fails as a storage failure and is not read again for a minute.
  `/metrics` counts the reads that read as no data in
  `pvol_pixel_read_failures_total` and those that failed their request in
  `pvol_pixel_read_aborts_total{reason="deadline"|"storage"}`. A
  composite that fails to load is a gap in an EDR series and an error for
  a map.
- **Maps** find each output pixel's source cell through a coarse
  projection grid, never a projection per pixel.

## What it serves

**Composites** (`odim`), one parameter per collection:

| Trait | APIs | Behaviour |
|---|---|---|
| `MapEngine` | WMS, Maps, Tiles | One layer, nearest-neighbour. `TIME` picks the nearest timestep; without it, the latest |
| `EdrEngine` | EDR | `position`: bilinear, one value per timestep. `area`: a bilinear WGS84 grid over the polygon's bbox at about the composite's resolution, at most 256 cells per axis and 1 M values per response (400 beyond), cells outside the polygon null. `radius`: an area over the circle. No locations |

**Polar volumes** (`odim-volume`), per site collection `{id}-{nod}`:

| Trait | APIs | Behaviour |
|---|---|---|
| `MapEngine` | WMS, Maps, Tiles | One layer per moment, plus `CELLS`. `TIME` picks the nearest volume. `ELEVATION` / `elevation` draws the sweep nearest the angle, else the lowest, among sweeps carrying the moment |
| `EdrEngine` | EDR | `locations`, `position`, `area`, `radius`, `trajectory` |
| `VolumeEngine` | 3D Tiles | Point cloud, voxel grid, storm cells |

- **Parameters** are the bare ODIM quantity codes (`DBZH`, `VRADH`, `ZDR`,
  …), the union over the newest volume's sweeps. Labels and units come from
  the table in [`src/quantities.rs`](src/quantities.rs); an unknown code is
  served without a unit. The default quantity (bare WMS layer, no
  `parameter-name`, no 3D Tiles `quantity`) is `DBZH`, else `TH`, else the
  first alphabetically. `[[wms.parameters]]` entries named by the bare
  quantity on the source style that moment at every site.
- **Vertical axis**: sweep elevation angles rounded to 0.1°. EDR `z` keeps
  the sweeps within 0.05° of a requested angle; none matching is a 400.
- **`position`**: without `z`, one vertical profile (value per sweep angle)
  per volume; with `z`, one time series per angle. A point beyond the
  radar's range is a 404. Clear air is a measurement and returns nulls; a
  point with no measurement at the requested angles is a 404: out of the
  sweep's range, a sweep without the parameter, a gate flagged nodata, or
  a sweep that could not be read.
- **`locations`** lists the antenna (id = `NOD`). **`area`** and
  **`radius`** return the `position` coverages at the antenna when it lies
  inside the area, else 404.
- **`trajectory`**: one vertical cross-section (CoverageJSON `Section`)
  per volume under a 2-D `LINESTRING`: nodes about 500 m apart (at most
  1024), heights above the antenna placed with the 4/3-Earth beam model (at
  most 100 levels, 25 km), and the per-node height of the lowest surveyed
  beam, which api-edr emits as `meteocore:beamCoverage`.
- **`CELLS`** (sites with a dBZ moment): storm cells at 35 dBZ, drawn as
  footprint outlines and centroid markers at each cell's peak dBZ, with
  tracks over the six previous scans. `ELEVATION` is ignored. WMS
  GetFeatureInfo answers the tracked cell whose outline holds the pixel's
  centre in the scan the layer draws, with its track id, motion, area and
  peak dBZ, from the product the tiles draw. The poll loop builds cell
  sets ahead of the tiles. It builds a new volume's set from the bytes the scan
  already downloaded, and fills the newest seven-scan window of every site
  when it starts and after each poll: at most 32 volumes per poll, four
  downloads at a time. The tiles of a cold window build its volumes in
  parallel. A tile waiting for another tile's build holds no render slot,
  so other layers keep rendering.
- **3D Tiles**: the point cloud holds at most 8 M points. The voxel grid
  behind the isosurface, echo-top and voxel products is cylindrical (range
  × azimuth × height up to 20 km); the caller picks its size, at most 32 M
  cells. Clear air is filled at −32 dBZ and unobserved cells are NaN. A
  volume without echoes is a 404. Grids and cell sets are cached
  (`MC_PVOL_VOXEL_GRID_CACHE_MB`, default 512;
  `MC_PVOL_CELL_SET_CACHE_MB`, default 64).
- **Site inventory** (`FeatureEngine`, with `features` in `apis`, under
  the base id `{id}`): one Point feature per radar at its antenna, id =
  `NOD`, with `nod`, `name`, `wmo`, `longitude`, `latitude`,
  `antenna_height_m`, `quantities`, `elevation_angles`, `coverage_radius_m`,
  `latest_volume_time`, `volume_count` and `collection`, and a `related`
  link to the site collection when Maps, Tiles or EDR serves it. `bbox`
  matches the antenna; `datetime` keeps sites with a volume in the
  interval. The collection's temporal extent runs from the oldest to the
  newest volume of any site. The same site list feeds a nowcast
  collection's `radar_source`.

## Configuration

Settings live in `[odim]` (`[collections.odim]` inline). The collection's
`data_path` is a local directory or, for composites only, an `http(s)://`
URL.

```toml
id = "radar-eu-composite"
title = "OPERA radar composite"
description = "Pan-European reflectivity composite"
engine_type = "odim"
apis = ["edr", "wms", "maps", "tiles"]

[odim]
endpoint = "https://s3.waw3-1.cloudferro.com"
bucket = "openradar-24h"
prefix_pattern = "%Y/%m/%d/OPERA/COMP/"
filename_template = "OPERA@%Y%m%dT%H%M@0@DBZH.h5"
parameter = "reflectivity"
unit = "dBZ"
time_window = "-PT12H"
poll_interval_secs = 300
```

```toml
id = "radar-fi-volume"
title = "FMI radar volumes"
description = "Polar volumes, one collection per radar"
engine_type = "odim-volume"
apis = ["edr", "wms", "maps", "tiles", "3dtiles", "features"]
data_path = "testdata/radar-fmi-pvol"

[odim]
poll_interval_secs = 300
max_files = 24
```

| Field | Default | Used by | Meaning |
|---|---|---|---|
| `filename_template` | — | `odim` | strftime file name; the whole name must match. Required unless the next two are set |
| `filename_pattern`, `timestamp_format` | — | `odim` | Regex with a `(?P<timestamp>…)` group, and its chrono format |
| `parameter`, `unit` | — | `odim` (required) | Advertised parameter name and unit; no conversion |
| `gain`, `offset`, `nodata` | from the file | `odim` | Override the file's scaling, `value = raw × gain + offset`, and its nodata value |
| `poll_interval_secs` | `30` | both | Rescan interval |
| `max_files` | unbounded | both | Keep the newest N files (`odim-volume`: per site) |
| `endpoint`, `bucket` | — | both | S3 source; set both or neither |
| `prefix_pattern` | — | both | Key prefix with strftime codes, expanded per UTC day (per hour for an hourly prefix, which needs a `time_window` of at most 24 h). Required for S3 (`""` lists the bucket root); appended to an HTTP `data_path` |
| `time_window` | — | both | ISO 8601 duration such as `-PT12H`: bounds the prefixes listed and the times kept. Without it a remote source lists today's and yesterday's prefixes and a local source keeps every file |
| `discovery` | `"list"` | `odim`, HTTP | `"list"` (WebDAV `PROPFIND`) or `"template"`: HEAD-probe the file name of each `cadence_secs` step back from now over `time_window` (at most 288), never re-probing a found file |
| `cadence_secs` | — | `odim`, template | Spacing of probed timestamps; required in template mode |
| `resampling` | `"nearest"` | `odim-volume` | Map render: `"nearest"` or `"bilinear"` (EDR and 3D Tiles always use nearest) |
| `prewarm_sweeps` | `1` | `odim-volume` | Lowest sweeps pre-decoded per volume; `0` disables |
| `prewarm_quantities` | default quantity | `odim-volume` | Quantities pre-decoded at those sweeps, e.g. `["DBZH", "VRADH"]`; one a volume lacks is skipped. Size `MC_PVOL_PIXEL_CACHE_MB` to about twice sites × retained volumes × sweeps × quantities × one sweep |

Example collections in `collections.d/`: `radar-eu-composite-dbzh-s3-h5`
(public S3 bucket), `radar-de-composite-dbzh-http-h5` (HTTP template
discovery), `radar-fi-volume-s3-h5` (polar volumes on public S3). The
`*-local-h5` files read local sample directories that are not in git.

## Limits

- Composites: `/what/object` must be `COMP`, and only `/dataset1/data1` is
  read. Supported projections are PROJ `stere`, `tmerc`, `laea` and
  `longlat`, each projected one on the earth figure the projdef states
  (#1243): a `+R=` sphere, a PROJ ellipsoid or datum name, or `+a=` with
  a shape, by PROJ's rules; WGS84 when it states none. An unknown name, an
  invalid figure, and a name stated beside axes are errors. Datum shifts
  are not applied.
- Polar sources are a local directory (not recursive) or S3; HTTP is not
  supported. A volume without a `NOD`, or with a non-alphanumeric one, is
  skipped.
- A remote polar scan downloads every volume in the window to read its
  metadata. Bound it with `time_window`: `max_files` trims the catalog only
  after the download.
- The polar map render and EDR point samples treat range as ground range,
  with no slant-range correction. Rays are assumed evenly spaced from
  north, so sector scans are not supported.
- Each radar is its own collection; there is no multi-radar mosaic. A
  radar that appears after load becomes a collection on the next reload.
  A remote source that found no radar at load is retried in the background.

## Testing

```bash
cargo test -p engine-odim
```

- Unit tests use synthetic volumes, `testdata/pvol-cold-batch.h5` (a small
  two-sweep volume from `testdata/make_pvol_cold_batch.py`) and
  `testdata/odim-dmi-fixture.h5` (a DMI composite, whose chunked dataset
  the byte-range tests read). The byte-range tests also decode every
  moment of the uncommitted FMI, SMHI and DMI volumes from its ranges when
  those files are present.
- `tests/integration.rs` runs composites against the DMI fixture, read
  locally and through an object-store adapter; template discovery probes
  stub files. Polar volumes run against FMI and SMHI files in
  `testdata/radar-fmi-pvol/` and `testdata/radar-smhi-pvol/`, which are not
  in git; those tests skip when the files are absent.
- `tests/cells_viewport.rs` renders a cold 35-tile `CELLS` viewport over a
  remote source with slow GETs, within the render deadline.
- `examples/` holds standalone tools: 3D Tiles point cloud, isosurface and
  echo-top generators, a volume statistics probe and the voxel sampling
  check in
  [`docs/odim-voxel-validation.md`](../../docs/odim-voxel-validation.md).

Contributor notes: [`CLAUDE.md`](CLAUDE.md).
