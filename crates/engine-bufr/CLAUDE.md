# engine-bufr crate — Claude Instructions

WMO BUFR surface-observation engine (`engine_type = "bufr"`): SYNOP / SHIP
station reports → an in-memory, time-windowed store → `EdrEngine`
(`locations`, `position`, `area`, `radius`) + `FeatureEngine` (one Point
feature per station). Source: a polled directory / object-store prefix of
BUFR files (`data_path`) or a WIS2 Global Broker subscription
(`[bufr.wis2]`, `src/wis2.rs`). Real fixtures: `testdata/bufr-synop/` (8
reports captured from the WIS2 Global Broker, values cross-checked with
ecCodes `bufr_dump`) and `testdata/bufr-regressions/` (one small message per
live decode failure fixed in #1008; its README names the cause of each).

## The decoder boundary

- **`src/decode.rs` is the only module that imports `ds_bufr`.** Keep it
  that way: if a real feed hits an unsupported operator (207/22x, or 203 in
  compressed data) the fix is in `ds-bufr` behind `Decoder::decode`, not
  to spread the API.
- `Tables::default()` rebuilds three hash maps from ~1 MB of statics —
  build **once per engine** (`Decoder::new`), never per message.
- `Value::Decimal(v, s)` means `v · 10^s` with `s` negative for fractions.
- Files may concatenate several `BUFR…7777` messages; `decode()` scans for
  the magic and uses the section-0 total length. A failure is scoped to
  its own message (`Decoded::failed`): the reports of the messages before
  and after it survive, and only a stream with no `BUFR` magic at all is
  an `Err`. Pinned by `concatenated_file_keeps_the_good_messages_around_a_bad_one`.
- Every failure has a `FailureKind` (`DecodeError::kind`), counted per
  message in `bufr_decode_failures_total{reason, kind}`: `kind` is
  `not_bufr` (no BUFR magic: NIL bulletins), `truncated` (`failed to fill
  whole buffer`: an element read with the wrong width ran past the data
  section, or a short message), `unknown_descriptor` (no table entry: an
  unregistered local element), `invalid` or `unsupported`; `reason` keeps
  the older `error`/`unsupported` split. None is fatal to the file or the
  scan. The WIS2 source WARNs the first failure of each `(centre, kind)`
  with its `data_id` (a sample to fetch from a Global Cache) and an hourly
  per-centre summary (`decode failures in the last 60 min: …`); the metric
  stays per collection, since centres are unbounded.
- `ds-bufr` supports compressed character fields and delayed replication,
  operators 203 (uncompressed), 204 and 208, and numeric widths through 64
  bits (#693, #1008). Regression fixtures in `testdata/bufr-decoder/` are
  generated independently by ecCodes; `testdata/bufr-regressions/` are live
  messages checked element by element against ecCodes `bufr_dump`. This
  does not establish complete coverage of every live centre/template.
- **Master-table versions.** The generated tables are the current version.
  `LEGACY_MASTER_TABLE_B` gives messages of master version ≤ 13 the
  narrower 302045 radiation widths (it-meteoam, jp-jma, il-ims still encode
  13); without it every 307080/307086 with radiation ran past its data.
  Extend it only from ecCodes `bufr/tables/0/wmo/<version>/element.table`.
- National local descriptors have no width in the master tables, so one
  unknown element misaligns the whole message. `DWD_LOCAL_TABLE_B` in
  `decode.rs` registers the DWD (centre 78) elements seen live, each from
  the first local version that defines it, for local versions 1–8, verified
  against ecCodes' centre-78 local tables. Never install national entries
  globally: the same number can mean a different width at another centre.
  Extend centre/version selection only from a verified published local table.
- **One observation, several messages.** A DWD bulletin carries a station's
  SYNOP and its national supplement (020193 …: visibility, soil
  temperature, precipitation) as separate messages for the same station
  and time. `decode()` merges reports of one `(station, time)` at the same
  position (within 0.01°) within one stream, earlier elements first, so the
  supplement adds values instead of replacing the SYNOP row in the store.
  Reports at different positions never merge: anonymised vessels share the
  call sign `SHIP`. Across payloads a later report still replaces the row
  (a correction).

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

Built-ins (`src/params.rs`): air_temperature (012101|012004),
dew_point_temperature, relative_humidity, pressure (010004), pressure_msl
(010051), pressure_tendency_3h, wind_direction/speed/gust,
precipitation_{1,3,6,12,24}h (013011 by period; 24 h also 013023),
air_temperature_{max,min}_{12,24}h (012111/012112 by period), visibility,
cloud_cover_total, present_weather (code table), snow_depth.
`[[bufr.parameters]]` replaces by name or appends; `builtin_parameters =
false` serves only the config entries. Column order = table order; the
store row is a dense `Box<[f32]>` (`NaN` = missing); output widens through
`round_stored` so `16.97` stays `16.97`.

**Units are source-driven, then mechanically converted for display.**
Every entry (built-in or config) names the **BUFR Table B unit** the
descriptor is encoded in (`K`, `Pa`, `kg m-2`, `m s-1`, …); the shared
`ds_core::units::display_conversion` rule for that unit string — the same
table engine-grib applies (K → °C, Pa → hPa, kg m-2 → mm, m2 s-2 → gpm;
anything else unchanged) — is applied at row extraction, so the store
holds and the API serves `°C` / `hPa` / `mm`. Never key a conversion on a
parameter name (root `CLAUDE.md` rule): a config override with
`unit = "K"` gets `°C` by construction, whatever it is called. There is
no opt-out, as for GRIB; a client wanting kelvin converts back.

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
  report inside the interval — the snapshot's `[first_report, last_report]`
  overlap is only the prefilter, the rows decide (one store read lock per
  `datetime` query, a BTreeMap range probe per candidate; hourly SYNOP
  leaves gaps a narrow window falls into). That rule is `has_report_in`,
  shared with EDR `/locations?datetime=` (`location_time_filter`, #932,
  each listed instant matched exactly): keep the two lists identical;
  `sortby` ∈ `last_report,
  first_report, report_count, name` via `ds_core::feature::sort_features`;
  `data_version` = snapshot version.

## Lifecycle & runtime rules

- `new()` does a best-effort initial scan (local fixtures serve
  immediately; a failing source starts `Degraded`). `poll_loop()` on
  `poll_runtime()`: scan every `poll_interval_secs`, prune every 60 s,
  snapshot every 10 s when dirty. `live_health()` degrades after 3
  consecutive failed scans. Wired in `server/src/admin.rs` (`"bufr" =>
  ["edr", "features"]`, built by `new_with_state(…, state_store())`), boot
  spawn + shutdown in `main.rs` (`shutdown()` flushes the WIS2 state
  snapshot), `rotate_poll_loops!` on reload.
- `source.rs` uses `ds-storage` (`list` + `get_many`, bounded concurrency,
  16 MiB per file): **poll runtime only** (Critical Rule 7), never from a
  request handler, never inside `spawn_blocking`. `scan()` fetches in
  `FETCH_CHUNK`-sized `get_many` calls and streams each file to a sink
  (the engine decodes + ingests it right there), so a 50 k-file backlog is
  never resident at once — `get_many` buffers a whole batch, which is why
  its doc says "a chunk, not thousands of paths" (engine-odim convention).
  A listed file with a temporary basename (`ds_core::temp_files::
  is_temporary_key`, e.g. a publisher's in-progress `.name.bufr`) is never
  fetched (#1009).
- The fixture retention in `collections.d/obs-bufr-local.toml` is
  `P36500D` only because the fixtures are dated; production keeps `PT24H`.

## WIS2 mode (`[bufr.wis2]`, `src/wis2.rs`)

Read `crates/ds-wis2/CLAUDE.md` first (sessions, the 6×-per-cache duplicate
fact, download policy). Engine-side specifics:

- `new()` does no network — the pipeline starts in `poll_loop()` (the only
  entry point guaranteed to run on `poll_runtime()`); the collection boots
  `Degraded("connecting to WIS2 broker")` and `/health` overrides it live.
- **Most SYNOP payloads arrive inline** in the notification (a few hundred
  bytes of BUFR), so the common path never opens an HTTP connection;
  link-only producers (il-ims, us-noaa ship) are downloaded by ds-wis2.
  Each accepted payload goes through the same `ingest_bytes_keyed` as a
  scanned file.
- **Deletions:** the `(station, time)` rows every `data_id` produced are
  remembered (bounded, 200 k) so a `rel=deletion` withdraws exactly those
  rows. The store keys rows by `(station, time)` alone, so ownership is
  tracked **per key** (`Produced::owner`) and moves to the latest producer:
  when a second `data_id` re-produces a key (overlapping bulletins, a
  correction under a new id) the row is replaced and a later deletion of
  the first, stale `data_id` leaves it alone; deleting the owner removes it.
- **Health:** `Ready` = subscribed ∧ not disconnected for longer than
  `degrade_after_secs` ∧ a notification accepted within `stale_after`
  (default PT2H — hourly SYNOP with slack) ∧ at least one report ever
  decoded (fresh notifications whose payloads all fail to decode are
  `Degraded`, not green-with-nothing-served) ∧ past the warm-up (below).
  A quiet CAP feed is healthy; a quiet observation feed is not, hence the
  extra knob. Restored reports do not count as decoded: `probed` means
  this process's pipeline produced one.
- **Lifecycle:** `Wis2Source::run` is a `'session` loop like engine-cap's
  `wis2_loop` — if the pipeline cannot start or ends on its own it is
  marked disconnected (so `/health` degrades) and respawned after 30 s;
  an unchanged-config reload reuses the engine, so nothing else would
  restart it. The prune/snapshot tickers sit ahead of the message arm
  under `biased` so a backlog replay cannot starve them.
- Metrics: the shared `wis2_*` families (labelled by collection) plus the
  `bufr_*` ingest counters; `bufr_files_total` counts payloads here.
- The notification's `wigos_station_identifier` / Point geometry are NOT
  used: the decoded BUFR is the authority for identity and position, so a
  producer whose notification metadata disagrees with its data cannot
  split a station in two. Subsets without an id or position are skipped
  and counted (`bufr_reports_total{result="skipped"}`, ~0.1 % live).
- Without a state snapshot a boot starts empty until the next synoptic
  hour (H+20 typically); there is no Global Cache backfill (follow-up).

## Persistence across restarts (WIS2 mode, #1002, `src/persist.rs`)

Only WIS2 mode persists: nothing replays a broker's past notifications,
so the retention window (~270k reports globally) was lost at every
restart and took a day to refill. A `data_path` source re-reads its files
at boot and ignores the state store.

- With `[server] state_dir` the engine snapshots the store as one blob
  under `<id>.bufr` through `ds_core::state` (an `Arc<dyn StateStore>`,
  never a path; the file backend writes `<state_dir>/<id>.bufr.state`; see
  `crates/server/CLAUDE.md`), engine-cap's design: written on the poll
  runtime from the 10 s snapshot tick when `state_revision()` moved (the
  metadata `version`, bumped after every store change, plus the warm-up
  clock starting), at most every five minutes, an unchanged store every
  quarter of `warmup` (5 min–1 h, `state_policy`), and always from
  `shutdown()`. The encode runs under the store's READ lock, straight from
  the rows (no copy): requests keep reading, and the only writer is the
  same poll loop.
- Restored by `new_with_state` before the first metadata build, so the
  restored reports are served at once (still `connecting` until the session
  is up). Restore applies the CURRENT config: `ObsStore::prune` drops rows
  older than `retention` and stations beyond `max_stations`, and snapshot
  columns are mapped onto the current parameter table by their whole
  definition (name, descriptors, source unit, stored unit, period): a
  column the config dropped or redefined loses its values (logged), a new
  one is missing in restored rows. Never map by name alone — a changed unit
  or period would serve old values under the new meaning.
- Anything wrong with the snapshot (unreadable, not gzip, truncated, CRC,
  another `format`/`version`/`collection`, a bad value, unsorted or
  mismatched times, a duplicate station) rejects the WHOLE snapshot: WARN +
  cold start, and the next write replaces it.
- **Format** (measured in the module doc and `realistic_store_snapshot_size`):
  gzip (fast level, flate2 already in the tree via ds-wis2) over JSON,
  stations sorted by id (same state ⇒ same bytes); per station its row
  `times` in epoch seconds and each row as ONE string of comma-separated
  values, missing = empty, trailing missing dropped (`"8.2,-1.5,,1013.2"`).
  Values are written with Rust's shortest round-trip `f32` `Display` and
  read with `f32::from_str` — bit-exact, never through `f64`; station
  coordinates use serde_json's `float_roundtrip` (enabled in `Cargo.toml`).
  270k reports ≈ 7.7 MB, ~0.25 s to encode, ~0.18 s to decode (release).
  Keep the `BufWriter` in front of the `GzEncoder`: serde_json writes
  few-byte fragments, and deflating each made the encode 4× slower.
  `persist::VERSION` bumps on any non-additive change.
- The `rel=deletion` index (`Produced`) is NOT persisted: it covers only
  the last ~200k `data_id`s (≈ 5 h), whose ~100-character ids alone are
  about as large as the store's whole JSON, and observation feeds rarely
  withdraw. A deletion of a
  `data_id` received before the restart is a no-op; its rows age out.
- **Warm-up health.** A store that began filling less than `[bufr.wis2]
  warmup` ago reports `LiveStatus::WarmingUp` — `/health` "warming up after
  cold start: N reports received", degraded — wherever it would otherwise
  be `Ready` (a broker blip inside `degrade_after_secs` stays warming). The
  default is the collection's `retention`, not CAP's fixed PT24H: the store
  is complete once it holds a full retention window, and observations are
  never republished (the two agree at the default PT24H). The clock
  (`WarmupClock` in `wis2.rs`) starts on the first snapshot tick with the
  subscription up and travels in the snapshot: a restart mid warm-up keeps
  warming; a snapshot written longer than `warmup` ago keeps the rows still
  inside `retention` but restarts the warm-up (`WarmupCause::LongOutage`,
  "warming up after a long outage", written back at the first tick).
  Without a state store every start is cold.
- **The outage threshold is `warmup` (CAP's rule), and the reports
  published while the server was down are never filled in.** With the
  default `warmup` = `retention`, an outage shorter than `retention` is
  `Ready` at once with that gap in the window until it ages out, and a
  longer one leaves practically nothing inside `retention` to restore
  (only rows up to the 1 h future slack). Set `warmup` shorter than
  `retention` for a long outage to show as "warming up after a long
  outage".
- A reload that rebuilds the collection restores from the last periodic
  snapshot (≤ 5 min old); the replaced engine's `shutdown()` then writes
  its final state, which the new engine overwrites at its next write, so
  the reports the replaced engine received since its last periodic write
  are lost (as for CAP).

## Smoke test

```bash
cargo run -p server -- --collections=obs-bufr-local
curl 'localhost:8000/edr/collections/obs-bufr-local/locations'
curl 'localhost:8000/edr/collections/obs-bufr-local/locations/0-20000-0-02598?f=CoverageJSON'
curl 'localhost:8000/edr/collections/obs-bufr-local/position?coords=POINT(18.98 57.44)&f=CoverageJSON'
curl 'localhost:8000/edr/collections/obs-bufr-local/area?coords=POLYGON((10 55,30 55,30 70,10 70,10 55))&f=CoverageJSON'
curl 'localhost:8000/features/collections/obs-bufr-local/items?bbox=10,55,30,70'
curl -s localhost:8000/metrics | grep ^bufr_
# WIS2 mode (live): wait for the next synoptic hour (+~20 min)
cargo run -p server -- --collections=obs-synop-wis2
curl 'localhost:8000/edr/collections/obs-synop-wis2/locations'      # Swedish WIGOS ids
curl -s localhost:8000/metrics | grep -E '^(wis2_|bufr_)'          # duplicates ≈ 5× accepted
# Persistence: with [server] state_dir set, stop the server (Ctrl-C flushes
# <state_dir>/obs-synop-wis2.bufr.state), start it again: /locations answers
# at once, and the log says "restored N report(s) at M station(s)".
gunzip -c state/obs-synop-wis2.bufr.state | jq '.stations | length'
```
