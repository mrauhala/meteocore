# api-edr — OGC API - Environmental Data Retrieval 1.1 status

This page is the single source of truth for **what MeteoCore's EDR
implementation supports and what it does not**, per query type and per
engine. It is written for integrators and for anyone planning EDR work.

**Keep it current: every PR that touches EDR behaviour — `crates/api-edr`,
`EdrEngine` in `crates/core/src/edr_engine.rs`, or an engine's `EdrEngine`
implementation / `supported_query_types` — must update the tables below in
the same PR.** The `crates/api-edr/CLAUDE.md` rule points here.

Spec: OGC API - EDR 1.1 (OGC 19-086r6). Base route: `/edr`.

## Conformance classes

| Class | Declared | Notes |
|---|---|---|
| `core` | ✓ | |
| `collections` | ✓ | |
| `queries` | ✓ | one class for every query type; each collection's `data_queries` says which it supports |
| `json` | ✓ | |
| `covjson` | ✓ | every CoverageJSON body validates against `schemas/coveragejson.json` (`cargo test -p api-edr`) |
| `html` | ✓ | every metadata resource (landing, conformance, collections, collection, instances, instance) negotiates `?f=html` / `Accept` |
| `oas30` | ✓ | `/edr/api` (hand-written `api_definition()`), Swagger UI at `/edr/api/docs` |
| `geojson` | ✗ | data queries answer 400 for `f=GeoJSON`; only `/locations` is GeoJSON |
| `edr-geojson` | ✗ | same reason (a test pins that it is *not* declared) |

Also declared: OGC API - Common Part 1 (core, landing-page, oas30), Part 2
(collections, json, html) and Part 4 searchable-collections (`/collections?bbox=&datetime=&q=&limit=&offset=`).

## Query types

| Query type | Route | Status | Notes |
|---|---|---|---|
| `locations` | `/collections/{id}/locations`, `/locations/{locId}` | ✓ | GeoJSON list + CoverageJSON/PNG series per location |
| `position` | `/collections/{id}/position` | ✓ | `POINT` or `MULTIPOINT` (fanned out, flattened into one CoverageCollection — per-point grouping not preserved; at most 64 points, 16 KiB decoded coordinates, 1 million values combined; all coordinates finite and within CRS84 bounds) |
| `area` | `/collections/{id}/area` | ✓ | WKT `POLYGON` (holes allowed) or `west,south,east,north`; PNG rejected |
| `radius` | `/collections/{id}/radius` | ✓ | `coords=POINT`, `within`, `within-units=km\|m\|mi`; default trait impl = 64-vertex geodesic polygon → `query_area`; capped at 1000 km; pole/antimeridian circles are 400 (#667) |
| `trajectory` | `/collections/{id}/trajectory` | partial | 2-D `LINESTRING` only, meaning a *vertical cross-section* (PVOL sites). `LINESTRINGZ/M` (per-node z/time) not accepted; no along-path sampling on gridded engines |
| `instances` | `/collections/{id}/instances`, `/instances/{instanceId}` | ✓ | forecast model runs (`ds_core::instances`); instance-scoped queries: position, area, radius only |
| `cube` | — | ✗ | not in the trait or the router |
| `corridor` | — | ✗ | not in the trait or the router (`corridor-width`/`-height` documented as follow-up on trajectory) |
| `items` | — | ✗ | not in the trait or the router; the natural surface for CAP/GeoJSON/PostGIS-events, overlaps Features |

### Instance-scoped routes

| Route | Status |
|---|---|
| `/instances/{instanceId}/position` | ✓ |
| `/instances/{instanceId}/area` | ✓ |
| `/instances/{instanceId}/radius` | ✓ |
| `/instances/{instanceId}/locations`, `/trajectory` | ✗ |

## Parameters

| Parameter | Status | Notes |
|---|---|---|
| `coords` | ✓ | WKT per query type (see above) |
| `datetime` | ✓ | RFC 3339 instant, `start/end`, `../end`, `start/..` |
| `parameter-name` | partial | comma-separated; 400 only when *no* requested name matches — a mix of valid and unknown names silently drops the unknown ones (#666, the #605 rule) |
| `z` | ✓ | single, list, or `min/max` interval, snapped to the collection's advertised levels; 400 on a collection with no vertical extent |
| `f` | partial | `CoverageJSON` (default) and `PNG` (position/locations/trajectory plots) only; media-type aliases such as `f=application/json` rejected (#510); no CSV/NetCDF/GeoJSON |
| `crs` | ✗ | data queries accept CRS84 only; `crs_details` not advertised; `bbox-crs` on `/collections` is CRS84 only |
| `within`, `within-units` | ✓ | radius only |
| `resolution-x`/`-y`/`-z` | ✗ | (cube / area resolution hints) not accepted |
| `limit` | partial | `/collections` pagination only; `/locations` returns the full inventory (EDR 1.1 does not define locations paging) |

Data queries execute on a dedicated, bounded runtime, including radius and
instance routes. Admission is capped at 2–8 concurrent queries (available CPUs,
clamped), with room for 32 additional admitted requests waiting for a slot;
further requests receive 503 immediately. A 30-second deadline (including queue time) returns
504. Synchronous work already in progress retains its slot until it finishes;
a cancelled/timed-out MULTIPOINT stops before its next engine call. This bounds
concurrency without claiming that synchronous engine I/O is preemptible.

For `/locations`, the same permit covers retrieval, metadata, direct JSON
serialization and ETag hashing (#533). The response still contains the full
EDR 1.1 inventory, but no intermediate JSON tree duplicates every location and
its parameter metadata. The final response bytes and the engine's location
vector remain in memory; this is bounded concurrency, not constant total
response memory or pagination.

Every 200 carries `Cache-Control` + a strong ETag; `If-None-Match` → 304 (#499).

`/api/docs` uses embedded Swagger UI 5.33.0 assets, served under
`/api/docs/{asset}`, with a same-origin script policy and `nosniff` headers.
No executable documentation assets or validation requests use a CDN (#587).

## Domain types produced

`PointSeries`, `Point` (events), `Grid` (with optional `t` and `z` axes),
`VerticalProfile`, `Section` (trajectory cross-sections, with the
`meteocore:beamCoverage` foreign member). Everything validates against the
CoverageJSON 1.0 schema.

## Per-engine query-type matrix

✓ implemented · – not implemented · n/a not applicable (no model runs).

| Engine | locations | position | area | radius | trajectory | instances | Area semantics |
|---|---|---|---|---|---|---|---|
| CSV | ✓ | – | ✓ | ✓ | – | n/a | stations whose point is inside the polygon (≤ 500) |
| GeoTIFF | – | ✓ | ✓ | ✓ | – | n/a | polygon-tested |
| GRIB | – | ✓ | ✓ | ✓ | – | ✓ | Grid over the polygon's bbox at native resolution (≤ 1M cells), cells outside the polygon masked; antimeridian-crossing bboxes rejected (#667) |
| QueryData | – | ✓ | ✓ | ✓ | – | ✓ | Grid over bbox at native resolution, ≤ 256 cells/axis, cells outside the polygon masked (vertex fallback for sub-cell shapes); polygon outside the extent → 404; `t` axis when several steps |
| Zarr | – | ✓ | ✓ | ✓ | – | ✓ | Grid over bbox at native resolution, ≤ 256 cells/axis, one store read per variable for the whole time span (two across the antimeridian; cells within half a native cell of ±180° are not interpolated across the seam, #667), at most 8 variables per request, cells outside the polygon masked (vertex fallback for sub-cell shapes); polygon outside the extent → 404; `t` axis when several steps. Forecast stores (reference + lead axes) expose every run as an instance; `None` ⇒ latest |
| ODIM composite | – | ✓ | ✓ | ✓ | – | n/a | Grid over bbox, ≤ 256 cells/axis, masked to the polygon; `t` axis when several steps |
| ODIM PVOL site | ✓ | ✓ | ✓ | ✓ | ✓ | n/a | polar sampling; trajectory = RHI cross-section |
| PostGIS stations | ✓ | ✓ | ✓ | ✓ | – | n/a | stations-only `location_source`: exact `ST_Within` in SQL; observations-derived: exact point-in-polygon on the cached station set |
| PostGIS events | – | – | ✓ | ✓ | – | n/a | events in the polygon (exact, in SQL) as a `Point` CoverageCollection |
| BUFR | ✓ | ✓ | ✓ | ✓ | – | n/a | stations whose point is inside the polygon (exact, in memory; ≤ 10 001 stations, ≤ 500 000 values per response → 400); position = nearest station within `position_radius_km` (25 km) else 404; one `PointSeries` per station over the in-memory `retention` window; same semantics for the polled-directory and WIS2 (push) sources; units are the BUFR units mechanically converted for display (K → °C, Pa → hPa, kg m-2 → mm) like GRIB |
| Nowcast | – | – | ✓ (motion field) | ✓ | – | ✓ | motion blocks over the polygon's bbox, blocks outside the polygon masked; reflectivity via EDR = #523 |
| CAP, GeoJSON | — no `EdrEngine` (Features/Maps only) — | | | | | | |

Radius, cube, corridor and items have no engine-specific code: radius is
answered by every engine that answers area, the other three do not exist.

## Known gaps, in suggested order

1. `locations` and `trajectory` under `/instances/{id}/`.
2. `cube` and `corridor` (derivable from area / trajectory).
3. `crs` on data queries + `crs_details`; EDR GeoJSON output for point results (then declare `edr-geojson`).
4. `items` for the feature engines (CAP, GeoJSON, PostGIS events).

Related issues: #585 MULTIPOINT fan-out bound · #510 `f` aliases · #667
antimeridian bboxes · #668 400-vs-404 on unsupported query types · #666
shared parameter-name validation · #665 GRIB value rounding · #523 nowcast
reflectivity via EDR · #673 shared area budget.

GeoTIFF cold source decodes share a byte budget across APIs; exhausted decode
admission returns HTTP 503 without a partial CoverageJSON result.

GRIB wgrib2 accumulation and average fields use duration-qualified parameter
names, for example `APCP_acc_6h`, `APCP_acc_3h` and `DSWRF_avg_6h`. The time axis
is the **window end**, with the duration in the parameter label/name; the start
is that valid time minus the duration. A source parameter filter such as
`parameters = ["APCP"]` includes its available windows; clients query the
advertised qualified keys. Missing windows at a step are null. Values use the
source WMO unit and existing display conversion (precipitation kg/m² → mm);
there is no implicit division by duration or conversion of energy into flux.
ECMWF JSON naming remains unchanged.

If a wgrib2 index repeats the same parameter/level/window at different offsets,
the scan warns and queries select the first record. Duration-qualified names
separate different windows; they cannot recover product distinctions omitted
from the source sidecar, or prove that repeated records contain identical data.

An area query without `parameter-name` prefers the existing near-surface
instant/max/min products before newly supported acc/ave records. If only
aggregates are configured, the first available aggregate is the default.

BUFR decoding supports compressed character fields, operator 208, and numeric
fields through 64 bits. Unsupported operators or unknown national descriptors
skip the affected message (counted in `bufr_decode_failures_total`); other
messages in the same file remain available. See
[`engine-bufr` decoder notes](../engine-bufr/CLAUDE.md#the-decoder-boundary).
