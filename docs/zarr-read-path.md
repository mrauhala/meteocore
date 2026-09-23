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
- Concurrent requests for one chunk share a fill. Each waiter observes its own absolute deadline; failures release the fill guard for retry. Retention counts native bytes and estimated key/allocation overhead, without widening entire chunks to f64.
- Cache fills read full inner chunks only when their native size is at most 64 MiB and fits the configured budget. Larger chunks, shards with outer transforms, and indeterminate grids retain ordinary subset reads. This is a cache expansion limit, not admission control for all source and codec memory.
- Startup diagnostics report outer and effective inner shapes, native bytes, and time steps per chunk. Per-collection `zarr_decoded_cache_{hits_total,misses_total,bytes,capacity_bytes}` and Grafana panels expose decoded reuse and occupancy. Misses count fill attempts, including failed ones; successful coalesced waiters count as hits. Bypassed reads and timed-out waits do not increment these counters.
- Maps/WMS/Tiles and EDR variable reads now share process-wide source-memory admission (`MC_ZARR_READ_MEMORY_MB`, default 1024 MiB). Checked estimates reject oversized windows before payload I/O, regardless of output image size. Reservations remain attached to sampling windows and are released on errors, deadline exits, and unwinding. Competing reads fail immediately rather than waiting while holding other windows or executor slots.
- Icechunk inner-chunk retrieval now processes batches of up to four chunks through a shared four-worker pool. Every worker inherits the absolute request deadline and drives async storage through the persistent I/O runtime; native decoding runs off its reactor. Individual zarrs calls use the shared serial codec options. All batch jobs join on errors and unwinding before the caller releases its reservation. Completed-but-not-copied chunks and queued jobs are bounded by the batch size. Single-chunk and entirely cached batches stay on the caller. Zero decoded-cache capacity disables retention while preserving concurrent retrieval; plain Zarr and shards with outer transforms keep the ordinary serial path.
- Plain-Zarr polls now rebuild metadata and coordinates with fresh cache generations. Generation-and-path keys isolate cached payloads and missing keys; object-store clients and one byte budget survive refresh. Zero cache capacity disables retention. Successful builds publish a new catalog and nonzero render content version before retiring the previous generation, so new requests cannot load a retired catalog; failed builds preserve the published catalog. Same-timestamp payload corrections invalidate rendered images, even if metadata is unchanged.
- Plain-Zarr full-object, range-slicing, and size reads now share a payload fill for the same generation and object key. Waiters use their own absolute deadline (30 seconds for a background wait), and a failed or expired owner releases the fill for another caller to retry. Zero retention and oversized payloads still share the result with existing waiters without keeping it resident. Cache waits release Tokio runtime workers, allowing the owner's storage I/O to progress. Different keys and generations proceed independently.
- Encoded payloads now acquire additional reservations from the same global source budget before collection. Plain Zarr uses GET response sizes, with no preliminary HEAD; Icechunk uses immutable chunk-reference lengths and reserves all requested ranges before launching their reads. Payload cache hits and coalesced waiters also admit their codec copies. Decoded-cache hits do not perform encoded reads.

- Serial retrieval now sets both `concurrent_target(1)` and `chunk_concurrent_minimum(1)`. The default minimum of four could override the target for multi-chunk plain/fallback reads, moving storage onto Rayon workers without the caller's runtime, deadline, or encoded-budget scope. A multi-chunk map regression exposed this bypass; it now rejects an oversized encoded object before decoding.

## Plain-Zarr refresh semantics

Every successful plain-Zarr poll starts a new cache generation, including polls with unchanged metadata. Without a versioned manifest, a metadata-only scan cannot prove that payloads are unchanged. This intentionally trades cache reuse across poll boundaries for visibility of mutable data, without recursively listing every chunk.

Old readers retain their catalog and any cached objects or sampled windows. Once retired, a cache miss fails instead of fetching bytes from the current backend, including after eviction. Downloads that are still in progress when retirement occurs are discarded. Missing keys are retained within the same byte budget, so previously observed fill chunks stay missing for old readers while retained.

Generations are cache isolation, not transactional snapshots. External in-place writes before publication can still affect active uncached reads, and a catalog rebuild itself is not an atomic view of a concurrently modified tree. Publish stable stores, or use Icechunk for atomic updates across objects. An old query needing uncached bytes during refresh returns HTTP 503 and must be retried against the current catalog; Maps/WMS/Tiles include `Retry-After: 1`. The retirement signal stays typed through zarrs storage/codec wrappers, while unrelated storage or decode failures remain engine errors. An optimization to retain payloads across generations would need object-version validation; reusing path-only keys would reintroduce stale data.

## Source-memory accounting

For a subset with `N` native values of `B` bytes each, admission reserves `N × (2B + 16)` for native bytes, a typed conversion copy, raw f64 values, and physical f64 windows. Window axes/container overhead or position-series output is added with checked arithmetic. Some lifetimes do not overlap; summing them admits all native/conversion buffers together before payload reads.

One cold decode workspace adds `4 × (largest touched native decode unit + its shard index)`. Chunk shapes include stored padding and all forecast leads, even when the selected subset is tiny. Exclusively sharded partial reads use inner chunks; full-shard fast paths and shards with outer transforms use outer chunks. Plain-Zarr and outer-transform reads reserve one workspace together with the source buffers.

The Icechunk reader reserves source buffers first, then looks up and admits at most four inner chunks per batch. A decoded cache hit reserves the actual capacity of its native buffer, without decode or shard-index workspace. It holds that exact buffer through copying, so eviction cannot trigger an unadmitted cold read or leave an evicted buffer unaccounted for. A miss or bypass reserves a cold workspace before loading. Each batch grows only while another chunk fits; a failed extra slot reduces concurrency without counting as a rejected request. If its first chunk cannot fit, the read fails immediately. Unadmitted cache references are dropped before batch I/O. Chunk reservations end after copying, while source buffers remain reserved through sampling. Entirely cached batches copy on the caller. A miss that becomes a hit during admission or waits for another fill still conservatively holds its cold workspace.

The budget is shared by all collections and snapshots and survives reloads. A span's windows share one reservation until its last window is dropped, so HTTP cancellation cannot release memory still used by the executing worker. Budget exhaustion maps to the existing 503 resource-exhausted response. Zero rejects on-grid variable reads; metadata loading and off-grid empty responses do not reserve this budget.

Encoded admission adds twice the full-object or requested-range byte length for the collected body and zarrs' owned encoded copy. Plain-Zarr admission happens after GET headers and before body collection; collection uses a fixed advertised-size destination and rejects oversized or truncated bodies. Icechunk obtains sizes from its pinned chunk references without a payload HEAD request. Its adapter reserves the combined range lengths before starting the operation. These reservations share the native-window budget and fail immediately with typed `ResourceExhausted` if there is insufficient space.

Reservations belong to synchronous retrieval scopes, because zarrs converts storage `Bytes` into owned codec `Vec`s. Releasing a guard when the original `Bytes` is dropped would undercount decoding. Each Icechunk chunk worker installs its own scope; joins, deadline exits, and unwinding release it only after retrieval finishes. A plain or outer-transform fallback retains allowances through its complete subset retrieval. Repeated plain whole-object lookups within a scope reuse the largest allowance for that key; different objects and range operations accumulate conservatively. Compressed cache hits and coalesced waiters still need an allowance for their codec copies, while resident caches do not keep the scope alive. Metadata/coordinate discovery remains outside source admission.

Numeric variable arrays replace zarrs' gzip and zstd decode operations with bounded decoders, including inside nested shards and outside compressed shards. The codec chain computes each output limit from the declared chunk representation; compressed frame headers cannot raise it. Gzip validates EOF/checksums at the limit, and zstd uses a fixed destination with single-pass decompression. Fixed native outputs use the already admitted decode workspace. Bounded intermediate representations (such as stacked compression and compressed shard bodies) add a two-copy capacity allowance before allocating or growing, retained through retrieval. Missing representation bounds fail closed. Invalid lengths and corrupt frames remain engine errors; allocation/admission failures remain typed `ResourceExhausted`, and expired decode deadlines remain `DeadlineExceeded`. Gzip checks deadlines between blocks; the zstd call is checked before and after, not interrupted mid-call.

Blosc validates the complete encoded frame before delegating full or partial decompression to zarrs. Its header output length must satisfy the codec representation, and its block size must be valid and no larger than that output. The checked input preserves upstream getitem partial decoding, including for sharded arrays; it does not add storage reads or force full decompression. Both sync and async partial inputs are checked. Source admission adds the pinned c-blosc serial/getitem scratch allowance (`2/3 × blocksize + 4 × typesize` respectively) before decoding, plus the usual two-copy allowance for bounded intermediate outputs. These allowances live through retrieval and release on error/unwind. Missing chunks retain fill semantics. Invalid frame/representation pairs fail before scratch admission; budget failures retain typed `ResourceExhausted`.

This is an admission estimate, not an allocator-enforced RSS limit. Compressor-private contexts and allocations beyond the explicitly admitted Blosc scratch, transport buffering, Icechunk internal overfetch/copying beyond the requested ranges, persistent catalog/coordinate metadata, resident caches, and API output buffers remain outside it. Blosc retains upstream/native allocation behavior; admission does not make every allocator failure recoverable. Encoded sizes are discovered after native admission; tight capacity can reject a read even when smaller fan-out or earlier release of sequential encoded/codec buffers could have fit. These refinements remain follow-up work. `zarr_read_reserved_bytes`, `zarr_read_capacity_bytes`, and `zarr_read_rejected_total`, with matching Grafana panels, expose admitted estimates and rejections.

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

## Cold chunk concurrency experiment

```sh
cargo test -p engine-zarr --features icechunk --lib chunk_concurrency_latency_probe -- --ignored --nocapture
```

The probe reads four inner chunks from a 242×241 spatial subset at one reference time and forecast lead, using fresh repository clients and disabled payload/decoded retention. It pins the snapshot, alternates serial/four-worker order across three pairs, and compares every native output byte. This isolates the chunk reader; catalog discovery, map resampling, PNG encoding, and HTTP caches are outside the timer.

On 2026-09-22 against AIFS snapshot `RV08AB14MF5BGZ8YJX3G`, after compilation and other validation had finished:

| Pair | Serial | Four workers |
|---|---:|---:|
| 1 (serial first) | 4,363 ms | 3,459 ms |
| 2 (parallel first) | 5,438 ms | 3,809 ms |
| 3 (serial first) | 11,327 ms | 5,566 ms |

All outputs matched exactly. An earlier run overlapping local compilation measured serial 6,840/4,624/8,154 ms and parallel 7,037/3,407/4,775 ms. The large spread, small sample, and debug build preclude a fixed speedup claim or production latency guarantee. The deterministic gated-storage tests establish actual overlap and the worker bound independently of network timings. Single-inner-chunk requests do not gain fetch/decode parallelism.

The map-cache probe also passed on the same snapshot after this change: decoded repeat/pan/next-lead reads remained 4 ms, payload-only reads were 66/65/65 ms, and pixels matched exactly across all three cache configurations. The decoded-cache cold read took 2,368 ms and retained the same single 14,113,186-byte entry.

The bounded gzip/zstd change passed the map-cache probe on 2026-09-23 against snapshot `4E5WWS7GJXT83MVQF260`. All configurations produced identical pixels. Warm decoded repeat/pan/next-lead reads were 4/4/4 ms, and payload-only reads were 66/67/66 ms. The decoded-cache cold read took 2,303 ms and retained one 14,113,186-byte entry. These debug-build observations are consistent with earlier warm-read timings; they are not a controlled before/after benchmark or a GRIB comparison.

## Remaining review work

1. Refine encoded/intermediate/scratch headroom, coalesced-miss workspace admission, and sequential encoded/codec-buffer lifetimes. Decoded cache hits now reserve pinned buffer capacity; cold chunk workspaces end after batch copying. Encoded-object admission, gzip/zstd/Blosc output bounds, and Blosc's size-dependent scratch admission are implemented. Compressor-private contexts, other codecs, and coordinate/metadata discovery remain outside these explicit bounds.
2. Extend layout diagnostics and decoded-cache metrics with requested/decoded-byte amplification, storage requests/bytes, and fetch/decode/resample timings.
3. Replace plain-Zarr whole-object shard reads with efficient range retrieval where appropriate. Parallelizing this backend or outer-transformed Icechunk shards needs a separate runtime/admission design.
4. Handle arbitrary byte ranges in upstream Blosc getitem decoding. The pinned implementation rounds offsets/lengths down by the frame's typesize; a stacked gzip→Blosc(typesize=4)→zstd fixture fails even without our wrapper when the gzip stream is not element-aligned. The bounded adapter retains that existing behavior. Opaque compressed intermediates in the regression fixture use typesize=1 without shuffle; ordinary numeric Blosc reads remain element-aligned.

## Regression coverage

Blosc tests compare full/getitem reads across BloscLZ, LZ4, zlib, and zstd with no shuffle, byte shuffle, and bitshuffle; they cover multiple ranges, block boundaries, malformed headers, frame/representation mismatches, scratch/intermediate admission, and typed deadline errors. V2 Fortran layout and V3 inner/outer/nested sharding tests include Blosc. A local Icechunk map test commits oversized inner-frame headers, verifies rejection with decoded retention on/off, then repairs the payload and verifies refresh recovery.

Codec tests cover gzip/zstd exact and bounded lengths, empty and malformed streams, excessive expansion, unknown-size and concatenated zstd frames, intermediate admission/release, and typed deadlines. Independent V2 Fortran-order fixtures preserve chunk keys and values. V3 inner/outer compression, nested shards, stacked codecs, partial reads, fill values, and metadata round-trip through bounded arrays. A map rejects a small gzip object that expands beyond its native chunk and releases its reservation; a fresh catalog can subsequently read repaired data.

Encoded-admission tests reject a large HTTP response from headers without receiving its body, then successfully retry the fill. They cover cached full/range/size reads, independent budgets for coalesced readers, retained codec-copy allowances, body deadline cleanup, and malformed body lengths. Icechunk tests reserve full and mixed-range reads from pinned references, reject before accessing deleted external payloads, preserve missing chunks, and reuse decoded cache entries without encoded admission. A tiny map rejects an oversized encoded object even when its native window fits. Gated worker tests cover scope propagation and release on success, failure, deadline expiry, panic, and waiter cancellation.

Plain-Zarr tests cover V2 and V3 metadata/coordinate updates, same-time payload corrections, render-version changes, failed rebuild/retry, retained old pixels, missing-key isolation, old-reader failure after eviction, shared byte budgets, and zero retention. Tests assert retryable errors for retired map/EDR reads and full/partial shard decoding, while cached corruption remains an engine error. A gated localhost HTTP server verifies that a download racing retirement is discarded with a typed retryable error rather than cached or returned.

Controlled HTTP tests verify that nine concurrent full/range/size readers use one GET for present and missing objects, waiter deadlines leave the owner running (including zero retention and oversized payloads), failed/expired owners allow retries, and different keys/generations proceed independently. A two-worker Tokio test holds the object response while checking that cache waiters allow unrelated runtime work to progress. Another test uses the actual `RenderJob::acquire_raster`/`run` blocking-pool boundary: eight readers share one HTTP GET, preserve the executor deadline, reject expired warm/cold reads, and release all render slots. Plain-Zarr map tests run 48 reads through that executor with retention enabled/disabled, verifying real async file I/O, gzip decoding, and pixels.

Network-free tests exercise payload retention with deleted backing files, zero-cache behavior, warm payload reuse across changed snapshots, expired and in-flight deadlines, multiple caller/runtime contexts, no-op refresh, failed refresh/retry, old readers, explicit snapshot/tag selection, same-time data correction, and irregular-grid map/position consistency.

Decoded-cache tests compare native cached and uncached values across chunks, shards, clipped edges, and missing chunks; cover int16 values/fill, eviction, oversized bypass, corrupt-codec failure/retry, snapshot/array identity, and a waiter whose deadline expires while another caller holds the fill guard. Icechunk tests reuse decoded chunks for a pan, another lead, and an EDR query after deleting external payloads with compressed caching disabled.

Cache-aware admission tests read warm windows with budgets too small for a cold workspace, including sharded and padded edge chunks. Deleted backing files prove these reads avoid storage. Eviction between admission and loading preserves the pinned buffer and its reservation. Gated storage tests exercise reduced cold and mixed batches, source ownership after copying, and rejection counters that exclude successful fan-out reductions.

Source-memory tests cover tiny subsets inside large padded chunks, oversized native windows with small chunks, inner-chunk versus full-shard estimates, concurrent reservations, last-owner release, deadline/overflow rejection, off-grid reads, span ownership through sampling, and a 1×1 map whose admission fails before reading a corrupt payload. An admitted corrupt read releases its reservation on failure.

Concurrency tests gate actual storage reads to prove overlapping chunk work, the shared four-worker limit across requests, inherited deadlines, and reservation ownership after dropping the waiter. Success, storage failure, deadline expiry, and injected panic all join workers and release memory. After a failed batch, no further batches launch. Admission tests verify one-to-four workspace accounting, reduction under contention, and unchanged single-chunk estimates. Native-value comparisons cover disabled retention as well as cache hits, shard boundaries, edge padding, and missing chunks.

The cache override test creates V1 and V2 repositories with non-default metadata caches, compression, storage concurrency, and a virtual chunk container. It checks the full effective configuration after `Source::open` with caching enabled/disabled, and reopens without overrides to verify that persisted settings are unchanged. Icechunk's `Repository::open` merges the override into persisted settings; the derived configuration defaults leave fields unset rather than replacing their saved values.

The render-executor regression runs 72 map reads through the same `RenderJob::acquire_raster` / `run` boundary used by Maps, WMS, and Tiles, with four render slots and eight concurrent requests. It tests no caching, payload-only caching, and decoded-only caching. It reads external compressed shards and checks pixels, deadline propagation, slot release, and exactly one decoded fill per inner chunk. This covers the blocking-pool context beyond the isolated runtime bridge tests.

Tokio 1.53.1 permits `block_in_place` on a `spawn_blocking` thread: no async scheduler context is active there, so it calls the closure directly. The panic restriction applies to current-thread async execution and `LocalSet`, not blocking-pool workers; see [Tokio's implementation](https://docs.rs/tokio/1.53.1/src/tokio/runtime/scheduler/multi_thread/worker.rs.html#403-509). Plain Zarr intentionally uses this context-aware bridge for both EDR async workers and render blocking workers, with regression coverage for both; handle presence alone does not identify which kind of worker is calling. Explicit-runtime storage APIs remain preferred for other blocking/foreign workers when the caller supplies the I/O runtime. Icechunk handles current-thread callers separately and drives I/O on its persistent runtime.

A manual HTTP check on 2026-09-22 exercised the first batch's public AIFS snapshot `GK0994A5Q4QM558TR9MG` through a locally built server (`cargo build -p server --features icechunk`). At six concurrent requests, all 24 requests returned HTTP 200 PNGs with `X-Cache: MISS`: eight each for Maps, WMS, and Tiles over eight forecast leads. Maps/WMS used the bbox above at 128×128; Tiles used WebMercatorQuad 5/9/18. The six initial reads took 2.68–2.71 seconds and subsequent reads 79–137 ms, with no worker panics. The probe used `MC_RENDER_TIMEOUT_MS=15000` to isolate runtime correctness; these debug-build observations do not establish production latency guarantees.

Run:

```sh
cargo test -p engine-zarr
cargo test -p engine-zarr --features icechunk
cargo clippy -p engine-zarr --all-targets -- -D warnings
cargo clippy -p engine-zarr --features icechunk --all-targets -- -D warnings
```
