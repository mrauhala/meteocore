# engine-cap

Serves OASIS Common Alerting Protocol (CAP) v1.2 warnings: each alert area
as an OGC API Features feature and as a severity-shaded map layer.

## What it solves

Weather and civil-protection services publish warnings as CAP XML: files
in a directory, entries in an Atom/RSS feed, or WMO WIS2 notifications. A
warning names its areas (polygons, circles or zone codes), a validity window
and a severity; later messages update or cancel it. This crate keeps the
current alert set from one such source and serves it as queryable features
and as a map layer that can be drawn for any time.

## How it works

- **One snapshot.** Each refresh builds an immutable catalog from the source
  and swaps it in atomically; every request reads that snapshot.
- **Sources** (exactly one per collection):
  - `data_path`: a directory of `*.xml` files, subfolders included. Hidden
    names (`.name.xml`) and files in hidden folders are skipped.
  - `feed_url`: an Atom/RSS index; one CAP link per `<entry>`/`<item>`,
    fetched concurrently.
  - `[cap.wis2]`: a WIS2 Global Broker subscription through `ds-wis2`
    (see [WIS2 mode](#wis2-mode)).
- **Failures keep data.** Directory and feed sources keep the last good copy
  of each document. A failed fetch or parse keeps that copy; a document
  missing from a successful listing is removed. A failed refresh still
  advances the catalog's clock, so expired alerts leave the map on time.
- **Parsing.** CAP `lat,lon` coordinates become `[lon, lat]`. A `<circle>`
  becomes a geodesic polygon of `circle_segments` vertices.
- **Area geometry**, first that resolves: inline polygons and circles, then
  the `geocode_geometry` zone lookup, then (WIS2) the notification's
  `rel=geometry` zone polygon, then (opt-in) the notification bbox. An area
  that resolves to nothing has null geometry: listed, never drawn.
- **Revisions.** `status_filter` applies first. Per `(sender, identifier)`
  the newest `<sent>` wins (WIS2: the newest publication). An `Update` or
  `Cancel` withdraws exactly the messages its `<references>` name; `Cancel`,
  `Ack` and `Error` messages are never shown.
- **Validity window**: start = `onset`, else `effective`, else `sent`;
  end = `expires`, else start + `default_ttl`, else open.
- **Rendering.** Polygon vertices are projected to output pixels and
  filled; overlaps keep the highest severity.

## What it serves

| Trait | APIs | Content |
|---|---|---|
| `FeatureEngine` | Features, Tiles (vector tiles) | One feature per alert × `<info>` × `<area>` |
| `MapEngine` | WMS, Maps, Tiles (map tiles) | One layer, parameter `severity` |

**Features.**

- Id: `cap:<n>:<sender><identifier>.<info>.<area>` (`<n>` = sender length
  in bytes, indices from 0). `<identifier>.<info>.<area>` also resolves
  when exactly one sender uses that identifier.
- Properties under their CAP names: `identifier`, `sender`, `sent`,
  `status`, `msgType`, `scope` (from `<alert>`); `language`, `category`,
  `event`, `responseType`, `urgency`, `severity`, `certainty`, `effective`,
  `onset`, `expires`, `senderName`, `headline`, `description`,
  `instruction`, `web` (from `<info>`); `areaDesc`. Times are RFC 3339 UTC.
- Derived: `active_until` (resolved validity end), `radius_km` (an area
  that is exactly one circle), `geometry_source` (`inline`, `geocode`,
  `notification`, `bbox`) and `awareness_type_code` (the integer prefix of
  a MeteoAlarm `awareness_type` such as `"1; Wind"`).
- Producer `<parameter>`s are top-level properties under their `valueName`,
  verbatim; a repeated name becomes a list, and a name that clashes with a
  standard property becomes `parameter:<valueName>`. `<eventCode>`s are
  `eventCode:<valueName>`.
- Label: `event · areaDesc`, from the parts the area has, served as the
  feature's `title`. HTML listings show `severity`, `onset` and
  `active_until` first, then MeteoAlarm's `awareness_level` or `urgency`.
- `bbox` selects areas whose bounding box intersects it (no exact-shape
  test; areas without geometry never match). `datetime` keeps areas whose
  validity window overlaps it; without it every loaded area is returned.
  Property filters accept every standard name, even with no alerts loaded,
  plus the producer names in the loaded alerts.

**Map.**

- Pixel value: severity code 0 (Unknown, absent or unrecognised), Minor 1,
  Moderate 2, Severe 3, Extreme 4. Set `[wms] colormap = "cap_severity"`
  to shade them grey, green, yellow, orange and red.
- Without a time (`TIME` in WMS, `datetime` in Maps and Tiles), the map
  shows the areas active at the catalog's build time; an explicit time
  shows those active at that instant. Cached tiles are re-rendered
  whenever the alert set changes.
- Advertised times: the build time plus every window start and end up to
  seven days after it, at most 256. Over the cap, each window first gets
  one instant inside it, then the boundaries nearest the build time.

## WIS2 mode

- `Update` and `Cancel` messages and `rel=deletion` notifications apply on
  arrival. Withdrawn messages are remembered for `retention_grace`, at
  least an hour, so a late copy from another Global Cache cannot return.
- The catalog is rebuilt at most every 5 s when something changed, and at
  least every `poll_interval_secs`.
- An alert is evicted `retention_grace` after its validity end (receipt +
  7 days when it has none). Past `max_alerts`, valid alerts go too: not yet
  active first, then lowest severity, then oldest received.
- With `geometry_links`, each notification's `rel=geometry` zone polygon is
  downloaded on arrival and must fit the notification bbox. MeteoAlarm CAP
  documents carry only zone codes, so their polygons come from here. An
  area made of several zones draws their union.
- With `[server] state_dir` set, the alert set is saved to
  `<state_dir>/<collection id>.cap.state` (at most every five minutes when
  changed, at least hourly, and on shutdown) and restored at start under
  the current config.
- A reload that rebuilds the collection hands the whole alert set, with
  its warm-up, to the new engine in memory, under the new config, with
  or without `state_dir`. The new engine opens a new broker session, so
  notifications published between the old engine's shutdown and the new
  subscription are missed.
- A start without a usable snapshot (every start, without `state_dir`)
  begins empty, and `/health` reports "warming up after cold start: N
  alerts received" for `warmup` after the subscription comes up. A
  snapshot older than `warmup` restarts the warm-up.

## Configuration

A per-collection file (`[collections.cap]` when inline in `config.toml`):

```toml
id = "cap-alerts"
title = "CAP alerts"
description = "CAP v1.2 alert areas as features and a severity map layer."
engine_type = "cap"
apis = ["features", "wms", "maps", "tiles"]

[cap]
data_path = "testdata/cap"   # or feed_url = "https://…", or [cap.wis2]
# geocode_geometry = "testdata/cap/emma-fi.geojson"
# geocode_value_name = "EMMA_ID"

[wms]
colormap = "cap_severity"
```

| Field | Default | Meaning |
|---|---|---|
| `data_path` | — | Directory of CAP `*.xml` files |
| `feed_url` | — | `http(s)` Atom/RSS feed linking to CAP documents |
| `feed_allowlist` | `[]` | Extra `http(s)` URL prefixes, ending in `/`, that feed entries may use; needs `feed_url`. Entries off the feed's own origin and not listed are dropped |
| `poll_interval_secs` | `300` | Rescan interval; in WIS2 mode the forced rebuild interval (at least 5) |
| `language` | unset | Keep the `<info>` blocks in this language (primary subtag, case-insensitive), else the first. Unset keeps every `<info>` |
| `status_filter` | `["Actual"]` | `<status>` values to serve, case-insensitive; `[]` serves all |
| `default_ttl` | unset | ISO 8601 duration added to the start when `<expires>` is absent; unset means open-ended |
| `circle_segments` | `64` | Vertices per `<circle>` polygon (at least 3) |
| `geocode_geometry` | unset | Local GeoJSON FeatureCollection mapping zone codes to polygons. Loaded once; a bad file fails the collection |
| `geocode_property` | `"code"` | Lookup feature property holding the zone code |
| `geocode_value_name` | unset | Resolve only `<geocode>`s with this `valueName` (case-insensitive) |
| `retention_grace` | `"PT1H"` | WIS2: keep an alert this long past its validity end |
| `max_alerts` | `10000` | WIS2: most alerts held in memory |
| `geometry_links` | `true` | WIS2: download `rel=geometry` zone polygons |
| `bbox_fallback` | `false` | WIS2: draw the notification bbox for an area with no other geometry |

`[cap.wis2]` is the WIS2 subscription block shared with engine-bufr.
`topics` is required (filters starting `cache/` or `origin/`). Defaults:
`broker = "mqtts://globalbroker.meteo.fr:8883"`, `username` and `password`
`"everyone"` (or `password_env`), `download_allowlist = []`,
`warmup = "PT24H"`, `degrade_after_secs = 120`, `max_download_bytes` 8 MiB,
`dedup_window = "PT1H"`, `session_expiry_secs = 900`, `keep_alive_secs = 30`.

Runnable examples: `collections.d/cap-alerts.toml` serves the demo alerts in
`testdata/cap/`; `collections.d/cap-meteoalarm-wis2.toml` subscribes to the
MeteoAlarm warnings topic.

## Health and metrics

- Directory and feed: `ready` after the first successful load, even with
  zero alerts; `degraded` while the last refresh or any document failed.
- WIS2: `degraded` while connecting or subscribing, after a disconnect
  longer than `degrade_after_secs`, while over `max_alerts`, and during
  warm-up.
- `/metrics`: `cap_alerts_active`, `cap_alerts_superseded_total`; in WIS2
  mode also `cap_alerts_held` and the `cap_wis2_*` families.

## Limits

- No EDR, no `sortby`, one map layer per collection, and no CAP signature
  (XML-DSig) check. WMS GetFeatureInfo answers the alerts active at the
  frame's instant whose areas hold the pixel's centre, most severe first.
- HTTP redirects are refused; configure the final feed and document URLs.
  Each poll refetches the feed and every linked document.
- No WIS2 Global Cache backfill: an empty start fills only as producers
  republish their warnings.
- Size caps: 8 MiB per feed index or directory/feed document (WIS2
  payloads: `max_download_bytes`), 4 MiB per `rel=geometry` polygon,
  50 000 files per directory scan, 2 000 feed entries (64 with a query
  string), 10 000 alerts per document, 50 000 vertices per polygon (a
  larger one is dropped), 256 MiB lookup file. One render draws at most
  50 000 areas, keeping the most severe.

## Testing

```bash
cargo test -p engine-cap
```

The tests run offline. `tests/fixtures/` holds synthetic CAP documents
(polygon, circle, two languages, geocode-only, `Test` status, timed) and a
zone lookup. `tests/wis2-fixtures/` holds captured MeteoAlarm documents,
zone polygons and a notification; it sits outside `tests/fixtures/`, which
the directory-source tests scan recursively.

Contributor notes: [CLAUDE.md](CLAUDE.md).
