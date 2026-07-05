# MeteoCore — Claude Instructions

## What This Is

Rust workspace implementing OGC API - EDR, OGC API - Features, OGC API - Maps, OGC API - Tiles, OGC WMS 1.3.0, and OGC 3D Tiles servers. Twenty-one crates: `ds-core` (traits + types + shared utilities), `ds-storage` (S3/HTTP/local object store), `ds-render` (raster colorization + PNG encoding), `ds-mvt` (Mapbox Vector Tile encoder + LRU tile cache), `ds-3dtiles` (OGC 3D Tiles `.pnts`/`tileset.json` encoder), `engine-csv` (CSV data engine), `engine-geojson` (GeoJSON data engine), `engine-geotiff` (GeoTIFF/COG data engine), `engine-grib` (GRIB2 NWP data engine), `engine-odim` (ODIM_H5 weather-radar engine), `engine-querydata` (FMI QueryData data engine), `engine-zarr` (Zarr V2/V3 multidimensional-array engine), `engine-postgis` (PostGIS/TimescaleDB observation data engine), `engine-cap` (CAP v1.2 alert engine — Features + vector→raster Maps/WMS), `api-edr` (EDR HTTP layer), `api-features` (Features HTTP layer), `api-maps` (OGC API Maps HTTP layer), `api-tiles` (OGC API Tiles HTTP layer), `api-wms` (WMS 1.3.0 HTTP layer), `api-3dtiles` (OGC 3D Tiles HTTP layer), `server` (binary).

## Build & Run

```bash
cargo build                  # Build all crates
cargo test                   # Run all tests
cargo run -p server          # Start server (reads config.toml)
cargo check -p <crate>       # Type-check a single crate
```

### Server CLI flags

The `server` binary takes a small hand-rolled set of flags (no `clap`), each
also accepting `--flag=value` (see `parse_cli_args` in `server/src/main.rs`):

- `--collections <id1,id2,…>` — only load these collection IDs.
- `--host <HOST>` / `--port <PORT>` — override `[server].host`/`port` (CLI wins
  over config). `BASE_URL` still wins for link generation.
- `--config <PATH>` — config file path (wins over `CONFIG_PATH` env, then
  `./config.toml`). A missing `--config` path is a hard error.
- `--auto-collections <DIR>` — auto-discover collections from a directory tree
  (repeatable); see below.

**No-config boot:** if the default config path is absent **and** no `--config`
is given, the server starts from built-in defaults — host `127.0.0.1`, and it
**auto-scans for the first free port at/above 8000** (up to 100 ports). A port
pinned by config or `--port` does **not** auto-scan: a bind conflict is fatal.
Combine with `--auto-collections` for a zero-config `server --auto-collections
./data`.

**Auto-collections (`server/src/auto.rs`, #411 phase 1):** `--auto-collections
<DIR>` synthesizes `CollectionConfig`s from data files on disk (no TOML). Mapping
is **per-subdirectory + loose files**: each immediate subdir → a collection;
loose files in the root → grouped under the root name. Detection (first match
wins): zarr store (`zarr.json`/`.zgroup`/`*.zarr` name) → `zarr`; `*.sqd` →
`querydata`; `*.grib2`+index sidecars → `grib` (`.idx`→wgrib2, `.index`→ecmwf-json;
**no index ⇒ skipped**, the engine never builds them); `*.tif`/`*.h5` → **phase 2,
skipped** (need filename-template inference + ODIM COMP/PVOL probe); `*.geojson`
and `*.csv` → one collection **per file**. Each collection enables **all APIs
relevant to its type** (mirroring the `engine_type → supported_apis` allowlist):
raster/grid (zarr/grib/querydata) get edr+wms+maps+tiles, csv gets edr+features,
geojson gets features+tiles — so the data renders + shows a parameter selector in
`/preview` without a `[wms]` block (the render path falls back to a default
viridis colormap; range is generic `0..1` until a per-collection `[wms]`
colormap/min-max is set — #320). Synthesized configs are
appended to `config.collections` and run through the same `ServerConfig::validate()`
as TOML (duplicate ids rejected). Resolved once at startup (reload does not
re-scan auto roots in v1). Engine-config defaults come from
`{QueryData,Grib,Zarr}Config::auto_*` constructors in `ds-core` (reuse the serde
default fns — keep them DRY). The root may itself be a Zarr store (not just its
parent). **Symlinks are followed** (`is_dir`/`is_file` resolve them) — same trust
model as `collections_dir` (whoever can write the scan root controls what's served).

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

- **Four core traits: `EdrEngine` (EDR), `FeatureEngine` (Features), `MapEngine` (Maps/WMS/Tiles), and `VolumeEngine` (3D Tiles — volumetric point clouds).** They are separate traits — not all engines need to support all APIs. Engines return domain types, never JSON/XML. Serialization belongs in the API crates (`ds-3dtiles` is the framework-free byte encoder for the 3D Tiles path, mirroring `ds-render`/`ds-mvt`).
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
- **All EPSG:3857 ↔ WGS84 conversion goes through `ds_core::web_mercator`** — the single source of truth for `lon_to_x`/`x_to_lon`/`lat_to_y`/`y_to_lat`. Do **not** re-implement `R·ln(tan(π/4+φ/2))` or its inverse anywhere (engines, the meta-tile assembly, WMS/Tiles bbox parsing). Four independent copies once existed and drifted: the meta-tile copy clamped latitude to ±85° for its viewport bounds while the others didn't, mis-scaling the assembled image vs how the client reads it and displacing data ~10° toward the pole on zoomed-out views (#452). The shared functions are **unclamped** — Web Mercator is defined for any lat in (−90°, 90°); the ±85.0511° limit (`web_mercator::LAT_LIMIT_DEG`) is *only* about where the tile grid is cut off. **Never clamp latitude in a viewport/bbox conversion** (a zoomed-out request legitimately reaches past ±85°, and the client maps the image over the full requested extent); clamp to `LAT_LIMIT_DEG` *only* when selecting tile-grid indices.
- **All raster output→source coordinate mapping goes through `OutputCrs`/`ProjectionGrid`** (the `MapEngine::get_raster_tile` path). The WMS Web-Mercator **meta-tile assembly** (`ds-render/src/metatile.rs`) is the *only* place that re-derives the output coordinate map separately, so it must stay consistent with the engine path — that mismatch is exactly what made #452 invisible to the direct-path fix (#448). When debugging a "data displaced/misplaced" report, **confirm which render path the failing request actually uses**: WMS EPSG:3857 goes through meta-tiling, not the direct `get_raster_tile` that Maps/Tiles use.

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

### Model runs / forecast reference time (EDR instances) — #337

Forecast datasets have **two** time axes: the **model run** (forecast *reference
time* / init / analysis time) and the **valid time** (run + lead). The shared
machinery lives in **`ds_core::instances`** so every forecast engine selects runs,
builds instance lists, and encodes instance ids **identically** (no per-engine
duplication):

- `RunInfo { reference_time, valid_times }` — one model run; `instance_id()` =
  canonical compact stamp `%Y%m%dT%H%MZ`.
- `format_instance_id` / `parse_instance_id` — the URL ↔ reference-time codec
  (parse also accepts RFC 3339); the **API layer** owns the string form, engines
  only see `Option<DateTime<Utc>>`.
- `select_run(&BTreeMap<DateTime<Utc>, T>, Option<DateTime<Utc>>)` — `None` ⇒
  latest, `Some(rt)` ⇒ that exact run (absent ⇒ `None` → 404). The one selection
  rule everywhere.
- `build_instances(&runs, |rt, run| valid_times)` — `Vec<RunInfo>` from a
  reference-time-keyed run map.

**Engine contract:** store runs in a `BTreeMap<DateTime<Utc>, _>` keyed by
reference time; implement `EdrEngine::get_instances() -> Vec<RunInfo>` (default
empty = non-forecast); honour the trailing `reference_time: Option<DateTime<Utc>>`
on the query methods and `MapEngine::get_raster_tile` (`None` ⇒ latest); populate
`RasterInfo.reference_times`. **GRIB** (catalog `runs` map) and **QueryData**
(`max_runs` recent `.sqd` files, keyed by origin time) implement it; other engines
accept-and-ignore. Zarr's forecast-reference/lead detection → instances is a
follow-up (it currently pins the latest run internally; see [[project_zarr_engine]]).

**API surface:** EDR exposes `GET /collections/{id}/instances`,
`/instances/{instanceId}` (per-run metadata), and `/instances/{instanceId}/{position,area}`
(query a specific run); the no-instance routes default to the latest run
(unchanged). Collection metadata gains an `instances` data_query and the OpenAPI
spec advertises the instance paths — both gated on `get_instances()` being
non-empty.

**WMS `reference_time` dimension (done).** Forecast layers (non-empty
`RasterInfo.reference_times`) advertise a custom `<Dimension name="reference_time">`
in `GetCapabilities` alongside the standard `time` dimension (the valid-time axis),
defaulting to the latest run. `GetMap` accepts `DIM_REFERENCE_TIME=<run>`
(RFC 3339, or the compact instance-id stamp); the handler validates it against the
advertised runs and returns `InvalidDimensionValue` (HTTP 400) for an unknown run
or a non-forecast layer (no `nearestValue` — the engine requires an exact match).
The run flows through `get_raster_tile` and into the rendered + meta-tile cache
keys (`CacheKey.reference_time`, `TileKeyPrefix.reference_time`) so distinct runs
don't collide. **Maps/Tiles `reference_time` query parameter is still a follow-up**
(api-maps/api-tiles pass `reference_time: None` today).

## Engine Capabilities

| Engine | Traits | APIs |
|--------|--------|------|
| CAP | `FeatureEngine` + `MapEngine` | Features, WMS, Maps, Tiles (severity-shaded alert polygons) |
| CSV | `EdrEngine` + `FeatureEngine` | EDR (locations only), Features |
| GeoJSON | `FeatureEngine` | Features, Tiles (MVT) |
| GeoTIFF | `EdrEngine` + `MapEngine` | EDR (position, area), WMS, Maps, Tiles |
| GRIB | `EdrEngine` + `MapEngine` | EDR, WMS, Maps, Tiles |
| ODIM COMP | `EdrEngine` + `MapEngine` | EDR (position, area), WMS, Maps, Tiles |
| ODIM PVOL | `EdrEngine` + `MapEngine` + `VolumeEngine` (per-site views) + `FeatureEngine` (network engine) | EDR (position, locations, area, trajectory), WMS, Maps, Tiles, 3D Tiles, Features (site inventory) |
| QueryData | `EdrEngine` + `MapEngine` | EDR (position only), WMS, Maps, Tiles |
| Zarr | `EdrEngine` + `MapEngine` | EDR (position), WMS, Maps, Tiles; local + S3/HTTP |
| PostGIS | `EdrEngine` + `FeatureEngine` | EDR (position, locations, area), Features |

## ODIM PVOL Engine Notes (per-site collections)

- **One source → N collections.** A single `engine_type = "odim-volume"` config scans a directory / S3 prefix of `.h5` polar volumes spanning a whole radar *network*, then expands into **one OGC collection per radar site** (ODIM `nod`), with id `{base_id}-{nod}` (e.g. `radar-fi-volume-local-h5-fivih`). There is no network-level aggregate *raster* collection; the base id instead serves the **site inventory** as Features (below).
- **Base id = site inventory (Features).** When the source's `apis` includes `"features"`, the **owning `PolarVolumeEngine`** is registered as a `FeatureEngine` under the **base id** — one Point Feature per radar site (id = NOD, geometry = antenna, properties = `name`/`wmo`/`quantities`/`elevation_angles`/`coverage_radius_m`/`latest_volume_time`/`volume_count`/`collection`). `quantities`/`elevation_angles` use the `PropertyValue::List` variant (clean JSON arrays). `GET /features/collections/{base}/items[/{nod}]` with bbox/limit/offset/datetime. This is the one place the *engine* (not the per-site views) implements an API trait — views = per-site data, engine = network inventory.
- **The engine owns the source; views serve the per-site APIs.** `PolarVolumeEngine` (in `engine-odim/src/volume_engine.rs`) does the scan, parse cache, and poll loop and is registered only on the background poll runtime. Each site is served by a cheap `PolarVolumeSiteView` over the engine's shared `Arc<ArcSwap<Catalog>>` — so all views see poll refreshes for free. The engine implements **only** `FeatureEngine` (the inventory); the per-site `EdrEngine`/`MapEngine` live on the views.
- **Parameter = bare quantity.** A site collection's parameters are the bare ODIM quantities (`DBZH`, `VRADH`, `ZDR`, `RHOHV`, …) — **never** `<nod>:<quantity>`. The site is the collection (its single EDR location, its spatial/vertical extent), so it has no place in the parameter name. This also lets WMS styling key off the quantity: set per-quantity colormaps with `[[wms.parameters]]` (name = bare quantity) on the source config and every site inherits them.
- **Labels come from the ODIM quantity dictionary.** The bare quantity stays the parameter *id* (URL short-name, WMS `<Name>`, CoverageJSON key), but the human-readable *label* and unit come from `engine-odim/src/quantities.rs` (acronym + name, e.g. `DBZH` → `"DBZH — Reflectivity (horizontal)"`, unit `dBZ`); unknown codes fall back to the bare string. WMS additionally prefixes each child layer's `<Title>` with the site place name (`RasterInfo.layer_subtitle`, ODIM `/what` PLC) so flat clients that ignore the parent-layer tree can tell the per-site layers apart.
- **Render resampling is configurable (`[odim] resampling`, `OdimConfig.resampling`).** The per-site Cartesian render (`polar_sample`, WMS/Maps/Tiles) defaults to **`nearest`** — each output pixel takes its single enclosing polar cell, so cells tile the plane and widen with range and the discrete radar bins stay visible with every gate's value preserved. Set `resampling = "bilinear"` to instead blend the four surrounding `(ray, bin)` cells (`bilinear_cell`), smoothing the radial bin/wedge structure that gets coarse far from the radar (#186) at the cost of softening peaks/detail. The flag flows engine → every `PolarVolumeSiteView`. **It does NOT affect:** the COMP composite render (always nearest), EDR position/area queries (always nearest — a point query wants the measurement, not a cosmetic blend), or the 3D Tiles voxel/isosurface/echo-top products (those sample nearest then apply a separate 3-D `smooth_grid` blur in `ds-3dtiles`, #381; the point cloud is raw). `nearest` was the pre-#186 default; #186 made bilinear unconditional, and this restores nearest as the default while keeping bilinear available per collection.
- **Pixel pre-warm (`[odim] prewarm_sweeps`, `OdimConfig.prewarm_sweeps`, default `1`; #461).** Pixel arrays are read lazily per moment (#289) — a moment's first render does a cold **whole-`.h5` S3 download while holding a render-semaphore permit**, so a client animating the latest N timesteps of a **remote** PVOL fires N concurrent cold downloads that contend; some requests get cut off by the front proxy with an empty body before the slow render finishes ("missing timestep" frames — the symptom is intermittent, retry-works-instantly, because the backend finishes and caches it). Fix: the poll loop already holds each new volume's full bytes to build catalog metadata; `prewarm_pixels` (called from **both** arms of `build_catalog`) decodes the lowest `prewarm_sweeps` sweeps' moments **straight from those bytes on the background runtime** into `PIXEL_CACHE`, so the first interactive render is a cache hit. Originally remote-only (#461) on the assumption that local re-reads hit the OS page cache — **wrong off-peak**: prod's file cache is reclaimed between renders at night, so local first renders paid a cold 20–40 MB read + full HDF5 parse under the render permit (~2–3 s p99 spikes); #472 warms the local arm too. Best-effort + additive (never marks known-bad; `PixelCache::contains` skips a moment already resident, so a beyond-`max_files` volume re-fetched on a later poll doesn't re-decode); bounded by the pixel-cache byte LRU (`MC_PVOL_PIXEL_CACHE_MB`). `0` disables. Default `1` warms the base tilt (the standard reflectivity animation view, all of its quantities); raise it to warm more tilts. Complements #293 (batch-decode-on-first-miss, request-time) and #233 (interactive per-render deadline / reduced request-path retry budget — the "fail fast instead of pinning a permit" half, still open).
- **Auto-split happens in `server/src/admin.rs`** (`load_collections`, the `"odim-volume"` arm): build the engine once, enumerate `engine.sites()` (returns `(nod, label)` per site), and register one `PolarVolumeSiteView` per site (cloning the base `CollectionConfig` with a per-site id/title). Site discovery is a scan snapshot — sites added later surface on the next `POST /admin/collections/reload`.
- **Cross-sections.** `query_trajectory` returns a CoverageJSON `Section` (composite `[t,x,y]` axis + numeric `z` = height above antenna, via the 4/3-Earth beam model). `z` selects the elevation-angle band. Vertical axis is elevation angle (`VerticalKind::ElevationAngle`).
- **Storm cells (#367).** `ds_core::cells` segments a `VoxelGrid` into `StormCell`s on the **column-maximum 2-D projection** (mask = any voxel in the `(radius, azimuth)` column ≥ threshold, default 35 dBZ; one-cell morphological closing bridges speckle gaps; 4-neighbour CC, azimuth seam wraps) — the TITAN-style operational convention, so footprints **never nest or overlap** on a map and vertically-split echo is ONE cell (full-3-D CC rendered as nested-ring spaghetti — user-reported, fixed). Attributes come from the 3-D member voxels (max dBZ, linear-Z centroid, echo top/base, volume, area, column-max VIL w/ 56 dBZ hail cap) + a deterministic CCW footprint ring, and `track_cells` matches centroids across scans (gated greedy w/ constant-velocity prediction; `Track.motion_ms (u,v)` is the future motion/optical-flow seed). Generic surface = `VolumeEngine::read_cells(CellQuery)` (default impl over `read_voxel_grid`; window clamped to `MAX_TRACK_SCANS`; empty target scan = valid empty result, NOT 404). The PVOL view overrides it with the byte-bounded per-volume `CELL_SET_CACHE` (`MC_PVOL_CELL_SET_CACHE_MB`, default 64; volumes immutable ⇒ never stale) and gates to dBZ-unit quantities (VIL/linear-Z is reflectivity physics → else 400). **WMS/Maps/Tiles**: each reflectivity-capable site advertises a derived **`CELLS`** parameter layer (`engine_odim::cells::CELLS_PARAMETER`; appended to `SiteMeta.parameters` only — never an EDR quantity or 3D Tiles quantity) rendering footprint outlines + centroid markers (at the cell's max dBZ) + track trails into the `RasterTile` via `ds_core::raster_paint::Canvas` (Bresenham + Liang–Barsky clip, value-space pre-colorize) — vertices projected per-**vertex** via `OutputCrs::world_to_fraction` (the inverse of `project_node`; never per pixel, #203). **Trails** are drawn only for cells present in the *rendered* scan (a trail must terminate at a visible outline — drawing every windowed track painted orphan lines, user-reported) and painted at the reserved `CELLS_TRACK_SENTINEL` value, which the `ds_render::OverlayColorMap` (the layer's style wraps whatever colormap CELLS resolves to) renders as one neutral colour (`CELLS_TRACK_COLOR`, dark grey) so trails are visually distinct from the dBZ-coloured outlines, not blended. Styling: outlines inherit the collection colormap (dBZ) or an explicit `[[wms.parameters]] name = "CELLS"` (the overlay wrap applies on top of either). `time` selects the scan (TIME animation works); `z` is ignored. 3D Tiles `representation=cells` + Features (cell polygons / track LineStrings) are the follow-up PRs of #367.

## OGC 3D Tiles (`api-3dtiles`, epic #346)

Volumetric weather as OGC 3D Tiles — radar polar volumes today. The chain:
`VolumeEngine` (in `ds-core`) returns a `VolumePointCloud` (one point per echo
cell, placed at its true ECEF position via the 4/3-Earth beam model); the
framework-free `ds-3dtiles` crate encodes it to a `.pnts` tile + `tileset.json`;
`api-3dtiles` serves it over HTTP. Engines return the domain type, never bytes
(same rule as `MapEngine`→`ds-render`). `ds-3dtiles` has two encoders: the
`.pnts` point cloud, and (#357) an **isosurface** mesher that turns a `VoxelGrid`
into a glTF `.glb` triangle mesh.

- **Trait:** `VolumeEngine::read_point_cloud(quantity, time, min_value, reference_time)`
  → `VolumePointCloud`; `read_voxel_grid(quantity, time, dims, reference_time)`
  → `VoxelGrid` (a regular **cylindrical** grid — `radius`×`angle`×`height`,
  `NaN`=nodata — the substrate for true voxels #351 and isosurfaces #357);
  and `volume_info() -> Arc<VolumeInfo>` (O(1) cached snapshot, #211 — carries
  quantities, times, default quantity, and a coverage `region` for the tileset
  bounding volume without sampling). Implemented by `PolarVolumeSiteView` (which
  shares `select_entry_and_quantity` across both samplers); the cloud is bounded
  by `MAX_POINTS` (8M) and the grid by `MAX_VOXELS` (32M cells). Both resample
  via the envelope-guarded `sample_polar_slant` (no fabricated data across the
  cone of silence). Unknown quantity ⇒ `InvalidParameter` (→ 400). The
  `EXT_primitive_voxels` glTF *encoding* of a `VoxelGrid` is a follow-up (#351) —
  draft spec, render-verify against CesiumJS ≥1.127.
- **Isosurface meshing (#357, `ds-3dtiles/src/isosurface.rs`):**
  `encode_isosurface_glb(grid, threshold, color)` extracts a constant-value
  shell (e.g. "the 20 dBZ surface") from a `VoxelGrid` as a glTF 2.0 `.glb`
  triangle mesh — a **plain glTF mesh that renders in any 3D Tiles 1.1 client**
  (the verifiable alternative to the draft `EXT_primitive_voxels` voxel path).
  Uses **marching tetrahedra**, not marching cubes: a tet is K4, so the surface
  crosses exactly `|inside|·|outside|` edges and the topology is correct by
  construction (no 256-case table to mis-transcribe — chosen because the output
  isn't render-checkable at encode time). Cube → 6 tets (Kuhn split); any tet
  touching a `NaN` corner is skipped (no surface across the cone of silence).
  Surface vertices map fractional cell index → ground/azimuth/height (same
  cell-centre convention as the engine sampler) → `destination_point` +
  `geodetic_to_ecef` (both now in `ds_core::geo`), stored antenna-relative and
  pre-flipped Z-up→Y-up because a runtime re-applies Y-up→Z-up to **glTF**
  content (the flip the `.pnts` path skips — pnts isn't glTF). **Sealing
  (load-bearing for radar):** `encode_isosurface_glb(grid, threshold, color,
  background)` with `background=Some(bg<threshold)` treats every `NaN` corner as
  no-echo, so the surface **closes into solid blobs** — the **default** the API +
  demo use (`Some(-32.0)`), because leaving boundaries open renders as vertical
  *curtains*. `None` skips `NaN`-touching tets (open surface). **#360 layer
  (foundation, currently invisible under the sealed default):** the engine *does*
  distinguish clear air (`undetect`) from genuinely-unmeasured (`nodata`/cone of
  silence) — `RawPixels::sample_class` (reader.rs) → `Value`/`Undetect`/`Masked`,
  and `voxel_grid_from_volume` fills `Undetect` with the finite `NO_ECHO_FLOOR`
  (−32 dBZ) and leaves `Masked` `NaN`. With `Some` both seal alike (solid look);
  with `None` clear air still seals but unmeasured stays open — an opt-in "honest
  boundary" mode, and the substrate true voxels (#351) need. `tileset_json_glb`
  carries the antenna ECEF as the tile **`transform`** (glTF content has no
  embedded origin, unlike `.pnts` `RTC_CENTER`); the geodetic `region` is
  unaffected by it. Demo: `cargo run -p engine-odim --example gen_isosurface`
  (writes `.glb` + `tileset.json` + a token-free CesiumJS viewer; takes a
  comma-separated threshold list, default `20,35,50`). The mesh is
  **render-verified live** (CesiumJS 1.124): the shell sits correctly over the
  antenna, upright (so the Y-up→Z-up flip is right for direct 1.1 `.glb`
  content), and sealing closes the curtains into solid blobs.
  **Nested multi-threshold shells (#363):** `encode_isosurfaces_glb(grid,
  &[IsoShell], background)` meshes several thresholds into ONE `.glb` — one
  primitive + material per shell; alpha < 255 ⇒ `alphaMode: "BLEND"` (opaque
  shells stay OPAQUE to keep depth-writes), so the intense core glows through
  translucent envelopes ("onion-skin"). `nested_shells(thresholds, colormap)`
  is the shared colour/alpha-ramp policy (outer 35% → inner opaque; single
  threshold = opaque, the classic #357 look). Primitives are emitted
  **innermost-first** — glTF has no draw order, but nested shells share a
  bounding-sphere centre so CesiumJS's back-to-front translucency sort ties and
  primitive order breaks the tie (render-verified). The blur runs ONCE (shells
  re-march the same smoothed field); a threshold above all data is **skipped**
  (weak storm still shows its envelope), all-empty ⇒ `Empty`; `background` must
  sit below the *lowest* threshold; the triangle cap bounds the SUM. API:
  `?threshold=20,35,50` (comma list, ≤ 5 values, sorted+deduped into the
  canonical `content.uri`; isosurface-only — a list on `echotop` is a 400).
  **Indexed mesh + smooth normals (#382):** `MeshBuilder` interns vertices by
  exact position bits (marching-tet shared crossings are bit-identical — no
  quantisation) and accumulates area-weighted outward face normals, normalized
  per vertex at encode — kills the flat-shaded "crumpled paper" facets and
  shrinks the `.glb` by the vertex-sharing factor (~2.5×; 3 shells of the fivih
  fixture = 2.7 MB). Keep the `out_ref` outward-orientation logic — it's what
  keeps winding + normals consistent.
- **Echo-top-height (#362, `ds-3dtiles/src/echo_top.rs`):** two encoders off the
  per-`(radius, azimuth)`-column echo top (highest cell ≥ `threshold`,
  crossing-interpolated), each a height-coloured glTF `.glb` (per-vertex
  `COLOR_0`, normalized u8 VEC4; reuses the isosurface's `index_to_gltf_pos`,
  now `pub(crate)`, + `tileset_json_glb`):
  - `encode_echo_top_columns_glb` — **extruded bins**: one solid box per echo
    cell from the **ground** (height 0) up to its echo top, walls + flat
    normals, coloured ground→top. The **preferred look** (the demo uses it): no
    open edges, sits on the ground, the blocky bins read as a 3-D bar field of
    storm depth. ~7 MB.
  - `encode_echo_top_glb` — the thin **draped surface** (a quad only where all
    four corner columns have a top → clear air is a hole), smooth normals.
    ~0.3 MB; the flat 2-D height-field product. Floats at echo-top height with
    no sides — render-verify showed the columns are nicer.
  **Colormap gotcha:** colour by **height** with stops AT height values
  (`LutColorMap::from_stops`) — a *builtin* colormap's stops are in its own units
  (Temperature's are °C), so it collapses to one colour over a 0–15 km range (the
  first cut rendered uniformly red). Demo: `cargo run -p engine-odim --example
  gen_echo_top` (render-verified). API route is a follow-up; the mesher will be
  reused for VIL (#365).
- **Four representations are served from the API.** Three are selected by
  `?representation=points|isosurface|echotop` on `tileset.json`: `points` →
  `.pnts` (region-only tileset, `RTC_CENTER` self-places), `isosurface` /
  `echotop` → `.glb` plus a tile **`transform`** = the antenna ECEF. The two glTF
  products share `content.glb`, disambiguated by `?representation=echotop` in the
  content URI (default = isosurface). The fourth — **`voxels`** (#351) — has its
  own `…/voxel/` sub-path (below), not a `?representation=` variant. Capability +
  the origin/extents all of them need are coupled in one field:
  `VolumeInfo.voxel_grid: Option<VoxelGridCaps { origin, radius_m, height_m }>` —
  `Some` ⇒ the mesh + voxel products are available and the cylinder origin/extents
  are present (tileset built without sampling; "supports but no origin" is
  unrepresentable, never a 500). It drives the collection JSON's `representations`
  array → the viewer's toggle. The isosurface seals (`background=Some(-32)`); echo-top is
  coloured by a height ramp. Both mesh products take a `?resolution=` detail tier
  (`Resolution` enum → voxel-grid dims; `low` `[128,360,48]` ≈2.2 M cells, `med`
  `[256,360,56]` ≈5.2 M *default*, `high` `[512,360,64]` ≈11.8 M / ~12 s first
  compute — ETag caches repeats); the tier is echoed into the `content.uri`. The
  point cloud is native-resolution and ignores it. A too-low threshold or a bad
  resolution token → 400, an empty result → 404.
- **Point sizing + client-side filter:** `encode_pnts` writes a per-point `value`
  (the physical value) into the `.pnts` **batch table** (no `BATCH_ID` ⇒ per-point
  properties; CesiumJS exposes them to the style engine as `${value}`). The viewer
  styles `pointSize` by `${value}` (weak echo → ~1 px, strong → ~16 px) and
  **filters** by it: the `min dBZ` field restyles `show` client-side (instant, no
  re-fetch, as long as it stays ≥ the fetched floor; going lower re-fetches the
  larger set). **Do not add `BATCH_ID`** to make points pickable features — it
  disables per-point `pointSize` in CesiumJS's point-cloud pipeline (verified;
  `pickMetadata` also can't read a `.pnts` batch table in 1.124 — only its
  schema). The value is read off **color + size**, and the collection JSON
  carries a `legend` (the point colormap sampled into `#rrggbb` stops over a dBZ
  range via `legend_stops`) which the viewer renders as a gradient bar for the
  point cloud.
- **Time-dynamic playback (#350, done):** per-timestep tilesets already work via
  `?datetime=<volume time>` on tileset.json/content.* (the engine selects the
  volume nearest the time; `None` ⇒ latest). The collection JSON advertises a
  `times` manifest (`VolumeInfo.times` → RFC 3339 `…Z`, sorted ascending — each
  value round-trips straight back as `?datetime=`). The viewer **preloads one
  hidden tileset per timestamp** (`preloadWhenHidden: true` is load-bearing — a
  hidden tileset otherwise fetches no tiles, so the first reveal would stall) and
  animates by toggling `.show` — scrub/play is a pure visibility flip, **zero
  network per frame** (verified). A play/pause + slider time bar shows only when
  the collection has >1 volume; the point-cloud style (size + client-side filter)
  is applied across all frames. A non-time-dynamic collection is the degenerate
  1-frame case. The number of frames = the engine's retained volumes per site
  (`[odim] max_files`, **default unbounded** — a day ≈ 288 at 5-min cadence — or
  `time_window`); the viewer caps preload at the most-recent `MAX_FRAMES` (48) so
  an unbounded source can't fetch/hold hundreds of tilesets. A true sliding window
  for longer runs is a follow-up; operators should bound the source with
  `max_files`/`time_window`.
- **True cylindrical voxels (#351, Tier B — `ds-3dtiles/voxels.rs`):** a
  `VoxelGrid` → `EXT_primitive_voxels` glTF `.glb` + a
  `3DTILES_content_voxels` / `3DTILES_bounding_volume_cylinder` tileset that
  CesiumJS 1.142 ray-marches as a volume (render-verified). **Draft, CesiumGS-only
  — NOT in the Khronos registry, CesiumJS the sole implementation**, so encode
  against the live `VoxelCylinder3DTiles` **fixtures, not the README** (which is
  stale: cylinder `mode` = `2147483650` (0x80000002 — box 0x80000000, ellipsoid
  0x80000001), *not* the README's `2147483647`). Load-bearing gotchas: **axis
  swap** content `[radius,angle,height]` → glTF `dimensions [radius,height,angle]`,
  data **radius-fastest → height → angle-slowest** (our grid is the transpose);
  `EXT_structural_metadata` (schema + `propertyAttributes`) at the glTF **top
  level**; embedded BIN buffer; **implicit OCTREE tiling + a constant `.subtree`**
  is required; and the tile **`transform` must be the real ENU→ECEF frame**
  (east/north/up), *not* identity-rotation — identity works for the mesh products
  (absolute-ECEF vertices) but tilts the *parametric* cylinder by the latitude.
  **Azimuth convention remap (load-bearing — else the volume is rotated 90° AND
  mirrored vs the point/mesh products):** the grid angle axis is radar azimuth
  (index 0 → bearing 0, **clockwise from North**), but CesiumJS's cylinder
  (`VoxelCylinderShape`, verified in the 1.142 source) uses default angle bounds
  **-π..+π**, angle-index 0 → -π, angle **counter-clockwise from local +X**
  (=East via the ENU transform). The encoder remaps each output angle slot `s`
  to the radar bin at compass bearing **`270° - φ`** where `φ = -π + (s+0.5)/nA·2π`
  (`grid_azimuth_index`); the mesh/point products skip this (they map azimuth →
  ECEF directly). **The `+180°` (270 not the 90 the stated convention implies) is
  render-verified, not derived:** CesiumJS's effective cylinder-angle origin sits
  opposite (-X) to where the -π bound + +X=East transform predict, so the naive
  `90° - φ` leaves the volume 180°-rotated. ALWAYS render-verify a voxel-cylinder
  azimuth mapping against the point cloud — the handedness/offset can't be trusted
  from the spec alone. **Unmeasured (`NaN`) cells → the no-echo floor (-32 dBZ), NO
  `noData`** — an extreme sentinel trilinearly-interpolates into hard
  walls/floors at the data boundary; the floor (faded out by the transfer
  function) keeps boundaries soft, at the cost of a dense volume (no empty-space
  skipping → slower ray-march). The cellular radar field is further smoothed by a
  **multi-pass separable blur** (`smooth_grid`, 4 passes) so the cell lattice
  doesn't show at close zoom. Cylinder extents come from
  `VoxelGridCaps.radius_m`/`height_m` (= the grid's exact extents, O(1)); the
  bounding cylinder is lifted by `height/2` so data sits 0..H above the antenna.
  Served under its own sub-path so the implicit-tiling URIs resolve relatively;
  the viewer renders it via `Cesium3DTilesVoxelProvider` + a `VoxelPrimitive`
  with a reflectivity transfer-function `CustomShader` (`fsInput.metadata.<q>`).
  v1 MVP: single tile, latest time (no octree/mosaic/animation — follow-ups).
- **Routes** (mounted at `/3dtiles`): `GET /collections/{id}/tileset.json`
  (`?representation=&quantity=&datetime=&min_value=&threshold=&resolution=`),
  `GET /collections/{id}/content.pnts` (`?quantity=&datetime=&min_value=`),
  `GET /collections/{id}/content.glb` (`?representation=&quantity=&datetime=&threshold=&resolution=`),
  the voxel trio `GET /collections/{id}/voxel/{tileset.json,subtrees/*,content/*}`
  (`?quantity=&datetime=&resolution=`),
  plus `/` · `/collections` · `/collections/{id}` · **`/viewer`** (a bundled
  CesiumJS page with collection + quantity + **representation** + **resolution**
  pickers and a **time scrubber** (play/pause + slider) for multi-volume
  collections, `include_str!`-baked from `crates/api-3dtiles/viewer/index.html`;
  same-origin API base by default, with a `?base=` override). The tileset's
  `content.uri` embeds the resolved quantity (+ pinned time +
  `min_value`/`threshold` + `representation` + `resolution`) so the content fetch
  is deterministic.
- **Concurrency:** `read_point_cloud` / `read_voxel_grid` are sync (blocking
  HDF5 I/O + a long CPU loop — marching tet for the latter), so both content
  handlers bound them with the shared render semaphore and run via
  `spawn_blocking` — never inline on a request worker (the same pattern the
  raster APIs use for `get_raster_tile`).
- **Caching (two layers, hot-path audit 2026-06):** (1) `api-3dtiles/src/cache.rs`
  — a process-global byte-bounded LRU of the **encoded content bytes + ETag**
  (`.pnts`/`.glb`/voxel `.glb`), keyed by (collection, product, quantity,
  datetime, params, dims) **plus a data-version hashed from `VolumeInfo.times`**
  (new volume ⇒ new version, so "latest" and nearest-time selection invalidate
  without duplicating engine selection logic), with per-key **single-flight**
  coalescing (concurrent identical requests share one compute; the semaphore is
  only acquired by the computing request). `MC_3DTILES_CONTENT_CACHE_MB`
  (default 512, 0 disables). (2) engine-side `VOXEL_GRID_CACHE` in
  `engine-odim` — `read_voxel_grid` returns `Arc<VoxelGrid>` from a global LRU
  keyed (file, quantity, dims), so isosurface/echo-top/voxels and threshold
  changes share one polar resample (`MC_PVOL_VOXEL_GRID_CACHE_MB`, default 512);
  the resample itself resolves each `(radius, height)` column once
  (`ColumnTarget`) instead of per-cell sweep/moment/pixel-cache lookups.
  **`Cache-Control`:** content/tilesets whose `?datetime=` **exactly matches an
  advertised volume time** get `max-age=86400, immutable` (the viewer pins every
  animation frame from the `times` manifest, so reloads re-use the browser
  cache); a between-volumes datetime (nearest-selection can change) and "latest"
  keep `max-age=60`. A 304 revalidation costs two cache lookups, not a recompute. Cache metrics are in `/metrics`
  (`tiles3d_content_cache_*`, `pvol_voxel_grid_cache_*`). `content_uri` is
  validated (no `..`/absolute/scheme) in `ds-3dtiles`.
- **Config:** add `"3dtiles"` to a collection's `apis` (only `odim-volume`
  supports it today). v1 uses one shared reflectivity colormap; per-collection /
  per-quantity colormaps and true cylindrical voxels (#351) are follow-ups
  (time-dynamic tilesets #350 are done — see above). **Encoder/CesiumJS gotchas**
  (load-bearing) live
  in `ds-3dtiles`: tileset `geometricError > 0`, ECEF-native `.pnts` POSITION,
  `.pnts` not glb-with-POINTS.

## GeoTIFF Engine Notes

- **Must be tiled COG.** Strip-based TIFFs are rejected. One parameter (band) per collection.
- **CRS**: WGS84, TM, LAEA, LCC, Stereographic supported. CRS math in `ds-core/src/geo.rs`.
- **Reprojection**: `bbox_to_pixels()` samples 20 points per edge to capture projection curvature.
- **Data sources**: local directory (`data_path`), S3 (`endpoint` + `bucket` + `prefix_pattern`), or STAC (`stac_url` + `stac_asset_allowlist`). Mutually exclusive.
- **STAC security**: `stac_asset_allowlist` is mandatory (SSRF protection). HTTP redirects disabled. Pagination origin-checked.
- **Tile cache**: compressed bytes in LRU cache (default 256 MB), **remote sources only** — for local files the compressed bytes are already free from the mmap/page cache. Rendered image cache (default 512 MB) shared across WMS/Maps/Tiles.
- **Decoded-chunk cache (#463)**: process-global byte-bounded LRU of *decoded* native source tiles for **local** files (`MC_GEOTIFF_DECODED_CHUNK_CACHE_MB`, default 512, `0` disables). The WMS meta-tile loop renders one viewport as ~50–190 independent `get_raster_tile` calls whose covering source tiles overlap, so without the memo each source tile is LZW/DEFLATE-decoded ~6× per frame. Keyed `(path, mtime, size, inode, ifd, chunk)` with the identity captured from the mmap'd file handle — inode included for the same #253 reason as the catalog's unchanged-file test (mtime+size alone miss a same-size same-second atomic rename) — so a replacement can't serve stale pixels. Band extraction + nodata + scale/offset are applied at copy time for the intersecting window only (partial #206).

## GRIB Engine Notes

- Discovers data via index sidecar files on S3/HTTP **or a local directory**, fetches messages via byte-range reads.
- **Data source (mutually exclusive):** remote `endpoint`+`bucket`+`prefix_pattern` (S3, with strftime/run-hour date templating), or local `data_path` (a directory of `.grib2` + index sidecars; also accepts an `s3://`/`http(s)://` fixed-prefix URL). For `data_path`, `prefix_pattern` is optional and used as a literal sub-prefix (no date templating) since local fixtures are static — index/data files must share a basename (`X.index` ↔ `X.grib2`). Config load enforces the mutual exclusivity.
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
- **Model runs (#337):** polls the directory and retains the most recent `max_runs`
  `.sqd` files as model runs, keyed by each file's **origin (analysis) time**
  (`RunSet: BTreeMap<DateTime<Utc>, _>`), atomically swapped via `ArcSwap`.
  Already-loaded files are reused on poll (not re-parsed). Each run is an EDR
  instance / `RasterInfo.reference_times` entry; the latest run is the default for
  un-pinned queries. See the "Model runs" section above and [[project_querydata_engine_plan]].
- Supports WGS84, Stereographic, and Rotated Lat-Lon grids.
- EDR position queries use bilinear interpolation. Map rendering uses nearest-neighbor.
- Missing value sentinel: 32700.0.
- Config: `wms_parameter` (name/short name/ID), `poll_interval_secs` (default 30),
  `max_runs` (default 4; recent runs retained — set 1 for latest-only/no history).

## Zarr Engine Notes

Cloud-native multidimensional arrays (Zarr V2/V3) with CF-conventions metadata,
tracked in #125. **The crate is phased; Phases 1-3 ship today.**

- **Phases 1-3 (done):** local **and** remote (S3/HTTP) stores, WGS84/geographic
  lat-lon grid, multi-variable EDR **position** queries with bilinear
  interpolation, **WMS/Maps/Tiles rendering** (one layer per variable), CF time
  decoding, CF packing (`scale_factor`/`add_offset`/`_FillValue`/`missing_value`
  + the array's own Zarr fill), byte-range chunk reads with an LRU cache, and a
  startup WARN for pathological chunk shapes. **Not yet:** per-item-CRS STAC
  mode (Phase 4), kerchunk (Phase 5).
- **Rendering (Phase 3):** `MapEngine::get_raster_tile` reads a 2-D spatial
  *window* of the variable covering the request bbox (`Catalog::read_window`,
  +1 cell margin) into memory, then samples per output pixel — per-pixel for
  `Wgs84`/`WebMercator` (cheap `project_node`), via a coarse `ProjectionGrid`
  for `Projected` output (never project per pixel; #203). `raster_info()` is
  served from a cached `ArcSwap<RasterInfo>` rebuilt on each catalog swap
  (#211). Window sampling uses `cf::locate`, so it handles ascending/descending
  and irregular axes; the window read inherits `concurrent_target(1)`.
- **The Zarr format + codec pipeline is handled by the `zarrs` crate** (features:
  blosc/zstd/gzip/crc32c/sharding/transpose + `filesystem`+`ndarray`). Codecs are
  transparent to this engine — it only adds CF semantics, the OGC domain mapping,
  the storage bridge, and the poll-and-swap lifecycle. blosc/zstd build from C via
  `cmake`+`cc` (same toolchain `libaec-sys` already needs for GRIB).
- **Storage = `ds-storage` for every backend.** `engine-zarr/src/store.rs`
  `DsStore` implements zarrs `ReadableStorageTraits` + `ListableStorageTraits`
  over `ds_storage::DataStore` (local / S3 / HTTP via `build_store` /
  `build_s3_store_from_parts`), with a `quick_cache` LRU of full chunk-object
  bytes (byte ranges are served by slicing the cached buffer). Group/child
  discovery uses one-level delimiter listing (`DataStore::list_dir`), not a
  recursive chunk-key walk.
- **`concurrent_target(1)` is load-bearing, not tuning.** `zarrs` parallelises
  multi-chunk retrieval with **rayon by default**, and would call the storage
  layer from rayon workers — where `ds-storage`'s `block_in_place` bridge
  *panics*. `catalog::single_threaded_opts()` pins retrieval to the calling
  (request/poll) thread via `CodecOptions::with_concurrent_target(1)`, so storage
  reads never land on rayon. Every `retrieve_*` MUST go through it.
- **APIs:** the `engine_type → supported_apis` allowlist in `admin.rs` lists
  `"zarr" => &["edr", "wms", "maps", "tiles"]`. WMS/Maps/Tiles need a `[wms]`
  colormap (or a `style_bundle`) like the other raster engines; each variable
  becomes its own layer via `register_parameter_layer_styles`.
- **Catalog model:** `catalog::build` opens the root group, lists child arrays,
  treats 1-D arrays named after their dim as CF coordinate variables, classifies
  each dim via `cf::classify_axis` (coord-var `standard_name`/`units` first, name
  heuristic second — projected metre axes resolve to `Other` so they're not
  mistaken for degrees), validates lat/lon axes are monotonic, and exposes the
  remaining geographic data variables as parameters. A **time axis is required**
  (PointSeries needs a `t`). Variables with an unsupported dtype are skipped at
  build with a WARN.
- **Forecast (reference + lead) handling:** when a store has a CF
  `forecast_reference_time` axis (model run) **and** a `forecast_period`/lead
  axis (e.g. dynamical.org AIFS/GFS/ICON-EU), the engine uses the **latest run**
  and exposes **valid time = run + lead** as the time axis, pinning the run axis
  to the latest index — matching the GRIB "latest run + valid time" convention.
  `cf::parse_duration_seconds` decodes the lead axis (units like `seconds`).
  Model-run *selection* (EDR instances + WMS `reference_time`) is tracked in #337.
- **Reads:** `retrieve_array_subset_opt::<Vec<T>>` (single-threaded) requires the
  exact dtype, so the read path branches on `data_type()` (`*dt ==
  data_type::float32()`, …) and widens every supported int/float to `f64`. Fill
  sentinels are compared against the **raw** (pre-scale) value; NaN/±inf map to
  nodata.
- **Bad-chunking WARN (#125):** `time=1, lat=full, lon=full` (each timestep one
  full-domain chunk) is pathological for point/time-series queries; the engine
  logs it at startup but still serves.
- **Config:** `data_path` (local dir, or an `s3://`/`http(s)://` URL) **or**
  `endpoint`+`bucket`+`path` (S3) — mutually exclusive; optional `path` sub-path,
  `zarr_version` (2/3, advisory — `zarrs` auto-detects), `parameters` filter,
  `poll_interval_secs` (default 300), `cache_mb` (default 256, chunk LRU).
- **Icechunk (feature `icechunk`, #335):** a `[collections.zarr.icechunk]` table
  makes the source a transactional/versioned **Icechunk** repo instead of plain
  Zarr (e.g. the dynamical.org AIFS/GFS/ICON-EU datasets). Off by default
  (`icechunk` + `zarrs_icechunk` + `zarrs/async`); the engine errors clearly if
  the table is set without the feature built. Repo location reuses
  `data_path`/`endpoint`+`bucket`+`path`; the table picks the version (`branch`
  HEAD default `main`, or `tag`, or `snapshot`). Implementation:
  `engine-zarr/src/store.rs` `EngineStore` (a backend-agnostic
  Readable+Listable wrapper so the catalog stays non-generic across the plain
  and Icechunk backends), and `engine-zarr/src/icechunk.rs` opens the repo →
  read-only session → `AsyncIcechunkStore` → `AsyncToSyncStorageAdapter` (its
  `block_on` mirrors ds-storage: `block_in_place` in a runtime, temp runtime in
  tests; safe because retrieval is `concurrent_target(1)`).
  - **S3 backend = icechunk's `object_store` backend, NOT `aws-sdk-s3`.** The
    deps select `icechunk`/`zarrs_icechunk` with `default-features = false,
    features = ["object-store-s3", "object-store-fs"]`, so the build reuses
    the same `object_store` crate `ds-storage` uses instead of pulling the whole
    `aws-sdk-s3` tree — the icechunk feature's binary cost drops ~28 MB → ~8 MB.
    `zarrs_icechunk`'s feature-forwarding fix shipped in 0.5.1, so it's a plain
    crates.io dep now; **`icechunk` still needs one interim `[patch.crates-io]`**
    (see root `Cargo.toml`) pinned to the upstream merge commit of
    earth-mover/icechunk#2190 until a release > 2.0.6 carries it (#340).
    `build_storage` uses `new_s3_object_store_storage`; **anonymous access is set
    via `S3Options::with_anonymous(true)`** (the object_store backend keys
    skip-signing off `S3Options.anonymous`, *not* the `S3Credentials` arg —
    without it, it falls through to the AWS credential chain → EC2 IMDS and
    hangs off-EC2). Public datasets only (#335).
  - Icechunk still owns its own object storage, so this path does **not** go
    through `ds-storage`. New snapshots on a branch are picked up on **reload**,
    not poll (v1). Network-free e2e test generates a local repo: `cargo test -p
    engine-zarr --features icechunk`; live multi-dataset probe (anonymous S3 via
    object_store): `… --test icechunk -- --ignored --nocapture probe_models`.
- **Fixture:** `testdata/zarr-era5-t2m` (committed), regenerated by
  `cargo run -p engine-zarr --example gen_fixture`. Field is linear in lat/lon so
  bilinear is exact and integration assertions are tight.

## CAP Engine Notes (`engine_type = "cap"`, #396)

OASIS **Common Alerting Protocol (CAP) v1.2** emergency/weather alerts. One
`engine-cap` engine implements **both `FeatureEngine`** (one Feature per alert
area) **and `MapEngine`** (severity-shaded polygon fill) over a single
poll-and-swap `Catalog` (`crates/engine-cap`). It is the **first vector→raster
`MapEngine`** — it does not sample a grid; it fills alert **polygons** into the
output pixel grid with the shared `ds_render::rasterize::fill_polygon` primitive
(#397) fed by `ds_core::geo::geometry_to_pixels` (vertices projected via
`OutputCrs::world_to_fraction`, **never per pixel** — #203). `engine-cap` is also
the first engine to depend on `ds-render` (for the fill + `Combine`) — an
**approved exception** for vector→raster `MapEngine` impls (#397); engines still
return `RasterTile` domain types, colorization stays in the API layer.

- **Source = exactly one of `data_path` (local dir of `*.xml`) or `feed_url`
  (Atom/RSS index → linked CAP docs).** Both go through `ds-storage` from the
  background poll runtime only (`feedback_storage_sync_bridge_misuse`); the feed
  fetches the index then the linked docs with `DataStore::get_many` (bounded
  concurrency, per-object timeout, origin-grouped) — never a sequential blocking
  loop. Config (`CapConfig` in `ds-core`) validated at load: `data_path` XOR
  `feed_url`, `feed_url` http(s), non-empty `language`, `poll_interval_secs > 0`,
  positive ISO 8601 `default_ttl`, `circle_segments >= 3`.
- **Feed SSRF guard.** An entry link is fetched only if it shares the feed's
  **exact** origin (scheme+host+port — not a prefix, so `https://feed` rejects
  `https://feed.evil.com`) or matches an explicit `feed_allowlist` URL prefix;
  others are dropped with a WARN. This stops a compromised feed from pivoting the
  server to `http://169.254.169.254/…` or internal hosts — the threat the GeoTIFF
  STAC `stac_asset_allowlist` addresses. Feature `data_version()` (ETag) hashes
  severity + window **and** the text fields (event/headline/description/
  instruction/areaDesc), so an in-place text correction invalidates the ETag (no
  stale 304). **Known limitation:** the allowlist constrains entry *request* URLs,
  not redirect *responses* — `ds-storage`'s HTTP store uses object_store's reqwest
  client with the default follow-redirects policy (no disable knob in
  object_store 0.11), so a compromised DNS/CDN for the trusted `feed_url` host
  could still redirect a fetch internally. Feed mode trusts the feed host; a
  proper redirect-disabling fix belongs in `ds-storage` (hardens every HTTP engine)
  and is a cross-engine follow-up (#431).
- **Coordinate order is the load-bearing gotcha.** CAP polygons/circles are
  `lat,lon` (spec §3.3.4); `ds_core::Geometry` is `[lon, lat]`. `parser.rs`
  **swaps on ingest** (pinned by an absolute-position test — a Helsinki alert
  must land at lon≈25, lat≈60, not the wrong hemisphere). Rings are closed
  defensively; `<circle>` → an N-gon (`circle_segments`, default 64) on the
  geodesic via `destination_point`, carrying `radius_km` as a property.
- **One Feature per `(alert, info, area)`**, id = `{identifier}.{infoIdx}.{areaIdx}`
  (stable, URL-safe). The emitted `Feature.id` is **percent-encoded** to a single
  URL path segment (RFC 3986 pchar; `/`/`[`/`]`/space/non-ASCII escaped) so the
  api-features verbatim self-link href routes — axum's `Path` extractor decodes it
  back on `GET`. **A client building a URL from `Feature.id` must use it as-is,
  not re-percent-encode it**; the raw CAP `<identifier>` is in
  `properties.identifier`. For ordinary URN/dot ids the encoding is a no-op.
  Multiple `<info>` (languages) and multiple `<area>` per info each fan out. `language` config keeps the matching `<info>`s
  (primary-subtag, case-insensitive), falling back to the first info if none
  match. `status_filter` (default `["Actual"]`) drops Test/Exercise/Draft at the
  alert level. **Geocode-only areas** (UGC/EMMA_ID/FIPS, no polygon/circle) get
  geometry from the optional **`geocode_geometry`** lookup — a GeoJSON
  `FeatureCollection` mapping zone codes → polygons (`geocode_property`, default
  `"code"`; `geocode_value_name` restricts which CAP `<geocode>` valueName is
  resolved, e.g. `"EMMA_ID"`). **MeteoAlarm needs this** — its CAP areas are
  geocode-only EMMA_ID zones with no inline polygon, so without the lookup they
  render nothing and contribute no spatial extent. `testdata/cap/emma-fi.geojson`
  is the Finland EMMA zone set (from the MeteoAlarm Python package). An area that
  still resolves to nothing becomes a **`Geometry::Null` Feature** (valid per RFC
  7946 §3.2, listed but never on the map) and is counted (`geocode_only` in the
  load log). The lookup file is loaded once at construction (static reference
  data, not polled); a bad path is a hard `new()` error.
- **Extents.** `spatial_extent()` is the union of resolved geometry bboxes;
  `FeatureEngine::temporal_extent()` (new, default `None`) returns the alert
  windows' `[min start, max end]` (open bounds clamp to `as_of`), so the Features
  collection JSON advertises **both** a spatial and a temporal `extent`
  (api-features feeds them through `ds_core::ogc_extent::build_extent`).
- **Active window** = `[onset∨effective∨sent, expires∨(start+default_ttl)∨open]`.
  Features `datetime=` selects areas whose window overlaps the instant/interval;
  a no-datetime request lists all loaded areas. Map `time`/WMS `TIME` selects
  areas active at that instant; **no time ⇒ active *now*** (the snapshot's
  `as_of`, advanced each poll so expired alerts drop out of the "now" view).
- **WMS `TIME` shape:** the engine advertises `RasterInfo.times` = distinct
  window boundaries **≤ `as_of`** plus `as_of` itself (always the max, capped to
  256). This is load-bearing: the WMS handler resolves a TIME-less GetMap to
  `times.last()`, so making `as_of` the last entry is what makes the default
  render "now". `data_version()` (Feature ETags) hashes record ids+severity+window
  only (not `as_of`), so it stays stable across polls when content is unchanged.
- **Rendering:** single layer per collection, parameter `"severity"`, value =
  CAP severity code (Unknown=0, Minor=1, Moderate=2, Severe=3, Extreme=4).
  Overlapping alerts use `Combine::Max` → highest severity wins, order-independent.
  Style via the **`cap_severity` builtin colormap** (grey→green→yellow→orange→red
  with alpha; codes sit exactly on the 0–4 stops so no inter-code blending);
  set `[wms] colormap = "cap_severity"`. `raster_info()` is O(1) from a prebuilt
  `Arc<RasterInfo>` cached in the catalog snapshot (#211).
- **Lifecycle:** `CapEngine::new` does a best-effort initial load (never fails on
  an empty/unreachable source — starts degraded, the poll loop fills in), wired in
  `server/src/admin.rs` (`"cap" => ["features","wms","maps","tiles"]`), poll loop
  on `poll_runtime()` with `shutdown()` on reload. Demo: `collections.d/cap-alerts.toml`
  over `testdata/cap/` (synthetic fixtures). **Out of scope for v1 (follow-ups):**
  reference-chain supersedes/cancel beyond latest-wins, XML-DSig verification,
  per-`event` sub-layers, conditional-GET feed caching, antimeridian splitting
  (inherited from `geometry_to_pixels`).

## PostGIS Engine Notes

- Prerequisites: PostgreSQL ≥ 13 + PostGIS ≥ 3.0. TimescaleDB is a supported *deployment* choice (hypertables plan well) but the engine never branches on it.
- Three schema shapes selected by `observations.shape`: `long` (EAV), `wide` (column-per-parameter), `per_parameter` (table-per-parameter, one fan-out query per param).
- **The `[postgis.stations]` table is optional (#433).** Locations can instead be derived from the observations table's own geometry (`observations.geom_col`, e.g. `the_geom`; per_parameter inherits it or overrides per table). The mode is derived, not flagged:
  - stations present, no obs geom → **stations-only** (the original behavior): the whole registry is advertised regardless of whether it has data.
  - stations present + obs geom → **orphan fallback (mode B)**: **membership = the windowed obs reporters** (same set as mode A); the stations table only supplies *metadata* (label/properties/authoritative geometry) for the registered subset. Reporters with no stations row are bare orphans (`label = id`, empty properties); a **registered-but-silent station is NOT advertised** (every listed location has data within the window — use stations-only mode to advertise the full registry instead). #439.
  - no stations + obs geom → **observations-only (mode A)**: every location derived from the obs table (windowed reporters, bare).
  - no stations + no obs geom → hard config error (nothing can be placed).
  Orphan/observation-derived locations use **`SELECT DISTINCT ON (station_fk)`** — for `per_parameter`, **one query per table** (NOT a single `UNION`), deduped by id in Rust, each capped at `MAX_LOCATIONS`. One-query-per-table is load-bearing: a `UNION` of N multi-second per-table scans runs as one statement and blows a read-only role's `statement_timeout` (nexus `meteocore_ro` = 5 s killed the 6-table union at ~12 s; #435). On TimescaleDB the unique `(station_fk, time)` index drives a SkipScan, but even one table's full-history `DISTINCT ON` can exceed a 5 s cap (the largest nexus table, ~13 M rows, did). So the derivation is **time-windowed by default**: `observations.locations_window` (ISO 8601 duration, **default 24 h**; `"all"` = full history) adds `AND time_col >= now() - window`, restricting the scan to recent hypertable chunks — ~0.1 s instead of >5 s, and it advertises only **currently-reporting** stations (more correct for live obs). A climate-style collection sets `locations_window = "all"` (and then needs a role `statement_timeout` big enough to scan all history, or a pre-materialized locations table). In modes A/B **position is nearest-by-haversine and area is a bbox test, both computed in-memory** from the cached location set (a polygon area returns its bounding-box superset, not exact `ST_Within`) — far cheaper than a spatial query over the obs hypertable; stations-only mode keeps the live `ST_DWithin`/`ST_Within` SQL path. The **per-request observation fetch (`WHERE station_fk = $1`) is identical in every mode**, so request latency is unchanged; only the background metadata refresh pays the distinct-locations cost. Scaling story for very large tables: pre-materialize a distinct `(id, geom)` view and wire it as the `stations` block to fall back to the fast path, and/or raise `metadata_refresh_secs`.
- **DSN via env var only.** `[postgis].dsn_env` names an env var; a literal `postgres://` URL in TOML is rejected at load unless `MC_ALLOW_INLINE_DB_URL=1`.
- **TLS is deferred to #110.** v1 passes `NoTls`; `sslmode=` in the DSN is parsed but not applied. A startup WARN fires when a non-loopback DSN lacks `sslmode=require`. Until #110 lands, reach the DB over private network/VPN/loopback.
- **Security layers:** every identifier goes through `ds_core::config::is_valid_sql_identifier` at load + `security::quote_ident` at emit; every value is a `$N` bind. `stations.where_clause` is config-time only (no HTTP input reaches it) and validated against a blocklist (DML/DDL verbs + `UNION`/`EXECUTE`/`CALL`/`PERFORM`, `;`, comments) — if you need richer filtering, create a SQL VIEW.
- **Per-URL pool** shared across collections on the same `(host, port, db, user, sslmode)` tuple. First-caller-wins on size; `HARD_POOL_CAP = 32`. Per-load only (no reuse across reloads in v1).
- **Session limits come from the role, not the engine.** `statement_timeout`, `lock_timeout`, and `default_transaction_read_only` are set via `ALTER ROLE meteocore_ro SET ...` (see crate README). The engine uses `RecyclingMethod::Fast` and does not issue `SET` on checkout, so a superuser DSN or an unconfigured role bypasses those limits entirely. The role-setup SQL is operational-mandatory, not optional.
- **Live health monitoring (#110, done).** `PostgisEngine::poll_loop` runs a `SELECT 1` ping every 30 s (2 s deadline) that flips the collection `ready`↔`degraded` on DB reachability; `/health` reflects it live (the handler overrides the boot snapshot with `engine.health_status()` for postgis collections; `failed` — couldn't construct — has no engine and keeps its boot status). The ping is the `/health` authority — a *metadata-refresh* failure keeps the last good snapshot and does NOT flip health (it's observable via metrics instead). The ping uses a **dedicated** connection (`tokio_postgres::connect`), not the shared pool, so a busy pool can't masquerade as DB-unreachable. `/metrics` adds gauges `postgis_up{collection}`, `postgis_pool_{size,max_size,available,waiting}{pool_key}` (`size` = open connections, `max_size` = capacity, `available` = acquirable-now), `postgis_metadata_refresh_seconds{collection}` (last duration) — set live each scrape — plus real counters `postgis_{metadata_refreshes,metadata_refresh_failures,pings,ping_failures}_total{collection}` (process-global, rebaseline-on-reset delta-tracking in `metrics_handler` since engines reset on reload — so `rate()` works). The per-query histograms (`postgis_query_duration_seconds`/`rows_returned`/`query_errors_total`) are deferred — they need api-layer plumbing (the API calls engines generically via the trait) — see the #110 follow-up.
- **Metadata cache** (`ArcSwap<CollectionMeta>`) holds station list, parameter descriptors, temporal extent, spatial bbox. Synchronous bootstrap at construction, then **`PostgisEngine::poll_loop` refreshes it every `metadata_refresh_secs` (default 300 s)** on the background poll runtime (spawned in `main.rs` at boot + `admin.rs` on reload; `shutdown()` on reload). This keeps the location list + extents + the `locations_window` "currently reporting" set current without a manual reload — load-bearing for mode B, whose membership is time-sensitive. A failed refresh logs a WARN and keeps the previous snapshot (`MetadataCache::refresh` only swaps on success), so a transient DB blip never empties the cache.
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
# base_url = "https://api.example.com"  # optional, static fallback for absolute links behind a proxy
# trust_proxy_headers = true            # optional, derive per-request link base from proxy headers (default false)
# collections_dir = "collections.d"     # optional, directory of per-collection .toml files
# watch_collections_dir = true          # optional, auto-reload on collections_dir changes (default false)
# watch_debounce_ms = 500               # optional, coalesce-window for the watcher (default 500)

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
# Optional discovery metadata, surfaced across all APIs (see below).
keywords = ["weather", "observations", "Finland"]
[collections.license]
title = "CC-BY-4.0"   # required; an SPDX id (no spaces). With no `url` below,
                      # the link auto-resolves to
                      # https://spdx.org/licenses/CC-BY-4.0.html
# url = "https://creativecommons.org/licenses/by/4.0/"   # optional explicit override

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

### Reverse-proxy base URL (`trust_proxy_headers`, #12)

Absolute self-links (landing pages, `collections`, GeoJSON `links`, WMS
`GetCapabilities` OnlineResource/legend URLs, 3D Tiles docs, …) are built from a
base URL. By default that base is resolved **once at startup** —
`BASE_URL` env > `[server] base_url` > `http://{host}:{port}` — and is wrong when
the server sits behind a reverse proxy (`http://0.0.0.0:8000/edr/...`).

With `[server] trust_proxy_headers = true`, the base URL is instead resolved
**per request** from the standard forwarding headers, with this precedence:

1. RFC 7239 `Forwarded` (`proto=`/`host=` of the **last** element — the closest
   trusted proxy; the first element is the client-injectable oldest hop),
2. `X-Forwarded-Proto` + `X-Forwarded-Host` (+ optional `X-Forwarded-Port`),
3. the static fallback above.

The pure resolver is `ds_core::proxy::resolve_base_url` (framework-free — the
axum handlers pass header values in via a closure, so ds-core keeps its no-HTTP
rule); each API handler calls it through a small per-crate `request_base_url`
helper. **Security:** the flag is `false` by default because forwarding headers
are client-controllable; even when enabled, host values are sanitised
(whitespace/slashes/`@`/non-ASCII rejected), the scheme is restricted to
`http`/`https`, and only the first value of a comma list is used — a malformed
header falls through to the next source rather than producing a spoofed URL.
Enable it only when a trusted proxy sets/overwrites these headers. **Ensure the
proxy strips or overwrites `Forwarded`/`X-Forwarded-*` on untrusted client
requests, and that clients cannot reach the backend directly** — otherwise a
client bypassing the proxy could spoof these headers and control the emitted
self-links (an open-redirect risk for downstream consumers of those links). The
`Forwarded` parser uses the **last** element specifically because a client can
pre-inject the first one (RFC 7239 §4 append semantics).

### Collection Keywords & License

Two optional per-collection discovery fields live directly on the collection (any
engine), surfaced across **every** API from the shared `CollectionConfig`:

- **`keywords`** — a flat array of non-empty strings. Emitted as the OGC API –
  Common – Part 2 `"keywords"` array in the EDR/Features/Maps/Tiles collection
  JSON, as a `<KeywordList>` in WMS `GetCapabilities` (on the collection's layer,
  in WMS 1.3.0 schema order after `<Abstract>`), as chips on the HTML collection
  pages, and matched by `/collections?q=` (whole-word or phrase, alongside title
  and description).
- **`[collections.license]`** — `title` (required; a human name or SPDX id) plus
  an optional `url`. When `url` is omitted and `title` is a plausible SPDX id, the
  URL is synthesized as `https://spdx.org/licenses/<id>.html`. Rendered as a
  `rel="license"` link in the JSON APIs, a `<Attribution>` element in WMS (after
  the `<Dimension>` elements), and a link on the HTML pages. A license with no
  resolvable URL (free-text title, no `url`) still shows its name in WMS/HTML but
  produces no JSON link (a link object requires an `href`).

Both are validated at config load: empty keyword entries, an empty license
`title`, and a non-`http(s)` license `url` are rejected.

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
- Hot-reload (`POST /admin/collections/reload`) picks up added, removed, and changed files automatically. With `[server] watch_collections_dir = true` (default off), a filesystem watcher (`notify`) triggers the **same** reload automatically on add/edit/remove — debounced (`watch_debounce_ms`, default 500), running on the background `poll_runtime`, and keeping the live registry if the new config is invalid/empty. The shared core is `admin::do_reload`.
  - **Trust model:** the watcher's reload is authorized by *filesystem write access to `collections_dir`*, NOT the HTTP `ADMIN_TOKEN` that gates `POST /admin/collections/reload` — they are different control planes (local FS vs network). Anyone who can write a collection file already controls what the server serves. When the watcher is enabled and `ADMIN_TOKEN` is set, a startup WARN makes the asymmetry explicit; keep `collections_dir` writable only by trusted principals (avoid shared/NFS mounts for it).
- A single invalid file rejects the entire config (no partial loads).
- `[[style_bundles]]` blocks are NOT allowed in per-collection files — only in `config.toml`. Referencing a bundle from a per-collection `[wms]` is fine; defining one here is rejected with an explicit error.
- A `style_bundle` cannot coexist with `[[wms.parameters]]` on the same collection — inline per-parameter *defaults* are rejected at config load when a bundle is attached.
- Inside a bundle, each `[[style_bundles.extras]]` entry may carry an optional `parameter` field. Extras with a `parameter` are scoped to that layer only (e.g. `parameter = "wind_speed"` surfaces only under `collection/wind_speed`); untagged extras are shared across every parameter layer.

## Admin & Operations

- **Reload**: `POST /admin/collections/reload` — re-reads config, atomically swaps engines. Shared core `admin::do_reload` is also driven by the optional `collections_dir` watcher (`[server] watch_collections_dir`).
- **Health**: `GET /health` — per-collection status (ready/degraded/failed). HTTP 503 only when all failed.
- **Metrics**: `GET /metrics` — Prometheus format. Path labels use route patterns (not raw URLs) to avoid cardinality explosion.
- **Grafana dashboards live in this repo**: `docker/grafana/dashboards/meteocore-overview.json`, provisioned into the `meteocore-grafana` container (nexus) via `docker/grafana/provisioning/` — the JSON edit here is the whole change. **When adding a new `/metrics` family, update the dashboard too** (same PR or a follow-up issue like #469); cache families follow the established panel pattern: hit ratio, bytes vs capacity, miss rate.
- **State**: API state wrapped in `ArcSwap` for lock-free reads. Render semaphore (2× CPU cores, min 8) shared across Maps/Tiles/WMS. Engine loading in `server/src/admin.rs`.

## Code Style

- Use `thiserror` for error types, not manual `impl Display`
- Prefer returning `Result<T, DataServerError>` from engine methods
- Keep handlers thin — delegate logic to the engine, map errors to HTTP status codes
- Use `serde_json::json!` macro for building JSON responses
- Do not leak internal error details to clients — use generic messages for 500 errors
