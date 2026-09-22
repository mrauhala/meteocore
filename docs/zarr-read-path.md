# Zarr/Icechunk read-path improvements

The first implementation batch addresses the map-focused review from 2026-09-22.

## Implemented

- Icechunk uses `zarr.cache_mb` for compressed chunk/range retention. Zero disables payload caching, including when the repository persists a nonzero cache setting.
- Icechunk storage uses a persistent I/O runtime. Every operation, including range-stream consumption, observes the request's absolute deadline; background operations have a 30-second timeout. Raster conversion and sampling check deadlines between rows.
- Sources and published sessions are separate. The Icechunk source retains repository clients and immutable payload caches, resolves the selected version on poll, and builds a new catalog only for a changed snapshot. Failed builds retain the previous catalog and retry.
- A catalog owns its pinned session and snapshot-derived render content version. In-flight readers stay on their snapshot; same-time corrections invalidate rendered images. No-op polls preserve the catalog and its version.
- Spatial windows use actual coordinate brackets, fixing transparent holes within irregular ascending or descending grids.
- Spatial windows use f64 samples with NaN nodata and fuse conversion/reordering. Payload allocations fall from approximately 40 to 16 bytes per selected source sample, excluding codecs, coordinate axes, and output. Scaling overflow becomes nodata consistently.
- CI runs Icechunk unit tests as well as integration tests. Regression fixtures contain non-inline compressed shards.

## Live cache experiment

Command:

```sh
cargo test -p engine-zarr --features icechunk map_cache_latency_probe -- --ignored --nocapture
```

The probe pins both configurations to the same snapshot, compares pixels, and exercises a 128x128 WGS84 temperature map over [20,55,30,65], a small pan, and the next forecast lead.

Observed on 2026-09-22 against the public AIFS repository, snapshot `GK0994A5Q4QM558TR9MG`:

| Engine read | cache_mb=0 | cache_mb=256 |
|---|---:|---:|
| Cold | 2,971 ms | 4,398 ms |
| Repeat | 905 ms | 66 ms |
| Small pan | 732 ms | 66 ms |
| Next lead | 663 ms | 66 ms |

Cached and uncached pixels matched exactly. These are single debug-build measurements, excluding HTTP image/metatile caches and PNG encoding. Network variation is substantial; the cold numbers are not a performance regression measurement. Payload caching helps repeated reads but does not remove initial download cost or repeated decompression.

## Remaining review work

1. Add bounded decoded inner-chunk reuse and single-flight fills. Avoid caching entire outer shards merely because they are the outer Zarr chunk: AIFS's inner chunks already span all 61 leads.
2. Introduce bounded async chunk/subchunk retrieval with explicit decode-memory admission and deadline propagation. Keep `concurrent_target(1)` until the storage/threading contract is changed together.
3. Fix plain-Zarr mutable metadata/coordinate refresh and cache invalidation. Its source currently retains the existing DsStore behavior; the Icechunk lifecycle fix does not address this.
4. Budget native source and codec allocations for maps, beyond the output-pixel budget. Compact windows reduce memory but do not provide admission control.
5. Diagnose effective inner chunk shapes and map read amplification, and expose per-collection fetch/decode/cache metrics.
6. Replace plain-Zarr whole-object shard reads with efficient range retrieval where appropriate.

## Regression coverage

Network-free tests exercise payload retention with deleted backing files, zero-cache behavior, warm payload reuse across changed snapshots, expired and in-flight deadlines, multiple caller/runtime contexts, no-op refresh, failed refresh/retry, old readers, explicit snapshot/tag selection, same-time data correction, and irregular-grid map/position consistency.

The cache override test creates V1 and V2 repositories with non-default metadata caches, compression, storage concurrency, and a virtual chunk container. It checks the full effective configuration after `Source::open` with caching enabled/disabled, and reopens without overrides to verify that persisted settings are unchanged. Icechunk's `Repository::open` merges the override into persisted settings; the derived configuration defaults leave fields unset rather than replacing their saved values.

The render-executor regression runs 48 map reads through the same `RenderJob::acquire_raster` / `run` boundary used by Maps, WMS, and Tiles, with four render slots, eight concurrent requests, and caching enabled/disabled. It reads external compressed shards and checks pixels, deadline propagation, and slot release. This covers the blocking-pool context beyond the isolated runtime bridge tests.

Tokio 1.53.1 permits `block_in_place` on a `spawn_blocking` thread: no async scheduler context is active there, so it calls the closure directly. The panic restriction applies to current-thread async execution and `LocalSet`, not blocking-pool workers; see [Tokio's implementation](https://docs.rs/tokio/1.53.1/src/tokio/runtime/scheduler/multi_thread/worker.rs.html#403-509). Icechunk handles current-thread callers separately and drives I/O on its persistent runtime.

A manual HTTP check on 2026-09-22 exercised the public AIFS snapshot above through a locally built server (`cargo build -p server --features icechunk`). At six concurrent requests, all 24 requests returned HTTP 200 PNGs with `X-Cache: MISS`: eight each for Maps, WMS, and Tiles over eight forecast leads. Maps/WMS used the bbox above at 128×128; Tiles used WebMercatorQuad 5/9/18. The six initial reads took 2.68–2.71 seconds and subsequent reads 79–137 ms, with no worker panics. The probe used `MC_RENDER_TIMEOUT_MS=15000` to isolate runtime correctness; these debug-build observations do not establish production latency guarantees.

Run:

```sh
cargo test -p engine-zarr
cargo test -p engine-zarr --features icechunk
cargo clippy -p engine-zarr --all-targets -- -D warnings
cargo clippy -p engine-zarr --features icechunk --all-targets -- -D warnings
```
