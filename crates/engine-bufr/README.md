# engine-bufr

Serves WMO BUFR surface station reports (SYNOP, SHIP) as OGC API - EDR
station time series and OGC API - Features station points
(`engine_type = "bufr"`).

## What it solves

Weather services exchange surface observations as BUFR, a table-driven
binary format, published as files or WIS2 Global Broker notifications.
Clients want a station's recent values over HTTP, without a BUFR decoder.
This engine decodes the reports, keeps a sliding time window of them in
memory, and answers EDR and Features queries from that store.

## How it works

- **Source**: exactly one of
  - `data_path`: a local directory or an `s3://` prefix, listed recursively
    every `poll_interval_secs`. Only files not seen before, or whose size or
    modification time changed, are fetched. Hidden files and files in
    hidden folders (a publisher's temporary names) are skipped.
  - `[bufr.wis2]`: a WIS2 Global Broker subscription through `ds-wis2`.
    Most SYNOP notifications carry the BUFR message inline; others are
    downloaded from the notification's link.
- **Decoding** uses [`ds-bufr`](../ds-bufr/README.md). A file may hold
  several messages; one that fails to decode is counted and skipped.
- **Extraction** is by Table B descriptor, not by template, so any SYNOP or
  SHIP template works:
  - station id: the WIGOS id (`001125`–`001128`), else the block and
    station number as `0-20000-0-BBSSS`, else the ship call sign as
    `ship:<callsign>`;
  - report time: the first `004001`–`004006` values;
  - period: `004023`–`004026` set the period that qualifies the elements
    after them (precipitation, extremes, gusts);
  - a repeated element keeps its first non-missing value per period;
  - a subset without a station id, position or time is skipped and counted.
- **Merging**: reports of one station, time and position in one file or
  payload merge (a SYNOP and its national supplement); a later one replaces
  the stored row.
- **Store**: per station, one row per report time with one `f32` per
  parameter. Reports older than `retention` plus one hour, or more than an
  hour in the future, are rejected. Every 60 s, expired rows are pruned and
  the least recently reporting stations beyond `max_stations` are evicted.
  Station lists and extents come from a snapshot rebuilt every 10 s when
  the store changed.
- **Units**: each parameter names its Table B unit, served in the shared
  display units of `ds_core::units`: K as °C, Pa as hPa, `kg m-2` as mm,
  `kg m-2 s-1` as mm/h, `m2 s-2` as gpm and a WMO proportion as %. Other
  units are served as given.
- **WIS2 deletions**: a `rel=deletion` notification removes the rows its
  `data_id` produced. The last 200 000 `data_id`s are remembered.
- **Persistence (WIS2 only)**: with `[server] state_dir` set, the store is
  saved as gzip-compressed JSON to `<state_dir>/<collection id>.bufr.state`
  at most every five minutes when it changed, and at shutdown. A restart
  restores it under the current `retention` and `max_stations`; an
  unreadable or invalid file means a cold start. A `data_path` source
  re-reads its files instead.

## What it serves

| API | Query | Answer |
| --- | --- | --- |
| EDR | `locations` | One location per station; `datetime` keeps stations with a report in the interval. `/locations/{id}` is the station's `PointSeries`, 404 if unknown or empty in the window |
| EDR | `position` | Series of the nearest station within `position_radius_km`, else 404 |
| EDR | `area` | One `PointSeries` per station inside the polygon with data in the window; none gives an empty `CoverageCollection` |
| EDR | `radius` | The circle as a polygon, then `area` |
| Features | `/items` | One Point feature per station: `wigos_station_identifier`, `name`, `elevation`, `first_report`, `last_report`, `report_count` |

- EDR formats: CoverageJSON (default), CSV and HTML for every query; EDR
  GeoJSON for `locations`, `position` and `radius`; a PNG plot for
  `locations` and `position`. See [api-edr](../api-edr/README.md).
- Features `/items`: `bbox` matches the station point; `datetime` keeps
  stations with at least one report in the interval; `sortby` accepts
  `last_report`, `first_report`, `report_count` and `name`; every listed
  property is filterable. The feature id is the station id; with `edr` in
  `apis`, each feature has a `related` link to that EDR location.

Built-in parameters (`builtin_parameters = true`):

| Parameter | Descriptor(s) | Served unit |
| --- | --- | --- |
| `air_temperature`, `dew_point_temperature` | `012101`/`012004`, `012103`/`012006` | °C |
| `relative_humidity` | `013003` | % |
| `pressure` (station), `pressure_msl`, `pressure_tendency_3h` | `010004`, `010051`, `010061` | hPa |
| `wind_direction`, `wind_speed` | `011001`, `011002` | deg, m s-1 |
| `wind_gust` | `011041`, first value of any period | m s-1 |
| `precipitation_{1,3,6,12,24}h` | `013011` for that period; 24 h also `013023` | mm |
| `air_temperature_{max,min}_{12,24}h` | `012111` / `012112` for that period | °C |
| `visibility`, `snow_depth` | `020001`, `013013` | m |
| `cloud_cover_total` | `020010` | % |
| `present_weather` | `020003` | code table 0 20 003 |

## Configuration

```toml
[[collections]]
id = "obs-synop"
title = "Surface observations"
description = "SYNOP station reports decoded from BUFR"
engine_type = "bufr"
apis = ["edr", "features"]        # the only APIs this engine serves

[collections.bufr]
data_path = "data/synop"          # or a [collections.bufr.wis2] block, not both
retention = "PT24H"               # other fields and defaults: table below

[[collections.bufr.parameters]]   # replaces a built-in by name, or adds one
name = "precipitation_2h"         # [a-z0-9_]+
descriptors = ["013011"]          # Table B 0XXYYY, first match wins
unit = "kg m-2"                   # the Table B unit; served as mm
period_hours = 2                  # only for period-qualified elements
# label = "precipitation 2h"      # default: the name with _ as spaces
# observed_property = "..."       # default: the name
```

| Field | Default | Meaning |
| --- | --- | --- |
| `data_path` | – | Directory or `s3://` prefix of `.bufr`, `.bufr4`, `.bfr`, `.bin` or `.b` files |
| `wis2` | – | Shared `[….wis2]` subscription block (`Wis2Config` in `ds-core`): `topics` required, each starting with `cache/` or `origin/`; broker `mqtts://globalbroker.meteo.fr:8883` and public credentials by default |
| `poll_interval_secs` | `60` | `data_path` scan interval |
| `retention` | `PT24H` | How long reports are kept and the oldest report time accepted |
| `max_stations` | `50000` | Stations held |
| `stale_after` | `PT2H` | WIS2: degraded when no notification arrived this long |
| `position_radius_km` | `25.0` | EDR `position` search radius |
| `builtin_parameters` | `true` | `false` serves only `[[bufr.parameters]]` (at least one required) |

Runnable examples: `collections.d/obs-bufr-local.toml` reads the fixtures
(its long `retention` keeps the dated fixtures in the window), and
`collections.d/obs-synop-wis2.toml` subscribes to WIS2 SYNOP topics.

```bash
cargo run -p server -- --collections=obs-bufr-local
curl 'localhost:8000/edr/collections/obs-bufr-local/locations'
curl 'localhost:8000/edr/collections/obs-bufr-local/position?coords=POINT(18.98%2057.44)'
curl 'localhost:8000/features/collections/obs-bufr-local/items?bbox=10,55,30,70'
```

## Health and metrics

- `data_path`: degraded after three consecutive failed scans.
- WIS2: degraded while connecting or not yet subscribed, once the broker
  has been disconnected for `degrade_after_secs` (default 120), when no
  notification arrived within `stale_after`, and until the first report
  decodes. After a cold start, or a restore from a snapshot older than
  `warmup`, it reports "warming up" (degraded) for `[bufr.wis2] warmup`,
  by default the shorter of `retention` and `PT24H`.
- `/metrics`: `bufr_stations`, `bufr_reports_stored`,
  `bufr_scans_total{result}`, `bufr_files_total`,
  `bufr_reports_total{result}`, `bufr_decode_failures_total{reason,kind}`,
  and the shared `wis2_*` families.

## Limits

- `ds-bufr` implements operators 201–204, 206 and 208, with 203 only in
  uncompressed data. A message using any other operator (such as 207 or
  22x) fails and is counted.
- Local (national) descriptors are registered only for DWD (centre 78,
  local table versions 1–8). Another centre's unknown local element fails
  its message.
- Series have no vertical axis. Upper-air profiles are not served.
- Ships are keyed by call sign and placed at their newest report. Vessels
  reporting the anonymised call sign `SHIP` share one station.
- A 400 answers a response over 500 000 values (stations × times ×
  parameters), a non-rectangular polygon over 20 million station × vertex
  tests, and a `radius` circle that crosses the antimeridian or contains a
  pole.
- `data_path` considers at most 50 000 listed objects per scan and skips
  files over 16 MiB.
- WIS2 has no backfill: reports published while the server was down are
  never filled in, and deletions of `data_id`s received before a restart
  have no effect.

## Testing

`cargo test -p engine-bufr` runs against three fixture sets whose expected
values come from ecCodes, not from this decoder:

- `testdata/bufr-synop/`: eight SYNOP files captured from the WIS2 Global
  Broker, used end to end (EDR, Features, retention).
- `testdata/bufr-regressions/`: one small live message per decoder edge
  case, each named in its README.
- `testdata/bufr-decoder/`: synthetic format fixtures generated with ecCodes.

The ignored `realistic_store_snapshot_size` test measures the WIS2 snapshot
of a global store; run it with `--release` and `-- --ignored --nocapture`.

Contributor rules: [CLAUDE.md](CLAUDE.md).
