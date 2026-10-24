# api-edr

OGC API - Environmental Data Retrieval (EDR) 1.2 for MeteoCore, mounted at
`/edr`. This page is also the EDR support-status page: which conformance
classes, query types, parameters and engines are supported, and what is not.

## What it solves

Weather data comes as model grids, radar volumes and composites, satellite
scans and station observations, each in its own format. Clients want to ask
the same simple questions of all of it: the forecast at a point, the
observations inside a polygon, the values along a flight path. This crate
answers those questions as standard EDR queries over every collection whose
engine implements `EdrEngine`, and encodes the answer as CoverageJSON, EDR
GeoJSON, CSV, a PNG plot or an HTML page.

## How it works

- `api_edr::router` builds an axum router that the server nests at `/edr`.
  Its state is a registry of `EdrEngine`s keyed by collection id, swapped
  atomically on reload.
- Engines return domain types; this crate validates the request, picks the
  output format and encodes the result. CoverageJSON is built in
  `src/response.rs`, EDR GeoJSON in `src/geojson.rs`, CSV in `src/csv.rs`,
  HTML pages in `src/html.rs`, and PNG plots through `ds-render`.
- Engines are synchronous. Every data query runs on the shared query runtime
  of `ds-executor`, never on an HTTP worker: one running query per runtime
  worker (2 to 8, from the CPU count), 32 more waiting, and a 30-second
  deadline that includes the wait. A full queue answers 503 at once; a
  missed deadline answers 504. Radar cross-sections run on the same
  runtime's blocking pool. Response bodies are encoded there too, under
  the response memory budget (see [Responses and limits](#responses-and-limits)).
- `params::query_formats` is the one list of formats each query offers. The
  handlers negotiate over it and `data_queries` advertises it, so the two
  agree.
- Collection search, the landing and catalog HTML, conditional GET and HTML
  error pages come from `api-common`. `items` reuses the Features API's
  parsing, GeoJSON encoding and paging links from `api-features`.

## Conformance classes

`/edr/conformance` declares each class as EDR 1.2
(`https://www.opengis.net/spec/ogcapi-edr-1/1.2/conf/<class>`) and, except
`instances`, also as EDR 1.1
(`http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/<class>`).

| Class | What backs it |
|---|---|
| `core` | landing page, conformance, `/api`, the data queries |
| `collections` | `/collections` and `/collections/{id}` with `data_queries` |
| `queries` | the query types below; each collection's `data_queries` lists its own |
| `instances` (1.2 only) | forecast model runs, see [Instances](#instances) |
| `json` | JSON metadata documents |
| `geojson` | `/locations`, `items`, and point queries on station collections, as `application/geo+json` |
| `edr-geojson` | those bodies carry the `edrProperties` members (`datetime`, `parameter-name`, `label`, `edrqueryendpoint`) on every feature |
| `covjson` | every coverage, sent as `application/vnd.cov+json` and valid against `schemas/coveragejson.json` |
| `html` | every metadata resource and every data query negotiates HTML |
| `oas30` | `/edr/api`, sent as `application/vnd.oai.openapi+json;version=3.0`; Swagger UI at `/edr/api/docs` |

Also declared: OGC API - Common Part 1 (`core`, `landing-page`, `oas30`) and
Part 2 (`collections`, `json`, `html`). `/edr/conformance` also lists Common
Part 1 `core` and Part 2 `collections` with `https://` URIs, the form EDR
1.2 names. Common Part 4 `searchable-collections` is not declared; see the
[Common Parts 1–4 matrix](../../docs/ogc-api-common-matrix.md).

## Endpoints

| Route | Answers |
|---|---|
| `/`, `/conformance` | landing page, conformance |
| `/api`, `/api/docs` | OpenAPI 3.0 document, Swagger UI (assets embedded, no CDN) |
| `/collections`, `/collections/{id}` | catalog with Common search and paging ([api-common](../api-common/README.md)), collection document |
| `/collections/{id}/locations` | GeoJSON list of named locations |
| `/collections/{id}/locations/{locId}` | data at one location, or at a comma-separated list |
| `/collections/{id}/{position,area,radius,trajectory,cube}` | data queries |
| `/collections/{id}/items`, `/items/{itemId}` | the collection's features |
| `/collections/{id}/instances`, `/instances/{instanceId}` | model runs |
| `/collections/{id}/instances/{instanceId}/{position,area,radius,cube}` | data queries against one run |

A query type the collection's engine does not support has no resource: its
route answers 404 and `/api` omits the path.

## Query types

| Query | Input | Result |
|---|---|---|
| `locations` | the location id in the path | the location's data; see [Location lists](#location-lists) |
| `position` | WKT `POINT` or `MULTIPOINT` (at most 64 points, 16 KiB of coordinates, 256 points × `datetime` instants) | a series or profile per point; a MULTIPOINT is flattened into one CoverageCollection |
| `area` | WKT `POLYGON` (holes allowed) or `west,south,east,north` | a grid over the polygon's bbox with cells outside it null, or the stations inside it |
| `radius` | `POINT` + `within` + `within-units` (`km`, `m`, `mi`; at most 1000 km) | the area query over a 64-vertex geodesic circle; a circle containing a pole or crossing the antimeridian is a 400 |
| `trajectory` | WKT `LINESTRING`, `LINESTRING Z`, `M` or `ZM` | values along the path, or a radar cross-section; see [Trajectory](#trajectory) |
| `cube` | `bbox` (required) and optional `z` | a grid with `t`, `z`, `y`, `x` axes (no `z` on a station grid); see [Cube](#cube) |
| `items` | `bbox`, `datetime`, `limit`, `offset` | GeoJSON features; see [Items](#items) |
| `instances` | none | model runs; see [Instances](#instances) |

All coordinates are CRS84 (longitude, latitude), finite and in range.
Coverage domain types: `Point`, `PointSeries`, `Grid` (optional `t` and `z`
axes), `VerticalProfile`, `Section` and `Trajectory`, alone or in a
`CoverageCollection`.

### Output formats

| Query | Formats, default first |
|---|---|
| `locations`, `position` on a station collection | CoverageJSON, GeoJSON, PNG, CSV, HTML |
| `locations`, `position` elsewhere | CoverageJSON, PNG, HTML |
| `radius` on a station collection | CoverageJSON, GeoJSON, CSV, HTML |
| `area` on a station collection | CoverageJSON, CSV, HTML |
| `trajectory` as a radar cross-section | CoverageJSON, PNG, HTML |
| any other data query | CoverageJSON, HTML |
| `/locations` list, `items` | GeoJSON, HTML |

A station collection is one whose engine answers time series at its listed
locations (`EdrEngine::serves_station_series`: CSV, BUFR, PostGIS stations).

- **Choosing.** `f` names a format case-insensitively, or its media type:
  `application/vnd.cov+json` (EDR 1.1's `application/prs.coverage+json` is
  accepted too), `application/geo+json`, `image/png`, `text/csv`,
  `text/html`. A format the query does not offer is a 400 naming the ones it
  does. Without `f`, `Accept` chooses by q-value, ties going to the order
  above; wildcards and `application/json` keep the default, and nothing
  acceptable falls back to it instead of a 406. A browser therefore gets the
  HTML page. When `Accept` chose among several offered formats, the
  response carries `Vary: Accept`.
- **EDR GeoJSON.** One feature per station: its `Point`, the location id,
  the `edrProperties` members, a `time` array and one array per parameter
  aligned with it. A parameter named like one of those members cannot be
  encoded and is a 400.
- **CSV.** One long-form layout, a row per location, time and parameter:
  `location_id,label,longitude,latitude,datetime,parameter,value,unit`.
  UTF-8, CRLF, RFC 4180 quoting, sent as
  `text/csv; charset=utf-8; header=present`. A missing value is an empty
  field. A text cell a spreadsheet would read as a formula gets a leading
  `'` (OWASP CSV injection guidance). A body over 64 MiB is a 400.
- **PNG.** A time-series or profile plot (`width` × `height`, default
  800 × 600), or for a radar cross-section one heatmap panel per parameter.
  Each panel uses its parameter's own default map style, the one the WMS
  layer `<collection>/<parameter>` and Maps/Tiles `parameter-name` use.
  A parameter without a style of its own uses the collection's default.
  A location list has no PNG.

### Location lists

`/locations/{locId}` takes a comma-separated list of ids; `%2C` is a comma
inside an id. The handler queries each id in turn through the engine's
location query, so every engine that serves `locations` supports lists and
advertises `multiple_locations: true`. A list is one CoverageCollection in
request order (on a station collection also one GeoJSON FeatureCollection or
one CSV table). Limits: 64 ids, 256 ids × `datetime` instants, 1 million
values. An unknown id is a 404 for the whole list; an id without data
contributes nothing, and a list where no id has data is a 204.

### Trajectory

- **Along a path** (GRIB, QueryData, Zarr, NetCDF). `ds_core::trajectory`
  densifies the path along great circles to about one sample per source
  cell (at most 2000 samples), and each engine samples it like its position
  query. `M` is Unix epoch seconds, interpolated along the path; each sample
  takes the selected run's nearest timestep, and a vertex time outside the
  time axis is a 400. `Z` is a level in the collection's vertical coordinate
  and snaps to the nearest advertised level; a level outside the advertised
  range is a 400, and a collection without levels ignores it. `z` with a Z
  or ZM path, and `datetime` with an M or ZM path, are 400s. A 2-D or Z
  path gives one `Trajectory` coverage per timestep; on a collection with
  levels, a 2-D or M path gives one per `z` level. The response is capped
  at 250 000 nodes and 1 million values.
- **Radar cross-section** (ODIM PVOL sites). A 2-D `LINESTRING` becomes a
  vertical `Section` whose domain carries the `meteocore:beamCoverage`
  foreign member (the lowest beam's height per node). `z` selects sweeps by
  elevation angle.
- `MULTILINESTRING` is not supported.

### Cube

GRIB pressure and model-level views offer `cube`, and so do NetCDF
collections and station grids; a collection without levels (a station grid,
a NetCDF surface family) answers a `t`, `y`, `x` grid. Parameters: `bbox`
(four numbers, or six whose vertical pair is a `z` interval that an explicit
`z` overrides), `z`, `datetime`, `parameter-name`, `resolution-x`/`-y`/`-z`,
`crs`, `f` and `limit`; any other or repeated parameter is a 400. Without
`z` every level is returned. `resolution-*` is a count of evenly spaced
positions from edge to edge, each taking the nearest native value (0 or
absent is native; at most 1 000 000). A NetCDF grid is rarely aligned with
CRS84, so its native cells are those of its area query, at the source's
mean cell size but never coarsened, and every cell or position is sampled
bilinearly, as its position query samples a point; its `resolution-z` too
takes the nearest level. `resolution-z` on a collection without levels is
a 400. A `datetime` list joins one cube per instant along `t`. The response
is capped at 1 million values.

### Items

`items` serves the collection's `FeatureEngine`, for EDR collections whose
engine has one (CSV, BUFR, PostGIS stations, nowcast), whether or not the
collection also lists `features`. Filtering is the Features API's: `bbox` in
CRS84 (`west > east` crosses the antimeridian) and `datetime` as an instant
or interval, a 400 on features without time; see the
[Features engine matrix](../api-features/README.md#per-engine-matrix).
`limit` defaults to 10 and is clamped to 10 000; `offset` pages. Only
`bbox`, `datetime`, `limit`, `offset` and `f` are accepted, and
`/items/{itemId}` takes only `f`. Every feature gains the `edrProperties`
members: a station item points at its `/locations/{locId}` query, a nowcast
cell at a radius query over the motion field sized to the cell. As in the
Features API, a feature without a `title` property gets its engine's label
as one, such as a nowcast cell's `Rank 2 · very severe · max 52 dBZ`.
Features' `related` links are left out: `edrqueryendpoint` already names
the item's data.

### Instances

Forecast engines (GRIB, QueryData, Zarr, NetCDF, and nowcast generations)
expose each model run as an instance. The id is the run's reference time in
RFC 3339 UTC, for example `2026-06-07T06:00:00Z`; links carry the colons
unencoded. `{instanceId}` also accepts any RFC 3339 offset of the same
instant and the compact `20260607T0600Z` form. An id that does not parse is
a 400 and an unknown run a 404. A collection without runs answers
`/instances` with an empty list and every `/instances/{instanceId}` path
with a 404.

- The list runs newest first. Up to 500 runs are listed whole; a longer run
  axis pages 100 runs at a time. `limit` (1–500, larger values clamped) and
  `offset` page any list; `offset` without `limit`, or any parameter other
  than these and `f`, is a 400. A page carries `numberMatched`,
  `numberReturned` and `next`/`prev` links.
- An instance document has its collection's title and description and the
  run's own extent. A query under `/instances/{instanceId}/` reads that
  run; the same query without an instance path reads the latest run.
- The run axis is not advertised as an `extent.custom` dimension, and data
  queries take no `reference_time` parameter: select a run by its instance
  path.

## Parameters

| Parameter | Notes |
|---|---|
| `coords` | WKT for the query type, CRS84 |
| `datetime` | RFC 3339 instant, `start/end`, `../end`, `start/..`; a list `T1,T2,…` of up to 16 instants; a repeating interval `Rn/start/duration` (`n` from 1 to 16 instants; the duration in weeks, days, hours, minutes or whole seconds, never years or months). Each listed instant is queried as if it were requested alone, and the answers merge along `t` (at most 1 million values together); an instant without data contributes nothing, and none with data is a 404. An interval that ends before it starts is a 400 |
| `parameter-name` | comma-separated, case-insensitive; an unknown name is a 400 listing the valid ones. Absent or blank means every parameter |
| `z` | a level, a list, `min/max`, `../max`, `min/..` or `Rn/min/step`; at most 1000 levels. Engines keep the levels they have, and none kept is a 400. Absent means every level. A collection without a vertical extent ignores a well-formed `z`; a malformed one is always a 400. On ODIM PVOL sites `z` is an elevation angle matched within 0.05° |
| `within`, `within-units` | radius only: `km`, `m` or `mi`, at most 1000 km |
| `bbox` | `cube`, `/locations` and `items`: four or six numbers in CRS84; `/locations` and `items` ignore the heights |
| `resolution-x`/`-y`/`-z` | cube only |
| `crs` | every data query: CRS84 only (`CRS84`, `OGC:CRS84` or the OGC URI); anything else is a 400 |
| `f` | see [Output formats](#output-formats); metadata resources take `json` or `html` |
| `limit` | 1 to 10 000, larger values clamped, anything else a 400. On a data query it keeps the first top-level coverages (absent keeps all); PNG and HTML ignore it. On `/locations` it pages the list; absent returns the complete inventory |
| `offset` | pages `/locations` and `/instances` (only with `limit`) and `items` |
| `width`, `height` | PNG size, clamped to 160–2000 × 120–2000 px |

`/locations` takes only `limit`, `offset`, `bbox`, `datetime` and `f`. Its
`datetime` keeps the locations with an observation in the window, which
only CSV, BUFR and ODIM PVOL sites can answer; other engines answer a 400.

## Responses and limits

- **Errors.** JSON errors are `{code, description}`. A request that asked for
  HTML (`f=html`, or `Accept` ranking `text/html` first) gets the error as a
  page with the same status. 500s carry no details. `/api` lists every
  status each operation answers: 304, 400, 404 and 500, plus 503 and 504 on
  data queries, `/locations` and `items`, and 204 on `/locations/{locId}`.
- **Caching.** Every 200 carries `Cache-Control` and a strong ETag, and
  `If-None-Match` gets a 304. Metadata and open-ended queries get
  `public, max-age=60`; a data query whose closed `datetime` window ended at
  least an hour ago gets `public, max-age=86400`.
- **Value budgets.** Most answers are capped at 1 million values
  (timesteps × levels × cells × parameters), BUFR and PostGIS at 500 000
  values per response. On most gridded engines, masking a polygon that is
  not a rectangle is also capped at 20 million cell × vertex tests. Over a
  cap is a 400 that says how to narrow the query.
- **Response memory.** Every CoverageJSON, GeoJSON, CSV and HTML body and
  every `/locations` list is admitted by one process-wide budget,
  `MC_EDR_RESPONSE_MEMORY_MB` (default 512 MiB). A body is charged for
  each buffer before it is allocated: CoverageJSON is serialised straight
  into it, CSV is counted first, and an HTML page reserves its estimated
  size before it renders. It stays charged until the client has read it
  or gone away. A body that does not fit is a 503 `ServerBusy` with
  `Retry-After: 1`. PNG plots are not counted. `/locations` lists are also
  capped at `MC_EDR_LOCATIONS_MAX_BYTES` (default 16 MiB) each: a list over
  it is a 503 `ResponseLimit` suggesting `limit`. A body is never
  truncated. `MC_EDR_LOCATIONS_MEMORY_MB` is retired: it is ignored, with a
  warning at startup.

## Parameter metadata

`parameter_names` in a collection and `parameters` in CoverageJSON share one
set of builders and follow Requirement 7 of the Metocean EDR profile:

- `label` in `parameter_names` is at most 50 characters, cut with an
  ellipsis; CoverageJSON parameters leave `label` out and carry the full
  label as `observedProperty.label`. `description` is the full label plus
  its unit.
- The unit is the unit of the values served. GRIB, BUFR, Zarr and NetCDF
  convert values and unit from the source's: K → °C, Pa → hPa, kg m⁻² → mm,
  kg m⁻² s⁻¹ → mm/h, a proportion → %. Geopotential stays in m² s⁻²: its
  height is another quantity, a derived parameter. Zarr and NetCDF also
  serve CF's `1` as % when the `standard_name` is a fraction
  (`relative_humidity`, `…_area_fraction`). Other units are served as the
  source spells them. GRIB, Zarr and NetCDF maps draw the same converted
  values. QueryData takes each unit from the FMI parameter number, and gives
  none where the number has no known unit; its `parameter-name` also accepts
  a descriptor's short name, such as `2t` for `2 Metre Temperature (2t)`,
  keyed as asked. A station grid takes its source's units.
- `unit.symbol` is the QUDT unit when `ds_core::units` knows one, else
  UCUM-typed for a UCUM unit, else a plain string (`dBZ`, `gpm`).
- `observedProperty.id` is the NERC CF standard-name URI when the engine
  knows the name (Zarr and NetCDF from `standard_name`, BUFR's built-in
  parameters, derived wind and derived geopotential height), otherwise the
  engine's observed-property name.
- A parameter with its own time axis (satellite products, GRIB windows that
  some steps lack) carries its own `extent.temporal`.

Not covered: CF standard names on the other engines. Labels taken from
config or source files are served as given, tagged `en`, and are not
checked to be English.

### Derived wind

GRIB, QueryData, Zarr and NetCDF collections that publish wind only as u/v
components also serve wind speed and, where the source states the component
frame and the grid allows it, direction (`ds_core::wind`). The derived
parameters work like native ones in every query and count against the same
value budget. `derive_wind = false` turns this off for a collection.

### Derived geopotential height

GRIB, Zarr and NetCDF collections that publish geopotential (GRIB 0/3/4, CF
`geopotential`, in m² s⁻²) but no geopotential height at that level also
serve the height, geopotential ÷ g₀ (9.80665 m s⁻²) in gpm, named in the
source's vocabulary: ECMWF `z` → `gh`, wgrib2 `GP` → `HGT`, MEPS
`geopotential_pl` → `geopotential_height_pl`. A single-level collection's
surface geopotential gives the surface's height. It is computed from the
geopotential sampled at each point, resolves time, run and level as the
geopotential does, carries the CF `geopotential_height` observed property,
and counts against the same value budget. Collections that ship the height
(`gh`, `HGT`) get no second one. `derive_geopotential_height = false` turns
this off for a collection.

### Per-step amounts

A NetCDF variable named `<name>_acc` in kg m⁻² or mm, accumulated since the
run start (MEPS `precipitation_amount_acc`), also gives `<name>`, the
amount of each step (`precipitation_amount`, in mm, labelled with the step:
`Precipitation amount (1 h accumulation)`). Its value at a step is the
accumulation there less the accumulation at the run's step before, clamped
at zero; a run's first step is its own accumulation. It works like a native
parameter in every NetCDF query, trajectory and cube included, in the run
and steps the accumulation's would use, and its `observedProperty` is the
accumulation's standard name. A trajectory sample takes its own step's step
before, sampled at the sample: on an `M` path, the step before each
sample's step in the run the path reads. A query reads each lead once, the
steps before included, and the `MC_NETCDF_EDR_QUERY_MB` cap counts them.
`[netcdf] derive_per_step = false` turns this off.

## Per-engine query-type matrix

✓ supported · – not supported · n/a no model runs.

| Engine | locations | position | area | radius | cube | trajectory | instances | items |
|---|---|---|---|---|---|---|---|---|
| CSV | ✓ | – | ✓ | ✓ | – | – | n/a | ✓ |
| GeoTIFF | – | ✓ | ✓ | ✓ | – | – | n/a | – |
| GRIB | – | ✓ | ✓ | ✓ | ✓ level views | ✓ path | ✓ | – |
| QueryData | – | ✓ | ✓ | ✓ | – | ✓ path | ✓ | – |
| Zarr | – | ✓ | ✓ | ✓ | – | ✓ path | ✓ | – |
| NetCDF | – | ✓ | ✓ | ✓ | ✓ | ✓ path | ✓ | – |
| ODIM composite | – | ✓ | ✓ | ✓ | – | – | n/a | – |
| ODIM PVOL site | ✓ | ✓ | ✓ | ✓ | – | ✓ section | n/a | – |
| PostGIS stations | ✓ | ✓ | ✓ | ✓ | – | – | n/a | ✓ |
| PostGIS events | – | – | ✓ | ✓ | – | – | n/a | – |
| BUFR | ✓ | ✓ | ✓ | ✓ | – | – | n/a | ✓ |
| Nowcast | – | – | ✓ | ✓ | – | – | ✓ | ✓ |
| Satellite | – | ✓ | ✓ | ✓ | – | – | n/a | – |
| Station grid | – | ✓ | ✓ | ✓ | ✓ no `z` | – | n/a | – |

CAP and GeoJSON collections have no `EdrEngine`. Radius and location lists
need no engine code of their own; items serve the engine's own
`FeatureEngine`.

| Engine | Area and engine notes |
|---|---|
| CSV | stations inside the polygon with a row in the window; more than 500 stations is a 400 |
| GeoTIFF | grid at native resolution; on a STAC source, a window of more than 400 items whose metadata is not loaded yet is a 400 |
| GRIB | native grid nodes over the bbox; `datetime` matches steps exactly; without `datetime`, the last step of the latest run that has every named parameter. Cube on the `<id>-pressure` and `<id>-model` views that `[grib] level_types` creates |
| QueryData, Zarr, ODIM composite, Satellite | grid over the bbox, at most 256 cells per axis, cells outside the polygon null. Zarr reads at most 8 variables per area query. Satellite: each product has its own time axis, an instant matches only a scan at exactly that time, RGB composites are not EDR parameters, and a query fetches at most 8 evicted scans and decodes at most 133 million pixels of image blocks (1024 GOES-R 2 km strips or 70 GK2A chunks) |
| NetCDF | CRS84 grid at the source's mean cell size, at most 256 cells per axis, over lat/lon or CF Lambert conformal conic, geostationary, polar and oblique stereographic, Lambert azimuthal equal-area and transverse Mercator grids, each on the earth figure it states, projected x/y in metres or kilometres, and curvilinear grids of 2-D latitude/longitude, each point placed by an inverse lookup over the nodes (a polar grid's extent reaches its pole, one across the antimeridian is `west > east`); pressure levels through `z`. Trajectory and cube sample each point like position; an M path reads the run that covers its earliest time; a cube has a `t` axis and, on pressure levels, a `z` axis, and its native cells are the area's without the 256 cap. A query decodes at most `MC_NETCDF_EDR_QUERY_MB` (default 544 MiB), counting only the fields it reads: a trajectory's planned (step, level) fields, a cube's steps × the levels its positions take. Through OPeNDAP a query reads only the window of each field its points or cells sample, or the whole field when the window passes half of it; the values are the whole field's |
| ODIM PVOL site | polar sampling of each volume; `/locations` lists the site; trajectory is a vertical cross-section. The volume files a query reads and the moments it decodes are admitted by `MC_PVOL_READ_MEMORY_MB` (default 1024 MiB); one that does not fit is a 503 `ServerBusy` |
| PostGIS stations | stations inside the polygon, one series each; `/locations` holds at most `max_locations` stations (default 200 000); a value outside its parameter's `valid_range` or equal to one of its `missing_values` is a null, its time kept; see [engine-postgis](../engine-postgis/README.md) |
| PostGIS events | events inside the polygon as `Point` coverages, with the same `valid_range` and `missing_values` nulls |
| BUFR | stations inside the polygon over the in-memory retention window; position is the nearest station within `position_radius_km` (default 25 km) |
| Nowcast | the motion field (`motion_u`, `motion_v` in m/s, `motion_quality`) over the polygon's bbox; generations are instances; items are tracked cells |
| Station grid | the analysis grid the map draws (each station's latest valid value within `[station_grid] max_age`, interpolated onto a CRS84 grid), read at the cell under each point; without `datetime` the latest analysis; an instant must be an analysis time; at most 96 analyses (times × parameters) per query; an analysis still computing at the deadline is a 503, and its computation goes on for a retry. With a terrain, lapse-rate corrected values; with a land/sea mask, null outside it; no analysis time (404) until both are sampled |

Each engine crate documents its own query details.

## HTML

- Metadata pages use the shared
  [HTML API workbench](../api-common/README.md#html-api-workbench).
- Every data query answers `f=html`, or a browser's `Accept`, with a page of
  its response: a summary, the PNG plot where the query offers one, every
  value in tables, the request as an editable form, and the full metadata
  folded below. Nothing is truncated; the value caps bound the page.
- An HTML request to position, area, radius, trajectory or cube without
  coordinates (`coords`, or a cube's `bbox`) gets the query's form,
  prefilled with an example inside the collection's extent, instead of a
  400. JSON clients still get the 400.
- The `/locations` page shows 100 locations at a time with a map; `items`
  pages show a feature table.
- A collection page's request guide, here and on the shared root's page,
  gives ready EDR requests built from the queries the collection offers:
  its locations and the whole extent for stations, the values at the
  extent's centre and a small area for a grid, the whole extent for one
  answering areas alone (a nowcast, lightning events). The shared root reads
  the queries from the EDR registry, once per load.

## Configuration

This crate has no config section of its own. A collection is served when its
`apis` contains `"edr"` (the default when `apis` is omitted) and its engine
type supports EDR: `csv`, `geotiff`, `grib`, `querydata`, `zarr`, `netcdf`,
`odim`, `odim-volume`, `postgis`, `bufr`, `nowcast`, `satellite` or
`station-grid`. The runnable `collections.d/weather.toml` is a CSV station
collection (`engine_type` defaults to `csv`; `id`, `title` and
`description` are required):

```toml
id = "weather"
title = "Finnish Weather Observations"
description = "Hourly weather observations from Finnish weather stations"
data_path = "testdata/weather.csv"
apis = ["edr", "features"]
```

`collections.d/weather-analysis.toml` grids that collection's temperature
and wind speed as a station grid.

Environment variables: `MC_EDR_RESPONSE_MEMORY_MB` (default 512) and
`MC_EDR_LOCATIONS_MAX_BYTES` (bytes, default 16 MiB); see
[Responses and limits](#responses-and-limits).

## Limits

- Data queries are served in CRS84 only.
- No `corridor` query, no `MULTILINESTRING` trajectory, no
  `resolution-x`/`-y` on area.
- No `locations`, `trajectory` or `items` under `/instances/{instanceId}/`.
- GeoJSON and CSV only on station collections; no GeoJSON for area; no
  NetCDF output.
- Along-path trajectory only on GRIB, QueryData, Zarr and NetCDF; cube only
  on GRIB level views, NetCDF and station grids.
- `parameter_names` entries carry no `dataType`, and every CoverageJSON
  range is encoded as `float`.
- `position`, `area`, `radius`, `trajectory` and `/locations/{locId}` ignore
  query parameters they do not define. `cube`, `items`, `/locations` and
  `/instances` reject them with a 400.

## Testing

```bash
cargo test -p api-edr
```

The tests drive the router with mock engines. They validate the
CoverageJSON they produce against `schemas/coveragejson.json`, metadata and
`/locations` against both vendored EDR OpenAPI bundles, 1.1 and 1.2
(`tests/support/edr_schema.rs`; see [schemas](../../schemas/README.md)), and
`/api` against the OpenAPI 3.0 meta-schema, checking that every status a
route answers is documented. End-to-end tests with real engines and
fixtures are in `crates/server/tests/` (`edr_geojson.rs`,
`edr_locations_filter.rs`, `grib_cube_edr.rs`, `netcdf_edr.rs`,
`satellite_edr.rs`).

Contributor rules are in [CLAUDE.md](CLAUDE.md).
