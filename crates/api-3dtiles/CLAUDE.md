# api-3dtiles crate — Claude Instructions

OGC 3D Tiles HTTP layer (epic #346). The chain: `VolumeEngine` (ds-core
trait) → domain types → `ds-3dtiles` encoders → this crate serves the bytes.
Encoder rules live in `crates/ds-3dtiles/CLAUDE.md`; the engine side in
`crates/engine-odim/CLAUDE.md`.

## Representations

Four products are served:

- `?representation=points` → `.pnts` (region-only tileset; `RTC_CENTER`
  self-places). Native resolution; ignores `?resolution=`.
- `?representation=isosurface` (default) and `?representation=echotop` →
  `.glb` + tile `transform` = antenna ECEF. Both share `content.glb`,
  disambiguated by `?representation=` in the content URI.
- **`voxels`** (#351) has its own `…/voxel/` sub-path (NOT a
  `?representation=` value) so the implicit-tiling URIs resolve relatively.

Capability + origin/extents are coupled in ONE field:
`VolumeInfo.voxel_grid: Option<VoxelGridCaps { origin, radius_m, height_m }>`
— `Some` ⇒ mesh + voxel products available AND cylinder origin/extents
present ("supports but no origin" is unrepresentable, never a 500). It
drives the collection JSON's `representations` array → the viewer's toggle.

- Isosurface seals with `background=Some(-32)`; echo-top uses a height ramp.
- Mesh products take `?resolution=low|med|high` (`Resolution` enum →
  voxel-grid dims: low `[128,360,48]` ≈2.2M cells, med `[256,360,56]` ≈5.2M
  — the default, high `[512,360,64]` ≈11.8M, ~12 s first compute). The tier
  is echoed into `content.uri`.
- Isosurface accepts `?threshold=20,35,50` (comma list, ≤5 values,
  sorted+deduped into the canonical `content.uri`); a list on `echotop` is
  a 400.
- Bad threshold/resolution → 400; empty result → 404.

## Routes (mounted at `/3dtiles`)

- `GET /collections/{id}/tileset.json`
  (`?representation=&quantity=&datetime=&min_value=&threshold=&resolution=`)
- `GET /collections/{id}/content.pnts` (`?quantity=&datetime=&min_value=`)
- `GET /collections/{id}/content.glb`
  (`?representation=&quantity=&datetime=&threshold=&resolution=`)
- Voxel trio: `GET /collections/{id}/voxel/{tileset.json,subtrees/*,content/*}`
  (`?quantity=&datetime=&resolution=`)
- Plus `/`, `/collections`, `/collections/{id}`, and `/viewer`.

The tileset's `content.uri` embeds the resolved quantity (+ pinned time +
min_value/threshold + representation + resolution) so the content fetch is
deterministic.

## Time-dynamic playback (#350)

- Per-timestep tilesets via `?datetime=<volume time>` (engine selects the
  nearest volume; `None` ⇒ latest). The collection JSON advertises a `times`
  manifest (`VolumeInfo.times`, RFC 3339 `…Z`, ascending; each value
  round-trips as `?datetime=`).
- The viewer uses a global six-slot queue across control changes. A slot
  covers tileset JSON AND root content loading; `fromUrl` alone is not readiness.
  JSON loads use an abortable `Resource` subclass (preserved by `clone`) with a
  timeout, so superseded/stalled metadata cannot permanently consume a slot.
  Hidden loads use `preloadWhenHidden: true` until `tileLoad`, then disable it.
  Only content-ready frames enter the cache/playback set; failures and canceled
  loads are destroyed. Track load/unload through public Cesium events.
- Refresh the manifest every minute and on returning to a visible tab. Preserve
  controls, camera, cached frames, and historical seeks; follow new volumes when
  viewing latest. Guard metadata fetches against out-of-order collection changes.
- The viewer caps preload at the most recent `MAX_FRAMES` (48) so an
  unbounded source can't hold hundreds of tilesets. Frame count = the
  engine's retained volumes (`[odim] max_files`/`time_window`).

## Viewer

`GET /viewer` — a bundled CesiumJS page (`include_str!`-baked from
`viewer/index.html`) with collection/quantity/representation/resolution
pickers and a time scrubber (shown when >1 volume). Same-origin API base by
default; `?base=` selects another *path on the same origin* (a proxy-prefixed
mount). Other origins are ignored, so a crafted link cannot make a served viewer
render another server's data as ours (CodeQL `js/client-side-request-forgery`).
Opened from disk, the viewer targets the public instance.

- Point styling: `pointSize` by `${value}` (weak ~1 px → strong ~16 px);
  the `min dBZ` field restyles `show` client-side (instant while ≥ the
  fetched floor; going lower re-fetches).
- The collection JSON carries a `legend` (point colormap sampled to
  `#rrggbb` stops via `legend_stops`) rendered as a gradient bar.
- Voxels render via `Cesium3DTilesVoxelProvider` + `VoxelPrimitive` with a
  reflectivity transfer-function `CustomShader` (`fsInput.metadata.<q>`).

## Concurrency & caching

- `read_point_cloud`/`read_voxel_grid` are sync (blocking I/O + long CPU).
  Both content handlers bound them with the shared render semaphore and run
  via `spawn_blocking` — never inline on a request worker (same pattern as
  the raster APIs).
- **Content cache** (`src/cache.rs`): process-global byte-bounded LRU of
  encoded content bytes + ETag, keyed (collection, product, quantity,
  datetime, params, dims) **plus a data-version hashed from
  `VolumeInfo.times`** (new volume ⇒ new version — "latest"/nearest-time
  invalidate without duplicating engine selection logic). Exact advertised
  times use a stable version so new arrivals retain archived frames. Per-key
  shared-result flights coalesce successes AND errors (including with retention
  disabled). A cache-owned task finishes and caches an encode even if HTTP
  waiters disconnect; RAII cleans up flights on panic. Blocking encodes own
  semaphore permits through completion.
  `MC_3DTILES_CONTENT_CACHE_MB` (default 512, 0 disables retention).
- The engine-side `VOXEL_GRID_CACHE` is the second layer (see engine-odim
  notes).
- **Cache-Control:** a `?datetime=` exactly matching an advertised volume
  time → `max-age=86400, immutable` (the viewer pins frames from the `times`
  manifest, so reloads hit the browser cache). Between-volumes datetimes and
  "latest" → `max-age=60`. A 304 costs two cache lookups, never a recompute.
- Cache metrics: `tiles3d_content_cache_*`, `pvol_voxel_grid_cache_*` in
  `/metrics`.

## Config

Add `"3dtiles"` to a collection's `apis` (only `odim-volume` supports it).
v1 uses one shared reflectivity colormap; per-collection/per-quantity
colormaps are follow-ups.

Viewer regression tests: `node --test crates/api-3dtiles/tests/viewer.test.cjs`
(delay metadata/content and check scheduling, failure, cancellation, refresh).
These run in CI; WebGL/render verification remains a separate check.

Point/mesh computations now use `ds-executor` admission: the shared bounded
render queue and a 30 s queue/compute deadline. The raster 3 s default does not
apply to these larger products. Their workers (and the separate voxel worker)
retain CPU permits on client disconnection. Admission/deadline failures are
503 + Retry-After. Coalesced cache waiters share the cache-owned result flight.
