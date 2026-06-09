# MeteoCore — Claude Instructions

## What This Is

Rust workspace implementing OGC API - EDR, OGC API - Features, OGC API - Maps, OGC API - Tiles, OGC WMS 1.3.0, and OGC 3D Tiles servers. Twenty crates: `ds-core` (traits + types + shared utilities), `ds-storage` (S3/HTTP/local object store), `ds-render` (raster colorization + PNG encoding), `ds-mvt` (Mapbox Vector Tile encoder + LRU tile cache), `ds-3dtiles` (OGC 3D Tiles `.pnts`/`tileset.json` encoder), `engine-csv` (CSV data engine), `engine-geojson` (GeoJSON data engine), `engine-geotiff` (GeoTIFF/COG data engine), `engine-grib` (GRIB2 NWP data engine), `engine-odim` (ODIM_H5 weather-radar engine), `engine-querydata` (FMI QueryData data engine), `engine-zarr` (Zarr V2/V3 multidimensional-array engine), `engine-postgis` (PostGIS/TimescaleDB observation data engine), `api-edr` (EDR HTTP layer), `api-features` (Features HTTP layer), `api-maps` (OGC API Maps HTTP layer), `api-tiles` (OGC API Tiles HTTP layer), `api-wms` (WMS 1.3.0 HTTP layer), `api-3dtiles` (OGC 3D Tiles HTTP layer), `server` (binary).

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
| CSV | `EdrEngine` + `FeatureEngine` | EDR (locations only), Features |
| GeoJSON | `FeatureEngine` | Features |
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
- **Auto-split happens in `server/src/admin.rs`** (`load_collections`, the `"odim-volume"` arm): build the engine once, enumerate `engine.sites()` (returns `(nod, label)` per site), and register one `PolarVolumeSiteView` per site (cloning the base `CollectionConfig` with a per-site id/title). Site discovery is a scan snapshot — sites added later surface on the next `POST /admin/collections/reload`.
- **Cross-sections.** `query_trajectory` returns a CoverageJSON `Section` (composite `[t,x,y]` axis + numeric `z` = height above antenna, via the 4/3-Earth beam model). `z` selects the elevation-angle band. Vertical axis is elevation angle (`VerticalKind::ElevationAngle`).

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
  (writes `.glb` + `tileset.json` + a token-free CesiumJS viewer). The mesh is
  **render-verified live** (CesiumJS 1.124): the shell sits correctly over the
  antenna, upright (so the Y-up→Z-up flip is right for direct 1.1 `.glb`
  content), and sealing closes the curtains into solid blobs.
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
  level**; embedded BIN buffer; `NaN` → a declared `noData` sentinel; **implicit
  OCTREE tiling + a constant `.subtree`** is required; and the tile **`transform`
  must be the real ENU→ECEF frame** (east/north/up), *not* identity-rotation —
  identity works for the mesh products (absolute-ECEF vertices) but tilts the
  *parametric* cylinder by the latitude. Cylinder extents come from
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
- **Caching:** content-derived ETag on both `.pnts` and `.glb` (deterministic
  bytes ⇒ cheap 304s, shared `binary_response` helper); `content_uri` is
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
- **Tile cache**: compressed bytes in LRU cache (default 256 MB). Rendered image cache (default 512 MB) shared across WMS/Maps/Tiles.

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
- **State**: API state wrapped in `ArcSwap` for lock-free reads. Render semaphore (2× CPU cores, min 8) shared across Maps/Tiles/WMS. Engine loading in `server/src/admin.rs`.

## Code Style

- Use `thiserror` for error types, not manual `impl Display`
- Prefer returning `Result<T, DataServerError>` from engine methods
- Keep handlers thin — delegate logic to the engine, map errors to HTTP status codes
- Use `serde_json::json!` macro for building JSON responses
- Do not leak internal error details to clients — use generic messages for 500 errors
