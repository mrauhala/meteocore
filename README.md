# MeteoCore

A high-performance modular meteorological data server built in Rust. Implements [OGC API - EDR](https://ogcapi.ogc.org/edr/), [OGC API - Features](https://ogcapi.ogc.org/features/), [OGC API - Maps](https://ogcapi.ogc.org/maps/), [OGC API - Tiles](https://ogcapi.ogc.org/tiles/), and [OGC WMS 1.3.0](https://www.ogc.org/standard/wms/) as separate services sharing the same data sources.

## Workspace Crates

| Crate | Description |
|-------|-------------|
| `ds-core` | Traits, types, shared utilities |
| `ds-storage` | S3/HTTP/local object store |
| `ds-render` | Raster colorization + PNG encoding |
| `engine-csv` | CSV data engine |
| `engine-geojson` | GeoJSON data engine |
| `engine-geotiff` | GeoTIFF/COG data engine |
| `engine-grib` | GRIB2 NWP data engine |
| `engine-querydata` | FMI QueryData (.sqd) data engine |
| `engine-postgis` | PostGIS/TimescaleDB observation data engine |
| `api-edr` | EDR HTTP layer |
| `api-features` | Features HTTP layer |
| `api-maps` | OGC API Maps HTTP layer |
| `api-tiles` | OGC API Tiles HTTP layer |
| `api-wms` | WMS 1.3.0 HTTP layer |
| `server` | Binary |

## Quick Start

```bash
cargo build                  # Build all crates
cargo test                   # Run all tests
cargo run -p server          # Start server (reads config.toml)
cargo check -p <crate>       # Type-check a single crate
```

### Production Build

```bash
cargo build --release -p server
./target/release/server
```

The release binary is self-contained — deploy it alongside `config.toml` and your data files.

### Fuzz Testing

Fuzz targets live in `fuzz/` (separate from workspace, uses `cargo-fuzz` + `libfuzzer`). Requires nightly.

```bash
cargo install cargo-fuzz                                          # One-time setup
cargo +nightly fuzz run fuzz_tiff_metadata -- -max_total_time=60  # Fuzz TIFF parser
cargo +nightly fuzz run fuzz_geo_transform -- -max_total_time=60  # Fuzz CRS transforms
```

Seed corpus in `fuzz/corpus/fuzz_tiff_metadata/` — add real GeoTIFF files for better coverage.

## Architecture

### Core Traits

Three core traits, each corresponding to one or more APIs:

| Trait | APIs | Description |
|-------|------|-------------|
| `EdrEngine` | EDR | Time-series queries (position, area, locations) returning CoverageJSON |
| `FeatureEngine` | Features | Paginated spatial feature queries returning GeoJSON |
| `MapEngine` | Maps, WMS, Tiles | Raster tile rendering returning PNG/JPEG/WebP images |

Traits are separate — not all engines need to support all APIs. Engines return domain types, never JSON/XML. Serialization belongs in the API crates.

### Dependency Rules

- **ds-core** has no framework dependencies. Only chrono, serde, thiserror, toml.
- **ds-render** has no framework dependencies. Only ds-core and `png`.
- **API crates** depend only on ds-core (and ds-render for api-wms/api-maps), not on any engine crate.
- **CORS** is applied at the server level (`server/src/main.rs`), not in individual API crates.
- **CRS and GeoTransform** live in ds-core (`ds_core::geo`), shared by all engines.

### Collection Routing

- API state is a registry of engines keyed by collection ID (`EdrState` / `FeaturesState` / `MapsState` / `WmsState`).
- Handlers look up engines from a `HashMap<String, Arc<dyn EdrEngine/FeatureEngine/MapEngine>>` by collection ID from the URL path.
- The `apis` config field is enforced — only collections listing a given API in their `apis` array are wired to that API's router.
- Tiles reuses MapEngine — tile z/x/y coordinates are converted to a bbox via TileMatrixSet math, then passed to `MapEngine::get_raster_tile()`.

### State Architecture

API state (`EdrState`, `FeaturesState`, `MapsState`, `TilesState`, `WmsState`) is wrapped in `ArcSwap` for lock-free reads and atomic swaps on reload. The `ServerState` in `server/src/admin.rs` holds the `ArcSwap` pointers, health registry, and GeoTIFF engine list. Engine loading logic is in `admin::load_collections()`, shared by startup and reload.

The render semaphore (sized to available CPU cores, minimum 4) and rendered cache are shared across Maps, Tiles, and WMS APIs. The semaphore uses `acquire().await` so excess requests queue instead of failing.

## Route Structure

```
/                                              Root landing page (links to all services)
/edr/                                          EDR landing page
/edr/api                                       EDR OpenAPI definition (JSON)
/edr/api/docs                                  EDR Swagger UI
/edr/conformance                               EDR conformance classes
/edr/collections                               EDR collection listing
/edr/collections/{id}                          EDR collection detail
/edr/collections/{id}/locations                EDR locations query
/edr/collections/{id}/locations/{loc_id}       EDR location data query (CoverageJSON)
/edr/collections/{id}/position                 EDR position query (CoverageJSON)
/edr/collections/{id}/area                     EDR area query (CoverageJSON)

/features/                                     Features landing page
/features/api                                  Features OpenAPI definition (JSON)
/features/api/docs                             Features Swagger UI
/features/conformance                          Features conformance classes
/features/collections                          Features collection listing
/features/collections/{id}                     Features collection detail
/features/collections/{id}/items               Feature items (paginated GeoJSON)
/features/collections/{id}/items/{feature_id}  Single feature (GeoJSON)

/maps/                                         Maps landing page
/maps/api                                      Maps OpenAPI definition (JSON)
/maps/api/docs                                 Maps Swagger UI
/maps/conformance                              Maps conformance classes
/maps/collections                              Maps collection listing
/maps/collections/{id}                         Maps collection detail
/maps/collections/{id}/map                     Get map (default style, PNG/JPEG/WebP)
/maps/collections/{id}/styles                  List available styles
/maps/collections/{id}/styles/{styleId}/map    Get styled map (PNG/JPEG/WebP)

/tiles/                                        Tiles landing page
/tiles/api                                     Tiles OpenAPI definition (JSON)
/tiles/api/docs                                Tiles Swagger UI
/tiles/conformance                             Tiles conformance classes
/tiles/tileMatrixSets                          List supported tiling schemes
/tiles/tileMatrixSets/{tileMatrixSetId}        Get tiling scheme definition
/tiles/collections                             Tiles collection listing
/tiles/collections/{id}                        Tiles collection detail
/tiles/collections/{id}/tiles                  List tilesets for collection
/tiles/collections/{id}/tiles/{tms}/{z}/{row}/{col}              Get tile (PNG/JPEG/WebP)
/tiles/collections/{id}/styles/{styleId}/tiles/{tms}/{z}/{row}/{col}  Get styled tile

/wms/?SERVICE=WMS&REQUEST=GetCapabilities      WMS 1.3.0 GetCapabilities (XML)
/wms/?SERVICE=WMS&REQUEST=GetMap&...           WMS 1.3.0 GetMap (PNG image)
/wms/?SERVICE=WMS&REQUEST=GetLegendGraphic&... WMS 1.3.0 GetLegendGraphic

/admin/collections/reload                      POST: reload config and swap engines
/health                                        Per-collection health status
/metrics                                       Prometheus metrics (text format)
```

## Configuration

Edit `config.toml` to configure the server and data collections:

```toml
[server]
host = "0.0.0.0"
port = 8000
# base_url = "https://api.example.com"  # optional, for absolute links behind a proxy
# collections_dir = "collections.d"     # optional, load per-collection .toml files from directory

[[collections]]
id = "weather"
title = "Finnish Weather Observations"
description = "Hourly weather observations from Finnish weather stations"
data_path = "testdata/weather.csv"
apis = ["edr", "features"]     # optional, defaults to ["edr"]
engine_type = "csv"             # optional, defaults to "csv"

[[collections]]
id = "municipalities"
title = "Finnish Municipalities"
description = "Municipality boundaries from Statistics Finland"
data_path = "testdata/municipalities.geojson"
engine_type = "geojson"
apis = ["features"]

[[collections]]
id = "radar"
title = "FMI Radar Composite"
description = "Finnish Meteorological Institute radar reflectivity"
engine_type = "geotiff"
apis = ["edr", "wms", "maps", "tiles"]

[collections.geotiff]
filename_template = "radar_%Y%m%dT%H%MZ.tif"
parameter = "reflectivity"
unit = "dBZ"
nodata = 255                    # override if file lacks GDAL_NODATA
# Local directory:
# data_path = "testdata/radar"
# S3 with dynamic prefix:
endpoint = "https://s3.example.com"
bucket = "radar-data"
prefix_pattern = "%Y/%m/%d/"
time_window = "-PT2H"           # keep last 2 hours
max_files = 24

[collections.wms]
colormap = "radar_dbz"          # built-in colormap (or use color_stops for custom)
# rendered_cache_mb = 128       # optional, default 128 MB
```

### Server Config Fields

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `host` | yes | — | Bind address |
| `port` | yes | — | Bind port |
| `base_url` | no | `http://{host}:{port}` | External base URL for absolute links (set when behind a reverse proxy) |
| `collections_dir` | no | — | Directory of per-collection `.toml` config files (see [Per-File Collection Configs](#per-file-collection-configs)) |

### Collection Config Fields

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `id` | yes | — | Unique collection identifier, used in URL paths |
| `title` | yes | — | Human-readable collection title |
| `description` | yes | — | Collection description |
| `data_path` | yes* | — | Path to data file (CSV, GeoJSON) or directory (GeoTIFF) |
| `apis` | no | `["edr"]` | Which APIs expose this collection: `"edr"`, `"features"`, `"maps"`, `"tiles"`, `"wms"` |
| `engine_type` | no | `"csv"` | Data engine: `"csv"`, `"geojson"`, `"geotiff"`, `"grib"`, `"querydata"`, `"postgis"` |
| `wms` | no | — | WMS rendering config. Required when `apis` contains `"wms"`. |

### Per-File Collection Configs

Instead of defining all collections inline in `config.toml`, you can split them into individual `.toml` files in a directory:

```
/etc/meteocore/
├── config.toml
└── collections.d/
    ├── 01-radar-opera.toml
    ├── 02-radar-fmi.toml
    ├── 10-gfs-global.toml
    └── cities.toml.disabled     # not loaded (non-.toml extension)
```

Enable by setting `collections_dir` in `[server]`:

```toml
[server]
collections_dir = "collections.d"   # relative to config.toml, or absolute
```

Each file contains one collection using the same fields as `[[collections]]`, but without the wrapper:

```toml
# collections.d/01-radar-opera.toml
id = "radar-opera"
title = "OPERA Radar Composite"
description = "European radar reflectivity composite"
engine_type = "geotiff"
apis = ["edr", "wms", "maps", "tiles"]

[geotiff]
filename_template = "OPERA@%Y%m%dT%H%M@0@ACRR.tiff"
parameter = "reflectivity"
unit = "dBZ"

[wms]
colormap = "radar_dbz"
```

**Key behaviors:**
- **Coexists with inline collections.** Both `[[collections]]` in `config.toml` and files in `collections_dir` are loaded and merged. Inline collections load first, then directory files sorted alphabetically by filename.
- **Duplicate IDs are rejected.** If the same `id` appears in both inline config and a directory file (or in two directory files), startup fails with an error naming both sources.
- **Only `.toml` files are loaded.** Rename to `.toml.disabled` (or any non-`.toml` extension) to disable a collection without deleting the file.
- **`id` is required in file content**, not derived from the filename. A warning is logged if the filename stem differs from the `id`.
- **Missing directory is a hard error.** If `collections_dir` is set but the directory doesn't exist, the server refuses to start.
- **Hot-reload picks up changes.** `POST /admin/collections/reload` re-reads the directory, loading new files, removing deleted ones, and applying edits.

## Data Engines

### CSV

Fixed columns: `location, latitude, longitude, time` (in that order). All remaining columns become parameters. Parameter units are mapped in `engine-csv/src/loader.rs`.

```csv
location,latitude,longitude,time,temperature,humidity,wind_speed
Helsinki,60.1699,24.9384,2024-01-01T00:00:00Z,-2.5,85.0,3.2
```

### GeoJSON

Standard GeoJSON FeatureCollection files (RFC 7946). Requirements:

- **Coordinates must be in WGS84 (EPSG:4326).** The engine validates all coordinates fall within lon -180..180, lat -90..90 and rejects files in projected CRS with a helpful error message.
- **Supported geometry types:** Point, Polygon, MultiPolygon.
- **Feature IDs:** Extracted from the top-level `"id"` field on each GeoJSON feature object. Falls back to array index if absent.
- **Properties:** Mapped to `PropertyValue` enum (String, Integer, Float, Bool, Null). Nested objects/arrays are serialized to string.

**Security limits** (hardcoded in `engine-geojson/src/loader.rs`):

| Limit | Value | Purpose |
|-------|-------|---------|
| Max file size | 500 MB | Prevents memory exhaustion |
| Max features | 1,000,000 | Prevents excessive load time |
| Max coords per geometry | 100,000 | Prevents geometry bombs |

**Spatial indexing:** R-tree (`rstar` crate) built from per-feature bounding boxes, bulk-loaded at startup in O(n log n).

**Converting projected data to WGS84:**

```bash
ogr2ogr -f GeoJSON -t_srs EPSG:4326 output.geojson input.geojson
```

### GeoTIFF

Cloud-Optimized GeoTIFF (COG) files with tiled layout. Implements `EdrEngine` (EDR) for position/area queries returning CoverageJSON, and `MapEngine` (WMS/Maps/Tiles) for raster tile rendering.

**Requirements:**
- **Must be tiled.** Strip-based TIFFs are not supported. Convert with: `gdal_translate -co TILED=YES -co COMPRESS=DEFLATE input.tif output.tif`
- **One parameter per collection.** Each collection reads a single band from the GeoTIFF files. Multi-band files are supported — select the band with the `band` config field (1-based).
- **Files are discovered by filename pattern.** Each file must contain a parseable timestamp in its filename (e.g., `radar_20260325T1200Z.tif`).

#### Preparing COG Files

**Quick recipe (GDAL 3.1+):**

```bash
gdal_translate -of COG \
  -co COMPRESS=DEFLATE \
  -co PREDICTOR=YES \
  -co BLOCKSIZE=256 \
  -co OVERVIEW_RESAMPLING=AVERAGE \
  -co OVERVIEWS=AUTO \
  input.tif output_cog.tif
```

For radar/classification data, use nearest-neighbor resampling to preserve discrete values:

```bash
gdal_translate -of COG \
  -co COMPRESS=DEFLATE \
  -co PREDICTOR=YES \
  -co BLOCKSIZE=256 \
  -co OVERVIEW_RESAMPLING=NEAREST \
  -co OVERVIEWS=AUTO \
  -a_nodata 255 \
  radar_input.tif radar_cog.tif
```

**Recommended settings:**

| Setting | Value | Rationale |
|---------|-------|-----------|
| Tile size | 256x256 | Matches Tiles API tile size. Smaller tiles = more granular caching. |
| Compression | Deflate | Best ratio with predictor. Tile cache stores compressed bytes. |
| Predictor | 2 (integer) or 3 (float) | Use `PREDICTOR=YES` with COG driver for automatic selection. |
| Overviews | Powers of 2, auto | Without overviews, every low-zoom tile reads full-resolution data. |
| Overview resampling | NEAREST for discrete data, AVERAGE for continuous | Radar: NEAREST preserves categories. Temperature: AVERAGE avoids aliasing. |

**Settings to avoid:**

| Setting | Problem |
|---------|---------|
| Strip layout (no tiling) | Server rejects with "Not a tiled TIFF" error |
| Tile size > 512x512 | Wastes bandwidth/memory on partial reads |
| JPEG compression | Not supported by the server's TIFF reader |
| No overviews | Every low-zoom request reads full resolution — very slow for large files |

#### Supported CRS

Queries are always in WGS84 (lon/lat). The engine reprojects internally when the source files use a projected CRS.

| CRS | GeoKey Code | Example |
|-----|-------------|---------|
| WGS84 / CRS84 | Geographic (model type 2) | EPSG:4326 |
| Transverse Mercator | ProjCoordTrans = 1 | EPSG:3067 (TM35FIN) |
| Lambert Azimuthal Equal Area | ProjCoordTrans = 10 | EPSG:3035 (ETRS89-LAEA) |
| Lambert Conformal Conic (2SP) | ProjCoordTrans = 8 | Various national grids |
| Stereographic | ProjCoordTrans = 14 | FMI/DMI ODIM radar composites |

CRS parameters are read from GeoTIFF GeoKeys (tag 34735). Files without GeoKeys are assumed WGS84. Rotated or skewed rasters are not supported.

When reprojecting, `bbox_to_pixels()` samples 20 points along each bbox edge (not just 4 corners) to capture projection curvature — critical for TM at high latitudes.

#### Supported Compression & Data Types

| Compression | Notes |
|-------------|-------|
| None | Uncompressed tiles |
| Deflate | zlib/deflate (TIFF compression tag 8 or 32946) |
| LZW | TIFF-specific LZW with early code size switch |

| Data Type | Bits | Notes |
|-----------|------|-------|
| UInt8 | 8 | Common for radar/classification data |
| UInt16 | 16 | Common for satellite imagery |
| Int16 | 16 | Signed, e.g., temperature offsets |
| Float32 | 32 | Standard for continuous fields |
| Float64 | 64 | High-precision fields |

Values are converted to `f64` internally. Physical values: `physical = raw * scale + offset`.

#### Data Source Modes

| Mode | Config | Description |
|------|--------|-------------|
| Local directory | `data_path = "path/to/dir"` | Scans a local directory |
| Fixed remote prefix | `data_path = "s3://bucket/prefix/"` | Scans a single S3/HTTP prefix |
| Dynamic remote prefix | `endpoint` + `bucket` + `prefix_pattern` | Expands date-based prefixes on each poll cycle |
| STAC catalog | `stac_url` + `stac_asset_allowlist` | Discovers files via STAC API items endpoint |

#### Polling and File Discovery

The engine polls for new files at a configurable interval (`poll_interval_secs`, default 30s):

- **Local files:** New files are held in a "pending" state for one poll cycle to confirm they are fully written (size stability check). Files matching `exclude_patterns` (default: `*.tmp`, `*.part`) are skipped.
- **Remote files:** Uses COG byte-range reads to fetch only the 64 KB IFD header for metadata. Falls back to full download if header-only parse fails.
- **Metadata caching:** Files with unchanged size reuse their cached metadata across poll cycles.
- **Failure handling:** If a poll cycle fails, the old catalog is preserved. Zero-file results when the old catalog had files are treated as transient failures.
- **Duplicate timestamps:** Lexicographically last filename wins.

#### STAC Catalog Integration

The engine can discover GeoTIFF files via a STAC API instead of directory listing. Useful when the data provider exposes a STAC catalog but denies S3 LIST operations (e.g., MET Norway radar).

**How it works:**
1. On startup, fetches collection extent from the STAC collection endpoint (bbox + temporal interval)
2. When a query arrives, fetches STAC items for that datetime range on-demand
3. Creates lightweight stubs from STAC metadata (datetime, bbox, asset URL)
4. GeoTIFF headers are loaded lazily via COG byte-range reads when pixel data is first needed
5. Loaded metadata is cached — subsequent queries skip all HTTP
6. Poll loop only fetches newly published items (incremental)

**Security:**
- `stac_asset_allowlist` is mandatory (SSRF protection). Every asset URL must match at least one prefix.
- HTTP redirects disabled (prevents redirect-based SSRF).
- Pagination `next` links must be same-origin as the items URL.
- Only `http://` and `https://` asset URLs are accepted.

**Config example:**
```toml
[[collections]]
id = "met-norway-radar"
title = "MET Norway Radar Mosaic"
engine_type = "geotiff"
apis = ["edr", "wms"]

[collections.geotiff]
stac_url = "https://radar-stacapi.met.no/v1/collections/Mosaic-Norway-v1/items"
stac_asset_key = "data"
stac_asset_allowlist = ["https://rgw.met.no/"]
parameter = "reflectivity"
unit = "dBZ"
nodata = 255
max_files = 24
poll_interval_secs = 60

[collections.wms]
colormap = "radar_dbz"
```

#### Tile Caching

The engine caches **compressed** tile bytes (not decoded pixels) in a lock-free LRU cache (~58x better memory efficiency than caching decoded tiles). Default cache size is 256 MB (`tile_cache_mb`). Set to 0 to disable.

#### Security Limits

| Limit | Value | Constant | Purpose |
|-------|-------|----------|---------|
| Max raster dimension | 100,000 px | `MAX_RASTER_DIMENSION` | Prevents loading enormous files |
| Max decoded tile size | 64 MB | `MAX_DECODED_TILE_BYTES` | Prevents decompression bombs |
| Max area query pixels | 1,000,000 | `MAX_AREA_PIXELS` | Prevents huge area queries |
| Max remote file size | 50 MB | `MAX_REMOTE_FILE_SIZE` | Prevents downloading oversized files |
| Max filename length | 255 chars | `MAX_FILENAME_LENGTH` | Prevents abuse via long filenames |

#### GeoTIFF Config Fields

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `filename_template` | * | — | Strftime-based template, e.g., `"radar_%Y%m%dT%H%MZ.tif"`. Auto-derives regex and timestamp format. |
| `filename_pattern` | * | — | Explicit regex with `(?P<timestamp>...)` capture group. Requires `timestamp_format`. |
| `timestamp_format` | * | — | chrono strftime format for the captured timestamp |
| `parameter` | yes | — | Parameter name, e.g., `"reflectivity"` |
| `unit` | yes | — | Unit of measurement, e.g., `"dBZ"` |
| `poll_interval_secs` | no | `30` | Directory poll interval in seconds. Must be > 0. |
| `tile_cache_mb` | no | `256` | Tile cache size in MB. Set to 0 to disable. |
| `band` | no | `1` | Band number to read (1-based). |
| `max_files` | no | none | Keep only the N most recent files by timestamp. |
| `nodata` | no | from file | Override nodata value. |
| `scale` | no | from file | Override scale factor. `physical = raw * scale + offset` |
| `offset` | no | from file | Override offset. |
| `exclude_patterns` | no | `["*.tmp", "*.part"]` | Glob patterns for files to skip. |
| `endpoint` | no | — | S3-compatible endpoint URL |
| `bucket` | no | — | S3 bucket name. Required when `endpoint` is set. |
| `prefix_pattern` | no | `""` | Object prefix, optionally with strftime templates |
| `time_window` | no | none | ISO 8601 duration for file selection, e.g., `"-PT2H"` |
| `scan_days` | no | auto | Number of days to scan for date-based prefixes |
| `stac_url` | no | — | STAC API items endpoint URL |
| `stac_asset_key` | no | `"data"` | Which STAC asset key to use |
| `stac_asset_allowlist` | no | — | Required SSRF protection when `stac_url` is set |

\* Either `filename_template` **or** both `filename_pattern` + `timestamp_format` must be set (not required in STAC mode).

#### Troubleshooting

| Error | Cause | Fix |
|-------|-------|-----|
| "Not a tiled TIFF (TileWidth missing)" | Strip layout | `gdal_translate -co TILED=YES -co COMPRESS=DEFLATE input.tif output.tif` |
| "Raster dimensions exceed maximum" | File > 100,000 x 100,000 px | Downsample or use overviews |
| "No matching GeoTIFF files found" | Pattern mismatch | Check `filename_template` against actual filenames |
| "Either data_path, endpoint+bucket, or stac_url must be configured" | Missing data source | Set one of the three data source modes |
| Empty results / all-None values | Wrong band or missing nodata | Check band count with `gdalinfo`; set `nodata` |
| Slow poll cycles | Many remote files or non-COG layout | Set `max_files` and/or `time_window`; convert to COG |

### GRIB2

GRIB2 files from NWP models. The engine discovers data via JSON index sidecar files, fetches individual GRIB messages via byte-range reads. Primary target: ECMWF IFS open data on S3.

**Requirements:**
- **Index sidecar files (`.index`)** — JSON-lines format with `_offset` and `_length` fields per GRIB message.
- **Regular lat/lon grid** — Template 3.0 (equidistant cylindrical) only.
- **S3/HTTP access** — Uses `ds_storage` for byte-range reads.

**Data access pattern:**
1. Poll S3 prefix for `.index` files (lightweight, ~35 KB each)
2. Parse index -> build catalog: `(reference_time, step) -> (file_url, message_offsets)`
3. On query: HTTP Range request for the specific GRIB message (~500 KB per surface field)
4. Decode message -> regular lat/lon grid -> serve via EdrEngine/MapEngine

**Multi-parameter collections:** Unlike GeoTIFF (one band per collection), a GRIB collection exposes all parameters from the data source. EDR queries select parameters via `parameter-name`. MapEngine uses per-parameter WMS layers.

**Automatic unit conversion** (config-free):

| Source Unit | Display Unit | Conversion | Parameters |
|-------------|-------------|------------|------------|
| K | C | -273.15 | 2t, 2d, skt, t, sot |
| Pa | hPa | x0.01 | msl, sp |
| m | mm | x1000 | tp, cp, sf, sd |
| 0-1 | % | x100 | tcc, lcc, mcc, hcc, lsm |
| m2/s2 | gpm | /9.80665 | z |

**Supported compression:**

| Method | Notes |
|--------|-------|
| Simple packing (5.0) | Pure Rust |
| Complex packing (5.2, 5.3) | Pure Rust |
| CCSDS/AEC (5.42) | Requires `libaec-sys` (C dep). Used by ECMWF. |

#### GRIB Config Fields

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `endpoint` | yes | — | S3-compatible endpoint URL |
| `bucket` | yes | — | S3 bucket name |
| `prefix_pattern` | yes | — | Prefix with strftime templates |
| `index_suffix` | no | `".index"` | Suffix for index sidecar files |
| `data_suffix` | no | `".grib2"` | Suffix for GRIB data files |
| `poll_interval_secs` | no | `300` | Poll interval in seconds |
| `max_runs` | no | none | Keep only the N most recent forecast runs |
| `time_window` | no | none | ISO 8601 duration for valid time filtering |
| `parameters` | no | all | Optional parameter filter, e.g., `["2t", "msl", "tp"]` |
| `grid_cache_mb` | no | `256` | LRU cache size for decoded grids |
| `run_hours` | no | all | Model run hours to poll, e.g., `[0, 6, 12, 18]` |

#### GRIB Config Example

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
prefix_pattern = "%Y%m%d/00z/ifs/0p25/oper/"
poll_interval_secs = 300
max_runs = 2
parameters = ["2t", "msl", "tp", "tcc"]

[collections.wms]
colormap = "temperature"

[[collections.wms.parameters]]
name = "2t"
colormap = "temperature"

[[collections.wms.parameters]]
name = "msl"
colormap = "viridis"
min = 950.0
max = 1050.0
```

### QueryData

FMI QueryData (.sqd) binary format for NWP gridded data. Implements `EdrEngine` (EDR position queries) and `MapEngine` (WMS/Maps/Tiles rendering).

**Key characteristics:**
- **Binary format** with text header. Magic bytes: `@$°£Q`. Version 6.0+ only, little-endian.
- **Memory-mapped** file access via `memmap2` for zero-copy reads.
- **Multi-parameter:** Exposes all parameters from the file. `wms_parameter` config selects which to render for WMS/Maps/Tiles.
- **Polls directory** for latest `.sqd` file, atomically swaps via `ArcSwap`.
- **EDR position queries** use bilinear interpolation across grid points.
- **Map rendering** uses nearest-neighbor resampling.
- **Missing value sentinel:** 32700.0 (treated as `None`).

**Supported CRS:** WGS84, Stereographic, Rotated Lat-Lon.

#### QueryData Config Fields

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `wms_parameter` | no | first param | Parameter to render for WMS. Matched by full name, short name in parens, or numeric ID. |
| `poll_interval_secs` | no | `30` | Directory poll interval in seconds (clamped to >= 1) |

#### QueryData Config Example

```toml
[[collections]]
id = "ecmwf-kenya"
title = "ECMWF Kenya Surface"
description = "ECMWF surface forecast for Kenya region"
data_path = "testdata/ecmwf-kenya"
engine_type = "querydata"
apis = ["edr", "wms", "maps", "tiles"]

[collections.querydata]
wms_parameter = "2t"
poll_interval_secs = 60

[collections.wms]
colormap = "viridis"

[[collections.wms.parameters]]
name = "2t"
colormap = "temperature"
```

### PostGIS observation data

```toml
[[collections]]
id = "fmi-obs"
title = "FMI Weather Observations"
description = "Per-parameter hypertables on TimescaleDB"
engine_type = "postgis"
apis = ["edr", "features"]

[collections.postgis]
dsn_env = "FMI_OBS_DSN"             # env-var name holding the postgres:// URL

[collections.postgis.stations]
table = "public.stations"
id_col = "wigos_id"
label_col = "name"
geom_col = "the_geom"
property_cols = ["territory"]

[collections.postgis.observations]
shape = "per_parameter"             # "long" | "wide" | "per_parameter"
station_fk_col = "wigos_id"
time_col = "time"
time_col_tz = "UTC"                 # required when time_col is timestamp w/o tz
value_col = "value"

[[collections.postgis.observations.tables]]
parameter = "air_temperature"
table = "public.airtemperature"

[[collections.postgis.parameters]]
name = "air_temperature"
label = "2 m air temperature"
unit = "degC"
observed_property = "air_temperature"
```

- Requires PostgreSQL ≥ 13, PostGIS ≥ 3.0; TimescaleDB optional but recommended.
- TLS is deferred to [#110](https://github.com/mrauhala/meteocore/issues/110); v1 connects with `NoTls` — deploy the DB behind a private network or loopback.
- Full reference lives in `crates/engine-postgis/README.md` (role SQL, index recipes, startup hard-error vs. WARN list).

## OGC API - Features

### Query Parameters

| Parameter | Format | Description |
|-----------|--------|-------------|
| `bbox` | `west,south,east,north` or 6-value 3D | Bounding box filter. Supports antimeridian-crossing. |
| `limit` | integer | Page size. Default 100, max 1000. |
| `offset` | integer | Pagination offset. Default 0. |
| `datetime` | RFC 3339 instant or interval | Temporal filter. Supports open bounds (`../end`, `start/..`). |

### Response Details

- Content-Type: `application/geo+json`
- FeatureCollection responses include `timeStamp`, `numberMatched`, `numberReturned`
- Collection metadata includes `extent.spatial.bbox`, `crs`, `storageCrs`

## WMS 1.3.0

Single `/wms/` route dispatches on the `REQUEST` query parameter:

- **GetCapabilities**: XML capabilities with layers, CRS, extents, time dimension, styles
- **GetMap**: Map image (PNG or JPEG)
- **GetLegendGraphic**: Legend image showing colormap scale

### GetMap Parameters

| Parameter | Required | Description |
|-----------|----------|-------------|
| `SERVICE` | yes | Must be `WMS` |
| `VERSION` | yes | Must be `1.3.0` |
| `REQUEST` | yes | `GetMap` |
| `LAYERS` | yes | Single layer name (= collection ID) |
| `CRS` | yes | `CRS:84`, `EPSG:4326`, `EPSG:3857`, `EPSG:3067`, or `EPSG:3035` |
| `BBOX` | yes | Bounding box — axis order depends on CRS (see below) |
| `WIDTH` | yes | Image width in pixels (max 4096) |
| `HEIGHT` | yes | Image height in pixels (max 4096) |
| `FORMAT` | yes | `image/png` or `image/jpeg` |
| `TRANSPARENT` | no | Default `TRUE` |
| `STYLES` | no | Style name (default: `default`) |
| `TIME` | no | ISO 8601 timestamp; defaults to latest available |

### WMS 1.3.0 Axis Order

**Critical:** WMS 1.3.0 BBOX axis order depends on the CRS:

- **CRS:84**: `BBOX=west,south,east,north` (lon/lat)
- **EPSG:4326**: `BBOX=south,west,north,east` (lat/lon — swapped!)
- **EPSG:3857, EPSG:3067, EPSG:3035**: `BBOX=minx,miny,maxx,maxy` (easting/northing)

### WMS Error Handling

Errors are returned as XML `ServiceExceptionReport` documents with WMS-specific error codes: `LayerNotDefined`, `CRSNotDefined`, `InvalidDimensionValue`, `MissingParameterValue`, `InvalidFormat`, etc.

### Rendering Pipeline

1. Parse WMS parameters, validate, normalize bbox axis order
2. Look up `MapEngine` by layer name (= collection ID)
3. Check rendered image cache (LRU, keyed by quantized bbox + layer + time)
4. If miss: await render semaphore, call `MapEngine::get_raster_tile()` on a blocking thread
5. Engine projects bbox to source CRS, selects best overview level, reads source tiles, resamples to output dimensions (nearest-neighbor)
6. If tile is all nodata: return transparent PNG without caching
7. Apply colormap (LUT for integer data, linear interpolation for continuous data) -> RGBA buffer
8. Encode RGBA -> PNG, cache, return

### Colormaps

| Built-in Name | Description | Default Range |
|---------------|-------------|---------------|
| `radar_dbz` | Standard radar reflectivity (blue->green->yellow->red) | 0-70 dBZ |
| `grayscale` | Linear black->white | 0-1 |
| `viridis` | Perceptually uniform (good default) | 0-1 |
| `temperature` | Temperature palette (purple->blue->cyan->green->yellow->red) | -40-50 C |
| `precipitation` | Precipitation accumulation (transparent->blue->purple->white) | 0-50 mm |
| `wind_speed` | Wind speed (green->yellow->orange->red->purple) | 0-50 m/s |

Custom color stops override built-in colormaps:

```toml
[collections.wms]
[[collections.wms.color_stops]]
value = 0.0
color = "#00000000"
[[collections.wms.color_stops]]
value = 50.0
color = "#FF0000"
```

### Named Styles

```toml
[[collections.wms.styles]]
name = "grayscale"
title = "Grayscale"
colormap = "grayscale"
min = 0.0
max = 70.0
```

### WMS Config Fields

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `colormap` | no | `"viridis"` | Built-in colormap name for the default style |
| `color_stops` | no | — | Array of `{value, color}` entries. Overrides built-in colormap. |
| `min` | no | from colormap | Minimum value for the colormap range |
| `max` | no | from colormap | Maximum value for the colormap range |
| `styles` | no | — | Array of named styles |
| `rendered_cache_mb` | no | `512` | Rendered image cache size in MB. Set to 0 to disable. |

### Security Limits

| Limit | Value |
|-------|-------|
| MAX_MAP_PIXELS | 16,777,216 (16M) |
| MAX_MAP_DIMENSION | 4,096 px |
| Render semaphore | CPU core count (min 4) |
| Max LAYERS | 1 |
| CRS whitelist | CRS:84, EPSG:4326/3857/3067/3035 |
| FORMAT whitelist | `image/png`, `image/jpeg` |

## OGC API - Maps

REST-based map image API. Maps and WMS share the `MapEngine` trait but have separate HTTP layers.

### Maps Query Parameters

| Parameter | Required | Default | Description |
|-----------|----------|---------|-------------|
| `bbox` | yes | — | `west,south,east,north` (always lon/lat order) |
| `width` | no | `256` | Image width in pixels (max 4096) |
| `height` | no | `256` | Image height in pixels (max 4096) |
| `crs` | no | `CRS:84` | Output CRS |
| `datetime` | no | latest | ISO 8601 timestamp |
| `f` | no | `image/png` | `image/png`, `image/jpeg`, `image/webp` |

### Key Differences from WMS

- Maps uses REST paths (`/maps/collections/{id}/map`), WMS uses query parameters
- Maps bbox is always lon/lat order (no axis-order swapping)
- Maps supports WebP output format
- Maps has named styles via `/collections/{id}/styles/{styleId}/map`

## OGC API - Tiles

Serves raster data as tiled map images. Tiles reuses the same rendering pipeline as Maps/WMS.

### Tile Addressing

`{tileMatrixSetId}/{tileMatrix}/{tileRow}/{tileCol}` — all tiles are fixed 256x256 pixels.

### Supported TileMatrixSets

| ID | CRS | Description |
|----|-----|-------------|
| `WebMercatorQuad` | EPSG:3857 | Standard web map tiles (Google/OSM scheme) |
| `WorldCRS84Quad` | CRS:84 | Geographic tiles in lon/lat |

### Tiles Query Parameters

| Parameter | Required | Default | Description |
|-----------|----------|---------|-------------|
| `datetime` | no | latest | ISO 8601 timestamp |
| `f` | no | `image/png` | `image/png`, `image/jpeg`, `image/webp` |

## Caching

### Rendered Image Cache (Tier 2)

Separate from the GeoTIFF source tile cache (Tier 1). Caches final PNG/JPEG/WebP bytes. Shared across WMS, Maps, and Tiles APIs.

- Default size: 512 MB (configurable via `rendered_cache_mb`)
- Cache key: quantized bbox (6 decimal places) + layer + style + format + width + height + CRS + time
- Lock-free concurrent LRU (uses `quick_cache`)
- No TTL — immutable data. Cache invalidated on collection reload.
- Error tiles and empty tiles (all nodata) are NOT cached.

### HTTP Cache Headers

| Scenario | Cache-Control | ETag |
|----------|---------------|------|
| Request with explicit timestamp | `public, max-age=86400, immutable` | Yes |
| Request without timestamp (latest) | `public, max-age=60, must-revalidate` | Yes |
| GetLegendGraphic | `public, max-age=86400, immutable` | No |
| Metadata endpoints | No cache headers | No |

## OpenAPI and Swagger UI

Each OGC API serves a dynamic OpenAPI 3.0.3 specification and a Swagger UI:

| API | OpenAPI spec | Swagger UI |
|-----|-------------|------------|
| EDR | `/edr/api` | `/edr/api/docs` |
| Features | `/features/api` | `/features/api/docs` |
| Maps | `/maps/api` | `/maps/api/docs` |
| Tiles | `/tiles/api` | `/tiles/api/docs` |

OpenAPI specs are generated dynamically from configured collections. WMS uses XML GetCapabilities for service description.

## Admin, Health & Metrics

### Dynamic Collection Reload

`POST /admin/collections/reload` re-reads `config.toml` (and `collections_dir` if configured), creates new engines, and atomically swaps them into the running server. If the reload produces zero working collections, the old state is preserved.

### Health Endpoint

`GET /health` returns per-collection health status:

| Status | Meaning |
|--------|---------|
| `ready` | Engine loaded and has data |
| `degraded` | Engine loaded but no data yet |
| `failed` | Engine failed to load (error message included) |

Returns HTTP 503 only when all collections have failed.

### Prometheus Metrics

`GET /metrics` returns Prometheus text format. Path labels are the matched route template (e.g. `/edr/collections/{id}/position`), not the raw URL, so cardinality stays bounded.

**HTTP:**

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `http_requests_total` | counter | method, path, status | Total HTTP requests |
| `http_request_duration_seconds` | histogram | method, path | Request latency histogram |
| `http_response_bytes_total` | counter | method, path | Response body bytes sent |

**Collections & health:**

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `collections_total` | gauge | — | Total configured collections |
| `collections_healthy` | gauge | — | Collections in ready state |
| `collections_degraded` | gauge | — | Collections in degraded state |
| `collections_failed` | gauge | — | Collections in failed state |

**GeoTIFF tile cache** (per-collection, compressed byte cache for remote COGs):

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `tile_cache_hits_total` | counter | collection | Tile cache hits |
| `tile_cache_misses_total` | counter | collection | Tile cache misses |
| `tile_cache_bytes` | gauge | collection | Bytes currently held |
| `tile_cache_capacity_bytes` | gauge | collection | Configured capacity |
| `tile_cache_entries` | gauge | collection | Number of cached entries |

**Rendered image cache** (global, shared across WMS/Maps/Tiles):

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `rendered_cache_hits_total` | counter | — | Rendered image cache hits |
| `rendered_cache_misses_total` | counter | — | Rendered image cache misses |
| `rendered_cache_bytes` | gauge | — | Bytes currently held |
| `rendered_cache_capacity_bytes` | gauge | — | Configured capacity |
| `rendered_cache_entries` | gauge | — | Number of cached entries |

**GRIB grid cache** (per-collection, decoded grid cache):

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `grid_cache_hits_total` | counter | collection | Grid cache hits |
| `grid_cache_misses_total` | counter | collection | Grid cache misses |
| `grid_cache_bytes` | gauge | collection | Bytes currently held |
| `grid_cache_capacity_bytes` | gauge | collection | Configured capacity |
| `grid_cache_entries` | gauge | collection | Number of cached entries |

**Render & storage:**

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `render_semaphore_available` | gauge | — | Available render permits |
| `render_semaphore_total` | gauge | — | Total render permits (CPU cores) |
| `storage_bytes_read_total` | counter | collection, engine_type | Bytes read from remote storage |

#### Computing cache hit rates

All cache hit/miss metrics are true counters, so you can use PromQL `rate()` directly:

```promql
sum(rate(tile_cache_hits_total[5m]))
  / clamp_min(sum(rate(tile_cache_hits_total[5m])) + sum(rate(tile_cache_misses_total[5m])), 0.0001)
```

Replace `tile_` with `rendered_` or `grid_` for the other two caches. Drop the outer `sum(...)` and add a `by (collection)` clause to get per-collection hit rates for tuning.

### Structured Logging

The server emits one structured log line per HTTP request with the fields needed to diagnose slow queries and correlate them with client-side errors: `request_id`, `method`, `path`, `route`, `api`, `collection`, `query_type`, `query`, `status`, `duration_ms`, `result_size`.

Two log formats are supported, selected via the `LOG_FORMAT` environment variable:

| `LOG_FORMAT` | Output | When to use |
|--------------|--------|-------------|
| _unset_ or `pretty` | Human-readable ANSI | Local development |
| `json` | Newline-delimited JSON, one object per event | Production — directly ingestable by Loki / Alloy / Promtail without regex parse stages |

Log level is controlled via the standard `RUST_LOG` env var (default `info`), e.g. `RUST_LOG=server=debug,engine_geotiff=warn`.

#### Correlation IDs

Every request is assigned a correlation ID:

- If the client sends an `X-Request-ID` header with a safe value (printable ASCII, ≤128 chars, no CRLF), it is reused.
- Otherwise the server generates a fresh UUIDv4.
- The ID is echoed back in the response `X-Request-ID` header so the client can log it.
- The ID appears as `request_id` on every log event emitted during the request and inside a dedicated `http_request` tracing span.

This lets you jump from a failed client request, to the exact server log line, to every downstream log written while that request was being handled.

### Observability Stack

The repo ships a batteries-included observability stack for local development and smoke testing at `compose.yml`. It uses **Docker Hardened Images** (free Community tier, non-FIPS) for every sidecar so the stack matches a realistic production profile: distroless, non-root, shell-less, CVE-scanned.

Services:

| Service | Image | Port | Purpose |
|---------|-------|------|---------|
| `meteocore` | built from `./Dockerfile` | 8000 | The server, `LOG_FORMAT=json` |
| `prometheus` | `dhi.io/prometheus:3.11` | 9090 | Scrapes `/metrics` every 15s |
| `loki` | `dhi.io/loki:3.6` | 3100 | Stores structured log events |
| `alloy` | `dhi.io/alloy:1.15` | 12345 | Tails meteocore stdout, parses JSON, pushes to Loki |
| `grafana` | `dhi.io/grafana:12.3` | 3000 | Auto-provisioned dashboard + both datasources |

First-time setup (DHI Community images require authentication):

```bash
docker login dhi.io           # free Docker account
docker compose up -d
open http://localhost:3000    # Grafana (anonymous admin for local dev)
```

Tear down including volumes: `docker compose down -v`.

Alloy promotes a bounded set of labels to Loki: `level`, `api`, `query_type`, `status_class` (`2xx` / `4xx` / `5xx`). High-cardinality fields like `collection`, `path`, `request_id`, and the raw query string stay in the log body and are queryable via `| json` in LogQL — this keeps Loki's index bounded as the number of collections grows.

The bundled Grafana dashboard at `grafana/dashboards/meteocore-overview.json` has a **Logs** row with four Loki-backed panels: request rate by `(api, query_type)`, error rate by `status_class`, live request stream, and a dedicated 4xx/5xx stream. All panels work with the Prometheus and Loki datasources that are auto-provisioned from `grafana/provisioning/`.

> Production note: the compose stack is intended for local dev. It uses anonymous admin Grafana, filesystem-backed Loki, no retention beyond 7 days, and runs Alloy as root so it can tail the Docker socket. Do not deploy it as-is.

## Testing

```bash
cargo test                   # Run all tests
cargo test -p api-edr        # CoverageJSON schema validation
cargo test -p api-features   # Features API tests
cargo test -p api-maps       # Maps API tests
cargo test -p api-wms        # WMS tests
cargo test -p ds-core        # Core tests (datetime, bbox, CRS)
cargo test -p engine-csv     # CSV engine tests
cargo test -p engine-geotiff # GeoTIFF engine tests
```

CoverageJSON output is validated against the official [OGC CoverageJSON 1.0 schema](https://schemas.opengis.net/covjson/1.0/coveragejson.json) stored in `schemas/coveragejson.json`.

## Known Limitations

- CSV/GeoJSON data loaded into memory at startup; GeoTIFF reads tiles on demand
- CSV engine supports only the `locations` query type
- GeoJSON engine implements `FeatureEngine` only (not EDR or WMS)
- GeoTIFF engine implements `EdrEngine` + `MapEngine` only (not Features)
- GeoTIFF: one band per collection; strip-based TIFFs not supported
- WMS: single LAYERS only, no SLD/SE styling, no GetFeatureInfo
- WMS/Maps/Tiles: nearest-neighbor resampling only
- STAC: no retry logic, no HTTP caching (ETag/Last-Modified)
- Tiles: raster only (no vector tiles yet), WebMercatorQuad and WorldCRS84Quad only, fixed 256x256
- GRIB: regular lat/lon grids only, GRIB2 only, requires index sidecar files
- QueryData: serves latest file only, no compressed files, EDR position only, level 0 only

## Tech Stack

- **Rust** with axum + tokio async runtime
- **chrono** for datetime handling
- **serde** + serde_json for serialization
- **tower-http** for CORS
- **thiserror** for error types
- **quick_cache** for LRU caching
- **rstar** for R-tree spatial indexing
- **quick-xml** for WMS XML output
- **png** for image encoding
- **libaec-sys** for GRIB CCSDS/AEC decompression
- **memmap2** for memory-mapped QueryData file access
