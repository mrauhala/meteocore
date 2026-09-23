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

Also declared: OGC API - Common Part 1 (core, landing-page, oas30) and
Part 2 (collections, json, html). Collection discovery supports `bbox`,
`bbox-crs` (CRS84 only), `datetime`, `q`, `query`, `limit`, `offset` and `f` through
[api-common](../api-common/README.md). Unknown/unsupported or duplicate controls
return structured HTTP 400 errors. Filters run before paging; JSON/HTML links
preserve filters and the negotiated format. Collection descriptions expose HTML
alternate links and configured keywords/license metadata through the shared helper.

Text discovery supports whitespace-normalized whole-word phrases in `q`, and
`query` adds required (`+`) and excluded (`-`) terms within comma-separated OR
alternatives. Encode `+` as `%2B`; `q` and `query` are ANDed when both supplied.
See [the shared text-search contract](../api-common/README.md)
for examples and validation choices.

Part 4 `searchable-collections` is **not declared**: the 2026-09-17 draft also
requires `sd` and `resolution`, which are not implemented. Existing
basic search remains supported. See the [Common Parts 1–4 matrix](../../docs/ogc-api-common-matrix.md)
for the specification baselines and remaining gaps.

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
its parameter metadata. Encoded location buffers are bounded by
`MC_EDR_LOCATIONS_MAX_BYTES` (default 16 MiB per complete inventory) and `MC_EDR_LOCATIONS_MEMORY_MB` (default 128
MiB process-wide). Memory reservations cover buffer growth, including the old
and new allocations during copying, and remain with response bytes through
middleware and client delivery. Exhaustion returns 503 without partial JSON or
truncation; cancellation/deadlines stop serialization. Engine-owned inventory
snapshots and the `get_locations()` result are separate from this encoded-buffer
budget; retrieval remains under the bounded query executor. EDR 1.1's complete
inventory contract remains unchanged: there is no implicit pagination.

Every 200 carries `Cache-Control` + a strong ETag; `If-None-Match` → 304 (#499).

`/api/docs` uses embedded Swagger UI 5.33.0 assets, served under
`/api/docs/{asset}`, with a same-origin script policy and `nosniff` headers.
No executable documentation assets or validation requests use a CDN (#587).

## Domain types produced

`PointSeries`, `Point` (events), `Grid` (with optional `t` and `z` axes),
`VerticalProfile`, `Section` (trajectory cross-sections, with the
`meteocore:beamCoverage` foreign member). Everything validates against the
CoverageJSON 1.0 schema.

Zarr/Icechunk storage reads honor the query deadline. Branch-backed collections
refresh on their poll interval; each query uses one pinned snapshot, and a failed
refresh retains the previous catalog. Explicit snapshot IDs remain pinned.
Plain Zarr rebuilds metadata and coordinates using fresh cache generations on
each poll, sharing one object-cache byte budget across generations. Failed rebuilds
keep the published catalog. A replacement becomes visible before the previous
generation is retired. Retired queries can use retained objects and
sampled windows; uncached reads or downloads crossing retirement return HTTP 503
so clients can retry against the current catalog. Concurrent reads of the same
plain object within one generation share its download while preserving each
waiter's deadline. Zero object-cache capacity disables retention, but existing
waiters still share a completed download, as they do for oversized objects.
Plain generations do not provide transactional consistency
against external in-place writes; stable publication or Icechunk is needed for
atomic updates across objects.
Icechunk position, area, and radius queries share native decoded inner chunks
with map reads within the same snapshot. `zarr.icechunk.decoded_cache_mb` bounds
resident decoded bytes separately from the compressed payload cache (default
256 MiB, `0` disables). Concurrent requests coalesce chunk fills while retaining
their own wait deadlines; unsupported or oversized chunks use ordinary reads.
Zarr and Icechunk position/area/radius reads also share process-wide source
memory admission (`MC_ZARR_READ_MEMORY_MB`, default 1024 MiB). It reserves the
native subset and conversion/window buffers before payload I/O. Cold chunks
also reserve a decode-workspace estimate; decoded hits reserve only the
capacity of the buffer held through copying, including if evicted meanwhile.
Coalesced decoded readers hold only their source buffers while waiting, then
admit the returned buffer capacity. A reader taking over a failed fill must
pass cold admission before decoding. Cache waits retain individual deadlines
and run outside the shared decode pool, after completing any queued batch.
A read that cannot fit, including concurrent contention,
returns 503 without waiting while holding other windows. Area windows retain
their reservation through sampling. This budget is separate from response
limits, resident caches, and persistent metadata. Encoded full-object/range
reads add a two-copy allowance before collection and retain it through native
retrieval, including compressed-cache hits and coalesced waiters. Native decoded
cache hits do not need encoded admission. Numeric-variable gzip/zstd/Blosc outputs
are capped by the declared codec representation, including sharded layouts;
if bounded-codec setup fails for one variable, catalog discovery warns and
omits that variable while retaining usable ones. Catalog construction still
fails if no usable variables remain. During reads,
bounded intermediate outputs acquire additional capacity allowances through
retrieval. Blosc also validates frame/block sizes before full and partial
decoding and admits its size-dependent native scratch buffers. Invalid frame
lengths remain engine errors, while admission failures and expired deadlines
preserve their typed errors. Other codecs, compressor-private contexts,
and transport/internal storage buffers remain outside the estimate;
it is not an RSS limit.
Icechunk reads process up to four inner chunks concurrently through a shared
four-worker pool, even with decoded retention disabled. Admission reserves
each active cold workspace or cached buffer and reduces concurrency when
memory is tight. These chunk reservations end after copying into the source
window; the window's reservation remains through sampling.
For common gzip/zstd/Blosc/CRC32C layouts, cold admission includes encoded,
intermediate, and scratch headroom estimated from metadata. Retrieval consumes
that allowance before reserving extra capacity. Unknown layouts and estimates
too large for a single chunk use serial actual-size admission; conservative
bounds can reduce concurrency, but do not become new codec-output limits.
Worker deadlines and reservations remain attached until all chunk jobs finish,
including error and cancellation paths. Plain Zarr and shards with outer
transforms retain serial retrieval.

## Per-engine query-type matrix

✓ implemented · – not implemented · n/a not applicable (no model runs).

| Engine | locations | position | area | radius | trajectory | instances | Area semantics |
|---|---|---|---|---|---|---|---|
| CSV | ✓ | – | ✓ | ✓ | – | n/a | stations whose point is inside the polygon (≤ 500) |
| GeoTIFF | – | ✓ | ✓ | ✓ | – | n/a | polygon-tested |
| GRIB | – | ✓ | ✓ | ✓ | – | ✓ | Grid over the polygon's bbox at native resolution (≤ 1M values across levels/parameters), cells outside the polygon masked; antimeridian-crossing bboxes rejected (#667) |
| QueryData | – | ✓ | ✓ | ✓ | – | ✓ | Grid over bbox at native resolution, ≤ 256 cells/axis, cells outside the polygon masked (vertex fallback for sub-cell shapes); polygon outside the extent → 404; `t` axis when several steps |
| Zarr | – | ✓ | ✓ | ✓ | – | ✓ | Grid over bbox at native resolution, ≤ 256 cells/axis, one subset retrieval per variable for the whole time span (each may read multiple chunks) (two across the antimeridian; cells within half a native cell of ±180° are not interpolated across the seam, #667), at most 8 variables per request, cells outside the polygon masked (vertex fallback for sub-cell shapes); polygon outside the extent → 404; `t` axis when several steps. Forecast stores (reference + lead axes) expose every run as an instance; `None` ⇒ latest |
| ODIM composite | – | ✓ | ✓ | ✓ | – | n/a | Grid over bbox, ≤ 256 cells/axis, masked to the polygon; `t` axis when several steps |
| ODIM PVOL site | ✓ | ✓ | ✓ | ✓ | ✓ | n/a | polar sampling; trajectory = RHI cross-section |
| PostGIS stations | ✓ | ✓ | ✓ | ✓ | – | n/a | stations-only `location_source`: exact `ST_Within` in SQL; observations-derived: exact point-in-polygon on the cached station set |
| PostGIS events | – | – | ✓ | ✓ | – | n/a | events in the polygon (exact, in SQL) as a `Point` CoverageCollection |
| BUFR | ✓ | ✓ | ✓ | ✓ | – | n/a | stations whose point is inside the polygon (exact, in memory; ≤ 10 001 stations, ≤ 500 000 values per response → 400); position = nearest station within `position_radius_km` (25 km) else 404; one `PointSeries` per station over the in-memory `retention` window; same semantics for the polled-directory and WIS2 (push) sources; units are the BUFR units mechanically converted for display (K → °C, Pa → hPa, kg m-2 → mm) like GRIB |
| Nowcast | – | – | ✓ (motion field) | ✓ | – | ✓ | motion blocks over the polygon's bbox, blocks outside the polygon masked; reflectivity via EDR = #523 |
| CAP, GeoJSON | — no `EdrEngine` (Features/Maps only) — | | | | | | |

Radius, cube, corridor and items have no engine-specific code: radius is
answered by every engine that answers area, the other three do not exist.

Compatible nowcast reloads retain motion-field instances alongside forecast
runs and cell history (#604). Reuse requires unchanged nowcast config, the
same raster source engine and a compatible retained geometry/product contract;
source/tuning changes still rebuild. Auxiliary source edits affect subsequent
generations, not already-published instances.

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

GRIB position queries share each fetched/decoded field across every requested
coordinate, including `MULTIPOINT` profiles. Up to four field reads/decodes run
concurrently per admitted query, preserving point/time/level order. The complete
batch's one-million-value budget is checked before fetching. Storage reads and
workers share the existing 30-second EDR deadline; expiry stops new reads and
returns 504. Other engines retain their sequential position behavior through the
default batch hook, with combined output limits checked after each point.

GRIB sources can set `message_cache_mb` to retain compressed fields separately
from decoded grids (`0` by default). Repeated queries at another coordinate can
then reuse message bytes after decoded grids are evicted. This saves storage
reads while retaining the normal decode and unit-conversion path. The byte
budget is additional to `grid_cache_mb` and shared by all level collections and
APIs of that source; header probes do not fill it. Failed reads/decodes are not
retained, and messages larger than the budget are served without retention.

GRIB area/radius queries use the same four-worker limit across parameter/level
fields. One initial field supplies both geometry and values, including with the
cache disabled. The 1M-value and polygon-mask budgets are checked before
allocating the output values or fetching further fields. Results preserve
requested level order and polygon holes; incompatible grids remain errors.
On error or deadline expiry, dispatch stops and active workers drain before
query admission is released.

GRIB wgrib2 accumulation and average fields use duration-qualified parameter
names, for example `APCP_acc_6h`, `APCP_acc_3h` and `DSWRF_avg_6h`. The time axis
is the **window end**, with the duration in the parameter label/name; the start
is that valid time minus the duration. A source parameter filter such as
`parameters = ["APCP"]` includes its available windows; clients query the
advertised qualified keys. Missing windows at a step are null. Values use the
source WMO unit and existing display conversion (precipitation kg/m² → mm);
there is no implicit division by duration or conversion of energy into flux.
ECMWF JSON naming remains unchanged.

GRIB sources can opt into `level_types = ["single", "pressure", "model"]`.
The server publishes only present/enabled families as `{id}-single`,
`{id}-pressure` and `{id}-model`. Single-level fields have no vertical extent;
pressure uses hPa and model/hybrid uses dimensionless ordinal level numbers.
The views share discovery, polling and the grid cache. Late-arriving families
are registered automatically from the accepted config.

Pressure/model position queries return a `PointSeries` when one level is
selected, or a `CoverageCollection` of `VerticalProfile` coverages (one per
step) for multiple levels. `z` omitted selects all levels; single/list/interval
selectors are supported. Area/radius queries return a `[z,y,x]` Grid at one
forecast step; the 1M-value budget includes every selected level and parameter.
A missing field at an available level is null; an unavailable level is 400.
Levels are exact discrete coordinates, not interpolated. Model levels are not
converted to geometric heights. Soil-depth/isentropic axes and fractional index
level values remain unsupported by this split.

Pressure/model parameter labels and units can use a probed or decoded level of the same
parameter and level type while another level is unprobed; exact-level metadata
takes precedence. These lookups use shared indexes rather than scanning every
cached level, and do not fetch additional data.

GRIB background metadata probes read message headers without unpacking values or
filling the decoded-grid cache. Each scan probes at most 32 missing parameter
identities or missing representative geometries, eight concurrently, usually
with one 4 KiB range per message. The regular latitude/longitude grid header is
read alongside parameter metadata. Each collection uses a representative field
from its latest run for spatial bounds; families can use different grids.
Unknown/unsupported geometry omits the spatial extent until it is known, rather
than advertising global coverage. Geometry probes refresh for new runs even
when their parameter units are already cached. A collection is expected to use
a common grid within each level family; this is not a union of mixed grids.
Map metadata is published as a shared snapshot, with grid cell counts between
nodes (including the closing longitude cell on cyclic grids) so advertised
resolution matches native spacing. Catalog time unions are computed at publication.
Failed probes remain retryable; labels and units can also populate on a query.

GRIB caches preserve the decoder's `f32` values without widening whole grids;
sampling, coordinates, unit conversion and response values remain `f64`.

Without `level_types`, the existing collection ID and canonical-level behavior
are preserved. Single-level and legacy parameter names select a canonical level
per run, shared by metadata, position, area and Maps. Missing canonical fields
are null in position and errors in area; `z` is rejected. A temperature
difference such as dewpoint depression stays in K. Area longitude axes remain
continuous between grid nodes, and global interpolation wraps the grid seam.
Wgrib2 ground, MSL, whole-atmosphere and other named surfaces/layers retain
distinct identities within the single-level view. Ground/MSL/whole-atmosphere
products take precedence over named upper-air fields, independent of index
order; a missing ground field cannot be replaced by a tropopause field.
Soil layers retain both depth boundaries internally but remain excluded from
the three level families.

For datetime selection, a GRIB run must contain the requested start instant
within its published valid-time extent. An incomplete newer run does not hide
a covering older run. Area selects the nearest step within the chosen run;
position returns the steps in the requested interval. Explicit instance pins
never fall back to another run.

If a wgrib2 index repeats the same parameter/level/window at different offsets,
the scan emits one summary warning after parameter/family filtering, with
per-record details at DEBUG, and queries select the first record.
Duration-qualified names separate different windows; they cannot recover product distinctions omitted
from the source sidecar, or prove that repeated records contain identical data.

An area query without `parameter-name` prefers the existing near-surface
instant/max/min products before newly supported acc/ave records. If only
aggregates are configured, the first available aggregate is the default.

BUFR decoding supports compressed character fields, operator 208, and numeric
fields through 64 bits. Unsupported operators or unknown national descriptors
skip the affected message (counted in `bufr_decode_failures_total`); other
messages in the same file remain available. See
[`engine-bufr` decoder notes](../engine-bufr/CLAUDE.md#the-decoder-boundary).

### HTML workbench

Landing, conformance, collections, collection and model-run metadata use the
shared `api-common::workbench` shell with light/dark/system themes and a persistent
JSON switch. The collection builder exposes Common text/spatial/temporal search
and paging. Collection and instance detail pages retain the complete EDR metadata,
including parameter descriptions, extents and available query links. Overview and
metadata tabs separate coverage from the full metadata table; a local geographic
backdrop locates the advertised extent. Data-query
payloads keep their existing representations. See the
[shared HTML behavior](../api-common/README.md#html-api-workbench).

The HTML overview distinguishes finding a collection from requesting its data.
Collection pages list advertised EDR operations and link to the API reference
for required data-query inputs; collection search is not an EDR data builder.

HTML breadcrumbs use collection titles, including parent collection links on
model-run lists and individual run metadata pages. IDs remain unchanged in URLs.

Collection advanced search starts closed and retains the user's open/closed
choice across searches in the browser session. Resource URLs are clickable.

The compact HTML catalog summarizes advertised parameter names and UTC coverage.
It retains time precision and distinguishes an out-of-range page from no matches.
List/cards preference survives searches; JSON continues to reflect applied filters.

HTML collection catalogs and overviews show vertical bounds, available levels,
and units from the canonical vertical reference system. Single-level collections
omit the vertical display. EDR JSON continues to use string coordinates and VRS.
