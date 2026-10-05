# api-edr — OGC API - Environmental Data Retrieval 1.1 status

This page is the single source of truth for **what MeteoCore's EDR
implementation supports and what it does not**, per query type and per
engine. It is written for integrators and for anyone planning EDR work.

**Keep it current: every PR that touches EDR behaviour — `crates/api-edr`,
`EdrEngine` in `crates/core/src/edr_engine.rs`, or an engine's `EdrEngine`
implementation / `supported_query_types` — must update the tables below in
the same PR.** The `crates/api-edr/CLAUDE.md` rule points here.

Spec: OGC API - EDR 1.1 (OGC 19-086r6), plus EDR 1.2's `limit`,
locations paging (#922), several location ids in one locations query
(#923) and the cube query with `resolution-z` (#925). Base route: `/edr`.

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
| `geojson` | ✓ | feature content is `application/geo+json`: the `/locations` list, `items`, and the point queries of station collections (see [GeoJSON output](#geojson-output)) |
| `edr-geojson` | ✓ | those bodies are EDR GeoJSON: every feature's `properties` carries the `edrProperties` members `datetime`, `parameter-name`, `label` and `edrqueryendpoint`, on `items` too (#970, see [Items](#items)). Each route's body validates against the EDR 1.1 and 1.2 bundles' `application/geo+json` schema, a single item against the items list's feature schema, 1.2's `featureGeoJSON` (`tests/geojson_output_tests.rs`, `tests/items_tests.rs`, `crates/server/tests/edr_geojson.rs`) |

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

### Schema validation

Tests validate real router responses against both vendored EDR OpenAPI 3.0
bundles, 1.1 and 1.2 (#919). 1.1 stays declared, so both must pass. Covered:
the landing page, `/conformance`, `/collections`, collection documents (with
locations, position, area and radius queries, a vertical extent, and a
satellite collection's per-parameter time axes), the instances list, one
instance document, and the `/locations` GeoJSON. A negative control drops one
data query link's `title`, which only 1.2 requires, and expects 1.2 to reject
it. Both bundles' own `parameter_names` schema constrains no entry, so the
helper also checks each entry against their parameter schema; a second
negative control drops an entry's `observedProperty`. CoverageJSON data
responses validate against `schemas/coveragejson.json` instead: the 1.2 OpenAPI 3.0 bundle's NdArray schema rejects valid float
arrays. The shared helper is `tests/support/edr_schema.rs`;
[`schemas/README.md`](../../schemas/README.md) records the pinned upstream
commit and why the 3.0 bundle rather than the 3.1 one.

## Query types

| Query type | Route | Status | Notes |
|---|---|---|---|
| `locations` | `/collections/{id}/locations`, `/locations/{locId}` | ✓ | GeoJSON list, each feature's data link and `edrqueryendpoint` carrying its id percent-encoded (#970), complete without `limit` and paged with it, `bbox` and `datetime` filtering it before paging (see below), + CoverageJSON/PNG series per location, and EDR GeoJSON on station collections; `{locId}` may be a comma-delimited list of up to 64 ids, answered in request order as one CoverageCollection or, on station collections, one EDR GeoJSON FeatureCollection (no PNG), and with a `datetime` list at most 256 ids × instants (see [Location lists](#location-lists)) |
| `position` | `/collections/{id}/position` | ✓ | `POINT` or `MULTIPOINT` (fanned out, flattened into one CoverageCollection — per-point grouping not preserved; at most 64 points, 16 KiB decoded coordinates, 1 million values combined, and with a `datetime` list at most 256 points × instants; all coordinates finite and within CRS84 bounds); EDR GeoJSON too on station collections, one feature per point |
| `area` | `/collections/{id}/area` | ✓ | WKT `POLYGON` (holes allowed) or `west,south,east,north`; PNG and GeoJSON rejected |
| `radius` | `/collections/{id}/radius` | ✓ | `coords=POINT`, `within`, `within-units=km\|m\|mi`; default trait impl = 64-vertex geodesic polygon → `query_area`; capped at 1000 km; pole/antimeridian circles are 400 (#667); PNG rejected; EDR GeoJSON too on station collections |
| `trajectory` | `/collections/{id}/trajectory` | ✓ | gridded engines (GRIB, QueryData, Zarr): values sampled along a WKT `LINESTRING`, `LINESTRING Z`, `M` or `ZM` (Z = level, M = Unix epoch seconds), CoverageJSON `Trajectory` only (PNG and GeoJSON → 400), see [Trajectory](#trajectory-926); PVOL sites: a 2-D `LINESTRING` is a *vertical cross-section* (`Section`, also PNG, never GeoJSON). `MULTILINESTRING` not supported |
| `instances` | `/collections/{id}/instances`, `/instances/{instanceId}` | ✓ | forecast model runs (`ds_core::instances`); the id is the run's RFC 3339 reference time, e.g. `2026-06-07T06:00:00Z` (see [Instance ids](#instance-ids)); instance-scoped queries: position, area, radius, cube |
| `cube` | `/collections/{id}/cube` | ✓ | `bbox` required: CRS84, four numbers, or six whose vertical pair is a `z` interval that an explicit `z` overrides. `z` optional, in the full `z` grammar below: absent → every level; ignored, like the six-number pair, on a collection without a vertical extent. `datetime` as on every query, a list included: one cube per instant, joined along `t` into one `Grid`. `resolution-x`/`-y`/`-z` (`resolution-z` without a vertical extent is a 400), `crs` (CRS84 only), `f` (CoverageJSON only: `PNG` and `GeoJSON` are 400, and `Accept` cannot choose another format). An unknown or repeated query parameter is a 400 naming the accepted ones. Response: a `Grid` with `t`, `z`, `y`, `x` axes, ≤ 1M values across timesteps × levels × cells × parameters → 400. `data_queries.cube.link.variables.height_units` is the vertical axis unit. Only collections with vertical levels offer it: GRIB pressure and model-level views (#925) |
| `corridor` | — | ✗ | not in the trait or the router (`corridor-width`/`-height` documented as follow-up on trajectory) |
| `items` | `/collections/{id}/items`, `/items/{itemId}` | ✓ | GeoJSON features of the collection's `FeatureEngine`, for EDR collections whose engine has one (see [Items](#items)); `bbox`, `datetime`, `limit` + `offset` paging |

A query type a collection's engine does not support (not in its `supported_query_types`, so not in `data_queries`) has no resource: position, area, radius, cube and trajectory all answer 404 `NotFound`, and `/api` omits the path (#668). Items follows the same rule: a collection without a feature engine has no `items` in `data_queries`, no path in `/api`, and a 404 on `/items`.

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
| GeoJSON | on a station collection, one EDR GeoJSON FeatureCollection: every id's features in request order, each with its own id, label and `edrqueryendpoint`, so unlike the CoverageCollection the features say which location they are. `f=GeoJSON` or `Accept: application/geo+json`; `numberReturned` is the feature count and `numberMatched` the count before `limit`, left out when `limit` stopped the list before the last id. A gridded collection answers 400, as for one id |
| PNG | 400: the plot labels series by index, not by location. `Accept` never picks PNG for a list |

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
| `/items` | EDR GeoJSON `FeatureCollection` with `numberMatched`, `numberReturned`, `timeStamp` and `self`/`next`/`prev` links under `/edr` that carry `bbox` and `datetime` |
| `/items/{itemId}` | EDR GeoJSON `Feature` with `self` and `collection` links; an unknown id is 404 |
| feature `properties` | the engine's own, plus the `edrProperties` members `/req/edr-geojson/content` A requires (#970), with the values the item's `/locations` feature carries, since every station item is one of the collection's locations: `label` the location's label, `edrqueryendpoint` its `/locations/{locId}` query with the id percent-encoded, `parameter-name` the collection's parameters and `datetime` its temporal extent as `start/end`, empty without one. An item that is not a location gets its `name` property or id as `label` and, as `edrqueryendpoint`, the radius query its engine sizes for it (`EdrEngine::item_radius`, `within` rounded up to 100 m) with the item's own instant as `datetime`; else a `position` query at its point; else the collection. A tracked nowcast cell gets its id, `radius?coords=POINT(lon lat)&within=<r>&within-units=km` over the motion field with `r` its `area_km2` as a disc, and its `observed` frame time. No floor on `r`: a circle narrower than a motion block still answers the nearest block (#671). The members replace an engine property of the same name. They are added by `api-edr` only: the Features API's `/items` keeps the engine's properties alone |
| `bbox` | CRS84, 4 or 6 values (heights ignored), `west > east` crosses the antimeridian; malformed → 400. No `bbox-crs` |
| `datetime` | RFC 3339 instant, `start/end`, `../end`, `start/..`; a reversed interval → 400; on a collection whose features carry no time (PostGIS stations) → 400 rather than the unfiltered set (Features #682; EDR's abstract test would include time-less features) |
| `limit` | `/req/edr/rc-limit-*`: default 10, maximum 10 000; a larger value is served as 10 000; 0, a sign, a fraction or text → 400 |
| `offset` | position of a page, emitted by the `next`/`prev` links; not an EDR parameter |
| `f` | `GeoJSON` (the only format), `application/geo+json`, `json`, `application/json`; anything else → 400. `/items/{itemId}` takes only `f` |
| other parameters | 400 naming the valid ones (`sortby`, property filters, `crs`, `bbox-crs`, `parameter-name`, …); a repeated parameter → 400 |
| execution | on the bounded EDR query executor, like every data query; engine errors map through `map_query_error` |
| caching | the ETag hashes the page with `timeStamp` blanked, so `If-None-Match` revalidates; a closed `datetime` window in the past gets the long `Cache-Control` |
| metadata | `data_queries.items` with `title`, `description`, `query_type`, `output_formats`, `default_output_format` and `crs_details` (CRS84); not on instance documents, no `itemType` or `rel=items` link (those mean a Features resource with an HTML view) |

Not implemented: `/instances/{instanceId}/items`, HTML, and the Features
extensions (`sortby`, property filters, `crs`). The EDR GeoJSON feature schema
that `/rec/core/edr-geojson` recommends is the encoding (#970). `datetime` is
the collection's extent, as on `/locations`, not the station's own reporting
period.

Every advertised query type's `data_queries.<type>.link.variables` carries
the six fields EDR 1.2 requires (#918): `title` (`Position query`, …), a
`description` naming what `coords` takes, `query_type`, `output_formats`
(from `params::query_formats`, the list the handlers negotiate over: PNG
only for locations, position and a radar cross-section trajectory, GeoJSON
only for the point queries of station collections; an along-path trajectory
is CoverageJSON only, and its description names the Z/M forms),
`default_output_format`
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
| `/instances/{instanceId}/cube` | ✓ |
| `/instances/{instanceId}/locations`, `/trajectory` | ✗ |

### Instance ids

An instance `id` is its run's reference time in RFC 3339 UTC,
`2026-06-07T06:00:00Z`, as the MetOcean EDR profile requires
(`/req/nwp/collection_granularity` C, #947): whole seconds always, a
fraction only when the run has one. The instance title says `run
2026-06-07T06:00:00Z`. Links carry the colons unencoded, which RFC 3986
`pchar` allows: `/collections/{id}/instances/2026-06-07T06:00:00Z/position`.
`{instanceId}` accepts:

- the id itself, percent-encoded or not (`2026-06-07T06%3A00%3A00Z`);
- any RFC 3339 offset naming the same instant (`2026-06-07T09:00:00+03:00`);
- the compact `20260607T0600Z` and `20260607T060000Z` stamps that were the
  id before #947, so existing links keep resolving.

Links in a response always use the canonical id, whichever form the request
named. Anything else is a 400; a well-formed id with no such run is a 404,
and so is every id on a collection without instances.

## Parameters

| Parameter | Status | Notes |
|---|---|---|
| `coords` | ✓ | WKT per query type (see above) |
| `bbox` | ✓ | `items` (CRS84), cube and `/locations`: four or six comma-separated numbers (EDR 1.2 `bbox`/`cube-bbox`, `style: form`, `explode: false`), `west > east` crossing the antimeridian; `/collections` discovery also takes one. On `/locations` (#932) it keeps the locations whose point lies inside the box, edges included, before `limit` pages the list; a six-number box's heights must be numbers and are otherwise ignored, since a location is a point without a height, as `items` ignores them. A malformed or repeated `bbox` is a 400 |
| `datetime` | ✓ | RFC 3339 instant, `start/end`, `../end`, `start/..`, and EDR 1.2's list of instants `T1,T2,T3` and repeating interval `Rn/date-time/duration` (`/req/core/datetime-response` D). A list names at most 16 instants (`params::MAX_DATETIME_INSTANTS`), since each is a sequential engine query and no intervals; repeats collapse. Each instant is its own engine query with the window `(t, t)`, so it is matched exactly as a request for that instant alone; the answers merge (`src/datetime_list.rs`): series and `t`-axis grids at the same place join into one coverage with every instant's steps, ascending and each once, and other coverages are listed. An instant with no data (the engine's 404) contributes nothing; none with data is that 404, and any other engine error fails the request. The merged response is bounded to 1 million values; the deadline is checked before every instant. A repeating interval (#933) is the list of its `n` instants, the start and then one duration apart, queried exactly as that list: `R4/2026-10-01T00:00:00Z/PT6H` is 00, 06, 12 and 18 UTC. `n` counts instants, as `z=R20/100/50` counts levels; the informative collection-response annex reads `R4/100/5` as five values instead, but the `z` parameter's example, the OpenAPI temporal extent example and ISO 8601 parsers all count `n` items. `n` is 1 to 16, a list's cap: `R0`, an unbounded `R/…` or `R-1/…`, and `R17` up are 400s. The duration is a positive ISO 8601 duration (`ds_core::datetime::parse_iso8601_duration`) in weeks or days, hours, minutes and whole seconds, added as a fixed length to the UTC start; calendar years and months (`P1M`, `P1Y`) are a 400, since their length varies, and so are a zero or signed duration, a fractional component and any other shape (`Rn/duration/end`, `Rn/start/end`). On an along-path trajectory a 2-D or Z path takes each listed instant (one coverage per instant), and any `datetime`, a list included, with a `LINESTRING M`/`ZM` is a 400: that path carries its own times. An interval that ends before it starts is a 400 on every route, as in Features, Maps and Tiles (#932; it used to reach the engines, and CSV and BUFR panicked on it). On `/locations` (#932) it keeps the locations with at least one observation in the interval, each instant of a list matched exactly, before `limit` pages the list: the rule of the station engines' Features `datetime` (#682), from the same engine code (`EdrEngine::location_time_filter`). A collection whose engine cannot tell when a location has data answers it with a 400 naming the collection, not the unfiltered list; see the engine matrix |
| `parameter-name` | ✓ | comma-separated, case-insensitive, repeats collapse; any unknown name (or an empty list) is a 400 listing the valid names — one rule in `ds_core::edr_engine::select_parameters` for GeoTIFF, ODIM, Zarr, QueryData and Nowcast (#666); GRIB keeps its own equivalent check |
| `z` | ✓ | EDR 1.2 grammar (`/req/edr/z-response`): a level, a list, a closed `min/max` interval, the open intervals `../max` and `min/..` (an open end reaches the lowest or highest advertised level), and the recurring interval `Rn/min/step` (`n` levels from `min`, `step` apart, as in the standard's `R20/100/50` = 20 levels; non-zero step). A list or a recurring interval names at most 1000 levels (`params::MAX_Z_LEVELS`, #940); more is a 400 naming the cap, raised while parsing, before any engine work. An interval selects the advertised levels inside it (none is a 400). A level, a list and a recurring interval go to the engine as a list, which keeps only the levels it has (clause B), in request order and each once, and answers 400 when it keeps none, as for an interval (#969): GRIB keeps the run's exact levels; an ODIM PVOL site keeps the sweep within 0.05° of each requested angle, half the 0.1° step its sweep angles are advertised at, so `z=50` on a 0.3–9° volume is a 400, not the 9° sweep. A pinned PVOL angle samples each volume's own sweep at that angle; a volume in the window without one has no value there. A cube without `z` returns every level (clause F and `/req/edr/cube-z-response` E), not the 400 that `/req/edr/rc-cube` E recommends: those SHOULDs conflict. A collection with no vertical extent **ignores** a well-formed `z` on every query route, instance routes included (clause A, a SHALL in 1.2); a malformed `z` is still a 400 everywhere. Cube also takes the interval from a six-number `bbox` when `z` is absent, ignored the same way without a vertical extent. On an along-path trajectory: the levels a 2-D or M path is sampled on; `z` with a `LINESTRING Z`/`ZM` is a 400 on every collection, since that path carries its own levels |
| `f` | partial | `CoverageJSON` (default), `GeoJSON` (locations/position/radius on station collections; never area or cube) and `PNG` (position/locations plots, one location, not a list; radar cross-section trajectories), case-insensitively, also as media types: `application/vnd.cov+json`, `application/prs.coverage+json` (EDR 1.1's type, still accepted), `application/geo+json`, `image/png` (encode `+` as `%2B`; a bare `+` read as a space is accepted). CoverageJSON is always sent as `application/vnd.cov+json`, the EDR 1.2 type (#920), whichever `f` spelling or `Accept` header asked for it; `/api` and the `/locations` data links name the same type. A format the query does not offer is a 400 naming the ones it does; each query's offer is its `data_queries` `output_formats`. Without `f`, the `Accept` header chooses among the offered media types by q-value (ties go to the order CoverageJSON, GeoJSON, PNG; wildcards and `application/json` keep CoverageJSON, nothing acceptable falls back to it rather than 406), and the response carries `Vary: Accept` when the query offers more than one format. Metadata resources take `json`/`html` or `application/json`/`text/html` (#510). No CSV/NetCDF |
| `crs` | partial | data queries serve CRS84 only, which every `data_queries` link advertises in `crs_details` (#918). Cube validates it: the CRS84 URI, `CRS84` or `OGC:CRS84` are accepted, anything else is a 400; the other data queries do not read it (#84). `bbox-crs` on `/collections` is CRS84 only |
| `within`, `within-units` | ✓ | radius only |
| `resolution-x`/`-y`/`-z` | partial | cube only: `n` evenly spaced positions from the bbox's west/south edge to its east/north edge (for `z`, from the lowest to the highest selected level), both ends included, each taking the nearest native value; a position more than half a cell off the grid is null. `0` or absent is the native resolution; a whole number up to 1 000 000, else 400 stating that range. Area does not take `resolution-x`/`-y` |
| `limit` | ✓ | EDR 1.2 `/req/edr/rc-limit-definition`: an integer from 1 to 10000; a larger value is clamped to 10000, not an error; `0`, a sign, a fraction, an exponent or a non-number is a 400. Absent means no limit, not the spec's suggested default of 10. On position, area, radius, `/locations/{locId}` and the instance position/area/radius routes it caps the top-level coverages of a CoverageCollection, in engine order; the rest are dropped, since CoverageJSON has no paging links. A single Coverage is one object and is unchanged. A MULTIPOINT keeps the first coverages in point order, then each point's own order, so a vertical profile per step counts once per step, and the points past the limit are never queried. A list of location ids does the same in id order; the ids past the limit are not queried, but an unknown one is still a 404. On `/locations` it pages the list, below. Not on trajectory or cube, where it is a 400: EDR 1.2 does not list it for either, and cube returns a single Grid coverage. `items` (#928) pages with the Features default of 10. `/collections` pages with Common's default and maximum of 1000 |
| `offset` | ✓ | `/locations` with `limit`, as on `/collections`: the offset pagination extension. `offset` without `limit` on `/locations` is a 400. `/locations` takes only `limit`, `offset`, `bbox`, `datetime` and `f`; any other parameter is a 400 naming them. `bbox` and `datetime` filter before paging (#932) |

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
station id. `bbox` and `datetime` (#932) filter the inventory first, in
place in that order, so a page is cut from the locations inside the box with
an observation in the window, and its counts and links describe that list.
`datetime` is decided by the engine (`EdrEngine::location_time_filter`), on
the query executor from its in-memory index: CSV observation rows, BUFR
reports, an ODIM PVOL site's volume times (not its temporal extent: a window
between two volumes matches nothing, as `/locations/{nod}` answers 404
there). Every other engine, PostGIS stations included, cannot tell and
answers `datetime` with a 400. A filtered list without `limit` stays
unpaged, but its `self` link carries the request's query. A page adds `numberMatched` and
`numberReturned`, and `self`,
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

## GeoJSON output

Station collections, whose engine answers time series at its named
locations (`EdrEngine::serves_station_series`: CSV, PostGIS station shapes,
BUFR), also answer their point queries (`locations/{locId}`, `position`,
`radius`) as EDR GeoJSON (#929): `f=GeoJSON`, `f=application/geo+json` or
`Accept: application/geo+json`. Everything else keeps CoverageJSON, and
`f=GeoJSON` is a 400 there: gridded, polar and nowcast engines, PostGIS
events (`Point` coverages with no station), `area` and `cube` on every
engine (not point queries), and `trajectory`.

The body is an EDR GeoJSON FeatureCollection with **one feature per
station**, as the CoverageJSON twin has one coverage per station (a
MULTIPOINT position has one per point):

- `geometry`: the station's `Point`; `id`: the location id.
- `properties`: the members EDR's `edrProperties` requires, then the series.
  `datetime` is the instant or the `start/end` period the series spans;
  `label` the station name; `parameter-name` the parameters carried;
  `edrqueryendpoint` the station's `/locations/{locId}` resource (the id
  percent-encoded); `time` the RFC 3339 instants; and one array per
  parameter aligned with `time`, `null` where there is no value (as in
  CoverageJSON).
- `parameters`: the parameter objects of the CoverageJSON twin, each with its
  `id`, as an array. `links`: `self`, an `alternate` per other offered format,
  and the collection. `limit` caps the features as it caps CoverageJSON's
  top-level coverages (one per station); `numberReturned` is the feature
  count and `numberMatched` the count before `limit`. A MULTIPOINT
  position whose points past `limit` were never queried has no
  `numberMatched`. There are no paging links. A `datetime` list merges
  each station's series along `time` as it does for CoverageJSON, so it is
  still one feature per station, and `numberMatched` counts the merged
  features. A location list is one FeatureCollection too (see
  [Location lists](#location-lists)).

Each coverage is named by the one location of `get_locations()` at its exact
coordinates (the `serves_station_series` contract, pinned per engine by
`station_series_sit_at_listed_locations` in engine-csv and engine-bufr); a
`locations/{locId}` result by that id. A series no single location sits at
(two stations sharing a point, a ship that moved since the snapshot) has no
`id`, the label `POINT(lon lat)` and the `/locations` list as its endpoint. A
parameter named `datetime`, `label`, `parameter-name`, `edrqueryendpoint` or
`time` would collide with the feature's own members: GeoJSON of it is a 400
naming the alternatives. Bodies are byte-identical across identical requests,
so ETags revalidate.

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
| GRIB | Code Table 4.2 unit after display conversion (`°C`, `hPa`, `m s-1`, `mm`, `%`, …) | – (WMO triples are not mapped to CF); ✓ derived wind |
| QueryData | none: descriptors carry no trustworthy unit | – ; ✓ derived wind |
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

## Derived wind (#897)

A collection that publishes wind only as u/v components serves speed and
direction derived from them (`ds_core::wind`; the server wraps the engine in
`DerivedWind`). They are in `parameter_names` and work like native
parameters in every query type, `parameter-name` (case-insensitive) and
`f=png`. The components are queried with the engine's own sampling and
combined value by value, never interpolated afterwards: speed `hypot(u, v)`,
direction `atan2(-u, -v)` in degrees, where the wind blows **from**, 0° =
north. Calm (`u = v = 0`) has no direction; a missing component gives a
missing value. Units: the components' for speed, `°` (QUDT `DEG`) for
direction; `observedProperty.id` is the NERC URI of `wind_speed` /
`wind_from_direction`. Components a request did not name are not returned.
Without `parameter-name` every parameter is returned, the derived ones on
top of the components. The derived values count against the same response
budget as the engine's (1 million values, `MAX_AREA_VALUES`), counted before
any is computed: a response that would exceed it is a 400 "Query would
return N values (… queried + … derived wind values)" naming the limit.

| Engine | Pairs | Derived names | Frame from | Derived |
|---|---|---|---|---|
| GRIB | ECMWF `10u`/`10v`, `100u`/`100v`, `u`/`v`; wgrib2 `UGRD`/`VGRD` | `10si`/`10wdir`, `100si`/`100wdir`, `ws`/`wdir`, `WIND`/`WDIR` | GRIB2 flag table 3.3 bit 5, from the header probe or a decode | speed and direction (lat/lon grids, where both frames coincide); speed only until a pair's headers are read |
| QueryData | FMI `WindUMS`/`WindVMS` (23/24), or vocabulary short names | `WindSpeedMS`/`WindDirection` (or the vocabulary's, e.g. `10si`/`10wdir`) | no flag: FMI newbase's convention, relative to the data's own grid (`NFmiFastQueryInfo::DoWindComponentFix`) | speed and direction on lat/lon areas; speed only on LCC, stereographic and rotated lat/lon |
| Zarr | CF `standard_name` `eastward_wind`/`northward_wind`, `x_wind`/`y_wind` | the store's vocabulary: `wind_u_10m` → `wind_speed_10m`/`wind_direction_10m`, `10m_u_component_of_wind` → `10m_wind_speed`/…, ECMWF `u10` → `si10`/`wdir10` | the standard name | speed and direction (lat/lon grids) |
| others | – | – | – | – |

A derived parameter is skipped when the collection already has that
quantity at that level (by name, GRIB 0/2/1 or 0/2/0, CF standard name, FMI
number) or its name is taken; speed and direction are independent.
`derive_wind = false` turns it off per collection. The server logs per pair
what was derived, or why not. `100wdir` is MeteoCore's name, not an ECMWF
short name: ECMWF has none for 100 m direction, so it follows `10wdir`.
Derived values are checked against native ones on Météo-France ARPEGE
analysis fields (`testdata/grib-arpege-wind`): speed within 0.012 m/s,
direction within 0.5° wherever the wind exceeds 1 m/s, over the whole
domain. Not yet: direction from grid-relative components on rotated or
projected grids (rotation by the convergence angle).

## Domain types produced

`PointSeries`, `Point` (events), `Grid` (with optional `t` and `z` axes),
`VerticalProfile`, `Section` (trajectory cross-sections, with the
`meteocore:beamCoverage` foreign member), `Trajectory` (along-path samples:
a composite `[t, x, y]` or `[t, x, y, z]` axis, optionally a single-valued
`z` axis). Everything validates against the CoverageJSON 1.0 schema.

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

| Engine | locations | position | area | radius | cube | trajectory | instances | items | Area semantics |
|---|---|---|---|---|---|---|---|---|---|
| CSV | ✓, `datetime` by observation rows | – | ✓ | ✓ | – | – | n/a | ✓ stations | stations whose point is inside the polygon (≤ 500) |
| GeoTIFF | – | ✓ | ✓ | ✓ | – | – | n/a | – | Grid over the polygon's bbox at native resolution, no 256-cell coarsening (≤ 1M values across timesteps → 400, checked before any read, #858), pixels outside the polygon masked; `t` axis when several steps; a file that cannot be read is a timestep of nulls, any other error fails the query |
| GRIB | – | ✓ | ✓ | ✓ | ✓ pressure/model views | ✓ | ✓ | – | Grid over the polygon's bbox at native resolution (≤ 1M values across levels/parameters), cells outside the polygon masked; antimeridian-crossing bboxes rejected (#667) |
| QueryData | – | ✓ | ✓ | ✓ | – | ✓ | ✓ | – | Grid over bbox at native resolution, ≤ 256 cells/axis, cells outside the polygon masked (vertex fallback for sub-cell shapes); polygon outside the extent → 404; `t` axis when several steps. Lat/lon, rotated, stereographic and LCC grids (including the tangent-cone MEPS grid, on the sphere its file declares); a projected grid's extent is its projected rectangle's edges, not its corners' lon/lat box |
| Zarr | – | ✓ | ✓ | ✓ | – | ✓ | ✓ | – | Grid over bbox at native resolution, ≤ 256 cells/axis, one subset retrieval per variable for the whole time span (each may read multiple chunks) (two across the antimeridian; cells within half a native cell of ±180° are not interpolated across the seam, #667), at most 8 variables per request, cells outside the polygon masked (vertex fallback for sub-cell shapes); polygon outside the extent → 404; `t` axis when several steps. Forecast stores (reference + lead axes) expose every run as an instance; `None` ⇒ latest |
| ODIM composite | – | ✓ | ✓ | ✓ | – | – | n/a | – | Grid over bbox, ≤ 256 cells/axis, masked to the polygon; `t` axis when several steps |
| ODIM PVOL site | ✓, `datetime` by volume times | ✓ | ✓ | ✓ | – | ✓ | n/a | – (the site inventory is the network's Features collection) | polar sampling; trajectory = RHI cross-section |
| PostGIS stations | ✓, `datetime` → 400 | ✓ | ✓ | ✓ | – | – | n/a | ✓ stations, no `datetime` | stations-only `location_source`: exact `ST_Within` in SQL; observations-derived: exact point-in-polygon on the cached station set |
| PostGIS events | – | – | ✓ | ✓ | – | – | n/a | – (events as features = #503) | events in the polygon (exact, in SQL) as a `Point` CoverageCollection |
| BUFR | ✓, `datetime` by reports | ✓ | ✓ | ✓ | – | – | n/a | ✓ stations | stations whose point is inside the polygon (exact, in memory; ≤ 10 001 stations, ≤ 500 000 values per response → 400); position = nearest station within `position_radius_km` (25 km) else 404; one `PointSeries` per station over the in-memory `retention` window; same semantics for the polled-directory and WIS2 (push) sources; units are the BUFR units mechanically converted for display (K → °C, Pa → hPa, kg m-2 → mm) like GRIB |
| Nowcast | – | – | ✓ (motion field) | ✓ | – | – | ✓ | ✓ tracked cells, each naming a radius query over the motion field | motion blocks over the polygon's bbox, blocks outside the polygon masked; reflectivity via EDR = #523 |
| Satellite | – | ✓ | ✓ | ✓ | – | – | n/a | – | GOES-R, Himawari-9 (ISatSS) and the GMGSI global mosaic, whose values are 8-bit display counts (unit `1`), not brightness temperatures; its grid wraps at 180°, so a position either side of the seam, or an area given west > east across it, reads the pixels there. Each product is a parameter on its own time axis: the time axis of a response is the union of the selected products' scans (null where a product has none), an instant snaps per product to its latest scan at or before it, and `parameter_names` carries each product's own `extent.temporal`. RGB composites (`[[satellite.composites]]`) are map layers, not EDR parameters: naming one in `parameter-name` → 400. Position = the pixel under the point per scan; behind the Earth or off the mosaic's 72°S–72°N → 404. Area = grid over the bbox at the nadir pixel size (≤ 256 cells/axis), sampled through a coarse projection grid, masked to the polygon; polygon outside the imagery → 404; `t` axis when several scans. A query may download at most 8 scans the cache evicted and decode at most 1024 image blocks (summed per product grid; a GOES-R block is a strip of 24 full-width rows, a GMGSI block a 793 × 1322 chunk) → 400 |
| CAP, GeoJSON | — no `EdrEngine` (Features/Maps only) — | | | | | | | | |

Radius, corridor, items and location lists have no engine-specific code:
radius is answered by every engine that answers area, items by every engine
that implements `FeatureEngine` for its EDR collection (the Features engine
matrix describes each one's features), a list of location ids by every
engine that answers locations, one `query_location` per id, and corridor
does not exist. Cube needs a vertical axis, so only the GRIB pressure and
model-level views offer it: Zarr exposes no vertical dimension yet (a
variable's extra axes are read at index 0), ODIM PVOL's axis is elevation
angle, not a grid level, and the other gridded engines have no levels.
Trajectory is along-path sampling on GRIB, QueryData and Zarr (see
[Trajectory](#trajectory-926)) and a vertical cross-section on PVOL sites;
GeoTIFF, ODIM composite, Satellite, Nowcast and the station engines do not
answer it (404).

### Trajectory (#926)

The gridded engines share one implementation, `ds_core::trajectory`: WKT
parsing, path densification, the time and level rules and the CoverageJSON
`Trajectory` layout. Each engine only selects its run, timesteps, levels and
parameters exactly as its position query does, and samples the planned
fields with the position query's interpolation (bilinear; GRIB also applies
its display unit conversion).

- **Geometry.** `LINESTRING`, `LINESTRING Z`, `LINESTRING M` and
  `LINESTRING ZM`, case-insensitive, the dimension apart (ISO) or attached
  (`LINESTRINGZM`, as in the EDR examples); `lon lat [z] [m]` per vertex in
  CRS84. The whole path is validated before dispatch; a malformed or
  wrong-arity vertex, a coordinate out of range or all-identical vertices
  are 400. `MULTILINESTRING` is not supported (EDR makes it optional per
  collection).
- **Densification.** Segments follow the short great circle (as the radar
  cross-section does), so a path from 170° to −170° crosses the
  antimeridian. Each segment gets about one sample per source grid cell it
  crosses, the vertices kept exactly; at most 2000 samples → 400. Antipodal
  segment ends define no single path → 400.
- **M (time).** Seconds since the Unix epoch, OGC API - EDR 1.2's trajectory
  query-type definition. A sample's time is interpolated along its segment
  and takes the nearest timestep of the selected run, the earlier on a tie;
  the domain reports that timestep. A vertex time outside the run's time
  range is a 400 (EDR 1.2 abstract test `/conf/trajectory/
  coords-param-invalid-time`). The path's time window selects the run like
  a `datetime` window does. `datetime` (a list too) together with M/ZM →
  400.
- **Z (level).** In the collection's vertical coordinate (`extent.vertical`,
  hPa or model level on GRIB level views), interpolated along the segment
  and snapped to the nearest advertised level, which the domain reports. A
  vertex level outside the advertised range is a 400 (`/conf/trajectory/
  coords-param-invalid-linestringz`); a collection without a vertical extent
  ignores Z (`/req/edr/z-response` A) — QueryData and Zarr today. `z`
  together with Z/ZM → 400, also where the collection would ignore `z`.
- **2-D and Z paths** use the `datetime` selection of a position query (all
  steps of the latest run when omitted; a `datetime` list queries each
  instant and lists the answers): one `Trajectory` coverage per timestep. **2-D and M paths** on a collection with a vertical extent are
  sampled on the `z` levels (all levels when omitted, `/req/edr/z-response`
  F): one coverage per level, each with a single-valued `z` axis. A single
  coverage is a bare `Coverage`, several a `CoverageCollection`.
- **Output.** A composite `[t, x, y]` axis, or `[t, x, y, z]` for Z/ZM
  paths, and ranges of shape `[samples]` over axis `composite`. Composite
  axis values must be unique, so a sample repeating an earlier node (a
  closed loop back to its start) is dropped: it would read the same field at
  the same place. CoverageJSON only: `f=PNG` and `f=GeoJSON` are 400s, and
  `Accept: image/png` gets CoverageJSON, without `Vary`; the whole response is
  capped at 250 000 nodes (coverages × samples) and 1 000 000 values
  (× parameters) → 400.
  A path entirely outside the collection's extent is a 404; samples off the
  grid are null.
- **Reads.** GRIB fetches and decodes each planned field (parameter × step ×
  level) once, sampling every point that reads it, on the position query's
  four-worker scheduler; a missing field is null. Zarr splits the path into
  segments whose read window is at most 256 × 256 native cells per step and
  1M values across its step span, and reads each segment once per variable
  (at most 64 reads → 400; the antimeridian starts a new segment; the store's
  own longitude frame only, #667). QueryData samples its memory-mapped run,
  projecting each sample once.
- Not yet: `/instances/{id}/trajectory` (#924), `corridor` (#927), and
  along-path sampling on GeoTIFF, ODIM composite and Satellite.

Output formats: every query is CoverageJSON. EDR GeoJSON is served by CSV,
PostGIS stations and BUFR for the point queries they support (CSV:
locations, radius; PostGIS stations and BUFR: locations, position, radius);
no other engine serves it ([GeoJSON output](#geojson-output)). PNG is
position/locations/trajectory only. Area and cube are CoverageJSON only on
every engine.

Compatible nowcast reloads retain motion-field instances alongside forecast
runs and cell history (#604). Reuse requires unchanged nowcast config, the
same raster source engine and a compatible retained geometry/product contract;
source/tuning changes still rebuild. Auxiliary source edits affect subsequent
generations, not already-published instances.

## Known gaps, in suggested order

1. `locations` and `trajectory` under `/instances/{id}/`.
2. `corridor` (derivable from trajectory); cube on Zarr once it exposes
   vertical levels; `resolution-x`/`-y` on area.
3. `crs` on data queries, adding its CRSs to `crs_details` (#84); EDR GeoJSON for `area` on station collections and for PostGIS events.
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
list or recurring interval keeps the run's levels it names, like an interval,
and is a 400 only when it names none. Area/radius queries return a `[z,y,x]` Grid at one
forecast step; the 1M-value budget includes every selected level and parameter.
A missing field at an available level is null; a `z` naming no available level is 400.
Levels are exact discrete coordinates, not interpolated. Model levels are not
converted to geometric heights. Soil-depth/isentropic axes and fractional index
level values remain unsupported by this split.

GRIB cube queries (#925) answer on the pressure and model-level views. The
time axis follows area and position: no `datetime` is the run's last step,
an instant the nearest step, and an interval every step of the run inside
it, the run being the latest that covers the interval start, else the
latest with any step inside it (so `../end` works). A pinned instance never
falls back. A datetime list is one cube per instant (the nearest step each),
joined along `t`; instants snapping to the same step give it once. Without `parameter-name` every parameter of the view is
returned; `z` levels are exact, as for area. The response budget is checked
before any read when `resolution-x` and `-y` are both given, else after the
first field supplies the grid geometry and before the output is allocated
or any other field fetched. Fields are read with the area query's
four-worker limit, and a level no `resolution-z` position samples is not
read. Resampling reads the decoded grid in place, never allocating the
native subset. A 360° grid wraps: with `bbox=-180,…,180,…` the 180° column
samples the −180° meridian. A bbox crossing the antimeridian (`west > east`)
is a 400, as for area (#667); a bbox off the grid is a 404.

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
