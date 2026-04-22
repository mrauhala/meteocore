# MeteoCore — Claude Instructions

## What This Is

Rust workspace implementing OGC API - EDR, OGC API - Features, OGC API - Maps, OGC API - Tiles, and OGC WMS 1.3.0 servers. Sixteen crates: `ds-core` (traits + types + shared utilities), `ds-storage` (S3/HTTP/local object store), `ds-render` (raster colorization + PNG encoding), `engine-csv` (CSV data engine), `engine-geojson` (GeoJSON data engine), `engine-geotiff` (GeoTIFF/COG data engine), `engine-grib` (GRIB2 NWP data engine), `engine-querydata` (FMI QueryData data engine), `engine-postgis` (PostGIS/TimescaleDB observation data engine), `api-edr` (EDR HTTP layer), `api-features` (Features HTTP layer), `api-maps` (OGC API Maps HTTP layer), `api-tiles` (OGC API Tiles HTTP layer), `api-wms` (WMS 1.3.0 HTTP layer), `server` (binary).

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
| PostGIS | `Engine` + `FeatureEngine` | EDR (position, locations, area), Features |

## GeoTIFF Engine Notes

- **Must be tiled COG.** Strip-based TIFFs are rejected. One parameter (band) per collection.
- **CRS**: WGS84, TM, LAEA, LCC, Stereographic supported. CRS math in `ds-core/src/geo.rs`.
- **Reprojection**: `bbox_to_pixels()` samples 20 points per edge to capture projection curvature.
- **Data sources**: local directory (`data_path`), S3 (`endpoint` + `bucket` + `prefix_pattern`), or STAC (`stac_url` + `stac_asset_allowlist`). Mutually exclusive.
- **STAC security**: `stac_asset_allowlist` is mandatory (SSRF protection). HTTP redirects disabled. Pagination origin-checked.
- **Tile cache**: compressed bytes in LRU cache (default 256 MB). Rendered image cache (default 512 MB) shared across WMS/Maps/Tiles.

## GRIB Engine Notes

- Discovers data via index sidecar files on S3/HTTP, fetches messages via byte-range reads.
- Supports two index formats via `index_format` config: `"ecmwf-json"` (default, JSON-lines as shipped by ECMWF open data) and `"wgrib2"` (colon-separated text as shipped by NOAA GFS).
- Only regular lat/lon grids (Template 0). Multi-parameter collections (unlike GeoTIFF).
- **Unit conversion is driven by the WMO `(discipline, category, parameter_number)` triple read out of every decoded message**, not by hardcoded short-name tables. Source units come from WMO Code Table 4.2 (see `crates/engine-grib/src/units.rs`) plus per-center overlays for local parameter numbers 192-254. Display conversions are mechanical: K→°C, Pa→hPa, kg m⁻²→mm, m² s⁻²→gpm, proportion→%. Colormap ranges use display units.
- **Per-provider vocabularies are not needed.** Adding a new provider only requires new local-overlay entries if it uses local parameter numbers (192-254). The ECMWF-`tcc`-vs-GFS-`TCDC` cloud-cover asymmetry and the `z`-vs-`HGT` geopotential asymmetry are both handled by construction because they use different WMO triples.
- Parameter metadata is populated lazily: `scan_once` runs a bounded eager-probe pass (≤32 messages per scan) against the newest run's first step file so `/collections` metadata is populated by the first poll cycle.
- Wgrib2 index files carry only byte offsets — the last record's length is resolved via `DataStore::head()` on the corresponding data file. If HEAD fails or the file size suggests a partial upload, the index is skipped and retried on the next poll.
- **v1 limitations for GFS support:** only regular lat/lon grids (gaussian-grid products like `gdas.*` fail loudly); accumulated (`acc fcst`) and averaged (`ave fcst`) aggregate fields are dropped, so **`APCP` is unavailable** — use `PRATE` for precipitation; `max fcst`/`min fcst` windowed aggregates are coerced to the end step (preserves `GUST`). Users are strongly advised to set a `parameters` filter when using `index_format = "wgrib2"` — a single GFS 0.25° file contains ~700 messages.
- CCSDS/AEC compression requires `libaec` C library (via `libaec-sys`).

## QueryData Engine Notes

- FMI QueryData (.sqd) binary format. Memory-mapped file access via `memmap2`.
- Multi-parameter: exposes all parameters from the file. `wms_parameter` config selects which to render.
- Polls directory for latest `.sqd` file, atomically swaps via `ArcSwap`.
- Supports WGS84, Stereographic, and Rotated Lat-Lon grids.
- EDR position queries use bilinear interpolation. Map rendering uses nearest-neighbor.
- Missing value sentinel: 32700.0.
- Config: `wms_parameter` (name/short name/ID), `poll_interval_secs` (default 30).

## PostGIS Engine Notes

- Prerequisites: PostgreSQL ≥ 13 + PostGIS ≥ 3.0. TimescaleDB is a supported *deployment* choice (hypertables plan well) but the engine never branches on it.
- Three schema shapes selected by `observations.shape`: `long` (EAV), `wide` (column-per-parameter), `per_parameter` (table-per-parameter, one fan-out query per param).
- **DSN via env var only.** `[postgis].dsn_env` names an env var; a literal `postgres://` URL in TOML is rejected at load unless `MC_ALLOW_INLINE_DB_URL=1`.
- **TLS is deferred to #110.** v1 passes `NoTls`; `sslmode=` in the DSN is parsed but not applied. A startup WARN fires when a non-loopback DSN lacks `sslmode=require`. Until #110 lands, reach the DB over private network/VPN/loopback.
- **Security layers:** every identifier goes through `ds_core::config::is_valid_sql_identifier` at load + `security::quote_ident` at emit; every value is a `$N` bind. `stations.where_clause` is config-time only (no HTTP input reaches it) and validated against a blocklist (DML/DDL verbs + `UNION`/`EXECUTE`/`CALL`/`PERFORM`, `;`, comments) — if you need richer filtering, create a SQL VIEW.
- **Per-URL pool** shared across collections on the same `(host, port, db, user, sslmode)` tuple. First-caller-wins on size; `HARD_POOL_CAP = 32`. Per-load only (no reuse across reloads in v1).
- **Metadata cache** (`ArcSwap<CollectionMeta>`) holds station list, parameter descriptors, temporal extent, spatial bbox. Synchronous bootstrap at construction; background 300 s refresh is deferred to a follow-up.
- **Row caps** (non-configurable, protective invariants): locations query `LIMIT 50_001` (covers FMI ~10k / NOAA COOP ~30k), per-observation-query `LIMIT 10_001`, stations-in-polygon prefilter `LIMIT 501`, nearest-station `LIMIT 1`.
- **Time-zone columns:** `time_col_tz` field required when the mapped `time_col` is `timestamp without time zone`. The WHERE clause wraps the bind (`$N AT TIME ZONE '<tz>'`) so the column btree/BRIN index remains usable; the SELECT list wraps the column to emit `timestamptz` so `DateTime<Utc>` decodes cleanly.
- **No data in window** returns `LocationNotFound` → 404 (matches `CsvEngine`); emitting an empty PointSeries would fail CoverageJSON validation.
- **Supported `pg_type`s** for `property_cols`: `bool`, `int2/4/8`, `float4/8`, `text`/`varchar`/`bpchar`/`name`, and `NULL`. Unsupported types (arrays, json, enums, numeric, timestamp-typed properties) are rejected at refresh time.
- **CI tripwire:** `scripts/check_sql_safety.sh` flags single-line + multi-line `format!`/`concat!` containing SQL verbs, string concatenation onto SQL literals, and `push_str(variable)`. Keep `SELECT` keywords in plain `String` literals, not inside `format!`.

## Config Format

```toml
[server]
host = "0.0.0.0"
port = 8000
# base_url = "https://api.example.com"  # optional, for absolute links behind a proxy
# collections_dir = "collections.d"     # optional, directory of per-collection .toml files

# Optional shared WMS style bundles. MUST live in top-level config.toml —
# a [[style_bundles]] block inside a collections_dir file is silently
# dropped by serde and any collection referencing it will fail validation.
[[style_bundles]]
id = "radar_multi"
[style_bundles.default]
colormap = "radar_dbz"
[[style_bundles.extras]]
name = "radar_fmi"
title = "FMI Radar"
colormap = "radar_fmi"

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
# Either attach a named style_bundle (defined above), or set colormap/styles
# inline — mixing the two in one [wms] block is rejected at config load.
style_bundle = "radar_multi"
# colormap = "radar_dbz"
```

See config struct definitions in each engine crate and `ds-core/src/config.rs` for all fields.

### Per-File Collection Configs (`collections_dir`)

Collections can optionally be defined as individual `.toml` files in a directory instead of (or in addition to) inline `[[collections]]` in `config.toml`.

```toml
# config.toml
[server]
collections_dir = "collections.d"   # relative to config.toml's parent, or absolute
```

```toml
# collections.d/radar-opera.toml — one collection per file, no [[collections]] wrapper
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

**Rules:**
- Both inline `[[collections]]` and `collections_dir` can coexist — inline collections load first, then directory collections sorted alphabetically by filename. Duplicate IDs across sources are rejected.
- Only `.toml` files are loaded; other files are ignored. Rename to `.toml.disabled` to disable a collection.
- The `id` field is required inside each file (not derived from filename). A warning is logged if the filename stem differs from the `id`.
- The directory must exist if `collections_dir` is set (missing directory = hard error). An empty directory is valid but logs a warning.
- Non-recursive: only files directly in the directory, no subdirectory traversal.
- Hot-reload (`POST /admin/collections/reload`) picks up added, removed, and changed files automatically.
- A single invalid file rejects the entire config (no partial loads).
- `[[style_bundles]]` blocks are NOT allowed in per-collection files — only in `config.toml`. Referencing a bundle from a per-collection `[wms]` is fine; defining one here is rejected with an explicit error.
- A `style_bundle` cannot coexist with `[[wms.parameters]]` on the same collection — inline per-parameter *defaults* are rejected at config load when a bundle is attached.
- Inside a bundle, each `[[style_bundles.extras]]` entry may carry an optional `parameter` field. Extras with a `parameter` are scoped to that layer only (e.g. `parameter = "wind_speed"` surfaces only under `collection/wind_speed`); untagged extras are shared across every parameter layer.

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
