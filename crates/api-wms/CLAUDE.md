# api-wms crate — Claude Instructions

WMS 1.3.0 HTTP layer. Read the root `CLAUDE.md` first — Critical Rules 3
(no XML via `format!()`) and 4 (web_mercator) apply throughout this crate.

## XML output

**WMS uses XML, not JSON. All XML output uses `quick-xml::Writer` for proper
escaping. Never build XML with `format!()` or string concatenation — XML
injection risk.**

## Parameter names (#1065)

- WMS 1.3.0 §6.8.1: names are case-insensitive. `WmsQuery` deserializes the
  query string as its percent-decoded `(name, value)` pairs and
  `WmsQuery::from_pairs` keys them by lower-cased name (`slot`), for every
  request type. A new parameter is a field plus a lower-case `slot` arm
  (and an entry in the params tests' `NAMES`/`fields`, whose exhaustive
  destructuring will not compile without it), never a serde alias: an
  alias matches only the spellings it lists, and the OGC test suite's
  `ReQuEsT`, `WiDtH` stopped 62 of its tests at their first request.
- Values are kept as sent and matched exactly, so layer and style names
  are case-sensitive. `TRANSPARENT`, the JSON legend `FORMAT` and
  GetFeatureInfo's `INFO_FORMAT` (a MIME type) are the documented
  exceptions, matched case-insensitively.
- An unknown name is ignored. A repeated parameter, in any mix of cases,
  takes its first value; serde's derive used to reject a repeat with a
  plain-text 400 that was no ServiceException.
- GetMap and GetFeatureInfo require `VERSION=1.3.0`; without it they are
  `MissingParameterValue` (#1065), which ets-wms13's `getmap:no-version`
  checks. Only GetCapabilities negotiates the version and answers any or no
  `VERSION` with the 1.3.0 document.

## Exception codes (#1065)

- Use WMS 1.3.0 Table E.1's code wherever it names one, spelled as it
  spells it (`WmsError::error_code`): `InvalidCRS` (never `CRSNotDefined`),
  `LayerNotDefined`, `StyleNotDefined`, `InvalidFormat`,
  `InvalidDimensionValue` (malformed `TIME`/`ELEVATION`, unknown
  `DIM_REFERENCE_TIME`, and either of the last two on a layer without
  that axis), `OperationNotSupported`, and for GetFeatureInfo
  `LayerNotQueryable` and `InvalidPoint`. `MissingParameterValue` /
  `InvalidParameterValue` are for what the table does not cover.
- A `LAYERS` list is checked name by name before `LAYER_LIMIT` (5)
  refuses more: `resolve_map_layer` runs on every `(layer, style)` pair
  (`GetMapParams::layers`), so an undefined one is `LayerNotDefined` /
  `StyleNotDefined`. Several layers pair with `STYLES` values one to one
  (§7.3.3.4), an empty `STYLES` or one of commas only meaning every
  default; a single layer takes the whole `STYLES` value, as it always has.

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
- The direct path calls `ds_core::map_engine::get_raster_tile_in_domain` /
  `get_raster_tiles_in_domain`, never the engine's own methods: a CRS:84 /
  EPSG:4326 BBOX may reach past a pole, and those rows must stay empty
  (§7.3.3.6, #1065). The helpers ask the engine only for the rows whose
  centres lie inside ±90°, and nothing for a BBOX wholly past a pole (the
  empty image). Engines need no guard of their own; Maps does the same.
- A view with no row inside its CRS is the empty image before either path
  runs: `OutputCrs::domain_rows` is `None` (#1089). Besides a geographic
  BBOX wholly past a pole, that is an EPSG:3067/3035 BBOX wholly outside
  the projection's valid area (`ds_core::geo::outside_projection_domain`).
  The check must precede meta-tiling, whose tiles reach past the view and
  call the engine directly: a box just past the LAEA disk has covering
  tiles that cross it.
- `params::parse_bbox` asks `outside_projection_domain` before
  `wgs84_envelope`, which samples the box's edges and adds a pole the box
  holds and, for EPSG:3067, where its area's edges enter and leave it
  (`geo::area_edge_points`). Wholly outside is the empty image, with read
  window `NO_READ_WINDOW` (NaN, never read). An envelope of `None` for any
  other box means the edges miss a valid area inside it: an EPSG:3035 box
  wider than the disk, or a tall one crossing the disk between samples.
  That stays `InvalidParameterValue`: there is no read window, and a global
  one would read whole datasets. An EPSG:3067 box that meets the area
  always has an envelope. Malformed boxes are rejected before
  either. Both projections' inverses are `None` past their area: the LAEA
  disk, and for EPSG:3067 past a pole's northing or an easting
  9 997 965 m from 27°E's (`ds_core::geo::Crs::TransverseMercator`, #1113,
  #1130). In a box
  partly outside, `project_node` gives those pixels NaN and they stay
  empty. A meta-tile wholly outside is an all-nodata tile with no engine
  call, so a zoomed-out view of the north stays meta-tiled.
- Several layers (#1181): each layer takes its own path (`render_layer`):
  meta-tiled pixels from `ds_render::assemble_metatiled[_composite]`, or
  the direct path's `render_tile_rgba` / `render_composite_tiles_rgba`,
  all background-free. `render_layers` draws them bottom up with
  `ds_render::draw_over` and encodes once with `encode_rgba`; a single
  layer goes through the same code, so its bytes are unchanged.

## Rendered-image cache (#1010)

`ds_render::RenderedCache`, shared with Maps and Tiles, holds encoded images
under the request as resolved (`CacheKey`): the style layer and style name
`ds_render::resolve_layer_style` returns, not the raw `LAYERS`/`STYLES`, so
`STYLES=` and `STYLES=default`, and a multi-parameter collection's root
layer and `coll/<default>`, share entries (#1133); the meta-tile
`TileKeyPrefix` keys the same pair. Maps keys the same way into the same
instance, so a WMS view and a Maps request whose keys match (CRS code,
envelope, size, style, parameter, time and background: a WMS PNG or WebP
with `TRANSPARENT=TRUE` and a default Maps map, or `TRANSPARENT=FALSE`
and `transparent=false` on one colour) share one entry, whichever render
path filled it; Tiles keys its TileMatrixSet id as the CRS. Every successful non-empty render is
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
- **Size.** `[server] rendered_cache_mb`; 256 MB, the default (`ds_core::config::DEFAULT_RENDERED_CACHE_MB`),
  holds roughly 25 minutes of that traffic's renders. The replay of exact
  repeats gained nothing at 512 MB; about 1 hit in 8 reuses an older image,
  so halving from 512 MB costs at most that share. A miss on a meta-tiled
  view usually costs assembly and encoding (~40 ms), not an engine read:
  give spare memory to `metatile_cache_mb`.
  The deprecated per-collection `[wms] rendered_cache_mb` counts only while
  the `[server]` key is unset: the largest value among WMS/Maps/Tiles
  collections wins (`ds_core::config::select_rendered_cache_mb`, #1028);
  never pick "the first block found".
- **Hot allocation.** The same replay with `quick_cache`'s hot ring lowered
  from its 0.97 default (`ByteBoundedCache::new_with_hot_allocation`): at
  256 MB, 0.5 and 0.25 stayed near the default's ~2 300 hits; at 512 MB,
  0.25 served ~2 765 against ~2 200. `RenderedCache` keeps the default. A
  deployment raising `rendered_cache_mb` for heavy Tiles traffic could gain
  from 0.25: replay that size before shipping it (#1016, #1055).
- A hit ratio of a few % with the fill pinned at 100 % is expected with
  viewport clients and is not a sizing signal.
- **Layer lists are never cached** (#1181): a GetMap of several layers
  neither looks up nor inserts. Its key would name several collections,
  and `evict_collections` matches one `CacheKey.layer`, so reloading
  another listed collection would leave its image stale. Each layer keys
  its meta-tiles on its single-layer `TileKeyPrefix`, so a repeated list
  costs assembly, `draw_over` and one encode, no engine read. A single
  layer keeps its exact `CacheKey`.

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
  style's, then on a multi-parameter collection's root layer the default
  parameter whose style layer it draws with, #1133) before defaulting TIME, resolves the run with
  `resolve_parameter_reference_time` (GRIB's run depends on the parameter,
  #1005) and snaps with `resolve_parameter_time`, so the caches key the
  parameter's own run and timestep.
- **ELEVATION** — advertised when the collection has a vertical extent
  (`RasterInfo.vertical`); rejected with 400 `InvalidDimensionValue`
  otherwise.
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
- **Long axes (#1006).** `time` and `reference_time` values come from
  `ds_core::time_axis::wms_extent`: up to `MAX_LISTED_VALUES` (500) values
  the comma-separated list, byte-identical to before; past it, Annex C
  `min/max/resolution` ranges, one per stretch of at least three values one
  whole-second step apart, other values listed between them in axis order. An
  archive's 7300-run axis was a 206 810-character `<Dimension>`; it is now one
  range per gap. Lossless: every value a range names is on the axis, and
  GetMap validates `DIM_REFERENCE_TIME` against `RasterInfo.reference_times`
  itself (`binary_search`, the list is ascending by contract), never the
  advertised text. Never hand-format a time dimension's values.
- **Content version.** Both keys also carry the frame's
  `MapEngine::content_version_for(parameter, time, reference_time,
  elevation)` (`CacheKey.content_version`, `TileKeyPrefix.content_version`),
  read in `resolve_layer` right after the time and run resolved, with the
  arguments the render gets. It is `0` for immutable-timestep engines (the
  default forwards `content_version()`); an engine whose content for a
  fixed instant is revised in place (engine-cap) bumps it, so an explicit
  `TIME=` render can't be served stale forever from the no-TTL caches.
  GRIB's is per message drawn (#1334), so a scan adding other steps keeps
  a frame's image and meta-tiles.
- **Several layers (#1181).** `handlers::resolve_layers` gives `TIME`,
  `ELEVATION` and `DIM_REFERENCE_TIME` to the listed layers that have the
  dimension (`!info.times.is_empty()`, `info.vertical`,
  `info.reference_times`) and `None` to the others. A run a forecast layer
  lacks is still its `InvalidDimensionValue`. A dimension no listed layer
  has stays what it is for one layer: `ELEVATION` / `DIM_REFERENCE_TIME`
  `InvalidDimensionValue`, `TIME` passed to every layer. Without `TIME`
  each layer defaults and snaps its own (`resolve_layer`) and keys its own
  time, run, level and content version. `Cache-Control` is `immutable`
  only when every layer took an explicit `TIME` that resolved and every
  content version is 0.

## RGB composite layers (#819)

An engine's `MapEngine::composites()` are child layers `coll/<composite>`
after the parameter layers (a collection with composites always gets the
parent layer). They have no `StyleInfo`: `composite_layer` in
`handlers.rs` spots one before the style lookup.

- **Capabilities**: `write_composite_layer` gives each its title, an
  `<Abstract>` of the channels, the `time` dimension from
  `parameter_times(<composite>)` (the scans every band has) and one style,
  `default` (`ds_render::COMPOSITE_STYLE`), with a LegendURL.
  `queryable` comes from `composite_is_queryable`.
- **Styles**: any other `STYLES` is `StyleNotDefined`, on GetMap and
  GetLegendGraphic. An unknown `coll/<name>` lists composites among the
  valid names.
- **Time**: `resolve_parameter_time(Some(<composite>))` keys the rendered
  and meta-tile caches and is the `time` passed to `get_raster_tiles`, so
  no band is drawn from a scan the key does not name (#507). `None` (no
  shared scan) renders the empty image without calling the engine, so
  nothing is cached under a key naming no scan.
- **Rendering**: the direct path composes with
  `ds_render::render_composite_tiles_rgba`; the meta-tile path with
  `ds_render::assemble_metatiled_composite`, which caches the composed RGBA
  256×256 tiles in the same `TilePixelCache` as colormapped ones.
  `TRANSPARENT`/`BGCOLOR` are applied at the one encode, as for parameter
  layers.
- **Admission**: `RenderJob::acquire_raster_planes` with one plane per band;
  in a layer list `acquire_layered_raster` with the most planes one layer
  holds.
- **Legend**: the channel list (`composite_legend_json`,
  `render_composite_legend`), with the bands' units; no colour bar.

## TRANSPARENT / BGCOLOR (#163)

- `TRANSPARENT` is `TRUE`/`FALSE` (case-insensitive) and defaults to `FALSE`,
  as WMS 1.3.0 §7.3.3.9 specifies (#1065): a GetMap that omits it is opaque,
  nodata in `BGCOLOR`. Overlay clients send `TRUE` (OpenLayers' WMS sources
  do unless told otherwise), so they keep transparent nodata. Any other
  value is `InvalidParameterValue`.
- `BGCOLOR` is strictly `0xRRGGBB` (lower-case `0x`, hex digits of either
  case), default white; anything else is `InvalidParameterValue`.
- `GetMapParams.background` is `Some(BGCOLOR)` for `TRANSPARENT=FALSE`, the
  default, and for JPEG (no alpha, so its nodata is always background);
  `None` only for PNG/WebP with `TRANSPARENT=TRUE`. Every render path reads
  it: direct, meta-tiled, composites, the empty image and the error tile.
  With several layers it applies once, at the one encode in
  `render_layers`; each layer's pixels stay RGBA until then.
  The image is composited onto it at encode time (`ds_render::flatten_onto`):
  every pixel opaque, PNG8 kept, no `tRNS`.
- Caching: the background is part of the rendered-image `CacheKey` (the
  encoded bytes differ) but NOT of the meta-tile key — cached tiles stay
  RGBA, so opaque and transparent views share them. The all-nodata path uses
  `ds_render::background_tile` and a failed render `ds_render::error_tile`,
  both memoized per size + background + format with its quality; the error
  tile is flattened too.
- GetCapabilities advertises nothing for these; the layer `opaque` attribute
  is unrelated and unchanged.
- ets-wms13's `getmap:bbox-outside-crs` and `getmap:transparent-default`
  send no `TRANSPARENT` and want the opaque default: every pixel's RGB
  white, and no transparent pixel. A `TRUE` default failed both (a
  transparent pixel is RGBA 0,0,0,0). Tests that read a decoded pixel's
  alpha to tell drawn from empty must send `TRANSPARENT=TRUE`.

## EMPTY and ERROR images follow FORMAT (#1091)

- WMS 1.3.0 answers GetMap in the requested `FORMAT`, so the all-nodata
  image (`x-cache: EMPTY`) and the red error tile (`x-cache: ERROR`) are
  encoded in it at the effective `QUALITY`, with that `Content-Type`, like a
  populated render. Both go through `ds_render::encode_rgba`, the encoder
  entry point every render takes; never hand-roll a second encoding for
  them. ets-wms13's `getmap:each-format` checks this whenever the layer it
  picks has no data at its test bbox.
- JPEG has no alpha: the empty image is solid `BGCOLOR` (default white) and
  the semi-transparent error tile is flattened onto it. So are PNG and WebP
  without `TRANSPARENT=TRUE`, which keeps their alpha.
- Neither is inserted into the rendered cache. Both come from one
  process-wide memo in ds-render (`ds_render::background_tile`,
  `ds_render::error_tile`, #1093), keyed on the image and
  `(width, height, background, format)` so a PNG and a JPEG of one size, or
  the empty image and the error tile, never share an entry. The memo is
  bounded by bytes (32 MiB, one shard), not entries: a solid JPEG grows with
  the pixel count, to about 1 MB at 8000 × 8000. It has no `/metrics`
  family. A miss is single-flight, so an outage's concurrent failures of one
  size share one encode. The ERROR `Cache-Control` stays short.
- Both are encoded inside the render job, never on the async request worker
  (#1093): `finish_render` turns the render's outcome into the image to
  serve on the blocking worker that holds the CPU slot and the memory
  reservation, under the render deadline, and the handler only labels the
  result. Keep it there; `background_tile` and `error_tile` encode on a miss
  and block a caller waiting for another's encode. A `ResourceExhausted` or
  `DeadlineExceeded` render stays the 503 and encodes nothing. Maps and Tiles
  finish their empty image in the job the same way (`render_job`).

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
- Continuous palettes: recommend lossy WebP, never switch formats (#1338).
  A continuous palette (temperature, pressure, precipitation: linear stops)
  draws more than 256 colours, so `encode_png` falls back to 32-bit RGBA: a
  1024² temperature GetMap was 650–920 KB as PNG and 18–42 KB as
  `image/webp` at `QUALITY=80` in a production deployment, for +3–4 ms of
  encoding; precipitation with rain in view was 4–8× smaller. GetMap answers
  the `FORMAT` asked for (PNG when absent), so the server cannot choose: the
  advice lives in the README ("Choosing an image format") and in
  `webp_quality = 80` in the shipped GRIB/NetCDF/Zarr NWP examples. Do not
  change the server default: radar and discrete-class palettes stay lossless
  (PNG8, or WebP 100), because lossy coding blurs class edges and shifts
  pixels off the palette.

## Capabilities niceties

- `<Service>` follows the WMS 1.3.0 schema sequence: `Name`, `Title`,
  `Abstract`, the mandatory `OnlineResource` (`{base_url}/wms`), then the
  limits GetMap enforces, read from the constants that enforce them (#1012):
  `<LayerLimit>` = `params::LAYER_LIMIT` (5, #1181),
  `<MaxWidth>`/`<MaxHeight>` = `params::MAX_MAP_DIMENSION`. Change a limit
  there and capabilities follows; never hard-code the numbers in the XML.
- The root declares `xmlns:xsi`, `xmlns:sld` and an `xsi:schemaLocation`
  (`SCHEMA_LOCATION`) pairing the WMS namespace with WMS 1.3.0's
  `capabilities_1_3_0.xsd` and the SLD namespace with SLD 1.1.0's
  `sld_capabilities.xsd`. The document must validate against them: the
  OGC test suite's `validate-using-schemaLocation` does exactly that.
- `<Request>` is `GetCapabilities`, `GetMap`, `GetFeatureInfo` (its
  `<Format>`s are `params::INFO_FORMATS`), then `sld:GetLegendGraphic`.
  The WMS schema allows only `GetFeatureInfo` and `_ExtendedOperation`s
  after GetMap; SLD 1.1.0 declares GetLegendGraphic as one, in its own
  namespace (GeoServer and MapServer write it the same way). A
  `<GetLegendGraphic>` in the WMS namespace is invalid. A new operation
  that is not WMS's own needs its namespace and schema pair too.
- The collections' layers come in collection-id order, never the
  registry's HashMap order, which changes with every process (#1102). The
  OGC test suite tests the first or last named layer; `ci/wms-ets/config.toml`
  picks its ids for that.
- A change that fixes or deliberately breaks an ets-wms13 test updates
  `ci/wms-ets/expected-failures.txt` in the same PR. A listed test that
  passes is only a warning; an unlisted failure fails the WMS ETS job.
- `<Capability>` is `Request`, `Exception`, then the root `Layer`, the
  schema's sequence. `<Exception>` lists `EXCEPTION_FORMATS`, `XML` only:
  every request error is a ServiceExceptionReport, served as `text/xml`
  (WMS 1.3.0's MIME type for it; the 1.1.1 `application/vnd.ogc.se_xml`
  fails the suite), and `EXCEPTIONS` is ignored. Honouring
  `INIMAGE`/`BLANK` is open (#1027); add a format there only together with
  the code that serves it.
- `<LegendURL width height>` holds `<Format>` then `<OnlineResource>`. Its
  size is `params::legend_size(None, None)`, the function GetLegendGraphic
  sizes a legend with, so the advertised size is the size the link returns;
  never hard-code it.
- Collection `keywords` → `<KeywordList>` (after `<Abstract>`, WMS 1.3.0
  schema order); license → `<Attribution>` (after `<Dimension>` elements).
- ODIM per-site layers: `<Title>` is prefixed with the site place name via
  `RasterInfo.layer_subtitle` so flat clients can tell per-site layers apart.

## GetFeatureInfo (#1064)

`feature_info.rs`. The value is the collection's EDR position query at the
centre of pixel `(I, J)`, never a 1×1 `get_raster_tile` or a meta-tile
pixel: the same engine call answers EDR `position`, so the two report the
same value (the source value, sampled as the engine's position query does:
bilinear for GRIB, QueryData, Zarr and NetCDF, while a projected GetMap resamples
meta-tiles by nearest neighbour). An RGB composite answers each band's
value from one position query of its bands at the composite's resolved
scan. A layer whose engine `layer_picks_features` answers the features
`MapEngine::pick_features` returns for the pixel: CAP alerts, ODIM
`CELLS`, PostGIS events (#1100).

The value is written as EDR writes it: `Sample` carries the range's
`ds_core::model::RangeValue` (#1341), serialised in the JSON form and
displayed in text and HTML, so GetFeatureInfo and EDR `position` give
the same number byte for byte (`server/tests/wms_get_feature_info.rs`
compares them). Never unwrap it to `f64` on the way out.

- **Engines.** `WmsState::point_engines` holds the `EdrEngine` of every WMS
  collection whose engine answers position queries
  (`feature_info::answers_position`, `supported_query_types` containing
  `position`). The server registers it whatever the collection's `apis`
  list (`admin::register_point_engine`, the `derived_wrapped` EDR side for
  GRIB/QueryData/Zarr/NetCDF, so a derived wind speed, geopotential
  height, relative humidity or dew point is queryable); `edr` in
  `apis` only decides whether EDR routes are published. The capability is
  read once per load, never per request: `supported_query_types` builds a
  `Vec`.
- **Queryable.** `layer_is_queryable`: the engine picks the layer's features
  (`MapEngine::layer_picks_features`, O(1)), or the collection has a point
  engine and the layer shows its parameter's own values
  (`MapEngine::layer_shows_values`, O(1)). An RGB composite uses
  `composite_is_queryable`: a point engine, and every band shows values.
  Station symbols and nowcast layers are `queryable="0"` and answer
  `LayerNotQueryable`. GetCapabilities and the handler use the same
  functions, so the attribute and the answer cannot disagree. Never decide
  it with a probe query.
- **Composites and features (#1100).** A composite's bands are read at
  `ResolvedLayer::time`, the scan GetMap passes `get_raster_tiles`; `None`
  reads nothing and answers null. A feature layer gets a
  `ds_core::feature_pick::FeaturePick`: the pixel centre, the map's grid
  (`PickMap`) and pixel, from which the engine maps the pixel's ground area
  (`FeaturePick::footprint`, or `grown_ring` for symbols wider than a pixel;
  corners through `project_node`, continuous across 180°), the resolved time
  and run, the layer's `ELEVATION`, and `min(FEATURE_COUNT,
  MAX_FEATURE_COUNT = 50)`. The engine selects with its render's rule, on
  the query runtime. Never re-derive an engine's selection here.
- **Map part.** `validate_get_feature_info` runs `validate_get_map`, and the
  handler resolves it through `handlers::resolve_layers`, GetMap's own
  layer, style, dimension, time, run and content-version resolution. Never
  resolve a GetFeatureInfo dimension a second way: the value must belong to
  the frame GetMap draws and keys its caches on. `QUERY_LAYERS` must be
  layers of `LAYERS` (`LayerNotDefined`, Table E.1's "a layer not shown on
  the map"), each named once; a layer `LAYERS` lists twice is queried as
  its first listing draws it. Every query layer is checked queryable before
  any is read, and the answers come in `QUERY_LAYERS` order (#1181).
- **Point.** `OutputCrs::position_on_earth` at `((I+0.5)/WIDTH,
  (J+0.5)/HEIGHT)`: `project_node`, the mapping every engine renders with
  (EPSG:4326's BBOX is already lon/lat by then), for a point on the Earth,
  then `ds_core::geo::wrap_lon`. Never re-derive the CRS math. A projected
  map's point comes from the metres `OutputCrs::Projected` carries, never
  from `GetMapParams.bbox`, which is NaN for a BBOX wholly outside the
  projection. A centre off the Earth has no value and no engine call: the
  JSON `geometry` and `value` are null. That is every pixel of a map whose
  `domain_rows` is `None` (GetMap's empty image without a read, #1089), a
  centre past a pole, and a projected centre outside the projection's
  valid area, where its inverse is `None`: past the LAEA disk, and past a
  pole's northing or the eastings where TM's series stops being exact
  (`ds_core::geo::Crs::TransverseMercator`, #1113, #1130). `I`/`J` outside
  the image, negative or non-integer is
  `InvalidPoint`.
- **Dimensions.** The resolved time as `datetime` `(t, t)`, the resolved run
  as `reference_time`, and as `z` the level GetMap draws:
  `MapEngine::resolve_elevation(parameter, ELEVATION, time, run)`, with the
  `ELEVATION` the layer takes (`ResolvedLayer::elevation`), from the
  parameter, time and run GetMap passes `get_raster_tile`. Never pass
  `ELEVATION` or `vertical.levels[0]` as such: GRIB draws the run's own
  first level, and an ODIM volume the nearest sweep carrying the quantity,
  where the position query matches `z` to one sweep exactly. The response
  reports the time, the run and the level when the layer has them.
- **Execution.** `RenderJob::acquire_feature_info` (the render slots and
  queue GetMap uses, no memory reservation: one point) and `job.run_query`,
  a task on ds-executor's shared query runtime, the one api-edr runs its
  queries on (#1103). Several query layers share that one admission and
  one `run_query`, read in turn under the one deadline; the first failure
  other than no data fails the request and the rest are not read
  (#1181). The deadline is GetFeatureInfo's own,
  `MC_FEATURE_INFO_TIMEOUT_MS` (10 s), never the render deadline: a cold
  GRIB position query can need more than GetMap's 3 s. It starts at
  admission, so it bounds the slot wait and the query together; expiry is
  the 503. `tests/get_feature_info.rs`
  `get_feature_info_has_a_deadline_of_its_own` pins it in a child process.
  Never `job.run`: on a `spawn_blocking` worker of the serving runtime,
  GRIB's field jobs and the `DataStore` bridge take `Handle::current()` and
  put their blocking work back on request workers (root Critical Rule 7).
  `crates/server/tests/wms_get_feature_info.rs` serves its GRIB requests on
  a current-thread runtime, where that panics.
  `LocationNotFound` / `FeatureNotFound` / `ReferenceTimeNotFound` /
  `CollectionNotFound` is an empty answer (value null): no data there, or no
  longer any after a run eviction or a reload. `ResourceExhausted` /
  `DeadlineExceeded` 503; an engine `InvalidParameter` (and the other
  client-error variants) a generic `InvalidParameterValue`; anything else
  `Internal`, its detail only in the log (Critical Rule 11).
- **Formats.** `params::INFO_FORMATS` is the one list capabilities
  advertises and `InfoFormat::parse` accepts; an omitted `INFO_FORMAT` is
  `text/plain`. JSON is a GeoJSON FeatureCollection through `axum::Json`
  (serde, no new dependency): a Point feature per value read, one per band
  of a composite, and each picked feature with its id, own geometry and
  ordered properties (`layer` first); feature layers are also listed in the
  foreign member `layers` (time, `numberMatched`, `numberReturned`). Text
  writes a feature layer's fields, `matched:`/`returned:`, then `id:` and
  one line per property, line breaks as spaces. HTML is
  written with quick-xml like every other markup here, which escapes every
  label and value (`check_geo_safety.sh` rejects hand-built markup in this
  crate).
- **Caching.** `cache_control_value`, as GetMap: `immutable` only for an
  explicit `TIME` that resolved, with `content_version` 0, and only for an
  answer: every value and band non-null, or at least one feature. An empty
  answer always revalidates: GeoTIFF and GRIB answer a failed
  read with a null sample, and GetMap never pins a failed render either. No
  server-side cache: one point read is cheaper than a cache entry.

GetMap cache misses use `ds-executor::RenderJob`: a bounded shared queue and
one absolute deadline across admission/render/encoding. A list of N layers
is admitted with `acquire_layered_raster`: one slot, one layer's memory plus
the image below, and N render deadlines (#1181). The worker retains
CPU and memory permits after HTTP timeout/disconnect. Propagate
`DeadlineExceeded` as 503 + Retry-After, never a successful transparent/error
image. Meta-tile fan-out checks the same deadline between tiles.

Every served GetMap carries a `ds_executor::RenderTiming` (timed from the
rendered-cache lookup): `hit`, `assembled` (meta-tiles all cached) or `cold`.
A layer list is labelled by its bottom layer's collection and is
`assembled` only when every layer was; drawing layers over each other
counts as `assemble`.
An error tile carries none. Maps and Tiles report `hit`/`cold` the same way.
Its render phases (#147) reuse the slow-render log's measurements, never a
second timer: `sem_wait` is `queue`; a meta-tiled view reports
`MetaTileStats.engine` (uncached tiles only) as `engine`, `assemble`, and
`colorize + encode` as `encode`, matching the direct path's `render_tile`.
An all-nodata view skips assemble/encode; a hit reports no phase.
