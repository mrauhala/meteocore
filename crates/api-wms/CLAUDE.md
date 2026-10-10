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

## Rendered-image cache (#1010)

`ds_render::RenderedCache`, shared with Maps and Tiles, holds encoded images
under the exact request (`CacheKey`). Every successful non-empty render is
inserted on its first render, a meta-tiled viewport included; empty and
error images never are. Measured on a day of a live deployment's GetMaps
(85 000, every one an EPSG:3857 viewport, none tile-shaped):

- **Who hits.** 4 % repeated an earlier request URL exactly: fixed views
  such as a display cycling the same animation frames, a client's default
  view, a fixed NWP view. About a third of hits are not exact repeats: the
  key carries the timestep `resolve_parameter_time` picks, so a TIME that
  snaps to a step already rendered for the view (a frame not ingested yet
  snaps to the latest) reuses that step's image.
  Tiles `z/x/y` keys repeat by construction; Maps traffic was negligible.
- **Do not gate inserts.** Admitting only tile-aligned views would drop
  every one of those hits. Admitting a view on its second render was
  replayed through the cache with the day's exact repeats and served ~1 450
  of them at any size, against ~2 300 for inserting all at 256 MB: most
  repeating URLs come exactly twice, and `quick_cache` already evicts
  never-read entries first. A snapped TIME needs the first render cached too.
- **Size.** 256 MB, the default (`ds_core::config::DEFAULT_RENDERED_CACHE_MB`),
  holds roughly 25 minutes of that traffic's renders. The replay of exact
  repeats gained nothing at 512 MB; about 1 hit in 8 reuses an older image,
  so halving from 512 MB costs at most that share. A miss on a meta-tiled
  view usually costs assembly and encoding (~40 ms), not an engine read:
  give spare memory to `metatile_cache_mb`.
- A hit ratio of a few % with the fill pinned at 100 % is expected with
  viewport clients and is not a sizing signal.

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
  style's) before defaulting TIME, resolves the run with
  `resolve_parameter_reference_time` (GRIB's run depends on the parameter,
  #1005) and snaps with `resolve_parameter_time`, so the caches key the
  parameter's own run and timestep.
- **ELEVATION** — advertised when the collection has a vertical extent
  (`RasterInfo.vertical`); rejected with 400 otherwise.
- **`reference_time` (forecast model run, #337/#345):** forecast layers
  (non-empty `RasterInfo.reference_times`) advertise a custom
  `<Dimension name="reference_time">` alongside `time`, defaulting to the
  latest run. GetMap accepts `DIM_REFERENCE_TIME=<run>` (RFC 3339, which is
  also the EDR instance id, or the pre-#947 compact `%Y%m%dT%H%MZ` instance
  id), validated against the advertised runs —
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

## RGB composite layers (#819)

An engine's `MapEngine::composites()` are child layers `coll/<composite>`
after the parameter layers (a collection with composites always gets the
parent layer). They have no `StyleInfo`: `composite_layer` in
`handlers.rs` spots one before the style lookup.

- **Capabilities**: `write_composite_layer` gives each its title, an
  `<Abstract>` of the channels, the `time` dimension from
  `parameter_times(<composite>)` (the scans every band has) and one style,
  `default` (`ds_render::COMPOSITE_STYLE`), with a LegendURL.
- **Styles**: any other `STYLES` is `StyleNotDefined`, on GetMap and
  GetLegendGraphic. An unknown `coll/<name>` lists composites among the
  valid names.
- **Time**: `resolve_parameter_time(Some(<composite>))` keys the rendered
  and meta-tile caches and is the `time` passed to `get_raster_tiles`, so
  no band is drawn from a scan the key does not name (#507). `None` (no
  shared scan) renders the empty image without calling the engine, so
  nothing is cached under a key naming no scan.
- **Rendering**: the direct path composes with
  `ds_render::render_composite_tiles`; the meta-tile path with
  `ds_render::render_metatiled_composite`, which caches the composed RGBA
  256×256 tiles in the same `TilePixelCache` as colormapped ones.
  `TRANSPARENT`/`BGCOLOR` are applied at encode time on both, as for
  parameter layers.
- **Admission**: `RenderJob::acquire_raster_planes` with one plane per band.
- **Legend**: the channel list (`composite_legend_json`,
  `render_composite_legend`), with the bands' units; no colour bar.

## TRANSPARENT / BGCOLOR (#163)

- `TRANSPARENT` is `TRUE`/`FALSE` (case-insensitive) and defaults to `TRUE`
  — deliberately not the spec's `FALSE`, which would turn every overlay
  client that omits it opaque. Any other value is `InvalidParameterValue`.
- `BGCOLOR` is strictly `0xRRGGBB` (lower-case `0x`, hex digits of either
  case), default white; anything else is `InvalidParameterValue`.
- `GetMapParams.background` is `Some(BGCOLOR)` for `TRANSPARENT=FALSE` and
  for JPEG (no alpha, so its nodata is always background), else `None`.
  The image is composited onto it at encode time (`ds_render::flatten_onto`):
  every pixel opaque, PNG8 kept, no `tRNS`.
- Caching: the background is part of the rendered-image `CacheKey` (the
  encoded bytes differ) but NOT of the meta-tile key — cached tiles stay
  RGBA, so opaque and transparent views share them. The all-nodata path uses
  `ds_render::background_tile` (memoized per size + background); the error
  tile is flattened too.
- GetCapabilities advertises nothing for these; the layer `opaque` attribute
  is unrelated and unchanged.

## QUALITY / `[wms] webp_quality`

- `QUALITY` (vendor, GetMap only) is 1–100 for JPEG and WebP; WebP 100 is
  lossless. PNG + `QUALITY` and out-of-range values are
  `InvalidParameterValue`. GetLegendGraphic ignores it (legends stay
  lossless) so clients that send it on every request keep working.
- The handler resolves the effective format once, before keying:
  `params.format.with_quality(params.quality, collection webp_quality)`.
  `ds_render::ImageFormat` carries the quality, so `CacheKey.format` keys
  it; the meta-tile key needs none (its tiles are RGBA, encoded per
  request). Never re-derive the format from `params.format` after that
  point, or lossy and lossless responses alias in the rendered cache.

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

Every served GetMap carries a `ds_executor::RenderTiming` (timed from the
rendered-cache lookup): `hit`, `assembled` (meta-tiles all cached) or `cold`.
An error tile carries none. Maps and Tiles report `hit`/`cold` the same way.
Its render phases (#147) reuse the slow-render log's measurements, never a
second timer: `sem_wait` is `queue`; a meta-tiled view reports
`MetaTileStats.engine` (uncached tiles only) as `engine`, `assemble`, and
`colorize + encode` as `encode`, matching the direct path's `render_tile`.
An all-nodata view skips assemble/encode; a hit reports no phase.
