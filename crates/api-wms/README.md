# api-wms

OGC Web Map Service (WMS) 1.3.0 for MeteoCore: map images, point values and
legends for every collection that lists `wms` in its `apis`.

## What it solves

QGIS, OpenLayers, Leaflet and many operational display systems read maps
over WMS 1.3.0, not OGC API. This crate serves the same engines, styles and
caches as OGC API - Maps and Tiles behind one WMS endpoint, and handles what
WMS adds: CRS-dependent axis order, XML capabilities and exceptions, the
`TIME`, `ELEVATION` and model-run dimensions, and case-insensitive parameter
names.

## How it works

- One route, `GET /wms` (`/wms/` works too), dispatching on `REQUEST`.
- The server registers each `wms` collection's `MapEngine` in `WmsState`,
  and its `EdrEngine` when the engine answers EDR position queries (for
  GetFeatureInfo). A reload swaps the state atomically. The crate depends on
  `ds-core`, `ds-render` and `ds-executor`, never on an engine crate.
- GetMap resolves each layer's style, timestep, model run and level before
  it builds a cache key, so the key names the frame the engine draws. Then:
  rendered-image cache lookup, admission (a render slot and output memory
  from `ds-executor`), the render on a blocking worker, one encode.
- EPSG:3857, EPSG:3067 and EPSG:3035 maps are assembled from cached
  256 × 256 meta-tiles on internal tile grids, resampled by nearest
  neighbour so discrete palettes stay exact. CRS:84 and EPSG:4326 maps, and
  views that would need too many tiles or zoom past the grids, render
  directly.
- Styles resolve through `ds-render`, as in Maps and Tiles, so a layer looks
  the same in all three.
- All XML and the HTML GetFeatureInfo are written with `quick-xml`, which
  escapes every value.

Contributor rules and gotchas live in [CLAUDE.md](CLAUDE.md).

## Quick start

With the server on port 8000:

```
http://localhost:8000/wms?SERVICE=WMS&REQUEST=GetCapabilities
http://localhost:8000/wms?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=radar&STYLES=&CRS=CRS:84&BBOX=19,59,32,71&WIDTH=512&HEIGHT=512&FORMAT=image/png&TRANSPARENT=TRUE
```

In QGIS, add a WMS/WMTS connection with the URL `http://localhost:8000/wms`.

## Requests

| `REQUEST` | Answers |
|---|---|
| `GetCapabilities` | The capabilities document, `text/xml` |
| `GetMap` | A PNG, JPEG or WebP image of up to five layers |
| `GetFeatureInfo` | Each queried layer's value at a pixel: text, GeoJSON or HTML |
| `GetLegendGraphic` | A legend image, or the legend as JSON |

A missing `REQUEST` is `MissingParameterValue`, any other value
`OperationNotSupported`. GetCapabilities answers any `VERSION`, or none,
with the 1.3.0 document. GetMap and GetFeatureInfo require `VERSION=1.3.0`.

**Capabilities.** The root's `xsi:schemaLocation` names WMS 1.3.0's
`capabilities_1_3_0.xsd` and SLD 1.1.0's `sld_capabilities.xsd`, so a
validator finds both: GetLegendGraphic is listed as
`sld:GetLegendGraphic`, an SLD extended operation. `<Service>` advertises
`LayerLimit` 5 and `MaxWidth` / `MaxHeight` 8000, the limits GetMap
enforces. `<Exception>` offers `XML` only. Layers come in collection-id
order. A collection's layer carries the five CRSs, an
`EX_GeographicBoundingBox` and a `CRS:84` `BoundingBox` when its extent is
known, its dimensions, a `<KeywordList>` from `keywords` and an
`<Attribution>` from `[collections.license]`. Each named layer lists its
styles, each with a PNG `LegendURL` of 180 × 300 pixels. Absolute links
use the `BASE_URL` environment variable, else `[server] base_url`, else
`http://{host}:{port}`; with `[server] trust_proxy_headers = true` they
follow the request's forwarding headers.

### Layers

| Layer name | Draws |
|---|---|
| `{collection}` | A single-parameter collection |
| `{collection}/{parameter}` | One parameter of a multi-parameter collection |
| `{collection}/{composite}` | An RGB composite of a satellite collection |

A multi-parameter collection, or one with composites, is a parent layer
without a `<Name>`: a child layer per parameter, then one per composite.
Children inherit the parent's CRSs, extent and dimensions; a parameter with
a time axis of its own declares its own `time`. `LAYERS={collection}` of a
multi-parameter collection is accepted too and draws its default parameter
with that parameter's style. An unknown parameter is `LayerNotDefined`.

### Dimensions

| Dimension | Parameter | Advertised |
|---|---|---|
| `time` | `TIME` | ISO 8601 values, `nearestValue="1"`; default the latest, or one the engine sets (CAP: now, as of its latest rebuild) |
| `elevation` | `ELEVATION` | The vertical levels in their unit, `nearestValue="1"`; default the first |
| `reference_time` | `DIM_REFERENCE_TIME` | A forecast layer's model runs; default the latest; exact match |

A `time` or `reference_time` axis of more than 500 values is written in
Annex C form: each run of three or more evenly spaced values as one
`min/max/resolution` range, every other value as itself.

## Parameter names

Names are case-insensitive and percent-decoded: `REQUEST`, `request` and
`ReQuEsT` are one parameter. Values are matched exactly, so layer and style
names are case-sensitive; `TRANSPARENT`, GetLegendGraphic's JSON `FORMAT`
and `INFO_FORMAT` are matched in any case. A parameter given more than once
takes its first value. Parameters the server does not use are ignored,
`SERVICE`, `EXCEPTIONS`, `SLD`, `SLD_BODY` and `UPDATESEQUENCE` among them.

## GetMap

| Parameter | Required | Values |
|---|---|---|
| `VERSION` | yes | `1.3.0`. Missing: `MissingParameterValue`; other: `InvalidParameterValue` |
| `LAYERS` | yes | Up to five layer names, drawn in list order, the first at the bottom ([Several layers](#several-layers)) |
| `STYLES` | no | One style per layer by position, empty for its default: `STYLES=a,,b`. Empty or omitted gives every layer its default; a single layer takes the whole value. Another count is `InvalidParameterValue`. `STYLE` is read when `STYLES` is absent |
| `CRS` | yes | `CRS:84`, `EPSG:4326`, `EPSG:3857`, `EPSG:3067`, `EPSG:3035`; anything else `InvalidCRS` |
| `BBOX` | yes | Four finite numbers, min below max, in the CRS's axis order ([CRS and BBOX](#crs-and-bbox)) |
| `WIDTH`, `HEIGHT` | yes | 1 to 8000 each |
| `FORMAT` | no | `image/png` (default), `image/jpeg`, `image/webp`; anything else `InvalidFormat` |
| `TRANSPARENT` | no | `FALSE` (default): an opaque image, nodata in `BGCOLOR`. `TRUE`: nodata stays transparent in PNG and WebP. An overlay needs `TRUE` |
| `BGCOLOR` | no | `0xRRGGBB`, default `0xFFFFFF`: the background for `TRANSPARENT=FALSE` and for JPEG |
| `TIME` | no | One instant: RFC 3339, or `YYYY-MM-DDThh:mm[:ss]` as UTC. Each layer's engine resolves it to the timestep it draws (GeoTIFF: the latest not after it). Unparseable: `InvalidDimensionValue` |
| `ELEVATION` | no | One finite level; a list or another value is `InvalidDimensionValue` |
| `DIM_REFERENCE_TIME` | no | A model run, RFC 3339 or an EDR instance id. A run the layer does not have is `InvalidDimensionValue` |
| `QUALITY` | no | Vendor: 1 to 100 for `image/jpeg` (default 85) and `image/webp` (1–99 lossy, 100 lossless; default the collection's `[wms] webp_quality`, else lossless). With PNG, or out of range: `InvalidParameterValue` |

PNG is written 8-bit indexed when the image has at most 256 colours, else
RGBA. A view with no data is the empty image (`x-cache: EMPTY`): `BGCOLOR`,
or transparent with `TRANSPARENT=TRUE` in PNG or WebP. A render that fails
in the engine is a red image with HTTP 200 (`x-cache: ERROR`), its cause
only in the server log. Both come in the requested `FORMAT`.

### Several layers

A GetMap may name up to five layers, `LAYERS=radar,warnings`. Each layer is
drawn as a GetMap of it alone draws it, with its own style, and the layers
are drawn over each other in list order, the first at the bottom: a layer's
nodata and translucent colours show the layers below. `FORMAT`,
`TRANSPARENT` and `BGCOLOR` apply once, to the combined image. Any layer
kind can be listed: parameters, RGB composites, CAP warnings, nowcast,
lightning events, radar volumes.

- **Checks.** Every name and style is checked first, so an unknown one is
  `LayerNotDefined` / `StyleNotDefined`; a list of more than five served
  layers is then `InvalidParameterValue`.
- **Dimensions.** `TIME`, `ELEVATION` and `DIM_REFERENCE_TIME` apply to the
  listed layers that have that dimension; the others ignore it. A value a
  layer cannot take is an error, as for one layer. When no listed layer
  has the dimension, `ELEVATION` and `DIM_REFERENCE_TIME` are
  `InvalidDimensionValue`, and `TIME` goes to every layer. Without `TIME`
  each layer draws its own default.
- **Failures.** A layer that fails fails the map: the error image, or 503.
- **Rendering.** The layers render one after another in one render slot.
  The deadline is the render deadline once per layer, counted from when
  the request starts waiting for admission. The memory reserved is one
  layer's, plus 4 B per pixel for the image of the layers below.
- **Caching.** Each layer reads and fills its own meta-tiles, the ones a
  GetMap of that layer alone uses. The combined image is never in the
  rendered-image cache, and its `Cache-Control` is `immutable` only when
  every layer draws an explicit `TIME` over content its engine never
  revises in place.
- **WebP quality.** Without `QUALITY`, WebP is lossy only when every listed
  collection sets `[wms] webp_quality`, at the highest of them.

## CRS and BBOX

| CRS | `BBOX` order | Grid |
|---|---|---|
| `CRS:84` | west, south, east, north | WGS84, longitude first |
| `EPSG:4326` | **south, west, north, east** | WGS84, latitude first |
| `EPSG:3857` | minx, miny, maxx, maxy | Web Mercator metres |
| `EPSG:3067` | minx, miny, maxx, maxy | ETRS-TM35FIN metres |
| `EPSG:3035` | minx, miny, maxx, maxy | ETRS89-LAEA Europe metres |

WMS 1.3.0 follows each CRS's own axis order, so `EPSG:4326` is
latitude-first. When in doubt, use `CRS:84`.

- A `CRS:84` or `EPSG:4326` box may reach past a pole. Rows whose centres
  lie past ±90° stay empty, and a box wholly past a pole is the empty
  image. No engine is asked for those rows.
- An `EPSG:3067` or `EPSG:3035` box wholly outside the projection's valid
  area is the empty image, and no engine is asked. For `EPSG:3035` that
  area is the LAEA disk, about 12 742 km in radius around
  (4 321 000, 3 210 000). For `EPSG:3067` it reaches the poles but only
  50° of longitude either side of the central meridian, 27°E: the
  Transverse Mercator series MeteoCore uses is not reliable further out.
  `EPSG:3067` maps therefore show nothing west of 23°W or east of 77°E.
- A box partly outside renders what lies inside; its pixels past the area
  stay empty.
- An `EPSG:3035` box whose edges miss the disk inside it (wider than the
  disk, or crossing it only between its edge samples) has no read window
  and is `InvalidParameterValue`, as is a malformed box.

## GetFeatureInfo

The value of each queried layer at a pixel of the map, for QGIS's identify
tool or a click in a web map:

```
http://localhost:8000/wms?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetFeatureInfo&LAYERS=radar&QUERY_LAYERS=radar&STYLES=&CRS=CRS:84&BBOX=10,67,12,69&WIDTH=256&HEIGHT=256&I=137&J=140&INFO_FORMAT=application/json
```

The value is the collection's EDR position query at the centre of pixel
(`I`, `J`), so GetFeatureInfo and `/edr/collections/{id}/position` answer
the same value at the same point, time, run and level. It is the source
value as the engine samples it (bilinear for GRIB, QueryData, Zarr and
NetCDF), not the pixel's colour: a projected GetMap is assembled by nearest
neighbour.

| Parameter | Required | Values |
|---|---|---|
| GetMap's | yes | The map: `VERSION`, `LAYERS`, `STYLES`, `CRS`, `BBOX`, `WIDTH`, `HEIGHT` and the dimensions, validated as GetMap validates them, `FORMAT`, `TRANSPARENT`, `BGCOLOR` and `QUALITY` included |
| `QUERY_LAYERS` | yes | Layers of `LAYERS`, answered in this order, each once. One `LAYERS` does not name is `LayerNotDefined`; one that is not queryable is `LayerNotQueryable`, checked before any layer is read |
| `I`, `J` | yes | The pixel's column (0 to `WIDTH` − 1) and row (0 at the top). Anything else, negative or non-integer included, is `InvalidPoint` |
| `INFO_FORMAT` | no | `text/plain` (default), `application/json`, `text/html`; anything else `InvalidFormat` |
| `FEATURE_COUNT` | no | A positive integer, default 1. A layer has one value at a point, so any count returns it |

Each layer is read at the time, model run and level GetMap draws it at: an
omitted `TIME` is the layer's default, and a `TIME` between timesteps is the
timestep GetMap draws for it. The level is the one the engine draws, which
is not always `ELEVATION` or the first advertised level (GRIB without
`ELEVATION` draws the run's own first level; an ODIM volume the nearest
sweep that carries the quantity). The answer reports the resolved time, and
the run and level when the layer has them.

`application/json` is a GeoJSON FeatureCollection, one Point per queried
layer. Over the configuration example below, that request answers:

```json
{
  "type": "FeatureCollection",
  "features": [{
    "type": "Feature",
    "geometry": {"type": "Point", "coordinates": [11.07421875, 67.90234375]},
    "properties": {
      "layer": "radar",
      "parameter": "reflectivity",
      "value": 40.5,
      "unit": "dBZ",
      "time": "2026-03-24T23:35:00Z"
    }
  }]
}
```

`reference_time` and `elevation` join the properties when the layer has
them. `text/plain` writes `layer:` and then one `name: value` line per field,
`lon` and `lat` included; `text/html` is one escaped table per layer.

- **No data is an answer.** A nodata pixel, or a point outside the data, is
  JSON `value: null` and `nodata` in the text formats. A pixel centre that
  is not on the Earth (past a pole, or outside an `EPSG:3067` / `EPSG:3035`
  valid area) also has a null `geometry`, and no engine is asked.
- **Queryable layers.** A layer is `queryable="1"` when its collection's
  engine answers EDR position queries for its parameter, whether or not the
  collection lists `edr`. RGB composites, the ODIM per-site `CELLS`
  overlay, CAP, PostGIS events (lightning) and nowcast layers are
  `queryable="0"` and answer `LayerNotQueryable`.
- **Execution.** The queries wait in GetMap's render queue and run on the
  query runtime EDR uses, one layer after another, under one deadline of
  their own, `MC_FEATURE_INFO_TIMEOUT_MS` (10 s), which also covers the
  wait for a render slot. Overload or expiry is 503 with `Retry-After: 1`.
  An engine failure is HTTP 500 with a generic message, its detail only in
  the server log, and the layers after it are not read.

## GetLegendGraphic

| Parameter | Required | Values |
|---|---|---|
| `LAYER` | yes | A layer name; `LAYERS` is read first when both are given |
| `STYLE` | no | Default `default`; `STYLES` is read first when both are given |
| `FORMAT` | no | `image/png` (default), `image/jpeg`, `image/webp`, or `application/json` for the palette stops and value range |
| `WIDTH`, `HEIGHT` | no | Default 180 × 300; clamped to 1–512 × 1–1024 |

The legend resolves its style as GetMap does, so a `{collection}/{parameter}`
layer gets that parameter's palette. An RGB composite's legend lists its
three channels instead of a colour bar. `QUALITY` is ignored: a JPEG legend
is encoded at quality 85, a WebP legend lossless.

## Exceptions

A request error is a WMS 1.3.0 `ServiceExceptionReport` served as
`text/xml`, with HTTP 400:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<ServiceExceptionReport version="1.3.0" xmlns="http://www.opengis.net/ogc">
  <ServiceException code="LayerNotDefined">Layer 'nonexistent' does not exist</ServiceException>
</ServiceExceptionReport>
```

| Code | When |
|---|---|
| `LayerNotDefined` | Unknown layer or parameter; a `QUERY_LAYERS` layer not in `LAYERS` |
| `StyleNotDefined` | A style the layer does not offer |
| `InvalidCRS` | A CRS other than the five above |
| `InvalidFormat` | Unsupported `FORMAT` or `INFO_FORMAT` |
| `InvalidDimensionValue` | Malformed `TIME`, `ELEVATION` or `DIM_REFERENCE_TIME`; an `ELEVATION` or `DIM_REFERENCE_TIME` no listed layer has; a run the layer does not have |
| `LayerNotQueryable` | GetFeatureInfo on a layer that is not queryable |
| `InvalidPoint` | GetFeatureInfo `I` or `J` outside the image |
| `MissingParameterValue` | A required parameter is missing |
| `InvalidParameterValue` | Any other invalid value: `VERSION`, size, `BBOX`, `STYLES` count, `TRANSPARENT`, `BGCOLOR`, `QUALITY`, `FEATURE_COUNT`, more than five layers |
| `OperationNotSupported` | An unknown `REQUEST` |

An internal error is HTTP 500 with code `Internal` and a generic message.
An overloaded or timed-out request is HTTP 503 with code `Internal`,
"Server busy, try again later" and `Retry-After: 1`. The detail of both is
only in the server log.

## Deviations from WMS 1.3.0

- `SERVICE` is not checked. GetMap defaults an omitted `FORMAT` to PNG and
  an omitted `STYLES` to every layer's default; GetFeatureInfo defaults an
  omitted `INFO_FORMAT` to `text/plain`.
- `EXCEPTIONS` is ignored: errors are always the XML report. `INIMAGE` and
  `BLANK` are not offered.
- A GetMap whose render fails in the engine returns the red error image
  with HTTP 200, not an exception.
- `TIME` and `ELEVATION` take one value: no lists, intervals or `current`.
- Styles come from the server's configuration only: no `SLD` or `SLD_BODY`.
- `EPSG:3067` covers only 50° of longitude either side of 27°E.
- Vendor additions: `QUALITY`, the `reference_time` dimension
  (`DIM_REFERENCE_TIME`) and GetLegendGraphic `FORMAT=application/json`.

## Caching

- **Rendered images.** A single layer's encoded GetMap image, keyed on the
  resolved style, format and quality, CRS, box, size, time, parameter,
  level, run, content version and background. One server-wide cache, shared
  with Maps and Tiles, without TTL; a reload evicts a changed collection's
  entries. Its size is `rendered_cache_mb` in `[collections.wms]`, default
  256 (MB), read from one collection's block: a block that omits it counts
  as 256, so set the same value in every `[collections.wms]` block. Empty
  and error images and multi-layer images are never cached.
- **Meta-tiles.** Decoded 256 × 256 RGBA tiles for the projected CRSs,
  without background, so opaque and transparent views share them.
  `[server] metatile_cache_mb`, default 1024; `0` turns meta-tiling off and
  every map renders directly. Only WMS uses it.
- **HTTP.** GetMap carries a content-derived `ETag` and answers a matching
  `If-None-Match` with 304. `x-cache` tells `HIT`, `MISS`, `EMPTY` or
  `ERROR`.

| Response | `Cache-Control` |
|---|---|
| GetMap with a `TIME` that resolved, on content never revised in place | `public, max-age=86400, immutable` |
| GetMap otherwise (no `TIME`, CAP alerts, the error image) | `public, max-age=60, must-revalidate` |
| GetFeatureInfo | As GetMap, and a null value always revalidates |
| GetLegendGraphic | `public, max-age=86400` |

## Limits and admission

| Limit | Value |
|---|---|
| Image size | `WIDTH` and `HEIGHT` ≤ 8000, `WIDTH × HEIGHT` ≤ 64 000 000 |
| Layers per GetMap or GetFeatureInfo | 5 |
| Render slots | `[server] render_concurrency`: default 2× CPUs, at least 8; shared with Maps, Tiles and 3D Tiles |
| Waiting queue | `MC_RENDER_QUEUE_CAPACITY`: default 3× the render slots |
| Render memory | `MC_RENDER_MEMORY_MB`, default 1024: 32 B per output pixel, 16 B more per further band of an RGB composite |
| Render deadline | `MC_RENDER_TIMEOUT_MS`, default 3000, once per layer; it covers the wait for admission |
| GetFeatureInfo deadline | `MC_FEATURE_INFO_TIMEOUT_MS`, default 10000; it covers the wait for a slot |

Cache hits skip admission. A full queue, an expired deadline or a request
larger than the whole memory budget is 503 with `Retry-After: 1`. At the
default budget a map over 33 554 432 pixels (about 5793 × 5793) is a 503
on every retry; see
[Sizing render memory](../../README.md#sizing-render-memory-on-larger-hosts).

## Configuration

A collection is served when its engine draws maps and `apis` lists `wms`:
`geotiff`, `grib`, `querydata`, `zarr`, `netcdf`, `satellite`, `odim`,
`odim-volume`, `cap`, `nowcast`, and `postgis` with the events shape.

```toml
[[collections]]
id = "radar"
title = "Radar reflectivity"
description = "Five radar frames five minutes apart"
data_path = "testdata/radar"
engine_type = "geotiff"
apis = ["wms"]

[collections.geotiff]
filename_template = "radar_%Y%m%dT%H%MZ.tif"
parameter = "reflectivity"
unit = "dBZ"

[collections.wms]
colormap = "radar_dbz"     # the default style
# min = 0.0                # override the palette's value range
# max = 70.0
# webp_quality = 80        # 1-100; unset = lossless WebP

[[collections.wms.styles]]
name = "grayscale"         # STYLES=grayscale
colormap = "grayscale"
min = 0.0
max = 70.0
```

`[collections.wms]` is optional. Its other fields are `color_stops`,
`[[collections.wms.parameters]]` (per-parameter defaults), `style_bundle`,
`parameter_defaults = false` (turns off the built-in per-parameter styles)
and `rendered_cache_mb`. Without a colormap a layer gets a generic fallback
palette, though parameter layers may match the built-in per-parameter
defaults. Palettes, bundles and style rules are
documented in the root README's
[Styling](../../README.md#styling-shared-with-maps-and-tiles) section.

Most configs in `collections.d/` list `wms`. `ci/wms-ets/config.toml`
serves only fixtures committed to the repository and runs from a fresh
checkout:

```bash
cargo run -p server -- --config ci/wms-ets/config.toml
```

## Testing

```bash
cargo test -p api-wms
cargo test -p server --test wms_get_feature_info
```

The crate's tests run the router against mock engines: parameter parsing,
capabilities, exceptions, `TRANSPARENT`/`BGCOLOR` pixels, caching and
meta-tiling, model runs, legends, several layers, GetFeatureInfo and derived
wind speed. The server test checks GetFeatureInfo against EDR position
answers on committed fixtures: GeoTIFF radar frames, a GRIB wind forecast
and two CF-NetCDF forecast samples, one of them on pressure levels.

## OGC test suite

The `WMS ETS` workflow (`.github/workflows/wms-ets.yml`) runs OGC's WMS
1.3.0 executable test suite,
[ets-wms13](https://github.com/opengeospatial/ets-wms13) 1.34, under TEAM
Engine 6.0.0-RC2 from the suite's published container, pinned by digest.
It runs weekly, on demand, and on pull requests that change this crate, the
workflow, `ci/wms-ets/` or the fixtures that config serves. It is not a
required check.

**The server.** A debug build on `ci/wms-ets/config.toml`, with the render
and GetFeatureInfo deadlines raised to 60 s and the render memory to
2560 MiB, which admits the advertised 8000 × 8000 maximum. Overflow checks
and debug assertions are on, so reproduce a 500 or a dropped connection on
a release build before filing it. Its layers:

| Layer | Covers |
|---|---|
| `ets-geotiff` | A plain raster: one GeoTIFF on an EPSG:3067 grid |
| `ets-grib-pressure` | An elevation axis (GRIB pressure levels) and a model-run axis, one value each |
| `ets-multi/<param>` | A multi-parameter GRIB forecast (ARPEGE 10 m wind): a parent layer with a child per parameter, one model run, an `<Attribution>` |
| `ets-netcdf-levels/<param>` | MEPS CF-NetCDF: four pressure levels and two model runs declared on the parent layer; air temperature, the x/y wind components and the wind speed derived from them |
| `ets-radar` | A time axis: five radar frames |

Every fixture layer is `queryable="1"`: each engine answers EDR position
queries, though no fixture collection lists `edr`.

**Layer order.** GetCapabilities lists layers in collection-id order, and
several tests take the first or last named layer, so the ids decide which
layers are tested. Another fixture collection takes an id that sorts after
`ets-multi` and before `ets-radar`; one with child layers must sort after
`ets-multi`, because `getmap:bbox-inherited` and the multi-style tests take
the first child layers. The first named layer stays `ets-geotiff`, a
top-level layer with no data at 0°N 0°E:

- `getmap:transparent-default` asks for a box around 0°N 0°E without
  `TRANSPARENT`. Over no data it tests the default itself, `FALSE`: the
  empty image is opaque white.
- On a child layer, `getmap:layerlimit` and
  `getmap:transparent-opaque-layer` fail on the suite's own requests: the
  first encodes the `/` in the name twice, the second sends an empty `CRS`
  for a layer that inherits its `BoundingBox`.

**Conformance classes.** The run requests the `basic` and `queryable`
classes; the data-independent tests always run. The `time`,
`raster_elevation` and `vector_elevation` classes test only Blue Lake
layers (`cite:Autos`, `cite:Terrain`, `cite:Lakes` and others), so they
stay off, as does the recommendation class. The fixtures' time, level and
run axes are still tested: the capabilities tests validate their
`<Dimension>` declarations, and `getmap:each-layer` and
`getfeatureinfo:each-queryable-layer` request every layer at its default
time, level and run.

**Expected failures.** `ci/wms-ets/expected-failures.txt` lists the tests
expected to fail, one `prefix:local-name` per line, grouped by cause. Add a
line only for a failure that is a deliberate decision or needs a dataset
the fixtures do not serve; fix everything else. The one cause listed:

- **OGC's Blue Lake test dataset** (the `cite:*` layers), which MeteoCore
  does not serve. It is needed by the `main:data-preconditions` class and
  by `basic:bbox-exponential`, `bbox-pixel-interpretation`, `no-bgcolor`,
  `blue-bgcolor` and `transparent-true`, which test Blue Lake layers.
  `queryable:std-data-queryable` looks for the `BasicPolygons`, `Forests`,
  `Lakes` and `NamedPlaces` layers, and `queryable:feature_count-default`
  and `feature_count-1` query a Blue Lake layer, so with none served they
  send empty `LAYERS` and `QUERY_LAYERS`.

**Verdict.** `ci/wms-ets/summarize.py` applies the list; its tests,
`test_summarize.py`, run first in the job. The job fails when:

- a test fails that is not on the list (an error annotation names it);
- the suite could not run: the server or TEAM Engine did not start, the
  server died during the run, the run returned no report, or no
  conformance class ran (the suite could not read the capabilities);
- the list cannot be read, repeats a name or holds a line that is not a
  test name.

A listed test that did not fail is a warning, in the summary and as an
annotation: remove it from the list.

**Report.** The job summary opens with the test counts and the verdict,
then a table of outcomes per conformance class and each failure with its
message and request. Failures in the Blue Lake classes
(`data-preconditions`, `basic`, `queryable`) are collapsed to one line
each, except `basic:gif-or-png`,
`basic:options-requirements` and `queryable:getfeatureinfo-supported`,
which read MeteoCore's own capabilities and are listed in full. The
`wms-ets-report` artifact holds TEAM Engine's CTL log, the session with
every test's log and the EARL report (`teamengine-session.tar.gz`), the
server and TEAM Engine logs, the capabilities document as served at
startup, and the summary as Markdown and JSON.
