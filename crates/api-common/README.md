# api-common

The shared OGC API - Common layer for the EDR, Maps, Tiles and Features
crates. It covers collection search and paging, collection descriptions,
OpenAPI fragments, error pages, the HTML workbench and the shared OGC API
root.

## What it solves

Every OGC API in MeteoCore serves the same Common resources: a landing page,
a conformance declaration, a searchable `/collections` list and a
description of each collection, all as JSON and as HTML. Four separate
copies of the parameter rules, links and pages would drift apart, so this
crate holds the one copy. It owns the HTTP side. The pure policy behind it
(search matching, extents, content negotiation, escaping) is in `ds-core`.
Engines never use this crate.

## How it works

- The `CollectionRequest` extractor validates a `/collections` query before
  any engine is consulted.
- The API crate builds the JSON for each collection from one snapshot of
  its engine registry. It passes the JSON in as a `CollectionEntry`,
  together with the CRS84 bbox and time span that the JSON advertises.
  `collections_response` then filters, sorts, pages and renders the list.
- `f` selects the representation and takes precedence over `Accept`. The
  representation is chosen before any other parameter is parsed.
- Maps, Tiles and Features build every link from the external base URL plus
  the router's `Mount`, never from a fixed path. So one set of handlers
  serves both a per-API service and the shared root. EDR builds its links
  under its fixed `/edr` mount.
- HTML pages are rendered from the JSON that the same request returns, and
  rendering runs no engine queries.

| Item | Purpose |
|---|---|
| `CollectionRequest`, `collections_response` | Validate a `/collections` query, then filter, sort, page and represent the list |
| `collection_metadata`, `parameter_names` | Common collection fields and links; the Maps/Tiles `parameter_names` member |
| `collection_operation`, `parameter_name_parameter`, `quality_parameter`, `legend_schema` | OpenAPI definitions defined once, so every block declares identical ones |
| `CONFORMANCE_CLASSES`, `conformance_classes` | The Common classes, joined with each API's own |
| `JsonQuery`, `JsonError` | Query extraction and errors that answer with a JSON `{code, description}` body, with the reason recorded for the request log |
| `Mount`, `mounts`, `rel`, `openapi_tags`, `ApiKind` | Link roots, per-API paths, registered OGC relations, OpenAPI tags, API label for logs and metrics |
| `shared` | The shared OGC API root, composed from building blocks |
| `caching` | `conditional_get` middleware: `Cache-Control`, ETag, 304 |
| `errors` | HTML error pages for browsers, the 404 fallback, JSON-only routes |
| `subset`, `map_frame` | Request rules that Maps and Tiles share |
| `workbench` | Server-rendered HTML pages for all four APIs |

## Collection discovery

`/collections` accepts the same parameters on every API. Their names come
from `ds_core::collection_search::CollectionParameter`.

| Parameter | Behaviour |
|---|---|
| `bbox` | 4 or 6 comma-separated CRS84 numbers. Matches on horizontal intersection; vertical bounds are ignored. `west > east` crosses the antimeridian |
| `bbox-crs` | CRS84 only, as a URI, CURIE or short form |
| `datetime` | An RFC 3339 instant or interval (`start/end`, `../end`, `start/..`) that must overlap the collection's temporal extent |
| `q`, `query` | Text search (see below) |
| `sortby` | Comma-separated `title` and `id` keys, each with an optional `-` for descending order. Titles compare case-insensitively; ties sort by id |
| `limit` | 1–1000; larger values are clamped. Default: 1000 for JSON, 24 for an HTML page |
| `offset` | The number of matches to skip. Default: 0 |
| `f` | `json` or `html`. Overrides `Accept` |

- A collection that advertises no spatial or temporal extent matches every
  `bbox` or `datetime` filter.
- Filters run first, then `sortby`, then paging. Without `sortby`, JSON
  lists collections by id and HTML lists them by title.
- An unknown parameter, a repeated parameter or an invalid value returns a
  400 with a `{code, description}` body. For an HTML request, the 400 is the
  catalog form, filled in as sent, with the field at fault marked.
- The JSON response has `collections`, `numberMatched`, `numberReturned`
  and `links`. The `self`, `next` and `prev` links keep every filter and
  set `f` explicitly. They include `limit` and `sortby` only when the
  request did.
- The `alternate` link always spells out `limit` and `sortby`, because JSON
  and HTML have different defaults. So an HTML page's JSON alternate is
  `?limit=24&f=json&sortby=title`.

### Text search

`q` and `query` search the title, the description and each keyword, but not
the id. They match each of these separately. Matching ignores case (Unicode
lowercase), normalises whitespace and treats punctuation literally. A term
or phrase must match whole words within one property or keyword. So
`q=sea surface` matches "Sea  surface", but not "undersea surface" or
"sea-surface". It also does not match a title that ends in "sea" when the
description starts with "surface".

- `q` takes comma-separated alternatives. A collection matches if any one
  alternative does.
- `query` also takes comma-separated alternatives:
  - Inside an alternative, `+` marks a required term and `-` an excluded one.
  - Unsigned words extend the term before them. Words before any sign form a
    required phrase.
  - Required terms may match different properties.
  - A sign inside a word is literal (`united-states`, `radar+hail`).
  - An alternative with only excluded terms matches every collection that
    lacks them.
- `q`, `query`, `bbox` and `datetime` combine with AND.
- An empty `query` alternative returns 400, and so does a `+` or `-` with no
  term attached.

| Decoded `query` | Meaning |
|---|---|
| `canada +weather -extreme` | canada and weather, anywhere in the searched fields, and no extreme |
| `canada +extreme weather` | canada and the phrase "extreme weather" |
| `canada -extreme,united states` | canada without extreme, OR the phrase "united states" |
| `-extreme` | every collection whose searched fields lack extreme |

In a URL, encode a required `+` as `%2B`, because form decoding turns a bare
`+` into a space: `/collections?query=canada%20%2Bweather%20-extreme`.
Navigation links keep the operators encoded. With curl,
`curl --get --data-urlencode 'query=canada +weather -extreme'` encodes them
for you.

## Collection descriptions

`collection_metadata` builds the Common fields of a collection:

- `id`, `title` and `description`, plus `keywords` when any are configured.
- An HTML alternate of the caller's `self` link.
- A `license` link when a license is configured:
  - It points to the configured `url`, or to
    `https://spdx.org/licenses/<id>.html` when the title looks like an SPDX
    id.
  - Its `type` is `text/html` unless `[collections.license] type` is set.
  - A free-text title with no `url` gets no link.

The API's own fields, such as `extent`, `crs`, `styles` and `data_queries`,
are merged on top; EDR instances replace `id` and `title` this way.

`parameter_names` lists the parameters a multi-parameter raster can render,
as EDR-style `Parameter` objects keyed by their `parameter-name` value and
sorted by name. Each entry has a label and, where known, a unit and the
parameter's own time extent. RGB composites are also listed. They have no
unit, and a `description` names their channels.

## Shared OGC API root

`shared::router` composes OGC API standards as `BuildingBlock`s under one
root. The server mounts it at `/` with the Maps, Tiles and Features blocks,
in that order. The composer serves:

- `/`: the landing page, which links to the related services the server
  passes in (EDR, 3D Tiles, WMS, health and metrics)
- `/api` and `/api/docs`: one merged OpenAPI 3.0.3 document, and Swagger UI
  for it
- `/conformance`
- `/collections` and `/collections/{id}`

Each block adds its data routes, an OpenAPI fragment, landing links and
conformance classes. For each collection it serves, a block also adds a
`Contribution`. Contributions merge in block order:

- Links are concatenated, and the first block to describe a field keeps it.
- A block can claim a field it omits (`Contribution::claims`) so that no
  later block fills it in. Raster blocks claim `storageCrs`, which they omit
  when the native CRS has no OGC URI.
- `styles` merge by id. `crs` lists are combined in block order, without
  duplicates.
- Discovery uses the bbox and time span of the block whose `extent` is kept.
- A block that serves feature items adds an `ItemsOverview`
  (`Contribution::items`): what one item is, and the properties its items
  page filters and sorts by. The first block to add one keeps it. Only the
  collection's HTML page shows it; it adds nothing to the JSON.
- In OpenAPI, the earlier block wins a name clash. Blocks keep shared
  components identical or prefix them (`features-bbox`).

A class that holds only in the shared layout goes in `shared_conformance`.
For example, Maps "Map Tilesets" needs `{map}/tiles`, which only the shared
root has. Common metadata routes use `conditional_get`, which defaults to
`Cache-Control: public, max-age=60` and adds an ETag and 304 responses;
data routes set their own ETags. Blocks reuse their per-API services' state,
so a reload updates both.

## Conformance

`CONFORMANCE_CLASSES` lists Common Part 1 `core`, `landing-page` and
`oas30`, and Common Part 2 `collections`, `json` and `html`. The
[Common Parts 1–4 matrix](../../docs/ogc-api-common-matrix.md) records each
declaration and its known gaps.

## Error pages

JSON `{code, description}` is the error contract. `errors::html_errors`
wraps the shared root, EDR, Features, Maps and Tiles routers. When a request
asks for HTML, a JSON 4xx or 5xx becomes a workbench page with the same
status. The page names the parameter at fault, when known. It links back to
the catalog, and to the collection when the failed resource lies below one.
A 5xx page shows a generic sentence, never the body.

A request asks for HTML when the first `f` in its query is `html`. With no
`f`, or an empty one, it asks for HTML when `Accept` ranks `text/html` above
every other listed type; `*/*`, `text/*` and XML types do not count. This is
stricter than the rule for successful responses, which are HTML whenever
`Accept` admits `text/html` other than through `*/*`.

Handlers can add detail with `errors::ErrorDetail`: the parameter at fault,
a heading, a message, a hint, and accepted values shown as links.
`errors::not_found` is the server's 404 fallback: an HTML page for a
browser, an empty 404 otherwise. `errors::json_only_get` returns 400 for any
`f` other than JSON on routes that only serve JSON (style lists, tilesets,
tile matrix sets).

## Map request helpers

Maps and Tiles share these helpers, so both parse and size a map the same
way.

- `subset` parses the OGC API - Maps `subset` grammar: repeatable,
  comma-separated `axis(low:high)` or `axis(value)`, where values are
  numbers, quoted text or `*`. Each route supplies its own axes and aliases,
  and an unknown or repeated axis is an error. `TimeSelection` reads
  `datetime` values and time subsets, including partial dates; vector tiles
  use it too. `unknown_parameter` finds a query parameter the route does
  not accept, for a 400 that lists the accepted ones.
- `render_time` picks the time to render:
  - no time: the engine's default, else the latest time;
  - an instant: passed on for the engine to snap to a time step;
  - an interval: the latest time step inside it.

  When nothing matches, Maps returns 404 and map tiles return 204.
- `map_frame` defines `MapCrs` (CRS84, EPSG:4326, EPSG:3857, EPSG:3067,
  EPSG:3035), each in its own axis order. It also handles `Content-Bbox`
  and converts scale denominator to resolution at 0.28 mm per pixel. The
  default longer side is 1024 px. Output is capped at 8000 px per side and
  64 000 000 px in total; larger requests return 400. The projection math
  comes from `ds_core`.

## HTML API workbench

`workbench` renders the HTML pages: landing, conformance, the collection
catalog, collection pages, EDR model-run lists and error pages. api-edr and
api-features wrap their data and item pages in the same shell (`Page`).
Every page has a JSON link and a copyable URL and cURL command for the
resource as currently filtered and paged.

- **Workspaces.** The switcher offers the shared root and EDR. The per-API
  Maps, Tiles and Features services appear in it only on their own pages.
- **Catalog.** 24 collections per page, sorted by title, unless the request
  says otherwise. Each row has fixed slots for advertised time, cadence,
  area, vertical range, and parameters or styles. Feature collections also
  show an item count. A dash marks a value that is not advertised. Results
  can be shown as a list or as cards.
- **Collection page.** Overview and metadata tabs. The heading shows one
  chip per OGC API that the collection's links advertise. The metadata tab
  shows every member of the collection JSON as tables and links, including
  every link with its relation and media type. A missing member reads "Not
  advertised", and nothing is inferred. When the Features API serves the
  collection's items, the Overview adds a line on what one item is (when the
  engine gives one) and "Filter by" and "Sort by" chips. Each chip opens the
  items page with its request builder set to that property, named in the
  URL fragment (`#filter=<name>`, `#sort=<name>`).
- **Map preview.** Drawn with MapLibre over Natural Earth outlines, which
  the server serves under `/preview/vendor/`.
  - It draws with whatever the collection advertises: map tiles, Maps
    images (Web Mercator PNG) or vector tiles. On the shared root, a
    collection the WMS serves (its `apis` lists `wms`) can also be drawn with
    WMS 1.3.0 GetMap, the meta-tiled path desktop WMS clients see. Map tiles
    are the default where advertised, else Maps. A "Draw with" switch
    appears when there is more than one.
  - Maps images, map tiles and WMS get parameter, style, time and level
    controls when the collection advertises them. The style select is hidden
    when there is one style. Vector tiles take none.
  - Every control applies on `change`; there is no Update button. Pan and
    zoom request a new Maps image 300 ms after the view settles, and map
    tiles follow the view by themselves. Every request reads one drawn
    state, so a pan never applies an edit that was not committed.
  - A new frame never flashes. It loads at opacity 0 as its own raster
    source and layer, below the borders, with no paint transition and no
    tile fade. Once MapLibre reports it loaded, one paint shows it and hides
    the old frame, which is removed after that paint. The visible source is
    never removed or retargeted.
  - A failed request keeps the last frame and marks it stale in the status
    line ("Still showing …"). A 503 is retried at most three times, each
    after `Retry-After` or a 1, 2 and 4 s backoff, whichever is longer. A 404, or map tiles that all come back
    empty, says there is no data at that time.
  - The legend stays, dimmed, while the next one loads. Each legend
    document is fetched once per page, and the colour bar is drawn at the
    data's 85 % opacity.
  - HiDPI: a Maps image is the map's CSS size × `devicePixelRatio`, at most
    about 4 Mpx. At a ratio of 1.5 or more, map tiles are requested at
    512 × 512 pixels (Tiles `width`/`height`) over the same 256-pixel tile
    area.
  - The status line names the time drawn: for a Maps image, the instant
    from `Content-Datetime`; for map tiles and WMS, which name none, the
    time requested, and only once the frame has loaded.
  - **Time bar.** A collection with a time axis gets a time bar under the
    map. `mapRequest` carries the axis compactly, `{start, step, count}`
    (step in seconds) for a regular grid and the list of times for an
    irregular one, and each parameter's own axis (`parameter_names[*].extent.temporal`).
    The page emits no option per time step.
    - Play and pause, one step either way, first and last frame of the
      loop, speed (1, 2, 4 or 8 frames a second; 4 at cadences of 15 min
      or less, else 2) and Loop, Bounce or Once. The last frame of a loop
      holds for 1.2 s. A `…` menu jumps to a date and time in UTC and sets
      the loop's ends.
    - The track is laid out by time: notches where frames are at least
      4 px apart, else a strip broken at gaps; day and hour ticks; the
      viewer's clock; the frames buffered; failed frames in red.
    - The loop window is a band with two handles; `[` and `]` set its ends
      at the playhead. By default it holds the last 24 frames of
      observations, or a forecast's 24 frames from the step nearest now, or
      every frame when there are fewer, at most 96. An axis counts as a
      forecast when it ends more than 10 min after the viewer's clock.
    - The label shows the time drawn, in UTC with how long ago or ahead it
      is ("25 min ago", "in 3 h"), and changes in the paint that swaps the
      pixels.
    - The map opens on the latest frame of observations, or a forecast's
      step nearest now. Every frame request names its time from the axis
      (`datetime`, or WMS `TIME`), so it is cached as `immutable` and a
      second loop asks the server for nothing new.
    - Playback shows a frame only once it has fully loaded; until then the
      current frame holds. Failed and empty frames are marked and skipped.
      Frames change with hard cuts.
    - Frames are buffered ahead only once the viewer uses the time bar:
      map tiles one frame at a time, up to K = clamp(⌊160 / tiles in
      view⌋, 2, 6); images at most 2 fetches at a time and 3 frames ahead;
      one frame under Save-Data. A frame not buffered goes first. A layer
      change drops them all; a hidden tab or a map scrolled out of view
      pauses playback and buffering; a pan pauses image playback. Before
      the first play on a metered connection the bar says about how many
      frames are to load.
    - Dragging the playhead shows buffered frames at once and asks for the
      frame under the pointer after 150 ms at rest or on release.
    - Another parameter rebinds the bar to that parameter's axis at the
      nearest instant.
    - Keys, while focus is in the map or its time bar: Space or K play and
      pause; `,` and `.` step (Shift: 10 frames), as do the arrows on the
      time slider; Page Up and Page Down a day; Home and End the axis ends;
      `[` and `]` the loop's ends; `<` and `>` the speed; N the frame
      nearest now; `?` lists them. Arrow keys on the map pan it.
    - The playhead is a native range input whose `aria-valuetext` is the
      time. Steps, pauses and errors are announced, played frames are not,
      and nothing ever plays by itself.
  - **WMS GetMap.** `CRS=EPSG:3857`, so the BBOX is minx,miny,maxx,maxy in
    metres from MapLibre's Web Mercator; `LAYERS=<collection>/<parameter>`
    or the collection layer, `STYLES`, `TIME` and `ELEVATION`;
    `FORMAT=image/png&TRANSPARENT=TRUE&EXCEPTIONS=XML`. An XML answer is
    the ServiceException it carries, and an error image (`x-cache: ERROR`)
    is never drawn. Images are sized and swapped as Maps images are. It is
    not in the no-JavaScript form.
  - The URL fragment keeps the view and the choices under the API's own
    parameter names, for example
    `#map=6/61.5/24.25&parameter-name=t&style=warm&datetime=2024-01-01T01:00:00Z&elevation=850&mode=maps`,
    and a loop window the viewer set as `window=<start>/<end>`. It is not
    written for each played frame.
    A reload or a shared link reopens them. It is written with
    `history.replaceState` at most about once a second, and never over
    another fragment such as `#metadata`.
  - On a collection page the wheel and a one-finger drag scroll the page.
    The map zooms with Ctrl or ⌘ and the wheel, and pans with two fingers.
  - On an items page, the request builder's **Use map view** copies the
    map's current view into `bbox` as a CRS84 box, west > east across the
    antimeridian.
- **Without JavaScript.** Navigation, metadata, JSON links, paging and basic
  text search all work. The map controls are a `GET` form to the
  collection's Maps endpoint. Its only fields are Maps parameters
  (`parameter-name`, `datetime`, `elevation`), and each style has its own
  "Open map image" button. The time is a date and time input bounded by
  the axis and stepping by a regular grid's step. JavaScript adds the optional search fields, the
  map, clipboard buttons and view switching.
- **Theme.** Light, dark or system, remembered in the browser.

## Limits

- Common Part 4 Searchable Collections is not declared. Draft OGC 25-046
  also requires `sd` and `resolution`, which are not implemented.
- There is no sortables resource, CQL2 filtering or collection hierarchy.
- Text search has no ranking, quoting, stemming or language-specific
  tokenizer.
- The workbench has no HTML views of map images, tiles, WMS or 3D Tiles.
- The map preview needs JavaScript and WebGL.

## Testing

```sh
cargo test -p api-common                          # unit tests
node --test crates/api-common/tests/map.test.cjs  # map script, stub DOM and MapLibre
cargo test -p server --test common_discovery      # cross-API contracts
```

The map tests run the shipped `workbench/map.js` on
[`tests/fixtures/map-panel.html`](tests/fixtures/map-panel.html), the map
panel as `workbench` renders it. A unit test fails when the fixture is
stale; `UPDATE_MAP_FIXTURE=1 cargo test -p api-common map_panel_fixture`
regenerates it. api-maps submits the panel's no-JavaScript form to its own
router (`workbench_form` in its integration tests).

[`common_discovery`](../server/tests/common_discovery.rs) runs the real
EDR, Maps, Tiles, Features and shared-root routers over in-memory catalogs,
with no storage or rendering. It validates their JSON against the pinned
Common Part 2 and Part 4 bundles and the TMS 2.0 schemas in
[`schemas/`](../../schemas/README.md), and validates the shared OpenAPI
against the OpenAPI 3.0 meta-schema. It also checks paging, errors and HTML
negotiation. It mounts Maps, Tiles and Features under another prefix and
follows every advertised JSON link. Each must resolve and, apart from
cross-API links to Tiles, stay on that mount.

Contributor rules are in [CLAUDE.md](CLAUDE.md).
