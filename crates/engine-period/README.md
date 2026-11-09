# engine-period

Rolling-window statistics over another collection's frames, served through
WMS 1.3.0, OGC API - Maps, OGC API - Tiles and OGC API - EDR
(`engine_type = "period"`). Radar rain accumulation first: how much rain
fell in the last hour, three hours, day.

## What it solves

A radar composite serves each 5-minute frame as the source publishes it, so
"how much rain fell in the last 24 hours" means fetching and summing 288
frames on the client. This derived collection keeps the answer: one grid per
window, updated as frames arrive, served as a layer and an EDR parameter of
its own.

## How it works

- **Source.** `source` names a gridded observation collection in the same
  config, whatever APIs it lists: GeoTIFF, satellite, Zarr, NetCDF, or a
  station grid. Not a nowcast or another period collection, and not a
  source with model runs (forecast windows are a follow-up). Not an ODIM
  composite either: it serves undetect (scanned, no echo) as nodata, so a
  cell dry all window would be null instead of 0 mm, `mean` would average
  the rainy frames only and coverage would count dry frames as gaps; it is
  refused at load until the source can tell undetect from nodata. A
  GeoTIFF composite that stores no echo as a low dBZ value is fine.
  `parameter` is the source parameter, the source's default when omitted.
  The engine holds the source's engine: any edit to the source
  collection's config rebuilds it, and the period collection's windows are
  then summed again from scratch.
- **Windows.** Each of `windows` (ISO 8601 durations) ends at the source's
  latest frame `T` and holds the frames in `[T − L, T)`. A frame counts for
  its spacing to the next frame, capped at the source's cadence (`cadence`,
  or the median spacing of its newest frames), so a missed frame leaves its
  interval uncovered instead of stretching its predecessor over it. A
  window that is not a multiple of the cadence holds the frames that start
  in it: `PT21M` over 5-minute frames sums 20 minutes (coverage 0.95, so
  served), `PT12M` sums 10 (0.83, not served at the default). Sums are
  never scaled up for coverage: a window at 0.9 sums the rain of 90% of
  it. A window shorter than the cadence never holds a frame: refused at
  load when `cadence` is configured, and explained in `/health` when it is
  estimated.
- **Rain rate.** With `rain_rate = { a, b, min_dbz }` (default
  Marshall–Palmer, `a = 200`, `b = 1.6`, `min_dbz = 7`), reflectivity in dBZ
  becomes rain rate in mm/h, `R = (10^(dBZ/10) / a)^(1/b)`, before the
  statistic; below `min_dbz` (0.1 mm/h by default) an echo counts as dry,
  and nodata stays nodata. The source's unit must be dBZ.
- **Statistics.** `sum`: each frame's rate times the hours it counts for,
  summed: mm from a rain rate, or a per-hour rate of the source's own
  (`mm/h` → `mm`); any other unit is a load error. `mean`: the mean of the
  frames' values at the cell over the frames with a value there.
  `count_above`: the frames at or above `threshold` (mm/h with `rain_rate`,
  else the source's unit). A cell without a value in any frame of the
  window is nodata. `max` and `min` are refused: a running maximum cannot
  take a leaving frame out (follow-up).
- **Coverage.** A window's coverage is the time its frames count for over
  its length: on an even cadence the share of its frames present. Below
  `min_coverage` (default 0.9) the window serves nothing; at or above it,
  `coverage_<window>` serves the share at every cell, scaled down where some
  frames had no value at that cell.
- **Running grids.** Per window an `f32` accumulator and a `u16` count of
  the frames with a value at each cell: 6 bytes a cell, whatever the
  window's length; no frame is kept. Each poll, on the background poll
  runtime, reads the frame that just got its successor (the newest frame
  counts once the next one arrives and fixes its duration) and adds it, and
  reads each window's leaving frame again and subtracts it with the same
  contribution bit for bit.
- **Strips.** A frame is never read whole: it is read in strips of whole
  rows of the working grid, at most `read_strip_cells` cells each (default
  4 Mi, at least one row), top to bottom, each strip's box on the grid's own
  row edges. Each strip is folded into every window and rebuild that takes
  the frame, under the window's write lock, and dropped before the next is
  read. A cell gets the same operations in the same order as from a whole
  frame, so the grids are the same bit for bit, whatever the strip size.
  A frame whose read fails part way breaks the windows it reached (part of
  a frame cannot be taken out again); they are rebuilt.
- **Failed reads.** A frame that cannot be read is not added: it is read
  again at the next poll, then after 2, 4 and at most 8 polls, and the
  coverage shows the gap meanwhile. A window that already holds it keeps
  it, and reads it again when it leaves; one failed read breaks only the
  window that had to take the frame out then, never the longer windows
  holding it, and a re-sum waits for it rather than swapping in grids
  without it.
- **Revised frames.** Each strip a window adds is digested, and a strip is
  taken out only if it reads back with the same digest. A frame the source
  revised in place since (a station grid's late observation, a GeoTIFF
  object republished) is never subtracted with its new values: the windows
  holding it break when it leaves and are rebuilt from the revised frames.
- **Versions while folding.** Before a poll changes a window's grids, the
  window serves a fresh in-flux content version, and once the poll has
  published, another fresh one. A map request that misses the cache while
  strips are being folded in can draw the window part way through its
  update, but its image is cached under the in-flux version, which no
  request asks for after the poll: the window's last consistent image stays
  in the cache under its own key, and the next one is rendered afresh.
- **Re-sum.** Every window is summed again from scratch after it has slid
  four times its length, into a second set of grids with what is left of the
  poll's read budget, newest frames first, and swapped in, so `f32` rounding
  cannot build up. One such re-sum runs at a time. The same rebuild builds a
  new window, and one whose frame could not be taken out (the source no
  longer has it, the read failed, or it changed). A broken window's grids
  are dropped before its rebuild's are allocated, so a burst that breaks
  every window still holds at most one window's grids more than usual.
- **Late frames.** A frame arriving inside a window is added, and its
  predecessor retimed, at the same end. The window's version moves, which
  keys the map caches (`content_version_for`); a poll that changes nothing
  moves nothing. A source whose content version moves (frames revised in
  place) gets every window re-summed, one at a time.
- **A source that cannot be summarised** (it loses its grid, or gains
  model runs) drops every window: nothing is served and `/health` says
  why, until it can be again.

## Serving

- One parameter per window, `<base>_<window>` (the window as configured,
  upper case), then `coverage_<window>` for each: with `rain_rate`, `sum` is
  `precipitation_amount_PT1H` in mm, on the precipitation palette (0–50 mm)
  by default; `mean` is `rain_rate_mean_PT1H` in mm/h; `count_above` is
  `rain_rate_count_above_PT1H`. Without it the base is the source
  parameter's name with `_sum`, `_mean` or `_count_above`.
- Each window's time axis is its end, `[T]`, or nothing while it is not
  served (see [Latest end only](#latest-end-only)). WMS `TIME`, Maps/Tiles
  `datetime` and EDR `datetime` select it as any other timestep:
  latest-not-after. A not-yet-served window renders empty.
- The working grid is a regular CRS84 grid over the source's WGS84 extent
  at the source's own cell counts, while they fit `max_pixels` (default
  40 000 000, at most 64 000 000), so a 36 M-cell composite keeps its cell
  count. Past it, the longer axis is halved until the grid fits, and a
  frame is read as the source's map path renders that size: a GeoTIFF's
  overview. Only a CRS84 (WGS84 lat/lon) source's grid is its native grid,
  cell for cell. A projected source (EPSG:3067 TM35FIN, an LAEA
  composite) is resampled by its own map path onto the CRS84 grid with its
  cell counts over its WGS84 bounding box, so a cell is the source pixel
  under its centre, and corners outside the projected domain are nodata.
  A cell then equals a position query on the source only for a CRS84
  source at native size. The load logs the grid chosen and its bytes.
  Maps sample it nearest-cell.
- EDR: `position`, `area` and `radius` read the grid the map draws, at the
  cell under each point; an area at the grid's resolution while it fits the
  value budget, evenly coarser past it. Without `datetime`, the latest end;
  a `datetime` that does not contain it is a 404.

## Memory and I/O

| | |
|---|---|
| Grids | 6 B × cells × windows, plus at most one window's while a re-sum or rebuilds run, also when every window breaks at once (`period_grid_bytes`: the most the last poll held) |
| Frame values held | one strip of one frame at a time, whatever the grid's size and the source's type |
| Reads per source step | 1 + windows, plus a re-sum's quarter frame per window |
| Reads per poll | at most `max_reads_per_cycle` (default 16, at least windows + 1) |

Sizing, windows of 1, 3 and 24 hours:

| | 4 Mpx grid | 36.3 M cells, a 4963 × 7316 composite at its cell counts |
|---|---|---|
| Grids, one window | 24 MB | 218 MB |
| Grids, the three windows | 72 MB | 654 MB |
| Grids while a re-sum runs | 96 MB | 871 MB |
| Strips per frame read | 1 (2000 × 2000) | 9 of 845 rows |
| Source tile of one strip, boxed `f64` | 64 MB | 67 MB |
| Its frame values, `f32` / raw bytes | 16 / 4 MB | 17 / 4 MB |

Grids are resident, `period_grid_bytes`, which the memory plan reserves.
Strips are transient, during a poll on the background runtime, one at a
time: the source's tile and its frame values, about 84 MB at the default
`read_strip_cells` whatever the grid's size, plus the source's own buffers
for one strip's read. Which form a tile takes is the source's: a local
8-bit GeoTIFF with a nodata byte gives raw bytes, kept as 1 B/cell; a
GeoTIFF read from object storage or HTTP, 8-bit or not, gives boxed `f64`
(16 B/cell), converted to 4 B/cell as it is read. Lowering
`read_strip_cells` lowers the peak and adds source reads per frame.

The source must keep the longest window plus one frame: a source that keeps
less drops frames still inside a window. The window then breaks once and
waits, and `/health` says what it needs.

## Configuration

```toml
[[collections]]
id = "radar-rain"
engine_type = "period"
apis = ["wms", "maps", "tiles", "edr"]

[collections.period]
source = "radar"                     # gridded observations in the same config
parameter = "reflectivity"           # default: the source's default
statistic = "sum"                    # sum | mean | count_above
windows = ["PT1H", "PT3H", "PT24H"]  # one parameter per window
rain_rate = { a = 200.0, b = 1.6, min_dbz = 7.0 }
min_coverage = 0.9
# threshold = 1.0                    # count_above only
# cadence = "PT5M"                   # default: the source's median spacing
# poll_interval_secs = 30
# max_pixels = 40000000              # 65 536 ..= 64 000 000; native while it fits
# max_reads_per_cycle = 16
# read_strip_cells = 4194304          # cells per source read: a frame in strips of whole rows
```

An unknown source, parameter or statistic, a bad window, `rain_rate` on a
source not in dBZ, or `sum` of a unit that is not a rate fails the
collection at load. The runnable example is
`collections.d/radar-no-rain-accumulation-local.toml`, over the committed
MET Norway frames in `testdata/radar`.

## Metrics

`period_grid_bytes{collection}` (in the dashboard's memory stores),
`period_polls_total{collection}`, `period_frame_reads_total{collection}`.

## Latest end only

Each window serves one time: its end at the source's latest frame. When the
next frame arrives the window moves on, and the accumulation that ended at
the previous frame is gone: it is not kept, and it is not computed again.
A client cannot animate accumulations, or ask for the hour that ended at
an earlier frame: a `datetime` that does not contain the latest end is a
404 in EDR, and maps render the latest end for any `TIME`, the one value
of the layer's time dimension. Keeping earlier ends would cost one grid each;
computing them on demand is a follow-up.

## Not yet (#1379)

Past window ends computed on demand; `max` and `min`; windows over a
forecast run's valid times; clock-aligned windows (00–24 UTC).
