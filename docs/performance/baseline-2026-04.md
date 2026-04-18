# MeteoCore load-test baseline — 2026-04-18

Baseline for issue [#93](https://github.com/mrauhala/meteocore/issues/93). Future optimization work should measure against these numbers.

## Environment

| | |
|---|---|
| Target | `https://meteocore.app.meteo.fi` (production) |
| Client | Locust 2.43.4, running from the maintainer's workstation over the public internet |
| Test driver | [`locustfile.py`](../../locustfile.py) (unchanged) |
| Grafana | [`https://grafana-meteocore.app.meteo.fi/`](https://grafana-meteocore.app.meteo.fi/) (auth required) |
| Server binary | whatever was deployed on 2026-04-18 ~20:00 UTC |
| Collections exercised | 7 radar layers (`fmi-/smhi-/dmi-/met-/dwd-radar-composite-dbz`, `opera-reflectivity`, `opera-precipitation`) |
| Render pool | 12 permits (`render_semaphore_total = 12`) |
| Tile cache capacity | 2304 MiB |
| Rendered cache capacity | 512 MiB |
| Grid (COG) cache capacity | 512 MiB |

Client-side latencies include transatlantic network RTT; compare them to server-side histograms for engine-internal cost.

## Test matrix (as executed)

| # | Test | Users | Spawn | Duration | Reason for deviation |
|---|---|---|---|---|---|
| 1 | Cache-warm steady state | 10 | 2/s | 5 min | — |
| 2 | Sustained load | 50 | 5/s | 5 min | — |
| 3 | Burst | 100 | 20/s | 2 min | — |
| 4 | Cold start | 50 | 5/s | 5 min | **Not run** — requires a server restart to drop caches; can't be triggered remotely. See "Running a cold-start test" below. |

All tests back-to-back with no pause; tests 2 and 3 inherit a pre-warmed cache from their predecessor.

## Headline numbers

| Test | Reqs | Failures | Throughput | p50 (client) | p95 (client) | p99 (client) | max |
|---|---|---|---|---|---|---|---|
| 10u warm   | 2 155 | 0 | **7.2 req/s**  | 68 ms  | 420 ms | 920 ms | 1.65 s |
| 50u sustained | 10 671 | 0 | **35.6 req/s** | 82 ms  | 390 ms | 770 ms | 2.30 s |
| 100u burst | 8 433 | 0 | **70.3 req/s** | 97 ms  | 480 ms | 850 ms | 2.50 s |

**Zero failures across 21 259 requests.** No load-shedding observed at 100 concurrent users.

Throughput scales roughly linearly from 10 → 50 → 100 users (7.2 → 35.6 → 70.3 req/s, a 10× increase over a 10× user increase), with p95 latency only rising from 420 ms → 480 ms. The server isn't close to saturation at 100 users.

## Server-side latency (from Prometheus histograms, full test window)

Delta between `/metrics` snapshots taken immediately before test 1 and immediately after test 3:

| Endpoint | Reqs | Avg | ≤25 ms | ≤100 ms | ≤500 ms | ≤1 s |
|---|---|---|---|---|---|---|
| `GET /wms` (GetMap + GetCapabilities + GetLegendGraphic mixed) | 14 281 | **19.3 ms** | 88.8 % | 94.6 % | 99.3 % | 99.9 % |
| `GET /tiles/…/tiles/{tms}/{z}/{x}/{y}` | 5 682 | **5.7 ms** | 92.6 % | 100.0 % | 100.0 % | 100.0 % |
| `GET /maps/collections/{id}/map` | 2 134 | **3.7 ms** | 94.2 % | 99.6 % | 100.0 % | 100.0 % |
| `GET /tiles/…/styles/{style}/tiles/…` | 1 373 | **5.3 ms** | 93.4 % | 100.0 % | 100.0 % | 100.0 % |
| `GET /metrics` | 910 | 0.4 ms | 100.0 % | 100.0 % | 100.0 % | 100.0 % |
| `GET /health` | 753 | 0.5 ms | 100.0 % | 100.0 % | 100.0 % | 100.0 % |
| `GET /tiles/collections` | 720 | 0.7 ms | 100.0 % | 100.0 % | 100.0 % | 100.0 % |

Server-side average is 4–19 ms across all rendering endpoints. Client-side p50 of 70–100 ms therefore implies ~60–80 ms of network RTT + TLS — **not** engine cost. Most wall-clock time on this baseline is network, not rendering.

## Cache behaviour

Hit rates computed from deltas of `*_cache_hits_total` / `*_cache_misses_total` between snapshots.

| Cache | Test 1 (10u) | Test 2 (50u) | Test 3 (100u burst) |
|---|---|---|---|
| Tile cache (raw COG tile bytes) | 90.9 % (2 242 h / 224 m) | 89.4 % (873 / 103) | 76.5 % (13 / 4) |
| Rendered cache (PNG output) | 44.9 % (1 633 / 2 002) | 67.6 % (6 115 / 2 932) | 71.0 % (5 077 / 2 075) |
| Grid cache (EDR point queries) | 0 / 0 | 0 / 0 | 0 / 0 |

- **Tile cache** hit rate stays around 90 % once warm. The test-3 numbers (13 hits, 4 misses) are small because most tile traffic is absorbed upstream by the rendered cache; only rendered-cache misses fall through to the tile cache.
- **Rendered cache** warms from 45 % → 71 % hit rate over the three tests as the working set of (bbox, width, height, style) combinations gets exercised. This is the cache that determines user-visible latency.
- **Grid cache** sees zero activity — the locust scenarios don't hit EDR position queries. If you want to exercise it, add a position-query task.

### Cache utilisation after test 3

| Cache | Bytes | Capacity | Entries | % full |
|---|---|---|---|---|
| Tile | 12.9 MiB | 2304 MiB | 325 | 0.6 % |
| Rendered | 490.4 MiB | 512 MiB | 3 314 | **95.7 %** |
| Grid | 308.9 MiB | 512 MiB | 39 | 60.3 % |

The **rendered cache is running at its capacity ceiling** (490 / 512 MiB). Once past the first few minutes of warmup, new renders evict old entries LRU-style. The tile cache, in contrast, has ~2.3 GiB of headroom and is barely used — the capacity there is effectively wasted given current demand. Reasonable optimisation direction: increase `rendered_cache_capacity_bytes` and/or decrease `tile_cache_capacity_bytes`, but **do not do so without re-running this baseline to confirm** that a larger rendered cache actually improves hit rate for realistic traffic (it may already have reached the long-tail floor).

### Render semaphore

`render_semaphore_total = 12`, `render_semaphore_available = 11` at the post-test-3 snapshot. We didn't catch saturation in a snapshot; the Grafana utilisation panel is the authoritative view. **Given p99 latencies well under 1 s and zero failures at 100-user burst, there is no evidence the semaphore was a bottleneck during this run.**

## Per-endpoint client-side percentiles (test 3, 100-user burst)

Worst offenders only; full CSVs in `/tmp/baseline-2026-04/t3_burst_100u_stats.csv` on the maintainer's machine.

| Endpoint | p50 | p95 | p99 | max |
|---|---|---|---|---|
| `/wms GetMap EPSG:3857` | 200 ms | 750 ms | 1 200 ms | 2 500 ms |
| `/wms GetMap EPSG:4326` | 140 ms | 450 ms | 800 ms | 1 400 ms |
| `/wms GetMap CRS:84` | 120 ms | 400 ms | 600 ms | 820 ms |
| `/maps GetMap` | 130 ms | 350 ms | 550 ms | 1 100 ms |
| `/tiles z=4` (lowest zoom, largest area) | 62 ms | 170 ms | 260 ms | 540 ms |
| `/tiles z=10` (city) | 59 ms | 160 ms | 210 ms | 500 ms |

EPSG:3857 GetMap is consistently the slowest path at every percentile — it's weighted `@task(5)` in the locustfile (the most-sampled task), uses the widest bbox variety, and has the largest WIDTH/HEIGHT range (up to 2048 px). That matches expectation.

## Traffic mix during tests

From `http_requests_total` delta (25 854 server-side requests in the test window; 21 259 from locust, ~4 600 from background traffic — polling, Prometheus scrapes, other clients):

- 55 % `/wms` (WMS-weighted locustfile scenarios)
- 22 % tile requests
- 8 % `/maps`
- 15 % metadata (`/health`, `/metrics`, `/tiles/collections`, `/wms?...=GetCapabilities`, `/wms?...=GetLegendGraphic`)

## Running a cold-start test

Because I can't restart the remote server, the cold-start scenario from the issue wasn't executed. To do it manually:

```bash
# 1. On the server host, restart MeteoCore to drop caches
systemctl restart meteocore   # or whatever the deployment uses

# 2. Immediately kick off the load from a workstation
/tmp/locust-env/bin/locust -f locustfile.py --host https://meteocore.app.meteo.fi \
  --users 50 --spawn-rate 5 --run-time 5m --headless \
  --csv cold_50u --only-summary

# 3. Compare the first-minute vs last-minute p95 in cold_50u_stats_history.csv
#    to estimate cache warmup time.
```

Expected signal: the first ~30–60 s should show degraded p95 (server-side render cost dominates), converging to the warm baseline as the rendered cache fills.

## Reproducing this run

```bash
# from repo root
python3 -m venv /tmp/locust-env
/tmp/locust-env/bin/pip install locust

curl -s https://meteocore.app.meteo.fi/metrics > /tmp/metrics_before.txt

/tmp/locust-env/bin/locust -f locustfile.py --host https://meteocore.app.meteo.fi \
  --users 10 --spawn-rate 2 --run-time 300s --headless --csv t1 --only-summary

curl -s https://meteocore.app.meteo.fi/metrics > /tmp/metrics_after_t1.txt

/tmp/locust-env/bin/locust -f locustfile.py --host https://meteocore.app.meteo.fi \
  --users 50 --spawn-rate 5 --run-time 300s --headless --csv t2 --only-summary

curl -s https://meteocore.app.meteo.fi/metrics > /tmp/metrics_after_t2.txt

/tmp/locust-env/bin/locust -f locustfile.py --host https://meteocore.app.meteo.fi \
  --users 100 --spawn-rate 20 --run-time 120s --headless --csv t3 --only-summary

curl -s https://meteocore.app.meteo.fi/metrics > /tmp/metrics_after_t3.txt
```

## Conclusions

1. **Current production configuration comfortably handles 100 concurrent users** with zero failures and sub-second p99 latency.
2. **Server-side render cost is 4–19 ms average.** Client-visible latency is dominated by network, not the engine.
3. **The rendered cache is the binding cache.** It's running at 96 % capacity and its hit rate (45 % → 71 %) explains the warmup curve. The tile cache is vastly oversized for current traffic.
4. **No evidence of render-semaphore contention** at this load. Whether it becomes the bottleneck at higher user counts is untested.
5. **Cold-start behaviour is still unmeasured.** The first-minute p95 under cold cache is the missing data point.

Future optimisation work should: (a) produce its own baseline run, not compare against this one across different deployments; (b) watch the rendered-cache hit rate as the primary indicator; (c) include a cold-start measurement.
