# api-wms — OGC WMS 1.3.0 for Metocean Data Server

This crate implements an OGC Web Map Service (WMS) 1.3.0 endpoint for serving raster data as map images. It renders GeoTIFF collections as colorized PNG tiles suitable for display in QGIS, Leaflet, OpenLayers, and other WMS-capable clients.

## Quick Start

1. Add `"wms"` to a GeoTIFF collection's `apis` array in `config.toml`:

```toml
[[collections]]
id = "radar"
title = "Weather Radar"
description = "Radar reflectivity composite"
data_path = "testdata/radar"
engine_type = "geotiff"
apis = ["edr", "wms"]

[collections.geotiff]
filename_template = "radar_%Y%m%dT%H%MZ.tif"
parameter = "reflectivity"
unit = "dBZ"

[collections.wms]
colormap = "radar_dbz"
```

2. Start the server:

```bash
cargo run -p server
```

3. Access the WMS:

```
# GetCapabilities (XML)
http://localhost:8000/wms/?SERVICE=WMS&REQUEST=GetCapabilities

# GetMap (PNG image)
http://localhost:8000/wms/?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=radar&CRS=CRS:84&BBOX=19,59,32,71&WIDTH=256&HEIGHT=256&FORMAT=image/png
```

## Client Configuration

### QGIS

1. Layer > Add Layer > Add WMS/WMTS Layer
2. Click "New" to create a connection
3. Name: `Metocean Data Server`
4. URL: `http://localhost:8000/wms/`
5. Click "Connect", select layers, click "Add"

### Leaflet

```javascript
L.tileLayer.wms("http://localhost:8000/wms/", {
    layers: "radar",
    format: "image/png",
    transparent: true,
    crs: L.CRS.EPSG4326
}).addTo(map);
```

### OpenLayers

```javascript
new ol.layer.Tile({
    source: new ol.source.TileWMS({
        url: "http://localhost:8000/wms/",
        params: {
            LAYERS: "radar",
            FORMAT: "image/png",
            TRANSPARENT: true
        }
    })
});
```

## Supported Operations

| Operation | Description |
|-----------|-------------|
| `GetCapabilities` | Returns XML document listing available layers, CRS, extents, time dimension, and styles with their legends; `<Service>` advertises the GetMap limits (`LayerLimit`, `MaxWidth`, `MaxHeight`) |
| `GetMap` | Returns a rendered map image (PNG, JPEG or WebP) of up to five layers drawn over each other ([Several layers](#several-layers)) |
| `GetFeatureInfo` | Returns each queried layer's value at a pixel of the map, as plain text, GeoJSON or HTML ([GetFeatureInfo](#getfeatureinfo)) |
| `GetLegendGraphic` | Returns a legend image showing the colormap scale |

The capabilities document names its schemas and validates against them: the
root's `xsi:schemaLocation` points at the WMS 1.3.0 `capabilities_1_3_0.xsd`
and at the SLD 1.1.0 `sld_capabilities.xsd`. `<Request>` lists
`GetCapabilities`, `GetMap`, `GetFeatureInfo` and `sld:GetLegendGraphic`:
GetLegendGraphic is an SLD operation, which the WMS schema admits only as an
extended operation in SLD's namespace. `<Capability>` lists `<Request>`, then `<Exception>`,
which offers `XML` only (see [Error Responses](#error-responses)), then the
layers. Each style's `<LegendURL>` names the legend's `<Format>`,
`image/png`, and carries its `width` and `height`: the size its link
returns, GetLegendGraphic's default of 180 × 300 pixels.

## Parameter Names

Parameter names are case-insensitive in every request, as WMS 1.3.0 §6.8.1
requires: `REQUEST`, `request` and `ReQuEsT` are one parameter. Values are
kept as sent and matched exactly, so layer and style names are
case-sensitive; `TRANSPARENT`, the JSON legend `FORMAT` and GetFeatureInfo's
`INFO_FORMAT` are the exceptions, matched case-insensitively. Names and values are percent-decoded
first, so `ReQuEsT=%47%65%74%4D%61%70` is a GetMap. A parameter the server
does not read is ignored, and a parameter given more than once, in any mix
of cases, takes its first value: `LAYERS=a&layers=b` requests layer `a`.

## GetMap Parameters

| Parameter | Required | Description |
|-----------|----------|-------------|
| `SERVICE` | yes | Must be `WMS` |
| `VERSION` | yes | Must be `1.3.0`. Without it the request is `MissingParameterValue`, as WMS 1.3.0 requires: only GetCapabilities negotiates the version |
| `REQUEST` | yes | `GetMap` |
| `LAYERS` | yes | Up to five comma-separated layer names (`LayerLimit` 5), drawn in list order, the first at the bottom ([Several layers](#several-layers)). A layer name is a collection ID, or `collection/parameter` for one parameter of a multi-parameter collection. A multi-parameter collection's own layer is not advertised, but `LAYERS=collection` is accepted and draws its default parameter with that parameter's style: the image and legend `collection/<default parameter>` gives. Every name and style is checked first, so a list naming a layer or style the server does not serve answers `LayerNotDefined` / `StyleNotDefined`, and a list of more than five served layers `InvalidParameterValue` |
| `CRS` | yes | Coordinate reference system (see below) |
| `BBOX` | yes | Bounding box (axis order depends on CRS) |
| `WIDTH` | yes | Output image width in pixels (1–8000) |
| `HEIGHT` | yes | Output image height in pixels (1–8000) |
| `FORMAT` | yes | `image/png`, `image/jpeg` or `image/webp` |
| `TRANSPARENT` | no | `FALSE` (default, as WMS 1.3.0 specifies) or `TRUE`. `FALSE` returns an opaque image with nodata painted in `BGCOLOR`; `TRUE` keeps nodata transparent in PNG and WebP. A map overlaid on others needs `TRUE`, which OpenLayers' WMS sources send unless told otherwise. With several layers it applies once, to the image they make together |
| `BGCOLOR` | no | `0xRRGGBB` background for `TRANSPARENT=FALSE` and JPEG output (default `0xFFFFFF`) |
| `QUALITY` | no | Vendor parameter: encoder quality 1–100 for `image/webp` (1–99 lossy, 100 lossless; default the collection's `[wms] webp_quality`, else lossless) and `image/jpeg` (default 85). With `image/png` or out of range: `InvalidParameterValue` |
| `TIME` | no | ISO 8601 timestamp. Defaults to latest available, each layer's own. With several layers it applies to the layers with a time dimension ([Several layers](#several-layers)) |
| `ELEVATION` | no | One vertical level, for a layer with an elevation dimension; `InvalidDimensionValue` when no listed layer has one |
| `DIM_REFERENCE_TIME` | no | A model run of a forecast layer; defaults to its latest. `InvalidDimensionValue` for a run the layer does not have, or when no listed layer has runs |
| `STYLES` | no | One style per layer, by position, each empty for that layer's default: `STYLES=a,,b`. An empty `STYLES`, or one of commas only, gives every layer its default; another number of values is `InvalidParameterValue`. A single layer takes the whole value as its style |

### Several layers

A GetMap may name up to five layers, the `<LayerLimit>5</LayerLimit>` its
capabilities advertise, as `LAYERS=radar,warnings` (#1181). Each layer is
drawn as a GetMap of that layer alone draws it, with its own style, and the
layers are drawn over each other in list order, the first at the bottom
(WMS 1.3.0 §7.3.3.3): a layer's nodata, and its translucent colours, show
the layers below it. `FORMAT`, `TRANSPARENT` and `BGCOLOR` apply once, to
the image the layers make together: with `TRANSPARENT=FALSE`, `BGCOLOR`
fills only what no layer covers. Every layer kind can be listed: parameter
layers, RGB composites, CAP warnings, nowcast, lightning events and radar
volumes.

- **Dimensions.** `TIME`, `ELEVATION` and `DIM_REFERENCE_TIME` each apply
  to the listed layers that have that dimension; the other layers ignore
  it. A value a layer with the dimension cannot take is that layer's error,
  as for one layer: a run the layer does not have is
  `InvalidDimensionValue`. A dimension no listed layer has is the
  request's, also as for one layer: `ELEVATION` and `DIM_REFERENCE_TIME`
  are `InvalidDimensionValue`, while a `TIME` goes to every layer, as it
  goes to a single layer without times. Without `TIME`, each layer draws
  its own default, the latest of its own axis for a parameter that has one.
- **Failures.** A layer that fails fails the map: the error tile, or HTTP
  503 when the server is busy or the render deadline passes.
- **Rendering.** The layers render one after another, in one render slot,
  under the render deadline (`MC_RENDER_TIMEOUT_MS`, 3 s) once per layer:
  15 s for five. The deadline counts from admission, so it covers the wait
  for memory and a slot too, and a list is not shed where each of its
  layers alone would be served. The render memory reserved is what one
  layer holds, 32 B per output pixel and 16 B more per further band of an
  RGB composite, plus 4 B per pixel for the image of the layers below,
  whatever the number of layers.
- **Caching.** In EPSG:3857, 3067 and 3035 each layer reuses its own cached
  meta-tiles, the ones a GetMap of that layer alone caches, and caches the
  tiles it renders for those requests too. The image of several layers is
  never in the rendered-image cache: its key would name several
  collections, and reloading one of them could not evict it. Its
  `Cache-Control` is `immutable` only when every layer draws an explicit
  `TIME`, over content its engine never revises in place.
- **WebP quality.** Without `QUALITY`, WebP is lossy only when every listed
  layer's collection sets `[wms] webp_quality`, at the highest of them, so
  a layer kept lossless for its discrete classes keeps the image lossless.

## Supported CRS

| CRS | BBOX Order | Description |
|-----|------------|-------------|
| `CRS:84` | west, south, east, north | WGS84, lon/lat order |
| `EPSG:4326` | south, west, north, east | WGS84, **lat/lon order** |
| `EPSG:3857` | minx, miny, maxx, maxy | Web Mercator |
| `EPSG:3067` | minx, miny, maxx, maxy | TM35FIN (Finland) |
| `EPSG:3035` | minx, miny, maxx, maxy | ETRS89-LAEA (Europe) |

**Important:** WMS 1.3.0 changed BBOX axis order to match the CRS definition. EPSG:4326 uses latitude/longitude order, not longitude/latitude. CRS:84 uses the more intuitive longitude/latitude order. When in doubt, use CRS:84.

Any other CRS answers `InvalidCRS`. A CRS:84 or EPSG:4326 BBOX may reach
past a pole: the rows whose centres lie beyond ±90° have no content
(`BGCOLOR`, white by default, or transparent with `TRANSPARENT=TRUE`), as WMS 1.3.0 §7.3.3.6
asks, and a BBOX wholly past a pole is the empty image. No engine is asked
for those rows (#1065).

An EPSG:3067 or EPSG:3035 BBOX wholly outside its projection's valid area,
where no point maps to the Earth, is the empty image too, in the requested
`FORMAT`, and no engine is asked (#1089):

- EPSG:3035: a box past the LAEA disk, of radius about 12 742 km around
  (4 321 000, 3 210 000).
- EPSG:3067: a box past a pole, northings more than 9 997 965 m from the
  equator, or more than 50° of longitude from the central meridian, 27°E
  (#1113). Transverse Mercator itself reaches further, but the series
  MeteoCore computes it with is 27 km off at 50° on the equator and stops
  converging from about 56°, where it named places anywhere. So EPSG:3067
  maps show nothing west of 23°W, which cuts western Iceland (the
  Westfjords, Snæfellsnes), most of Greenland, the Azores and the
  westernmost 0.5° of the ICON-EU domain, nor east of 77°E. An exact
  Transverse Mercator could lift the bound (#1130).

A box partly outside renders what lies inside, and its pixels past the
area are empty. Along the area's edge a strip up to 2 px wide stays
empty, and where the LAEA disk's rim crosses a box edge between two of the
box's edge samples, 20 a side, the read window can stop short of the rim
by up to their spacing. Two boxes stay `InvalidParameterValue`:

- a malformed one: min ≥ max, or a value that is no finite number;
- an EPSG:3035 one whose edge samples all miss the disk inside it: a box
  wider than the disk, or a very tall one whose edges cross it only
  between samples. Its edges give no area to read. An EPSG:3067 box that
  meets the area always has a read window: around the whole area it holds
  both poles, and where the meridians 50° out cross it, where they enter
  and leave it bounds the window.

## GetFeatureInfo

GetFeatureInfo returns the value of each queried layer at a pixel of the
map a client shows, for QGIS's identify tool, a click in OpenLayers or
Leaflet, or any WMS client:

```
http://localhost:8000/wms/?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetFeatureInfo&LAYERS=radar&QUERY_LAYERS=radar&STYLES=&CRS=CRS:84&BBOX=19,59,32,71&WIDTH=256&HEIGHT=256&I=128&J=100&INFO_FORMAT=application/json
```

The value is the collection's EDR position query at the centre of pixel
(`I`, `J`): GetFeatureInfo and `/edr/collections/{id}/position` at that
point, time, model run and level answer the same value. It is the source
value, sampled the way the engine's position query samples it (bilinear
between grid nodes for GRIB, QueryData, Zarr and NetCDF), not the colour of the
pixel: an EPSG:3857/3067/3035 GetMap is assembled from meta-tiles by nearest
neighbour, so next to a class edge the pixel can show a neighbour's colour.

| Parameter | Required | Description |
|-----------|----------|-------------|
| GetMap's | yes | The parameters that define the map: `VERSION`, `LAYERS`, `STYLES`, `CRS`, `BBOX`, `WIDTH`, `HEIGHT`, `TIME`, `ELEVATION`, `DIM_REFERENCE_TIME`. Every GetMap parameter sent, `FORMAT`, `TRANSPARENT`, `BGCOLOR` and `QUALITY` included, is validated as GetMap validates it, with GetMap's exceptions: an unsupported `FORMAT` is `InvalidFormat` here too. The image parameters do not change the value |
| `QUERY_LAYERS` | yes | The layers of `LAYERS` to read, answered in this order, a layer named twice once. A layer the server does not serve, or one `LAYERS` does not name, is `LayerNotDefined`; one that is not queryable `LayerNotQueryable`, before any layer is read. Each is read at the time, run and level GetMap draws it at ([Several layers](#several-layers)) |
| `I` | yes | The pixel's column, 0 to `WIDTH` − 1. Anything else, a negative or non-integer value included, is `InvalidPoint` |
| `J` | yes | The pixel's row, 0 (top) to `HEIGHT` − 1; otherwise `InvalidPoint` |
| `INFO_FORMAT` | no | `text/plain` (the default when omitted), `application/json` or `text/html`, in any case. Anything else is `InvalidFormat` |
| `FEATURE_COUNT` | no | A positive integer, default 1. A layer has one value at a point, so any count returns it |

The pixel's centre goes through the same CRS handling as GetMap's pixels:
EPSG:4326's `BBOX` is lat/lon, CRS:84's lon/lat, and EPSG:3857, 3067 and
3035 are inverse-projected. `TIME` and `DIM_REFERENCE_TIME` resolve exactly
as GetMap resolves them, so the value belongs to the frame the client
shows: an omitted `TIME` is the layer's default, and a `TIME` between
timesteps is the timestep GetMap draws for it (a GeoTIFF's latest frame not
after it). The level is likewise the one GetMap draws, as the engine
selects it, with or without `ELEVATION`. That is not always `ELEVATION`
itself or the first advertised level:

- GRIB: without `ELEVATION`, the model run's own first level, which can
  differ from the first of the levels all retained runs advertise
  together.
- ODIM radar volumes: the sweep nearest `ELEVATION`, or the lowest sweep,
  among the sweeps that carry the quantity. On a split-cut scan a velocity
  moment can be missing from the lowest sweep, and an `ELEVATION` between
  two sweeps draws the nearer one.

The answer reports the resolved time, and the model run and level when the
layer has them.

`application/json` is a GeoJSON FeatureCollection, one Point feature per
queried layer at the sampled position:

```json
{
  "type": "FeatureCollection",
  "features": [{
    "type": "Feature",
    "geometry": {"type": "Point", "coordinates": [11.0765, 67.8999]},
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
them. `text/plain` lists the same fields one `name: value` line each, after
`layer:`; `text/html` is a table per layer, with every label and value
escaped.

A point without data is an answer, not an exception: the JSON `value` is
`null` and the text formats say `nodata`. That covers a nodata pixel and a
point outside the collection's data, beyond a radar's range for example. A
pixel whose centre is not on the Earth has no position either: its JSON
`geometry` is `null` too, and no engine is asked. That is a centre:

- past a pole of a CRS:84 or EPSG:4326 `BBOX`;
- anywhere in an EPSG:3067 or EPSG:3035 `BBOX` wholly outside its
  projection's valid area, the `BBOX` GetMap answers with the empty image;
- outside that area in a `BBOX` partly outside it: past the LAEA disk, or
  past a pole or more than 50° from EPSG:3067's central meridian.

**Queryable layers.** A layer is `queryable="1"` in GetCapabilities exactly
when its collection's engine answers EDR position queries for the layer's
parameter, whether or not the collection lists `edr` in its `apis`; without
it no EDR route is published, and the WMS still reads values through the
engine. These layers stay `queryable="0"` and answer `LayerNotQueryable`:

- RGB composite layers (`coll/<composite>`): a value per band, which no
  position query answers.
- The ODIM per-site `CELLS` overlay: storm-cell outlines, not a moment.
- CAP alert layers, PostGIS events-shape (lightning) layers and nowcast
  layers: their engines answer no position query.

GetFeatureInfo runs on the same render slots and queue as GetMap, under a
deadline of its own, `MC_FEATURE_INFO_TIMEOUT_MS` (10 s by default), not
GetMap's 3 s render deadline: a cold position query can take longer than a
render. The deadline counts from admission, so it covers both the wait for a
render slot and the queries: several query layers are read one after
another, in one slot, under that one deadline. An overloaded or timed-out
request is HTTP 503 with `Retry-After: 1`. The engine calls run on the query
runtime EDR queries also run on, never on the threads that serve requests.
An engine failure is a generic exception, its detail only in the server log,
and the query layers after it are not read.

## WMS Configuration

The `[collections.wms]` section configures how raster data is rendered as map images.

### Built-in Colormaps

```toml
[collections.wms]
colormap = "radar_dbz"    # Standard radar reflectivity colors
# colormap = "viridis"    # Perceptually uniform (good for continuous data)
# colormap = "grayscale"  # Linear black-to-white
```

| Name | Value Range | Description |
|------|-------------|-------------|
| `radar_dbz` | 0–70 | Blue, cyan, green, yellow, orange, red, magenta, white |
| `viridis` | 0–1 | Dark purple through blue, green, yellow |
| `grayscale` | 0–1 | Black to white |
| `temperature` | -40–50 | Purple, blue, cyan, green, yellow, orange, red |
| `precipitation` | 0–50 | Transparent, light blue, blue, purple, magenta, white |
| `wind_speed` | 0–50 | Green, yellow, orange, red, crimson, purple, violet |

Use `min` and `max` to override the default value range:

```toml
[collections.wms]
colormap = "temperature"
min = -20.0    # only show -20 to 40 range
max = 40.0
```

### Custom Color Stops

Define your own value-to-color mapping. Overrides the built-in colormap.

```toml
[collections.wms]
colormap = "ignored_when_stops_defined"

[[collections.wms.color_stops]]
value = 0.0
color = "#00000000"    # transparent (RRGGBBAA)

[[collections.wms.color_stops]]
value = 0.1
color = "#0000FF"      # blue (RRGGBB, fully opaque)

[[collections.wms.color_stops]]
value = 5.0
color = "#00FF00"      # green

[[collections.wms.color_stops]]
value = 10.0
color = "#FF0000"      # red
```

Colors are specified in `#RRGGBB` (6 hex digits, fully opaque) or `#RRGGBBAA` (8 hex digits, with alpha). Values between stops are linearly interpolated.

### Named Styles

Define multiple styles per layer. Clients select via `STYLES=` parameter.

```toml
[collections.wms]
colormap = "radar_dbz"    # default style

[[collections.wms.styles]]
name = "grayscale"
title = "Grayscale Reflectivity"
colormap = "grayscale"
min = 0.0
max = 70.0

[[collections.wms.styles]]
name = "viridis"
title = "Viridis Reflectivity"
colormap = "viridis"
min = 0.0
max = 70.0
```

Styles are listed in GetCapabilities and include LegendURL links.

### Cache Configuration

```toml
[collections.wms]
colormap = "radar_dbz"
rendered_cache_mb = 256    # Default: 256 MB. Set to 0 to disable.

[server]
metatile_cache_mb = 1024   # Global (server-wide). Default: 1024 MB. 0 disables meta-tiling.
```

The rendered image cache stores a single layer's final PNG/JPEG/WebP bytes keyed by bbox, style, format, dimensions, CRS, and timestamp; an image of several layers is not cached ([Several layers](#several-layers)). No TTL — radar data is immutable once produced. Cache is invalidated on collection reload (`POST /admin/collections/reload`).

For EPSG:3857 GetMap, the handler additionally uses an internal **meta-tile cache**: each request is decomposed into fixed 256×256 WebMercatorQuad tiles, rendered+cached as decoded RGBA, and resampled to the exact viewport — so overlapping fullscreen views reuse cached tiles instead of re-rendering. This cache is **global** (configured once under `[server] metatile_cache_mb`, not per-collection) and is consumed by WMS today (Maps/Tiles render directly for now). `metatile_cache_mb = 0` disables it and reverts to a direct render (reversible via reload).

Also configure the source tile cache for remote GeoTIFFs:

```toml
[collections.geotiff]
tile_cache_mb = 256    # Default: 256 MB. Caches compressed COG tile bytes from S3.
```

### HTTP Caching

WMS responses include headers for client and CDN caching:

| Scenario | Cache-Control |
|----------|---------------|
| GetMap with `TIME=` | `public, max-age=86400, immutable` (24h) |
| GetMap without `TIME` | `public, max-age=60, must-revalidate` (1min) |
| GetFeatureInfo | As GetMap: `immutable` only with `TIME=` on content the engine never revises in place, and only for a value: a null answer always revalidates |
| GetLegendGraphic | `public, max-age=86400, immutable` (24h) |

All GetMap responses include an `ETag` header derived from the request parameters.

## Security Limits

| Limit | Value | Purpose |
|-------|-------|---------|
| Max image pixels | 64,000,000 | `WIDTH × HEIGHT` cap (`params::MAX_MAP_PIXELS`); the render memory budget `MC_RENDER_MEMORY_MB` admits fewer at its default |
| Max dimension | 8,000 px | Limits width and height individually (`params::MAX_MAP_DIMENSION`); advertised in GetCapabilities as `<MaxWidth>`/`<MaxHeight>` |
| Max layers | 5 | `LAYERS` names per GetMap and GetFeatureInfo (`params::LAYER_LIMIT`); advertised as `<LayerLimit>` |
| Concurrent renders | `[server] render_concurrency` | Render slots shared by WMS, Maps, Tiles and 3D Tiles (default 2× CPUs, min 8) |
| CRS whitelist | 5 CRS | Only supported projections accepted |
| Format whitelist | PNG, JPEG, WebP | No unexpected format handling |
| No external SLD | — | Eliminates SSRF risk from style references |
| XML output | quick-xml Writer | Prevents XML injection in GetCapabilities/errors |

## Error Responses

WMS errors are returned as XML `ServiceExceptionReport` documents per the
WMS 1.3.0 spec, with `Content-Type: text/xml`, WMS 1.3.0's MIME type for
them:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<ServiceExceptionReport version="1.3.0" xmlns="http://www.opengis.net/ogc">
  <ServiceException code="LayerNotDefined">
    Layer 'nonexistent' does not exist
  </ServiceException>
</ServiceExceptionReport>
```

Error codes: `LayerNotDefined`, `StyleNotDefined`, `InvalidCRS`, `InvalidDimensionValue`, `MissingParameterValue`, `InvalidFormat`, `InvalidParameterValue`, `OperationNotSupported`, and for GetFeatureInfo `LayerNotQueryable` and `InvalidPoint`.

The codes WMS 1.3.0 Table E.1 defines are used where it says: an unknown
layer or `collection/parameter` is `LayerNotDefined` and a style the layer
does not offer `StyleNotDefined`, for every layer of a `LAYERS` list before
the list's length is refused; a CRS other than the five above is
`InvalidCRS`; an unsupported `FORMAT` is `InvalidFormat`; a malformed `TIME`
or `ELEVATION`, an `ELEVATION` for a layer without one, or a
`DIM_REFERENCE_TIME` that is no run of the layer, is `InvalidDimensionValue`;
an unknown `REQUEST` is `OperationNotSupported`. GetFeatureInfo adds
`LayerNotQueryable` for a layer that is not queryable, `InvalidPoint` for an
`I` or `J` outside the image, `InvalidFormat` for an unsupported
`INFO_FORMAT`, and `LayerNotDefined` for a `QUERY_LAYERS` layer that
`LAYERS` does not name. Until #1065 an unsupported
CRS was `CRSNotDefined`, and a malformed `ELEVATION` or one for a layer
without one `InvalidParameterValue`.

GetCapabilities advertises this one exception format,
`<Exception><Format>XML</Format></Exception>`, right after `<Request>`. The
`EXCEPTIONS` parameter is ignored: a request error is always this XML
document, and so is the 503 of an overloaded or timed-out render. A GetMap
whose render fails in the engine is not a request error: it returns a red
error image with HTTP 200. Whether to honour `EXCEPTIONS=INIMAGE` and
`BLANK` is still open (#1027). Until #1065 the exception documents were
served as WMS 1.1.1's `application/vnd.ogc.se_xml`.

## Architecture

```
Client (QGIS/Leaflet/OpenLayers)
    │
    ▼ HTTP GET /wms/?SERVICE=WMS&REQUEST=GetMap&...
    │
api-wms (handlers.rs)
    │ parse WMS params, validate, normalize bbox axis order
    │ check rendered cache → if HIT, return cached PNG
    │ acquire render semaphore (max 8 concurrent)
    │
    ▼ spawn_blocking
    │
engine-geotiff (MapEngine::get_raster_tile)
    │ bbox_to_pixels → read source tiles → nearest-neighbor resample
    │ returns RasterTile { width, height, values: Vec<Option<f64>> }
    │
    ▼
ds-render (render_tile_png)
    │ colorize: value → RGBA via LUT/linear colormap
    │ encode: RGBA buffer → PNG bytes (pure Rust, compression=Fast)
    │
    ▼
api-wms (handlers.rs)
    │ cache PNG, return with Content-Type: image/png
    ▼
Client
```

## Current Limitations

- No external SLD/SE style documents; local SLD ColorMap palette import is supported
- GetFeatureInfo answers one value per queried layer, from the collection's position query; composites, overlays and collections without a position query are not queryable ([GetFeatureInfo](#getfeatureinfo))
- Source sampling depends on the engine (including bilinear for GRIB/QueryData/Zarr/NetCDF); meta-tile assembly uses nearest-neighbor to preserve discrete palettes
- Collections must implement `MapEngine` and enable `wms`; GeoTIFF, GRIB, ODIM, QueryData, Zarr, nowcast, CAP and PostGIS events are supported
- CAP limits that can leave a warning off the map, the same in WMS, Maps and Tiles: a WIS2-fed collection holds at most `max_alerts` (default 10 000) alerts, and past it drops still-valid ones from the map and `/items`, not-yet-active and lowest severity first, reporting `degraded` in `/health`; one render draws at most 50 000 active alert areas, and past that leaves out the least severe (#805)
- JPEG output composites transparency onto white background (no alpha channel)

Uncached GetMap requests have a configurable 3 s queue/render deadline
(`MC_RENDER_TIMEOUT_MS`), once per layer of a list, and a bounded waiting queue (`MC_RENDER_QUEUE_CAPACITY`,
default 3× CPU slots). Overload or expiry returns HTTP 503 with `Retry-After: 1`;
cached images remain available. GeoTIFF remote reads use the same deadline.
GetFeatureInfo waits in the same queue under its own 10 s deadline
(`MC_FEATURE_INFO_TIMEOUT_MS`).

GRIB metadata is shared through cached descriptors for capabilities and map
requests. Bounds come from a header probe of each level collection's latest-run
representative grid; unknown geometry is omitted rather than marked global.

## OGC test suite

The `WMS ETS` workflow (`.github/workflows/wms-ets.yml`, #1065) runs OGC's
WMS 1.3.0 executable test suite, [ets-wms13](https://github.com/opengeospatial/ets-wms13),
under TEAM Engine from the suite's published container, pinned by digest. It
runs weekly, on demand, and on pull requests that change this crate, the
workflow, its config in `ci/wms-ets/` or the fixtures that config serves. It
builds the server, a debug build with the render and GetFeatureInfo
deadlines raised to 60 s and the render memory budget to 2560 MiB, which
admits the advertised 8000 × 8000 maximum, and starts it on
`ci/wms-ets/config.toml`, whose collections use only fixtures committed to
the repository:

| Layer | Covers |
|-------|--------|
| `ets-geotiff` | A plain raster: one GeoTIFF on a projected grid (EPSG:3067) |
| `ets-radar` | A time axis: five radar frames |
| `ets-grib-pressure` | An elevation axis (GRIB pressure levels) and a model-run axis (`reference_time`), one value each |
| `ets-multi/<param>` | A multi-parameter forecast: a parent layer with one child layer per parameter, a model-run axis of one run, and an `<Attribution>` |
| `ets-netcdf-levels/<param>` | Several pressure levels and model runs: MEPS CF-NetCDF (`testdata/meps-netcdf-pl`) with an elevation axis of four levels, 1000 to 300 hPa, and a model-run axis of two runs, declared on the parent layer and inherited by its child layers: air temperature, the x/y wind components and the wind speed derived from them |

GetCapabilities lists the layers in collection-id order, the order of the
OGC API `/collections` list (#1102). Several tests take the first or the last
named layer, so the ids decide which layers the suite tests, and every run
tests the same ones. A new fixture collection takes an id that sorts after
`ets-multi` and before `ets-radar`, the last named layer. One with child
layers must sort after `ets-multi`, because `getmap:bbox-inherited` and the
multi-style tests take the first child layers.

The suite's time and elevation classes, `time`, `raster_elevation` and
`vector_elevation`, test only Blue Lake layers (`cite:Autos`, `cite:Terrain`,
`cite:Lakes` and others), so they stay off. The several levels and runs of
`ets-netcdf-levels` are tested as the other axes are: the capabilities tests
validate their `<Dimension>` declarations, and `getmap:each-layer` and
`getfeatureinfo:each-queryable-layer` request every child layer at its
default level, run and time.

The run requests the basic conformance class and, since GetFeatureInfo
(#1064), the queryable class; the data-independent tests always run. Every
fixture layer is advertised `queryable="1"`: each engine answers EDR
position queries, though no fixture collection lists `edr`. The job summary
opens with the verdict against the expected-failure list, below, then lists
the passed, failed and skipped tests, with
each failure's message and request. Failures in the classes that need OGC's
test dataset, below, are collapsed to one line each, except
`basic:gif-or-png`, `basic:options-requirements` and
`queryable:getfeatureinfo-supported`, which test MeteoCore's own
capabilities and are listed in full. The `wms-ets-report` artifact holds
the full report: TEAM Engine's CTL log, the session directory with every
test's log and the EARL report (`teamengine-session.tar.gz`), the server log
and the capabilities document as the server served it at startup.

`ci/wms-ets/expected-failures.txt` lists the tests expected to fail, one
name per line, grouped by the causes below. The job fails when a test fails
that is not on the list, with an error annotation naming it, and when the
suite could not run: the server or TEAM Engine did not start, the server
died during the run, the run returned no report, no conformance class ran,
or the list is missing, repeats a name or holds a line that is not a test
name. A listed test that did not fail is a warning in the job summary and an
annotation: remove it from the list. A new failure is fixed, or, when it
comes from a decision or the test dataset, added to the list under its
cause. `summarize.py` applies the list, and `test_summarize.py`, which the
job runs first, tests it.

The run meets these deliberate limits:

- Up to five layers per GetMap and GetFeatureInfo (`LayerLimit` 5, #1181).
  The suite's requests naming two to five layers are drawn and answered:
  its multi-layer `STYLES` tests, the five layers of
  `getmap:styles-some-default` (`STYLES=s,,s,,`) among them, and
  `getfeatureinfo:two-query_layers`, `three-query_layers` and
  `less-query_layers`. Its lists naming an undefined layer or style pass:
  the names are checked before the length. `getmap:layerlimit` sends
  `LayerLimit` copies of one layer.
- The queryable class tests the Blue Lake dataset below, except
  `queryable:getfeatureinfo-supported`, which reads MeteoCore's capabilities
  and passes. `queryable:std-data-queryable` looks for the `BasicPolygons`,
  `Forests`, `Lakes` and `NamedPlaces` layers, and the two `feature_count`
  tests query a Blue Lake layer, so with none served they send empty
  `LAYERS` and `QUERY_LAYERS`. The data-independent GetFeatureInfo tests
  read MeteoCore's own layers.
- The first named layer is `ets-geotiff`, a top-level layer with no data at
  0°N 0°E. Three tests take it:
  - `getmap:transparent-default` asks for a box around 0°N 0°E without
    `TRANSPARENT` and wants no transparent pixel. Over no data it tests the
    default itself: `FALSE` (#1065), so the empty image is `BGCOLOR` white.
    Over data, as on the global `ets-grib-pressure`, it would pass whatever
    the default.
  - `getmap:layerlimit` and `getmap:transparent-opaque-layer` pass. On a
    child layer, such as `ets-multi/10wdir`, both fail on the request
    ets-wms13 1.34 builds: `layerlimit` percent-encodes the name twice, so
    the `/` arrives as `%2F` and the layer is not defined, and
    `transparent-opaque-layer` sends an empty `CRS` and the box `0,0,1,1`
    for a layer that inherits its `BoundingBox` and `CRS` from its parent,
    as WMS 1.3.0 §7.2.4.8 allows.
- The basic class's dataset tests and the separate data-preconditions class
  need OGC's "Blue Lake" test dataset (the `cite:*` layers), which MeteoCore
  does not serve. They fail.
- GetMap and GetFeatureInfo accept only `VERSION=1.3.0`, and refuse a
  request without it. This costs nothing here: the suite sends 1.3.0, and
  GetCapabilities answers any `VERSION` with the 1.3.0 document, as version
  negotiation requires.

The server is a debug build, with overflow checks and debug assertions on, so
reproduce a 500 or a dropped connection on a release build before filing it.

**First run, 2026-10-15** (ets-wms13 1.34, TEAM Engine 6.0.0-RC2): 139 tests, 43
passed, 96 failed (72 directly, 24 through a failed subtest). By cause:

| Failures | Cause | Status |
|---:|---|---|
| 62 | Mixed-case parameter names (`ReQuEsT`, `WiDtH`): WMS 1.3.0 §6.8.1 makes parameter names case-insensitive, and `WmsQuery` accepted only lower, upper and capitalised spellings | fixed: names are matched case-insensitively for every request |
| 7 | `LegendURL` has no `<Format>` child, so the suite cannot compare the legend's MIME type | fixed: `<Format>image/png</Format>` plus the legend's width and height |
| 1 | The capabilities root has no `xsi:schemaLocation` | fixed: WMS 1.3.0 and SLD 1.1.0 schema locations; `GetLegendGraphic` is written as `sld:GetLegendGraphic` so the document validates |
| 1 | No `<Exception>` element in `<Capability>` | fixed: `XML` only; honouring `EXCEPTIONS=INIMAGE/BLANK` stays open on #1027 |
| 1 | `getmap:no-version`: GetMap without `VERSION` renders instead of raising an exception | fixed: GetMap and GetFeatureInfo without `VERSION` are `MissingParameterValue` (#1065) |
| 1 + class | `data-preconditions` and most of `basic` need OGC's Blue Lake dataset (`cite:` layers) | expected: MeteoCore serves its own collections |

The mixed-case failures stop most GetMap tests at the first request, so the
next run after that fix shows what lies behind them.

The same fix also made request errors `text/xml`, the WMS 1.3.0 exception
MIME type; `application/vnd.ogc.se_xml` is WMS 1.1.1's, and
`getmap:exceptions-default` / `getmap:exceptions-xml` rejected it.

**Second run, 2026-10-15** (ets-wms13 1.34, TEAM Engine 6.0.0-RC2, after the
first run's fixes): 146 tests, 115 passed, 31 failed (20 directly, 11 through
a failed subtest), and the data-preconditions class. By cause:

| Failures | Tests | Cause | Status |
|---:|---|---|---|
| 7 | `getmap:two-layers`, `three-layers`, `two-styles`, `three-styles`, `styles-default-multiple-layers`, `styles-default-commas`, `styles-some-default` | GetMap renders one layer (`LayerLimit` 1, #1014) | fixed: up to five layers (#1181) |
| 4 | `getmap:first-layer-invalid`, `second-layer-invalid`, `first-style-invalid`, `second-style-invalid` | A `LAYERS` list was refused for its length, `InvalidParameterValue`, before its names were checked | fixed: every named layer and style is checked first, so an undefined one is `LayerNotDefined` / `StyleNotDefined`; a list of served layers is still refused |
| 1 | `getmap:invalid-crs` | An unsupported CRS answered `CRSNotDefined`, not Table E.1's `InvalidCRS` | fixed: `InvalidCRS`. A malformed `ELEVATION`, or one for a layer without one, is now `InvalidDimensionValue`, as a malformed `TIME` was |
| 1 | `getmap:large-size` | The advertised maximum, 8000 × 8000, is 64 M pixels; the default render memory budget admits 33.5 M, so it was a 503 | fixed in the job: `MC_RENDER_MEMORY_MB=2560`, the render deadline 60 s. Production sizes its own budget |
| 1 | `getmap:bbox-outside-crs` | The box is wholly north of 90°N. The server returned the empty image, transparent since `TRANSPARENT` defaulted to `TRUE`, and the suite wants every pixel's RGB white | fixed: `TRANSPARENT` defaults to `FALSE`, as WMS 1.3.0 specifies (#1065). The test exposed a real fault next to it: a box reaching past a pole had the engine draw there, a global GRIB grid the rows of its edge cell south of 90°S. Fixed: WMS and Maps ask the engine only for the rows inside ±90° |
| 1 | `getmap:no-version` | GetMap without `VERSION` renders | fixed: GetMap and GetFeatureInfo without `VERSION` are `MissingParameterValue` (#1065) |
| 5 + class | `basic:bbox-exponential`, `bbox-pixel-interpretation`, `no-bgcolor`, `blue-bgcolor`, `transparent-true`, and `data-preconditions` | OGC's Blue Lake dataset (`cite:` layers) | expected: MeteoCore serves its own collections |
| 11 | `getmap:getmap`, `bbox`, `crs`, `layers`, `styles`, `width-and-height`, `version`; `basic:getmap`, `bbox`, `bgcolor`, `transparent` | fail through a subtest above | follow their subtests |

With these fixes the next run should pass the six fixed tests, and
`getmap:crs` and `getmap:width-and-height` through them.

**With GetFeatureInfo, 2026-10-16** (ets-wms13 1.34, TEAM Engine 6.0.0-RC2,
the basic and queryable classes, #1064): 177 tests, 141 passed, 21 failed
directly and 15 through a failed subtest. Every data-independent
GetFeatureInfo test of one layer passes, `each-queryable-layer` on all seven
layers and `each-info_format` for all three formats among them. By cause:

| Failures | Tests | Cause | Status |
|---:|---|---|---|
| 7 | `getmap:two-layers`, `three-layers`, `two-styles`, `three-styles`, `styles-default-multiple-layers`, `styles-default-commas`, `styles-some-default` | One layer per GetMap (`LayerLimit` 1, #1014) | fixed: up to five layers (#1181) |
| 3 | `getfeatureinfo:two-query_layers`, `three-query_layers`, `less-query_layers` | The same limit: their `LAYERS` name two or three layers | fixed: an answer per query layer (#1181) |
| 2 | `getmap:bbox-outside-crs`, `transparent-default` | `TRANSPARENT` defaulted to `TRUE` (#163), where WMS 1.3.0 says `FALSE`; `transparent-default` only when its layer has no data in its box | fixed: `TRANSPARENT` defaults to `FALSE` (#1065). A request without it gets an opaque image over `BGCOLOR`; a client that sends `TRANSPARENT` gets what it got before |
| 1 | `getmap:no-version` | GetMap without `VERSION` renders | fixed: GetMap and GetFeatureInfo without `VERSION` are `MissingParameterValue` (#1065) |
| 5 + class | `basic:bbox-exponential`, `bbox-pixel-interpretation`, `no-bgcolor`, `blue-bgcolor`, `transparent-true`, and `data-preconditions` | OGC's Blue Lake dataset (`cite:` layers) | expected: MeteoCore serves its own collections |
| 3 | `queryable:std-data-queryable`, `feature_count-default`, `feature_count-1` | The same dataset | expected |

**With `TRANSPARENT` defaulting to `FALSE`, 2026-10-18** (ets-wms13 1.34,
TEAM Engine 6.0.0-RC2, #1182): 177 tests, 145 passed, 19 failed directly and
13 through a failed subtest, and the data-preconditions class.
`getmap:bbox-outside-crs` and `transparent-default` pass. The other failures
are the deliberate and Blue Lake ones above, and
`ci/wms-ets/expected-failures.txt` lists them.
