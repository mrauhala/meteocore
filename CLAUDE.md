# MeteoCore — Claude Instructions

## What This Is

Rust workspace implementing OGC API - EDR, OGC API - Features, OGC API - Maps, OGC API - Tiles, and OGC WMS 1.3.0 servers. Thirteen crates: `ds-core` (traits + types + shared utilities), `ds-storage` (S3/HTTP/local object store), `ds-render` (raster colorization + PNG encoding), `engine-csv` (CSV data engine), `engine-geojson` (GeoJSON data engine), `engine-geotiff` (GeoTIFF/COG data engine), `api-edr` (EDR HTTP layer), `api-features` (Features HTTP layer), `api-maps` (OGC API Maps HTTP layer), `api-tiles` (OGC API Tiles HTTP layer), `api-wms` (WMS 1.3.0 HTTP layer), `server` (binary).

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

- **Three core traits: `Engine` (EDR), `FeatureEngine` (Features), and `MapEngine` (Maps/WMS/Tiles).** They are separate traits — not all engines need to support all APIs. Engines return domain types, never JSON/XML. Serialization belongs in the API crates.
- **ds-core has no framework dependencies.** Only chrono, serde, thiserror, toml. Keep it that way. Use `PropertyValue` enum instead of `serde_json::Value` for feature properties.
- **ds-render has no framework dependencies.** Only ds-core and `png`. Pure rendering library for colorization and image encoding.
- **API crates depend only on ds-core** (and ds-render for api-wms/api-maps), not on any engine crate. API state is a registry of engines keyed by collection ID (`EdrState` / `FeaturesState` / `MapsState` / `WmsState`), not a single engine.
- **EDR, Features, Maps, Tiles, and WMS are separate services** with separate base routes (`/edr/...`, `/features/...`, `/maps/...`, `/tiles/...`, `/wms/...`). They share data sources but have independent endpoints.
- **WMS uses XML, not JSON.** All XML output in api-wms uses `quick-xml::Writer` for proper escaping. Never build XML with `format!()` or string concatenation (XML injection risk).
- **CORS is applied at the server level**, not in individual API crates. The `CorsLayer` lives in `server/src/main.rs`.
- **New engines** implement `Engine`, `FeatureEngine`, and/or `MapEngine` traits in their own crate, get wired up in `server/src/main.rs`.
- **Collection routing is dynamic.** Handlers look up engines from a `HashMap<String, Arc<dyn Engine/FeatureEngine/MapEngine>>` by collection ID from the URL path. No collection IDs are hardcoded.
- **The `apis` config field is enforced.** Only collections listing a given API in their `apis` array are wired to that API's router. A GeoJSON collection with `apis = ["features"]` will not appear in EDR, WMS, or Tiles.
- **Tiles reuses MapEngine.** Tile z/x/y coordinates are converted to a bbox via TileMatrixSet math, then passed to `MapEngine::get_raster_tile()`. No separate tile engine trait is needed. TilesState has `map_engines` (raster tiles) with space for future `vector_engines` (vector tiles from FeatureEngine).

## Crate Name

The core crate is named `ds-core` in Cargo.toml (imported as `ds_core` in Rust). It was renamed from `core` to avoid shadowing Rust's built-in `core` crate, which breaks proc macros like `#[tokio::main]`.

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

## Adding a New Engine

1. Create `crates/engine-<name>/` with `Cargo.toml` depending on `ds-core`
2. Implement `Engine`, `FeatureEngine`, and/or `MapEngine` traits from `ds_core::engine` / `ds_core::feature_engine` / `ds_core::map_engine`
3. Add the crate to workspace members in root `Cargo.toml`
4. Add the crate as a dependency of `crates/server/Cargo.toml`
5. Add a match arm for the new `engine_type` in `server/src/main.rs`
6. Wire it into the appropriate registries (`edr_engines` / `feature_engines` / `map_engines`) based on the collection's `apis` config

## Adding a New EDR Endpoint

1. Add the handler function in `crates/api-edr/src/handlers.rs`
2. Add the route in `crates/api-edr/src/lib.rs`
3. If new query params are needed, add them in `params.rs`
4. If new response formats are needed, add serializers in `response.rs`
5. Update `api_definition()` in `handlers.rs` to include the new path in the OpenAPI spec

## Adding a New Features Endpoint

1. Add the handler function in `crates/api-features/src/handlers.rs`
2. Add the route in `crates/api-features/src/lib.rs`
3. If new query params are needed, add them in `params.rs`
4. If new response formats are needed, add serializers in `response.rs`
5. Update `api_definition()` in `handlers.rs` to include the new path in the OpenAPI spec

## Adding a New Maps Endpoint

1. Add the handler function in `crates/api-maps/src/handlers.rs`
2. Add the route in `crates/api-maps/src/lib.rs`
3. If new query params are needed, add them in `params.rs`
4. Update `api_definition()` in `handlers.rs` to include the new path in the OpenAPI spec

## Adding a New Tiles Endpoint

1. Add the handler function in `crates/api-tiles/src/handlers.rs`
2. Add the route in `crates/api-tiles/src/lib.rs`
3. If new query params are needed, add them in `params.rs`
4. If new TileMatrixSet support is needed, add it in `tilematrixset.rs`
5. Update `api_definition()` in `handlers.rs` to include the new path in the OpenAPI spec

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
- **OpenAPI**: Served at `/features/api` (linked from landing page via `rel: service-desc`). Swagger UI at `/features/api/docs` (`rel: service-doc`).
- **Conformance**: Declares `core`, `oas30`, `geojson`

## WMS 1.3.0

The server implements OGC WMS 1.3.0 for serving raster data as map images. Only GeoTIFF collections can be exposed via WMS (they implement `MapEngine`).

### Endpoints

Single `/wms/` route dispatches on the `REQUEST` query parameter:

- **GetCapabilities**: `?SERVICE=WMS&REQUEST=GetCapabilities` — returns XML capabilities with layers, CRS, extents, time dimension, styles
- **GetMap**: `?SERVICE=WMS&REQUEST=GetMap&LAYERS=...&CRS=...&BBOX=...&WIDTH=...&HEIGHT=...&FORMAT=image/png` — returns map image (PNG or JPEG)
- **GetLegendGraphic**: `?SERVICE=WMS&REQUEST=GetLegendGraphic&LAYER=...&STYLE=...` — returns legend image showing colormap scale

### GetMap Parameters

| Parameter | Required | Description |
|-----------|----------|-------------|
| `SERVICE` | yes | Must be `WMS` |
| `VERSION` | yes | Must be `1.3.0` |
| `REQUEST` | yes | `GetMap` |
| `LAYERS` | yes | Single layer name (= collection ID). Multi-layer not yet supported. |
| `CRS` | yes | `CRS:84`, `EPSG:4326`, `EPSG:3857`, `EPSG:3067`, or `EPSG:3035` |
| `BBOX` | yes | Bounding box — axis order depends on CRS (see below) |
| `WIDTH` | yes | Image width in pixels (max 4096) |
| `HEIGHT` | yes | Image height in pixels (max 4096) |
| `FORMAT` | yes | `image/png` or `image/jpeg` |
| `TRANSPARENT` | no | Default `TRUE` |
| `STYLES` | no | Style name (default: `default`). Empty string = default. |
| `TIME` | no | ISO 8601 timestamp; defaults to latest available |

### WMS 1.3.0 Axis Order

**Critical:** WMS 1.3.0 BBOX axis order depends on the CRS:

- **CRS:84**: `BBOX=west,south,east,north` (lon/lat — same as internal)
- **EPSG:4326**: `BBOX=south,west,north,east` (lat/lon — swapped!)
- **EPSG:3857, EPSG:3067, EPSG:3035**: `BBOX=minx,miny,maxx,maxy` (easting/northing)

The handler normalizes all bbox values to `[west, south, east, north]` internally. Test with both CRS:84 and EPSG:4326 to catch axis order bugs.

### WMS Error Handling

Errors are returned as XML `ServiceExceptionReport` documents with WMS-specific error codes: `LayerNotDefined`, `CRSNotDefined`, `InvalidDimensionValue`, `MissingParameterValue`, `InvalidFormat`, etc.

### Security Limits

| Limit | Value | Location |
|-------|-------|----------|
| MAX_MAP_PIXELS | 8,000,000 (8M) | `api-wms/src/params.rs` |
| MAX_MAP_DIMENSION | 4,096 px | `api-wms/src/params.rs` |
| Render semaphore | CPU core count (min 4) | `server/src/admin.rs` |
| CRS whitelist | CRS:84, EPSG:4326/3857/3067/3035 | `api-wms/src/params.rs` |
| No external SLD | SLD parameter rejected | Not implemented |
| Max LAYERS | 1 | `api-wms/src/params.rs` |
| FORMAT whitelist | `image/png`, `image/jpeg` | `api-wms/src/params.rs` |

### Rendering Pipeline

1. Parse WMS parameters, validate, normalize bbox axis order
2. Look up `MapEngine` by layer name (= collection ID)
3. Check rendered image cache (LRU, keyed by quantized bbox + layer + time)
4. If miss: await render semaphore (queues if all slots busy), call `MapEngine::get_raster_tile()` on a blocking thread
5. Engine projects bbox to source CRS (sampling 20 points per edge to capture projection curvature), selects best overview level, reads source tiles, resamples to output dimensions (nearest-neighbor)
6. If tile is all nodata: return transparent PNG without caching (allows retry on next request)
7. Apply colormap (LUT for integer data, linear interpolation for continuous data) → RGBA buffer
8. Encode RGBA → PNG using pure Rust `png` crate (compression level Fast)
9. Cache rendered PNG, return with `Content-Type: image/png`

### Colormaps

Colormaps are configured per collection in `[collections.wms]`:

| Built-in Name | Description | Default Range |
|---------------|-------------|---------------|
| `radar_dbz` | Standard radar reflectivity (blue→green→yellow→red) | 0–70 dBZ |
| `grayscale` | Linear black→white | 0–1 |
| `viridis` | Perceptually uniform (good default for continuous data) | 0–1 |
| `temperature` | Temperature palette (purple→blue→cyan→green→yellow→red) | -40–50 °C |
| `precipitation` | Precipitation accumulation (transparent→blue→purple→white) | 0–50 mm |
| `wind_speed` | Wind speed (green→yellow→orange→red→purple) | 0–50 m/s |

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

### Rendered Image Cache (Tier 2)

Separate from the GeoTIFF source tile cache (Tier 1). Caches final PNG/JPEG bytes.

- Default size: 512 MB (configurable via `rendered_cache_mb`)
- Cache key: quantized bbox (6 decimal places) + layer + style + format + width + height + CRS + time
- Lock-free concurrent LRU (uses `quick_cache`)
- No TTL — radar data is immutable once produced. Cache invalidated on collection reload.
- Cache hit skips entire pipeline: no tile reads, no colorization, no image encoding
- Error tiles (render failures) are NOT cached — re-attempted on next request
- Empty tiles (all nodata) are NOT cached — allows recovery from transient failures

### HTTP Cache Headers

WMS responses include cache headers for client-side and CDN caching:

| Scenario | Cache-Control | ETag |
|----------|---------------|------|
| GetMap with explicit `TIME=` | `public, max-age=86400, immutable` | Yes |
| GetMap without `TIME` (latest) | `public, max-age=60, must-revalidate` | Yes |
| GetLegendGraphic | `public, max-age=86400, immutable` | No |
| GetCapabilities | No cache headers | No |

Requests with explicit timestamps are immutable (data won't change), so clients and CDNs can cache for 24 hours. Requests for "latest" data get 60-second cache to pick up new measurements.

### WMS Config Fields

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `colormap` | no | `"viridis"` | Built-in colormap name for the default style |
| `color_stops` | no | — | Array of `{value, color}` entries. Overrides built-in colormap. |
| `min` | no | from colormap | Minimum value for the colormap range |
| `max` | no | from colormap | Maximum value for the colormap range |
| `styles` | no | — | Array of named styles (see below) |
| `rendered_cache_mb` | no | `512` | Rendered image cache size in MB. Set to 0 to disable. |

### Named Styles

Additional styles beyond the default are defined in `[[collections.wms.styles]]`:

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `name` | yes | — | Style name (used in `STYLES=` parameter) |
| `title` | no | same as name | Human-readable title for GetCapabilities |
| `colormap` | no | `"viridis"` | Built-in colormap name |
| `color_stops` | no | — | Custom color stops (overrides colormap) |
| `min` | no | from colormap | Minimum value for this style's range |
| `max` | no | from colormap | Maximum value for this style's range |

### Adding WMS to a Collection

Add `"wms"` to the `apis` array and configure `[collections.wms]`:

```toml
[[collections]]
id = "radar"
engine_type = "geotiff"
apis = ["edr", "wms"]

[collections.geotiff]
filename_template = "radar_%Y%m%dT%H%MZ.tif"
parameter = "reflectivity"
unit = "dBZ"

[collections.wms]
colormap = "radar_dbz"

# Additional named styles (optional)
[[collections.wms.styles]]
name = "grayscale"
title = "Grayscale"
colormap = "grayscale"
min = 0.0
max = 70.0

[[collections.wms.styles]]
name = "viridis"
title = "Viridis"
colormap = "viridis"
min = 0.0
max = 70.0
```

Only `engine_type = "geotiff"` collections support WMS. CSV and GeoJSON engines do not implement `MapEngine`.

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

Cloud-Optimized GeoTIFF (COG) files with tiled layout. The engine implements `Engine` (EDR) for position/area queries returning CoverageJSON, and `MapEngine` (WMS) for raster tile rendering returning PNG images.

### Requirements

- **Must be tiled.** Strip-based TIFFs are not supported. Convert with: `gdal_translate -co TILED=YES -co COMPRESS=DEFLATE input.tif output.tif`
- **One parameter per collection.** Each collection reads a single band from the GeoTIFF files. Multi-band files are supported — select the band with the `band` config field (1-based).
- **Files are discovered by filename pattern.** Each file must contain a parseable timestamp in its filename (e.g., `radar_20260325T1200Z.tif`).

### Preparing COG files

Optimal settings for serving via MeteoCore (applies to both local and remote files):

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

**Manual recipe (older GDAL):**

```bash
# 1. Create tiled GeoTIFF
gdal_translate -of GTiff \
  -co TILED=YES -co BLOCKXSIZE=256 -co BLOCKYSIZE=256 \
  -co COMPRESS=DEFLATE -co PREDICTOR=2 \
  input.tif temp.tif

# 2. Add overviews (powers of 2 to align with tile zoom levels)
gdaladdo -r average \
  --config COMPRESS_OVERVIEW DEFLATE \
  --config PREDICTOR_OVERVIEW 2 \
  temp.tif 2 4 8 16 32 64

# 3. Re-create as COG (overviews before data for efficient range reads)
gdal_translate -of COG \
  -co COMPRESS=DEFLATE -co PREDICTOR=YES -co BLOCKSIZE=256 \
  temp.tif output_cog.tif && rm temp.tif
```

**Recommended settings:**

| Setting | Value | Rationale |
|---------|-------|-----------|
| Tile size | 256x256 | Matches Tiles API tile size (one source tile = one output tile at native resolution). Smaller tiles = more granular caching, less wasted data on partial reads. |
| Compression | Deflate | Best ratio with predictor. The tile cache stores compressed bytes, so better compression = more tiles in cache. |
| Predictor | 2 (integer) or 3 (float) | Horizontal differencing (2) for UInt8/UInt16/Int16. Floating point predictor (3) for Float32/Float64. Use `PREDICTOR=YES` with COG driver for automatic selection. |
| Overviews | Powers of 2, auto | Align with TileMatrixSet zoom levels. Without overviews, every low-zoom tile reads full-resolution data. |
| Overview resampling | NEAREST for discrete data, AVERAGE for continuous | Radar dBZ/classification: NEAREST preserves categories. Temperature/wind: AVERAGE avoids aliasing. |

**Settings to avoid:**

| Setting | Problem |
|---------|---------|
| Strip layout (no tiling) | Server rejects with "Not a tiled TIFF" error |
| Tile size > 512x512 | Wastes bandwidth/memory on partial reads |
| JPEG compression | Not supported by the server's TIFF reader |
| No overviews | Every low-zoom request reads full resolution — very slow for large files |
| LZW without predictor | Worse compression than Deflate with predictor |

### Supported coordinate reference systems

Queries are always in WGS84 (lon/lat). The engine reprojects internally when the source files use a projected CRS. Supported projections:

| CRS | GeoKey Code | Example |
|-----|-------------|---------|
| WGS84 / CRS84 | Geographic (model type 2) | EPSG:4326 |
| Transverse Mercator | ProjCoordTrans = 1 | EPSG:3067 (TM35FIN) |
| Lambert Azimuthal Equal Area | ProjCoordTrans = 10 | EPSG:3035 (ETRS89-LAEA) |
| Lambert Conformal Conic (2SP) | ProjCoordTrans = 8 | Various national grids |

CRS parameters are read from GeoTIFF GeoKeys (tag 34735). Files without GeoKeys are assumed WGS84. **Rotated or skewed rasters are not supported** (the engine assumes axis-aligned pixels).

When reprojecting, `bbox_to_pixels()` samples 20 points along each bbox edge (not just 4 corners) to capture projection curvature — critical for TM at high latitudes where bbox edges project as curves. `world_to_pixel()` uses `floor()` for consistent rounding with `bbox_to_pixels()`. Overview selection breaks early when an overview's coarse pixel grid can't detect bbox intersection, falling back to the previous (larger) candidate.

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
| STAC catalog | `stac_url` + `stac_asset_allowlist` | Discovers files via STAC API items endpoint |

### Polling and file discovery

The engine polls for new files at a configurable interval (`poll_interval_secs`, default 30s). Behavior:

- **Local files:** New files are held in a "pending" state for one poll cycle to confirm they are fully written (size stability check). Files matching `exclude_patterns` (default: `*.tmp`, `*.part`) are skipped.
- **Remote files:** Uses COG byte-range reads to fetch only the 64 KB IFD header for metadata. Falls back to full download if header-only parse fails (e.g., non-COG layout).
- **Metadata caching:** Files with unchanged size reuse their cached metadata across poll cycles — no re-download.
- **Failure handling:** If a poll cycle fails, the old catalog is preserved. If a poll returns 0 files but the old catalog had files, it is treated as a transient failure and the old catalog is kept.
- **Duplicate timestamps:** When two files have the same timestamp, the lexicographically last filename is kept.
- **STAC mode:** Queries the STAC API items endpoint for file discovery. Timestamps come from `properties.datetime` (with `start_datetime` fallback). Asset URLs are validated against `stac_asset_allowlist` (SSRF protection). Pagination follows `rel=next` links (same-origin only, max 20 pages, 120s total timeout). HTTP redirects are disabled.

### STAC catalog integration

The engine can discover GeoTIFF files via a STAC API instead of directory listing. This is useful when the data provider exposes a STAC catalog but denies S3 LIST operations (e.g., MET Norway radar).

**How it works:**
1. On startup, fetches collection extent from the STAC collection endpoint (bbox + temporal interval) — no items
2. When a query arrives for a datetime range, fetches STAC items for that range on-demand
3. Creates lightweight stubs from STAC metadata (datetime, bbox, asset URL) — no GeoTIFF downloads
4. GeoTIFF headers are loaded lazily via COG byte-range reads when pixel data is first needed
5. Loaded metadata is cached — subsequent queries for the same timestamp skip all HTTP
6. Poll loop only fetches newly published items (incremental, since latest known timestamp)

**Fully on-demand architecture:** The STAC catalog mirrors the full temporal extent of the origin server without downloading anything. Items are discovered and GeoTIFF metadata is loaded only when queries request specific time ranges. This enables serving years of archived data with near-zero startup cost. A cap of 50 GeoTIFF loads per query prevents timeout on very large time ranges.

**Security:**
- **Asset URL allowlist** (`stac_asset_allowlist`): mandatory, no default. Every asset URL must match at least one prefix (validated by scheme + host + port + path prefix, not string matching).
- **HTTP redirects disabled**: prevents redirect-based SSRF attacks.
- **Pagination origin check**: `next` links must be same-origin as the items URL.
- **Scheme validation**: only `http://` and `https://` asset URLs are accepted.

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

### Tile caching

The engine caches **compressed** tile bytes (not decoded pixels) in a lock-free LRU cache. This gives ~58× better memory efficiency than caching decoded tiles. Default cache size is 256 MB (`tile_cache_mb`). Set to 0 to disable. Cache keys include the IFD index to prevent collisions between full-resolution and overview tiles.

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
| `tile_cache_mb` | no | `256` | Tile cache size in MB for compressed COG tiles. Set to 0 to disable. |
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
| `stac_url` | no‡ | — | STAC API items endpoint URL. Replaces `data_path`/`endpoint+bucket` for file discovery. |
| `stac_asset_key` | no | `"data"` | Which STAC asset key to extract the GeoTIFF URL from. |
| `stac_asset_allowlist` | no‡ | — | Required SSRF protection: list of allowed URL prefixes for asset downloads. |

\* Either `filename_template` **or** both `filename_pattern` + `timestamp_format` must be set (not required in STAC mode).
† `endpoint` and `bucket` must both be set or both absent.
‡ `stac_url` is mutually exclusive with `data_path` and `endpoint+bucket`. `stac_asset_allowlist` is required when `stac_url` is set.

### Troubleshooting

| Error | Cause | Fix |
|-------|-------|-----|
| "Not a tiled TIFF (TileWidth missing)" | File uses strip layout, not tiles | `gdal_translate -co TILED=YES -co COMPRESS=DEFLATE input.tif output.tif` |
| "Raster dimensions exceed maximum" | File is larger than 100,000 × 100,000 px | Downsample or use overviews |
| "Decompressed tile exceeds maximum size" | Tile dimensions × bands × bytes/sample > 64 MB | Use smaller tiles (256×256 or 512×512) |
| "No matching GeoTIFF files found" | No files match the filename pattern | Check `filename_template` against actual filenames in the directory |
| "Either data_path, endpoint+bucket, or stac_url must be configured" | Missing data source | Set `data_path` for local/HTTP, `endpoint` + `bucket` for S3, or `stac_url` for STAC |
| "'stac_url' and 'data_path' are mutually exclusive" | Both STAC and local config set | Use only one data source mode |
| "'stac_asset_allowlist' is required when 'stac_url' is set" | Missing SSRF protection | Add `stac_asset_allowlist` with allowed URL prefixes |
| "'endpoint' is set but 'bucket' is missing" | Incomplete S3 config | Set both `endpoint` and `bucket` |
| "poll_interval_secs must be > 0" | Zero poll interval | Set to at least 1 (typically 30-60) |
| Empty results / all-None values | Wrong `band` number, or missing `nodata` override | Check band count with `gdalinfo`; set `nodata` if file lacks the tag |
| Slow poll cycles | Many remote files, or non-COG layout causing full downloads | Set `max_files` and/or `time_window` to limit scan scope; convert to COG |

## OGC API Maps

The server implements OGC API Maps for serving raster data as map images via a RESTful JSON API. Only GeoTIFF collections can be exposed via Maps (they implement `MapEngine`). Maps and WMS share the same `MapEngine` trait but have separate HTTP layers and state.

### Maps Query Parameters

| Parameter | Required | Default | Description |
|-----------|----------|---------|-------------|
| `bbox` | yes | — | Bounding box: `west,south,east,north` (always lon/lat order) |
| `width` | no | `256` | Image width in pixels (max 4096) |
| `height` | no | `256` | Image height in pixels (max 4096) |
| `crs` | no | `CRS:84` | Output CRS: `CRS:84`, `EPSG:4326`, `EPSG:3857`, `EPSG:3067`, `EPSG:3035` |
| `datetime` | no | latest | ISO 8601 timestamp |
| `f` | no | `image/png` | Output format: `image/png`, `image/jpeg`, `image/webp` |
| `transparent` | no | — | Transparency support |
| `bbox-crs` | no | — | CRS for bbox coordinates (only `CRS:84` supported) |

### Key Differences from WMS

- **Maps uses REST paths** (`/maps/collections/{id}/map`), WMS uses query parameters (`?REQUEST=GetMap&LAYERS=...`)
- **Maps bbox is always lon/lat order** (no axis-order swapping like WMS 1.3.0 EPSG:4326)
- **Maps supports WebP** output format in addition to PNG and JPEG
- **Maps has named styles** via `/collections/{id}/styles/{styleId}/map`
- **Maps has its own state** (`MapsState`) with styles, render semaphore, and rendered cache

## OGC API Tiles

The server implements OGC API - Tiles Part 1 for serving raster data as tiled map images. Only GeoTIFF collections can be exposed via Tiles (they implement `MapEngine`). Tiles reuses the same rendering pipeline as Maps/WMS — tile z/x/y coordinates are converted to a bbox via TileMatrixSet math, then rendered through `MapEngine::get_raster_tile()`.

### Tile Addressing

Tiles are addressed by `{tileMatrixSetId}/{tileMatrix}/{tileRow}/{tileCol}` where:
- **tileMatrixSetId**: Tiling scheme (e.g., `WebMercatorQuad`, `WorldCRS84Quad`)
- **tileMatrix**: Zoom level (0-24)
- **tileRow**: Row index (0 = top/north)
- **tileCol**: Column index (0 = left/west)

All tiles are fixed 256x256 pixels (not user-configurable).

### Supported TileMatrixSets

| ID | CRS | Description |
|----|-----|-------------|
| `WebMercatorQuad` | EPSG:3857 | Standard web map tiles (Google/OSM scheme) |
| `WorldCRS84Quad` | CRS:84 | Geographic tiles in lon/lat |

TileMatrixSet definitions and bbox computation math live in `api-tiles/src/tilematrixset.rs`.

### Tiles Query Parameters

| Parameter | Required | Default | Description |
|-----------|----------|---------|-------------|
| `datetime` | no | latest | ISO 8601 timestamp |
| `f` | no | `image/png` | Output format: `image/png`, `image/jpeg`, `image/webp` |

Tile coordinates (z, row, col) and TileMatrixSet ID come from URL path parameters.

### Security Limits

| Limit | Value | Location |
|-------|-------|----------|
| MAX_ZOOM_LEVEL | 24 | `api-tiles/src/params.rs` |
| DEFAULT_MAX_ZOOM | 18 | `api-tiles/src/params.rs` |
| TILE_SIZE | 256 px (fixed) | `api-tiles/src/params.rs` |
| Render semaphore | CPU core count (min 4), shared with Maps/WMS | `server/src/admin.rs` |
| TileMatrixSet whitelist | WebMercatorQuad, WorldCRS84Quad | `api-tiles/src/tilematrixset.rs` |
| Format whitelist | `image/png`, `image/jpeg`, `image/webp` | `api-tiles/src/params.rs` |

### Tile Cache

Tiles share the `RenderedCache` with Maps and WMS. Cache keys use quantized bbox computed from tile coordinates, so a Maps request for the same area at 256x256 can share cached results with tile requests. Empty tiles (all nodata) return a pre-generated transparent PNG without cache insertion. This applies to all three APIs (WMS, Maps, Tiles) — empty tiles are never cached to allow recovery from transient failures.

### HTTP Cache Headers

| Scenario | Cache-Control | ETag |
|----------|---------------|------|
| Tile with explicit `datetime=` | `public, max-age=86400, immutable` | Yes |
| Tile without `datetime` (latest) | `public, max-age=60, must-revalidate` | Yes |
| TileMatrixSet metadata | No cache headers | No |

### Adding Tiles to a Collection

Add `"tiles"` to the `apis` array. Tiles reuses the same `[collections.wms]` config for colormap/styles:

```toml
[[collections]]
id = "radar"
engine_type = "geotiff"
apis = ["edr", "wms", "maps", "tiles"]

[collections.geotiff]
filename_template = "radar_%Y%m%dT%H%MZ.tif"
parameter = "reflectivity"
unit = "dBZ"

[collections.wms]
colormap = "radar_dbz"
```

Only `engine_type = "geotiff"` collections support Tiles. CSV and GeoJSON engines do not implement `MapEngine`.

## OpenAPI and Swagger UI

Each OGC API (EDR, Features, Maps, Tiles) serves a dynamic OpenAPI 3.0.3 specification and a Swagger UI page:

| API | OpenAPI spec (service-desc) | Swagger UI (service-doc) |
|-----|---------------------------|-------------------------|
| EDR | `/edr/api` | `/edr/api/docs` |
| Features | `/features/api` | `/features/api/docs` |
| Maps | `/maps/api` | `/maps/api/docs` |
| Tiles | `/tiles/api` | `/tiles/api/docs` |

OpenAPI specs are **generated dynamically** from configured collections — new collections appear automatically after config reload. The Swagger UI is a static HTML page loading `swagger-ui-dist@5` from unpkg CDN. The shared HTML template lives in `ds_core::openapi::swagger_ui_html()`.

Landing pages link to both:
- `rel="service-desc"` → OpenAPI JSON (`application/vnd.oai.openapi+json;version=3.0`)
- `rel="service-doc"` → Swagger UI HTML (`text/html`)

WMS uses XML `GetCapabilities` for service description (no OpenAPI).

When adding new endpoints to any API, update the `api_definition()` handler in that crate's `handlers.rs` to include the new paths in the spec.

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
apis = ["edr", "wms"]

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

### Config fields

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `id` | yes | — | Unique collection identifier, used in URL paths |
| `title` | yes | — | Human-readable collection title |
| `description` | yes | — | Collection description |
| `data_path` | yes | — | Path to data file (CSV or GeoJSON) |
| `apis` | no | `["edr"]` | Which APIs expose this collection: `"edr"`, `"features"`, `"maps"`, `"tiles"`, `"wms"` |
| `engine_type` | no | `"csv"` | Data engine: `"csv"`, `"geojson"`, or `"geotiff"` |
| `wms` | no | — | WMS rendering config (see WMS Config Fields). Required when `apis` contains `"wms"`. |

Server-level fields:

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `host` | yes | — | Bind address |
| `port` | yes | — | Bind port |
| `base_url` | no | `http://{host}:{port}` | External base URL for absolute links (set when behind a reverse proxy) |

## API State Architecture

All API crates use registry-based state instead of a single engine:

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

// api-maps
pub struct MapsState {
    pub engines: HashMap<String, Arc<dyn MapEngine>>,
    pub collections: HashMap<String, CollectionConfig>,
    pub styles: HashMap<String, Vec<StyleInfo>>,
    pub render_semaphore: Arc<Semaphore>,
    pub rendered_cache: Arc<RenderedCache>,
}

// api-tiles
pub struct TilesState {
    pub map_engines: HashMap<String, Arc<dyn MapEngine>>,
    pub collections: HashMap<String, CollectionConfig>,
    pub styles: HashMap<String, HashMap<String, StyleInfo>>,
    pub render_semaphore: Arc<Semaphore>,
    pub rendered_cache: Arc<RenderedCache>,
}

// api-wms
pub struct WmsState {
    pub engines: HashMap<String, Arc<dyn MapEngine>>,
    pub collections: HashMap<String, CollectionConfig>,
    pub colormaps: HashMap<String, Arc<dyn ColorMap>>,
    pub render_semaphore: Arc<Semaphore>,
    pub rendered_cache: Arc<RenderedCache>,
}
```

Handlers look up the engine by collection ID from the URL path parameter (or WMS LAYERS parameter). Unknown collection IDs return 404 (or WMS `LayerNotDefined` XML error). Collection metadata (title, description, links) is built from `CollectionConfig`, not hardcoded.

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

API state (`EdrState`, `FeaturesState`, `MapsState`, `TilesState`, `WmsState`) is wrapped in `ArcSwap` for lock-free reads and atomic swaps on reload. The `ServerState` in `server/src/admin.rs` holds the `ArcSwap` pointers, health registry, and GeoTIFF engine list. Engine loading logic is in `admin::load_collections()`, shared by startup and reload. On reload, the WMS/Maps/Tiles rendered image cache is replaced (old cache is dropped). The render semaphore (sized to available CPU cores, minimum 4) and rendered cache are shared across Maps, Tiles, and WMS APIs. The semaphore uses `acquire().await` so excess requests queue instead of failing — important for animation clients that prefetch many timesteps concurrently.

## Known Limitations

- Parameter units are hardcoded in the CSV loader's match statement
- CSV/GeoJSON data loaded into memory at startup; GeoTIFF reads tiles on demand
- CSV engine supports only the `locations` query type (no position, area, radius, trajectory, corridor)
- GeoTIFF engine supports `position` and `area` queries (no locations, radius, trajectory, corridor)
- GeoJSON engine implements `FeatureEngine` only (not `Engine`/EDR, not `MapEngine`/WMS) — polygon boundary data has no time-series parameters
- GeoTIFF engine implements `Engine` (EDR) and `MapEngine` (Maps/WMS/Tiles) only (not `FeatureEngine`/Features)
- GeoTIFF CRS: WGS84, Transverse Mercator, LAEA, and LCC supported; other projections fall back to WGS84
- GeoTIFF area queries extract the bounding box from POLYGON WKT — they do not clip to the actual polygon shape
- Strip-based (non-tiled) GeoTIFFs are not supported — convert to COG first
- No per-file timeout on remote reads — a hung S3 endpoint blocks the poll cycle
- GeoTIFF multi-band: one band per collection; multiple bands as separate parameters not yet supported
- WMS: single LAYERS only (no multi-layer composition), no SLD/SE styling, no GetFeatureInfo
- WMS: nearest-neighbor resampling only (no bilinear interpolation)
- WMS: JPEG output composites transparency onto white background (no alpha channel support)
- STAC: no retry logic on transient API failures (relies on poll loop to retry next cycle)
- STAC: no HTTP caching (ETag/Last-Modified) — re-fetches item list every poll cycle
- STAC: items with neither `datetime` nor `start_datetime` are silently skipped
- STAC: first query to a datetime range fetches STAC items + GeoTIFF headers on-demand (~100-500ms per file)
- STAC: GeoTIFF metadata loads are capped at 50 per query to prevent timeout on large ranges
- Tiles: only raster map tiles (no vector tiles yet — planned via FeatureEngine)
- Tiles: only WebMercatorQuad and WorldCRS84Quad TileMatrixSets supported
- Tiles: fixed 256x256 tile size (no 512x512 HiDPI support yet)
- Tiles/WMS/Maps: no request coalescing for concurrent identical renders (duplicate work if same tile requested simultaneously)
- Tiles: no per-collection max zoom configuration (hardcoded DEFAULT_MAX_ZOOM = 18, absolute MAX = 24)

## Code Style

- Use `thiserror` for error types, not manual `impl Display`
- Prefer returning `Result<T, DataServerError>` from engine methods
- Keep handlers thin — delegate logic to the engine, map errors to HTTP status codes
- Use `serde_json::json!` macro for building JSON responses
- Do not leak internal error details to clients — use generic messages for 500 errors
