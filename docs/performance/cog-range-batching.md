# COG range batching (#46)

Remote bbox reads support bounded coalescing of nearby **cache misses**, shared
by the object-store and direct-HTTP paths. Local mmap reads are unchanged.

`MC_COG_RANGE_BATCH_TILES` defaults to **1 (disabled)**, clamps to 1–16, and is
read once per process. Set it to 4 to evaluate four-tile batches. The existing
`MC_COG_TILE_CONCURRENCY` still bounds simultaneous jobs (default 16). This is
an opt-in request-count optimization; measurements below do **not** justify
turning it on universally or claiming the old issue's estimated latency gain.

Each merged range has these limits:

- At most 1 MiB total span; a larger individual tile retains its existing read
  and 64 MiB encoded-input validation.
- At most 4 KiB between consecutive ranges and 10% extra bytes across all gaps.
- Only requested cache misses are candidates. Sparse ranges remain separate;
  small intervening ranges, including cached bytes, may be read as bounded gaps.
- The merged input reserves decode memory before I/O, alongside the existing
  per-tile input/decoder/output reservations. Tile copies inserted into the
  compressed LRU own just their bytes, so a small cache entry cannot pin an
  unaccounted whole batch. The merged input is released after its tiles decode.
- The existing absolute deadline follows all Rayon work, reads and retries.
  Rejected or short batches fall back to individual reads. Admission/deadline
  failures propagate; they never become transparent gaps. Invalid metadata is
  rejected before any tile I/O. Ordinary tile failures keep the existing retry
  and nodata policy.

## Current OPERA comparison — 2026-09-16 UTC

Source: public CloudFerro `openradar-24h`,
`2026/09/16/OPERA/COMP/OPERA@20260916T2100@0@DBZH.tiff`, 5,081,686 bytes,
ETag `4bc1b26fe95e0f93bf80f4c13ab8a1ff`.

The ignored `benchmark_cold_cog_ranges` test compares the existing individual
parallel reads against the new grouped path on the **same decoded tiles**.
Every comparison asserts exact equality of every decoded value. Both arms use
no application compressed cache; the HTTP connection pool is shared and the
origin's own cache state is unknown. Six pairs alternate execution order, with
16 fetch workers and debug builds on macOS. Header discovery is outside timing;
fetch and decode are inside. These are source-window timings, **not complete
WMS render timings**. Runs at different batch limits are separate experiments.

Dense central window: 56 full-resolution source tiles, 2,221,216 useful bytes.

| Max tiles/batch | Requests, individual → batch | Extra bytes | Median individual | Median batch |
|---|---:|---:|---:|---:|
| 2 | 56 → 28 | 224 (0.010%) | 731.7 ms | 936.2 ms |
| 4 | 56 → 14 | 336 (0.015%) | 670.5 ms | 757.2 ms |
| 16 | 56 → 4 | 416 (0.019%) | 772.2 ms | 934.9 ms |

Sparse sample: 21 tiles, 421,566 bytes. **All settings kept 21 requests and
transferred exactly the same bytes**; no large intervening tiles were fetched.
The identical-I/O controls had substantial network variation (e.g. four-tile
experiment medians 313.0 vs 604.2 ms). Raw measurements are in
[cog-range-batching.csv](cog-range-batching.csv).

The request-count reduction is demonstrated; a latency improvement is not.
Larger batches can lose parallelism and incur longer individual transfers.
Keep the default disabled, evaluate against each deployment's RTT/throughput,
source tile sizes, cache-miss pattern and concurrency, and leave #46's default
rollout/performance gate open. Warm full-cache hits still perform zero I/O.

## Reproduce

Choose a recent DBZH COG and obtain its size from the public bucket listing;
the example below expires with the source's 24-hour retention. The benchmark
requires network access and is intentionally ignored in normal CI.

```sh
MC_COG_BENCH_URL='https://s3.waw3-1.cloudferro.com/openradar-24h/2026/09/16/OPERA/COMP/OPERA@20260916T2100@0@DBZH.tiff' \
MC_COG_BENCH_SIZE=5081686 \
MC_COG_RANGE_BATCH_TILES=4 \
cargo test -p engine-geotiff benchmark_cold_cog_ranges -- --ignored --nocapture
```

Deterministic tests separately cover gap/span/count limits, unsorted/overlapping
ranges, partial/warm caches, IFD isolation, disabled caches, short-body fallback,
admission/deadline propagation, pre-I/O metadata validation and exact bbox
pixels through both the object-store and an actual localhost HTTP server.
