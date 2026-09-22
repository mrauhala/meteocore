# Zarr/Icechunk read-path improvements

Implementation batches address the map-focused review from 2026-09-22.

## Implemented

- Icechunk uses `zarr.cache_mb` for compressed chunk/range retention. Zero disables payload caching, including when the repository persists a nonzero cache setting.
- Icechunk storage uses a persistent I/O runtime. Every operation, including range-stream consumption, observes the request's absolute deadline; background operations have a 30-second timeout. Raster conversion and sampling check deadlines between rows.
- Sources and published sessions are separate. The Icechunk source retains repository clients and immutable payload caches, resolves the selected version on poll, and builds a new catalog only for a changed snapshot. Failed builds retain the previous catalog and retry.
- A catalog owns its pinned session and snapshot-derived render content version. In-flight readers stay on their snapshot; same-time corrections invalidate rendered images. No-op polls preserve the catalog and its version.
- Spatial windows use actual coordinate brackets, fixing transparent holes within irregular ascending or descending grids.
- Spatial windows use f64 samples with NaN nodata and fuse conversion/reordering. Payload allocations fall from approximately 40 to 16 bytes per selected source sample, excluding codecs, coordinate axes, and output. Scaling overflow becomes nodata consistently.
- CI runs Icechunk unit tests as well as integration tests. Regression fixtures contain non-inline compressed shards.
- Icechunk retains decoded native inner chunks in a separate per-collection `zarr.icechunk.decoded_cache_mb` budget (default 256 MiB, zero disables). Snapshot ID, array path, and inner-chunk coordinates identify entries. The budget is shared across published snapshots; old readers keep their snapshot keys. Plain mutable Zarr stores bypass decoded retention.
- Concurrent requests for one chunk share a fill. Each waiter observes its own absolute deadline; failures release the fill guard for retry. Only one chunk is held by a reader while copying its overlap into the requested subset. Retention counts native bytes and estimated key/allocation overhead, without widening entire chunks to f64.
- Cache fills read full inner chunks only when their native size is at most 64 MiB and fits the configured budget. Larger chunks, shards with outer transforms, and indeterminate grids retain ordinary subset reads. This is a cache expansion limit, not admission control for all source and codec memory.
- Startup diagnostics report outer and effective inner shapes, native bytes, and time steps per chunk. Per-collection `zarr_decoded_cache_{hits_total,misses_total,bytes,capacity_bytes}` and Grafana panels expose decoded reuse and occupancy. Misses count fill attempts, including failed ones; successful coalesced waiters count as hits. Bypassed reads and timed-out waits do not increment these counters.
- Maps/WMS/Tiles and EDR variable reads now share process-wide source-memory admission (`MC_ZARR_READ_MEMORY_MB`, default 1024 MiB). Checked estimates reject oversized windows before payload I/O, regardless of output image size. Reservations remain attached to sampling windows and are released on errors, deadline exits, and unwinding. Competing reads fail immediately rather than waiting while holding other windows or executor slots.

## Source-memory accounting

For a subset with `N` native values of `B` bytes each, admission reserves `N × (2B + 16)` for native bytes, a typed conversion copy, raw f64 values, and physical f64 windows. Window axes/container overhead or position-series output is added with checked arithmetic. Some lifetimes do not overlap; summing them avoids acquiring a second reservation while holding an earlier allocation.

Decode workspace adds `4 × (largest touched native decode unit + its shard index)`. Chunk shapes include stored padding and all forecast leads, even when the selected subset is tiny. Exclusively sharded partial reads use inner chunks; full-shard fast paths and shards with outer transforms use outer chunks. The decoded-cache reader splits whole windows into inner chunks, avoiding that full-shard fast path. Retrieval remains serial, so only the largest workspace is reserved. Parallel reads must reserve the combined active workspace before launch.

The budget is shared by all collections and snapshots and survives reloads. A span's windows share one reservation until its last window is dropped, so HTTP cancellation cannot release memory still used by the executing worker. Budget exhaustion maps to the existing 503 resource-exhausted response. Zero rejects on-grid variable reads; metadata loading and off-grid empty responses do not reserve this budget.

This is an admission estimate, not an allocator-enforced RSS limit. Encoded object buffers (including plain-Zarr whole-object shard downloads), codec-private scratch, persistent catalog/coordinate metadata, resident caches, and API output buffers remain outside it. Default estimates apply to cache hits too, so low limits can reject reads whose actual working set would be smaller. `zarr_read_reserved_bytes`, `zarr_read_capacity_bytes`, and `zarr_read_rejected_total`, with matching Grafana panels, expose admitted estimates and rejections.

## Live cache experiment

Command:

```sh
cargo test -p engine-zarr --features icechunk map_cache_latency_probe -- --ignored --nocapture
```

The probe pins all three configurations to the same snapshot, compares pixels, and exercises a 128x128 WGS84 temperature map over [20,55,30,65], a small pan, and the next forecast lead.

Observed on 2026-09-22 against the public AIFS repository, snapshot `GK0994A5Q4QM558TR9MG`:

| Engine read | cache_mb=0 | cache_mb=256 |
|---|---:|---:|
| Cold | 2,971 ms | 4,398 ms |
| Repeat | 905 ms | 66 ms |
| Small pan | 732 ms | 66 ms |
| Next lead | 663 ms | 66 ms |

Cached and uncached pixels matched exactly. The first batch measured compressed payload caching alone; decoded retention was not yet implemented.

The second batch reran all configurations against snapshot `RV08AB14MF5BGZ8YJX3G` on 2026-09-22:

| Engine read | No caches | Payload 256 MiB, decoded 0 | Payload 256 MiB, decoded 256 MiB |
|---|---:|---:|---:|
| Cold | 3,336 ms | 2,809 ms | 4,287 ms |
| Repeat | 1,046 ms | 66 ms | 4 ms |
| Small pan | 1,370 ms | 67 ms | 4 ms |
| Next lead | 1,543 ms | 65 ms | 4 ms |

All pixels matched exactly. The decoded configuration filled one native chunk on the cold read and then recorded one hit per read, with no further fills. Resident weight was 14,113,186 bytes (14,112,960 bytes of native f32 data plus accounting overhead). The chunk spans all 61 leads, so subsequent animation frames reuse its decoded data.

After adding source-memory admission, the same probe passed again on 2026-09-22 against snapshot `RV08AB14MF5BGZ8YJX3G`, with exact pixel equality across all configurations. Decoded repeat/pan/next-lead reads remained 4 ms; payload-only reads were 65/66/68 ms. The new default budget admitted the cold and warm requests.

These are single debug-build measurements, excluding HTTP image/metatile caches and PNG encoding. Network variation is substantial; the cold numbers do not establish a regression or improvement. Decoded reuse avoids repeated decompression on overlapping reads but does not remove initial download/decode cost. This is not a controlled GRIB-versus-Icechunk benchmark.

## Remaining review work

1. Introduce bounded async chunk/subchunk retrieval with explicit decode-memory admission and deadline propagation. Keep `concurrent_target(1)` until the storage/threading contract is changed together.
2. Fix plain-Zarr mutable metadata/coordinate refresh and cache invalidation. Its source currently retains the existing DsStore behavior; the Icechunk lifecycle fix does not address this.
3. Extend source-memory admission to encoded-object buffers and codec-specific scratch limits, and refine estimates for cache hits. The current reservation covers native windows and estimated decode/index workspace, not every underlying allocation.
4. Extend layout diagnostics and decoded-cache metrics with requested/decoded-byte amplification, storage requests/bytes, and fetch/decode/resample timings.
5. Replace plain-Zarr whole-object shard reads with efficient range retrieval where appropriate, and coalesce overlapping plain-store payload fills.

## Regression coverage

Network-free tests exercise payload retention with deleted backing files, zero-cache behavior, warm payload reuse across changed snapshots, expired and in-flight deadlines, multiple caller/runtime contexts, no-op refresh, failed refresh/retry, old readers, explicit snapshot/tag selection, same-time data correction, and irregular-grid map/position consistency.

Decoded-cache tests compare native cached and uncached values across chunks, shards, clipped edges, and missing chunks; cover int16 values/fill, eviction, oversized bypass, corrupt-codec failure/retry, snapshot/array identity, and a waiter whose deadline expires while another caller holds the fill guard. Icechunk tests reuse decoded chunks for a pan, another lead, and an EDR query after deleting external payloads with compressed caching disabled.

Source-memory tests cover tiny subsets inside large padded chunks, oversized native windows with small chunks, inner-chunk versus full-shard estimates, concurrent reservations, last-owner release, deadline/overflow rejection, off-grid reads, span ownership through sampling, and a 1×1 map whose admission fails before reading a corrupt payload. An admitted corrupt read releases its reservation on failure.

The cache override test creates V1 and V2 repositories with non-default metadata caches, compression, storage concurrency, and a virtual chunk container. It checks the full effective configuration after `Source::open` with caching enabled/disabled, and reopens without overrides to verify that persisted settings are unchanged. Icechunk's `Repository::open` merges the override into persisted settings; the derived configuration defaults leave fields unset rather than replacing their saved values.

The render-executor regression runs 72 map reads through the same `RenderJob::acquire_raster` / `run` boundary used by Maps, WMS, and Tiles, with four render slots and eight concurrent requests. It tests no caching, payload-only caching, and decoded-only caching. It reads external compressed shards and checks pixels, deadline propagation, slot release, and exactly one decoded fill per inner chunk. This covers the blocking-pool context beyond the isolated runtime bridge tests.

Tokio 1.53.1 permits `block_in_place` on a `spawn_blocking` thread: no async scheduler context is active there, so it calls the closure directly. The panic restriction applies to current-thread async execution and `LocalSet`, not blocking-pool workers; see [Tokio's implementation](https://docs.rs/tokio/1.53.1/src/tokio/runtime/scheduler/multi_thread/worker.rs.html#403-509). Icechunk handles current-thread callers separately and drives I/O on its persistent runtime.

A manual HTTP check on 2026-09-22 exercised the first batch's public AIFS snapshot `GK0994A5Q4QM558TR9MG` through a locally built server (`cargo build -p server --features icechunk`). At six concurrent requests, all 24 requests returned HTTP 200 PNGs with `X-Cache: MISS`: eight each for Maps, WMS, and Tiles over eight forecast leads. Maps/WMS used the bbox above at 128×128; Tiles used WebMercatorQuad 5/9/18. The six initial reads took 2.68–2.71 seconds and subsequent reads 79–137 ms, with no worker panics. The probe used `MC_RENDER_TIMEOUT_MS=15000` to isolate runtime correctness; these debug-build observations do not establish production latency guarantees.

Run:

```sh
cargo test -p engine-zarr
cargo test -p engine-zarr --features icechunk
cargo clippy -p engine-zarr --all-targets -- -D warnings
cargo clippy -p engine-zarr --features icechunk --all-targets -- -D warnings
```
