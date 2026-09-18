# HTML API workbench implementation

Implements the shared workbench approved in [design PR #744](https://github.com/mrauhala/meteocore/pull/744),
on top of the Common text-search changes in #743.

Open `/?f=html` or any of `/edr/`, `/features/`, `/maps/`, `/tiles/` with
`?f=html`. A browser's `Accept: text/html` also selects HTML. Explicit `f=json`
selects the same resource as JSON. These are server-rendered API resources with
ordinary URLs, not a separate client-side application.

- Collection lists offer Common `q`, `query`, `bbox`, `datetime`, `limit` and
  `offset`. Existing `bbox-crs` values remain editable. Results and navigation
  come from the shared server discovery pipeline.
- Feature items expose the collection's supported property/sort/time controls,
  response counts, paging, typed properties and geometry. The map contains only
  the returned page over locally bundled Natural Earth outlines; overlapping shapes
  have a picker and quick-look panel.
- Applied parameters remain in the persistent JSON link and copyable request.
  Unsubmitted edits have a separate request preview. Browser enhancement restores
  the last filtered list when returning from a collection or item.
- Light/dark/system themes share a 16px body/control scale and 14px minimum for
  supporting text and code. Theme choice persists locally.
- The sidebar, typography, grouped search controls, chips and result rows use the
  approved mockup's HTML structure and styles. Collection views restore overview,
  request-data and metadata tabs; item details restore summary metrics and source
  quality callouts when the corresponding properties exist.
- Collection details retain full API metadata, keywords and license labels,
  including free-text licenses without a URL. EDR model-run pages share the shell.
- Common sorting, hierarchy, `sd` and `resolution` remain unsupported and have
  no active controls. Existing API validation errors remain structured JSON.
- Without JavaScript, metadata, navigation, paging, JSON switching and basic
  collection text search work. Advanced/item-query editing and map interaction
  require JavaScript.

See [shared implementation notes](../../../crates/api-common/README.md#html-api-workbench)
and the [Common capability matrix](../../ogc-api-common-matrix.md).

## Validation

- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test -p ds-core -p api-common -p api-edr -p api-features -p api-maps -p api-tiles`
- `cargo test -p server --test common_discovery`

The core and API suites passed 1,026 tests (four pre-existing ignored tests);
cross-API contracts passed seven. Contracts cover query/format preservation, full metadata,
license display, proxy prefixes, capability-driven controls, escaping, GeoJSON/HTML
negotiation and representation-specific caching.

Chrome checks used the real server with captured public storm-cell/warning/station
samples from 2026-09-17 plus repository CSV, GeoJSON and GeoTIFF fixtures. Verified
advanced search, empty-result recovery, list/cards, item paging, filtered return
links, property search, map selection and theme persistence. At 390px width,
collection and item pages had no horizontal document overflow. Measured text on
those pages had no contrast failures, a 14px minimum, and two font stacks; lowest
measured contrast was 5.97:1 in light and 7.36:1 in dark. These checks are not a
claim of a complete accessibility audit.

[Landing page, light](screenshots/landing-light.png) ·
[Collections, light](screenshots/collections-light.png) ·
[Collection overview, light](screenshots/collection-light.png) ·
[Feature list, light](screenshots/items-light.png) ·
[Feature properties, dark](screenshots/item-dark.png)

## Design comparison

The initial implementation simplified several approved layouts too far. The
updated implementation reuses the mockup's CSS and restores its page composition,
workspace selector, endpoint index, query groups, result details, collection
overviews, coverage locators and item inspection panels. The production adapters
keep actual server URLs and data. Preview-only state selectors, captured-data
banners, synthetic groups and unsupported future controls are excluded. Missing
metadata is shown as unavailable rather than replaced with the mockup's examples.

The geographic backdrop comes from public-domain Natural Earth 1:110m country
outlines. See [source and transformation details](../../../crates/server/preview/vendor/LICENSE-workbench-land.txt).
It is a general-purpose locator; all geometry and bounding values remain available
in the current resource's JSON.

## Discover collections, then request data

The overview has one **Find collections** action. The adjacent **Request data**
step explains what becomes available after choosing a collection; it does not
repeat the search action. Catalog controls are labelled **Collection search**.
Within a Features collection, **Request data** opens the **Data request builder**
and **Request features** retrieves matching items. EDR/Maps/Tiles explain their
own operations without implying a new universal data builder. Discovery and
data filters remain independent, and JSON always represents the current resource.

## Maps collection data preview

Maps collection overviews fetch actual rendered PNG data for the visible area
over the geographic backdrop. Pan/zoom updates the request; advertised styles
and a metadata-driven time selector can be applied with **Update map**. Default time uses
the collection default. The exact image URL is visible below the map and opens
through **Open rendered image**. The global JSON switch remains the collection
metadata representation. Loading failures and timeouts stay in the map panel.

[Maps collection with radar data](screenshots/map-data-light.png)

Validated the local GeoTIFF radar fixture: zoom changes the bbox/image request;
style changes replace the pixels; explicit time is retained in the image URL;
failed image requests display an error and hide stale imagery. Recovery,
light/dark themes and 390px layouts were checked in Chrome.

Map refreshes keep the current image visible until the replacement source is
ready, then swap it. The status, image link and URL occupy stable space so the
page below the map does not jump. Style/time fields and the update button have
equal heights and align at the bottom; narrow screens stack the controls.

Browser regression check: desktop and 390px page height and coverage-section
position stayed identical before, during and after map refresh. All three map
controls measure 46px high; desktop controls share the same bottom edge.

## Review with a global forecast collection

[GFS map, metadata and legend](screenshots/map-gfs-light.png)

The local instance now also enables `collections.d/noaa-gfs.toml` against NOAA's
public S3 bucket. Its advertised irregular axis includes hourly and three-hourly
steps; the selector uses those exact coordinates. Regular radar grids expand
from their advertised resolution and count. Previous/next buttons request the
adjacent time, and the default option leaves `datetime` absent. Without a usable
grid, the UTC date/time input remains available. The choice list is bounded.

The review found and corrected an example configuration mismatch: the GFS default
near-surface field is PRMSL (hPa), but the old default palette was temperature.
The default now uses the built-in 950–1050 hPa pressure palette; named temperature, pressure
and wind-gust styles explicitly bind their data parameter. NOAA license metadata
links to the [NWS use conditions](https://www.weather.gov/disclaimer).

The selected style's advertised legend appears beside the map. Its link opens
the machine-readable legend, including parameter and units. Map endpoint links are clickable and state the required `bbox`; the map
controls provide a complete image request. The duplicate resource list and unscoped global-preview button were
removed from the Maps request section. Breadcrumbs prefer collection/item titles.

All serialized metadata fields remain in Metadata & links (including arbitrary
nested extensions); license and keywords are also surfaced in the overview.
An absent storage CRS is explicitly unknown. Rich-metadata regression fixtures
cover license text/links, keywords, vertical values/units and nested false values.

Remaining limits: this is a map preview and image request builder, not the full
Maps parameter editor. Arbitrary `parameter-name`, elevation, output CRS/size and
model-run selection are not UI controls. Named styles can bind a parameter; the
API reference and exact rendered URL expose the rest. Time axes describe the
collection and do not guarantee every parameter exists at every step. Browser
requests do not select a historical forecast run. Revisit these controls when
adding a complete data-request workspace, rather than borrowing EDR-only metadata.

The GFS engine currently selects the first indexed level for an explicit TMP
parameter (0.01 hPa in the checked NOAA product), rather than guaranteeing 2 m
temperature. The temperature style is labelled **default level** for this reason.
GRIB vertical selection remains the existing #81 limitation; pressure and wind
gust provide unambiguous fields for this preview.

Validation after this review: 648 API tests and 7 cross-API discovery contracts
passed, as did full-workspace clippy and formatting. In Chrome, desktop controls
remained 46 px high and aligned; the page height (2115 px) and coverage position
(1516.36 px) stayed unchanged after a GFS zoom refresh. At 390 px there was no
horizontal page overflow. Light/dark themes and explicit time/style changes
were checked against the real S3-backed collection.

Legend initialization waits for the full document so a fast/cached map cannot
race sidebar parsing. Legend images revalidate when selected, avoiding old
palette images after a configuration reload; unchanged legends stay visible
through viewport refreshes.

## Navigation and map-overlay follow-up

Resource cards now link both the title and displayed URL as one clickable target.
Map links retain the bbox requirement hint. Country outlines use a contrasting
foreground stroke/halo above every raster replacement. Advanced collection search
starts closed and remembers the user's choice per API path for the session;
submitting a search does not reopen it or discard active advanced filters.

Browser verification: advanced search was closed on first load, stayed open after
an open-panel submission, and stayed closed after a close-and-submit. Resource
URLs were verified as anchors. Clicking Styles was blocked by the Chrome client
(`ERR_BLOCKED_BY_CLIENT`); its endpoint was checked separately over HTTP. GFS country
borders remained visible above the rendered image after zooming. The existing
648 API tests, seven cross-API contracts, clippy and HTTP smoke checks passed.

## Collections UX pass

Search now spans the content width above results. Catalog request tools are in a
native disclosure; the global JSON switch remains visible. The draft URL has a
separate disclosure, opening on the first edit without replacing the applied URL.
Search text and advanced expression retain their API names (`q` / `query`). Mobile
navigation combines the product name and API selector into one row.

Results prioritize UTC temporal coverage, advertised bounds, parameter names or
styles, and any advertised sampling resolution/count. The same-day radar window
keeps its actual clock times; date/time components wrap together. Generic
"Environmental data" / "Time-aware" badges are removed. List/cards choice persists
per API path for the session. Metadata comes from the existing response document.

An offset beyond the results now says that the page is outside the results and
offers **Go to first page**, preserving filters and limit. It does not display an
impossible range/page number or claim zero matches. A truly empty search retains
its separate zero-results message; unaligned offsets display a range.

Validation: 650 API tests and seven cross-API discovery contracts passed, along
with workspace clippy, formatting and real-server HTTP smoke checks. Chrome
confirmed that List/Cards survives a search, draft edits leave the applied JSON
URL unchanged, advanced search stays closed, and the first-page recovery link
retains the filters and page size. At 390 × 844, the first result begins at 676 px
(previously 1,335 px), with no horizontal page overflow; light and dark themes
were checked. Maps advertises the radar's five-minute sampling interval; EDR
only advertises its coverage and does not acquire an inferred cadence.

Catalog screenshots: [desktop](screenshots/collections-light.png),
[mobile](screenshots/collections-mobile.png).

## Generic item browser

Items use common label properties (`name`, `label`, `title`, `nimi`, etc.) with an
ID fallback. Domain-specific radar metrics, alert chips and the ambiguous
“Observed / sent” summary have been removed. Every source property remains
available with its JSON type; units are not guessed from field names.

Listing columns come from actual properties and advertised filter fields. The
first four non-null scalar fields other than labels are the default; users can
choose up to eight columns, remembered per collection for the session. Missing
properties and null values remain distinct. Selection is presentation state and
never changes the API URL. The table grows to show every response row, with
page scrolling vertically and top/bottom pagination. Wide tables support
horizontal scrolling. The map is alongside results on wide screens. Raw geometry is retained on details.

The map explicitly shows the current page and uses generic property facts. The
request builder, applied URL and draft URL are collapsible; applied filters remain
visible. Empty offsets offer first-page recovery without dropping filters.

Validation: 654 API tests passed (four existing ignored), seven cross-API
contracts passed, workspace clippy with warnings denied, formatting and JS syntax
checks passed. Chrome checks covered station, municipality and warning samples,
column persistence through paging/search, draft versus applied requests, typed
property search and filter-preserving first-page recovery. Builder/advanced
disclosure states survive submission. Primary query controls align at 46 px.

The listing has no per-feature raw-geometry disclosures. Following layout review,
the result table grows to the requested limit and scrolls with the document;
it has no height cap or separate vertical scrolling. On wide screens the map
sits to the right, as on item details. On narrow screens it follows the table.
Page-size labels and controls share an explicit flex row with a 12 px gap in
both catalogs and item lists.

Screenshots: [items desktop](screenshots/items-light.png),
[items mobile](screenshots/items-mobile.png),
[item detail, dark](screenshots/item-dark.png).

Layout follow-up verification: the 1,000-row table's client height equals its
scroll height on both desktop and 390 px mobile, confirming no internal vertical
scroll area. The map lies to the right on desktop and below results on mobile.
Page-size label/select gaps measure 12 px in both item controls and the catalog;
mobile pages have no horizontal document overflow. All 654 API tests, seven
cross-API contracts, clippy, formatting and HTTP smoke checks passed again.

## Array presentation follow-up

Flat property arrays again show their values immediately as wrapping chips,
including numeric lists such as elevation angles and string lists such as
quantities. The same generic rule applies to item details and selected listing
columns. Empty arrays are labelled explicitly. Arrays containing arrays/objects
and object values retain expandable JSON views. No collection-specific rules
or changes to the API data are introduced.

Browser verification rendered the original captured Herwijnen response directly
through the HTML renderer, preserving its typed arrays (the GeoJSON sample loader
stringifies them). All 14 elevation angles and nine quantities appear as chips
in details and selected listing columns, including after restoring column choices.
135 common/Features tests, seven cross-API contracts, formatting and workspace
clippy passed. [Array chips screenshot](screenshots/item-array-chips.png).
