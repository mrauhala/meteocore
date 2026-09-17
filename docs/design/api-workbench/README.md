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
  the returned page, without background tiles; overlapping shapes have a picker.
- Applied parameters remain in the persistent JSON link and copyable request.
  Unsubmitted edits have a separate request preview. Browser enhancement restores
  the last filtered list when returning from a collection or item.
- Light/dark/system themes share a 16px body/control scale and 14px minimum for
  supporting text and code. Theme choice persists locally.
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
- `cargo test -p api-common -p api-edr -p api-features -p api-maps -p api-tiles`
- `cargo test -p server --test common_discovery`

The API suites passed 645 tests (four pre-existing ignored tests); cross-API
contracts passed seven. Contracts cover query/format preservation, full metadata,
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

[Collections, light](screenshots/collections-light.png) ·
[Feature properties, dark](screenshots/item-dark.png)
