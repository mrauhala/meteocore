# Projected WMS pan reuse — issue #375

Measured 2026-09-16 on macOS ARM64 with the **debug** server binary. These are
local regression measurements, not production latency forecasts. The baseline
was main `5652f85`; the changed build adds EPSG:3067/3035 tile reuse. Each run
starts a fresh server, but the OS file cache may already be warm.

## Workload

Committed fixture: `testdata/radar-tm35fin/radar_tm35_20260406T0640Z.tif`
(SHA-256 `76e9895718a10e7ef385d1b0eec9990cc820386164b690fe18dc6d6c958ba0ac`).

25 sequential 1024×768 PNG GetMaps at 500 m/pixel, each panned 16 km east.
Every bbox is different, so the exact-image cache cannot answer the next pan.
EPSG:3067 starts at `[100000,6500000,612000,6884000]`; EPSG:3035 starts at
`[4800000,4050000,5312000,4434000]`. Both use `radar_dbz`, a 128 MiB meta-tile
cache, and default source/decode cache settings. Direct controls set
`metatile_cache_mb = 0`. Pan percentiles exclude the first request; p95 is the
nearest-rank 95th percentile of the remaining 24 requests.

| Path | First request (ms) | Pan median (ms) | Pan p95 (ms) | Total (ms) | Tile hits / misses |
|---|---:|---:|---:|---:|---:|
| EPSG:3067, pre-change | 107.7 | 86.5 | 87.7 | 2185.7 | 0 / 0 |
| EPSG:3067, tile reuse | 192.7 | 60.1 | 83.3 | 1702.6 | 468 / 32 |
| EPSG:3067, disabled control | 99.1 | 86.9 | 89.4 | 2190.3 | 0 / 0 |
| EPSG:3035, tile reuse | 226.5 | 60.4 | 91.9 | 1770.8 | 456 / 32 |
| EPSG:3035, disabled control | 121.1 | 110.8 | 112.4 | 2775.3 | 0 / 0 |

The cached runs retained about 8 MiB and had no tile-budget declines. Cold
renders were slower because complete covering tiles must be rendered and
colourized. The benefit appears on overlapping requests: about 31% lower
median pan latency in 3067 and 45% in 3035 for this workload. This does not
establish a benefit for one-off, non-overlapping, or remote-source requests.
The existing zero-cache switch remains available.

## Pixel and cache checks

- Synthetic projected fields encode both axes and nodata. Assembled pixels
  exactly match direct pixels across tile seams, negative indices, non-square
  viewports and 125 m fractional pans in both CRSs.
- Real EPSG:3067 radar PNGs sampled at frames 0, 1, 8 and 24 were byte-identical
  between tiled and direct paths, including cold and warm rendering.
- All 25 real EPSG:3035 frames were decoded and compared: 0–4 of 786432 pixels
  differed per frame (maximum 0.00051%). There were 64 differing pixels total,
  including 24 alpha differences, across 19.66 million compared pixels.
  Tile-sized and viewport-sized projection grids can choose different nearest
  source pixels at boundaries; bitwise equality is not a general guarantee
  for reprojected or off-ladder views.
- Tests isolate identical tile indices across CRS, parameter, style, time,
  elevation, reference time and content version. HTTP tests prove panning
  reuse and the kill switch for both projected CRSs, and revised-content
  invalidation on all render paths. Existing Web Mercator latitude/alignment,
  cache-budget, deadline and discrete-palette regressions still pass.

## Reproduce

```sh
cargo build -p server
python3 scripts/bench_projected_wms.py > /tmp/pan-3067.json
python3 scripts/bench_projected_wms.py --cache-mb 0 > /tmp/direct-3067.json
python3 scripts/bench_projected_wms.py --crs EPSG:3035 > /tmp/pan-3035.json
python3 scripts/bench_projected_wms.py --crs EPSG:3035 --cache-mb 0 > /tmp/direct-3035.json
```

The script needs Python's standard library only. It starts a temporary local
server and shuts it down afterward, emitting request timings, PNG SHA-256s
and cache metrics as JSON. Use `--frames-dir /tmp/pan-frames` to retain PNGs.
For production-representative timings, build with `--release` and pass
`--binary target/release/server`; compare both paths with the same binary.
