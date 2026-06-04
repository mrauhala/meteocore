# MeteoCore — Claude Instructions

## What This Is

Rust workspace implementing OGC API - EDR, OGC API - Features, OGC API - Maps, OGC API - Tiles, and OGC WMS 1.3.0 servers. Seventeen crates: `ds-core` (traits + types + shared utilities), `ds-storage` (S3/HTTP/local object store), `ds-render` (raster colorization + PNG encoding), `ds-mvt` (Mapbox Vector Tile encoder + LRU tile cache), `engine-csv` (CSV data engine), `engine-geojson` (GeoJSON data engine), `engine-geotiff` (GeoTIFF/COG data engine), `engine-grib` (GRIB2 NWP data engine), `engine-odim` (ODIM_H5 weather-radar engine), `engine-querydata` (FMI QueryData data engine), `engine-postgis` (PostGIS/TimescaleDB observation data engine), `api-edr` (EDR HTTP layer), `api-features` (Features HTTP layer), `api-maps` (OGC API Maps HTTP layer), `api-tiles` (OGC API Tiles HTTP layer), `api-wms` (WMS 1.3.0 HTTP layer), `server` (binary).

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

- **Three core traits: `EdrEngine` (EDR), `FeatureEngine` (Features), and `MapEngine` (Maps/WMS/Tiles).** They are separate traits — not all engines need to support all APIs. Engines return domain types, never JSON/XML. Serialization belongs in the API crates.
- **ds-core has no framework dependencies.** Only chrono, serde, thiserror, toml. Keep it that way. Use `PropertyValue` enum instead of `serde_json::Value` for feature properties.
- **CRS and GeoTransform live in ds-core** (`ds_core::geo`), shared by all engines.
- **ds-render has no framework dependencies.** Only ds-core and `png`.
- **API crates depend only on ds-core** (and ds-render for api-wms/api-maps, plus api-edr for its `f=png` profile/time-series plots), not on any engine crate. API state is a registry of engines keyed by collection ID.
- **EDR, Features, Maps, Tiles, and WMS are separate services** with separate base routes (`/edr/...`, `/features/...`, `/maps/...`, `/tiles/...`, `/wms/...`).
- **WMS uses XML, not JSON.** All XML output in api-wms uses `quick-xml::Writer` for proper escaping. Never build XML with `format!()` or string concatenation (XML injection risk).
- **CORS is applied at the server level**, not in individual API crates. The `CorsLayer` lives in `server/src/main.rs`.
- **New engines** implement `EdrEngine`, `FeatureEngine`, and/or `MapEngine` traits in their own crate, get wired up in `server/src/main.rs`.
- **Collection routing is dynamic.** Handlers look up engines from a `HashMap<String, Arc<dyn EdrEngine/FeatureEngine/MapEngine>>` by collection ID from the URL path. No collection IDs are hardcoded.
- **The `apis` config field is enforced.** Only collections listing a given API in their `apis` array are wired to that API's router.
- **Tiles reuses MapEngine.** Tile z/x/y coordinates are converted to a bbox via TileMatrixSet math, then passed to `MapEngine::get_raster_tile()`. No separate tile engine trait is needed.

## Crate Name

The core crate is named `ds-core` in Cargo.toml (imported as `ds_core` in Rust). It was renamed from `core` to avoid shadowing Rust's built-in `core` crate, which breaks proc macros like `#[tokio::main]`.

## Adding a New Engine

1. Create `crates/engine-<name>/` with `Cargo.toml` depending on `ds-core`
2. Implement `EdrEngine`, `FeatureEngine`, and/or `MapEngine` traits
3. Add the crate to workspace members in root `Cargo.toml`
4. Add the crate as a dependency of `crates/server/Cargo.toml`
5. Add a match arm for the new `engine_type` in `server/src/main.rs`
6. Wire it into the appropriate registries based on the collection's `apis` config
7. **Obey the Engine Performance & Concurrency Rules below** — especially: spawn the poll loop on the dedicated background runtime (not the request runtime), and the render/decode path must not project per output pixel.

## Engine Performance & Concurrency Rules

Hard-won from production incidents (epic #201; spike investigation #221/#222). These are *generic* — apply them to every engine and rendering feature, not just the ones where they were found.

### Concurrency / the shared Tokio runtime

- **Poll/scan loops must not block the request-serving runtime.** All engines share ONE multi-thread Tokio runtime (worker count ≈ CPU cores) for the HTTP handlers; a `poll_loop`/`scan_once`/`poll_once` that does blocking I/O directly on a worker thread parks that worker for the whole operation, and when several collections' polls overlap the pool starves and **every** collection's requests spike — periodic multi-second p99 even at low load (#221, #208). **The mechanism in use: run all poll loops on the dedicated background runtime (`poll_runtime()` in `server/src/main.rs`), never the main one** — so their blocking parks background threads, not request workers. A blocking scan *may* additionally be wrapped in `tokio::task::spawn_blocking` (the ODIM engine does for its HDF5 scan) — but **not** when it calls `ds-storage`, whose `block_in_place` (below) *panics* on a `spawn_blocking` pool thread; that panic is exactly why poll isolation uses a separate runtime rather than `spawn_blocking`. Note: as of this writing the grib (`scan_once` inside `poll_loop`), geotiff and querydata (`poll_once`) bodies still do their blocking I/O directly and are made safe only by running on the background runtime — they are *not* individually converted to `spawn_blocking` (#221).
- **`ds-storage` (`DataStore`) is a *sync* bridge over async object_store.** Its methods call `block_in_place(|| handle.block_on(..))` when a runtime handle exists, and **construct a brand-new `Runtime` when one does not** (`crates/storage/src/lib.rs`). `block_in_place` is only valid on a multi-thread-runtime *worker* thread — it panics on a `spawn_blocking` pool thread. So: (a) call `DataStore` from the background poll runtime (or another dedicated runtime), never from a request-handler task (parks a request worker) and never wrapped in `spawn_blocking` (panics); (b) never call it from a non-Tokio thread such as a **rayon** pool — that hits the `Runtime::new()`-per-call fallback (#222). If you need parallel remote fetches, use async concurrency (`join_all`) on the runtime, or pass a `Handle` into the worker, not rayon + `DataStore`.
- **N sequential blocking network calls multiply the stall.** The grib new-run probe did up to 32 sequential byte-range reads on one thread (~seconds). Batch/parallelise (bounded) or cap per cycle; never loop blocking I/O.
- **Background metadata refresh must not contend with request serving.** Discovering new data (S3 LIST, STAC HTTP, GRIB index, COG header) is background work — keep it off the request-serving worker threads.

### Rendering / raster

- **Never project per output pixel.** The CRS forward transform (≈ a dozen transcendental ops for TM/LAEA/LCC/Stereographic) dominates render cost. Evaluate the projection on a coarse, curvature-adaptive grid and bilinearly interpolate the output→source pixel map (`engine-geotiff/src/resample.rs` is the reference; #203). Per-pixel projection was a ~10× regression.
- **A cache only helps if its key matches the access pattern.** The rendered-image cache keyed on exact bbox+width+height gets ~3% hits for fullscreen arbitrary-viewport WMS — pure wasted RAM (#202). Cache at a granularity that actually repeats (tile-aligned), or don't allocate the cache.
- **Decode to compact native types, not `Vec<Option<f64>>`.** Boxing every sample to `Option<f64>` is a 16× memory blowup and allocator churn; carry native ints with a nodata sentinel (#206). Wire up the typed fast paths you build (e.g. `IntegerLutColorMap`, #207) instead of leaving them dead.

### Hot-path discipline

- **Capability/metadata accessors used per request must be O(1) from a snapshot** (`ArcSwap`/`RwLock` read), never recompute or allocate per call (e.g. `raster_info()` cloning all timestamps — #211).
- **Before blaming a mechanism for a latency spike, check the magnitude adds up.** A 2.3 MB local read from page cache is ~tens of ms, not seconds; multi-second stalls at low load almost always mean *blocking/contention* (a parked runtime worker, a held lock, sequential network calls), not extra CPU.

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
- **PointSeries**: x/y are single-value axes, t is string-values axis, z is an optional single-value axis.
- **Grid**: x/y are numeric-values axes, t and z optional. NdArray shape: `[t, y, x]`, `[y, x]`, `[z, y, x]`, or `[t, z, y, x]`.
- **VerticalProfile**: x/y single-value, z numeric-values axis, t optional single-value. NdArray shape: `[z]`.
- **Parameter**: requires `observedProperty` with `label` as i18n object `{"en": "..."}`.
- **NdArray**: requires `shape` and `axisNames` when values has >1 item. `values.length` must equal product of `shape`.
- **i18n objects**: Keys must be BCP 47 language tags (e.g. `"en"`).
- **Reference systems**: Spatial uses `GeographicCRS` with CRS84 id. Temporal uses `TemporalRS` with `"Gregorian"` calendar.

### Adding new domain types

1. Add variant to `DomainDescription` enum in `ds-core/src/model.rs`
2. Add match arm in `build_domain()` in `api-edr/src/response.rs`
3. Check schema's `domainBase.dependencies.domainType` for axis requirements
4. Add a validation test in `covjson_validation.rs`

Currently implemented: `PointSeries`, `Grid`, `VerticalProfile`.

### Vertical dimension

Collections may expose a single vertical axis (`ds_core::vertical::VerticalDimension`,
surfaced on `RasterInfo.vertical` and `EdrEngine::get_vertical_extent`). `MapEngine::get_raster_tile`
takes `z: Option<f64>` (one rendered layer); the `EdrEngine` query methods take
`z: Option<&[f64]>` (one or more levels). The ODIM `odim-volume` engine uses it for
radar elevation angle. WMS exposes it as the `ELEVATION` dimension; Maps/Tiles as an
`elevation` query parameter; EDR as the `z` query parameter. The API layer rejects a
`z`/`elevation` against a collection with no vertical extent (HTTP 400).

## Engine Capabilities

| Engine | Traits | APIs |
|--------|--------|------|
| CSV | `EdrEngine` + `FeatureEngine` | EDR (locations only), Features |
| GeoJSON | `FeatureEngine` | Features |
| GeoTIFF | `EdrEngine` + `MapEngine` | EDR (position, area), WMS, Maps, Tiles |
| GRIB | `EdrEngine` + `MapEngine` | EDR, WMS, Maps, Tiles |
| ODIM COMP | `EdrEngine` + `MapEngine` | EDR (position, area), WMS, Maps, Tiles |
| ODIM PVOL | `EdrEngine` + `MapEngine` (per-site views) + `FeatureEngine` (network engine) | EDR (position, locations, area, trajectory), WMS, Maps, Tiles, Features (site inventory) |
| QueryData | `EdrEngine` + `MapEngine` | EDR (position only), WMS, Maps, Tiles |
| PostGIS | `EdrEngine` + `FeatureEngine` | EDR (position, locations, area), Features |

## ODIM PVOL Engine Notes (per-site collections)

- **One source → N collections.** A single `engine_type = "odim-volume"` config scans a directory / S3 prefix of `.h5` polar volumes spanning a whole radar *network*, then expands into **one OGC collection per radar site** (ODIM `nod`), with id `{base_id}-{nod}` (e.g. `radar-fi-volume-local-h5-fivih`). There is no network-level aggregate *raster* collection; the base id instead serves the **site inventory** as Features (below).
- **Base id = site inventory (Features).** When the source's `apis` includes `"features"`, the **owning `PolarVolumeEngine`** is registered as a `FeatureEngine` under the **base id** — one Point Feature per radar site (id = NOD, geometry = antenna, properties = `name`/`wmo`/`quantities`/`elevation_angles`/`coverage_radius_m`/`latest_volume_time`/`volume_count`/`collection`). `quantities`/`elevation_angles` use the `PropertyValue::List` variant (clean JSON arrays). `GET /features/collections/{base}/items[/{nod}]` with bbox/limit/offset/datetime. This is the one place the *engine* (not the per-site views) implements an API trait — views = per-site data, engine = network inventory.
- **The engine owns the source; views serve the per-site APIs.** `PolarVolumeEngine` (in `engine-odim/src/volume_engine.rs`) does the scan, parse cache, and poll loop and is registered only on the background poll runtime. Each site is served by a cheap `PolarVolumeSiteView` over the engine's shared `Arc<ArcSwap<Catalog>>` — so all views see poll refreshes for free. The engine implements **only** `FeatureEngine` (the inventory); the per-site `EdrEngine`/`MapEngine` live on the views.
- **Parameter = bare quantity.** A site collection's parameters are the bare ODIM quantities (`DBZH`, `VRADH`, `ZDR`, `RHOHV`, …) — **never** `<nod>:<quantity>`. The site is the collection (its single EDR location, its spatial/vertical extent), so it has no place in the parameter name. This also lets WMS styling key off the quantity: set per-quantity colormaps with `[[wms.parameters]]` (name = bare quantity) on the source config and every site inherits them.
- **Labels come from the ODIM quantity dictionary.** The bare quantity stays the parameter *id* (URL short-name, WMS `<Name>`, CoverageJSON key), but the human-readable *label* and unit come from `engine-odim/src/quantities.rs` (acronym + name, e.g. `DBZH` → `"DBZH — Reflectivity (horizontal)"`, unit `dBZ`); unknown codes fall back to the bare string. WMS additionally prefixes each child layer's `<Title>` with the site place name (`RasterInfo.layer_subtitle`, ODIM `/what` PLC) so flat clients that ignore the parent-layer tree can tell the per-site layers apart.
- **Auto-split happens in `server/src/admin.rs`** (`load_collections`, the `"odim-volume"` arm): build the engine once, enumerate `engine.sites()` (returns `(nod, label)` per site), and register one `PolarVolumeSiteView` per site (cloning the base `CollectionConfig` with a per-site id/title). Site discovery is a scan snapshot — sites added later surface on the next `POST /admin/collections/reload`.
- **Cross-sections.** `query_trajectory` returns a CoverageJSON `Section` (composite `[t,x,y]` axis + numeric `z` = height above antenna, via the 4/3-Earth beam model). `z` selects the elevation-angle band. Vertical axis is elevation angle (`VerticalKind::ElevationAngle`).

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
- **Session limits come from the role, not the engine.** `statement_timeout`, `lock_timeout`, and `default_transaction_read_only` are set via `ALTER ROLE meteocore_ro SET ...` (see crate README). The engine uses `RecyclingMethod::Fast` and does not issue `SET` on checkout, so a superuser DSN or an unconfigured role bypasses those limits entirely. The role-setup SQL is operational-mandatory, not optional.
- **Live health monitoring is NOT implemented** (tracked in #110). `/health` reflects boot-time status only — a DB that goes down after startup will still show `ready`. A prominent WARN fires per collection at `PostgisEngine::new()` to make this explicit.
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

# Optional /preview SPA tuning. Cap the time slider's `values[]` to the most
# recent ISO 8601 duration before the latest timestep. Useful for STAC-backed
# collections whose archive spans years but whose useful scrub range is the
# last few hours. Does NOT constrain the underlying engine — only the manifest.
[collections.preview]
time_window = "PT12H"
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
- **State**: API state wrapped in `ArcSwap` for lock-free reads. Render semaphore (2× CPU cores, min 8) shared across Maps/Tiles/WMS. Engine loading in `server/src/admin.rs`.

## Code Style

- Use `thiserror` for error types, not manual `impl Display`
- Prefer returning `Result<T, DataServerError>` from engine methods
- Keep handlers thin — delegate logic to the engine, map errors to HTTP status codes
- Use `serde_json::json!` macro for building JSON responses
- Do not leak internal error details to clients — use generic messages for 500 errors
