# engine-bufr crate — Claude Instructions

WMO BUFR surface-observation engine (`engine_type = "bufr"`): SYNOP / SHIP
station reports → an in-memory, time-windowed store → `EdrEngine`
(`locations`, `position`, `area`, `radius`) + `FeatureEngine` (one Point
feature per station). Source: a polled directory / object-store prefix of
BUFR files (`data_path`); the WIS2 push source (`[bufr.wis2]`) is the next
PR. Real fixtures: `testdata/bufr-synop/` (8 reports captured from the WIS2
Global Broker, values cross-checked with ecCodes `bufr_dump`).

## The decoder boundary

- **`src/decode.rs` is the only module that imports `tinybufr`.** Keep it
  that way: if a real feed hits one of tinybufr's gaps (compressed
  character strings, operators 203/204/207/22x) the fix is to vendor or
  replace the decoder behind `Decoder::decode`, not to spread the API.
- `Tables::default()` rebuilds three hash maps from ~1 MB of statics —
  build **once per engine** (`Decoder::new`), never per message.
- `Value::Decimal(v, s)` means `v · 10^s` with `s` negative for fractions.
- Files may concatenate several `BUFR…7777` messages; `decode()` scans for
  the magic and uses the section-0 total length. A failure is scoped to
  its own message (`Decoded::failed`): the reports of the messages before
  and after it survive, and only a stream with no `BUFR` magic at all is
  an `Err`. Pinned by `concatenated_file_keeps_the_good_messages_around_a_bad_one`.
- Unsupported operators / features are `DecodeError::Unsupported`
  (counted per message in `bufr_decode_failures_total{reason="unsupported"}`),
  other failures `reason="error"`; neither is fatal to the file or the scan.

## Extraction rules (template-agnostic)

- Station identity: WIGOS `001125–001128` → `0-20000-0-02598`; else
  traditional `001001/001002` → `0-20000-0-{BB}{SSS}`; else ship call sign
  `001011` → `ship:{callsign}`; none → subset skipped (`no_station_id`).
- Position `005001/005002`, `006001/006002`; elevation `007030 > 007001 >
  007007`; nominal time = the **first** `004001–004006` group.
- **Period context**: `004023/004024/004025/004026` (days/hours/minutes/
  seconds) set the current period. A negative value opens "the N hours
  ending at the nominal time"; a `0` is the end marker of a start/end pair
  and keeps the previous period (SYNOP extremes are `(−12, 0) 012111`);
  missing clears it. Every following element carries that period.
- **First non-missing occurrence wins** per `(descriptor, period)` — in
  307096 the 2 m air temperature precedes the sensor-height replicas;
  precipitation / extremes each have their own preceding period. If a
  producer needs a different rule, add a `[[bufr.parameters]]` override,
  don't change the walker.
- ecCodes key names are not Table B names: its `pressure` is the `007004`
  standard-level coordinate; station pressure `010004` is
  `nonCoordinatePressure`. Trust the descriptor, not the label.

## Parameter table

Built-ins (`src/params.rs`, BUFR units, no conversion — the source
metadata is authoritative): air_temperature (012101|012004),
dew_point_temperature, relative_humidity, pressure (010004), pressure_msl
(010051), pressure_tendency_3h, wind_direction/speed/gust,
precipitation_{1,3,6,12,24}h (013011 by period; 24 h also 013023),
air_temperature_{max,min}_{12,24}h (012111/012112 by period), visibility,
cloud_cover_total, present_weather (code table), snow_depth.
`[[bufr.parameters]]` replaces by name or appends; `builtin_parameters =
false` serves only the config entries. Column order = table order; the
store row is a dense `Box<[f32]>` (`NaN` = missing); output widens through
`round_stored` so `290.12` stays `290.12`.

## Store & snapshot

- `store.rs`: `HashMap<station, StationSeries { info, rows: BTreeMap<time,
  row> }>`; ingest replaces an existing `(station, time)` row (corrections
  / `rel=update`); reports newer than `now + 1 h` or older than `now −
  retention − 1 h` are `OutOfWindow`; `prune()` drops expired rows, empty
  stations, and least-recently-seen stations beyond `max_stations`.
  Bound: retention × cadence × stations (global SYNOP ≈ 20 k × 24 × 22
  cols ≈ 50 MB).
- Ingest takes the `RwLock` write lock per file; request handlers take the
  read lock for one station lookup or one area scan. Every capability
  accessor (`get_locations`, extents, Features listing) reads the
  `ArcSwap<Snapshot>` rebuilt every 10 s when dirty (Critical Rule 10).
- `position` = nearest station within `position_radius_km` (25) by
  `ds_core::geo::great_circle_distance_m` — never a local haversine;
  `area` = `parse_area_coords` + `check_mask_budget` + exact
  `QueryPolygon::contains`, capped at `MAX_STATIONS_IN_POLYGON` (10 001) and
  a `MAX_RESPONSE_VALUES` (500 000) budget → `QueryTooLarge` (400); an
  empty area is an empty `CoverageCollection`, not 404; `radius` is the
  trait default (point + `within` → area).
- Features: `bbox` = station point inside; `datetime` = station has a
  report inside the interval; `sortby` ∈ `last_report, first_report,
  report_count, name` via `ds_core::feature::sort_features`;
  `data_version` = snapshot version.

## Lifecycle & runtime rules

- `new()` does a best-effort initial scan (local fixtures serve
  immediately; a failing source starts `Degraded`). `poll_loop()` on
  `poll_runtime()`: scan every `poll_interval_secs`, prune every 60 s,
  snapshot every 10 s when dirty. `live_health()` degrades after 3
  consecutive failed scans. Wired in `server/src/admin.rs` (`"bufr" =>
  ["edr", "features"]`), boot spawn + shutdown in `main.rs`,
  `rotate_poll_loops!` on reload.
- `source.rs` uses `ds-storage` (`list` + `get_many`, bounded concurrency,
  16 MiB per file): **poll runtime only** (Critical Rule 7), never from a
  request handler, never inside `spawn_blocking`. `scan()` fetches in
  `FETCH_CHUNK`-sized `get_many` calls and streams each file to a sink
  (the engine decodes + ingests it right there), so a 50 k-file backlog is
  never resident at once — `get_many` buffers a whole batch, which is why
  its doc says "a chunk, not thousands of paths" (engine-odim convention).
- The fixture retention in `collections.d/obs-bufr-local.toml` is
  `P36500D` only because the fixtures are dated; production keeps `PT24H`.

## Smoke test

```bash
cargo run -p server -- --collections=obs-bufr-local
curl 'localhost:8000/edr/collections/obs-bufr-local/locations'
curl 'localhost:8000/edr/collections/obs-bufr-local/locations/0-20000-0-02598?f=CoverageJSON'
curl 'localhost:8000/edr/collections/obs-bufr-local/position?coords=POINT(18.98 57.44)&f=CoverageJSON'
curl 'localhost:8000/edr/collections/obs-bufr-local/area?coords=POLYGON((10 55,30 55,30 70,10 70,10 55))&f=CoverageJSON'
curl 'localhost:8000/features/collections/obs-bufr-local/items?bbox=10,55,30,70'
curl -s localhost:8000/metrics | grep ^bufr_
```
