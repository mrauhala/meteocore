# Metocean Data Server — Claude Instructions

## What This Is

Rust workspace implementing OGC API - EDR and OGC API - Features servers. Eight crates: `ds-core` (traits + types), `ds-storage` (S3/HTTP/local object store), `engine-csv` (CSV data engine), `engine-geojson` (GeoJSON data engine), `engine-geotiff` (GeoTIFF/COG data engine), `api-edr` (EDR HTTP layer), `api-features` (Features HTTP layer), `server` (binary).

## Build & Run

```bash
cargo build                  # Build all crates
cargo test                   # Run all tests
cargo run -p server          # Start server (reads config.toml)
cargo check -p <crate>       # Type-check a single crate
```

### Fuzz testing

Fuzz targets live in `fuzz/` (separate from workspace, uses `cargo-fuzz` + `libfuzzer`). Requires nightly.

```bash
cargo install cargo-fuzz                                          # One-time setup
cargo +nightly fuzz run fuzz_tiff_metadata -- -max_total_time=60  # Fuzz TIFF parser
cargo +nightly fuzz run fuzz_geo_transform -- -max_total_time=60  # Fuzz CRS transforms
```

Seed corpus in `fuzz/corpus/fuzz_tiff_metadata/` — add real GeoTIFF files for better coverage.

## Architecture Rules

- **Two core traits: `Engine` (EDR) and `FeatureEngine` (Features).** They are separate traits — not all engines need to support both APIs. Engines return domain types, never JSON. Serialization belongs in the API crates.
- **ds-core has no framework dependencies.** Only chrono, serde, thiserror, toml. Keep it that way. Use `PropertyValue` enum instead of `serde_json::Value` for feature properties.
- **API crates depend only on ds-core**, not on any engine crate. API state is a registry of engines keyed by collection ID (`EdrState` / `FeaturesState`), not a single engine.
- **EDR and Features are separate services** with separate base routes (`/edr/...` and `/features/...`). They share data sources but have independent landing pages, conformance endpoints, and collection listings.
- **CORS is applied at the server level**, not in individual API crates. The `CorsLayer` lives in `server/src/main.rs`.
- **New engines** implement `Engine` and/or `FeatureEngine` traits in their own crate, get wired up in `server/src/main.rs`.
- **Collection routing is dynamic.** Handlers look up engines from a `HashMap<String, Arc<dyn Engine/FeatureEngine>>` by collection ID from the URL path. No collection IDs are hardcoded.
- **The `apis` config field is enforced.** Only collections listing a given API in their `apis` array are wired to that API's router. A GeoJSON collection with `apis = ["features"]` will not appear in EDR.

## Crate Name

The core crate is named `ds-core` in Cargo.toml (imported as `ds_core` in Rust). It was renamed from `core` to avoid shadowing Rust's built-in `core` crate, which breaks proc macros like `#[tokio::main]`.

## Route Structure

```
/                                              Root landing page (links to both services)
/edr/                                          EDR landing page
/edr/conformance                               EDR conformance classes
/edr/collections                               EDR collection listing
/edr/collections/{id}                          EDR collection detail
/edr/collections/{id}/locations                EDR locations query
/edr/collections/{id}/locations/{loc_id}       EDR location data query (CoverageJSON)

/features/                                     Features landing page
/features/api                                  Features OpenAPI definition
/features/conformance                          Features conformance classes
/features/collections                          Features collection listing
/features/collections/{id}                     Features collection detail
/features/collections/{id}/items               Feature items (paginated GeoJSON)
/features/collections/{id}/items/{feature_id}  Single feature (GeoJSON)

/admin/collections/reload                      POST: reload config and swap engines
/health                                        Per-collection health status
/metrics                                       Prometheus metrics (text format)
```

## Adding a New Engine

1. Create `crates/engine-<name>/` with `Cargo.toml` depending on `ds-core`
2. Implement `Engine` and/or `FeatureEngine` traits from `ds_core::engine` / `ds_core::feature_engine`
3. Add the crate to workspace members in root `Cargo.toml`
4. Add the crate as a dependency of `crates/server/Cargo.toml`
5. Add a match arm for the new `engine_type` in `server/src/main.rs`
6. Wire it into the appropriate registries (`edr_engines` / `feature_engines`) based on the collection's `apis` config

## Adding a New EDR Endpoint

1. Add the handler function in `crates/api-edr/src/handlers.rs`
2. Add the route in `crates/api-edr/src/lib.rs`
3. If new query params are needed, add them in `params.rs`
4. If new response formats are needed, add serializers in `response.rs`

## Adding a New Features Endpoint

1. Add the handler function in `crates/api-features/src/handlers.rs`
2. Add the route in `crates/api-features/src/lib.rs`
3. If new query params are needed, add them in `params.rs`
4. If new response formats are needed, add serializers in `response.rs`

## Features API Query Parameters

| Parameter | Format | Description |
|-----------|--------|-------------|
| `bbox` | `west,south,east,north` or `west,south,min-h,east,north,max-h` | Bounding box filter. Supports 4-value (2D) and 6-value (3D, height ignored). Supports antimeridian-crossing (west > east). |
| `limit` | integer | Page size. Default 100, max 1000, min 1. Clamped silently if out of range |
| `offset` | integer | Pagination offset. Default 0 |
| `datetime` | RFC 3339 instant or interval | Temporal filter. Supports instants, intervals (`start/end`), and open bounds (`../end`, `start/..`, `../..`) |

## Features API Response Details

- **Content-Type**: Items/item endpoints return `application/geo+json` (not `application/json`)
- **`timeStamp`**: FeatureCollection responses include `timeStamp` (RFC 3339)
- **`numberMatched`/`numberReturned`**: Always present in FeatureCollection responses
- **Collection metadata**: Includes `extent.spatial.bbox`, `crs`, `storageCrs`
- **OpenAPI**: Served at `/features/api` (linked from landing page via `rel: service-desc`)
- **Conformance**: Declares `core`, `oas30`, `geojson`

## Geometry Types

The `Geometry` enum in `ds_core::feature` supports:

- **`Point { x, y }`** — Single coordinate pair (lon, lat).
- **`Polygon { exterior, holes }`** — Exterior ring as `Vec<[f64; 2]>` plus optional hole rings. Coordinates are `[lon, lat]` pairs.
- **`MultiPolygon { polygons }`** — Vec of `(exterior, holes)` tuples.
- **`Null`** — Null geometry for features without spatial location (RFC 7946 §3.2).

Helper methods on `Geometry`:
- `bbox() -> Option<[f64; 4]>` — Computes bounding box `[west, south, east, north]`. Returns `None` for null geometry.
- `centroid() -> Option<(f64, f64)>` — Computes centroid `(lon, lat)`. Returns `None` for null geometry.

The `Bbox` struct provides:
- `contains(x, y) -> bool` — Point-in-bbox test. Handles antimeridian-crossing bboxes.
- `intersects_bbox(&[f64; 4]) -> bool` — AABB intersection test. Handles antimeridian-crossing bboxes.
- `crosses_antimeridian() -> bool` — True when west > east.

The `FeatureEngine` trait provides:
- `get_features(&self, query) -> Result<FeaturePage>` — Paginated feature query with bbox/datetime filtering.
- `get_feature(&self, id) -> Result<Feature>` — Single feature by ID.
- `feature_count(&self) -> usize` — Total features (default: delegates to get_features with limit=0).
- `spatial_extent(&self) -> Option<[f64; 4]>` — Overall spatial extent (default: None).

## CSV Data Format

Fixed columns: `location, latitude, longitude, time` (in that order). All remaining columns become parameters. Parameter units are mapped in `engine-csv/src/loader.rs`.

## GeoJSON Data Format

Standard GeoJSON FeatureCollection files (RFC 7946). Requirements:

- **Coordinates must be in WGS84 (EPSG:4326).** The engine validates all coordinates fall within lon -180..180, lat -90..90 and rejects files in projected CRS with a helpful error message.
- **Supported geometry types:** Point, Polygon, MultiPolygon.
- **Feature IDs:** Extracted from the top-level `"id"` field on each GeoJSON feature object. Falls back to array index if absent.
- **Properties:** Mapped to `PropertyValue` enum (String, Integer, Float, Bool, Null). Nested objects/arrays are serialized to string.

### Security limits (hardcoded in `engine-geojson/src/loader.rs`)

| Limit | Value | Purpose |
|-------|-------|---------|
| Max file size | 500 MB | Prevents memory exhaustion |
| Max features | 1,000,000 | Prevents excessive load time |
| Max coords per geometry | 100,000 | Prevents geometry bombs |

### Spatial indexing

The GeoJSON engine uses an R-tree (`rstar` crate) built from per-feature bounding boxes. Bbox queries use AABB envelope intersection (not exact polygon intersection), which is both fast and OGC API Features spec-compliant. The R-tree is bulk-loaded at startup in O(n log n).

### Converting projected data to WGS84

If your source data uses a projected CRS (e.g., EPSG:3067 for Finnish data), convert it before loading:

```bash
ogr2ogr -f GeoJSON -t_srs EPSG:4326 output.geojson input.geojson
```

Or with Python:
```python
from pyproj import Transformer
transformer = Transformer.from_crs("EPSG:3067", "EPSG:4326", always_xy=True)
lon, lat = transformer.transform(easting, northing)
```

## GeoTIFF Data Format

Cloud-Optimized GeoTIFF (COG) files with tiled layout. The engine implements `Engine` (EDR) only — it exposes position and area queries returning CoverageJSON.

### Requirements

- **Must be tiled.** Strip-based TIFFs are not supported. Convert with: `gdal_translate -co TILED=YES -co COMPRESS=DEFLATE input.tif output.tif`
- **One parameter per collection.** Each collection reads a single band from the GeoTIFF files. Multi-band files are supported — select the band with the `band` config field (1-based).
- **Files are discovered by filename pattern.** Each file must contain a parseable timestamp in its filename (e.g., `radar_20260325T1200Z.tif`).

### Supported coordinate reference systems

Queries are always in WGS84 (lon/lat). The engine reprojects internally when the source files use a projected CRS. Supported projections:

| CRS | GeoKey Code | Example |
|-----|-------------|---------|
| WGS84 / CRS84 | Geographic (model type 2) | EPSG:4326 |
| Transverse Mercator | ProjCoordTrans = 1 | EPSG:3067 (TM35FIN) |
| Lambert Azimuthal Equal Area | ProjCoordTrans = 10 | EPSG:3035 (ETRS89-LAEA) |
| Lambert Conformal Conic (2SP) | ProjCoordTrans = 8 | Various national grids |

CRS parameters are read from GeoTIFF GeoKeys (tag 34735). Files without GeoKeys are assumed WGS84. **Rotated or skewed rasters are not supported** (the engine assumes axis-aligned pixels).

### Supported compression

| Method | Notes |
|--------|-------|
| None | Uncompressed tiles |
| Deflate | zlib/deflate (TIFF compression tag 8 or 32946) |
| LZW | TIFF-specific LZW with early code size switch |

### Supported data types

| Type | Bits | Notes |
|------|------|-------|
| UInt8 | 8 | Common for radar/classification data |
| UInt16 | 16 | Common for satellite imagery |
| Int16 | 16 | Signed, e.g., temperature offsets |
| Float32 | 32 | Standard for continuous fields |
| Float64 | 64 | High-precision fields |

Values are converted to `f64` internally. Physical values are computed as: `physical = raw * scale + offset`.

### Data source modes

| Mode | Config | Description |
|------|--------|-------------|
| Local directory | `data_path = "path/to/dir"` | Scans a local directory |
| Fixed remote prefix | `data_path = "s3://bucket/prefix/"` | Scans a single S3/HTTP prefix |
| Dynamic remote prefix | `endpoint` + `bucket` + `prefix_pattern` | Expands date-based prefixes on each poll cycle |

### Polling and file discovery

The engine polls for new files at a configurable interval (`poll_interval_secs`, default 30s). Behavior:

- **Local files:** New files are held in a "pending" state for one poll cycle to confirm they are fully written (size stability check). Files matching `exclude_patterns` (default: `*.tmp`, `*.part`) are skipped.
- **Remote files:** Uses COG byte-range reads to fetch only the 64 KB IFD header for metadata. Falls back to full download if header-only parse fails (e.g., non-COG layout).
- **Metadata caching:** Files with unchanged size reuse their cached metadata across poll cycles — no re-download.
- **Failure handling:** If a poll cycle fails, the old catalog is preserved. If a poll returns 0 files but the old catalog had files, it is treated as a transient failure and the old catalog is kept.
- **Duplicate timestamps:** When two files have the same timestamp, the lexicographically last filename is kept.

### Tile caching

The engine caches **compressed** tile bytes (not decoded pixels) in a lock-free LRU cache. This gives ~58× better memory efficiency than caching decoded tiles. Default cache size is 64 MB (`tile_cache_mb`). Set to 0 to disable.

### Security limits (hardcoded)

| Limit | Value | Constant | Purpose |
|-------|-------|----------|---------|
| Max raster dimension | 100,000 px | `MAX_RASTER_DIMENSION` | Prevents loading enormous files |
| Max decoded tile size | 64 MB | `MAX_DECODED_TILE_BYTES` | Prevents decompression bombs |
| Max area query pixels | 1,000,000 | `MAX_AREA_PIXELS` | Prevents huge area queries |
| Max remote file size | 50 MB | `MAX_REMOTE_FILE_SIZE` | Prevents downloading oversized files |
| Max filename length | 255 chars | `MAX_FILENAME_LENGTH` | Prevents abuse via long filenames |

### GeoTIFF config fields

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `filename_template` | * | — | Strftime-based template, e.g., `"radar_%Y%m%dT%H%MZ.tif"`. Auto-derives regex and timestamp format. |
| `filename_pattern` | * | — | Explicit regex with `(?P<timestamp>...)` capture group. Requires `timestamp_format`. |
| `timestamp_format` | * | — | chrono strftime format for the captured timestamp, e.g., `"%Y%m%dT%H%MZ"` |
| `parameter` | yes | — | Parameter name, e.g., `"reflectivity"` |
| `unit` | yes | — | Unit of measurement, e.g., `"dBZ"` |
| `poll_interval_secs` | no | `30` | Directory poll interval in seconds. Must be > 0. |
| `tile_cache_mb` | no | `64` | Tile cache size in MB. Set to 0 to disable. |
| `band` | no | `1` | Band number to read (1-based). |
| `max_files` | no | none | Keep only the N most recent files by timestamp. |
| `nodata` | no | from file | Override nodata value. Use when files lack a GDAL_NODATA tag. |
| `scale` | no | from file | Override scale factor. `physical = raw * scale + offset` |
| `offset` | no | from file | Override offset. `physical = raw * scale + offset` |
| `exclude_patterns` | no | `["*.tmp", "*.part"]` | Glob patterns for files to skip. |
| `endpoint` | no† | — | S3-compatible endpoint URL, e.g., `"https://s3.example.com"` |
| `bucket` | no† | — | S3 bucket name. Required when `endpoint` is set. |
| `prefix_pattern` | no | `""` | Object prefix, optionally with strftime templates, e.g., `"%Y/%m/%d/data/"` |
| `time_window` | no | none | ISO 8601 duration for file selection, e.g., `"-PT2H"` (past 2 hours) |
| `scan_days` | no | auto | Number of days to scan for date-based prefixes. Auto-derived from `time_window`. |

\* Either `filename_template` **or** both `filename_pattern` + `timestamp_format` must be set.
† `endpoint` and `bucket` must both be set or both absent.

### Troubleshooting

| Error | Cause | Fix |
|-------|-------|-----|
| "Not a tiled TIFF (TileWidth missing)" | File uses strip layout, not tiles | `gdal_translate -co TILED=YES -co COMPRESS=DEFLATE input.tif output.tif` |
| "Raster dimensions exceed maximum" | File is larger than 100,000 × 100,000 px | Downsample or use overviews |
| "Decompressed tile exceeds maximum size" | Tile dimensions × bands × bytes/sample > 64 MB | Use smaller tiles (256×256 or 512×512) |
| "No matching GeoTIFF files found" | No files match the filename pattern | Check `filename_template` against actual filenames in the directory |
| "Either data_path or endpoint+bucket must be configured" | Missing data source | Set `data_path` for local/HTTP, or `endpoint` + `bucket` for S3 |
| "'endpoint' is set but 'bucket' is missing" | Incomplete S3 config | Set both `endpoint` and `bucket` |
| "poll_interval_secs must be > 0" | Zero poll interval | Set to at least 1 (typically 30-60) |
| Empty results / all-None values | Wrong `band` number, or missing `nodata` override | Check band count with `gdalinfo`; set `nodata` if file lacks the tag |
| Slow poll cycles | Many remote files, or non-COG layout causing full downloads | Set `max_files` and/or `time_window` to limit scan scope; convert to COG |

## Config Format

```toml
[server]
host = "0.0.0.0"
port = 8000
# base_url = "https://api.example.com"  # optional, for absolute links behind a proxy

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
apis = ["edr"]

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
```

### Config fields

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `id` | yes | — | Unique collection identifier, used in URL paths |
| `title` | yes | — | Human-readable collection title |
| `description` | yes | — | Collection description |
| `data_path` | yes | — | Path to data file (CSV or GeoJSON) |
| `apis` | no | `["edr"]` | Which APIs expose this collection: `"edr"`, `"features"`, or both |
| `engine_type` | no | `"csv"` | Data engine: `"csv"`, `"geojson"`, or `"geotiff"` |

Server-level fields:

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `host` | yes | — | Bind address |
| `port` | yes | — | Bind port |
| `base_url` | no | `http://{host}:{port}` | External base URL for absolute links (set when behind a reverse proxy) |

## API State Architecture

Both API crates use registry-based state instead of a single engine:

```rust
// api-edr
pub struct EdrState {
    pub engines: HashMap<String, Arc<dyn Engine>>,
    pub collections: HashMap<String, CollectionConfig>,
}

// api-features
pub struct FeaturesState {
    pub engines: HashMap<String, Arc<dyn FeatureEngine>>,
    pub collections: HashMap<String, CollectionConfig>,
}
```

Handlers look up the engine by collection ID from the URL path parameter. Unknown collection IDs return 404. Collection metadata (title, description, links) is built from `CollectionConfig`, not hardcoded.

## CoverageJSON Schema Compliance

All CoverageJSON output **must** validate against the OGC CoverageJSON 1.0 schema at `schemas/coveragejson.json` (source: https://schemas.opengis.net/covjson/1.0/coveragejson.json).

### Schema validation tests

Integration tests in `crates/api-edr/tests/covjson_validation.rs` validate serializer output against the schema using the `jsonschema` crate. Run with `cargo test -p api-edr`.

**When modifying `response.rs` or adding new CoverageJSON output, always run these tests.** If adding a new domain type or response variant, add a corresponding test case.

### Key schema rules to follow

**Coverage object** (top level):
- Required fields: `type` ("Coverage"), `domain`, `parameters`, `ranges`
- `parameters` is a map of parameter name → Parameter object
- `ranges` is a map of parameter name → NdArray object

**Domain object** (`domain`):
- Required fields: `type` ("Domain"), `axes`, `referencing`
- `domainType` triggers axis constraints (e.g. PointSeries requires x, y, t)
- `referencing` array connects coordinate names to reference systems

**PointSeries domain**:
- `x` axis: `numericSingleValueAxis` — `{"values": [<single number>]}`
- `y` axis: `numericSingleValueAxis` — `{"values": [<single number>]}`
- `t` axis: `stringValuesAxis` — `{"values": ["2024-01-01T00:00:00+00:00", ...]}`
- No additional axes allowed (`additionalProperties: false`)

**Grid domain**:
- `x` axis: `numericValuesAxis` — `{"values": [10.0, 10.5, 11.0, ...]}`
- `y` axis: `numericValuesAxis` — `{"values": [60.0, 60.5, 61.0, ...]}`
- `t` axis: optional `stringValuesAxis` (omitted for single-timestep grids)
- NdArray shape: `[t, y, x]` (with time) or `[y, x]` (without time)

**Parameter object**:
- Required: `type` ("Parameter"), `observedProperty`
- `observedProperty` requires `label` as an i18n object: `{"en": "temperature"}`
- `unit` requires either `label` or `symbol` (we provide both)
- `description` must be an i18n object if present: `{"en": "..."}`

**NdArray object** (ranges):
- Required: `type` ("NdArray"), `dataType` ("float"/"integer"/"string"), `values`
- When values has >1 item: `shape` and `axisNames` are also required
- For `dataType: "float"`: values items must be `number | null`
- `values.length` must equal the product of `shape` dimensions

**Reference systems**:
- Spatial: `{"type": "GeographicCRS", "id": "http://www.opengis.net/def/crs/OGC/1.3/CRS84"}`
- Temporal: `{"type": "TemporalRS", "calendar": "Gregorian"}` — calendar must be "Gregorian" or an HTTP URL

**i18n objects**: Keys must be BCP 47 language tags (e.g. `"en"`). No additional properties allowed.

### Adding new domain types

When implementing a new domain type (Point, Trajectory, VerticalProfile, etc.):

1. Add a variant to `DomainDescription` enum in `ds-core/src/model.rs`
2. Add a match arm in `build_domain()` in `api-edr/src/response.rs`
3. Check the schema's `domainBase.dependencies.domainType` section for that type's axis requirements
4. Add a validation test in `covjson_validation.rs` using a helper like `make_grid_query_result`
5. The schema enforces `additionalProperties: false` on axes for typed domains — only the specified axes are allowed

Currently implemented: `PointSeries`, `Grid`.

### Common pitfalls

- Axis values must have `uniqueItems: true` — no duplicate timestamps or coordinates
- NdArray `axisNames` must have `uniqueItems: true`
- Forgetting `referencing` on an inline domain object (required when domain is an object, not a URL string)
- Using non-BCP47 keys in i18n objects (use `"en"`, not `"english"`)

## Admin, Health & Metrics

### Dynamic collection reload

`POST /admin/collections/reload` re-reads `config.toml` (or `CONFIG_PATH`), creates new engines, and atomically swaps them into the running server. Old GeoTIFF poll loops are shut down and new ones spawned. If the reload produces zero working collections, the old state is preserved.

Response: `{"status": "ok", "loaded": N, "configured": M, "collections": [...]}`.

### Health endpoint

`GET /health` returns per-collection health status. Each collection reports one of:

| Status | Meaning |
|--------|---------|
| `ready` | Engine loaded and has data |
| `degraded` | Engine loaded but no data yet (e.g., GeoTIFF waiting for first poll) |
| `failed` | Engine failed to load (error message included) |

Overall status: `healthy` (all ready), `degraded` (some degraded, none failed), `unhealthy` (any failed). Returns HTTP 503 when unhealthy.

### Prometheus metrics

`GET /metrics` returns Prometheus text format. Exposed metrics:

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `http_requests_total` | counter | method, path, status | Total HTTP requests |
| `http_request_duration_seconds` | histogram | method, path | Request latency |
| `collections_total` | gauge | — | Total configured collections |
| `collections_healthy` | gauge | — | Collections in ready state |
| `collections_degraded` | gauge | — | Collections in degraded state |
| `collections_failed` | gauge | — | Collections in failed state |

Path labels use axum's `MatchedPath` (route patterns, not raw URLs) to avoid unbounded cardinality.

### State architecture

API state (`EdrState`, `FeaturesState`) is wrapped in `ArcSwap` for lock-free reads and atomic swaps on reload. The `ServerState` in `server/src/admin.rs` holds the `ArcSwap` pointers, health registry, and GeoTIFF engine list. Engine loading logic is in `admin::load_collections()`, shared by startup and reload.

## Known Limitations

- Parameter units are hardcoded in the CSV loader's match statement
- CSV/GeoJSON data loaded into memory at startup; GeoTIFF reads tiles on demand
- CSV engine supports only the `locations` query type (no position, area, radius, trajectory, corridor)
- GeoTIFF engine supports `position` and `area` queries (no locations, radius, trajectory, corridor)
- GeoJSON engine implements `FeatureEngine` only (not `Engine`/EDR) — polygon boundary data has no time-series parameters
- GeoTIFF engine implements `Engine` only (not `FeatureEngine`/Features)
- GeoTIFF CRS: WGS84, Transverse Mercator, LAEA, and LCC supported; other projections fall back to WGS84
- GeoTIFF area queries extract the bounding box from POLYGON WKT — they do not clip to the actual polygon shape
- Strip-based (non-tiled) GeoTIFFs are not supported — convert to COG first
- No per-file timeout on remote reads — a hung S3 endpoint blocks the poll cycle
- GeoTIFF multi-band: one band per collection; multiple bands as separate parameters not yet supported

## Code Style

- Use `thiserror` for error types, not manual `impl Display`
- Prefer returning `Result<T, DataServerError>` from engine methods
- Keep handlers thin — delegate logic to the engine, map errors to HTTP status codes
- Use `serde_json::json!` macro for building JSON responses
- Do not leak internal error details to clients — use generic messages for 500 errors
