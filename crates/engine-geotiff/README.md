# engine-geotiff

Serves a time series of Cloud-Optimized GeoTIFFs (COGs), one file per
timestep, through OGC API - EDR, WMS, OGC API - Maps and OGC API - Tiles.

## What it solves

Radar composites and similar gridded products are often published as one
GeoTIFF per timestep, in a local directory, an object-store bucket or
behind a STAC API. This engine turns such a series into one collection with
a time axis, without importing or converting the files. It reads only the
tiles a request touches, so a remote COG is not downloaded whole.

## How it works

- **Discovery.** A poll loop runs every `poll_interval_secs` on the server's
  background runtime. It lists the source and reads each file's timestamp
  from its name (`filename_template`, or `filename_pattern` +
  `timestamp_format`). Excluded and hidden names are dropped first, and one
  file is kept per timestamp. A new local file joins once its size holds
  steady over two more polls. A STAC source takes each item's `datetime`:
  a poll lists newer items, a request lists older windows on demand.
- **Metadata.** Each new file's header is read once: tiling, overviews,
  georeferencing, nodata (`GDAL_NODATA`) and scale/offset (GDAL metadata).
  A remote file is read by a 512 KiB range request; when those bytes are
  not a COG this reader handles, the file is downloaded whole. A poll
  reads its new files four at a time, newest first, for at most two
  minutes, and leaves the files it did not reach to the next poll, so a
  backlog never holds back the latest frame.
- **Reading.** A request reads only the tiles that cover its window; a map
  reads the overview level that fits its output resolution. Remote tiles
  are fetched in parallel by byte range; local files are memory-mapped.
- **Rendering.** Output pixels map to source pixels through a coarse
  projection grid, sampled nearest-neighbour. Pixels outside the source
  footprint are nodata.
- **Time.** A map renders the newest timestep not after the requested
  time, the first one for a time before the series, and the newest one
  without a time. `resolve_time` shares this rule, so caches key on it.
- **Caching.** Each collection caches compressed tiles of remote sources
  (`tile_cache_mb`); one process-wide cache keeps decoded tiles. A poll
  that finds new remote frames reads the tiles of the newest four into the
  tile cache, coarsest level first and up to `MC_COG_PREWARM_MB` each, so
  their first views mostly skip the storage round trip.
- **Remote failures.** A failed tile fetch is retried twice, then the
  request fails with 503, never a nodata gap. An object gone after the
  scan (404, 416, or shorter than the range) draws nodata for that request
  and leaves the catalog. A 401 or 403 is a 503, and the timestep stays.

## What it serves

| API | What |
|---|---|
| EDR `position` | the value of the pixel holding the point at every timestep in the `datetime` window (all timesteps without one) |
| EDR `area` | a CoverageJSON Grid over the polygon's bounding box at native resolution; cells outside the polygon are null |
| EDR `radius` | the circle as a polygon, answered as `area` |
| WMS | one layer per collection with a `TIME` dimension; GetFeatureInfo reads the EDR position query |
| Maps, Tiles | map images and map tiles of the same raster, with `datetime` |

- One parameter per collection (`parameter`, `unit`). No `locations`,
  `cube`, `trajectory` or `corridor` query, no vertical axis and no model
  runs. An EDR `z` is ignored.
- The native CRS is reported as `EPSG:3067`, `EPSG:3035` or `CRS:84` when
  the grid matches one of those, its parameters on WGS84 or GRS80 (#1192);
  other projections, and those parameters on a sphere or another
  ellipsoid, get no EPSG code.
- The spatial extent reaches a pole the grid holds, at every longitude,
  and is `west > east` across the antimeridian; the map footprint guard
  takes the same extent (#1194).
- The native grid (#1237) is the first loaded file's: its cells in its own
  CRS, with the EPSG code its `ProjectedCSTypeGeoKey` states. When that code
  is the advertised `storageCrs`, the collection description adds
  `storageCrsBbox` and a grid in that CRS. Any projected grid also gets the
  `meteocore:nativeGrid` member, whose CRS is the stated code's URI or else
  the WKT2. See the [api-common README](../api-common/README.md#native-grid).
- Per-query details are in the EDR status page's
  [matrix](../api-edr/README.md#per-engine-query-type-matrix).

## Input files

A file that is not tiled, not georeferenced or not in a supported CRS is
skipped with a warning in the log.

- **Tiled TIFF only.** A strip-based file fails with "Not a tiled TIFF".
  Convert with `gdal_translate -of COG -co COMPRESS=DEFLATE in.tif
  out.tif`; overviews keep zoomed-out maps cheap.
- **Georeferencing:** ModelTiepoint + ModelPixelScale, or an axis-aligned
  ModelTransformation. Rotated or skewed rasters are rejected.
- **CRS:** geographic (read as WGS84 lon/lat, as is a file without
  GeoKeys), Transverse Mercator, Lambert Conformal Conic 2SP and Lambert
  Azimuthal Equal Area, each on the earth figure the GeoKeys state: the
  semi-major axis with the semi-minor axis or inverse flattening, else an
  EPSG ellipsoid code (#1243). With none stated, WGS84. An unknown
  ellipsoid code, an invalid figure, and a conic on an ellipsoid other
  than WGS84 or GRS 1980 are load errors. EPSG:3067 and EPSG:3035 are also
  recognised by code alone.
- **Samples:** UInt8, UInt16, Int16, Float32, Float64; other types fail
  when read. A multi-band file serves one band (`band`). Value = raw ×
  scale + offset; nodata is null.
- **Remote compression:** range reads decode uncompressed, LZW and Deflate
  tiles. Any other compression, sample type or predictor falls back to a
  whole-file download.

## Configuration

In a `collections.d/` file (inside `config.toml` the table is
`[collections.geotiff]`):

```toml
id = "radar"
title = "Radar reflectivity"
description = "5-minute radar reflectivity composite"
engine_type = "geotiff"
apis = ["edr", "wms", "maps", "tiles"]   # any of these four
data_path = "testdata/radar"             # local directory

[geotiff]
filename_template = "radar_%Y%m%dT%H%MZ.tif"
parameter = "reflectivity"
unit = "dBZ"
```

Pick one source. `data_path` is a collection field, outside `[geotiff]`.

| Source | Fields |
|---|---|
| Local directory | `data_path` = a directory; not recursive, symlinks skipped |
| Fixed remote prefix | `data_path = "s3://bucket/prefix/"` (AWS credentials from the environment) or an `http(s)://` URL |
| Date-partitioned bucket | `endpoint` + `bucket` + `prefix_pattern`; unsigned requests, for public buckets |
| STAC API | `stac_url` + `stac_asset_allowlist`; no filename fields needed |

`[geotiff]` fields:

| Field | Default | Meaning |
|---|---|---|
| `filename_template` | – | strftime template; the whole filename must match |
| `filename_pattern`, `timestamp_format` | – | regex with a `timestamp` group, and its chrono format; used as written, so anchor it `^…$` |
| `parameter`, `unit` | required | the collection's one parameter |
| `band` | `1` | 1-based band to read |
| `nodata`, `scale`, `offset` | from the file | override `GDAL_NODATA` and GDAL `SCALE`/`OFFSET` |
| `poll_interval_secs` | `30` | must be > 0; failing polls back off up to 16× |
| `exclude_patterns` | `["*.tmp", "*.part"]` | basenames to skip: `*.ext` or an exact name |
| `max_files` | none | keep the newest N timesteps |
| `tile_cache_mb` | `256` | compressed tile cache for remote sources; `0` disables it |
| `endpoint`, `bucket` | – | S3-compatible endpoint and bucket, both or neither |
| `prefix_pattern` | `""` | object prefix with date or hour specifiers; an hour specifier needs a `time_window` of at most 24 h |
| `time_window` | none | ISO 8601 duration, bucket mode only: `-PT2H` past, `PT6H` future |
| `scan_days` | `2` | days of date prefixes to list when there is no `time_window` |
| `stac_url` | – | STAC items endpoint |
| `stac_asset_key` | `"data"` | asset to read from each item |
| `stac_asset_allowlist` | required with `stac_url` | URL prefixes every asset URL must match |

Process-wide environment variables:

| Variable | Default | Meaning |
|---|---|---|
| `MC_GEOTIFF_DECODE_MEMORY_MB` | `1024` | memory for transient tile decoding across all GeoTIFF collections; past it a request is a 503 |
| `MC_GEOTIFF_DECODED_CHUNK_CACHE_MB` | `512` | decoded-tile cache; `0` disables it |
| `MC_COG_TILE_CONCURRENCY` | `16` | remote tile fetches in flight across all requests (1–1024) |
| `MC_COG_PREWARM_MB` | `32` | per-frame cap on the poll-time pre-warm, also at most 1/8 of `tile_cache_mb`; `0` turns it off |
| `MC_COG_RANGE_BATCH_TILES` | `1` (off) | coalesce up to N nearby tile reads into one range request (max 16) |

Runnable examples: `radar-no-composite-dbzh-local-cog-wgs84.toml` and
`radar-no-composite-dbzh-local-cog-lcc.toml` read committed sample files;
`radar-eu-composite-dbzh-s3-cog.toml` (bucket) and
`radar-no-composite-dbzh-stac-cog.toml` (STAC) read public upstream data.
All are in [`collections.d/`](../../collections.d/).

## Limits

- Nearest-neighbour sampling only; a geographic source is not wrapped
  across ±180° and is taken as WGS84 without a datum check.
- Other projections, including Stereographic, are not read.
- A file over 100 000 pixels per side, or with a decoded tile over 64 MiB,
  is rejected. Remote objects over 50 MiB are skipped.
- Remote range reads take samples as little-endian and undo the
  horizontal predictor (2) byte by byte, which is exact only for
  single-band 8-bit data. Local files have neither restriction.
- A full-resolution map read over 64 M source pixels is refused (400 in
  Maps and Tiles, an error tile in WMS); overviews avoid it.
- EDR area: at most 1 M values across all timesteps, and at most 20 M
  cell × polygon-vertex tests for a non-rectangular polygon; more is a 400.
  For a projected source the Grid's x and y axes are approximations along
  the window's middle row and column; the polygon mask uses each cell's
  true position.
- STAC: an EDR query whose window holds more than 400 items without loaded
  metadata is a 400. Until an item's header loads, the native CRS is
  reported as `CRS:84`. After three failed on-demand STAC listings in a
  row, the collection's map and EDR requests fail at once for 5 minutes.
- Local files are memory-mapped. Replace them by atomic rename, never by
  writing in place: truncating a mapped file can crash the server.
- `--auto-collections` skips GeoTIFF directories; configure them explicitly.

## Testing

```bash
cargo test -p engine-geotiff
```

Fixtures: `testdata/radar/` (WGS84 radar COGs), `testdata/radar-tm35fin/`
(an EPSG:3067 COG), `tests/fixtures/lcc_sphere.tif` (a spherical LCC
grid), `tests/fixtures/laea-polar/` (a polar LAEA grid around the North
Pole on the EASE-Grid sphere), `tests/fixtures/tm_airy.tif` (a TM grid on
Airy 1830 by its axes) and `tests/fixtures/laea_bessel_code.tif` (a LAEA
grid on Bessel 1841 by its EPSG ellipsoid code alone). Remote and STAC paths run against in-memory object stores and
in-process HTTP servers. The ignored tests (`-- --ignored`) call the public
MET Norway STAC API or measure a COG named by an environment variable.
Benchmarks run with `cargo bench -p engine-geotiff`; the
`fuzz_tiff_metadata` target in `fuzz/` exercises the header parser.

Contributor rules and gotchas are in [CLAUDE.md](CLAUDE.md).
