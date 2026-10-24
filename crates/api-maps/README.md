# api-maps

OGC API - Maps 1.0 for MeteoCore: renders a collection's raster data as a PNG,
JPEG or WebP map for the area, size, CRS and time a client asks for.

## What it solves

Raster engines keep gridded data in their own projections, time axes and
parameters, but a web client wants one picture. This crate turns an OGC API -
Maps request into one `MapEngine` read, colours it with the collection's style
and returns the image with the headers the standard requires. It knows no
engine type: it looks up the `MapEngine` by the collection id in the URL.

## How it works

- `router(state)` serves `/maps`; `MapsBlock` adds the same map routes to the
  [shared OGC API root](../api-common/README.md#shared-ogc-api-root). Both
  read one `MapsState` behind an `ArcSwap`, which a reload swaps.
- `params.rs` validates the request and resolves its frame: output CRS, area
  and pixel size (CRS and scale math in `api_common::map_frame`, shared with
  map tiles). An unknown query parameter, a cache-buster such as `_`
  included, is a 400 naming the accepted ones.
- The engine resolves the time step and model run it will render before the
  cache key is built, so a cached image is keyed on what it shows. The
  rendered-image cache is shared with WMS and Tiles; the ETag hashes the
  image bytes, and a matching `If-None-Match` gets a 304.
- On a miss, `ds-executor` admits the render (a render slot and an
  output-memory reservation, within a deadline). A blocking worker then calls
  `get_raster_tile` (or `get_raster_tiles` for an RGB composite), colours and
  encodes with `ds-render`.
- A view with no data returns an empty image in the requested format
  (transparent; white in JPEG) with `x-cache: EMPTY`.

## Endpoints

Paths are relative to `/maps`. The shared root serves the map, style and
legend routes too; api-common builds its other resources from this crate's
collection fields, links, conformance classes and OpenAPI paths.

| Path | Returns |
|---|---|
| `/` | Landing page (JSON or HTML) |
| `/api`, `/api/docs` | OpenAPI 3.0 definition (any parameter but `f` is a 400), Swagger UI |
| `/conformance` | Conformance classes (JSON or HTML) |
| `/collections` | Collections with a map engine (JSON or HTML) |
| `/collections/{id}` | Collection description (JSON or HTML) |
| `/collections/{id}/map` | Map in the `default` style |
| `/collections/{id}/styles` | Style list (JSON only; `f=html` or any parameter but `f` is a 400) |
| `/collections/{id}/styles/{styleId}/map` | Map in a named style |
| `/collections/{id}/styles/{styleId}/legend` | Legend, JSON (default) or PNG |

Metadata routes choose JSON or HTML from `f=json|html` or the `Accept` header
and answer with `Vary: Accept`. Collection search and paging come from
api-common; the HTML pages are its
[workbench](../api-common/README.md#html-api-workbench).

A collection description carries `dataType: "map"`, `crs` (the five output
CRSs), `styles` with map and legend links, `extent`, `storageCrs` when the
native CRS has an OGC URI, and `parameter_names` on a multi-parameter
collection. The extent describes the native grid
([api-common](../api-common/README.md#native-grid)):
- `storageCrsBbox` and a `grid` in the `storageCrs` when the source states
  that code;
- the `meteocore:nativeGrid` member for any grid that is not lat/lon. The per-API service adds a `tilesets-map` link to
`/tiles/collections/{id}/tiles` when the Tiles service renders the collection.

`/conformance` lists the OGC API - Common classes and these OGC API - Maps
1.0 classes: `core`, `collection-map`, `styled-map`, `spatial-subsetting`,
`scaling`, `datetime`, `crs`, `png`, `jpeg`. Map Tilesets is declared at the
shared root by api-tiles, which serves those tiles.

## Map parameters

| Parameter | Meaning |
|---|---|
| `bbox`, `bbox-crs` | Area: four values in the CRS's axis order. West greater than east crosses the antimeridian. |
| `subset`, `subset-crs` | Area by axis, e.g. `Lon(19:32),Lat(59:70)`, or `E`/`N` in a projected CRS. `*` or a left-out axis keeps the extent. Also `time(…)`. |
| `center`, `center-crs` | Area around a point, at the native resolution or `scale-denominator`. |
| `width`, `height` | Output size in pixels (see below). |
| `scale-denominator` | One pixel spans scale × 0.28 mm on the ground at the map centre. |
| `crs` | Output CRS: `CRS:84`, `EPSG:4326`, `EPSG:3857`, `EPSG:3067`, `EPSG:3035`, as code, URI or safe CURIE. Default: the `storageCrs` if it is one of these, else CRS84. |
| `datetime` | An instant or an interval (`a/b`, `../b`, `a/..`). |
| `f` | `image/png` (default; 8-bit indexed at up to 256 colours, else RGBA), `image/jpeg` or `image/webp`. |
| `quality` | 1–100 for JPEG (default 85) and WebP (100 is lossless; default `[wms] webp_quality`, else lossless). A 400 with PNG. |
| `parameter-name` | A parameter or RGB composite from `parameter_names`. An unknown name is a 400 listing the valid ones; a collection without a parameter list ignores it. |
| `elevation` | One vertical level. A 400 on a collection without a vertical dimension. |
| `transparent` | Accepted and ignored: there is no opaque-background option. |

- **Area.** At most one of `bbox`, a spatial `subset` and `center`. Without
  any, the map covers the collection's extent; a collection with no extent yet
  is a 404.
- **Size.** Over an area, an omitted `width` or `height` keeps pixels square,
  and with both omitted the longer side is 1024. `scale-denominator` over an
  area derives the size instead, and is a 400 with `width` or `height`. With
  `center`, or `scale-denominator` and no area, the size sets the extent; an
  omitted side takes the other's value, else 1024. Every size, given or
  derived, is capped at 8000 a side and 64 000 000 pixels.
- **Time.** `datetime` or `subset=time(…)`, not both. The engine snaps an
  instant to one of its time steps (a parameter's own axis when it has one).
  An interval renders the latest step inside it, and is a 404 when it holds
  none. Without a time, the engine's default time is used, else the latest.
- **Headers.** `Content-Crs`, `Content-Bbox` (the rendered corners in the
  CRS's axis order), `Content-Datetime` (the instant rendered, on collections
  with a time axis), `ETag` and `x-cache` (`HIT`, `MISS` or `EMPTY`).
  `Cache-Control` is `public, max-age=86400, immutable` for an instant that
  resolved to a time step on content the engine never revises in place, and
  `public, max-age=60, must-revalidate` otherwise.

## Styles and legends

Styles are the collection's `[wms]` styles, the same ones WMS and Tiles serve:
`default` plus every named style. With `parameter-name`, the style comes from
that parameter's own layer when one is configured; without it, on a
multi-parameter collection, from the default parameter's layer. An RGB
composite has only the `default` style; any other style is a 404.

The legend takes `f` (`json`, the default, or `png`; media types work too) and
`parameter-name`. JSON gives the palette stops, value range and interpolation
for a client to draw its own legend; PNG is a 180 × 300 image. A composite's
legend lists its channels. Legends are cached for a day but not `immutable`,
because palettes reload.

## Errors

Errors are JSON `{"code": …, "description": …}`, or a page if HTML was asked:

| Status | Code | When |
|---|---|---|
| 400 | `BadRequest` | Bad or unknown parameter; engine `InvalidParameter`, `InvalidBbox`, `InvalidDatetime` or `QueryTooLarge` (with its message) |
| 404 | `NotFound` | Unknown collection or style; no time step for the requested time; a model run gone before rendering |
| 503 | `ServerBusy` | Render queue full, deadline passed, or an engine budget exhausted; with `Retry-After: 1` |
| 500 | `ServerError` | Anything else, with a generic message |

## Configuration

A collection is served when its engine implements `MapEngine` and its `apis`
lists `"maps"`. The engine types that allow it are `geotiff`, `grib`,
`querydata`, `zarr`, `netcdf`, `satellite`, `odim`, `odim-volume`, `cap`,
`nowcast`, `station-grid`, `postgis`, `csv` and `bufr`. A PostGIS events
table draws as an age-coloured layer; a CSV, BUFR or PostGIS station
collection draws one layer per parameter, a marker per station coloured by
its value.

```toml
[[collections]]
id = "radar"
title = "Radar reflectivity"     # title and description are required
description = "Radar composite"
engine_type = "geotiff"
apis = ["wms", "maps", "tiles"]
# ... plus the engine's own block, e.g. [collections.geotiff]

[collections.wms]          # styles for WMS, Maps and Tiles
colormap = "radar_dbz"
# webp_quality = 80        # default WebP quality; unset = lossless
```

Every raster example under [`collections.d/`](../../collections.d/) enables
`maps`. [zarr-era5-t2m-local.toml](../../collections.d/zarr-era5-t2m-local.toml)
runs offline on a fixture in `testdata/`. Server-wide settings for Maps:

| Setting | Default | Effect |
|---|---|---|
| `[server] render_concurrency` | 2 × CPUs, min 8 | Render slots shared with WMS, Tiles and 3D Tiles |
| `MC_RENDER_QUEUE_CAPACITY` | 3 × render slots | Renders that may wait for admission; past it a 503 at once |
| `MC_RENDER_MEMORY_MB` | 1024 | Output-memory budget: 32 bytes per output pixel, 16 more per extra composite band |
| `MC_RENDER_TIMEOUT_MS` | 3000 | Queue and render deadline |
| `[wms] rendered_cache_mb` | 256 | Size of the one rendered-image cache shared by WMS, Maps and Tiles; read from one collection's `[wms]` block, so give every block the same value |

## Limits

- No `reference_time` parameter: a forecast collection renders the model run
  the engine picks by default.
- Maps are two-dimensional: a six-value `bbox` or a spatial slice such as
  `Lat(60)` is a 400, and `elevation` selects one level.
- At the default `MC_RENDER_MEMORY_MB`, a map over 33 554 432 pixels (about
  5793 × 5793) is always a 503. Raise the budget to serve larger maps.

## Testing

```bash
cargo test -p api-maps
cargo test -p server --test common_discovery
cargo test -p server --test satellite_composites --test grib_parameter_times
```

The crate's tests drive the router with mock `MapEngine`s (no data files) and
validate `/api` against `schemas/openapi-3.0.json`. `common_discovery` checks
the Common behaviour of `/maps` and the shared root; `satellite_composites`
(`testdata/goes19-abi`) and `grib_parameter_times` (`testdata/grib-arpege-wind`)
render real data through Maps.
