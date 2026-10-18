# engine-geojson

Serves one GeoJSON FeatureCollection file through OGC API - Features and as
Mapbox Vector Tiles.

## What it solves

Reference geometry, such as administrative boundaries, catchments or service
areas, often comes as a GeoJSON file. This engine publishes such a file
as a queryable, pageable feature collection and as vector tiles for a web
map, with no database. The same collection can feed other collections, for
example as the area layer a nowcast collection names in `impact_source`.

## How it works

- `GeoJsonEngine::load` parses the whole file once, at startup or on reload,
  and keeps every feature in memory as ds-core `Geometry` and
  `PropertyValue` types behind `Arc`s. A response shares the stored
  geometry and properties instead of copying them.
- An R-tree (`rstar`), bulk-loaded from each feature's bounding box, finds
  the `bbox` candidates. Each candidate's own geometry is then tested
  against the box, so a feature whose envelope overlaps the box but whose
  shape does not is left out. A box with west > east is split at the
  antimeridian.
- Property filters are applied before counting and paging, so
  `numberMatched` counts only matching features.
- Results come in file order.
- The feature id is the feature's top-level `id` (string or number), or its
  0-based position in the file when there is none. With duplicate ids,
  `/items` lists every feature and `/items/{featureId}` returns the last
  one.
- The filterable property names are every name that appears in at least one
  feature, collected once at load.
- The file's modification time is the engine's data version. Vector-tile
  cache keys include it, so tiles cached before an edit are not served after
  the reload.
- There is no poll loop. Every config reload (`POST /admin/collections/reload`
  or the `collections_dir` watcher) re-reads the file, even when the
  collection's config is unchanged. Editing the file alone triggers nothing.

## What it serves

The engine implements `FeatureEngine` only.

| API | Routes | Result |
|---|---|---|
| Features | `/features/collections/{id}/items`, `/items/{featureId}`; also at the shared root `/collections/{id}/items` | GeoJSON or HTML, paged with `limit`/`offset` |
| Tiles | `/tiles/collections/{id}/tiles/{tms}/{z}/{row}/{col}?f=mvt`; shared root `/collections/{id}/tiles/{tms}/{z}/{row}/{col}` | MVT with one layer named after the collection id, carrying every property |

`{tms}` is `WebMercatorQuad` or `WorldCRS84Quad`.

- Features `/items` accepts `bbox` and equality filters on any property
  name in the file, for example `?name=Helsinki`; another name is a 400.
  Filters combine with AND. A list property matches when any element
  matches; a null value never matches.
- Property values keep their JSON type: string, integer, float, boolean,
  null, or a list for a flat array of those. An object, or an array that
  contains arrays or objects, becomes its JSON text as a string.
- A feature with `"geometry": null` is listed by `/items` but never matches
  a `bbox` and is not part of the spatial extent.
- The collection's spatial extent is the union of the feature bounding
  boxes. It has no temporal extent.

The Features parameters and their semantics across engines are on the
Features status page:
[Per-engine matrix](../api-features/README.md#per-engine-matrix).

## Configuration

There is no engine-specific table; the collection's top-level fields are
enough.

```toml
[[collections]]
id = "municipalities"
title = "Finnish Municipalities"
description = "Municipality boundaries"        # required, like title
engine_type = "geojson"                        # the default is "csv"
data_path = "testdata/municipalities.geojson"  # required; a local file
apis = ["features", "tiles"]                   # the default ["edr"] fails
```

- `data_path` is a local file path. A relative path resolves against the
  server's working directory.
- `apis` accepts only `features` and `tiles`. Any other API, including the
  default `edr`, makes the collection fail to load.
- The runnable examples are
  [`collections.d/municipalities.toml`](../../collections.d/municipalities.toml)
  and [`collections.d/regions.toml`](../../collections.d/regions.toml):
  Finnish municipality and region boundaries from Statistics Finland at
  1:1M scale.
- `server --auto-collections <DIR>` creates one such collection, with
  Features and Tiles, per `.geojson` file in `<DIR>` and its immediate
  subdirectories. A directory that is a Zarr store, or that also holds
  QueryData, GRIB2, GeoTIFF or HDF5 files, yields no GeoJSON collection.

## File format

- A GeoJSON `FeatureCollection` with a `features` array. Any other top-level
  type fails the load.
- Coordinates are WGS84 longitude, latitude in degrees. A value outside
  -180..180 or -90..90 fails the load with a message asking for WGS84
  data, so a file in a projected CRS is rejected. A `crs` member is not
  read. Convert other data first, for example
  `ogr2ogr -f GeoJSON -t_srs EPSG:4326 out.geojson in.geojson`.
- Geometry types: `Point`, `Polygon`, `MultiPolygon`, or `null`. Every
  feature needs a `geometry` member.
- Only the first two values of a position are read; an altitude is dropped.

A single invalid feature fails the whole file. The collection is then
reported as failed in `/health`, and the other collections load as usual.

## Limits

- Local files only. The whole file is parsed and held in memory.
- At most 500 MiB per file and 1 000 000 features. A `MultiPolygon` may have
  at most 100 000 coordinates in total; `Point` and `Polygon` have no
  coordinate cap.
- `LineString`, `MultiPoint`, `MultiLineString` and `GeometryCollection`
  are not supported and fail the load.
- No time dimension: Features answers `datetime` with a 400. Vector tiles
  accept a time and ignore it, so every feature matches.
- No Features `sortby` (400).
- Nested property values are strings and cannot be filtered as structures.
- Edits to the file are served only after a reload.
- A vector tile with more than 50 000 features is a 422. Tile geometry is
  not simplified.

## Testing

```bash
cargo test -p engine-geojson
cargo test -p server --test geojson_feature_properties
cargo bench -p engine-geojson
```

- The unit tests in `src/loader.rs` write small inline FeatureCollections
  to temporary files. They cover geometry and property parsing, id
  fallback, `bbox` matching against geometries (including a box across the
  antimeridian), property filters before paging, paging, the extent, and
  rejection of projected coordinates and non-FeatureCollection input.
- The server test loads a file through the real engine and checks that
  typed and array properties survive into the Features JSON and HTML
  responses and into property filters.
- The Criterion benchmarks load and query
  `testdata/municipalities.geojson` (308 features).
