# Shared OGC API Common HTTP layer

EDR, Maps, Tiles and Features use this crate for collection discovery. Pure search
and extent policy remain in `ds-core`; this crate owns Axum/JSON plumbing and
the shared HTML workbench.
API adapters keep their engine registries, access links, extent representations
and API-specific fields.

| Shared component | Responsibility |
|---|---|
| `CollectionRequest` | Decode controls, reject unsupported/duplicate names, validate values, negotiate representation before consulting engines |
| `collections_response` | Sort by ID, filter before paging, counts, JSON/HTML, explicit-format self/next/prev and alternate links |
| `collection_metadata`, `collection_card` | Common descriptive fields, keywords, license and HTML alternate links; API fields may override defaults for EDR instances |
| `collection_operation` | OpenAPI discovery operation, supported parameters, JSON/HTML responses and structured 400 errors |
| `CONFORMANCE_CLASSES` | One Common class inventory, combined with each API's own declarations |

The supported parameter names come from `ds_core::collection_search::CollectionParameter`;
the same inventory drives pair validation and OpenAPI generation.

| Control | Behavior |
|---|---|
| `bbox` | Four/six comma-separated coordinates, horizontal intersection; antimeridian supported, vertical bounds ignored |
| `bbox-crs` | CRS84 only, including the existing URI/CURIE/short aliases |
| `datetime` | RFC 3339 instant or interval overlap; unknown temporal extents remain eligible |
| `q` | Case-insensitive title/description/keyword search; comma-separated OR; whole words and whitespace-normalized phrases |
| `query` | The same text matching plus required (`+`) and excluded (`-`) terms/phrases within each OR alternative |
| `limit` | Default/max 1,000; values above max clamp; invalid values and zero return 400 |
| `offset` | Nonnegative number of matching collections to skip; default zero |
| `f` | `json` or `html`, overrides Accept; navigation retains the selected format |

Unsupported controls (`sd`, `resolution`, `sortby`, CQL2 and hierarchy
controls, for example) and repeated controls return JSON `{code, description}`
with HTTP 400. This applies to collection lists; it does not change the parameter
contracts on feature items, maps, tiles, EDR data queries or instance lists.

## Text search (draft 25-046 §7.6–7.7)

`q` and `query` search title, description and each keyword. Commas separate OR
alternatives. A phrase must occur in order within one property or keyword;
whitespace is normalized, punctuation is literal, and phrase edges must fall on
word boundaries. For example, `q=sea surface` matches `Sea surface` with any intervening whitespace, but not
`undersea surface`, `sea-surface`, or a title ending in `sea` with a description
starting in `surface`. Matching uses Unicode lowercase and alphanumeric word
boundaries; the draft does not define a language-specific tokenizer.

`query` adds `+` requirements and `-` exclusions at the beginning of an alternative
or after whitespace. These operators bind more tightly than commas. Separate
terms can match different properties/keywords; each phrase still stays within one.
Internal signs remain literal: `united-states` and `radar+hail` are single terms.

| Decoded `query` value | Meaning |
|---|---|
| `canada +weather -extreme` | Both Canada and weather, anywhere in the searched fields; no extreme |
| `canada +extreme weather` | Canada plus the phrase extreme weather |
| `canada -extreme,united states` | Canada without extreme, OR the phrase United States (the exclusion is local to the first alternative) |
| `-extreme` | Every collection whose searched fields lack extreme |

Empty alternatives and operators without an immediately attached term return 400.
The draft leaves malformed-expression handling unspecified; these are explicit
validation choices. A negative-only alternative is interpreted as a complement.
`q` and `query`, when both supplied, are ANDed, along with bbox/datetime filters,
before paging. `q` does not interpret operators; existing empty-`q` behavior
(no text restriction) is retained. No quoting, escaping, ranking or CQL2 syntax
is introduced.

In URLs, encode a required-term `+` as `%2B`; an unescaped `+` is decoded as a
space by form query parsing. For example:

```text
/edr/collections?query=canada%20%2Bweather%20-extreme&limit=10
```

Self/next/prev and JSON/HTML alternate links retain the query with its operators
percent-encoded. Clients can also use `curl --get --data-urlencode
'query=canada +weather -extreme'` to construct the request.

## Extents and conformance

Features supplies feature temporal bounds; Tiles uses raster times when present
and feature temporal bounds as a fallback. Metadata and filtering use the same
precedence. Bounds alone are not advertised as a sampled time grid.

The Common Part 4 Searchable Collections URI is intentionally not advertised:
[draft 25-046](https://docs.ogc.org/DRAFTS/25-046.html), retrieved 2026-09-17,
also requires `sd` and `resolution`, which remain unimplemented. Text search
is the first increment of [#742](https://github.com/mrauhala/meteocore/issues/742). The
[Parts 1–4 matrix](../../docs/ogc-api-common-matrix.md) records the exact baselines
and other limitations; retained declarations are not a certification claim.

Cross-API contracts live in
[server/tests/common_discovery.rs](../server/tests/common_discovery.rs):

```sh
cargo test -p server --test common_discovery
cargo test -p ds-core -p api-edr -p api-features -p api-maps -p api-tiles
```

## HTML API workbench

`workbench` renders the shared server/API landing pages, conformance, collection
lists and full collection metadata for EDR, Maps, Tiles and Features. EDR model
runs and Features item pages use the same shell. The JSON link and copyable URL /
cURL always represent the current resource with its applied filters and paging;
unsubmitted edits appear separately in the request preview.

The collection builder derives its fields from `CollectionParameter::ALL`.
Search and paging use ordinary GET requests. Optional advanced controls omit
empty values; literal `+` operators are form-encoded. Unsupported Common sorting,
`sd`, `resolution` and hierarchy controls are not shown. Adding them requires the
shared validator, metadata and OpenAPI contract to support them first.

Light/dark/system themes persist in the browser. Body text and controls are
16px; supporting text and code are at least 14px. Navigation, metadata, JSON links
and paging work without JavaScript; basic collection text search also works.
JavaScript enables optional query fields, clipboard buttons, view switching,
property search and return links to the last filtered list. API validation errors
retain the existing structured JSON error response.

No engine queries run from the renderer. Metadata comes from the existing JSON
builders. The workbench does not add HTML representations to map images, tiles,
EDR data-query responses, or the WMS/3D Tiles viewers.

The HTML structure and theme styles follow the approved #744 mockup: API workspace
selector and branded sidebar, grouped collection query controls, removable applied
filters, metadata-rich result rows and cards, and collection overview/metadata tabs.
Overview maps show the advertised extent over bundled Natural Earth outlines; the
asset is served locally by the existing preview asset handler. The map and item
quick-look implementation are shared with Features. No preview-only snapshot,
future-control or loading-state demonstrations are exposed in the live UI.

The former page builders in `ds_core::html` have been removed. That module retains
content negotiation, escaping and view types; `workbench` owns page rendering.

The overview separates collection discovery (Find collections) from data access
(Request data after selection). Catalog filters search metadata/coverage; they
are not forwarded as filters on data. The overview has one discovery action.

URL and cURL copy controls live in the shared request bar; catalog headings do
not duplicate them.

Maps collection overviews render the advertised map endpoint over the locator.
The browser requests the visible CRS84 bbox as a Web Mercator PNG (at most
1024 × 768), with advertised style links and an optional datetime instant. Pan/zoom
requests are debounced and superseded fetches are cancelled. Loading/errors are
explicit; failed requests hide the previous image. The rendered-image URL is
separate from the collection metadata JSON link. No rendering occurs while
assembling metadata on the server.

Map refreshes retain the displayed image until its replacement has loaded in
MapLibre, then swap the layers. Status, image-link and request-URL areas keep
fixed dimensions to avoid shifting content during pan/zoom. Map controls use
equal-height fields and an aligned action button, stacking on narrow screens.

Breadcrumb labels use available collection/item titles while preserving resource
IDs in URLs, including parent collections on item and model-run pages. Maps time
controls expand an advertised regular grid (`cellsCount` + ISO duration) or use
its explicit irregular coordinates, with previous/next available-time buttons.
An interval alone does not imply sample availability; absent/oversized grids use
a native UTC date/time input (expansion is bounded at 10,000 choices). Advertised
style legends load with the displayed image; unchanged legends stay visible on
pan/zoom. Full nested metadata and license links remain in Metadata & links,
with configured licenses and searchable keywords also in the overview. Missing
storage CRS is labelled "Not advertised", never guessed from the coverage CRS.

Collection advanced search starts collapsed and remembers its disclosure state
per API path for the browser session, including after search submission. Applied
filters remain visible while collapsed. Resource titles and their URLs form one
clickable link; map endpoints retain their required-bbox hint. Country boundaries
render above map imagery with a contrasting halo, including after pan/zoom.

Catalogs use a full-width search panel and result summaries. The applied request
and draft search URL are separate disclosures; the draft opens on the first edit.
The JSON switch always retains the applied query. List/cards preference persists
per API path in the browser session. Empty pages beyond the match count explain
the offset and link to the first page with all filters retained; they do not claim
that no collections match. Unaligned offsets show a range rather than a misleading
page number. Summaries retain UTC clock precision (including seconds), show known
bounds and parameter/style names, and display sampling resolution only when
advertised. No extra engine queries or inferred sample cadence are introduced.
