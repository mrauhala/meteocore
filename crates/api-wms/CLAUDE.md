# api-wms crate — Claude Instructions

WMS 1.3.0 HTTP layer. Read the root `CLAUDE.md` first — Critical Rules 3
(no XML via `format!()`) and 4 (web_mercator) apply throughout this crate.

## XML output

**WMS uses XML, not JSON. All XML output uses `quick-xml::Writer` for proper
escaping. Never build XML with `format!()` or string concatenation — XML
injection risk.**

## BBOX axis order (critical gotcha)

WMS 1.3.0 BBOX axis order depends on the CRS:

- **CRS:84**: `BBOX=west,south,east,north` (lon/lat — same as internal)
- **EPSG:4326**: `BBOX=south,west,north,east` (lat/lon — **swapped!**)
- **EPSG:3857, EPSG:3067, EPSG:3035**: `BBOX=minx,miny,maxx,maxy`
  (easting/northing)

The handler normalizes all bbox values to `[west, south, east, north]`
internally. Test with BOTH CRS:84 and EPSG:4326 to catch axis-order bugs.

## Render path: meta-tiling

WMS EPSG:3857/3067/3035 GetMap goes through the **meta-tile** path
(`ds-render/src/metatile.rs`), NOT the direct `get_raster_tile` path that
Maps/Tiles use. Remember this when debugging: a fix applied to the direct
path can leave the WMS symptom unchanged (#448 vs #452).

- The meta-tile assembly is the only allowed re-derivation of the output
  coordinate map; it must stay consistent with `OutputCrs`/`ProjectionGrid`.
- Viewport/bbox conversions are UNCLAMPED (`ds_core::web_mercator`); clamp to
  `LAT_LIMIT_DEG` only for tile-index selection (#452).
- EPSG:3067/3035 use separate internal metre grids: origin (0,0),
  half-octave ladder from 128000 m/px (including 1000/500/250 m/px).
  These are not advertised OGC tile matrix sets. The grid identity is part
  of the tile cache key; each engine call gets a tile-specific projected
  `OutputCrs::Projected.bbox` plus its WGS84 source-read envelope.
- Geographic/unsupported output, excessive tile fan-out, and over-zoom use
  the direct path. `[server] metatile_cache_mb = 0` bypasses all meta-tiling.
- Assembly resampling is nearest-neighbour — bilinear blending destroyed the
  discrete radar palette and killed PNG8 (~9× bigger output, #451).
- Meta-tiling is engine-agnostic and WMS-only; keep it enabled (it is the pan
  substrate — 86% marginal tile hit rate in production).

## Dimensions

- **TIME** — valid-time axis from `RasterInfo.times`. A TIME-less GetMap
  resolves through `ds_core::map_engine::default_request_time`: the engine's
  `default_time()` when supplied, else the parameter's latest time, else
  `times.last()`. GetCapabilities advertises the same default. CAP advertises
  future warnings while keeping its snapshot's `as_of` as the "active now"
  default.
- **Per-parameter TIME (#819).** When `MapEngine::parameter_times(param)` is
  `Some`, `RasterInfo.times` is the union over parameters and each child
  layer (`coll/param`) re-declares `<Dimension name="time">` with its own
  values and default — WMS 1.3.0 Table 7 makes Dimension inheritance
  "replace". GetMap settles the parameter (`LAYERS=coll/param`, then the
  style's) before defaulting TIME and snaps with `resolve_parameter_time`,
  so the caches key the parameter's own timestep.
- **ELEVATION** — advertised when the collection has a vertical extent
  (`RasterInfo.vertical`); rejected with 400 otherwise.
- **`reference_time` (forecast model run, #337/#345):** forecast layers
  (non-empty `RasterInfo.reference_times`) advertise a custom
  `<Dimension name="reference_time">` alongside `time`, defaulting to the
  latest run. GetMap accepts `DIM_REFERENCE_TIME=<run>` (RFC 3339 or the
  compact `%Y%m%dT%H%MZ` stamp), validated against the advertised runs —
  unknown run or non-forecast layer → `InvalidDimensionValue` (HTTP 400), no
  `nearestValue` (engines require an exact match). The run flows through
  `get_raster_tile` and into the rendered + meta-tile cache keys
  (`CacheKey.reference_time`, `TileKeyPrefix.reference_time`) so distinct
  runs don't collide.
- **Content version.** Both keys also carry the engine's
  `MapEngine::content_version()` (`CacheKey.content_version`,
  `TileKeyPrefix.content_version`), read right after `resolve_time`. It is
  `0` for immutable-timestep engines; an engine whose content for a fixed
  instant is revised in place (engine-cap) bumps it, so an explicit
  `TIME=` render can't be served stale forever from the no-TTL caches.

## Capabilities niceties

- Collection `keywords` → `<KeywordList>` (after `<Abstract>`, WMS 1.3.0
  schema order); license → `<Attribution>` (after `<Dimension>` elements).
- ODIM per-site layers: `<Title>` is prefixed with the site place name via
  `RasterInfo.layer_subtitle` so flat clients can tell per-site layers apart.

GetMap cache misses use `ds-executor::RenderJob`: a bounded shared queue and
one absolute deadline across admission/render/encoding. The worker retains
CPU and memory permits after HTTP timeout/disconnect. Propagate
`DeadlineExceeded` as 503 + Retry-After, never a successful transparent/error
image. Meta-tile fan-out checks the same deadline between tiles.
