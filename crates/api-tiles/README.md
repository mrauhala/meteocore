# api-tiles

OGC API - Tiles 1.0 for MeteoCore: raster collections as map tiles
(PNG/JPEG/WebP) and feature collections as Mapbox Vector Tiles (MVT).

## What it solves

Web map clients load data as small tiles on a fixed grid. Because every client
asks for the same tiles, responses cache well in browsers, CDNs and the server.
This crate puts MeteoCore's radar, model, satellite and alert collections on
that grid. It has no tile store and no tile engine of its own. Each tile is
rendered on demand by the collection's engine, then cached.

## How it works

- The tile matrix set's math turns a tile address into a WGS84 bounding box.
- **Map tiles**: the box goes to `MapEngine::get_raster_tile` (or
  `get_raster_tiles` for an RGB composite), rendered in EPSG:3857 for
  WebMercatorQuad or CRS84 for WorldCRS84Quad. `ds-render` colours and encodes
  the result. An all-nodata tile skips encoding and gets a shared empty image
  (transparent; white in JPEG).
- **Vector tiles**: the box becomes a `FeatureQuery` on the collection's
  `FeatureEngine`. `ds-mvt` encodes the features into one layer named after
  the collection id (extent 4096), with every feature property.
- Before building the cache key, the handler asks the engine which time step
  and model run it will render, so "latest" and a time not yet arrived are
  cached under the step actually rendered.
- Map tiles share the rendered-image cache with WMS and Maps. Vector tiles have
  their own cache, keyed on the engine's data version and the time filter.
  ETags hash the bytes, and `If-None-Match` gets a 304.
- Rendering and MVT encoding run through `ds-executor`, on render slots and a
  deadline shared with WMS and Maps; map tiles also reserve output memory.
- `TilesState` (swapped on reload) holds the map and feature engine registries
  for both the per-API `router` and the shared root's `TilesBlock`.

## Endpoints

At the shared root, api-common serves the landing page, conformance and
catalog. This crate adds:

```
/tileMatrixSets[/{tileMatrixSetId}]                                  Tiling schemes
/collections/{id}/map/tiles[/{tileMatrixSetId}]                      Map tilesets, tileset
/collections/{id}/map/tiles/{tms}/{z}/{row}/{col}                    Map tile
/collections/{id}/styles/{styleId}/map/tiles[/{tileMatrixSetId}]     Styled map tilesets, tileset
/collections/{id}/styles/{styleId}/map/tiles/{tms}/{z}/{row}/{col}   Styled map tile
/collections/{id}/tiles[/{tileMatrixSetId}]                          Vector tilesets, tileset
/collections/{id}/tiles/{tms}/{z}/{row}/{col}                        Vector tile (MVT)
```

The per-API service at `/tiles` has its own landing page, `conformance` and
`collections` (JSON and HTML), `api` (OpenAPI 3.0) and `api/docs` (Swagger UI),
plus:

```
/tiles/tileMatrixSets[/{tileMatrixSetId}]
/tiles/collections/{id}/tiles[/{tileMatrixSetId}]
/tiles/collections/{id}/tiles/{tms}/{z}/{row}/{col}                  Map tile, or MVT with ?f=mvt
/tiles/collections/{id}/styles/{styleId}/tiles/{tms}/{z}/{row}/{col} Styled map tile
/tiles/collections/{id}/styles/{styleId}/legend                      Style legend
```

- Tile paths are zoom, row (y), column (x). `WebMercatorQuad` (EPSG:3857) and
  `WorldCRS84Quad` (CRS84, 2 × 1 at zoom 0) use 256 × 256 tiles, zooms 0–24.
- Tile matrix sets, tileset lists and tilesets are JSON only (`f=html` is a
  400). A request that asks for HTML gets its errors as an HTML page.
- The legend is JSON by default (palette stops, range, interpolation) or a
  180 × 300 PNG with `f=png`. `parameter-name` selects a parameter's style;
  for an RGB composite the legend lists the channels.

Collection metadata adds `dataType`, `crs`, `storageCrs`, `styles`,
`parameter_names` and `tilesets-map`/`tilesets-vector` links; tilesets give
`tileMatrixSetLimits` for zooms 0–18 from the extent. Catalog search and HTML
pages come from [api-common](../api-common/README.md).

## Tile parameters

| Parameter | Map tiles | Vector tiles |
|---|---|---|
| `f` | `image/png` (default), `image/jpeg`, `image/webp` | `mvt` or `application/vnd.mapbox-vector-tile` |
| `datetime` | Instant or interval (`a/b`, `../b`, `a/..`) | Same; filters features |
| `subset` | `datetime(…)` only: quoted RFC 3339 values, partial dates, `*` | Same |
| `width`, `height` | Image size over the tile's area, ≤ 8000 a side, ≤ 64 M px | 400 |
| `scale-denominator` | Image size from the scale at the tile centre; not with `width`/`height` | 400 |
| `parameter-name` | A parameter or RGB composite; unknown: 400 listing valid names (single-band: ignored) | 400 |
| `elevation` | Vertical level; 400 on a collection without one | 400 |
| `quality` | 1–100 for JPEG and WebP; 400 for PNG | 400 |

Any other query parameter, or `datetime` with `subset`, is a 400.

- **Map tile time.** No time: the engine's default step, normally the latest.
  An instant snaps to one of the engine's time steps; an interval, a partial
  date or `*` renders the latest step inside it. No step selected: 204 No
  Content. `OGCAPI-datetime` names the instant rendered.
- **Vector tile time.** The tile keeps features whose temporal geometry
  intersects the selection, and features without time. A time subset entirely
  outside the temporal extent is a 204. A collection without a time dimension
  ignores the time.
- **Encoding.** PNG uses an 8-bit palette when the tile has at most 256
  colours. JPEG defaults to quality 85. WebP is lossless unless `quality` or
  the collection's `[wms] webp_quality` sets a lossy quality. An RGB composite
  has one style, `default`.

## Responses

- Map tiles carry `Content-Crs`, `ETag` and `x-cache` (`HIT`, `MISS`,
  `EMPTY`). An explicit instant that resolved to a time step, on an engine that
  never revises content in place, is `public, max-age=86400, immutable`;
  everything else is `public, max-age=60, must-revalidate`.
- Vector tiles carry `ETag`, `x-cache` and `Cache-Control: public, max-age=300`.
- Errors are JSON `{"code", "description"}`. 400: a bad parameter, an
  engine's parameter error, zoom above 24, or an unknown tile matrix set on a
  tile route. 404: unknown collection, style or tile matrix set, or a tile
  outside the matrix. 422: a vector tile over 50 000 features. 500: generic
  message. 503 + `Retry-After: 1`: render slots, deadline or memory exhausted.

## Conformance

OGC API - Common (core, landing-page, oas30, collections, json, html), Tiles
(core, tileset, tilesets-list, geodata-tilesets, datetime, png, jpeg, mvt) and
TMS 2.0 (tilematrixset, json-tilematrixset). The shared root adds Maps
`tilesets`, as its map tiles sit under the map resource.

## Configuration

A collection serves tiles when its `apis` list includes `"tiles"`. Map tiles
come from `geotiff`, `grib`, `querydata`, `zarr`, `netcdf`, `satellite`,
`odim`, `odim-volume` (per-site collections), `nowcast`, `station-grid`,
`cap` and `postgis`: its events shape, and a station shape with map layers
on (a `[postgis.map]` block, or `wms` or `maps` in `apis`), drawn as one
marker per station coloured by value. Vector tiles come from `geojson`,
`cap` and the `postgis` station shapes.

Map styles come from the `[wms]` block shared with WMS and Maps: `colormap`
(the `default` style), `[[wms.styles]]` and `[[wms.parameters]]`.

```toml
[[collections]]
id = "radar"
title = "Radar reflectivity"
description = "Radar reflectivity composite"
engine_type = "geotiff"
apis = ["wms", "maps", "tiles"]
# engine block ([collections.geotiff]) omitted

[collections.wms]
colormap = "radar_dbz"        # the `default` style
# webp_quality = 80           # 1-100; unset = lossless WebP
```

| Setting | Default | Effect |
|---|---|---|
| `[server] base_url`, `trust_proxy_headers` | none, `false` | Absolute links in metadata |
| `[server] render_concurrency` | 2 × CPUs, min 8 | Render slots shared with WMS and Maps |
| `[wms] rendered_cache_mb` | 256 | Rendered-image cache size, read from a WMS or Maps collection |
| `MC_RENDER_TIMEOUT_MS` | 3000 | Queue and render deadline, MVT encoding included |
| `MC_RENDER_MEMORY_MB` | 1024 | Output-memory budget for map renders |

Runnable examples: `collections.d/municipalities.toml` (GeoJSON, vector
tiles) and `collections.d/zarr-era5-t2m-local.toml` (Zarr, map tiles).

## Limits

- Only WebMercatorQuad and WorldCRS84Quad; no projected tile matrix sets.
- No per-collection zoom range: zooms 0–24 are served, 0–18 advertised.
- Collection tilesets only; no dataset (multi-collection) tilesets.
- Forecast collections render the engine's latest model run; there is no
  `reference_time` parameter.
- Map tiles take no background colour or transparency parameter.
- Vector tiles carry every property in one layer: no property selection or
  styling. Their cache is 128 MB and not configurable.

## Testing

```bash
cargo test -p api-tiles
cargo test -p server --test common_discovery   # shared discovery, all APIs
```

Unit tests cover parameter validation, errors and tile matrix math.
`tests/integration.rs` drives `router` and `TilesBlock` in-process on mock
engines, with no data files.
