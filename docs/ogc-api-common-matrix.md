# OGC API Common implementation matrix

MeteoCore is also a platform for experimenting with OGC drafts and informing
standards development. This matrix records observable capabilities and gaps
across its four OGC API surfaces, including draft provisions that are not yet
implemented. It is an implementation assessment, not an OGC certification or a
claim that every requirement in a class has passed its abstract test suite.

Implementation assessment updated **2026-09-17** by the text-search increment of
[#742](https://github.com/mrauhala/meteocore/issues/742), based on main
[`adbacde`](https://github.com/mrauhala/meteocore/tree/adbacde87d28c977f104cfc911f3b3a71ed27592)
plus that change. Shared discovery shipped in #741 / #739. The capability tables
describe this source revision. The production probe table below remains
**historical pre-#739 evidence**, not verification of deployment of these changes.

**Specification baselines**

| Part | Baseline used | Status of this baseline |
|---|---|---|
| 1 — Core | [19-072, version 1.0.0](https://docs.ogc.org/is/19-072/19-072.html) | Approved |
| 2 — Geospatial Data | [20-024](https://docs.ogc.org/DRAFTS/20-024.html), retrieved 2026-09-17 | Draft |
| 3 — Schemas | [23-058r2, version 1.0.0](https://docs.ogc.org/is/23-058r2/23-058r2.html), also Features Part 5 | Approved 2026-04-30 |
| 4 — Discovery within many collections | [25-046](https://docs.ogc.org/DRAFTS/25-046.html), retrieved 2026-09-17 | Draft |

The [Common repository](https://github.com/opengeospatial/ogcapi-common/tree/3828187a8fc5e98386743afcdd0c0d650855956f)
was at `3828187` when assessed. Its README still links Part 3 to draft 23-058r1;
this matrix uses the approved successor linked by the
[OGC standards catalogue](https://www.ogc.org/standards/ogcapi-common/).
The rendered drafts can change independently of these links; retrieval
fingerprints are recorded below.

**Legend:** **Yes** = the named capability is implemented; **Partial** = a
known limitation is described; **No** = not implemented; **N/A** = not applicable
to the indicated data access model; **Unverified** = insufficient evidence for
a conformance conclusion. A **Yes** on one row does not establish conformance
for its entire requirements class. Optional metadata and recommendations are
included to support experimentation; a **No** is not automatically a violation.

Paths below are relative to `/edr`, `/maps`, `/tiles` or `/features` respectively.
Part 4 rows concern **collection discovery**, not querying the contents of a
collection.

| OGC API Common feature | EDR | Maps | Tiles | Features |
|---|---|---|---|---|
| **Part 1 — Core** | | | | |
| Landing page with API, documentation, conformance and data links (§9) | Yes | Yes | Yes | Yes |
| `/conformance` resource (§9.3) | Yes | Yes | Yes | Yes |
| Machine-readable API definition (§9.2, §11) | Yes — `/api`, OpenAPI 3.0.3 | Yes — same | Yes — same | Yes — same |
| Human-readable API documentation (§9.2) | Yes — `/api/docs` | Yes — same | Yes — same | Yes — same |
| JSON metadata representations (§10.3) | Yes | Yes | Yes | Yes |
| HTML metadata representations (§10.2) | Yes | Yes | Yes | Yes |
| Representation negotiation (§8.7) | Partial [1] | Partial [1] | Partial [1] | Partial [1] |
| Invalid supported collection parameter handling (§8.3.2) | Yes — e.g. `limit=0` → 400 | Yes — same | Yes — same | Yes — same |
| Unknown collection parameter handling (§8.3.1) | Rejected — 400 [2] | Rejected — 400 [2] | Rejected — 400 [2] | Rejected — 400 [2] |
| HTTP caching of Common metadata (§8.4, recommendation) | Yes — Cache-Control, ETag, 304 | No metadata cache policy [3] | No metadata cache policy [3] | Yes — Cache-Control, ETag, 304 |
| Cross-origin access (§8.5, recommendation) | Yes — server CORS | Yes — same | Yes — same | Yes — same |
| UTF-8 metadata (§8.6) | Yes | Yes | Yes | Yes |
| Complete Part 1 abstract-test-suite result | Unverified | Unverified | Unverified | Unverified |
| **Part 2 — Geospatial Data** | | | | |
| Collection listing `/collections` (§7.1) | Yes | Yes | Yes | Yes |
| Collection description `/collections/{id}` (§7.2) | Yes | Yes | Yes | Yes |
| Collection identifier, title and description | Yes | Yes | Yes | Yes |
| Collection keywords | Yes — configuration | Yes — configuration | Yes — configuration | Yes — configuration |
| License links | Yes — when configured with a resolvable URL | Yes — same | Yes — same | Yes — same |
| `attribution` / `attributionMediaType` | No | No | No | No |
| `itemType` for individually accessible items | Omitted; EDR locations require applicability review | N/A — map access | N/A — tile access | Yes — `feature` |
| Links to data access mechanisms | Yes — EDR `data_queries` | Yes — map/styles, tiles when enabled | Yes — tilesets | Yes — items, vector tiles when enabled |
| `self` links on collection list/detail | Yes | Yes | Yes | Yes |
| `alternate` links for every supported representation (§7.1.2, §7.2.2.3) | Yes [4] | Yes [4] | Yes [4] | Yes [4] |
| Spatial extent in CRS84 | Yes — engine-dependent | Yes — engine-dependent | Yes — raster or feature extent | Yes — engine-dependent |
| Temporal extent metadata | Yes — engine-dependent | Yes — raster times | Yes — raster or feature extent [5] | Yes — time-aware engines [5] |
| Multiple spatial boxes / temporal intervals for sparse data | No — one overall extent | No — one overall extent | No — one overall extent | No — one overall extent |
| Supported output CRS metadata (`crs`) | Yes — CRS84 | Yes — supported map CRSs | Yes — tile matrix set CRSs and native CRS where known | Yes — CRS84 |
| Native CRS metadata (`storageCrs`) | No | Partial — known CRS URI only | Partial — known raster CRS URI only | Yes — CRS84 |
| Native spatial bounds (`storageCrsBbox`) | No | No [6] | No [6] | N/A — storage and extent both CRS84 |
| Scale/cell-size suitability metadata (`minScaleDenominator`, `maxScaleDenominator`, `minCellSize`, `maxCellSize`) | No | No | No | No |
| Vertical extent information | Partial — EDR-specific representation [7] | Partial — levels/unit/grid [7] | Partial — raster levels/unit/grid [7] | No |
| Uniform Additional Dimensions class (§8) | No — EDR extent model [7] | Partial — no full class support [7] | Partial — no full class support [7] | No |
| Regular spatial grid description (§8.2) | No Common grid descriptor | Partial — geographic grids only | Partial — geographic raster grids only | N/A — feature data |
| Regular/irregular temporal grid description (§8.2) | No — EDR `temporal.values` instead | Yes — available raster time series | Partial — raster only | No — intervals only [5] |
| Additional axes such as ensemble/reference-time using the uniform extent model | No | No | No | No |
| Collection JSON and HTML encodings (§9) | Yes | Yes | Yes | Yes |
| Complete current Part 2 abstract-test-suite result | Unverified | Unverified; native-bounds gap remains | Unverified; native-bounds gap remains | Unverified |
| **Part 3 — Schemas** | | | | |
| Logical schemas using the Part 3 JSON Schema vocabulary (§7) | No | No | No | No |
| Schema annotations: roles, ordering, units, semantic definitions, special nulls (§7.2) | No | No | No | No |
| Advanced property roles (§8) | No | No | No | No |
| Reference descriptions (§9) | No | No | No | No |
| Returnables schema: `/collections/{id}/schema` (§10) | No | No | No | No |
| Receivables schema for writes (§10) | N/A — read-only | N/A — read-only | N/A — read-only | N/A — read-only |
| Queryables schema resource (§11) | No | No | No | No [8] |
| Sortables schema resource (§12) | No | No | No | No [8] |
| `profile` query parameter and profile negotiation (§13) | No | No | No | No |
| Profiles for references (§14) | No | No | No | No |
| Profiles for codelists (§15) | No | No | No | No |
| Profiles for value domains (§16) | No | No | No | No |
| **Part 4 — Discovery within many collections** | | | | |
| Collection `bbox` intersection (§7.3) | Yes — horizontal extent [9] | Yes — same [9] | Yes — same [9] | Yes — same [9] |
| Collection `bbox-crs` (§7.4) | Partial — CRS84 only | Partial — CRS84 only | Partial — CRS84 only | Partial — CRS84 only |
| Collection `datetime`: instant, interval, open ends (§7.5) | Yes — engine extent [5] | Yes — raster extent [5] | Yes — raster or feature extent [5] | Yes — feature extent [5] |
| Collection `q`: case-insensitive terms/phrases in title, description, keywords (§7.6) | Yes [10] | Yes [10] | Yes [10] | Yes [10] |
| Collection `query`: required/excluded terms (§7.7) | Yes [10] | Yes [10] | Yes [10] | Yes [10] |
| Collection page size `limit` (§7.8) | Yes — default/max 1,000 | Yes — same | Yes — same | Yes — same |
| Collection `next` paging links (§7.8) | Yes | Yes | Yes | Yes |
| Collection `prev` paging links (§7.8, optional) | Yes | Yes | Yes | Yes |
| `offset` paging mechanism (MeteoCore extension) | Yes | Yes | Yes | Yes |
| `numberMatched` and `numberReturned` (§7.8) | Yes | Yes | Yes | Yes |
| Combined filters before paging; links preserve supported filters | Yes | Yes | Yes | Yes |
| Scale denominator filter `sd` (§7.9) | No — 400 | No — 400 | No — 400 | No — 400 |
| Cell-size filter `resolution` (§7.9) | No — 400 | No — 400 | No — 400 | No — 400 |
| Client-selected collection sorting `sortby` (§8) | No — fixed ID order | No — fixed ID order | No — fixed ID order | No — fixed ID order |
| Collection-list sortables discovery (§8) | No | No | No | No |
| CQL2 collection filtering (§9) | No | No | No | No |
| Collection-list queryables discovery (§9) | No | No | No | No |
| Hierarchy `parent` metadata and navigation (§10) | No | No | No | No |
| Hierarchy filtering `parent` (§10) | No — 400 | No — 400 | No — 400 | No — 400 |
| Hierarchy depth selection `descendants=immediate/all` (§10) | No — 400 | No — 400 | No — 400 | No — 400 |
| Complete current Searchable Collections class | No — not declared [11] | No — same [11] | No — same [11] | No — same [11] |

**Limitations and interpretation**

1. Metadata routes share `f=json/html` and Accept negotiation, but do not fully
   rank competing media types by quality values. Features item routes separately
   accept media-type aliases; that does not extend every Common metadata route.
2. The shared extractor rejects unsupported and duplicate collection parameters
   with a JSON `BadRequest` response. Part 1 §8.3 permits some tolerance, but the
   project's stricter policy avoids misleading draft-testing clients. Unsupported
   controls such as `sd`, `parent` and `sortby` now return 400 rather than
   silently serving unfiltered results. `offset` and `f` remain supported extensions.
3. Maps and Tiles cache rendered data responses; their Common metadata handlers
   lack the EDR/Features metadata caching middleware. This row is about metadata.
4. Shared collection metadata now adds HTML alternate links to JSON list/detail
   responses. HTML pages link to JSON. List self/next/prev links explicitly retain
   the negotiated format and all supported filters, even when the original client
   selected HTML using only an Accept header.
5. Features and vector-only Tiles now supply feature temporal extents to both
   metadata and collection search. Tiles prefers available raster times and falls
   back to feature temporal bounds. Unknown extents remain eligible under the
   draft. Feature interval endpoints are no longer advertised as a two-sample grid;
   actual raster time series retain their regular/irregular grid descriptions.
6. The current Part 2 text requires native spatial bounds when storage and extent
   CRSs differ, while also retaining a related SHOULD recommendation. Projected
   Maps/Tiles metadata can advertise `storageCrs` without `storageCrsBbox`.
   This deserves both an implementation fix and a standards wording review.
7. EDR intentionally retains its own string-valued vertical and timestep metadata
   for EDR 1.1. Maps/Tiles emit numeric vertical intervals, units and coordinates,
   but lack a semantic `definition`/`vrs` and vertical grid `cellsCount` required
   by the current uniform-dimensions class. Geographic spatial and raster temporal
   grid descriptors exist, but projected spatial grids and general additional
   axes do not. No API advertises the uniform-dimensions class.
8. Features item property filters and `sortby` work on supporting engines. Neither
   provides Part 3 `/queryables` or `/sortables` resources. OpenAPI schemas and
   EDR `parameter_names` likewise do not establish Part 3 support.
9. Four- and six-number boxes are accepted; six-number input discards the vertical
   axis. Antimeridian crossing is supported. No vertical filtering is implemented.
10. `q` and `query` share whole-word/phrase matching across title, description
    and individual keywords, with normalized whitespace and literal punctuation.
    Phrases never span properties. `query` adds required/excluded terms, which may
    match different properties, inside comma-separated OR alternatives. Operators
    apply only at the start or after whitespace; internal signs are literal.
    `q` and `query` combine with AND. Matching uses Unicode lowercase and
    alphanumeric boundaries; language-specific tokenization is unspecified by the
    draft. Negative-only alternatives are complements. Empty query alternatives
    and dangling operators return 400 (an implementation validation choice).
    URL `+` operators require `%2B`; navigation preserves them. See the
    [text-search contract and examples](../crates/api-common/README.md).
11. The published Part 4 draft requires `sd` and `resolution` in addition to
    the implemented text and extent filters. Those remaining gaps and the final
    class assessment are tracked in [#742](https://github.com/mrauhala/meteocore/issues/742);
    Searchable Collections remains undeclared. Sorting, CQL2 and hierarchy are
    separate classes; their absence does not itself invalidate Searchable Collections.

**Advertised conformance versus assessed behavior**

All four APIs currently return the same Common declarations:

| Common declaration | EDR | Maps | Tiles | Features |
|---|---|---|---|---|
| Part 1 `core`, `landing-page`, `oas30` | Declared | Declared | Declared | Declared |
| Part 1 `json`, `html` | Not declared | Not declared | Not declared | Not declared |
| Part 2 `collections`, `json`, `html` | Declared | Declared | Declared | Declared |
| Part 2 `uad-collections` | Not declared | Not declared | Not declared | Not declared |
| Part 3 classes | Not declared | Not declared | Not declared | Not declared |
| Part 4 `searchable-collections` | Not declared; gaps above | Not declared; gaps above | Not declared; gaps above | Not declared; gaps above |
| Part 4 sorting, filtering, hierarchy | Not declared | Not declared | Not declared | Not declared |

The emitted URIs use `http://www.opengis.net/spec/ogcapi-common-{part}/1.0/conf/…`.
Current draft documents use HTTPS in some identifiers; exact URI expectations
should be recorded when running their conformance tests. JSON/HTML support and
declarations in other API specifications do not substitute automatically for
Common Part 1 class declarations.

**Evidence and reproducible checks**

Shared HTTP validation, responses, Common metadata fields, navigation, declarations
and OpenAPI live in [api-common](../crates/api-common/README.md). Pure policy lives
in [collection_search.rs](../crates/core/src/collection_search.rs),
[html.rs](../crates/core/src/html.rs),
[ogc_extent.rs](../crates/core/src/ogc_extent.rs), and
[datetime.rs](../crates/core/src/datetime.rs). HTTP adapters, metadata and
conformance declarations live in the
[EDR](../crates/api-edr/src/handlers.rs),
[Maps](../crates/api-maps/src/handlers.rs),
[Tiles](../crates/api-tiles/src/handlers.rs), and
[Features](../crates/api-features/src/handlers.rs) handlers.
Their adjacent `lib.rs` routers establish the missing schema routes.

**Historical production probes, before the #739 changes**, on
`https://meteocore.app.meteo.fi` returned:

| Probe / observation | EDR | Maps | Tiles | Features |
|---|---|---|---|---|
| `/collections?q=radar&limit=2&offset=2` | 200; 2/42 | 200; 2/41 | 200; 2/42 | 200; 2/10 |
| Same page has both `next` and `prev` | Yes | Yes | Yes | Yes |
| `/collections?limit=0` | 400 | 400 | 400 | 400 |
| `/collections?query=zzzz_nonexistent_collection&limit=1` | 200; 52 matched | 200; 50 matched | 200; 54 matched | 200; 16 matched |
| `/collections?parent=zzzz&descendants=immediate&limit=1` | 200; 52 matched | 200; 50 matched | 200; 54 matched | 200; 16 matched |
| `/collections?datetime=1900-01-01T00%3A00%3A00Z&limit=1` | 1 matched | 1 matched | 4 matched | All 16 matched |
| `/collections?limit=1`, `Accept: text/html` | 200 HTML | 200 HTML | 200 HTML | 200 HTML |
| JSON list/detail link to HTML | No | No | No | No |
| `/api` advertises schema/queryables/sortables routes | No | No | No | No |

In the paging row, `2/42` means two returned out of 42 matching collections.
Counts reflect that deployment's catalog at the time. The old-date query is not
expected to return zero when collections have unknown extents. API catalogs also
differ legitimately according to enabled engines and collection configuration.

For example, use these read-only requests, substituting each API prefix:

```sh
curl -fsS 'https://meteocore.app.meteo.fi/edr/conformance'
curl -fsS 'https://meteocore.app.meteo.fi/edr/collections?q=radar&limit=2&offset=2'
curl -fsS -H 'Accept: text/html' 'https://meteocore.app.meteo.fi/edr/collections?limit=1'
```

The [cross-API contract suite](../crates/server/tests/common_discovery.rs) now
exercises equivalent fixture catalogs through all four routers, including both
raster and vector-only Tiles. It follows actual navigation links, checks combined
filters/counts/temporal adapters, validates representation links and unknown or
duplicate parameter errors, exercises text operators and phrase matching, and
compares OpenAPI and Common declarations. This
verifies the updated source behavior without depending on production catalog data.

No full OGC abstract test suite was executed for this assessment. Existing unit
and integration tests and hand-written OpenAPI documents are useful evidence,
but do not establish conformance to subsequently changed drafts.

**Standards experimentation and maintenance**

Keep implementation gaps separate from proposed changes to a standard. Useful
experiments include the EDR/Common extent representation boundary, observable
handling of unsupported discovery parameters, hierarchy navigation across API
surfaces, and alignment between collection extent metadata and search results.
Record the exact requirement identifier, specification baseline, minimal request,
expected/actual response, and whether the finding is an implementation defect,
an ambiguous requirement, or a proposed capability.

When Common behavior changes, update the relevant cells, declarations, evidence
and assessment revision in the same PR. When a draft changes, review its normative
changes before changing any status; preserve a baseline comparison in the PR.
Use equivalent fixture catalogs for future cross-API contract tests, rather than
requiring the production APIs to expose identical collections.

Related implementation work: [#739](https://github.com/mrauhala/meteocore/issues/739)
(shared wiring and conformance audit),
[#296](https://github.com/mrauhala/meteocore/issues/296) (Part 2 metadata),
[#303](https://github.com/mrauhala/meteocore/issues/303) (search metadata),
[#301](https://github.com/mrauhala/meteocore/issues/301) (hierarchy),
[#302](https://github.com/mrauhala/meteocore/issues/302) (sorting/CQL2),
[#325](https://github.com/mrauhala/meteocore/issues/325) (attribution), and
[#686](https://github.com/mrauhala/meteocore/issues/686) (Features schemas/filtering).
Older issue descriptions may target superseded drafts.

Features item paging/sorting and EDR locations paging are outside this matrix.
EDR forecast-run instances and radar site naming are not implementations of
Common hierarchical collections.

**Retrieved-document SHA-256 fingerprints**

These identify the retrieved HTML bytes; generated-page changes may also change
the hashes. They do not imply that a rendered document was built from the recorded
Common repository commit.

```text
19-072:   6fe6439128d7c2041b2f08962f591381b8e0934bf71f6683caefea0dee865133
20-024:   0a31af834522c95e0f1e15985a558a461e027d0d09f3165c6ffa61b74cb4e7cc
23-058r2: 894eac40d35da34711fa83db901b127d4b11907ba2c25eb9bf9f8564b82a7b14
25-046:   aae2a94282682a400d17d78273861423e16baaf4faf4c23c70854e6b29372248
```
