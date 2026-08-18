# MeteoCore — Claude Instructions

MeteoCore is a Rust workspace implementing OGC API - EDR, OGC API - Features,
OGC API - Maps, OGC API - Tiles, OGC WMS 1.3.0, and OGC 3D Tiles servers for
weather data (radar, NWP models, observations, alerts).

Crates: `ds-core` (traits + types + shared utilities, directory `crates/core`),
`ds-storage` (S3/HTTP/local object store, directory `crates/storage`),
`ds-render` (raster colorization + PNG encoding, directory `crates/render`),
`ds-cache` (shared byte-bounded LRU cache plumbing),
`ds-poll` (shared poll-loop lifecycle: the `Shutdown` handle + `PollTicker`
every engine's background poll loop uses — never hand-roll a
`tokio::select!` shutdown signal, #481),
`ds-mvt` (Mapbox Vector Tile encoder + LRU tile cache), `ds-3dtiles`
(OGC 3D Tiles encoder), engines (`engine-csv`, `engine-geojson`,
`engine-geotiff`, `engine-grib`, `engine-odim`, `engine-querydata`,
`engine-zarr`, `engine-postgis`, `engine-cap`), API layers (`api-edr`,
`api-features`, `api-maps`, `api-tiles`, `api-wms`, `api-3dtiles`), and
`server` (the binary).

**Per-crate notes live in `crates/<crate>/CLAUDE.md`.** Before working on one
of these crates, read its file — it holds that crate's rules and gotchas:

- `crates/server/CLAUDE.md` — CLI flags, no-config boot, auto-collections,
  reload/watcher trust model, proxy headers.
- `crates/api-wms/CLAUDE.md` — BBOX axis order, meta-tiling, TIME/ELEVATION/
  reference_time dimensions.
- `crates/api-edr/CLAUDE.md` — CoverageJSON schema compliance, domain types,
  instances.
- `crates/api-3dtiles/CLAUDE.md` — routes, representations, caching, viewer.
- `crates/ds-3dtiles/CLAUDE.md` — .pnts / isosurface / echo-top / voxel
  encoders and their CesiumJS gotchas.
- `crates/engine-odim/CLAUDE.md` — PVOL per-site model, pixel pre-warm,
  resampling, storm cells.
- `crates/engine-geotiff/CLAUDE.md`, `crates/engine-grib/CLAUDE.md`,
  `crates/engine-querydata/CLAUDE.md`, `crates/engine-zarr/CLAUDE.md`,
  `crates/engine-cap/CLAUDE.md`, `crates/engine-postgis/CLAUDE.md`,
  `crates/engine-nowcast/CLAUDE.md` — one file per engine.

This root file holds only workspace-wide rules.

## Critical Rules

Violating these has caused production incidents. Never break them.

1. **Never commit directly to `main`.** Create a branch, open a PR.
2. **Run `cargo fmt` and `cargo clippy --all-targets -- -D warnings` before
   committing.** CI rejects both formatting drift and clippy warnings.
3. **Never build XML with `format!()` or string concatenation** — XML
   injection risk. All XML output uses `quick-xml::Writer` (api-wms).
   `scripts/check_geo_safety.sh` enforces this in CI.
4. **Never re-implement EPSG:3857 ↔ WGS84 math.** Use `ds_core::web_mercator`
   (`lon_to_x` / `x_to_lon` / `lat_to_y` / `y_to_lat`, `EARTH_RADIUS`,
   `LAT_LIMIT_DEG`). Four hand-rolled copies once drifted and displaced data
   ~10° at low zoom (#452, fixed by consolidation in #454).
   `scripts/check_geo_safety.sh` enforces this in CI (magic constants have
   named homes too: `web_mercator::{EARTH_RADIUS, LAT_LIMIT_DEG}`,
   `geo::WGS84_A`).
   - Never clamp latitude when converting a viewport/bbox — zoomed-out
     requests legitimately reach past ±85°, and the client maps the returned
     image over the full requested extent.
   - Clamp to `LAT_LIMIT_DEG` only when selecting tile-grid indices.
5. **Never project per output pixel in a render path.** The CRS forward
   transform dominates render cost; per-pixel projection was a ~10×
   regression (#203). Evaluate the projection on a coarse `ProjectionGrid`
   and interpolate the output→source pixel map, or project per-vertex for
   vector geometry.
6. **Engine poll/scan loops run on the dedicated background runtime**
   (`poll_runtime()` in `server/src/main.rs`), never on the request-serving
   runtime (#221). Do not wrap `ds-storage` calls in `spawn_blocking` — it
   panics (see rule 7).
7. **`ds-storage` (`DataStore`) is a sync bridge over async object_store**
   (`block_in_place(|| handle.block_on(..))`, which is only valid on a
   multi-thread-runtime worker thread). Therefore:
   - Call it from the background poll runtime (or another dedicated runtime).
   - Never from a request-handler task — it parks a request worker.
   - Never inside `spawn_blocking` — it panics.
   - Never from a non-Tokio thread such as a rayon pool — that hits a
     construct-a-new-`Runtime`-per-call fallback (#222). For parallel remote
     fetches, use async concurrency (`join_all`) on the runtime, or pass a
     `Handle` into the worker.
8. **SQL safety:** every identifier goes through `quote_ident`, every value
   is a `$N` bind, no SQL text inside `format!()`.
   `scripts/check_sql_safety.sh` enforces this in CI.
9. **Never loop blocking I/O sequentially on one thread.** N sequential
   blocking network calls multiply the stall (the GRIB new-run probe once did
   32 ≈ seconds). Batch, bound the parallelism, or cap per cycle.
10. **Capability/metadata accessors used per request must be O(1)** from a
    snapshot (`ArcSwap`/`RwLock` read) — never recompute or allocate per call
    (#211).
11. **Do not leak internal error details to clients.** Generic messages for
    500 errors.
12. **After modifying CoverageJSON output (`api-edr/src/response.rs`), run
    `cargo test -p api-edr`** — all CoverageJSON must validate against the
    OGC schema (`schemas/coveragejson.json`).

## Build & Run

```bash
cargo build                  # Build all crates
cargo test                   # Run all tests
cargo run -p server          # Start server (reads config.toml)
cargo check -p <crate>       # Type-check a single crate
```

Server CLI flags, no-config boot, and `--auto-collections` are documented in
`crates/server/CLAUDE.md`.

### Fuzz testing

Fuzz targets live in `fuzz/` (separate from the workspace; `cargo-fuzz` +
libfuzzer; requires nightly):

```bash
cargo install cargo-fuzz                                          # One-time setup
cargo +nightly fuzz run fuzz_tiff_metadata -- -max_total_time=60  # Fuzz TIFF parser
cargo +nightly fuzz run fuzz_geo_transform -- -max_total_time=60  # Fuzz CRS transforms
```

### Dependency security (RustSec)

`cargo audit` is a **CI gate** — the `audit` job in `.github/workflows/ci.yml`
(#584). It runs on PRs, on `main`, and on the weekly `schedule`, so an
advisory published against a crate we already ship turns CI red with no code
change; on `main` it also blocks the Docker publish.

The policy lives in `.cargo/audit.toml` rather than in workflow flags, so a
local run gives the identical verdict:

```bash
cargo install cargo-audit --locked   # one-time
cargo audit                          # exactly what CI enforces
```

- `deny = ["warnings"]` — unmaintained / unsound / yanked crates fail the
  gate too, not just vulnerabilities. Letting those accumulate is how #584
  reached 11 vulnerabilities before anyone looked.
- Fix by bumping: `cargo update` for transitive crates, a manifest bump for
  direct ones. Most of #584 was `cargo update` alone.
- Adding an `ignore` entry is the last resort. Each one needs the reachability
  analysis *and* a tracking issue written in the comment above it (#594).
- CI additionally runs `cargo metadata --locked` — a Cargo.lock that disagrees
  with the manifests would make the audit meaningless, since it reads the lock.
- Dependabot (`.github/dependabot.yml`) proposes weekly bumps for direct
  crates and for GitHub Actions; purely transitive crates are refreshed by
  `cargo update`, prompted by the scheduled audit run.

## Project Tracking

Backlog is in GitHub Issues: https://github.com/mrauhala/meteocore/issues

- Labels: `priority: high|medium|low`; `effort: tiny|small|medium|large`;
  type labels (`bug`, `enhancement`, `security`, `performance`, `reliability`,
  `architecture`, `operational`, `spec-compliance`); `epic` for parent issues
  with task lists.
- Milestones: **v0.2** (radar engines + rendering fixes), **v0.3** (QueryData
  improvements + multi-band GeoTIFF), **v1.0** (spec compliance + production
  hardening).
- When completing work, close the issue: `gh issue close <number>`.

```bash
gh issue list                              # All open issues
gh issue list -l "priority: high"          # High priority only
gh issue list --milestone "v0.2"           # Issues in v0.2 milestone
gh issue create --title "..." --label "bug,priority: high" --milestone "v0.2"
```

## Architecture Rules

- **Four core traits:** `EdrEngine` (EDR), `FeatureEngine` (Features),
  `MapEngine` (Maps/WMS/Tiles), `VolumeEngine` (3D Tiles — volumetric point
  clouds). They are separate traits — not every engine supports every API.
  Engines return domain types, never JSON/XML; serialization belongs in the
  API crates.
- **`ds-core` has no framework dependencies** (only chrono, serde, thiserror,
  toml). Keep it that way. Use the `PropertyValue` enum, not
  `serde_json::Value`, for feature properties. CRS math and `GeoTransform`
  live in `ds_core::geo`, shared by all engines.
- **`ds-render` has no framework dependencies** (no axum/tokio/http —
  encoders and codecs only: `png`, `jpeg-encoder`, `webp`, plus
  `serde_json` for the shared machine-readable legend document builder
  `legend_json`, kept in ds-render so WMS/Maps/Tiles can't drift apart).
  `ds-mvt` and `ds-3dtiles` are likewise framework-free byte encoders,
  mirroring `ds-render`.
- **Byte-bounded LRU caches go through `ds-cache`** (#480):
  `ds_cache::ByteBoundedCache` owns the weigher/env-parse/hit-miss-counter/
  metrics plumbing (including single-flight `get_or_insert_with`); call
  sites keep their key type, weight fn, `MC_*_CACHE_MB` env var name and
  defaults. Don't hand-roll a `quick_cache` byte-weighted cache — before
  extraction the same ~40 lines were copy-pasted 12×. In `server/src/
  admin.rs`, a global cache's `/metrics` family is one `CacheMetricSet`
  static + one `update()` call in `metrics_handler`.
- **API crates depend only on ds-core** (plus ds-render for
  api-wms/api-maps, and api-edr for its `f=png` time-series plots) — never on
  engine crates. API state is a registry of engines keyed by collection ID.
- **EDR, Features, Maps, Tiles, and WMS are separate services** with separate
  base routes (`/edr/...`, `/features/...`, `/maps/...`, `/tiles/...`,
  `/wms/...`).
- **Collection routing is dynamic.** Handlers look up engines from a
  `HashMap<String, Arc<dyn …Engine>>` by collection ID from the URL path.
  Never hardcode collection IDs.
- **The `apis` config field is enforced.** A collection is wired to an API's
  router only if that API is listed in its `apis` array.
- **Tiles reuses `MapEngine`.** Tile z/x/y → bbox via TileMatrixSet math,
  then `MapEngine::get_raster_tile()`. No separate tile engine trait.
- **CORS is applied at the server level** (`CorsLayer` in
  `server/src/main.rs`), not in individual API crates.
- **Crate name gotcha:** the core crate is named `ds-core` in Cargo.toml
  (imported as `ds_core`); its directory is `crates/core/`. It was renamed
  from `core` because shadowing Rust's built-in `core` crate breaks proc
  macros like `#[tokio::main]`.

### Adding a new engine

1. Create `crates/engine-<name>/` with a `Cargo.toml` depending on `ds-core`.
2. Implement `EdrEngine`, `FeatureEngine`, `MapEngine`, and/or `VolumeEngine`.
3. Add the crate to workspace members in root `Cargo.toml`.
4. Add it as a dependency of `crates/server/Cargo.toml`.
5. Add a match arm for the new `engine_type` in `server/src/main.rs` /
   `server/src/admin.rs`, including the `engine_type → supported_apis`
   allowlist.
6. Wire it into the appropriate registries per the collection's `apis`.
7. Obey the Performance & Concurrency rules — especially: spawn the poll loop
   on the background runtime, and never project per output pixel.
   **If the engine snaps a requested time to an available timestep**
   (latest-not-after, nearest, …) instead of exact-matching, it MUST
   override `MapEngine::resolve_time` with the SAME selection logic
   `get_raster_tile` uses (share one helper so they cannot drift) — the API
   layers key the no-TTL rendered/meta-tile caches on it. Skipping this
   reintroduces the #507 cache poisoning (stale animation frames).
   **If the engine retains model runs** (non-empty
   `RasterInfo.reference_times`), it MUST likewise override
   `MapEngine::resolve_reference_time` with the SAME run selection
   `get_raster_tile` uses (`None` ⇒ the concrete run it would render,
   including any cross-run fallback) — the caches key the run axis on it
   (#521). Skipping this freezes the first-rendered run's pixels when a
   newer run re-covers the same valid times.
8. Ship a runnable (enabled) example collection config AND do an end-to-end
   server + curl smoke test against real data. Unit tests alone miss
   integration and unit-conversion bugs.

### Adding a new API endpoint

Same pattern for api-edr, api-features, api-maps, api-tiles:

1. Add the handler in `handlers.rs`, the route in `lib.rs`, new query params
   in `params.rs`, new response formats in `response.rs`.
2. **Always update `api_definition()` in `handlers.rs`** so the OpenAPI spec
   includes the new path.

## Performance & Concurrency Rules

Hard-won from production incidents (epic #201; spike investigations
#221/#222). Apply them to every engine and rendering feature, not just where
they were found. Critical Rules 5–7, 9 and 10 above are part of this set.

- **All engines share ONE multi-thread request-serving Tokio runtime**
  (worker count ≈ CPU cores). A poll/scan loop doing blocking I/O on it parks
  a worker for the whole operation; when several collections' polls overlap,
  the pool starves and every collection's p99 spikes — multi-second, even at
  low load (#221, #208). That is why poll loops run on `poll_runtime()`.
  A blocking scan may additionally use `tokio::task::spawn_blocking` (the
  ODIM HDF5 scan does) — but never when it calls `ds-storage` (panics;
  Critical Rule 7). The grib/geotiff/querydata poll bodies still do blocking
  I/O directly and are safe only because they run on the background runtime.
- **Background metadata refresh must not contend with request serving.**
  Discovering new data (S3 LIST, STAC HTTP, GRIB index, COG header) is
  background work — keep it off request-serving workers.
- **A cache only helps if its key matches the access pattern.** A rendered
  cache keyed on exact bbox+width+height got ~3% hits for fullscreen
  arbitrary-viewport WMS — pure wasted RAM (#202). Cache at a granularity
  that actually repeats (tile-aligned), or don't allocate the cache.
- **Decode to compact native types, not `Vec<Option<f64>>`** — boxing every
  sample is a 16× memory blowup (#206). `RasterTile.values` is the
  `RasterValues` enum: `F64` (boxed universal form) or
  `U8 { data, nodata, gain, offset }` (raw bytes). Integer render paths
  should produce `U8`, which `ds-render` colorizes through a 256-entry LUT
  indexed by the raw byte (built per call; entry i ≡ `colormap.color(
  value_at(i))`, so the variants are pixel-identical by construction — pinned
  by the `u8_lut_colorize_matches_boxed_f64_exactly` test). GeoTIFF's map
  path produces `U8` for local u8 sources with an integer u8 nodata
  (`reader::read_bbox_u8`, self-gating with `Ok(None)` → boxed fallback);
  every other engine constructs `F64` via `.into()`. Adding a `RasterValues`
  variant causes exhaustive-match compile errors in `colorize`/`value_at` —
  keep them consistent.
- **All raster output→source coordinate mapping goes through
  `OutputCrs`/`ProjectionGrid`** (the `MapEngine::get_raster_tile` path). The
  WMS Web-Mercator meta-tile assembly (`ds-render/src/metatile.rs`) is the
  ONLY place that re-derives the output coordinate map, and it must stay
  consistent with the engine path (#452). When debugging a "data displaced /
  misplaced" report, first confirm which render path the failing request
  uses: **WMS EPSG:3857 goes through meta-tiling, not the direct
  `get_raster_tile` path that Maps/Tiles use.**
- **Before blaming a mechanism for a latency spike, check the magnitude adds
  up.** A 2.3 MB local read from page cache is tens of ms, not seconds.
  Multi-second stalls at low load almost always mean blocking/contention
  (a parked runtime worker, a held lock, sequential network calls), not
  extra CPU.

## Shared Domain Machinery (ds-core, `crates/core/`)

- **`ds_core::web_mercator`** — the ONLY EPSG:3857↔WGS84 implementation
  (Critical Rule 4).
- **`ds_core::geo`** — CRS transforms (WGS84, TM, LAEA, LCC, Stereographic),
  `GeoTransform`, `geometry_to_pixels`, `destination_point`,
  `geodetic_to_ecef`, `OutputCrs::footprint_pixel_window` (low-zoom ghost
  guard, #453).
- **`ds_core::instances`** — model-run (forecast reference time) machinery
  shared by ALL forecast engines (#337), so run selection, instance lists and
  instance-id encoding are identical everywhere:
  - `RunInfo { reference_time, valid_times }`; `instance_id()` = compact
    `%Y%m%dT%H%MZ` stamp.
  - `format_instance_id` / `parse_instance_id` — URL ↔ reference-time codec
    (parse also accepts RFC 3339). The API layer owns the string form;
    engines only see `Option<DateTime<Utc>>`.
  - `select_run(&BTreeMap<..>, Option<DateTime<Utc>>)` — `None` ⇒ latest,
    `Some(rt)` ⇒ exact run, absent ⇒ `None` → 404.
  - `build_instances(&runs, |rt, run| valid_times)`.
  - Engine contract: store runs in `BTreeMap<DateTime<Utc>, _>` keyed by
    reference time; implement `EdrEngine::get_instances()` (default empty =
    non-forecast); honour the trailing `reference_time` parameter on query
    methods and `MapEngine::get_raster_tile` (`None` ⇒ latest); populate
    `RasterInfo.reference_times`. GRIB and QueryData implement this; other
    engines accept-and-ignore. Zarr instances are a follow-up (it pins the
    latest run internally).
  - API surface: EDR `/instances`, `/instances/{id}`,
    `/instances/{id}/{position,area}` — gated on `get_instances()` being
    non-empty; no-instance routes default to the latest run. WMS exposes
    `DIM_REFERENCE_TIME` (see `crates/api-wms/CLAUDE.md`). Maps/Tiles still
    pass `reference_time: None` (follow-up).
- **Vertical dimension** — `ds_core::vertical::VerticalDimension`, surfaced
  on `RasterInfo.vertical` and `EdrEngine::get_vertical_extent`.
  `MapEngine::get_raster_tile` takes `z: Option<f64>` (one rendered layer);
  EDR query methods take `z: Option<&[f64]>` (one or more levels). WMS
  exposes it as `ELEVATION`, Maps/Tiles as `elevation`, EDR as `z`. The API
  layer rejects z/elevation against a collection with no vertical extent
  (HTTP 400). The ODIM PVOL engine uses it for radar elevation angle.
- **`ds_core::cells`** — storm-cell segmentation and tracking over
  `VoxelGrid` (see `crates/engine-odim/CLAUDE.md`).
- **`ds_render::rasterize::fill_polygon`** — the shared vector→raster fill
  primitive (#397), fed by `ds_core::geo::geometry_to_pixels` (vertices
  projected via `OutputCrs::world_to_fraction`, never per pixel). Vector
  `MapEngine`s (engine-cap today) may depend on ds-render for it — an
  approved exception; engines still return `RasterTile` domain types and
  colorization stays in the API layer.

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
| Nowcast | `MapEngine` + `FeatureEngine` (derived: wraps another collection's engine) | WMS, Maps, Tiles — motion-extrapolated future frames; Features — tracked cell intelligence (severity, deviant movers, #544). EDR + instances = #523 |
| PostGIS | `EdrEngine` + `FeatureEngine` + `MapEngine` (events shape only) | EDR (position, locations, area), Features; events shape: EDR (area) + WMS/Maps/Tiles (age-colored strike layer) |

## Config Format

```toml
[server]
host = "0.0.0.0"
port = 8000
# base_url = "https://api.example.com"  # optional, static fallback for absolute links behind a proxy
# trust_proxy_headers = true            # optional, per-request link base from proxy headers (default false)
# collections_dir = "collections.d"     # optional, directory of per-collection .toml files
# watch_collections_dir = true          # optional, auto-reload on collections_dir + colormaps_dir changes (default false)
# watch_debounce_ms = 500               # optional, coalesce-window for the watcher (default 500)
# colormaps_dir = "colormaps.d"  # optional, directory of palette files loaded as
                                 # named colormaps (name = file stem). Formats:
                                 # .toml (ColormapDef), GMT .cpt, GRLevelX .pal,
                                 # GDAL color-relief .txt/.clr, SLD .sld (ColorMap). Re-read on reload;
                                 # missing dir = hard error; other extensions skipped.

# Optional named colormaps. Like [[style_bundles]], MUST live in top-level
# config.toml (rejected in per-collection files). Registered next to the
# built-ins; the name works anywhere a built-in colormap name does ([wms]
# colormap, styles, parameters, bundle defaults/extras). A user colormap may
# shadow a built-in name (replaces it deployment-wide, logged WARN);
# duplicate user names are a config error. Unknown colormap name references
# anywhere in config are a LOAD ERROR (no silent viridis fallback).
[[colormaps]]
name = "radar_nssl2"
title = "NSSL Reflectivity 2"
# interpolation = "step"        # "linear" (default) | "step" (discrete classes)
# nodata_color = "#00000000"    # optional
color_stops = [
  { value = 5.0,  color = "#414141" },
  { value = 60.0, color = "#FF0000" },
]

# Optional per-parameter default-style override rules, checked before the
# EMBEDDED defaults table (#320: parameters of multi-parameter collections
# with no explicit style match built-in defaults — temperature palette for
# t2m/2t/TMP by unit K|C, pressure for msl by unit Pa|hPa, radar_dbz for
# DBZH, etc. — BEFORE the collection-level colormap; opt out per collection
# with `[wms] parameter_defaults = false`). Top-level config.toml only.
[[parameter_defaults]]
names = ["td", "dewpoint_2m"]   # exact matches (normalized: lowercase alnum)
contains = ["dew_point"]        # substring matches vs name and title
colormap = "temperature"
[[parameter_defaults.unit_ranges]]
unit = "K"
min = 233.15
max = 323.15

# Optional shared WMS style bundles. MUST live in top-level config.toml —
# a [[style_bundles]] block inside a collections_dir file is silently
# dropped by serde and any collection referencing it will fail validation.
[[style_bundles]]
id = "radar_multi"
[style_bundles.default]
colormap = "radar_dbz"
[[style_bundles.extras]]
colormap = "radar_fmi"   # name defaults to the colormap name, title to the
                         # palette's title; both can be set explicitly

[[collections]]
id = "weather"
title = "Finnish Weather Observations"
description = "Hourly weather observations"
data_path = "testdata/weather.csv"
apis = ["edr", "features"]     # defaults to ["edr"]
engine_type = "csv"             # defaults to "csv"
keywords = ["weather", "observations", "Finland"]   # optional discovery metadata
[collections.license]
title = "CC-BY-4.0"   # required; an SPDX id (no spaces). With no `url`, the
                      # link auto-resolves to https://spdx.org/licenses/<id>.html
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
# Attach a shared style_bundle and/or set inline fields — they merge
# slot-wise (bundles v2), inline winning each slot it defines (palette
# source, min, max; named styles union with inline winning name clashes).
style_bundle = "radar_multi"
# colormap = "radar_dbz"

# Optional /preview SPA tuning: cap the time slider's `values[]` to the most
# recent ISO 8601 duration before the latest timestep. Manifest-only; does
# NOT constrain the underlying engine.
[collections.preview]
time_window = "PT12H"

# Derived nowcast collection (#519): motion-extrapolates another collection's
# frames into the future. `source` must be a non-derived collection in the
# same config with wms/maps/tiles enabled.
[[collections]]
id = "radar-nowcast"
engine_type = "nowcast"
apis = ["wms", "maps", "tiles"]

[collections.nowcast]
source = "radar"        # collection id to extrapolate
horizon = "PT2H"        # how far into the future (default PT2H)
# step = "PT5M"         # timestep spacing (default: source cadence)
# lightning_source = "lightning"  # events-shape postgis collection: per-cell
                                  # flash_count/rate + lightning_jump (#549)

[collections.wms]
colormap = "radar_dbz"
```

See config struct definitions in each engine crate and `ds-core/src/config.rs`
for all fields.

### Per-file collection configs (`collections_dir`)

Collections can be individual `.toml` files in a directory (one collection per
file, no `[[collections]]` wrapper) instead of — or in addition to — inline
`[[collections]]` in `config.toml`:

```toml
# collections.d/radar-opera.toml
id = "radar-opera"                 # `id` is required (not derived from filename;
title = "OPERA Radar Composite"    #  a filename/id mismatch logs a warning)
engine_type = "geotiff"
apis = ["edr", "wms", "maps", "tiles"]

[geotiff]
filename_template = "OPERA@%Y%m%dT%H%M@0@ACRR.tiff"
parameter = "reflectivity"
unit = "dBZ"

[wms]
colormap = "radar_dbz"
```

Rules:

- Inline collections load first, then directory files sorted alphabetically by
  filename. Duplicate IDs across sources are rejected.
- Only `.toml` files load; rename to `.toml.disabled` to disable. Non-recursive.
- Missing directory = hard error; empty directory = valid but logs a warning.
- A single invalid file rejects the entire config (no partial loads).
- `[[style_bundles]]` is NOT allowed in per-collection files — only in
  `config.toml`. Referencing a bundle from a per-collection `[wms]` is fine;
  defining one is rejected with an explicit error.
- A `style_bundle` MERGES with inline `[wms]` fields (bundles v2): per
  parameter and per slot (palette source / min / max), the chain is inline
  `[[wms.parameters]]` → bundle `[[style_bundles.parameters]]` → inline
  `[wms]` fields → bundle default. Named styles are the union of bundle
  extras and inline `[[wms.styles]]`, inline winning name clashes. A bundle
  carries shared per-parameter defaults via `[[style_bundles.parameters]]`
  (same fields as `[[wms.parameters]]`; names must be unique per bundle).
- Inside a bundle, an `[[style_bundles.extras]]` entry with a `parameter`
  field is scoped to that parameter's layer only; untagged extras are shared
  across every parameter layer.
- Hot reload (`POST /admin/collections/reload`) picks up added/removed/changed
  files. The optional filesystem watcher and its trust model are documented in
  `crates/server/CLAUDE.md`.

### Collection keywords & license

Two optional per-collection discovery fields (any engine), surfaced across
every API from the shared `CollectionConfig`:

- **`keywords`** — flat array of non-empty strings. Emitted as OGC Common
  Part 2 `"keywords"` in collection JSON, `<KeywordList>` in WMS
  GetCapabilities (after `<Abstract>`, WMS 1.3.0 schema order), chips on HTML
  pages, and matched by `/collections?q=` (whole-word or phrase).
- **`[collections.license]`** — `title` required (human name or SPDX id) +
  optional `url`. A plausible SPDX id with no `url` synthesizes
  `https://spdx.org/licenses/<id>.html`. Rendered as a `rel="license"` JSON
  link, `<Attribution>` in WMS (after `<Dimension>` elements), and an HTML
  link. A free-text title with no `url` shows its name but produces no JSON
  link (a link object requires an `href`).

Both validated at config load: empty keyword entries, empty license title, or
a non-http(s) license URL are rejected.

## Admin & Operations

- **Reload:** `POST /admin/collections/reload` — re-reads config, atomically
  swaps engines. The shared core `admin::do_reload` is also driven by the
  optional `collections_dir` watcher. Reload is **incremental** (#574):
  collections whose `CollectionConfig` is unchanged keep their live engine
  (poll loop untouched, no re-bootstrap); only added/changed/removed ones
  rebuild, and their cached tiles are evicted per collection. See
  `crates/server/CLAUDE.md` for the rules.
- **Health:** `GET /health` — per-collection ready/degraded/failed status.
  HTTP 503 only when all collections failed.
- **Metrics:** `GET /metrics` — Prometheus format. Path labels use route
  patterns, never raw URLs (cardinality explosion).
- **Grafana dashboards live in this repo:**
  `docker/grafana/dashboards/meteocore-overview.json`, provisioned via
  `docker/grafana/provisioning/`. **When adding a new `/metrics` family,
  update the dashboard too** (same PR or a follow-up issue). Cache families
  follow the established panel pattern: hit ratio, bytes vs capacity, miss
  rate.
- **State:** API state wrapped in `ArcSwap` for lock-free reads. Render
  semaphore (2× CPU cores, min 8) shared across Maps/Tiles/WMS. Engine
  loading lives in `server/src/admin.rs`.

## Code Style

- Use `thiserror` for error types, not manual `impl Display`.
- Engine methods return `Result<T, DataServerError>`.
- Keep handlers thin — delegate logic to the engine, map errors to HTTP
  status codes.
- Use `serde_json::json!` for building JSON responses.
- Do not leak internal error details to clients — generic messages for 500s.
- Public ds-core domain structs (`RasterInfo`, `CollectionConfig`, …) are
  built via cross-crate struct literals by engines and tests. Do NOT add
  `#[non_exhaustive]` to them. Adding a field means patching every literal
  (grep the field name, then `cargo check --workspace --tests`).
