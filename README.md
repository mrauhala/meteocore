# MeteoCore

A high-performance modular meteorological data server built in Rust. Implements [OGC API - EDR 1.1](https://ogcapi.ogc.org/edr/), [OGC API - Features 1.0](https://ogcapi.ogc.org/features/), [OGC API - Maps 1.0](https://ogcapi.ogc.org/maps/), [OGC API - Tiles 1.0](https://ogcapi.ogc.org/tiles/), [OGC WMS 1.3.0](https://www.ogc.org/standard/wms/), and [OGC 3D Tiles 1.1](https://www.ogc.org/standard/3dtiles/) as separate services sharing the same data sources. A built-in `/preview` SPA renders every configured collection on a MapLibre canvas for quick visual smoke-testing; a bundled CesiumJS viewer serves volumetric 3D Tiles collections.

## Workspace Crates

### Foundation

| Crate | Description |
|-------|-------------|
| `ds-core` | Domain traits (`EdrEngine`, `FeatureEngine`, `MapEngine`, `VolumeEngine`), shared types (CRS, GeoTransform, PropertyValue), config parsing. No framework deps. |
| `ds-storage` | Unified S3 / HTTP / local-filesystem object store, used by every engine that fetches remote data. |
| `ds-render` | Raster colorization (LUT + linear gradient) and PNG encoding. No framework deps. |
| `ds-mvt` | Mapbox Vector Tile encoder + weighted LRU cache. Used by `api-tiles` to serve `?f=mvt` from `FeatureEngine` collections. |
| `ds-3dtiles` | Framework-free OGC 3D Tiles encoder — `.pnts` point clouds, glTF `.glb` isosurfaces (marching tetrahedra), echo-top column meshes, and cylindrical voxel grids. No framework deps. |

### Data Engines

Each engine implements one or more of the core traits.

| Crate | Traits | Source |
|-------|--------|--------|
| `engine-csv` | `EdrEngine` + `FeatureEngine` | CSV files with fixed `location, latitude, longitude, time, …` columns |
| `engine-geojson` | `FeatureEngine` | GeoJSON FeatureCollection files (WGS84 only) |
| `engine-geotiff` | `EdrEngine` + `MapEngine` | Cloud-Optimized GeoTIFF (local dir, S3, or STAC catalog) |
| `engine-grib` | `EdrEngine` + `MapEngine` | GRIB2 NWP data via JSON/wgrib2 index sidecars (ECMWF IFS, NOAA GFS) |
| `engine-odim` | `EdrEngine` + `MapEngine` (composites); `EdrEngine` + `MapEngine` + `VolumeEngine` + `FeatureEngine` (polar volumes) | ODIM_H5 weather radar — 2-D composites (FMI / DMI / SMHI / OPERA) and native polar volumes (`odim-volume`, one collection per radar site); pure-Rust HDF5 |
| `engine-querydata` | `EdrEngine` + `MapEngine` | FMI QueryData (`.sqd`) binary files, memory-mapped |
| `engine-zarr` | `EdrEngine` + `MapEngine` | Zarr V2/V3 multidimensional arrays with CF metadata (local, S3, HTTP); optional Icechunk repositories |
| `engine-postgis` | `EdrEngine` + `FeatureEngine` | PostgreSQL/PostGIS observation tables (TimescaleDB compatible) |

### OGC API Plugins

| Crate | Plugin | Conformance |
|-------|--------|-------------|
| `api-edr` | [OGC API - EDR 1.1](https://docs.ogc.org/is/19-086r6/19-086r6.html) | ogcapi-common-1: core, landing-page, oas30; ogcapi-edr-1: core, collections, json, edr-geojson, covjson |
| `api-features` | [OGC API - Features 1.0](https://docs.ogc.org/is/17-069r4/17-069r4.html) | core, oas30, geojson |
| `api-maps` | [OGC API - Maps 1.0](https://docs.ogc.org/is/20-058/20-058.html) | core, collection-map, styled-map, spatial-subsetting, scaling, datetime, crs, png, jpeg |
| `api-tiles` | [OGC API - Tiles 1.0](https://docs.ogc.org/is/20-057/20-057.html) | ogcapi-tiles-1: core, tileset, tilesets-list, png, jpeg, mvt; tms 2.0: tilematrixset, json-tilematrixset |
| `api-wms` | [OGC WMS 1.3.0](https://portal.ogc.org/files/?artifact_id=14416) | GetCapabilities, GetMap, GetLegendGraphic |
| `api-3dtiles` | [OGC 3D Tiles 1.1](https://docs.ogc.org/cs/22-025r4/22-025r4.html) | point clouds (`.pnts`), glTF mesh content (`.glb` — isosurface + echo-top), cylindrical voxels (`.glb`, `EXT_primitive_voxels` draft); bundled CesiumJS viewer SPA |

### Binary

| Crate | Description |
|-------|-------------|
| `server` | The deployable binary. Composes the plugins above into a single axum router, wires CORS, admin bearer-token auth (`/admin/*` only), and metrics, embeds the `/preview` MapLibre SPA, and owns config + hot-reload. |

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

### Command-Line Options

```
Usage: server [OPTIONS]

Options:
  --collections <id1,id2,...>   Only load collections with these IDs (comma-separated).
                                All others are silently skipped. Useful for smoke-testing
                                a single collection without editing config.toml.
  --host <HOST>                 Bind host. Overrides [server].host (CLI wins over config).
  --port <PORT>                 Bind port. Overrides [server].port (CLI wins over config).
                                Must be 1..=65535.
  --config <PATH>               Config file path. Overrides the CONFIG_PATH env var and the
                                ./config.toml default. A path given here that does not exist
                                is a hard error.
  --auto-collections <DIR>      Auto-discover collections from a directory (repeatable).
                                Synthesizes collections from the data files found, with no
                                config.toml. zarr/grib/querydata/csv/geojson (see below).
  -h, --help                    Print this help and exit.
```

Each flag also accepts the `--flag=value` spelling (e.g. `--port=8011`). The
parser is hand-rolled (no `clap`); `BASE_URL` still wins over `--host`/`--port`
for generated links.

**No-config boot.** If the default config path (`./config.toml`) is absent **and**
no `--config` is given, the server starts from built-in defaults — host
`127.0.0.1`, **auto-selecting the first free port at or above 8000** (scanning up
to 100 ports). With no `--auto-collections` it comes up empty (it answers
`/health` and an empty `/collections`, and can be populated via
`POST /admin/collections/reload`). A port pinned by config or `--port` is **not**
auto-scanned: a bind conflict is fatal.

```bash
server                                 # no config.toml -> 127.0.0.1, first free port >= 8000
server --port 9000                     # explicit port; conflict is fatal (no scan)
server --config /etc/mc.toml           # explicit config; missing path is an error
server --auto-collections ./data       # serve a directory of data with no config.toml
```

#### Auto-collections (`--auto-collections <DIR>`)

Point the server at a directory and it **synthesizes collections from the data
files on disk** — no `config.toml` needed (combine with the no-config boot above
for a zero-config `server --auto-collections ./data`). The flag is repeatable to
scan several roots. Synthesized collections are merged with any config-file
collections and validated together (duplicate ids are rejected).

**Mapping:** each immediate **subdirectory** of a root becomes a collection (id =
slugified directory name); data files sitting **loose** in the root are grouped
the same way under the root's name. Detection per directory (first match wins):

| On disk | Becomes | APIs |
|---------|---------|------|
| `zarr.json` / `.zgroup` / `.zarray`, or a `*.zarr` dir name | one `zarr` collection | EDR, WMS, Maps, Tiles |
| `*.sqd` | one `querydata` collection | EDR, WMS, Maps, Tiles; model runs from the files |
| `*.grib2`/`*.grb2` **with** `*.index`/`*.idx` sidecars | one `grib` collection | EDR, WMS, Maps, Tiles; `index_format` inferred (`.idx`→wgrib2, `.index`→ecmwf-json) |
| `*.grib2` **without** index sidecars | _(skipped)_ | the GRIB engine needs prebuilt indexes |
| `*.tif`/`*.tiff` (GeoTIFF), `*.h5`/`*.hdf5` (ODIM) | _(skipped)_ | **phase 2** — needs filename-template inference (#411) |
| `*.geojson` | one `geojson` collection **per file** | Features, Tiles (MVT) |
| `*.csv` | one `csv` collection **per file** | EDR, Features; columns are positional: `location,latitude,longitude,time,<params…>` |

Each collection enables **all APIs relevant to its type** so the data is
browsable in `/preview` (with a parameter selector) out of the box. Raster/grid
collections (zarr/grib/querydata) render via a **default colormap** when no
`[wms]` block is configured — the colormap *range* is generic (viridis `0..1`),
so weather data in physical units renders but with poor colour contrast until you
add a per-collection `[wms]` colormap with `min`/`max` (or auto-scaling lands,
[#320](https://github.com/mrauhala/meteocore/issues/320)).

Pointing `--auto-collections` directly at a single Zarr store (rather than its
parent) also works. **Symlinks are followed** — a symlinked subdirectory or data
file is scanned like a real one (operators commonly symlink data into a serving
directory). This is the same trust model as `collections_dir`: whoever can write
the scan root controls what is served, so keep it writable only by trusted
principals.

**Phase 1 limitations:** GeoTIFF and ODIM (the filename-timestamped formats) are
deferred to phase 2. Auto-collections are resolved once at startup;
`POST /admin/collections/reload` re-reads `config.toml`/`collections_dir` but does
not re-scan the auto roots.

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `CONFIG_PATH` | `./config.toml` | Path to the server configuration file (the `--config` flag takes priority) |
| `LOG_FORMAT` | human-readable | Set to `json` for newline-delimited JSON logs (production / Loki ingestion) |
| `RUST_LOG` | `info` | Log level filter, e.g. `server=debug,engine_geotiff=warn` |
| `ADMIN_TOKEN` | _(none — unauthenticated)_ | Bearer token required for `POST /admin/collections/reload`. When unset, the admin endpoint is open. |
| `MC_3DTILES_CONTENT_CACHE_MB` | `512` | 3D Tiles encoded-content cache size in MB. `0` disables. |
| `MC_PVOL_VOXEL_GRID_CACHE_MB` | `512` | PVOL polar-resampled voxel-grid cache size in MB. `0` disables. |
| `MC_PVOL_PIXEL_CACHE_MB` | `1024` | PVOL per-moment decoded-pixel cache size in MB. `0` disables. |
| `MC_ODIM_COMPOSITE_CACHE_MB` | `2048` | ODIM decoded-composite (COMP) cache size in MB. `0` disables. |
| `MC_GEOTIFF_DECODED_CHUNK_CACHE_MB` | `512` | GeoTIFF decoded-chunk cache for local sources, in MB. `0` disables. |
| `MC_COG_TILE_CONCURRENCY` | `16` | Max concurrent remote-COG tile (byte-range) fetches in the shared fetch pool. Raise for high-latency object stores; value must be ≥ 1. |
| `MC_ALLOW_INLINE_DB_URL` | _(unset)_ | Set to `1` to allow a literal `postgres://` URL in TOML instead of `dsn_env` (development only). |

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

Four core traits, each corresponding to one or more APIs:

| Trait | APIs | Description |
|-------|------|-------------|
| `EdrEngine` | OGC API - EDR 1.1 | Time-series queries (position, area, locations) returning CoverageJSON |
| `FeatureEngine` | OGC API - Features 1.0, OGC API - Tiles 1.0 (MVT) | Paginated spatial feature queries returning GeoJSON; vector tiles are encoded from the same query via `ds-mvt` |
| `MapEngine` | OGC API - Maps 1.0, OGC WMS 1.3.0, OGC API - Tiles 1.0 (raster) | Raster tile rendering returning PNG/JPEG/WebP |
| `VolumeEngine` | OGC 3D Tiles 1.1 | Volumetric point clouds and voxel grids — radar polar volumes rendered as `.pnts`, glTF isosurfaces, echo-top meshes, and cylindrical voxels |

Traits are separate — not all engines need to support all APIs. Engines return domain types, never JSON/XML. Serialization belongs in the API crates.

### Dependency Rules

- **ds-core** has no framework dependencies. Only chrono, serde, thiserror, toml.
- **ds-render** has no framework dependencies. Only ds-core and `png`.
- **API crates** depend only on ds-core (and ds-render for api-wms/api-maps), not on any engine crate.
- **CORS** is applied at the server level (`server/src/main.rs`), not in individual API crates.
- **CRS and GeoTransform** live in ds-core (`ds_core::geo`), shared by all engines.

### Collection Routing

Each API has its own state struct — a registry of engines keyed by collection ID:

| State type | Engine map key type | Used by |
|------------|---------------------|---------|
| `EdrState` | `Arc<dyn EdrEngine>` | `api-edr` |
| `FeaturesState` | `Arc<dyn FeatureEngine>` | `api-features` |
| `MapsState` | `Arc<dyn MapEngine>` | `api-maps` |
| `TilesState` | `Arc<dyn MapEngine>` (raster) + `Arc<dyn FeatureEngine>` (vector/MVT) | `api-tiles` |
| `WmsState` | `Arc<dyn MapEngine>` | `api-wms` |
| `TilesState3d` | `Arc<dyn VolumeEngine>` | `api-3dtiles` |

Handlers look up the engine for a request's `{id}` path segment from the appropriate `HashMap<String, Arc<dyn …>>`. The `apis` config field is enforced at load time — only collections listing a given API in their `apis` array are wired into that API's state. Tiles keeps two independent maps (`map_engines` for raster, `feature_engines` for MVT) because a collection may serve one or both.

Tiles (raster) reuses `MapEngine` — z/x/y coordinates are converted to a bbox via TileMatrixSet math and passed to `MapEngine::get_raster_tile()`.

### State Architecture

Each state struct is wrapped in `Arc<ArcSwap<…>>` for lock-free reads and atomic swaps on reload. `ServerState` (in `server/src/admin.rs`) owns all six `ArcSwap` pointers plus the health registry and GeoTIFF engine list (kept separately for the poll runtime). Engine loading lives in `admin::load_collections()`, called by both startup and `POST /admin/collections/reload`.

The render semaphore (2× CPU cores, minimum 8) and rendered-image cache are shared across `MapsState`, `TilesState`, and `WmsState`. `TilesState3d` holds the shared render semaphore (used for `.pnts` and `.glb` isosurface/echo-top encoding) and its own content cache (`api-3dtiles/src/cache.rs`). Voxel content uses a separate `VOXEL_SEMAPHORE` (¼ CPU cores) so slow high-resolution voxel encodes cannot occupy raster render slots. The semaphore uses `acquire().await` so excess requests queue rather than fail. The 2× factor reflects loose CPU ownership — libpng decode bursts and bilinear-sample passes interleave, leaving the slot idle a non-trivial fraction of its wall time.

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
/tiles/collections/{id}/tiles/{tms}/{z}/{row}/{col}              Get tile (PNG/JPEG/WebP, or MVT via ?f=mvt)
/tiles/collections/{id}/styles/{styleId}/tiles/{tms}/{z}/{row}/{col}  Get styled tile

/wms/?SERVICE=WMS&REQUEST=GetCapabilities      WMS 1.3.0 GetCapabilities (XML)
/wms/?SERVICE=WMS&REQUEST=GetMap&...           WMS 1.3.0 GetMap (PNG/JPEG/WebP)
/wms/?SERVICE=WMS&REQUEST=GetLegendGraphic&... WMS 1.3.0 GetLegendGraphic (PNG/JPEG/WebP)

/3dtiles/                                      3D Tiles landing page
/3dtiles/collections                           3D Tiles collection listing
/3dtiles/collections/{id}                      3D Tiles collection detail (times, representations)
/3dtiles/collections/{id}/tileset.json         Root tileset (?representation=points|isosurface|echotop, ?quantity=, ?datetime=, ?threshold=, ?resolution=, ?min_value=)
/3dtiles/collections/{id}/content.pnts         Point cloud content (?quantity=, ?datetime=, ?min_value=)
/3dtiles/collections/{id}/content.glb          Mesh content — isosurface or echo-top (?representation=, ?quantity=, ?datetime=, ?threshold=, ?resolution=)
/3dtiles/collections/{id}/voxel/tileset.json   Cylindrical voxel tileset (?quantity=, ?datetime=, ?resolution=)
/3dtiles/collections/{id}/voxel/subtrees/*     Implicit tiling subtree files
/3dtiles/collections/{id}/voxel/content/*      Voxel glTF content chunks
/3dtiles/viewer                                Bundled CesiumJS SPA with collection/quantity/representation/resolution pickers and time scrubber

/preview                                       Built-in MapLibre SPA (cards + map for every collection)
/preview/manifest.json                         Aggregated discovery JSON consumed by the SPA
/preview/{*path}                               Embedded SPA assets (HTML, JS, CSS, vendored MapLibre)

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
# metatile_cache_mb = 1024              # optional, global WMS meta-tile cache (MB); 0 disables meta-tiling

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
| `colormaps_dir` | no | — | Directory of palette files loaded as named colormaps (one per file, name = file stem). Formats: `.toml` (colormap fields), GMT `.cpt`, GRLevelX/RadarScope `.pal` (Color/Color4/SolidColor[4] with per-bin gradients, `Scale:`/`Offset:` inverted to data units, RadarScope mask tokens collapsed to one ramp, `ND:` → nodata color, `RF:` ignored), GDAL color-relief `.txt`/`.clr`, SLD `.sld` (ColorMap). Resolved relative to the config file's directory; re-read on reload. Missing directory is a hard error; other extensions are skipped (`.disabled` works). |
| `watch_collections_dir` | no | `false` | Auto-reload when files in `collections_dir` are added, changed, or removed. Debounced; runs on the background runtime. See trust-model note in [Per-File Collection Configs](#per-file-collection-configs). |
| `watch_debounce_ms` | no | `500` | Coalesce-window in milliseconds for the filesystem watcher (only used when `watch_collections_dir = true`). |
| `metatile_cache_mb` | no | `1024` | Size (MB) of the global Web Mercator meta-tile cache. Server-wide, not per-collection. `0` disables meta-tiling (EPSG:3857 GetMap reverts to a direct render; reload-reversible). Consumed by WMS today; Maps/Tiles will share it when meta-tiling extends to them. |

### Collection Config Fields

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `id` | yes | — | Unique collection identifier, used in URL paths |
| `title` | yes | — | Human-readable collection title |
| `description` | yes | — | Collection description |
| `data_path` | yes* | — | Path to data file (CSV, GeoJSON) or directory (GeoTIFF) |
| `apis` | no | `["edr"]` | Which APIs expose this collection: `"edr"`, `"features"`, `"maps"`, `"tiles"`, `"wms"`, `"3dtiles"` |
| `engine_type` | no | `"csv"` | Data engine: `"csv"`, `"geojson"`, `"geotiff"`, `"grib"`, `"odim"` (radar composite), `"odim-volume"` (radar polar volumes), `"querydata"`, `"zarr"`, `"postgis"` |
| `keywords` | no | — | Array of discovery keyword strings, e.g. `["radar", "reflectivity"]`. Surfaced in collection JSON, WMS capabilities, and matched by `/collections?q=`. |
| `license` | no | — | `[collections.license]` table with `title` (required — SPDX id or human name) and optional `url`. When `url` is omitted and `title` is an SPDX id, the URL is synthesized from `https://spdx.org/licenses/<id>.html`. |
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

# Optional: bound the /preview SPA's time slider for collections whose
# archive is far wider than the useful scrubbing range (e.g. a STAC
# catalogue spanning years of 5-min mosaics). ISO 8601 positive duration.
# Only affects the preview manifest — the underlying engine is unchanged.
[preview]
time_window = "PT12H"
```

**Key behaviors:**
- **Coexists with inline collections.** Both `[[collections]]` in `config.toml` and files in `collections_dir` are loaded and merged. Inline collections load first, then directory files sorted alphabetically by filename.
- **Duplicate IDs are rejected.** If the same `id` appears in both inline config and a directory file (or in two directory files), startup fails with an error naming both sources.
- **Only `.toml` files are loaded.** Rename to `.toml.disabled` (or any non-`.toml` extension) to disable a collection without deleting the file.
- **`id` is required in file content**, not derived from the filename. A warning is logged if the filename stem differs from the `id`.
- **Missing directory is a hard error.** If `collections_dir` is set but the directory doesn't exist, the server refuses to start.
- **Hot-reload picks up changes.** `POST /admin/collections/reload` re-reads the directory, loading new files, removing deleted ones, and applying edits.
- **Filesystem watcher** (`watch_collections_dir = true`) triggers the same reload automatically on add/edit/remove — debounced, no manual reload needed. **Trust model:** the watcher is authorized by *filesystem write access to `collections_dir`*, not the HTTP `ADMIN_TOKEN` that gates `POST /admin/collections/reload`. Anyone who can write a collection file already controls what the server serves. When both are active, a startup WARN makes the asymmetry explicit. Keep `collections_dir` writable only by trusted principals (avoid shared/NFS mounts).

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
id = "radar-no-composite-dbzh-stac-cog"
title = "MET Norway — radar reflectivity mosaic (STAC, COG)"
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

GRIB2 files from NWP models. The engine discovers data via index sidecar files, fetches individual GRIB messages via byte-range reads. Primary targets: ECMWF IFS open data on S3 (JSON-lines index) and NOAA GFS (wgrib2 index).

**Requirements:**
- **Index sidecar files** — either ECMWF JSON-lines (default, `_offset`/`_length` per message) or wgrib2 colon-separated text (set `index_format = "wgrib2"`).
- **Regular lat/lon grid** — Template 3.0 (equidistant cylindrical) only.
- **Data source:** S3/HTTP remote (default) or a local directory (`data_path`).

**Data access pattern:**
1. Poll S3 prefix (or local directory) for index files (lightweight, ~35 KB each)
2. Parse index → build catalog: `(reference_time, step) → (file_url, message_offsets)`
3. On query: byte-range read for the specific GRIB message (~500 KB per surface field)
4. Decode message → regular lat/lon grid → serve via EdrEngine/MapEngine

**Multi-parameter collections:** Unlike GeoTIFF (one band per collection), a GRIB collection exposes all parameters from the data source. EDR queries select parameters via `parameter-name`. MapEngine uses per-parameter WMS layers.

**Automatic unit conversion** (config-free):

Unit conversion is driven by the WMO `(discipline, category, parameter_number)` triple read from each GRIB message, not by short-name tables. Source units come from WMO Code Table 4.2 plus per-center overlays for local parameter numbers 192–254.

| Source Unit | Display Unit | Conversion |
|-------------|-------------|------------|
| K | °C | −273.15 |
| Pa | hPa | ×0.01 |
| kg m⁻² | mm | ×1 (accumulated liquid-equivalent, WMO standard triples) |
| m (metres of water) | mm | ×1000 (ECMWF local params 193/198/254) |
| m² s⁻² | gpm | ÷9.80665 |
| proportion (0–1) | % | ×100 |

Adding a new provider only requires new center-overlay entries if it uses local parameter numbers 192–254. Providers that use standard WMO triples (ECMWF IFS, NOAA GFS) are handled automatically.

**Supported compression:**

| Method | Notes |
|--------|-------|
| Simple packing (5.0) | Pure Rust |
| Complex packing (5.2, 5.3) | Pure Rust |
| CCSDS/AEC (5.42) | Requires `libaec-sys` (C dep). Used by ECMWF. |

#### GRIB Config Fields

Either `data_path` **or** `endpoint`+`bucket` must be set (mutually exclusive).

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `data_path` | * | — | Local directory of `.grib2` + index files, or an `s3://`/`http(s)://` fixed-prefix URL. Mutually exclusive with `endpoint`+`bucket`. |
| `endpoint` | * | — | S3-compatible endpoint URL. |
| `bucket` | * | — | S3 bucket name. |
| `prefix_pattern` | * | — | Object prefix with optional strftime date templates (e.g. `"%Y%m%d/00z/ifs/0p25/oper/"`). Required for S3; optional literal sub-prefix for `data_path`. |
| `index_format` | no | `"ecmwf-json"` | Index format: `"ecmwf-json"` (JSON-lines, ECMWF open data) or `"wgrib2"` (colon-separated text, NOAA GFS). |
| `index_suffix` | no | `".index"` | Suffix for index sidecar files |
| `data_suffix` | no | `".grib2"` | Suffix for GRIB data files |
| `poll_interval_secs` | no | `300` | Poll interval in seconds |
| `max_runs` | no | none | Keep only the N most recent forecast runs |
| `time_window` | no | none | ISO 8601 duration for valid time filtering |
| `parameters` | no | all | Optional parameter filter, e.g., `["2t", "msl", "tp"]`. Strongly recommended with `index_format = "wgrib2"` (a single GFS file can have ~700 messages). |
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

### ODIM Radar (HDF5)

Native weather-radar data in the ODIM_H5 format, read with a pure-Rust HDF5 parser (no `libhdf5` dependency). Two engine types share the reader:

#### `odim` — 2-D composites

One pre-projected reflectivity composite per timestep (FMI / DMI / SMHI / OPERA). Single parameter per collection. Source is a local directory or S3 (`endpoint` + `bucket` + `prefix_pattern`). Implements `EdrEngine` (position, area) and `MapEngine` (WMS / Maps / Tiles).

```toml
[[collections]]
id = "radar-opera"
title = "OPERA Radar Composite"
description = "European radar reflectivity composite"
engine_type = "odim"
apis = ["edr", "wms", "maps", "tiles"]
data_path = "testdata/radar-opera"     # local directory (a top-level CollectionConfig field)

[collections.odim]
filename_template = "OPERA@%Y%m%dT%H%M@0@ACRR.h5"
parameter = "reflectivity"             # required for COMP collections
unit = "dBZ"                           # required for COMP collections
# ...or stream from S3 instead of data_path (set inside [collections.odim]):
# endpoint = "https://s3.example.com"
# bucket = "radar"
# prefix_pattern = "%Y/%m/%d/"
poll_interval_secs = 300

[collections.wms]
colormap = "radar_dbz"
```

#### `odim-volume` — native polar volumes (PVOL), one collection per radar site

A single `odim-volume` source scans a directory / S3 prefix of `.h5` **polar volumes** spanning a whole radar *network*, then **auto-expands into one OGC collection per radar site** (ODIM `nod`), with id `{base_id}-{nod}` (e.g. `radar-fi-volume-fivih`). There is no network-level aggregate collection — each radar is its own collection.

- **Parameters are the bare ODIM quantities** — `DBZH`, `TH`, `VRADH`, `ZDR`, `RHOHV`, `KDP`, … The site *is* the collection (its single EDR location, its spatial/vertical extent), so the `nod` is not part of the parameter name.
- **Per-quantity WMS colormaps** via `[[wms.parameters]]` (with the bare quantity as `name`) on the source config — every per-site collection inherits them, so reflectivity, velocity, and dual-pol moments each get a fitting palette instead of one stretched over all of them.
- **Vertical dimension = elevation angle.** EDR `z` (and WMS `ELEVATION`) selects the sweep.
- **EDR** supports `position`, `locations` (the radar site), `area`, and **`trajectory`** — vertical cross-sections (RHI-like `Section` coverages, with `z` = height above the antenna via the 4/3-Earth beam model). `MapEngine` renders each quantity to WMS / Maps / Tiles.
- New radar sites that appear in the source surface as new collections on the next `POST /admin/collections/reload`.

```toml
[[collections]]
id = "radar-fi-volume"
title = "FMI radar polar volumes"
description = "Finnish radar polar volumes (ODIM_H5 PVOL) — one collection per site"
engine_type = "odim-volume"
apis = ["edr", "wms", "maps", "tiles"]
data_path = "testdata/radar-fmi-pvol"     # or S3: endpoint + bucket + prefix_pattern

[collections.odim]
poll_interval_secs = 300

[collections.wms]
colormap = "radar_dbz"          # fallback for any unlisted quantity

# Per-quantity palettes (name = bare quantity); inherited by every site.
[[collections.wms.parameters]]
name = "DBZH"
colormap = "radar_dbz"

[[collections.wms.parameters]]
name = "VRADH"                   # Doppler velocity — diverging palette
min = -48.0
max = 48.0
# color_stops = [...]            # blue → white → red about zero

[[collections.wms.parameters]]
name = "RHOHV"                   # correlation coefficient 0..1
colormap = "viridis"
min = 0.0
max = 1.0
```

A bare `LAYERS=radar-fi-volume-fivih` WMS request (or a Maps/Tiles request with no `parameter-name`) renders the site's primary quantity; use `LAYERS=radar-fi-volume-fivih/DBZH` to pick a specific moment. The layer is the full per-site collection id (`{base_id}-{nod}`) — replace `radar-fi-volume` with your source `id` and `fivih` with the radar's ODIM `nod`.

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
| `max_runs` | no | `4` | Number of most-recent `.sqd` files to retain as model runs. Each file is keyed by its origin (analysis) time and exposed as an EDR instance and a `RasterInfo.reference_times` entry. Set `1` for latest-only (no history). |

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

### Zarr

Cloud-native multidimensional arrays (Zarr V2/V3) with CF-conventions metadata. Implements `EdrEngine` (EDR position queries) and `MapEngine` (WMS/Maps/Tiles rendering) — one layer/parameter per data variable. The Zarr format and its codec pipeline (blosc/zstd/gzip/crc32c, sharding, transpose) are handled by the [`zarrs`](https://crates.io/crates/zarrs) crate; this engine adds CF semantics, the OGC domain mapping, the storage bridge, and the poll-and-swap lifecycle.

**Key characteristics:**
- **Multi-variable:** every geographic data variable in the store becomes a parameter (and a WMS/Maps/Tiles layer). Restrict with the `parameters` filter.
- **CF-conventions decoding:** CF time axis, CF packing (`scale_factor`/`add_offset`/`_FillValue`/`missing_value` plus the array's own Zarr fill value). A time axis is **required** (EDR PointSeries needs a `t`).
- **Storage:** local directory, S3 (`endpoint`+`bucket`+`path`), or HTTP — all via `ds-storage`, with byte-range chunk reads cached in an LRU keyed on full chunk objects (`cache_mb`).
- **EDR position queries** use bilinear interpolation; ascending/descending and irregular axes are handled via CF axis location.
- **Rendering** reads a 2-D spatial window covering the request bbox (+1 cell margin), then samples per output pixel (geographic/WebMercator) or via a coarse projection grid (projected output CRS — never per-pixel projection).
- **Poll-and-swap:** the store is re-read on `poll_interval_secs` so appended time steps surface without a reload; `RasterInfo` is served from a cached snapshot.

**Supported grids:** geographic (WGS84 lat/lon) only. Projected metre axes are detected and skipped (not mistaken for degrees). A startup WARN fires for pathological chunk shapes (e.g. `time=1, lat=full, lon=full` — one full-domain chunk per timestep is bad for point/time-series queries; the engine still serves).

**Forecast (reference + lead) handling:** when a store has a CF `forecast_reference_time` axis (model run) **and** a `forecast_period`/lead axis (e.g. dynamical.org AIFS/GFS/ICON-EU), the engine uses the **latest run** and exposes valid time (= run + lead) as the time axis. Selecting older runs (EDR instances / WMS `reference_time`) is tracked in [#337](https://github.com/mrauhala/meteocore/issues/337).

> **Note:** the engine returns stored values as-is — it does **not** convert units. For datasets where `temperature_2m` is already in °C (e.g. the dynamical.org forecasts), set the colormap range in °C.

#### Data Source Modes

| Mode | Config | Description |
|------|--------|-------------|
| Local directory | `data_path = "path/to/store.zarr"` | Local Zarr store root |
| Local/remote URL | `data_path = "s3://…"` or `"http(s)://…"` | Store root as a URL |
| S3 | `endpoint` + `bucket` + `path` | Store at `path` within the bucket |

`data_path` and the `endpoint`/`bucket` source are mutually exclusive.

#### Zarr Config Fields

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `data_path` | * | — | Local store root directory, or an `s3://` / `http(s)://` URL. Mutually exclusive with `endpoint`+`bucket`. |
| `endpoint` | * | — | S3-compatible endpoint URL. Required with `bucket`. |
| `bucket` | * | — | S3 bucket name. Required when `endpoint` is set. |
| `path` | no | — | Store path within the bucket (S3); optional sub-path for a local `data_path`. |
| `zarr_version` | no | auto | Metadata version `2` or `3` (advisory — `zarrs` auto-detects). |
| `parameters` | no | all | Only expose these variables as parameters. |
| `poll_interval_secs` | no | `300` | Store re-read cadence in seconds. |
| `cache_mb` | no | `256` | Chunk LRU cache size in MB (most useful for S3/HTTP). |
| `icechunk` | no | — | `[zarr.icechunk]` table → read an Icechunk repo instead of plain Zarr (see below). Requires the `icechunk` feature. |

\* One of `data_path` **or** `endpoint`+`bucket` must be set.

#### Zarr Config Example (local)

```toml
[[collections]]
id = "zarr-era5-t2m-local"
title = "ERA5-like 2 m Temperature (Zarr V3, local)"
description = "Synthetic CF-conventions Zarr V3 store served from a local directory"
engine_type = "zarr"
apis = ["edr", "wms", "maps", "tiles"]

[collections.zarr]
data_path = "testdata/zarr-era5-t2m"   # store root; or s3://… / http(s)://…
zarr_version = 3
poll_interval_secs = 300
# parameters = ["t2m"]                  # optional — default is every variable

[collections.wms]
colormap = "temperature"

# One WMS/Maps/Tiles layer per variable.
[[collections.wms.parameters]]
name = "t2m"
colormap = "temperature"
min = 250.0
max = 300.0
```

A committed fixture lives at `testdata/zarr-era5-t2m`; regenerate it with `cargo run -p engine-zarr --example gen_fixture`.

#### Icechunk

[Icechunk](https://icechunk.io/) is a transactional, versioned storage format for Zarr (used by the dynamical.org AIFS/GFS/ICON-EU public datasets). Adding a `[collections.zarr.icechunk]` table makes the source an Icechunk repository instead of a plain Zarr store — its presence selects the backend.

**Build requirement:** Icechunk support is **off by default** and gated behind the `icechunk` Cargo feature (it pulls in `icechunk` + `zarrs_icechunk`). Build with:

```bash
cargo build --release -p server --features icechunk
cargo run -p server --features icechunk
```

If a config sets `[collections.zarr.icechunk]` but the binary was built without the feature, the engine errors clearly at load.

**Details:**
- The repo location reuses `data_path` (local) or `endpoint`+`bucket`+`path` (S3); the table selects the **version** to read.
- The S3 backend uses Icechunk's `object_store` backend (not `aws-sdk-s3`), reusing the same crate `ds-storage` uses (keeps the icechunk feature's binary cost ~8 MB instead of ~28 MB).
- **Public datasets only.** Anonymous S3 access is used; there is no credential configuration.
- New snapshots on a branch are picked up on **reload** (`POST /admin/collections/reload`), not on poll.

##### Icechunk Config Fields (`[collections.zarr.icechunk]`)

At most one of `branch` / `tag` / `snapshot` may be set; the default is the HEAD of branch `main`.

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `branch` | no | `main` | Read the HEAD of this branch. |
| `tag` | no | — | Read this tag. |
| `snapshot` | no | — | Read this exact (immutable) snapshot id. |
| `region` | no | — | S3 region for the repo's object store (needed for AWS; ignored for local). |
| `force_path_style` | no | `true` | S3 path-style addressing. Set `false` for virtual-host style. |

##### Icechunk Config Example (public S3)

```toml
[[collections]]
id = "ecmwf-aifs-single"
title = "ECMWF AIFS-single forecast (Icechunk)"
description = "ECMWF AIFS deterministic AI forecast, global 0.25°, Icechunk on public S3 (dynamical.org)"
engine_type = "zarr"
apis = ["edr", "wms", "maps", "tiles"]

[collections.license]
title = "CC-BY-4.0"

[collections.zarr]
endpoint = "https://s3.us-west-2.amazonaws.com"
bucket = "dynamical-ecmwf-aifs-single"
path = "ecmwf-aifs-single-forecast/v0.1.0.icechunk"
parameters = ["temperature_2m"]

# Presence of this table selects the Icechunk backend.
[collections.zarr.icechunk]
branch = "main"
region = "us-west-2"

[collections.wms]
colormap = "temperature"

[[collections.wms.parameters]]
name = "temperature_2m"
colormap = "temperature"
min = -30.0       # temperature_2m is already in °C — no unit conversion
max = 40.0
```

Ready-to-use disabled examples ship in `collections.d/` (`ecmwf-aifs-single.toml.disabled`, `noaa-gfs-icechunk.toml.disabled`, `dwd-icon-eu.toml.disabled`) — rename to drop `.disabled` and run with `--features icechunk`.

## OGC 3D Tiles

Volumetric weather data served as OGC 3D Tiles 1.1. Currently only the `odim-volume` (polar volume) engine implements `VolumeEngine`. Add `"3dtiles"` to a collection's `apis` to enable it.

### Representations

Each per-site PVOL collection exposes four product representations selectable at request time:

| `?representation=` | Content | Endpoint |
|--------------------|---------|----------|
| `points` (default) | `.pnts` point cloud — one point per radar echo cell, placed at its true ECEF position via the 4/3-Earth beam model | `content.pnts` |
| `isosurface` | glTF `.glb` isosurface mesh (marching tetrahedra) — a constant-value shell such as the 20 dBZ surface, with nested translucent shells supported via `?threshold=20,35,50` | `content.glb` |
| `echotop` | glTF `.glb` echo-top column mesh — extruded bins from ground up to the highest echo ≥ threshold, height-coloured | `content.glb` |
| `voxels` | glTF `.glb` cylindrical voxel grid (`EXT_primitive_voxels` draft extension, CesiumJS ≥ 1.142 only) | `voxel/tileset.json` + content |

### Time-Dynamic Playback

Each tileset includes a `times` manifest of all available volume timestamps (RFC 3339 `…Z`, sorted ascending). The bundled CesiumJS viewer preloads one tileset per timestamp and animates by toggling visibility — scrubbing and playback require no additional network requests per frame.

### Routes and Query Parameters

| Route | Key parameters |
|-------|----------------|
| `GET /3dtiles/collections/{id}/tileset.json` | `representation`, `quantity`, `datetime`, `min_value`, `threshold` (comma list ≤5, isosurface only), `resolution` (`low`/`med`/`high`) |
| `GET /3dtiles/collections/{id}/content.pnts` | `quantity`, `datetime`, `min_value` |
| `GET /3dtiles/collections/{id}/content.glb` | `representation`, `quantity`, `datetime`, `threshold`, `resolution` |
| `GET /3dtiles/collections/{id}/voxel/tileset.json` | `quantity`, `datetime`, `resolution` |
| `GET /3dtiles/viewer` | Built-in CesiumJS SPA; `?base=` overrides API origin |

`datetime` accepts any RFC 3339 instant and selects the nearest available volume; omitting it selects the latest. `?datetime=` values that exactly match an advertised volume time receive `Cache-Control: max-age=86400, immutable`; others receive `max-age=60`.

### Config

```toml
[[collections]]
id = "radar-fi-volume"
title = "FMI radar polar volumes"
engine_type = "odim-volume"
apis = ["edr", "wms", "maps", "tiles", "3dtiles"]
data_path = "testdata/radar-fmi-pvol"

[collections.odim]
poll_interval_secs = 300
# max_files = 48    # retain last 48 volumes for time-dynamic playback
```

### Caching

Two complementary LRU caches reduce per-request compute:

- **Content cache** (`MC_3DTILES_CONTENT_CACHE_MB`, default 512 MB) — caches encoded `.pnts`/`.glb` bytes keyed by (collection, product, quantity, datetime, params, dims) plus a data-version hash derived from `VolumeInfo.times`. Concurrent identical requests share one compute via single-flight coalescing.
- **Voxel-grid cache** (`MC_PVOL_VOXEL_GRID_CACHE_MB`, default 512 MB, engine-side) — caches the polar-resampled `VoxelGrid` (keyed by file + quantity + dims) so isosurface, echo-top, and voxels all share one resample pass. Threshold changes re-use the cached grid.

Cache metrics: `tiles3d_content_cache_*` and `pvol_voxel_grid_cache_*` in `/metrics`.

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

A single `/wms/` endpoint dispatches on the `REQUEST` query parameter:

- **GetCapabilities** — XML capabilities listing layers, CRS, extents, time dimension, and styles
- **GetMap** — render a map image as PNG, JPEG, or WebP
- **GetLegendGraphic** — render a colormap legend strip

WMS, OGC API - Maps, and OGC API - Tiles share the same `MapEngine`, render semaphore, rendered-image cache, and colormaps. Only the HTTP layer is WMS-specific.

### Layer Names

A layer name is either:

- `{collection-id}` — for single-parameter engines (e.g. GeoTIFF)
- `{collection-id}/{parameter}` — for multi-parameter engines (GRIB, multi-param QueryData)

For multi-parameter engines, GetCapabilities emits a non-requestable parent layer (no `<Name>`) wrapping one child layer per parameter. `LAYERS=ecmwf-ifs/2t` requests the `2t` parameter; `LAYERS=ecmwf-ifs` returns `LayerNotDefined` because the parent is not directly requestable. A parameter that the engine doesn't advertise also returns `LayerNotDefined` (rather than silently falling back to the default), so callers can't cache a wrong-parameter image under a typo.

### GetMap Parameters

| Parameter | Required | Description |
|-----------|----------|-------------|
| `SERVICE` | yes | Must be `WMS` |
| `VERSION` | yes | Must be `1.3.0` |
| `REQUEST` | yes | `GetMap` |
| `LAYERS` | yes | One layer name (see [Layer Names](#layer-names)) — exactly one |
| `CRS` | yes | `CRS:84`, `EPSG:4326`, `EPSG:3857`, `EPSG:3067`, or `EPSG:3035` |
| `BBOX` | yes | Bounding box — axis order depends on CRS (see below) |
| `WIDTH` | yes | Image width in pixels (max 8000) |
| `HEIGHT` | yes | Image height in pixels (max 8000) |
| `FORMAT` | yes | `image/png`, `image/jpeg`, or `image/webp` |
| `STYLES` | no | Style name (empty or missing = `default`) |
| `TIME` | no | ISO 8601 timestamp; defaults to the latest available |
| `TRANSPARENT` | no | Accepted but currently a no-op — PNG/WebP output is always RGBA |
| `BGCOLOR` | no | Accepted but ignored |

### WMS 1.3.0 BBOX Axis Order

WMS 1.3.0 axis order is CRS-dependent:

| CRS | Axis order | Example for the same area |
|-----|------------|---------------------------|
| `CRS:84` | lon/lat | `BBOX=10,55,30,70` |
| `EPSG:4326` | **lat/lon — swapped!** | `BBOX=55,10,70,30` |
| `EPSG:3857` | easting/northing (meters) | `BBOX=1113194,7361866,3339584,11271098` |
| `EPSG:3067`, `EPSG:3035` | easting/northing (meters) | — |

Internally the handler normalizes everything to WGS84 `[west, south, east, north]`. Test with both `CRS:84` and `EPSG:4326` if you maintain a client — getting this wrong silently rotates the image.

### GetLegendGraphic Parameters

| Parameter | Required | Default | Description |
|-----------|----------|---------|-------------|
| `LAYER` (or `LAYERS`) | yes | — | Collection ID (`{id}/{param}` is accepted but the legend depends only on the collection's styles) |
| `STYLES` | no | `default` | Style name. Note: only the plural form works on this endpoint, unlike GetMap which accepts both. Tracked in #165. |
| `WIDTH` | no | `40` | Pixels, capped at 256 |
| `HEIGHT` | no | `200` | Pixels, capped at 1024 |
| `FORMAT` | no | `image/png` | `image/png`, `image/jpeg`, or `image/webp`. Any other value returns `InvalidFormat`. |

Legends are static — they always carry `Cache-Control: public, max-age=86400, immutable`.

### Errors

Errors are returned as `ServiceExceptionReport` XML with WMS 1.3.0 error codes: `LayerNotDefined`, `StyleNotDefined`, `CRSNotDefined`, `InvalidDimensionValue`, `MissingParameterValue`, `InvalidFormat`, `InvalidParameterValue`, `OperationNotSupported`. HTTP status is 400 for client errors, 503 when the render semaphore is saturated (`Server busy, try again later`), and 500 for internal errors (the message is redacted in the response body; the original detail is captured via `tracing::warn!` for operators).

### Rendering Pipeline

1. Parse and validate WMS parameters; normalize BBOX to WGS84 `[west, south, east, north]`.
2. Resolve `MapEngine` by collection ID. For `{id}/{param}` layers, validate `{param}` against `RasterInfo::parameters`.
3. Build a cache key from quantized bbox, layer, style, format, CRS, width, height, time, and parameter.
4. Check the rendered-image cache (shared LRU). On hit: compare the cached entry's content-derived ETag against `If-None-Match` — match → `304` with `X-Cache: HIT`, otherwise return the cached bytes with `X-Cache: HIT`.
5. On miss: acquire a render semaphore permit (timed out → 503). Run `MapEngine::get_raster_tile()` on a blocking thread — the engine projects the bbox into the source CRS, picks the best overview, reads source tiles, and resamples (nearest-neighbor).
6. Empty (all-nodata) tile: encode a transparent PNG with `Content-Type: image/png`. Error: render the red error tile (WMS only). Neither is inserted into the cache.
7. Populated tile: colorize (LUT for discrete data, linear gradient for continuous), encode to PNG/JPEG/WebP, wrap in `CachedRendered` (which derives the ETag via FNV-1a over the bytes), and insert into the cache.
8. Compare the freshly-computed ETag against `If-None-Match` — match → `304` carrying the same `X-Cache` label the 200 would have (`MISS`, `EMPTY`, or `ERROR`), otherwise return the body with `X-Cache: MISS | EMPTY | ERROR`. Revalidations stay categorised the same as initial fetches on dashboards.

### Styling (shared with Maps and Tiles)

Styles live under each collection's `[wms]` block — Maps and Tiles read the same configuration via the shared `MapEngine`/`StyleInfo` registry, so a style defined here is available to all three APIs.

**Built-in colormaps:**

| Name | Description | Default range |
|------|-------------|---------------|
| `radar_dbz` | Standard radar reflectivity ramp | 0–70 dBZ |
| `radar_smhi` | SMHI radar reflectivity ramp (gray below-threshold) | -30–70 dBZ |
| `radar_fmi` | FMI summer radar reflectivity ramp | -32–58 dBZ |
| `radar_bookbinder` | Bookbinder 8-bit Z curve | -32–94.5 dBZ |
| `radial_velocity` | Doppler radial velocity, diverging blue → white → red | ±48 m/s |
| `grayscale` | Linear black → white | 0–1 |
| `viridis` | Perceptually uniform (good default) | 0–1 |
| `temperature` | Purple → blue → cyan → green → yellow → red | -40 to 50 °C |
| `precipitation` | Transparent → blue → purple → white | 0–50 mm |
| `precipitation_rate` | Precipitation-rate ramp | 0–30 mm/h |
| `wind_speed` | Green → yellow → orange → red → purple | 0–50 m/s |
| `pressure` | Mean sea-level pressure, purple low → neutral → red high | 950–1050 hPa |
| `humidity` | Relative humidity, dry brown → pale → green → blue | 0–100 % |
| `cloud_cover` | White overlay with increasing opacity | 0–100 % |
| `cap_severity` | CAP alert severity codes, grey/green/yellow/orange/red | 0–4 |
| `lightning_age` | Lightning strike age, near-white → orange → dark violet | 0–60 min |

**Per-parameter default styles** (#320) — parameters of multi-parameter
collections (GRIB, QueryData, Zarr, radar volumes) with no explicit style
are matched against a built-in defaults table by normalized name/title plus
the collection's unit: `t2m`/`2t`/`TMP` (unit K or C) → `temperature`,
`msl`/`mslp` (Pa or hPa) → `pressure`, `DBZH`/`dbz` → `radar_dbz`,
`VRADH` → `radial_velocity`, humidity/cloud/wind/precipitation likewise.
Unit-gated rules never guess (a temperature with no unit hint stays on the
collection style). Defaults win over the collection-level colormap for
those parameters; opt out per collection with
`[wms] parameter_defaults = false`, or add/override rules globally with
top-level `[[parameter_defaults]]` blocks (`names`/`contains`, `colormap`,
`min`/`max` or `[[parameter_defaults.unit_ranges]]`).

**Named custom colormaps** — define once, reference anywhere a built-in
name works (`[wms] colormap`, `[[wms.styles]]`, `[[wms.parameters]]`, style
bundles). Only in top-level `config.toml` (like `[[style_bundles]]`), or as
files in `colormaps_dir`:

```toml
[[colormaps]]
name = "radar_house"
title = "House Radar Style"
# interpolation = "step"      # "linear" (default) | "step" (discrete classes)
# nodata_color = "#00000000"  # optional
color_stops = [
  { value = 5.0,  color = "#414141" },
  { value = 60.0, color = "#FF0000" },
]
```

A user colormap may shadow a built-in name (replacing it server-wide; logged
as a warning). Duplicate user names and unknown `colormap = "..."`
references anywhere in config are load errors — a typo fails startup/reload
instead of silently rendering viridis.

**Custom color stops** override the built-in colormap:

```toml
[collections.wms]
[[collections.wms.color_stops]]
value = 0.0
color = "#00000000"
[[collections.wms.color_stops]]
value = 50.0
color = "#FF0000"
```

**Named styles** add alternatives next to `default`. `name` defaults to
the `colormap` reference and `title` to the referenced palette's title, so
a pure palette reference is a one-liner (`colormap = "x"`); set either
field to override:

```toml
[[collections.wms.styles]]
name = "grayscale"
title = "Grayscale"
colormap = "grayscale"
min = 0.0
max = 70.0
```

Or attach a reusable `[[style_bundles]]` block defined in top-level `config.toml` via `style_bundle = "..."` — see the [Configuration](#configuration) section.

**`[wms]` config fields:**

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `style_bundle` | no | — | Name of a shared `[[style_bundles]]` block. Merges with the inline fields below (bundles v2): inline wins slot-wise (palette source / min / max), per parameter; named styles union with inline winning name clashes. Bundles can carry shared `[[style_bundles.parameters]]` blocks. |
| `colormap` | no | `viridis` | Built-in colormap for the default style |
| `color_stops` | no | — | Array of `{value, color}` entries. Overrides `colormap` for the default style. |
| `min` | no | from colormap | Minimum value for the default style's range |
| `max` | no | from colormap | Maximum value for the default style's range |
| `styles` | no | — | Array of named styles |
| `parameters` | no | — | Per-parameter default-style overrides (multi-parameter engines) |
| `rendered_cache_mb` | no | `512` | Shared rendered-image cache size in MB. Set to 0 to disable. (Global cache; lives under `[wms]` for backward compatibility — see note.) |

### Limits

| Limit | Value |
|-------|-------|
| `MAX_MAP_PIXELS` (`WIDTH × HEIGHT`) | 64,000,000 |
| `MAX_MAP_DIMENSION` (`WIDTH` or `HEIGHT`) | 8,000 px |
| Render permits | 2× CPU cores (minimum 8) |
| `LAYERS` count | exactly 1 |
| Supported CRS | `CRS:84`, `EPSG:4326`, `EPSG:3857`, `EPSG:3067`, `EPSG:3035` |
| Supported `FORMAT` (GetMap) | `image/png`, `image/jpeg`, `image/webp` |
| Supported `FORMAT` (GetLegendGraphic) | `image/png`, `image/jpeg`, `image/webp` |

## OGC API - Maps

REST-based map image API. Maps shares the `MapEngine` trait, render semaphore, rendered-image cache, and style/colormap configuration with WMS and Tiles — only the HTTP layer differs.

### Routes

| Route | Description |
|-------|-------------|
| `GET /maps/collections/{id}/map` | Render with the default style |
| `GET /maps/collections/{id}/styles/{styleId}/map` | Render with a named style (defined under `[wms]`) |
| `GET /maps/collections/{id}/styles` | List available styles for a collection |
| `GET /maps/collections/{id}` | Collection metadata (extent, styles, supported CRS) |

### Query Parameters

| Parameter | Required | Default | Description |
|-----------|----------|---------|-------------|
| `bbox` | yes | — | `west,south,east,north`, always lon/lat order |
| `bbox-crs` | no | `CRS:84` | Only `CRS:84` (or `http://www.opengis.net/def/crs/OGC/1.3/CRS84`) is accepted — every other value returns 400 |
| `width` | no | `256` | Image width in pixels, max 8000 |
| `height` | no | `256` | Image height in pixels, max 8000 |
| `crs` | no | `CRS:84` | Output CRS: `CRS:84`, `EPSG:4326`, `EPSG:3857`, `EPSG:3067`, or `EPSG:3035` |
| `datetime` | no | latest | ISO 8601 instant; defaults to the latest timestep advertised by the engine |
| `f` | no | `image/png` | `image/png`, `image/jpeg`, or `image/webp` |
| `parameter-name` | no | engine default | Selects a parameter on multi-parameter raster engines (GRIB, multi-param QueryData). Single-parameter engines ignore the value. Unknown names against a multi-parameter engine return 400. Non-OGC for OGC API - Maps today, but the `/preview` SPA dropdown depends on it. |
| `transparent` | no | — | Accepted but currently a no-op |

`width × height` is additionally capped at `MAX_MAP_PIXELS = 64,000,000` (= 8000²), so a request at the per-dimension cap never trips the pixel cap with a confusing second error.

### Responses

PNG/WebP responses are always RGBA (transparent for empty tiles). JPEG is RGB. Each successful response carries:

- `Content-Type: image/png|image/jpeg|image/webp`
- `Content-Crs:` OGC URI of the output CRS
- `ETag:` FNV-1a hash of the encoded response body — changes whenever the rendered pixels change, regardless of whether the cache key changed. This is what makes a server-side fix (e.g. colormap correction) invalidate stale browser caches instead of letting them serve infinite `304`s.
- `Cache-Control: public, max-age=86400, immutable` when `datetime` is set; `public, max-age=60, must-revalidate` otherwise
- `X-Cache: HIT | MISS | EMPTY | ERROR` for observability. Set on every 200 and 304 response: `HIT` for cache-hit revalidations, `MISS` for post-render revalidations, `EMPTY` for transparent-tile fast-paths, `ERROR` for the WMS error-tile fallback. The 304 carries the same label the 200 would, so revalidations stay in their original dashboard category. Operators can grep by value rather than reasoning about header absence.

`If-None-Match` is evaluated against the content-derived ETag from the cache hit or the freshly-rendered bytes — not before the cache lookup. An overloaded render semaphore returns `503 Service Unavailable` with the fixed body `{"code":"ServerBusy","description":"Server busy, try again later"}`. Internal errors return 500 with a redacted body; the original detail is captured via `tracing::warn!` for operators.

### Differences from WMS

- REST paths instead of `REQUEST=`/`LAYERS=` query parameters
- `bbox` is always lon/lat (no CRS-dependent axis swap)
- Multi-parameter selection uses `?parameter-name=`, not `LAYERS=collection/param`
- Errors are JSON, not `ServiceExceptionReport` XML

### Conformance Classes

`core`, `collection-map`, `styled-map`, `spatial-subsetting`, `scaling`, `datetime`, `crs`, `png`, `jpeg`. WebP is implemented but no `webp` conformance class is declared (none exists in the OGC API - Maps 1.0 spec).

## OGC API - Tiles

Serves raster data via `MapEngine` (sharing styles, semaphore, and rendered-image cache with WMS/Maps) and vector data via `FeatureEngine`. The same tile URL serves both — the response type is chosen by `?f=`.

### Routes

| Route | Description |
|-------|-------------|
| `GET /tiles/tileMatrixSets` | List supported tiling schemes |
| `GET /tiles/tileMatrixSets/{tileMatrixSetId}` | Tiling scheme definition (matrices, CRS, scale denominators) |
| `GET /tiles/collections/{id}` | Collection metadata (`dataType: map` or `vector`, TMS links, styles) |
| `GET /tiles/collections/{id}/tiles` | List tilesets for a collection, including `tileMatrixSetLimits` per zoom |
| `GET /tiles/collections/{id}/tiles/{tms}/{z}/{row}/{col}` | Get a tile (raster default, MVT via `?f=mvt`) |
| `GET /tiles/collections/{id}/styles/{styleId}/tiles/{tms}/{z}/{row}/{col}` | Get a styled raster tile; `?f=mvt` is rejected here |

All tiles are 256×256 pixels. Tile coordinates follow OGC convention: `tileRow` is Y top-to-bottom, `tileCol` is X left-to-right.

### Supported TileMatrixSets

| ID | CRS | Layout | Notes |
|----|-----|--------|-------|
| `WebMercatorQuad` | EPSG:3857 | 1×1 at z=0, doubling per zoom | Standard web-map scheme (Google/OSM) |
| `WorldCRS84Quad` | CRS:84 | 2×1 at z=0, doubling per zoom | Geographic (lon/lat) |

Both support `tileMatrix` 0–24 (capped by `MAX_ZOOM_LEVEL`). `tileMatrixSetLimits` advertised on `/collections/{id}/tiles` cover zooms 0 through `DEFAULT_MAX_ZOOM = 18`, which is sufficient for every collection currently shipped; clients may still request 19–24 directly.

### Query Parameters

| Parameter | Required | Default | Description |
|-----------|----------|---------|-------------|
| `f` | no | `image/png` | Raster: `image/png`, `image/jpeg`, `image/webp`. Vector: `mvt` or `application/vnd.mapbox-vector-tile`. |
| `datetime` | no | latest | ISO 8601 instant; ignored for `?f=mvt` |
| `parameter-name` | no | engine default | Same semantics as Maps `parameter-name`. Ignored for `?f=mvt`. |

`tileMatrix > 24` returns 400. Tile coordinates outside the matrix (e.g. `row=1` at `z=0` on `WebMercatorQuad`) return 404.

### Raster Tiles

Raster requests resolve a style (the `default` style for the unstyled route, or the path-specified `styleId`), then run the same render pipeline as WMS/Maps:

1. TMS → bbox conversion (Mercator math for `WebMercatorQuad`, linear lon/lat for `WorldCRS84Quad`).
2. ETag check / rendered-cache check.
3. On miss: acquire render permit (timed out → 503), call `MapEngine::get_raster_tile()` with the TMS-derived `output_crs`, colorize, encode.
4. Empty (all-nodata) tiles return a pre-generated transparent PNG with `X-Cache: EMPTY` and `Content-Type: image/png` regardless of the requested format — no engine call, no cache insert. Clients that strictly need JPEG/WebP output should check `X-Cache: EMPTY` and refetch only if needed; the alternative would be re-encoding a known-uniform buffer on every empty tile.

Successful responses include `Content-Type`, `Content-Crs` (OGC URI for the TMS's CRS), `ETag`, `Cache-Control` (`max-age=86400, immutable` when `datetime` is set, `max-age=60, must-revalidate` otherwise), and `X-Cache: HIT|MISS|EMPTY`.

### Vector Tiles (MVT)

When `?f=mvt` (or `?f=application/vnd.mapbox-vector-tile`) is requested against a collection backed by a `FeatureEngine` (GeoJSON, PostGIS, CSV's feature mode), the handler converts the tile bbox into a `FeatureQuery`, encodes the result via `ds-mvt`, and returns Mapbox Vector Tile bytes. The layer name in the MVT payload is the collection ID, matching the convention used by `mapbox-vector-tile-js` (bundled in MapLibre GL JS).

| Behavior | Detail |
|----------|--------|
| Feature limit | `MAX_FEATURES_PER_TILE = 50,000`. Exceeding it returns **422 Unprocessable Content** (`tile-too-dense`) — the request is well-formed but the data can't be served at that scale. Raise the collection's `minzoom` or narrow the bbox. |
| Polygon clipping | Features are clipped to a buffered tile envelope (1/16 of `extent`, matching MapLibre's default source-layer buffer) so seams between adjacent tiles remain invisible. |
| ETag | Content-derived (FNV-1a over the encoded bytes) — a data change yields different bytes and therefore a different ETag, so clients holding the old one revalidate. |
| Cache | `Cache-Control: public, max-age=300`. Cached in a separate `VectorTileCache` (not the raster `RenderedCache`), keyed by collection + TMS + z/x/y + properties hash + `FeatureEngine::data_version()` so a reload/refresh produces a clean miss instead of an infinite-revalidate loop. |
| Styled route | `?f=mvt` is rejected on `/styles/{styleId}/tiles/...` with 400 — vector tiles aren't styled server-side. |
| Render permit | MVT encoding shares the raster render semaphore (CPU-bound budget). Acquired *after* the feature query so engine I/O doesn't hold a slot. |

### Conformance Classes

`ogcapi-tiles-1`: `core`, `tileset`, `tilesets-list`, `png`, `jpeg`, `mvt`. `tms-2.0`: `tilematrixset`, `json-tilematrixset`. WebP is implemented but has no conformance class.

## Caching

### Rendered Image Cache (Tier 2)

Separate from the GeoTIFF source tile cache (Tier 1). Caches final PNG/JPEG/WebP bytes. Shared across WMS, Maps, and Tiles APIs.

- Default size: 512 MB (configurable via `rendered_cache_mb`)
- Cache key: quantized bbox (6 decimal places) + layer + style + format + width + height + CRS + time + parameter
- Lock-free concurrent LRU (uses `quick_cache`)
- No TTL — immutable data. Cache invalidated on collection reload.
- Error tiles and empty tiles (all nodata) are NOT cached.
- MVT vector tiles use a separate `VectorTileCache` (content-derived ETag, `Cache-Control: max-age=300`); they are NOT subject to `rendered_cache_mb`.

### Meta-Tile Cache (Web Mercator WMS)

A fullscreen WMS client requests an arbitrary bbox + size per pan/zoom, so the Tier-2 rendered cache (keyed on the exact bbox) rarely hits. For EPSG:3857 GetMap, the WMS handler instead decomposes each request into fixed 256×256 tiles aligned to the WebMercatorQuad grid, renders and caches *those* (decoded RGBA), and resamples them to the exact viewport — so the expensive decode/projection/colorize work is cached at tile granularity and reused across overlapping views.

- Default size: 1024 MB, configured server-wide via **`[server] metatile_cache_mb`** (it is a single global cache, not per-collection); **set to 0 to disable** meta-tiling (reverts to a direct single-shot render, reload-reversible). Consumed by the WMS GetMap path today; Maps/Tiles render directly and would share this cache when meta-tiling extends to them.
- Cache key: layer + parameter + style + time + elevation + ladder level + tile col/row.
- Resolution ladder: half-octave steps coinciding with standard WebMercator zooms; snaps to the finest step ≤ the request resolution (always downsampled, never upscaled).
- Web Mercator only; other CRSs and degenerate/oversized requests fall back to the direct render.
- Distinct from the per-collection GeoTIFF source tile cache (`tile_cache_*`).

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

**Meta-tile cache** (global, Web Mercator WMS decoded-RGBA tiles):

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `metatile_cache_hits_total` | counter | — | Meta-tile pixel cache hits |
| `metatile_cache_misses_total` | counter | — | Meta-tile pixel cache misses |
| `metatile_cache_bytes` | gauge | — | Bytes currently held |
| `metatile_cache_capacity_bytes` | gauge | — | Configured capacity |
| `metatile_cache_entries` | gauge | — | Number of cached entries |

**GRIB grid cache** (per-collection, decoded grid cache):

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `grid_cache_hits_total` | counter | collection | Grid cache hits |
| `grid_cache_misses_total` | counter | collection | Grid cache misses |
| `grid_cache_bytes` | gauge | collection | Bytes currently held |
| `grid_cache_capacity_bytes` | gauge | collection | Configured capacity |
| `grid_cache_entries` | gauge | collection | Number of cached entries |

**3D Tiles content cache** (global, encoded `.pnts`/`.glb`/voxel bytes):

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `tiles3d_content_cache_hits_total` | counter | — | 3D Tiles content cache hits |
| `tiles3d_content_cache_misses_total` | counter | — | 3D Tiles content cache misses |
| `tiles3d_content_cache_bytes` | gauge | — | Bytes currently held |
| `tiles3d_content_cache_capacity_bytes` | gauge | — | Configured capacity |
| `tiles3d_content_cache_entries` | gauge | — | Number of cached entries |

**PVOL voxel-grid cache** (global, polar-resampled voxel grids):

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `pvol_voxel_grid_cache_hits_total` | counter | — | Voxel-grid cache hits |
| `pvol_voxel_grid_cache_misses_total` | counter | — | Voxel-grid cache misses |
| `pvol_voxel_grid_cache_bytes` | gauge | — | Bytes currently held |
| `pvol_voxel_grid_cache_capacity_bytes` | gauge | — | Configured capacity |
| `pvol_voxel_grid_cache_entries` | gauge | — | Number of cached entries |

**Render & storage:**

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `render_semaphore_available` | gauge | — | Available render permits |
| `render_semaphore_total` | gauge | — | Total render permits (2× CPU cores, min 8) |
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

The repo ships a batteries-included observability stack for local development and smoke testing at `compose.yaml`. It uses **Docker Hardened Images** (free Community tier, non-FIPS) for every sidecar so the stack matches a realistic production profile: distroless, non-root, shell-less, CVE-scanned.

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

The bundled Grafana dashboard at `docker/grafana/dashboards/meteocore-overview.json` has a **Logs** row with four Loki-backed panels: request rate by `(api, query_type)`, error rate by `status_class`, live request stream, and a dedicated 4xx/5xx stream. All panels work with the Prometheus and Loki datasources that are auto-provisioned from `docker/grafana/provisioning/`.

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
- Tiles: WebMercatorQuad and WorldCRS84Quad only; fixed 256x256 raster tiles; MVT is supported via `?f=mvt` for `FeatureEngine`-backed collections
- GRIB: regular lat/lon grids only, GRIB2 only, requires index sidecar files; accumulated/averaged aggregate fields (`APCP`, `acc fcst`) are dropped
- QueryData: no compressed files, EDR position only, level 0 only; retains up to `max_runs` (default 4) most-recent files as model runs
- Zarr: geographic (WGS84 lat/lon) grids only, EDR position only; forecast model-run selection pins the latest run (#337); STAC per-item-CRS and kerchunk modes not yet implemented
- Zarr/Icechunk: requires the `icechunk` build feature, anonymous (public) S3 only, new snapshots picked up on reload (not poll)
- 3D Tiles: only `odim-volume` collections support `VolumeEngine`; voxel representation (`EXT_primitive_voxels`) requires CesiumJS ≥ 1.142 and is a CesiumGS draft extension (not in the Khronos registry); voxel octree/time-dynamic voxels are follow-ups; the 3D Tiles API has no `reference_time` parameter yet (model-run pinning; `datetime` selects valid time only)

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
- **zarrs** for Zarr V2/V3 array reading and codecs (with optional **icechunk** for versioned repositories)
- **CesiumJS** (vendored, bundled via `include_str!`) for the 3D Tiles viewer SPA
