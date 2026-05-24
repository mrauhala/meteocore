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
| `GetCapabilities` | Returns XML document listing available layers, CRS, extents, time dimension, and styles |
| `GetMap` | Returns a rendered map image (PNG or JPEG) |
| `GetLegendGraphic` | Returns a legend image showing the colormap scale |

GetFeatureInfo is not supported.

## GetMap Parameters

| Parameter | Required | Description |
|-----------|----------|-------------|
| `SERVICE` | yes | Must be `WMS` |
| `VERSION` | yes | Must be `1.3.0` |
| `REQUEST` | yes | `GetMap` |
| `LAYERS` | yes | Layer name (= collection ID). Only one layer per request. |
| `CRS` | yes | Coordinate reference system (see below) |
| `BBOX` | yes | Bounding box (axis order depends on CRS) |
| `WIDTH` | yes | Output image width in pixels (1–8000) |
| `HEIGHT` | yes | Output image height in pixels (1–8000) |
| `FORMAT` | yes | Must be `image/png` |
| `TRANSPARENT` | no | `TRUE` (default) or `FALSE` |
| `TIME` | no | ISO 8601 timestamp. Defaults to latest available. |
| `STYLES` | no | Ignored (only default style is supported) |

## Supported CRS

| CRS | BBOX Order | Description |
|-----|------------|-------------|
| `CRS:84` | west, south, east, north | WGS84, lon/lat order |
| `EPSG:4326` | south, west, north, east | WGS84, **lat/lon order** |
| `EPSG:3857` | minx, miny, maxx, maxy | Web Mercator |
| `EPSG:3067` | minx, miny, maxx, maxy | TM35FIN (Finland) |
| `EPSG:3035` | minx, miny, maxx, maxy | ETRS89-LAEA (Europe) |

**Important:** WMS 1.3.0 changed BBOX axis order to match the CRS definition. EPSG:4326 uses latitude/longitude order, not longitude/latitude. CRS:84 uses the more intuitive longitude/latitude order. When in doubt, use CRS:84.

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
rendered_cache_mb = 512    # Default: 512 MB. Set to 0 to disable.
metatile_cache_mb = 1024   # Default: 1024 MB. Set to 0 to disable meta-tiling.
```

The rendered image cache stores final PNG/JPEG/WebP bytes keyed by bbox, style, format, dimensions, CRS, and timestamp. No TTL — radar data is immutable once produced. Cache is invalidated on collection reload (`POST /admin/collections/reload`).

For EPSG:3857 GetMap, the handler additionally uses an internal **meta-tile cache**: each request is decomposed into fixed 256×256 WebMercatorQuad tiles, rendered+cached as decoded RGBA, and resampled to the exact viewport — so overlapping fullscreen views reuse cached tiles instead of re-rendering. `metatile_cache_mb = 0` disables this and reverts to a direct render (reversible via reload).

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
| GetLegendGraphic | `public, max-age=86400, immutable` (24h) |

All GetMap responses include an `ETag` header derived from the request parameters.

## Security Limits

| Limit | Value | Purpose |
|-------|-------|---------|
| Max image pixels | 16,777,216 | Prevents memory exhaustion from large renders |
| Max dimension | 4,096 px | Limits width and height individually |
| Concurrent renders | 8 | Semaphore prevents CPU/memory exhaustion |
| CRS whitelist | 5 CRS | Only supported projections accepted |
| Format whitelist | PNG, JPEG | No unexpected format handling |
| No external SLD | — | Eliminates SSRF risk from style references |
| XML output | quick-xml Writer | Prevents XML injection in GetCapabilities/errors |

## Error Responses

WMS errors are returned as XML `ServiceExceptionReport` documents per the WMS 1.3.0 spec:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<ServiceExceptionReport version="1.3.0" xmlns="http://www.opengis.net/ogc">
  <ServiceException code="LayerNotDefined">
    Layer 'nonexistent' does not exist
  </ServiceException>
</ServiceExceptionReport>
```

Error codes: `LayerNotDefined`, `StyleNotDefined`, `CRSNotDefined`, `InvalidDimensionValue`, `MissingParameterValue`, `InvalidFormat`, `InvalidParameterValue`, `OperationNotSupported`.

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

- Single layer per request (no multi-layer composition)
- No SLD/SE styling (no external style documents)
- No GetFeatureInfo (use EDR position query instead)
- Nearest-neighbor resampling only (no bilinear interpolation)
- Only GeoTIFF collections can be exposed via WMS
- JPEG output composites transparency onto white background (no alpha channel)
