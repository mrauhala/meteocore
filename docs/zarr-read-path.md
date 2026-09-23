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
- Plain-Zarr and outer-transformed Icechunk reads release encoded/intermediate admission after each stored chunk. They preserve serial decode-into retrieval, partial reads and full-shard paths; all nested shard/index allowances remain held through the chunk's decode. The native/source reservation remains with the sampling window.

## Plain-Zarr refresh semantics

Every successful plain-Zarr poll starts a new cache generation, including polls with unchanged metadata. Without a versioned manifest, a metadata-only scan cannot prove that payloads are unchanged. This intentionally trades cache reuse across poll boundaries for visibility of mutable data, without recursively listing every chunk.

Old readers retain their catalog and any cached objects or sampled windows. Once retired, a cache miss fails instead of fetching bytes from the current backend, including after eviction. Downloads that are still in progress when retirement occurs are discarded. Missing keys are retained within the same byte budget, so previously observed fill chunks stay missing for old readers while retained.

Generations are cache isolation, not transactional snapshots. External in-place writes before publication can still affect active uncached reads, and a catalog rebuild itself is not an atomic view of a concurrently modified tree. Publish stable stores, or use Icechunk for atomic updates across objects. An old query needing uncached bytes during refresh returns HTTP 503 and must be retried against the current catalog; Maps/WMS/Tiles include `Retry-After: 1`. The retirement signal stays typed through zarrs storage/codec wrappers, while unrelated storage or decode failures remain engine errors. An optimization to retain payloads across generations would need object-version validation; reusing path-only keys would reintroduce stale data.

## Source-memory accounting

For a subset with `N` native values of `B` bytes each, admission reserves `N × (2B + 16)` for native bytes, a typed conversion copy, raw f64 values, and physical f64 windows. Window axes/container overhead or position-series output is added with checked arithmetic. Some lifetimes do not overlap; summing them admits all native/conversion buffers together before payload reads.

One cold decode workspace adds `4 × (largest touched native decode unit + its shard index)`. Chunk shapes include stored padding and all forecast leads, even when the selected subset is tiny. Exclusively sharded partial reads use inner chunks; full-shard fast paths and shards with outer transforms use outer chunks. Plain-Zarr and outer-transform reads reserve one workspace together with the source buffers.

The Icechunk reader reserves source buffers first, then looks up and admits at most four inner chunks per batch. A decoded cache hit reserves the actual capacity of its native buffer, without decode or shard-index workspace. It holds that exact buffer through copying, so eviction cannot trigger an unadmitted cold read or leave an evicted buffer unaccounted for. A miss or bypass reserves a cold workspace before loading. Each batch grows only while another chunk fits; a failed extra slot reduces concurrency without counting as a rejected request. If its first chunk cannot fit, the read fails immediately. Unadmitted cache references are dropped before batch I/O. Chunk reservations end after copying, while source buffers remain reserved through sampling. Entirely cached batches copy on the caller.

Cache-fill ownership is claimed before cold admission. A reader waiting for the same decoded chunk retains its source buffers, but reserves no duplicate decode workspace or encoded/codec headroom. After a successful fill, it admits the exact returned buffer capacity and pins that buffer through copying. If the owner fails or panics, its encoded scope and native reservation release before its fill claim; a successor must pass cold admission before decoding. Cache waits observe the caller's deadline (30 seconds without one) and run on the caller only when its batch is empty. A busy later chunk flushes the current batch before retrying, preventing waits while holding queued fill claims or shared decode-worker slots. Memory admission itself remains fail-fast.

Cold batches also reserve encoded and codec headroom together with each native workspace. Catalog-time planning recognizes bytes/transpose with gzip, zstd, Blosc, and CRC32C, including ordinary shards with separate inner and index chains. It uses stored padding, encoder size bounds, two-copy bounded intermediates, and the validated Blosc block/type maxima; it does not fetch payloads or shard indexes to plan. Storage and codec scopes consume this prepaid allowance before acquiring any extra capacity, preserving their actual-size checks without double charging. The scope retains the reservation owner when passed to storage futures. Warm decoded hits need no headroom.

Within each recognized bytes-codec chain, headroom now uses the maximum Blosc scratch allowance instead of adding allowances across sequential stages. The collected body and all bounded intermediates remain additive, and concurrent chunks prepay independent credit. This follows pinned zarrs 0.23.14's `CodecChain::decode`/`decode_into`, which advance only after each codec call returns. For partial decoding, the adapter's `CheckedInput::decode` completes upstream input decoding before admitting its own frame's scratch. Per-call guards release that scratch before the next stage acquires it. Both serial options remain required. Estimates still cover any accepted frame block/type sizes, retain actual-size growth checks, and keep nested/unknown layouts on the existing fallback.

Encoder bounds may exceed actual payload sizes. If one combined estimate cannot fit but its native workspace can, the reader attempts just that chunk with actual-size admission. Unknown layouts, including nested shards, also run one cold chunk per batch. Neither fallback waits for memory; genuine exhaustion remains a typed 503. Actual bytes beyond an encoder estimate still require additional admission. This retains support for valid encodings whose optional headers exceed an encoder's usual bound. These hints are not new codec-output limits.

The budget is shared by all collections and snapshots and survives reloads. A span's windows share one reservation until its last window is dropped, so HTTP cancellation cannot release memory still used by the executing worker. Budget exhaustion maps to the existing 503 resource-exhausted response. Zero rejects on-grid variable reads; metadata loading and off-grid empty responses do not reserve this budget.

Encoded admission adds twice the full-object or requested-range byte length for the collected body and zarrs' owned encoded copy. Plain-Zarr admission happens after GET headers and before body collection; collection uses a fixed advertised-size destination and rejects oversized or truncated bodies. Icechunk obtains sizes from its pinned chunk references without a payload HEAD request. Its adapter reserves the combined range lengths before starting the operation. These reservations share the native-window budget and fail immediately with typed `ResourceExhausted` if there is insufficient space.

Reservations belong to synchronous retrieval scopes, because zarrs converts storage `Bytes` into owned codec `Vec`s. Releasing a guard when the original `Bytes` is dropped would undercount decoding. Each Icechunk chunk worker installs its own scope; joins, deadline exits, and unwinding release it only after retrieval finishes. Plain or outer-transform reads install a scope for each stored chunk in `retrieval::serial`, releasing its encoded/intermediate reservations before advancing to the next chunk. Repeated plain whole-object lookups within a chunk reuse the largest allowance for that key; range operations and intermediate outputs within that chunk still accumulate conservatively. Compressed cache hits and coalesced waiters still need an allowance for their codec copies, while resident caches do not keep the scope alive. Metadata/coordinate discovery remains outside source admission.

The stored-chunk boundary follows pinned zarrs 0.23.14's `array_sync_readable.rs`: `retrieve_chunk_subset_into` creates local storage/partial-decoder handles and finishes `partial_decode_into` before returning; complete chunks use `retrieve_chunk_into`. `retrieval::serial` calls `retrieve_array_subset_into_opt` on each outer-chunk overlap, writing directly into one exclusively borrowed view of the source output. All local decoder handles and their codec/index copies have dropped when that call returns. No extra native chunk buffer or copy is introduced. Single-chunk reads retain the ordinary owned-result fast path. Full-shard decoding, stored padding, nested shards, fill values, and serial options retain their existing semantics. The caller's native/source reservation covers the output after temporary admission ends, including through sampling and worker cancellation.

Numeric variable arrays replace zarrs' gzip and zstd decode operations with bounded decoders, including inside nested shards and outside compressed shards. The codec chain computes each output limit from the declared chunk representation; compressed frame headers cannot raise it. Gzip validates EOF/checksums at the limit, and zstd uses a fixed destination with single-pass decompression. Fixed native outputs use the already admitted decode workspace. Bounded intermediate representations (such as stacked compression and compressed shard bodies) add a two-copy capacity allowance before allocating or growing, retained through retrieval. Missing representation bounds fail closed. Invalid lengths and corrupt frames remain engine errors; allocation/admission failures remain typed `ResourceExhausted`, and expired decode deadlines remain `DeadlineExceeded`. Gzip checks deadlines between blocks; the zstd call is checked before and after, not interrupted mid-call.

Blosc validates the complete encoded frame before delegating full or partial decompression to zarrs. Its header output length must satisfy the codec representation, and its block size must be valid and no larger than that output. The checked input preserves upstream getitem partial decoding, including for sharded arrays; it does not add storage reads or force full decompression. Both sync and async partial inputs are checked. Source admission adds the pinned c-blosc serial/getitem scratch allowance (`2/3 × blocksize + 4 × typesize` respectively) before decoding, plus the usual two-copy allowance for bounded intermediate outputs. Scratch admission ends when each full/partial Blosc decoder call returns, including errors and unwinding; bounded intermediate outputs remain admitted through retrieval. Extra scratch capacity is released and borrowed prepaid credit is returned to the same scope, so sequential calls can reuse it. Overlapping calls hold independent guards. Partial calls retain their own guards around the upstream getitem decoder, without adding storage reads or copying its owned outputs. Missing chunks retain fill semantics. Invalid frame/representation pairs fail before scratch admission; budget failures retain typed `ResourceExhausted`.

The lifetime proof is in pinned `blosc-src 0.3.8`, `c-blosc/blosc/blosc.c`: `serial_blosc` and the normal/error decode loop in `blosc_getitem` free their temporary buffer before returning. The getitem out-of-bounds branches are an exception: they return after allocation without freeing that buffer. The adapter therefore validates explicit, open-ended, and suffix ranges against the checked frame length before each native call, including checked addition for range ends. Invalid ranges fail without entering those native branches. Valid unaligned ranges retain the existing upstream behavior tracked in #780. Encoded and intermediate buffers still have retrieval-wide reservations because their zarrs copies can outlive an individual decoder call.

This is an admission estimate, not an allocator-enforced RSS limit. Compressor-private contexts and allocations beyond the explicitly admitted Blosc scratch, transport buffering, Icechunk internal overfetch/copying beyond the requested ranges, persistent catalog/coordinate metadata, resident caches, and API output buffers remain outside it. Blosc retains upstream/native allocation behavior; admission does not make every allocator failure recoverable. Conservative encoder bounds can reduce concurrency more than actual sizes require; unestimated growth and competing reads can still exhaust the budget. Earlier release within one chunk's codec/index chain and tighter estimates remain follow-up work. `zarr_read_reserved_bytes`, `zarr_read_capacity_bytes`, and `zarr_read_rejected_total`, with matching Grafana panels, expose admitted estimates and rejections.

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

## ICON-EU production preview burst

On 2026-09-23, the deployed image at `dee3cc4` (through #783) exposed a
separate concurrency problem. Enabling ICON-EU in a fresh `/preview` tab
generated 57 tile requests across zoom levels 2, 3 and 4 in roughly 0.6 seconds.
Six failed with 503 in 0.223–2.784 ms. Zarr admission rejections increased;
render deadline and render queue rejection counters did not. The source budget
was the default 1 GiB; configured `MC_RENDER_MEMORY_MB=4096` controls a separate
output budget. The host had about 40 GiB available at the measurement time.

A controlled six-request burst bypassed the rendered-image cache with
`parameter-name=temperature_2m&datetime=2026-09-27T21%3A00%3A00Z`. Relative to
`/tiles/collections/dwd-icon-eu/tiles/WebMercatorQuad/`, the requests were
`3/3/5`, `3/3/3`, `3/3/4`, `3/2/3`, `4/5/7`, and `4/4/9`.
Four returned 200/MISS in 50.5–176.1 ms; the last two returned 503 in 3.3 and
1.0 ms. Sampling at approximately 25 ms observed 972.38 MiB reserved; the
instantaneous peak may have been higher. Both rejected tiles then succeeded
sequentially with 200/MISS in 74.6 and 79.4 ms. This isolates admission under
concurrency, not cold-S3 latency: decoded and compressed caches can still help
render-cache misses. Reservations returned to zero after the burst.

Startup diagnostics reported float32 inner chunks `[1,93,219,153]` inside
`[1,93,657,1377]` shards for all four ICON-EU parameters. A public metadata read
at snapshot `B3FKRVXJG1TVZ3RFGTN0` confirmed these shapes and one inner Blosc
stage (`cname=zstd`) for temperature and precipitation. Each 11.89 MiB decoded
chunk spans all 93 forecast leads. The 27 spatial chunks need 320.95 MiB per
parameter/run before entry overhead, while the default 256 MiB decoded cache
holds only 21. Broad views and parameter switches can therefore evict useful
chunks. Neither the serial-path change in #784 nor the stacked-Blosc estimate
change in #785 addresses this ordinary, single-Blosc inner-chunk layout.

The preview now fits the collection extent without animation before making
its layers visible on first enable. Re-enabling leaves the current view alone;
the explicit "Zoom to extent" action remains animated. A local browser smoke
check with the shipped MapLibre, ICON-EU manifest extent, and synthetic PNG
responses requested zoom levels 2/3/4 before the change and only level 4 after.
This removes intermediate-view work, but the six-request production result
shows that backend admission still needs attention.

Remaining operational experiments are a larger `MC_ZARR_READ_MEMORY_MB` and
ICON-EU decoded cache, with memory/latency monitoring and the same burst replay.
512 MiB can retain approximately one complete parameter/run; 1536 MiB can
retain approximately all four. These are sizing estimates, not tested production
settings. The original logs also contained 19 render timeouts near the default
three-second deadline; increasing that timeout alone cannot fix millisecond
admission failures. Cross-request admission and tighter estimates remain #777;
rejection-stage visibility, cold stage timings, and release-build concurrent
benchmarks belong to #778. Source-bound request filtering needs care where a
collection's reported extent changes with time or parameter.

## Remaining review work

1. [#777](https://github.com/mrauhala/meteocore/issues/777): encoded/intermediate allowances now release between stored chunks on serial reads, and Blosc scratch releases after each decoder call. Stacked Blosc chains prepay peak scratch rather than the sum of sequential calls. Further shortening within a single chunk's codec/index chain still needs an ownership proof. Coalesced decoded waiters avoid duplicate cold allowances, and replacement owners must pass admission. Common cold layouts prepay encoded/intermediate/scratch headroom; unknown or oversized estimates use serial actual-size admission. More precise size estimates and additional layouts can improve concurrency. Compressor-private contexts, other codecs, and coordinate/metadata discovery remain outside these explicit bounds.
2. [#778](https://github.com/mrauhala/meteocore/issues/778): extend layout diagnostics and decoded-cache metrics with requested/decoded-byte amplification, storage requests/bytes, and fetch/decode/resample timings; make a controlled comparison with GRIB on S3.
3. [#779](https://github.com/mrauhala/meteocore/issues/779): replace plain-Zarr whole-object shard reads with efficient range retrieval where appropriate. Parallelizing this backend or outer-transformed Icechunk shards needs a separate runtime/admission design.
4. [#780](https://github.com/mrauhala/meteocore/issues/780): handle arbitrary byte ranges in upstream Blosc getitem decoding. The pinned implementation rounds offsets/lengths down by the frame's typesize; a stacked gzip→Blosc(typesize=4)→zstd fixture fails even without our wrapper when the gzip stream is not element-aligned. The bounded adapter retains that existing behavior. Opaque compressed intermediates in the regression fixture use typesize=1 without shuffle; ordinary numeric Blosc reads remain element-aligned.

## Regression coverage

Blosc tests compare full/getitem reads across BloscLZ, LZ4, zlib, and zstd with no shuffle, byte shuffle, and bitshuffle; they cover multiple ranges, block boundaries, malformed headers, frame/representation mismatches, scratch/intermediate admission, and typed deadline errors. V2 Fortran layout and V3 inner/outer/nested sharding tests include Blosc. A local Icechunk map test commits oversized inner-frame headers, verifies rejection with decoded retention on/off, then repairs the payload and verifies refresh recovery.

Scratch-lifetime regressions decode eight full or partial Blosc reads inside one retrieval with capacity for a single scratch allowance plus 14 retained encoded bytes. Every call succeeds and returns to 14 admitted bytes, while an iterator gate verifies scratch is still admitted during getitem. A four-chunk plain-Zarr regression uses the real storage adapter and source budget, with room for source/native buffers, one chunk's encoded copies, and one scratch allowance. It verifies both cold reads and cached reads after deleting the backing chunk files; only source/native reservations remain after retrieval. Further tests cover overlapping scratch guards, prepaid-credit reuse, retained intermediate charges, invalid/overflowing ranges, panic cleanup, and async calls whose original scope ends while input is pending (success, deadline, cancellation, and invalid-range outcomes).

Serial-scope regressions use real plain storage with gzip, stacked gzip/zstd, ordinary shards, outer gzip, and nested shards. Full/partial reads include padded edges and absent chunks; tight-budget cold and cached reads return identical values, including after backing files are removed. Instrumented storage verifies the same operation sequence as upstream, one context throughout each chunk, release between chunks, caller-thread/deadline propagation, and restoration/cleanup on success, admission failure, storage error, panic, and deadline expiry. Independent V2 Fortran-order gzip/zstd/Blosc fixtures exercise cross-chunk output views. A local Icechunk regression uses external outer-compressed shards, payload caching on/off, full/partial reads, and map sampling.

In the 9×9 float32 fixture (4×4 stored chunks, eight present and one absent), a budget allowing the largest single-chunk temporary reservation succeeds where the former whole-window scope fails. The plain/stacked cases reserve 2,200 source/workspace bytes; the sharded cases reserve 2,456. Temporary allowances, excluding those source/workspace bytes:

| Layout | Former accumulated allowance | Reusable per-chunk allowance |
|---|---:|---:|
| Gzip | 956 B | 136 B |
| Gzip → zstd | 2,120 B | 298 B |
| Inner-gzip shards | 3,198 B | 404 B |
| Inner/outer-gzip shards | 9,590 B | 1,220 B |
| Nested shards with outer gzip | 120,292 B | 15,066 B |

These are controlled admission measurements, not RSS or an Icechunk-versus-GRIB latency benchmark. Within-chunk conservative intermediate allowances remain visible in the nested-shard result; no additional serial-path parallelism is introduced.

Codec tests cover gzip/zstd exact and bounded lengths, empty and malformed streams, excessive expansion, unknown-size and concatenated zstd frames, intermediate admission/release, and typed deadlines. Independent V2 Fortran-order fixtures preserve chunk keys and values. V3 inner/outer compression, nested shards, stacked codecs, partial reads, fill values, and metadata round-trip through bounded arrays. A map rejects a small gzip object that expands beyond its native chunk and releases its reservation; a fresh catalog can subsequently read repaired data.

Encoded-admission tests reject a large HTTP response from headers without receiving its body, then successfully retry the fill. They cover cached full/range/size reads, independent budgets for coalesced readers, retained codec-copy allowances, body deadline cleanup, and malformed body lengths. Icechunk tests reserve full and mixed-range reads from pinned references, reject before accessing deleted external payloads, preserve missing chunks, and reuse decoded cache entries without encoded admission. A tiny map rejects an oversized encoded object even when its native window fits. Gated worker tests cover scope propagation and release on success, failure, deadline expiry, panic, and waiter cancellation.

Plain-Zarr tests cover V2 and V3 metadata/coordinate updates, same-time payload corrections, render-version changes, failed rebuild/retry, retained old pixels, missing-key isolation, old-reader failure after eviction, shared byte budgets, and zero retention. Tests assert retryable errors for retired map/EDR reads and full/partial shard decoding, while cached corruption remains an engine error. A gated localhost HTTP server verifies that a download racing retirement is discarded with a typed retryable error rather than cached or returned.

Controlled HTTP tests verify that nine concurrent full/range/size readers use one GET for present and missing objects, waiter deadlines leave the owner running (including zero retention and oversized payloads), failed/expired owners allow retries, and different keys/generations proceed independently. A two-worker Tokio test holds the object response while checking that cache waiters allow unrelated runtime work to progress. Another test uses the actual `RenderJob::acquire_raster`/`run` blocking-pool boundary: eight readers share one HTTP GET, preserve the executor deadline, reject expired warm/cold reads, and release all render slots. Plain-Zarr map tests run 48 reads through that executor with retention enabled/disabled, verifying real async file I/O, gzip decoding, and pixels.

Network-free tests exercise payload retention with deleted backing files, zero-cache behavior, warm payload reuse across changed snapshots, expired and in-flight deadlines, multiple caller/runtime contexts, no-op refresh, failed refresh/retry, old readers, explicit snapshot/tag selection, same-time data correction, and irregular-grid map/position consistency.

Decoded-cache tests compare native cached and uncached values across chunks, shards, clipped edges, and missing chunks; cover int16 values/fill, eviction, oversized bypass, corrupt-codec failure/retry, snapshot/array identity, and a waiter whose deadline expires while another caller holds the fill guard. Icechunk tests reuse decoded chunks for a pan, another lead, and an EDR query after deleting external payloads with compressed caching disabled.

Cache-aware admission tests read warm windows with budgets too small for a cold workspace, including sharded and padded edge chunks. Deleted backing files prove these reads avoid storage. Eviction between admission and loading preserves the pinned buffer and its reservation. Gated storage tests exercise reduced cold and mixed batches, source ownership after copying, and rejection counters that exclude successful fan-out reductions.

Coalesced-admission tests gate a cold fill under a 1,492-byte budget: two 24-byte source buffers plus one 1,252-byte cold allowance use 1,300 bytes while waiting, leaving 192 bytes for the returned pin. The previous duplicate cold workspace could not fit; both reads now complete with one fill, identical values, and no rejected admission. A timed-out waiter leaves the owner running. Error/panic tests allow exactly one cold allowance and verify release before successor admission; a successor with only pin capacity rejects before I/O and releases its claim. A two-chunk test requires queued work to reach storage while the next fill is still owned by another caller.

Headroom tests cover metadata-only shard estimates, stacked codecs, Blosc scratch bounds, unknown layouts, prepaid-credit ownership/growth/deadlines, and serial fallback below conservative encoder bounds. In a gated fixture with a 5,592-byte budget and 2,520 source bytes, native-only admission reserved four 768-byte workspaces and left no room for encoded reads. The new path reserves two 1,252-byte native/headroom allowances (5,024 bytes total with source), completes the reads with identical values, and records no rejection. A local Icechunk fixture exercises actual gzip and Blosc shard reads under the same kind of native-only saturation.

Peak-scratch regressions cover one to three Blosc stages and overflow checks. A real plain-store chain of two Blosc stages decodes full/partial subsets with prepaid capacity for all actual encoded/intermediate bytes plus just one frame-derived scratch peak; the refunded peak can be reused after decoding, while one additional overlapping scratch byte fails admission. Gated cold reads exercise stacked Blosc on ordinary chunks and within shards, with full/partial subsets. At the same fixed budgets, two cold readers now run concurrently where the former summed estimate admitted only one, returning identical values without rejection:

| Four 192-byte native chunks | Source bytes | Former allowance per cold chunk | Peak-scratch allowance per cold chunk | Fixed total budget | Concurrent cold readers, before → after |
|---|---:|---:|---:|---:|---:|
| Blosc → Blosc | 4,608 | 4,872 | 3,276 | 11,160 | 1 → 2 |
| Same chain inside a four-entry shard | 4,608 | 5,264 | 3,668 | 11,944 | 1 → 2 |

The allowances include native workspace and headroom. Both chains retain 864 encoded/intermediate bytes and prepay 1,644 peak scratch bytes, rather than adding another 1,596 bytes for the sequential first stage. The sharded case also includes index workspace and encoded bytes. A local Icechunk fixture covers external stacked-Blosc shards with payload caching on/off and full/partial reads. These changes affect stacked Blosc chains; single-Blosc and other existing estimates retain their values. This is an admitted-concurrency comparison, not an end-to-end latency result.

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
