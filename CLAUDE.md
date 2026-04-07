# MeteoCore — Claude Instructions

## What This Is

Rust workspace implementing OGC API - EDR, OGC API - Features, OGC API - Maps, OGC API - Tiles, and OGC WMS 1.3.0 servers. Fifteen crates: `ds-core` (traits + types + shared utilities), `ds-storage` (S3/HTTP/local object store), `ds-render` (raster colorization + PNG encoding), `engine-csv` (CSV data engine), `engine-geojson` (GeoJSON data engine), `engine-geotiff` (GeoTIFF/COG data engine), `engine-grib` (GRIB2 NWP data engine), `engine-querydata` (FMI QueryData data engine), `api-edr` (EDR HTTP layer), `api-features` (Features HTTP layer), `api-maps` (OGC API Maps HTTP layer), `api-tiles` (OGC API Tiles HTTP layer), `api-wms` (WMS 1.3.0 HTTP layer), `server` (binary).

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

## Project Tracking

Backlog is tracked in **GitHub Issues**: https://github.com/mrauhala/meteocore/issues

### Labels
- **Priority:** `priority: high`, `priority: medium`, `priority: low`
- **Effort:** `effort: tiny` (<1h), `effort: small` (1-4h), `effort: medium` (½-1 day), `effort: large` (multi-day)
- **Type:** `bug`, `enhancement`, `security`, `performance`, `reliability`, `architecture`, `operational`, `spec-compliance`
- **Epic:** `epic` — parent issues with task lists tracking sub-issues

### Milestones
- **v0.2** — Radar engines + rendering fixes
- **v0.3** — QueryData improvements + multi-band GeoTIFF
- **v1.0** — Spec compliance + production hardening

### Useful commands
```bash
gh issue list                              # All open issues
gh issue list -l "priority: high"          # High priority only
gh issue list --milestone "v0.2"           # Issues in v0.2 milestone
gh issue create --title "..." --label "bug,priority: high" --milestone "v0.2"
```

When completing work, close the relevant issue: `gh issue close <number>`.

## Architecture Rules

- **Three core traits: `Engine` (EDR), `FeatureEngine` (Features), and `MapEngine` (Maps/WMS/Tiles).** They are separate traits — not all engines need to support all APIs. Engines return domain types, never JSON/XML. Serialization belongs in the API crates.
- **ds-core has no framework dependencies.** Only chrono, serde, thiserror, toml. Keep it that way. Use `PropertyValue` enum instead of `serde_json::Value` for feature properties.
- **CRS and GeoTransform live in ds-core** (`ds_core::geo`), shared by all engines.
- **ds-render has no framework dependencies.** Only ds-core and `png`.
- **API crates depend only on ds-core** (and ds-render for api-wms/api-maps), not on any engine crate. API state is a registry of engines keyed by collection ID.
- **EDR, Features, Maps, Tiles, and WMS are separate services** with separate base routes (`/edr/...`, `/features/...`, `/maps/...`, `/tiles/...`, `/wms/...`).
- **WMS uses XML, not JSON.** All XML output in api-wms uses `quick-xml::Writer` for proper escaping. Never build XML with `format!()` or string concatenation (XML injection risk).
- **CORS is applied at the server level**, not in individual API crates. The `CorsLayer` lives in `server/src/main.rs`.
- **New engines** implement `Engine`, `FeatureEngine`, and/or `MapEngine` traits in their own crate, get wired up in `server/src/main.rs`.
- **Collection routing is dynamic.** Handlers look up engines from a `HashMap<String, Arc<dyn Engine/FeatureEngine/MapEngine>>` by collection ID from the URL path. No collection IDs are hardcoded.
- **The `apis` config field is enforced.** Only collections listing a given API in their `apis` array are wired to that API's router.
- **Tiles reuses MapEngine.** Tile z/x/y coordinates are converted to a bbox via TileMatrixSet math, then passed to `MapEngine::get_raster_tile()`. No separate tile engine trait is needed.

## Crate Name

The core crate is named `ds-core` in Cargo.toml (imported as `ds_core` in Rust). It was renamed from `core` to avoid shadowing Rust's built-in `core` crate, which breaks proc macros like `#[tokio::main]`.

## Adding a New Engine

1. Create `crates/engine-<name>/` with `Cargo.toml` depending on `ds-core`
2. Implement `Engine`, `FeatureEngine`, and/or `MapEngine` traits
3. Add the crate to workspace members in root `Cargo.toml`
4. Add the crate as a dependency of `crates/server/Cargo.toml`
5. Add a match arm for the new `engine_type` in `server/src/main.rs`
6. Wire it into the appropriate registries based on the collection's `apis` config

## Adding a New API Endpoint

Same pattern for all API crates (api-edr, api-features, api-maps, api-tiles):

1. Add the handler function in `handlers.rs`
2. Add the route in `lib.rs`
3. If new query params are needed, add them in `params.rs`
4. If new response formats are needed, add serializers in `response.rs`
5. **Update `api_definition()` in `handlers.rs`** to include the new path in the OpenAPI spec

## WMS 1.3.0 Axis Order

**Critical gotcha:** WMS 1.3.0 BBOX axis order depends on the CRS:

- **CRS:84**: `BBOX=west,south,east,north` (lon/lat — same as internal)
- **EPSG:4326**: `BBOX=south,west,north,east` (lat/lon — swapped!)
- **EPSG:3857, EPSG:3067, EPSG:3035**: `BBOX=minx,miny,maxx,maxy` (easting/northing)

The handler normalizes all bbox values to `[west, south, east, north]` internally. Test with both CRS:84 and EPSG:4326 to catch axis order bugs.

## CoverageJSON Schema Compliance

All CoverageJSON output **must** validate against the OGC CoverageJSON 1.0 schema at `schemas/coveragejson.json`. Integration tests in `crates/api-edr/tests/covjson_validation.rs` validate against this schema.

**When modifying `response.rs` or adding new CoverageJSON output, always run `cargo test -p api-edr`.**

### Key schema rules

- **Coverage**: requires `type` ("Coverage"), `domain`, `parameters`, `ranges`
- **Domain**: requires `type` ("Domain"), `axes`, `referencing`. `domainType` triggers axis constraints.
- **PointSeries**: x/y are single-value axes, t is string-values axis. No additional axes allowed.
- **Grid**: x/y are numeric-values axes, t is optional. NdArray shape: `[t, y, x]` or `[y, x]`.
- **Parameter**: requires `observedProperty` with `label` as i18n object `{"en": "..."}`.
- **NdArray**: requires `shape` and `axisNames` when values has >1 item. `values.length` must equal product of `shape`.
- **i18n objects**: Keys must be BCP 47 language tags (e.g. `"en"`).
- **Reference systems**: Spatial uses `GeographicCRS` with CRS84 id. Temporal uses `TemporalRS` with `"Gregorian"` calendar.

### Adding new domain types

1. Add variant to `DomainDescription` enum in `ds-core/src/model.rs`
2. Add match arm in `build_domain()` in `api-edr/src/response.rs`
3. Check schema's `domainBase.dependencies.domainType` for axis requirements
4. Add a validation test in `covjson_validation.rs`

Currently implemented: `PointSeries`, `Grid`.

## Engine Capabilities

| Engine | Traits | APIs |
|--------|--------|------|
| CSV | `Engine` | EDR (locations only) |
| GeoJSON | `FeatureEngine` | Features |
| GeoTIFF | `Engine` + `MapEngine` | EDR (position, area), WMS, Maps, Tiles |
| GRIB | `Engine` + `MapEngine` | EDR, WMS, Maps, Tiles |
| QueryData | `Engine` + `MapEngine` | EDR (position only), WMS, Maps, Tiles |

## GeoTIFF Engine Notes

- **Must be tiled COG.** Strip-based TIFFs are rejected. One parameter (band) per collection.
- **CRS**: WGS84, TM, LAEA, LCC, Stereographic supported. CRS math in `ds-core/src/geo.rs`.
- **Reprojection**: `bbox_to_pixels()` samples 20 points per edge to capture projection curvature.
- **Data sources**: local directory (`data_path`), S3 (`endpoint` + `bucket` + `prefix_pattern`), or STAC (`stac_url` + `stac_asset_allowlist`). Mutually exclusive.
- **STAC security**: `stac_asset_allowlist` is mandatory (SSRF protection). HTTP redirects disabled. Pagination origin-checked.
- **Tile cache**: compressed bytes in LRU cache (default 256 MB). Rendered image cache (default 512 MB) shared across WMS/Maps/Tiles.

## GRIB Engine Notes

- Discovers data via JSON index sidecar files (`.index`), fetches messages via byte-range reads.
- Only regular lat/lon grids (Template 3.0). Multi-parameter collections (unlike GeoTIFF).
- Auto-converts K→°C, Pa→hPa, m→mm, 0-1→%, m²/s²→gpm. Colormap ranges use display units.
- CCSDS/AEC compression requires `libaec` C library (via `libaec-sys`).

## QueryData Engine Notes

- FMI QueryData (.sqd) binary format. Memory-mapped file access via `memmap2`.
- Multi-parameter: exposes all parameters from the file. `wms_parameter` config selects which to render.
- Polls directory for latest `.sqd` file, atomically swaps via `ArcSwap`.
- Supports WGS84, Stereographic, and Rotated Lat-Lon grids.
- EDR position queries use bilinear interpolation. Map rendering uses nearest-neighbor.
- Missing value sentinel: 32700.0.
- Config: `wms_parameter` (name/short name/ID), `poll_interval_secs` (default 30).

## Config Format

```toml
[server]
host = "0.0.0.0"
port = 8000
# base_url = "https://api.example.com"  # optional, for absolute links behind a proxy

[[collections]]
id = "weather"
title = "Finnish Weather Observations"
description = "Hourly weather observations"
data_path = "testdata/weather.csv"
apis = ["edr", "features"]     # defaults to ["edr"]
engine_type = "csv"             # defaults to "csv"

[[collections]]
id = "radar"
engine_type = "geotiff"
apis = ["edr", "wms", "maps", "tiles"]

[collections.geotiff]
filename_template = "radar_%Y%m%dT%H%MZ.tif"
parameter = "reflectivity"
unit = "dBZ"
nodata = 255
endpoint = "https://s3.example.com"
bucket = "radar-data"
prefix_pattern = "%Y/%m/%d/"
time_window = "-PT2H"
max_files = 24

[collections.wms]
colormap = "radar_dbz"
```

See config struct definitions in each engine crate and `ds-core/src/config.rs` for all fields.

## Admin & Operations

- **Reload**: `POST /admin/collections/reload` — re-reads config, atomically swaps engines.
- **Health**: `GET /health` — per-collection status (ready/degraded/failed). HTTP 503 only when all failed.
- **Metrics**: `GET /metrics` — Prometheus format. Path labels use route patterns (not raw URLs) to avoid cardinality explosion.
- **State**: API state wrapped in `ArcSwap` for lock-free reads. Render semaphore (CPU cores, min 4) shared across Maps/Tiles/WMS. Engine loading in `server/src/admin.rs`.

## Code Style

- Use `thiserror` for error types, not manual `impl Display`
- Prefer returning `Result<T, DataServerError>` from engine methods
- Keep handlers thin — delegate logic to the engine, map errors to HTTP status codes
- Use `serde_json::json!` macro for building JSON responses
- Do not leak internal error details to clients — use generic messages for 500 errors
