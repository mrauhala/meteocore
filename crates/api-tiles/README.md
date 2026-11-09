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
  WebMercatorQuad or CRS84 for WorldCRS84Quad. `ds-render` colours, paints
  the background and encodes the result. An all-nodata tile skips encoding
  and gets a shared empty image of its background (transparent by default;
  white in JPEG).
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
  A segment that is not a non-negative integer is a 400 naming it.
- Tile matrix sets, tileset lists and tilesets are JSON only (`f=html` is a
  400) and take no parameter but `f` (another one is a 400). A request that
  asks for HTML gets its errors as an HTML page.
- The legend is JSON by default (palette stops, range, interpolation) or a
  180 × 300 PNG with `f=png`. `parameter-name` selects a parameter's style;
  for an RGB composite the legend lists the channels. A contour style's
  legend is a line swatch and `every 4 hPa`, its JSON a `contour` member.
- A contour style (#1378) draws its lines into the tile from a read one or
  two pixels past the tile's edges, so a line leaving one tile meets itself
  in the next: a 2 × 2 block of tiles is the tile above drawn at 512 × 512.
- The shared root's styled map tileset list and tileset take
  `parameter-name` besides `f`: a style only that parameter's layer offers
  (its built-in `contours`) is resolved there, and the list's links and
  tile templates carry it.

Collection metadata adds `dataType`, `crs`, `storageCrs`, `styles` (the
collection's own, then those only some parameter layers offer, such as the
built-in `contours`, with their `parameters`),
`parameter_names` and `tilesets-map`/`tilesets-vector` links. Its extent
describes the native grid as Maps does
([api-common](../api-common/README.md#native-grid)), and a collection that
keeps model runs lists them as its `reference_time` dimension, as Maps does
([api-maps](../api-maps/README.md#endpoints)). Tilesets give
`tileMatrixSetLimits` for zooms 0–18 from the extent. Catalog search and HTML
pages come from [api-common](../api-common/README.md).

## Tile parameters

| Parameter | Map tiles | Vector tiles |
|---|---|---|
| `f` | `image/png` (default), `image/jpeg`, `image/webp` | `mvt` or `application/vnd.mapbox-vector-tile` |
| `datetime` | Instant or interval (`a/b`, `../b`, `a/..`) | Same; filters features |
| `subset` | `datetime(…)`: quoted RFC 3339 values, partial dates, `*`; and `reference_time(…)` on a collection with model runs | `datetime(…)` only |
| `width`, `height` | Image size over the tile's area, ≤ 8000 a side, ≤ 64 M px | 400 |
| `scale-denominator` | Image size from the scale at the tile centre; not with `width`/`height` | 400 |
| `parameter-name` | A parameter or RGB composite; unknown: 400 listing valid names (single-band: ignored) | 400 |
| `elevation` | Vertical level; 400 on a collection without one | 400 |
| `quality` | 1–100 for JPEG and WebP; 400 for PNG | 400 |
| `bgcolor`, `transparent`, `void-color`, `void-transparent` | The Maps Background parameters, as on a map ([api-maps](../api-maps/README.md#map-parameters)); a tile never reaches outside its CRS, so the void ones change no pixel | 400 |

Any other query parameter, or `datetime` with `subset`, is a 400.

- **Map tile time.** No time: the engine's default step, normally the latest.
  An instant snaps to one of the engine's time steps; an interval, a partial
  date or `*` renders the latest step inside it. No step selected: 204 No
  Content. `OGCAPI-datetime` names the instant rendered.
- **Map tile model run.** `subset=reference_time("<run>")`, as on a Maps map
  ([api-maps](../api-maps/README.md#map-parameters)): the run's EDR instance
  id, or `*` for the latest. A named run is drawn exactly: the time is one
  of its own valid times, by default its latest, and a time outside it is
  an empty tile, 204. An unknown run is a 404, an interval of runs a 400,
  and the axis a 400 naming `datetime` on a collection without runs or on a
  vector tile.
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
  `EMPTY`). An instant the engine draws exactly, on an engine without model
  runs or from the run the tile names, that never revises content in place,
  is `public, max-age=86400, immutable`; everything else is
  `public, max-age=60, must-revalidate`: no time, an interval even when
  closed, an instant snapped to another time step, whose URL draws another
  frame once one lands or the window slides, or an instant of an engine that
  keeps model runs (GRIB, QueryData, Zarr and NetCDF forecasts, nowcast
  generations) with no run named, or `*`: a newer run re-covers the instant. A
  PostGIS lightning frame is revised until an hour before the newest strike.
  Instants are compared, not their
  text, so a step written with an offset is exact. The header is each request's
  own: a snapped and an exact request share the step's cached tile.
- Vector tiles carry `ETag`, `x-cache` and `Cache-Control: public, max-age=300`.
- Errors are JSON `{"code", "description"}`. 400: a bad parameter, an
  engine's request error, zoom above 24, or an unknown tile matrix set on a
  tile route. 404: unknown collection, style or tile matrix set, a tile
  outside the matrix, or what the engine reports absent. 422: a vector tile
  over 50 000 features. 500: generic message. 503 + `Retry-After: 1`: render
  slots, deadline or memory exhausted, or a busy engine. 503
  `SourceUnavailable` + `Retry-After: 10`: the data's object store kept
  answering 429, 502, 503 or 504 through all of its retries. Its
  description names no upstream; the server logs the error at WARN. Map and
  vector tiles answer an engine's error alike, by its class
  (`ds_core::error::ErrorClass`), as every API does.

## Conformance

OGC API - Common (core, landing-page, oas30, collections, json, html), Tiles
(core, tileset, tilesets-list, geodata-tilesets, datetime, png, jpeg, mvt) and
TMS 2.0 (tilematrixset, json-tilematrixset). The shared root adds Maps
`tilesets`, as its map tiles sit under the map resource and take the
parameters of the Maps Scaling and Background classes the Maps block
declares.

## Configuration

A collection serves tiles when its `apis` list includes `"tiles"`. Map tiles
come from `geotiff`, `grib`, `querydata`, `zarr`, `netcdf`, `satellite`,
`odim`, `odim-volume` (per-site collections), `nowcast`, `station-grid`,
`cap`, `csv`, `bufr` and `postgis`: its events shape, and a station shape
with map layers on (a `[postgis.map]` block, or `wms` or `maps` in `apis`).
Station layers draw one marker per station coloured by value; for CSV and
BUFR, `tiles` alone turns them on. Vector tiles come from `geojson`, `cap`
and the `postgis` station shapes.

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
| `[server] rendered_cache_mb` | 256 | Rendered-image cache size, shared with WMS and Maps. Unset, the deprecated `[wms] rendered_cache_mb` sizes it: the largest value among the WMS, Maps and Tiles collections |
| `MC_RENDER_TIMEOUT_MS` | 3000 | Queue and render deadline, MVT encoding included |
| `MC_RENDER_MEMORY_MB` | 1024 | Output-memory budget for map renders |

Runnable examples: `collections.d/municipalities.toml` (GeoJSON, vector
tiles) and `collections.d/zarr-era5-t2m-local.toml` (Zarr, map tiles).

## Limits

- Only WebMercatorQuad and WorldCRS84Quad; no projected tile matrix sets.
- No per-collection zoom range: zooms 0–24 are served, 0–18 advertised.
- Collection tilesets only; no dataset (multi-collection) tilesets.
- A run's own valid times are not in the Tiles description, as in Maps
  ([api-maps](../api-maps/README.md#limits)).
- Vector tiles carry every property in one layer: no property selection or
  styling. Their cache is 128 MB and not configurable.

## Testing

```bash
cargo test -p api-tiles
cargo test -p server --test common_discovery   # shared discovery, all APIs
cargo test -p server --test grib_reference_time   # model runs over real GRIB
```

Unit tests cover parameter validation, errors and tile matrix math.
`tests/integration.rs` drives `router` and `TilesBlock` in-process on mock
engines, with no data files.
