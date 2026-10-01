# api-edr — OGC API - Environmental Data Retrieval 1.1 status

This page is the single source of truth for **what MeteoCore's EDR
implementation supports and what it does not**, per query type and per
engine. It is written for integrators and for anyone planning EDR work.

**Keep it current: every PR that touches EDR behaviour — `crates/api-edr`,
`EdrEngine` in `crates/core/src/edr_engine.rs`, or an engine's `EdrEngine`
implementation / `supported_query_types` — must update the tables below in
the same PR.** The `crates/api-edr/CLAUDE.md` rule points here.

Spec: OGC API - EDR 1.1 (OGC 19-086r6), plus EDR 1.2's `limit`,
locations paging (#922) and several location ids in one locations query
(#923). Base route: `/edr`.

## Conformance classes

| Class | Declared | Notes |
|---|---|---|
| `core` | ✓ | |
| `collections` | ✓ | |
| `queries` | ✓ | one class for every query type; each collection's `data_queries` says which it supports |
| `json` | ✓ | |
| `covjson` | ✓ | every CoverageJSON body validates against `schemas/coveragejson.json` (`cargo test -p api-edr`) and is sent as `application/vnd.cov+json` (EDR 1.2's `/req/covjson/definition`, #920). The declared URI is still 1.1's, whose requirement names `application/prs.coverage+json`; that type is accepted in `f` but no longer sent. Moving the declaration to 1.2 is #930 |
| `html` | ✓ | every metadata resource (landing, conformance, collections, collection, instances, instance) negotiates `?f=html` / `Accept` |
| `oas30` | ✓ | `/edr/api` (hand-written `api_definition()`), Swagger UI at `/edr/api/docs` |
| `geojson` | ✗ | data queries answer 400 for `f=GeoJSON`; only `/locations` and `items` are GeoJSON |
| `edr-geojson` | ✗ | same reason (a test pins that it is *not* declared) |

Also declared: OGC API - Common Part 1 (core, landing-page, oas30) and
Part 2 (collections, json, html). The landing page links `/conformance` and
`/collections` with both the short `conformance`/`data` relations this standard
requires and the registered `http://www.opengis.net/def/rel/ogc/1.0/conformance`
/ `…/data` relations Common Part 1 names. Collection discovery supports `bbox`,
`bbox-crs` (CRS84 only), `datetime`, `q`, `query`, `limit`, `offset` and `f` through
[api-common](../api-common/README.md). Unknown/unsupported or duplicate controls
return structured HTTP 400 errors. Filters run before paging; JSON/HTML links
preserve filters and the negotiated format. Advertised spatial extents are normalized to the CRS84
domain (grid cell edges past ±180°/±90° are clamped; an extent describing no area
is omitted), matching the other APIs and collection search. Collection descriptions expose HTML
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
| `locations` | `/collections/{id}/locations`, `/locations/{locId}` | ✓ | GeoJSON list, complete without `limit` and paged with it (see below), + CoverageJSON/PNG series per location; `{locId}` may be a comma-delimited list of up to 64 ids, answered as one CoverageCollection in request order, CoverageJSON only, and with a `datetime` list at most 256 ids × instants (see [Location lists](#location-lists)) |
| `position` | `/collections/{id}/position` | ✓ | `POINT` or `MULTIPOINT` (fanned out, flattened into one CoverageCollection — per-point grouping not preserved; at most 64 points, 16 KiB decoded coordinates, 1 million values combined, and with a `datetime` list at most 256 points × instants; all coordinates finite and within CRS84 bounds) |
| `area` | `/collections/{id}/area` | ✓ | WKT `POLYGON` (holes allowed) or `west,south,east,north`; PNG rejected |
| `radius` | `/collections/{id}/radius` | ✓ | `coords=POINT`, `within`, `within-units=km\|m\|mi`; default trait impl = 64-vertex geodesic polygon → `query_area`; capped at 1000 km; pole/antimeridian circles are 400 (#667) |
| `trajectory` | `/collections/{id}/trajectory` | partial | 2-D `LINESTRING` only, meaning a *vertical cross-section* (PVOL sites). `LINESTRINGZ/M` (per-node z/time) not accepted; no along-path sampling on gridded engines |
| `instances` | `/collections/{id}/instances`, `/instances/{instanceId}` | ✓ | forecast model runs (`ds_core::instances`); instance-scoped queries: position, area, radius only |
| `cube` | — | ✗ | not in the trait or the router |
| `corridor` | — | ✗ | not in the trait or the router (`corridor-width`/`-height` documented as follow-up on trajectory) |
| `items` | `/collections/{id}/items`, `/items/{itemId}` | ✓ | GeoJSON features of the collection's `FeatureEngine`, for EDR collections whose engine has one (see [Items](#items)); `bbox`, `datetime`, `limit` + `offset` paging |

A query type a collection's engine does not support (not in its `supported_query_types`, so not in `data_queries`) has no resource: position, area, radius and trajectory all answer 404 `NotFound`, and `/api` omits the path (#668). Items follows the same rule: a collection without a feature engine has no `items` in `data_queries`, no path in `/api`, and a 404 on `/items`.

### Location lists

EDR 1.2 lets `/locations/{locationId}` name several locations
(`/req/edr/REQ_rc-locationid-definition` and `-response`, #923), as in
`/locations/EGLL,EFHK`. Every collection that advertises `locations` says so
with `multiple_locations: true` in its locations link variables
(`/req/edr/rc-locations-variables`): the handler fans the list out over the
engine's own `query_location`, so no engine needs code for it.

| | Behaviour |
|---|---|
| one id | exactly as before: a Coverage, or the engine's CoverageCollection; 404 when it has no data in the window. A list that repeats one id is one id |
| separator | a literal comma; `%2C` is a comma inside an id, as OpenAPI `style: simple` sends one, so such an id is addressable alone or in a list. Before #923 a literal comma was part of the id. Elements are not trimmed |
| list | one CoverageCollection, even with one coverage left: each id's coverages in request order, flattened like a MULTIPOINT, so the coverages carry no id and are told apart by their domain's x and y. A repeated id is answered once |
| no data | a known id without data in the window contributes nothing; when no listed id has data, 204 with no body, carrying the data-query `Cache-Control` |
| unknown id | 404 naming it, failing the whole list. Engines answer an unknown id and one without data alike, so the inventory (`get_locations`) tells them apart, read only when some id is not answered |
| limits | at most 64 ids, counted before repeats collapse, and an empty element (`a,,b`, a leading or trailing comma) are 400; with a `datetime` list at most 256 ids × instants (`MAX_LOCATION_LOOKUPS`, as MULTIPOINT points × instants), a 400 naming both counts before any engine call; 1 million values combined, as MULTIPOINT; `limit` keeps the first coverages, and the ids past it are not queried but must still exist |
| execution | one `query_location` per id in turn on the bounded EDR executor, the deadline checked before each, as between MULTIPOINT points. A `datetime` list runs per id exactly as for that id alone (`datetime_list::run`): one query per instant, merged, and an id with no data at any instant counts as an id without data |
| PNG | 400: the plot labels series by index, not by location |

### Items

`items` (#928) delegates to the collection's `FeatureEngine`: EDR 1.2 defines
the query by reference to OGC API - Features, so there is no `EdrEngine` method
for it. The server wires it for every collection that lists `edr` and whose
engine implements `FeatureEngine` for the same collection, whether or not it
also lists `features`; the engine matrix below says which. Filtering and paging
are the Features API's (`api-features` parsing, GeoJSON encoding and links), so
the per-engine semantics of `bbox` and `datetime` are those in the
[Features engine matrix](../api-features/README.md#per-engine-matrix).

| | Behaviour |
|---|---|
| `/items` | GeoJSON `FeatureCollection` with `numberMatched`, `numberReturned`, `timeStamp` and `self`/`next`/`prev` links under `/edr` that carry `bbox` and `datetime` |
| `/items/{itemId}` | GeoJSON `Feature` with `self` and `collection` links; an unknown id is 404 |
| `bbox` | CRS84, 4 or 6 values (heights ignored), `west > east` crosses the antimeridian; malformed → 400. No `bbox-crs` |
| `datetime` | RFC 3339 instant, `start/end`, `../end`, `start/..`; a reversed interval → 400; on a collection whose features carry no time (PostGIS stations) → 400 rather than the unfiltered set (Features #682; EDR's abstract test would include time-less features) |
| `limit` | `/req/edr/rc-limit-*`: default 10, maximum 10 000; a larger value is served as 10 000; 0, a sign, a fraction or text → 400 |
| `offset` | position of a page, emitted by the `next`/`prev` links; not an EDR parameter |
| `f` | `GeoJSON` (the only format), `application/geo+json`, `json`, `application/json`; anything else → 400. `/items/{itemId}` takes only `f` |
| other parameters | 400 naming the valid ones (`sortby`, property filters, `crs`, `bbox-crs`, `parameter-name`, …); a repeated parameter → 400 |
| execution | on the bounded EDR query executor, like every data query; engine errors map through `map_query_error` |
| caching | the ETag hashes the page with `timeStamp` blanked, so `If-None-Match` revalidates; a closed `datetime` window in the past gets the long `Cache-Control` |
| metadata | `data_queries.items` with `title`, `description`, `query_type`, `output_formats`, `default_output_format` and `crs_details` (CRS84); not on instance documents, no `itemType` or `rel=items` link (those mean a Features resource with an HTML view) |

Not implemented: `/instances/{instanceId}/items`, HTML, the Features
extensions (`sortby`, property filters, `crs`), and the EDR GeoJSON feature
schema that `/rec/core/edr-geojson` recommends (a SHOULD): features carry their
engine's properties, not `datetime`/`parameter-name`/`label`/`edrqueryendpoint`.

Every advertised query type's `data_queries.<type>.link.variables` carries
the six fields EDR 1.2 requires (#918): `title` (`Position query`, …), a
`description` naming what `coords` takes, `query_type`, `output_formats`
(PNG only for locations, position and trajectory), `default_output_format`
(`CoverageJSON`) and `crs_details`, which lists the one CRS data queries
accept: `CRS84` with the WKT2 of OGC:CRS84, longitude first. Radius adds
`within_units`, locations `multiple_locations: true` (#923, see
[Location lists](#location-lists)). The same builder serves collection
documents, the `/collections` list and instance documents; the
`data_queries.instances` link carries only `query_type`, which is all 1.2's
`instancesLink` asks for. `parameter_names` entries carry no `dataType`:
engines do not report a value type, every CoverageJSON range is encoded as
`float`.

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
| `bbox` | ✓ | `items` only (CRS84); `/collections` discovery also takes one |
| `datetime` | ✓ | RFC 3339 instant, `start/end`, `../end`, `start/..`, and the EDR 1.2 list of instants `T1,T2,T3` (`/req/core/datetime-response` D). A list names at most 16 instants (`params::MAX_DATETIME_INSTANTS`), since each is a sequential engine query and no intervals; repeats collapse. Each instant is its own engine query with the window `(t, t)`, so it is matched exactly as a request for that instant alone; the answers merge (`src/datetime_list.rs`): series and `t`-axis grids at the same place join into one coverage with every instant's steps, ascending and each once, and other coverages are listed. An instant with no data (the engine's 404) contributes nothing; none with data is that 404, and any other engine error fails the request. The merged response is bounded to 1 million values; the deadline is checked before every instant. The repeating-interval form `R[n]/date-time/interval` is not accepted (400) |
| `parameter-name` | ✓ | comma-separated, case-insensitive, repeats collapse; any unknown name (or an empty list) is a 400 listing the valid names — one rule in `ds_core::edr_engine::select_parameters` for GeoTIFF, ODIM, Zarr, QueryData and Nowcast (#666); GRIB keeps its own equivalent check |
| `z` | ✓ | EDR 1.2 grammar (`/req/edr/z-response`): a level, a list, a closed `min/max` interval, the open intervals `../max` and `min/..` (an open end reaches the lowest or highest advertised level), and the recurring interval `Rn/min/step` (`n` levels from `min`, `step` apart, as in the standard's `R20/100/50` = 20 levels; at most 1000, non-zero step). An interval selects the advertised levels inside it (none is a 400). A level, a list and a recurring interval go to the engine as a list, which it matches its own way: ODIM snaps to the nearest sweep, GRIB requires exact levels. A collection with no vertical extent **ignores** a well-formed `z` on every query route, instance routes included (clause A, a SHALL in 1.2); a malformed `z` is still a 400 everywhere |
| `f` | partial | `CoverageJSON` (default) and `PNG` (position/locations/trajectory plots; one location, not a list) only, case-insensitively, also as media types: `application/vnd.cov+json`, `application/prs.coverage+json` (EDR 1.1's type, still accepted), `image/png` (encode `+` as `%2B`; a bare `+` read as a space is accepted). CoverageJSON is always sent as `application/vnd.cov+json`, the EDR 1.2 type (#920), whichever `f` spelling or `Accept` header asked for it; `/api` and the `/locations` data links name the same type. Metadata resources take `json`/`html` or `application/json`/`text/html` (#510). No CSV/NetCDF/GeoJSON |
| `crs` | ✗ | data queries accept CRS84 only, which every `data_queries` link advertises in `crs_details` (#918); the `crs` parameter itself is not parsed (#84); `bbox-crs` on `/collections` is CRS84 only |
| `within`, `within-units` | ✓ | radius only |
| `resolution-x`/`-y`/`-z` | ✗ | (cube / area resolution hints) not accepted |
| `limit` | ✓ | EDR 1.2 `/req/edr/rc-limit-definition`: an integer from 1 to 10000; a larger value is clamped to 10000, not an error; `0`, a sign, a fraction, an exponent or a non-number is a 400. Absent means no limit, not the spec's suggested default of 10. On position, area, radius, `/locations/{locId}` and the instance position/area/radius routes it caps the top-level coverages of a CoverageCollection, in engine order; the rest are dropped, since CoverageJSON has no paging links. A single Coverage is one object and is unchanged. A MULTIPOINT keeps the first coverages in point order, then each point's own order, so a vertical profile per step counts once per step, and the points past the limit are never queried. A list of location ids does the same in id order; the ids past the limit are not queried, but an unknown one is still a 404. On `/locations` it pages the list, below. Not on trajectory, where it is a 400: EDR 1.2 does not list it there. `items` (#928) pages with the Features default of 10. `/collections` pages with Common's default and maximum of 1000 |
| `offset` | ✓ | `/locations` with `limit`, as on `/collections`: the offset pagination extension. `offset` without `limit` on `/locations` is a 400. `/locations` takes only `limit`, `offset`, `bbox`, `datetime` and `f`; any other parameter is a 400 naming them. `bbox` and `datetime` are accepted but not applied yet (#932) |

Data queries execute on a dedicated, bounded runtime, including radius and
instance routes. Admission is capped at 2–8 concurrent queries (available CPUs,
clamped), with room for 32 additional admitted requests waiting for a slot;
further requests receive 503 immediately. A 30-second deadline (including queue time) returns
504. Synchronous work already in progress retains its slot until it finishes;
a cancelled/timed-out MULTIPOINT or location list stops before its next engine call. This bounds
concurrency without claiming that synchronous engine I/O is preemptible.

For `/locations`, the same permit covers retrieval, metadata, direct JSON
serialization and ETag hashing (#533). Without `limit` the response is the
complete inventory, byte for byte what it was before paging existed: no
counts, one `self` link, no implicit pagination. With `limit` (EDR 1.2,
#922) it is one page of the inventory in the engine's order, which is stable
within one inventory snapshot: CSV first-seen order, PostGIS and BUFR by
station id. A page adds `numberMatched` and `numberReturned`, and `self`,
`next` and `prev` links built like `/collections` paging: the resolved,
clamped `limit`, `offset` omitted at 0, `prev` only from a non-empty page,
and every other query parameter of the request repeated. The arithmetic is
`ds_core::collection_search::page_window`, the one `/collections` uses. An
`offset` past the end is an empty page without `next` or `prev`. A page
refreshed between requests can shift, as with any offset paging.

No intermediate JSON tree duplicates every location and its parameter
metadata. Encoded location buffers, a complete inventory or a page alike, are
bounded by `MC_EDR_LOCATIONS_MAX_BYTES` (default 16 MiB per response) and
`MC_EDR_LOCATIONS_MEMORY_MB` (default 128 MiB process-wide). Memory
reservations cover buffer growth, including the old and new allocations
during copying, and remain with response bytes through middleware and client
delivery. Exhaustion returns 503 without partial JSON or truncation: a
complete inventory over the per-response cap is a `ResponseLimit` whose
description suggests paging it with `limit`, a page over it one that
suggests a smaller `limit`. Cancellation/deadlines stop serialization.
Engine-owned inventory snapshots and the `get_locations()` result are
separate from this encoded-buffer budget and are the whole inventory for a
page too; retrieval remains under the bounded query executor.

Every 200 carries `Cache-Control` + a strong ETag; `If-None-Match` → 304 (#499).

`/api/docs` uses embedded Swagger UI 5.33.0 assets, served under
`/api/docs/{asset}`, with a same-origin script policy and `nosniff` headers.
No executable documentation assets or validation requests use a CDN (#587).

## Parameter metadata (Metocean Profile, Requirement 7)

A collection's `parameter_names` and a data query's CoverageJSON
`parameters` come from the same builders in `src/response.rs`, so a query
describes each parameter the way its collection does. Status against the
EUMETNET/OGC API - EDR Metocean Profile `/req/core/collection_parameter_names`
(#273):

| | Requirement | Status | Notes |
|---|---|---|---|
| A | keys and `id` carry no structured metadata | ✓ | keys are the engine's parameter names; no parameter-level `id` is emitted |
| B | `label`, `description`, `unit` | partial | `label` and `description` always, and they differ: `description` is the full engine label plus the served unit (`2 metre temperature, in K`), or `… (unit not specified)`. `unit` only where the engine knows one (table below) |
| C | `label` ≤ 50 characters | ✓ | a longer engine label is cut to 49 characters + `…`; the whole text stays in `description` and `observedProperty.label` |
| D | `label` in English | partial | the built-in tables (ODIM quantities, GRIB WMO Code Table 4.2, BUFR SYNOP) are English; config- or source-given labels (CSV column names, GeoTIFF/PostGIS/Satellite config, Zarr `long_name`) are served as given, tagged `en` |
| E | `unit.symbol.type` = `https://qudt.org/vocab/unit/<unit>`, `value` = `qudt:symbol` | partial | every unit `ds_core::units::qudt_unit` knows — `K`, `°C`, `Pa`, `hPa`, `m/s`, `km/h`, `m`, `km`, `cm`, `mm`, `mm/h`, `%`, `dB`, `°`, `kg/m²`, `kg/(m²·s)`, `kg/m³`, `kg/kg`, `J/kg`, `J/m²`, `W/m²`, `m²/s²`, `m³/m³`, `Pa/s`, `/s`, `s`, `min`, `h`, `DU`, `kA`, in their UCUM, CF/udunits and WMO spellings. Units with no faithful QUDT entry keep the engine's string typed as UCUM: `dBZ` (QUDT's `DeciB_Z` is acoustic Z-weighting, not reflectivity), `gpm`, `deg/km`, CF `1`, BUFR code tables. `unit.label` stays the engine's unit string |
| F | `observedProperty.id` = `https://vocab.nerc.ac.uk/standard_name/<name>` when CF, else `observedProperty.description` | partial | the CF URI when the engine knows the standard name (`ParameterDescription.standard_name`, only set from a CF `standard_name` attribute; a value with a CF modifier is not published). Otherwise `observedProperty.description` carries the description, and CoverageJSON keeps the engine's parameter name as `observedProperty.id` |

CoverageJSON parameters carry no parameter-level `label`: CoverageJSON asks
to leave it out when it equals `observedProperty.label`, which holds it.

| Engine | `unit` from | CF standard name |
|---|---|---|
| CSV | built-in column table (`temperature` °C, `humidity` %, `wind_speed` m/s, `pressure` hPa, `precipitation` mm); other columns none | – |
| GeoTIFF | config `unit` | – |
| GRIB | Code Table 4.2 unit after display conversion (`°C`, `hPa`, `m s-1`, `mm`, `%`, …) | – (WMO triples are not mapped to CF) |
| QueryData | none: descriptors carry no trustworthy unit | – |
| Zarr | CF `units` attribute | ✓ `standard_name` attribute |
| ODIM composite | config `unit` | – |
| ODIM PVOL | ODIM quantity table (`dBZ`, `m/s`, `dB`, `deg`, …), in `parameter_names` too; none for dimensionless quantities (RHOHV, SQI, QIND) | – |
| PostGIS | config `unit` | – (config `observed_property` is free text) |
| BUFR | Table B unit after display conversion (`°C`, `hPa`, `mm`, …) | – (built-in `observed_property` values are CF names, except `present_weather`, but not marked as such) |
| Nowcast | `m/s` (motion); none for `motion_quality` | – |
| Satellite | config product `unit` (`1` for GMGSI's display counts, no QUDT entry) | – (the NetCDF `standard_name` is not read) |

Remaining for Requirement 7: CF standard names for every engine but Zarr (a
vocabulary for GRIB/ODIM/BUFR built-ins, a `standard_name` config key for the
config-driven engines), QueryData units, and a check that config-given
labels are English.

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
retrieval, including compressed-cache hits and coalesced waiters. Plain Zarr and
outer-transformed Icechunk arrays release encoded/intermediate allowances after
each stored chunk finishes, so completed chunks do not accumulate reservations
over a position, area, or radius window. Inner/index buffers stay admitted until
that chunk finishes; native/source buffers remain covered through sampling. Native decoded
cache hits do not need encoded admission. Numeric-variable gzip/zstd/Blosc outputs
are capped by the declared codec representation, including sharded layouts;
if bounded-codec setup fails for one variable, catalog discovery warns and
omits that variable while retaining usable ones. Catalog construction still
fails if no usable variables remain. During reads,
bounded intermediate outputs acquire additional capacity allowances through
retrieval. Blosc also validates frame/block sizes before full and partial
decoding and admits its size-dependent native scratch buffers. Scratch
allowances end after each decoder call, releasing extra capacity and returning
prepaid credit for sequential reuse; encoded/intermediate copies remain
admitted through retrieval. Partial Blosc ranges are validated before native
decoding, including overflowing and out-of-bounds ranges. Invalid frame
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
that allowance before reserving extra capacity. Stacked Blosc codecs prepay the
largest scratch allowance within each serial codec chain, allowing sequential
calls to reuse that capacity; encoded/intermediate allowances still add up and
concurrent chunks retain independent reservations. Unknown layouts and estimates
too large for a single chunk use serial actual-size admission; conservative
bounds can reduce concurrency, but do not become new codec-output limits.
Worker deadlines and reservations remain attached until all chunk jobs finish,
including error and cancellation paths. Plain Zarr and shards with outer
transforms retain serial retrieval.

## Per-engine query-type matrix

✓ implemented · – not implemented · n/a not applicable (no model runs).

| Engine | locations | position | area | radius | trajectory | instances | items | Area semantics |
|---|---|---|---|---|---|---|---|---|
| CSV | ✓ | – | ✓ | ✓ | – | n/a | ✓ stations | stations whose point is inside the polygon (≤ 500) |
| GeoTIFF | – | ✓ | ✓ | ✓ | – | n/a | – | Grid over the polygon's bbox at native resolution, no 256-cell coarsening (≤ 1M values across timesteps → 400, checked before any read, #858), pixels outside the polygon masked; `t` axis when several steps; a file that cannot be read is a timestep of nulls, any other error fails the query |
| GRIB | – | ✓ | ✓ | ✓ | – | ✓ | – | Grid over the polygon's bbox at native resolution (≤ 1M values across levels/parameters), cells outside the polygon masked; antimeridian-crossing bboxes rejected (#667) |
| QueryData | – | ✓ | ✓ | ✓ | – | ✓ | – | Grid over bbox at native resolution, ≤ 256 cells/axis, cells outside the polygon masked (vertex fallback for sub-cell shapes); polygon outside the extent → 404; `t` axis when several steps. Lat/lon, rotated, stereographic and LCC grids (including the tangent-cone MEPS grid, on the sphere its file declares); a projected grid's extent is its projected rectangle's edges, not its corners' lon/lat box |
| Zarr | – | ✓ | ✓ | ✓ | – | ✓ | – | Grid over bbox at native resolution, ≤ 256 cells/axis, one subset retrieval per variable for the whole time span (each may read multiple chunks) (two across the antimeridian; cells within half a native cell of ±180° are not interpolated across the seam, #667), at most 8 variables per request, cells outside the polygon masked (vertex fallback for sub-cell shapes); polygon outside the extent → 404; `t` axis when several steps. Forecast stores (reference + lead axes) expose every run as an instance; `None` ⇒ latest |
| ODIM composite | – | ✓ | ✓ | ✓ | – | n/a | – | Grid over bbox, ≤ 256 cells/axis, masked to the polygon; `t` axis when several steps |
| ODIM PVOL site | ✓ | ✓ | ✓ | ✓ | ✓ | n/a | – (the site inventory is the network's Features collection) | polar sampling; trajectory = RHI cross-section |
| PostGIS stations | ✓ | ✓ | ✓ | ✓ | – | n/a | ✓ stations, no `datetime` | stations-only `location_source`: exact `ST_Within` in SQL; observations-derived: exact point-in-polygon on the cached station set |
| PostGIS events | – | – | ✓ | ✓ | – | n/a | – (events as features = #503) | events in the polygon (exact, in SQL) as a `Point` CoverageCollection |
| BUFR | ✓ | ✓ | ✓ | ✓ | – | n/a | ✓ stations | stations whose point is inside the polygon (exact, in memory; ≤ 10 001 stations, ≤ 500 000 values per response → 400); position = nearest station within `position_radius_km` (25 km) else 404; one `PointSeries` per station over the in-memory `retention` window; same semantics for the polled-directory and WIS2 (push) sources; units are the BUFR units mechanically converted for display (K → °C, Pa → hPa, kg m-2 → mm) like GRIB |
| Nowcast | – | – | ✓ (motion field) | ✓ | – | ✓ | ✓ tracked cells | motion blocks over the polygon's bbox, blocks outside the polygon masked; reflectivity via EDR = #523 |
| Satellite | – | ✓ | ✓ | ✓ | – | n/a | – | GOES-R, Himawari-9 (ISatSS) and the GMGSI global mosaic, whose values are 8-bit display counts (unit `1`), not brightness temperatures; its grid wraps at 180°, so a position either side of the seam, or an area given west > east across it, reads the pixels there. Each product is a parameter on its own time axis: the time axis of a response is the union of the selected products' scans (null where a product has none), an instant snaps per product to its latest scan at or before it, and `parameter_names` carries each product's own `extent.temporal`. RGB composites (`[[satellite.composites]]`) are map layers, not EDR parameters: naming one in `parameter-name` → 400. Position = the pixel under the point per scan; behind the Earth or off the mosaic's 72°S–72°N → 404. Area = grid over the bbox at the nadir pixel size (≤ 256 cells/axis), sampled through a coarse projection grid, masked to the polygon; polygon outside the imagery → 404; `t` axis when several scans. A query may download at most 8 scans the cache evicted and decode at most 1024 image blocks (summed per product grid; a GOES-R block is a strip of 24 full-width rows, a GMGSI block a 793 × 1322 chunk) → 400 |
| CAP, GeoJSON | — no `EdrEngine` (Features/Maps only) — | | | | | | | |

Radius, cube, corridor, items and location lists have no engine-specific
code: radius is answered by every engine that answers area, items by every
engine that implements `FeatureEngine` for its EDR collection (the Features
engine matrix describes each one's features), a list of location ids by
every engine that answers locations, one `query_location` per id, and cube
and corridor do not exist.

Compatible nowcast reloads retain motion-field instances alongside forecast
runs and cell history (#604). Reuse requires unchanged nowcast config, the
same raster source engine and a compatible retained geometry/product contract;
source/tuning changes still rebuild. Auxiliary source edits affect subsequent
generations, not already-published instances.

## Known gaps, in suggested order

1. `locations` and `trajectory` under `/instances/{id}/`.
2. `cube` and `corridor` (derivable from area / trajectory).
3. `crs` on data queries, adding its CRSs to `crs_details` (#84); EDR GeoJSON output for point results (then declare `edr-geojson`).
4. `items` for more collections: PostGIS events (with #503), and CAP/GeoJSON if they become EDR collections.

Related issues: #585 MULTIPOINT fan-out bound · #667
antimeridian bboxes · #668 400-vs-404 on unsupported query types · #665
GRIB value rounding · #523 nowcast
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
step) for multiple levels. `z` omitted selects all levels; single, list,
closed/open interval and recurring (`Rn/min/step`) selectors are supported — a
recurring interval is a list, so each of its levels must exist. Area/radius queries return a `[z,y,x]` Grid at one
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
are null in position and errors in area; `z` is ignored (no vertical extent). A temperature
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
