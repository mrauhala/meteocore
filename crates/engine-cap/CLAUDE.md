# engine-cap crate — Claude Instructions

OASIS Common Alerting Protocol (CAP) v1.2 alert engine (#396). Implements
BOTH `FeatureEngine` (one Feature per alert area) and `MapEngine`
(severity-shaded polygon fill) over one poll-and-swap `Catalog`. It is the
first vector→raster `MapEngine`: it fills alert polygons into the output
pixel grid with `ds_render::rasterize::fill_polygon` (#397) fed by
`ds_core::geo::geometry_to_pixels` — vertices projected via
`OutputCrs::world_to_fraction`, never per pixel. Its ds-render dependency is
an approved exception (root CLAUDE.md, Shared Domain Machinery).

## The load-bearing gotcha: coordinate order

**CAP polygons/circles are `lat,lon` (spec §3.3.4); `ds_core::Geometry` is
`[lon, lat]`. `src/parser.rs` swaps on ingest** — pinned by an
absolute-position test (a Helsinki alert must land at lon≈25, lat≈60).
Rings are closed defensively; `<circle>` → an N-gon (`circle_segments`,
default 64) on the geodesic via `destination_point`, carrying `radius_km`
as a property.

## Source & SSRF guard

- **Exactly one of `data_path`** (local dir of `*.xml`) **or `feed_url`**
  (Atom/RSS index → linked CAP docs). Both go through `ds-storage` from the
  background poll runtime only. The feed fetches the index then the linked
  docs with `DataStore::get_many` (bounded concurrency, per-object timeout,
  origin-grouped) — never a sequential blocking loop.
- **Feed SSRF guard:** an entry link is fetched only if it shares the feed's
  EXACT origin (scheme+host+port — not a prefix; `https://feed` rejects
  `https://feed.evil.com`) or matches an explicit `feed_allowlist` URL
  prefix; others are dropped with a WARN. Stops a compromised feed pivoting
  the server to `http://169.254.169.254/…` or internal hosts.
- Known limitation: the allowlist constrains request URLs, not redirect
  responses (object_store's reqwest client follows redirects; no disable
  knob in object_store 0.11). A proper fix belongs in ds-storage (#431).
- Config (`CapConfig` in ds-core) validated at load: `data_path` XOR
  `feed_url`, `feed_url` http(s), non-empty `language`,
  `poll_interval_secs > 0`, positive ISO 8601 `default_ttl`,
  `circle_segments >= 3`.

## Feature model

- **One Feature per `(alert, info, area)`**, id =
  `{identifier}.{infoIdx}.{areaIdx}` (stable, URL-safe). The emitted
  `Feature.id` is **percent-encoded** to a single URL path segment so the
  api-features verbatim self-link routes; axum's `Path` decodes it back.
  **Clients must use `Feature.id` as-is, not re-percent-encode it**; the raw
  CAP `<identifier>` is in `properties.identifier`.
- Multiple `<info>` (languages) and multiple `<area>` per info each fan out.
  `language` config keeps matching `<info>`s (primary-subtag,
  case-insensitive), falling back to the first info. `status_filter`
  (default `["Actual"]`) drops Test/Exercise/Draft at the alert level.
- **Geocode-only areas** (UGC/EMMA_ID/FIPS, no polygon/circle) get geometry
  from the optional `geocode_geometry` lookup — a GeoJSON FeatureCollection
  mapping zone codes → polygons (`geocode_property`, default `"code"`;
  `geocode_value_name` restricts which `<geocode>` valueName resolves, e.g.
  `"EMMA_ID"`). **MeteoAlarm requires this** — its CAP areas are
  geocode-only EMMA_ID zones; without the lookup they render nothing.
  `testdata/cap/emma-fi.geojson` is the Finland EMMA zone set. An area that
  still resolves to nothing becomes a `Geometry::Null` Feature (valid per
  RFC 7946 §3.2; listed, never on the map), counted as `geocode_only` in
  the load log. The lookup file loads once at construction; a bad path is a
  hard `new()` error.

## Time semantics

- **Active window** = `[onset ∨ effective ∨ sent,
  expires ∨ (start + default_ttl) ∨ open]`.
- Features `datetime=` selects areas whose window overlaps; no datetime ⇒
  all loaded areas. Map/WMS `TIME` selects areas active at that instant;
  **no TIME ⇒ active now** (the snapshot's `as_of`, advanced each poll so
  expired alerts drop out).
- **WMS TIME shape (load-bearing):** `RasterInfo.times` = distinct window
  boundaries ≤ `as_of` plus `as_of` itself (always the max entry, capped to
  256). The WMS handler resolves a TIME-less GetMap to `times.last()`, so
  `as_of` being last is what makes the default render "now".
- `data_version()` (Feature ETags) hashes record ids + severity + window +
  the text fields (event/headline/description/instruction/areaDesc) — an
  in-place text correction invalidates the ETag — but NOT `as_of`, so it
  stays stable across polls when content is unchanged.

## Rendering & extents

- Single layer per collection, parameter `"severity"`, value = CAP severity
  code (Unknown=0, Minor=1, Moderate=2, Severe=3, Extreme=4). Overlaps use
  `Combine::Max` — highest severity wins, order-independent.
- Style: the `cap_severity` builtin colormap (grey→green→yellow→orange→red
  with alpha; codes sit exactly on the 0–4 stops — no inter-code blending).
  Set `[wms] colormap = "cap_severity"`.
- `raster_info()` is O(1) from a prebuilt `Arc<RasterInfo>` in the snapshot.
- `spatial_extent()` = union of resolved geometry bboxes;
  `FeatureEngine::temporal_extent()` = `[min start, max end]` of alert
  windows (open bounds clamp to `as_of`), so the collection JSON advertises
  both extents via `ds_core::ogc_extent::build_extent`.

## Lifecycle

`CapEngine::new` does a best-effort initial load (never fails on an
empty/unreachable source — starts degraded, the poll loop fills in). Wired
in `server/src/admin.rs` (`"cap" => ["features","wms","maps","tiles"]`);
poll loop on `poll_runtime()`; `shutdown()` on reload. Demo:
`collections.d/cap-alerts.toml` over `testdata/cap/`.

Out of scope for v1 (follow-ups): reference-chain supersedes/cancel beyond
latest-wins, XML-DSig verification, per-`event` sub-layers, conditional-GET
feed caching, antimeridian splitting.
