# api-features

OGC API - Features for MeteoCore: GeoJSON and HTML access to every collection
whose engine implements `FeatureEngine`.

This page is the Features support-status page: what works per conformance
class, `/items` parameter and engine. Update it with every change to Features
behaviour, including an engine's `FeatureEngine` implementation. Contributor
rules are in [CLAUDE.md](CLAUDE.md).

## What it solves

Much weather data is a set of discrete objects rather than a grid: observation
stations, radar sites, warning areas, tracked storm cells, boundary polygons.
Clients want them as standard GeoJSON that they can filter by area, time and
property, and page through. This crate serves them as OGC API - Features
Part 1 (Core) and Part 2 (CRS by reference), so any Features client works,
while the engines stay free of HTTP and JSON.

## How it works

- An engine implements `ds_core::feature_engine::FeatureEngine`. It answers a
  `FeatureQuery` (`bbox`, `datetime`, `sortby`, property filters, `limit`,
  `offset`) with a `FeaturePage` of domain `Feature`s: an id, CRS84 geometry
  and typed `PropertyValue` properties.
- The handlers check each parameter against what the engine advertises
  (`filterables`, `sortables`, `has_time_dimension`). An unsupported
  parameter is a 400, never ignored; where there is a list of valid values,
  the message names it.
- The engine filters, sorts and pages. Sorting happens before paging, through
  `ds_core::feature::sort_features`: missing and null values sort last in
  both directions, and ties break on feature id.
- This crate encodes the response: GeoJSON, or an HTML page built from the
  same GeoJSON document. For the Part 2 `crs` parameter it reprojects
  geometry per vertex with `ds_core::web_mercator` and `ds_core::geo`.
- An engine can also say how to present its features: `label` names a
  feature, `summary_properties` picks a listing's first columns,
  `items_description` says in one line what an item is, and
  `Feature::related` names resources a feature leads to. The engine names
  them; this crate builds the URLs and drops a link that no API here
  serves. See
  [Labels, columns and related links](#labels-columns-and-related-links).
- State is `FeaturesState`: engines and collection configs keyed by
  collection id, the base URL and proxy setting, the ids that api-tiles
  serves as vector tiles, and the pages of the collections a feature may
  link to. It sits behind an `ArcSwap`, so a reload swaps it in one step.
- [api-common](../api-common/README.md) supplies collection discovery, Common
  metadata, the HTML workbench pages, HTML error pages and conditional GET.
- The `/items` and `/items/{featureId}` handlers serve two mounts: the
  per-API service at `/features` (`router`) and the `FeaturesBlock` building
  block of the shared OGC API root at `/`.
- api-edr reuses `params`, `response` and `crs` for EDR `items`, so changes
  there also change EDR.

## Routes

| Route | Formats | Notes |
|---|---|---|
| `/features/` | JSON, HTML | landing page |
| `/features/api` | OpenAPI 3.0 | hand-written; the collection, items and item operations of every collection. Takes only `f`: another parameter, or `f` given twice, is a 400 |
| `/features/api/docs` | HTML | Swagger UI from embedded assets under `/features/api/docs/`; no CDN |
| `/features/conformance` | JSON, HTML | |
| `/features/collections` | JSON, HTML | collections that have a feature engine; Common collection search |
| `/features/collections/{id}` | JSON, HTML | `itemType: "feature"`, `crs`, `storageCrs` (CRS84), `numberItems`, `extent`, an `items` link, and a `tilesets-vector` link when api-tiles serves the collection as vector tiles; WIS2 topic `themes`, configured or from a WIS2 subscription, and WCMP2 `describedby` links, configured or learned from WIS2 notifications (#1356) |
| `/features/collections/{id}/items` | GeoJSON, HTML | a page of features |
| `/features/collections/{id}/items/{featureId}` | GeoJSON, HTML | one feature; takes only `f` and `crs` |

`/collections` takes `bbox`, `bbox-crs` (CRS84 only), `datetime`, `q`,
`query`, `sortby` (`title` or `id`), `limit`, `offset` and `f`. Unknown or
repeated parameters are a 400. A collection's extent comes from the engine's
`spatial_extent` and `temporal_extent`, and a collection without one matches
any `bbox` or `datetime`. The search rules are in
[api-common](../api-common/README.md).

## Conformance classes

| Class | Declared | Notes |
|---|---|---|
| Part 1 `core` | yes | including the optional property equality filters |
| Part 1 `oas30` | yes | `/api`, validated against the OpenAPI 3.0 meta-schema in tests |
| Part 1 `geojson` | yes | the default encoding |
| Part 1 `html` | yes | metadata, item listings and single features |
| Part 2 `crs` | yes | `crs`, `bbox-crs`, `Content-Crs`; see [CRS by reference](#crs-by-reference) |
| Part 1 `gmlsf0` / `gmlsf2` | no | no GML encoding |
| Part 3 Filtering (CQL2) | no | `filter`, `filter-lang`, `filter-crs` are a 400; no `/queryables` |
| Part 4 Create, Replace, Update, Delete | no | read-only |
| Part 5 Schemas | no | no `/schema` |
| Part 8 Sorting (draft) | no | `sortby` works on `/items`; the class and `/sortables` are absent |

`/conformance` also lists OGC API - Common Part 1 (`core`, `landing-page`,
`oas30`) and Part 2 (`collections`, `json`, `html`). Common Part 4
`searchable-collections` is not declared; see the
[Common matrix](../../docs/ogc-api-common-matrix.md).

## Parameters on `/items`

| Parameter | Behaviour |
|---|---|
| `bbox` | 4 values, or 6 with the heights ignored, in `bbox-crs` axis order. In CRS84 and EPSG:4326 a west edge east of the east edge crosses the antimeridian. Malformed values, a south edge above the north edge, and latitudes or longitudes out of range are a 400 |
| `bbox-crs` | CRS of `bbox`, one of the collection's `crs` values; default CRS84. The engine always gets a CRS84 box. Checked even without `bbox` |
| `datetime` | RFC 3339 instant, `start/end`, `../end`, `start/..` or `../..`. A reversed interval is a 400. On a collection without a time dimension it is a 400 |
| `limit` | default 100; a value outside 1–1000 is clamped. A value that is not a non-negative integer is a 400 |
| `offset` | default 0; a value that is not a non-negative integer is a 400 |
| `sortby` | `[+\|-]property,…`. Every property must be in the engine's `sortables`, else a 400 naming the valid ones. A `+` decoded to a space counts as ascending. A property listed twice is a 400 |
| `crs` | response CRS, one of the collection's `crs` values; default CRS84. HTML takes CRS84 only |
| `f` | `json` or `html` (also `application/json`, `text/html`), case-insensitive. Feature routes also take `application/geo+json` (send `+` as `%2B`). Overrides `Accept` |
| `properties` | not supported: a 400 |
| `<property>=<value>` | Part 1 property equality filter; see below |

Each control parameter may appear once; a repeat is a 400.

Property filters:

- A name must be in the engine's `filterables`; an unknown name is a 400 that
  lists the valid ones. The control names above cannot be property filters.
  `/features/api` lists each collection's filter names as query parameters.
- Names are case-sensitive and matching is exact: no wildcards, substrings or
  type coercion. Numbers match their canonical form (`12`, `1.5`), booleans
  match `true` or `false`.
- On a numeric property, `size=10,30` means 10 or 30. In a string property
  the comma is literal.
- A list property matches when any element matches. Missing, null and
  non-finite values never match.
- All predicates, repeated names included, are ANDed with each other and
  with `bbox` and `datetime`, before counting, sorting and paging.

Example: `/features/collections/cap-alerts/items?severity=Severe&limit=10`.

### CRS by reference

Engines hold CRS84 geometry. `crs` transforms the output and `bbox-crs` the
input, in `src/crs.rs`, with the same projection definitions that Maps
renders with. A collection's `crs` list is exactly what `/items` accepts.

| CRS | URI | Axis order |
|---|---|---|
| CRS84 (default, `storageCrs`) | `http://www.opengis.net/def/crs/OGC/1.3/CRS84` | longitude, latitude |
| EPSG:4326 | `http://www.opengis.net/def/crs/EPSG/0/4326` | latitude, longitude |
| EPSG:3857 | `http://www.opengis.net/def/crs/EPSG/0/3857` | easting, northing |
| EPSG:3067 | `http://www.opengis.net/def/crs/EPSG/0/3067` | easting, northing |
| EPSG:3035 | `http://www.opengis.net/def/crs/EPSG/0/3035` | northing, easting |

- Accepted forms: the URI, its `https` form, the CURIE with or without
  brackets (`EPSG:3067`, `[OGC:CRS84]`) and `CRS84`. Anything else is a 400
  listing the URIs.
- Axis orders are EPSG's, as PROJ `cs2cs` prints them. WMS takes an
  EPSG:3035 `BBOX` easting first.
- Every `/items` and `/items/{featureId}` response carries `Content-Crs`,
  CRS84 included.
- Every vertex is reprojected. A geometry with a vertex that has no finite
  coordinates in the CRS, such as the South Pole in EPSG:3857, is `null`.
- An EPSG:3857 `bbox` maps exactly; a box past ±180° or with `minx > maxx`
  crosses the antimeridian. An EPSG:3067 or EPSG:3035 `bbox` (lower corner
  first) becomes the CRS84 envelope of densely sampled edge points, widened
  to a pole inside the box. Near the box corners, it can also match features
  just outside the projected rectangle. A box wholly outside the area the CRS
  can represent is a 400.
- Links repeat a requested `crs`. They carry `bbox` as its CRS84 conversion,
  without `bbox-crs`.

## Responses

- `/items` is a GeoJSON `FeatureCollection` with `numberMatched`,
  `numberReturned`, `timeStamp` and `self`, `next` (when more remain), `prev`
  (when `offset` > 0) and `alternate` links. Past the end, `prev` is the last
  page that has features.
- Each feature has `self` and `collection` links, plus a `related` link for
  each resource its engine names and an API here serves.
- A feature's `title` property is its engine's label, unless the feature
  has a `title` of its own. EDR `items` carries the same `title`, without
  the `related` links.
- Paging links carry `offset`, `limit`, the explicit format and the parsed
  `bbox`, `datetime`, `sortby`, property filters and `crs`. Following `next`
  keeps the same query.
- Geometry types are `Point`, `Polygon`, `MultiPolygon` and `null`.
- Feature ids are opaque strings. Links percent-encode an id as one path
  segment. Build URLs the same way, or follow the links.
- Content negotiation: `f` first, then `Accept`. `text/html` or `text/*`
  selects HTML; `*/*` stays JSON. Negotiated responses send `Vary: Accept`.
- Every 200 has a strong ETag, and `If-None-Match` gets a 304. The `/items`
  ETag ignores `timeStamp`. Cache-Control is `public, max-age=60` by default.
  An `/items` request whose closed `datetime` interval ended at least an hour
  ago gets `public, max-age=86400`.
- Errors are `{"code", "description"}`, and a 500 never shows internal
  detail. A request that asked for HTML gets the error as an HTML page with
  the same status. A rejected `/items` HTML request returns the items page
  with the request form filled in and the faulty field marked.
- An engine's own error on `/items` or `/items/{featureId}`: a request it
  turns down (`QueryTooLarge`, such as a station grid's analysis cap) is a
  400 with its message; one it cannot answer yet or now
  (`ResourceExhausted`, `DeadlineExceeded`: a station grid whose residuals
  are still being computed) is a 503 with `Retry-After: 1`; an object
  store that kept refusing the read (`Unavailable`) is a 503
  `SourceUnavailable` with `Retry-After: 10`; a missing feature is a 404;
  anything else is a 500.

## HTML

Metadata, item listings and single features render as pages of the shared
workbench (see [HTML API workbench](../api-common/README.md#html-api-workbench)).
An item listing has:

- a request builder, open when the request filters, sorts or was rejected.
  Its groups follow what the engine advertises: property equality, time
  (two UTC pickers that fill `datetime`, `..` for an open end), area
  (`bbox`, or **Use map view** for the map's current view), order (a
  sortable property and a direction) and paging;
- a property table of up to eight selectable columns. By default it shows up
  to four that have values on the page: the engine's `summary_properties`,
  else its sortables and then the other properties alphabetically, without
  labels, ids and coordinates. The browser's `localStorage` keeps a choice
  per collection id;
- a MapLibre locator map of the page.

The map draws only the returned page, simplified for display, over locally
bundled Natural Earth outlines. It makes no extra feature or tile requests.
A feature page lists every property with its JSON type, and shows its
`related` links as buttons. Feature titles come from a `name`-like property
(an engine's label arrives as `title`), else the id.

A collection's overview page adds the engine's `items_description` under
its title. Its request guide's Features entry gives the first 10 items as a
ready request, the request builder, and two chip rows: the properties its
items page filters and sorts by. A chip opens the items page with the
builder set to that property, through the URL fragment (`#filter=<name>`,
`#sort=<name>`), never a query parameter.

## Shared OGC API root

`FeaturesBlock` adds `/collections/{id}/items` and
`/collections/{id}/items/{featureId}` to the shared root at `/`, with the same
handlers, parameters and conditional GET.

- Only collections with a feature engine get `itemType: "feature"` and two
  `items` links, GeoJSON and HTML. A map-only collection's `/items` is a 404.
- Blocks merge in the order Maps, Tiles, Features. An earlier block's
  `extent` and `dataType` win, and the `crs` lists are merged. A
  Features-only collection is `dataType: "vector"` with a CRS84
  `storageCrs`.
- The items operations join the shared `/api` with components named
  `features-bbox`, `features-limit` and so on, because Maps defines its own
  `bbox`, `datetime` and `link`.
- The root's `/conformance` adds Features `core`, `oas30`, `geojson`, `html`
  and Part 2 `crs`.

## Per-engine matrix

"400" means the API rejects the parameter for that engine. Extents feed
collection metadata and collection search. `data_version` is part of the
api-tiles vector-tile cache key.

| Engine | Feature | `bbox` | `datetime` | `sortby` | Property filters | Spatial / temporal extent | `data_version` |
|---|---|---|---|---|---|---|---|
| CSV | one station (Point), in first-observation order | point inside the box | stations with an observation in the interval | 400 | `name`, `latitude`, `longitude` | bounding box of every row / first to last observation | 0 (static) |
| GeoJSON | each feature of the file | geometry intersects the box | 400: no time dimension | 400 | every property name in the loaded file | yes / – | the file's modification time |
| CAP | one alert area (Polygon, MultiPolygon or null) | area's bounding box intersects the box; null geometry excluded | alert active window intersects the interval | 400 | the standard CAP alert, info and area properties and derived ones (always), plus producer parameter and `eventCode:` names from loaded alerts | yes / earliest window start to latest window end, extended to the catalog time; none when every window is open | changes with the alert set |
| PostGIS stations | one station (Point) from the cached station list | point inside the box | 400: no time dimension | 400 | configured `stations.property_cols` (always), plus names the loaded rows carry | yes / – | changes per metadata refresh |
| BUFR | one station (Point) | point inside the box | stations with a report in the interval | `last_report`, `first_report`, `report_count`, `name` | `name`, `wigos_station_identifier`, `elevation`, `first_report`, `last_report`, `report_count` | yes / oldest to newest report | changes per snapshot |
| ODIM PVOL network | one radar site (Point), the site inventory | point inside the box | sites with a retained volume in the interval | 400 | site properties (below) | yes / earliest to latest retained volume | changes with the inventory |
| Nowcast | one tracked storm cell (Point) of one generation | cell point inside the box | the newest retained generation inside the interval; none inside: no features. Without `datetime`: the latest | cell facts (below) | every cell property except `significance_contributions`, including the groups of wired sources | yes / retained history | changes every generation |
| Station grid | one station's leave-one-out residual (Point) for one parameter and analysis time | station point inside the box | every analysis time inside the interval, at most 96 analyses (times × parameters; else 400); an instant that is no analysis time: no features. Without `datetime`: the latest analysis time | `residual`, `estimate`, `value`, `neighbours`, `time`, `parameter` | `parameter`, `time`, `value`, `estimate`, `residual`, `neighbours` | the grid widened by the search radius / first to last analysis time | its content version |

- Every `bbox` test above handles a box that crosses the antimeridian.
- ODIM PVOL site filters: `nod`, `name`, `wmo`, `longitude`, `latitude`,
  `antenna_height_m`, `quantities`, `elevation_angles`,
  `coverage_radius_m`, `latest_volume_time`, `volume_count`, `collection`.
- Nowcast sorts on `significance`, `significance_rank`, `max_dbz`,
  `area_km2`, `track_age`, `speed_ms`, `bearing_deg`,
  `intensity_trend_dbz_min`, `net_displacement_km`, `path_straightness` and
  `likely_clutter`, plus lightning, impact and radar fields when those
  sources are wired. It keeps the last 48 generations (4 hours at a 5-minute
  cadence). `/items/{featureId}` returns the cell as it is in the latest
  generation.
- PostGIS `events` collections have no Features surface: `apis = ["features"]`
  on them is a config error.
- An ODIM volume source with `features` in `apis` serves the site inventory
  under its own id. The per-site radar collections are not feature
  collections.
- Station grid (#1206): `estimate` is the analysis at the station from the
  other stations, by the grid's method, neighbours and radius; `residual`
  is `estimate − value`, null with no other station in reach; `neighbours`
  counts the stations behind the estimate. A lapse-rate corrected
  parameter's estimate comes from the other stations' sea-level values and
  is put back at the station's own height; a station `exclude_where`
  excludes, or one without a height, has no residual, and the land/sea
  mask removes none. They are computed with each
  grid on the station-grid pool and cached with it. `/items` never waits
  for a computation: an analysis not computed yet is a 503 with
  `Retry-After: 1` while it is computed, and the retry is served from the
  cache. The latest `precompute` analysis times are kept ready. A
  `parameter` or `time` filter narrows the analyses read and counted
  against the cap; `datetime` picks the analysis times. Ids are
  `<parameter>@<time>@<lon>,<lat>`. The collection's EDR `items` query is
  not wired: its EDR data is the grid.
- GeoTIFF, GRIB, QueryData, Zarr, NetCDF, Satellite and ODIM composite
  collections have no `FeatureEngine`.

### Labels, columns and related links

| Engine | Label (`title`) | First columns | `related` link |
|---|---|---|---|
| CSV | – | default | the station's EDR location |
| GeoJSON | – | default | – |
| CAP | `event · areaDesc`, the parts the area has | `severity`, `onset`, `active_until`, then `awareness_level` or `urgency` | – |
| PostGIS stations | – | default | the station's EDR location |
| BUFR | – | default | the station's EDR location |
| ODIM PVOL network | – | default | the site's own collection |
| Nowcast | `Rank 2 · very severe · max 52 dBZ` from `significance_rank`, `severity` and `max_dbz`, plus `· likely clutter` when flagged | `significance`, `severity`, `max_dbz`, `area_km2` | – |
| Station grid | `temperature at 60.25, 24.95`: the parameter and the station's latitude and longitude | `residual`, `value`, `estimate`, `neighbours` | – |

- Without a label, a page titles a feature by its `name`-like property, else
  its id. "Default" columns are the sortables, then the other properties
  alphabetically.
- An EDR location link needs `edr` in the collection's `apis`. A radar
  site's collection link goes to the shared root when Maps or Tiles serve
  the site, else to EDR, else it is left out.
- The ODIM PVOL network's overview says what an item is: "Radar site
  inventory: one point per radar. Each site's volumes are in its own
  collection." So does a station grid's: one point per station, parameter
  and analysis time, and what `estimate` and `residual` are. No other
  engine sets `items_description`.
- The nowcast label's rank orders a heuristic significance, and the
  severity class is the engine's own. Neither is a warning.

## Configuration

The crate has no config section of its own. List `features` in a
collection's `apis`. The engine types that accept it are `csv`, `geojson`,
`cap`, `bufr`, `postgis` (station shapes), `odim-volume`, `nowcast` and
`station-grid`.

```toml
[server]
# base_url = "https://api.example.com"  # absolute links behind a proxy
# trust_proxy_headers = true            # links from X-Forwarded-* / Forwarded

[[collections]]
id = "weather"
title = "Finnish Weather Observations"
data_path = "testdata/weather.csv"
engine_type = "csv"
apis = ["edr", "features"]
```

Runnable examples in `collections.d/`: `weather.toml` (CSV), `regions.toml`
and `municipalities.toml` (GeoJSON), `cap-alerts.toml` and
`cap-meteoalarm-wis2.toml` (CAP), `obs-bufr-local.toml` and
`obs-synop-wis2.toml` (BUFR), `radar-fi-nowcast.toml` (Nowcast) and
`weather-analysis.toml` (Station grid).

## Limits

- `limit` is at most 1000. Paging is by `offset`. Responses are buffered,
  not streamed.
- Geometry is Point, Polygon, MultiPolygon or null: no lines and no
  multi-points. There is no `properties` selection. The classes marked "no"
  under [Conformance classes](#conformance-classes) are not implemented.
- Part 2: no global CRS list on `/collections`, no
  `storageCrsCoordinateEpoch`, no 3D CRS.
- The HTML representation is CRS84 only.

## Testing

```bash
cargo test -p api-features
```

The tests run the router against in-memory mock engines; they need no data
files.

- `ogc_features_api_tests.rs`: routes, links, paging, `bbox`, `datetime`,
  `f` and `Accept` negotiation, errors, the vector-tileset link, and engine
  labels, columns and overview in HTML.
- `sorting_tests.rs`: `sortby` validation and order. It also validates
  `/api` against `schemas/openapi-3.0.json`.
- `crs_tests.rs`: Part 2, with expected coordinates taken from PROJ `cs2cs`.
- `caching_tests.rs`: ETag, `If-None-Match` and Cache-Control.
- `engine_errors_tests.rs`: an engine's errors on `/items` and
  `/items/{featureId}` and their statuses.

Shared code paths have tests elsewhere: run `cargo test -p api-edr --test
items_tests` for EDR `items`, `cargo test -p server --test
common_discovery` for the shared root and collection discovery, and
`cargo test -p server --test station_grid_features` for a station grid's
residuals and a CSV station collection's extent over HTTP.
