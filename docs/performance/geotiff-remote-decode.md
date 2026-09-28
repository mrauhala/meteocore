# Remote GeoTIFF decode after compressed-cache hits (#468)

The WMS meta-tile loop renders one projected viewport as dozens of 256 px
`get_raster_tile` calls. Adjacent meta-tiles cover the same 512 px COG
source tiles, so a remote COG decompressed and boxed each source tile
several times per frame even when every compressed tile was already cached
(#463 fixed this for local files only). This page records the measurement
that decided #468 and the result of extending the decoded-chunk cache to
remote sources.

## Setup — 2026-09-28 UTC

- Source: public CloudFerro `openradar-24h`,
  `2026/09/28/OPERA/COMP/OPERA@20260928T0940@0@DBZH.tiff`, 3,547,373 bytes,
  ETag `60d88f2547daf6920121c590ac80b80d`. 3800 × 4400 at 1 km, ETRS89-LAEA,
  512 × 512 DEFLATE tiles, two Float32 bands (DBZH + quality), overviews
  1900 × 2200 … 237 × 275.
- Collection: `collections.d/radar-eu-composite-dbzh-s3-cog.toml` with
  `time_window = "-PT1H"`; release build, Apple M2 Max (12 cores), one
  sequential client, `MC_RENDER_TIMEOUT_MS=60000` so the cold priming frame
  could finish.
- Every warm frame must miss the rendered-image and meta-tile caches but hit
  the compressed tile cache, as a new style, zoom level or an evicted
  meta-tile does in production. So `[wms] rendered_cache_mb = 0` and
  `[server] metatile_cache_mb = 1`: meta-tiling stays on (capacity > 0) but
  retains almost nothing. Every warm frame below re-rendered all of its
  meta-tiles.
- Requests: `GET /wms?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=radar-eu-composite-dbzh-s3-cog&STYLES=&CRS=EPSG:3857&FORMAT=image/png&TRANSPARENT=TRUE&TIME=2026-09-28T09:40:00Z&WIDTH=1920&HEIGHT=1080&BBOX=…`
  with three viewports:

  | Viewport | BBOX, EPSG:3857 m | Meta-tiles |
  |---|---|---:|
  | `europe_z5` | `-3587000,4530000,5813000,9810000` | 84 |
  | `central_z6` | `-1200000,5500000,3500000,8140000` | 84 |
  | `regional_z7` | `494000,6140000,2844000,7461000` | 45 |

- One priming request per viewport, then ten warm requests.
- Timings: the WMS slow-render log fields `render_ms`, `tile_loop_ms`,
  `assemble_ms` and `encode_ms`, with `SLOW_RENDER_LOG_MS` temporarily set
  to 0. For the baseline, temporary counters in `engine-geotiff`, since
  removed, split each `get_raster_tile` into: remote window read
  `read_bbox_parallel` wall time; decompress + predictor and full-tile
  `Option<f64>` boxing, both CPU time summed over fetch-pool workers;
  window copy; fetch time; and the number of distinct source tiles decoded.
  After the change, `/metrics` `geotiff_decoded_chunk_cache_*` deltas per
  request.

## Baseline: decode after compressed hit

Medians of 20 warm frames, two runs of ten. All tiles were compressed-cache
hits: fetch time 0.

| Viewport | Render ms, min–max | Window read ms | Decompress CPU ms | Boxing CPU ms | Copy ms | Tile decodes / distinct |
|---|---|---:|---:|---:|---:|---:|
| `europe_z5` | 185.0, 172–226 | 122.6 | 153.8 | 66.8 | 7.6 | 147 / 35 |
| `central_z6` | 234.5, 219–344 | 154.1 | 175.9 | 67.7 | 3.6 | 149 / 18 |
| `regional_z7` | 123.5, 116–153 | 74.1 | 95.2 | 44.5 | 3.2 | 99 / 15 |

The remote window read was 60–66 % of each warm render, and apart from a
3–8 ms copy it was all decompression and boxing of tiles already decoded
earlier in the same frame: each distinct source tile was decoded 4.2×, 8.3×
and 6.6× per frame. Meta-tile assembly (5–7 ms) and PNG encoding (1–2 ms)
were minor. One two-band Float32 tile costs about 1.0 ms to inflate and
0.45 ms to box on this machine. The cold priming frame (`europe_z5`,
3.5 s) was dominated by range fetches (#46).

That is well above the 15–20 % bar in the issue, so #468 implements the
issue's option (a).

## Change

- Remote tiles decode to a native one-band chunk instead of a boxed
  `Vec<Option<f64>>` of the whole tile. Nodata and scale/offset are applied
  only to the copied window, as on the local path.
- The chunk is memoized in the process-global decoded-chunk cache, keyed by
  object path, the engine's compressed-cache namespace, band, IFD and chunk,
  within the shared `MC_GEOTIFF_DECODED_CHUNK_CACHE_MB` budget. A two-band
  OPERA tile is cached as its one band, 1 MB. A warm `europe_z5` frame
  touches 35 tiles, ≈ 35 MB.
- Remote lookups are plain get + insert rather than single-flight, so a
  fetch-pool worker never waits on another request's range read.

## Result

Same setup and file. Medians of 20 warm frames, two runs of ten; the
cache-disabled column is one run of ten with
`MC_GEOTIFF_DECODED_CHUNK_CACHE_MB=0`, which isolates the removal of
full-tile boxing.

| Viewport | Before: render ms, min–max | After | Change | After, cache disabled |
|---|---|---|---:|---|
| `europe_z5` | 185.0, 172–226 | 74.0, 67–104 | −60 % | 180.0, 176–204 |
| `central_z6` | 234.5, 219–344 | 82.0, 81–85 | −65 % | 221.5, 220–225 |
| `regional_z7` | 123.5, 116–153 | 53.0, 51–62 | −57 % | 114.0, 113–133 |

- The meta-tile loop, `tile_loop_ms`, fell from 177 / 226.5 / 113.5 ms to
  65 / 73 / 42.5 ms. Assembly and encoding were unchanged.
- Every warm frame was served entirely from decoded chunks: 147, 149 and 99
  hits, 0 misses per frame. The first, cold `europe_z5` frame decoded its
  35 distinct tiles once each: 35 misses, 112 hits, where the baseline
  decoded 147 times.
- Decoded-cache residency after all three viewports: 53 chunks, 55.6 MB,
  about 1.05 MB per OPERA tile.
- Without retention the change is worth only 3–8 %: the boxing it removed
  ran in parallel on the fetch pool, so most of it was not on the critical
  path. The saving comes from not decoding again.
- Output is byte-identical: the three PNGs hash to the same SHA-256 from
  both builds.
- Cold frames are dominated by range fetches and vary with the origin: 3.6
  to 5.3 s for `europe_z5` across runs. They are not compared here.

After deploying, a remote collection's `tile_cache_hits_total` falls:
repeat reads are now answered by `geotiff_decoded_chunk_cache_hits_total`
before the compressed cache is consulted. The compressed cache still serves
tiles whose decoded chunk was evicted, and it still prevents refetching.

## Reproducing

1. In a scratch copy of `config.toml`, add `metatile_cache_mb = 1` under
   `[server]` and point `collections_dir` at a directory holding a copy of
   `radar-eu-composite-dbzh-s3-cog.toml` with `rendered_cache_mb = 0` added
   under `[wms]`.
2. To get per-request phase logs, set `SLOW_RENDER_LOG_MS` in
   `crates/api-wms/src/handlers.rs` to 0 temporarily, then
   `cargo build --release -p server`.
3. Start the server:
   `MC_RENDER_TIMEOUT_MS=60000 RUST_LOG=info target/release/server --config <scratch config> --collections=radar-eu-composite-dbzh-s3-cog --port 18468`.
   Add `MC_GEOTIFF_DECODED_CHUNK_CACHE_MB=0` for the cache-disabled column.
4. For each viewport, send the GetMap request once to prime the caches,
   then ten more times. Read `render_ms`, `tile_loop_ms`, `assemble_ms` and
   `encode_ms` from the `slow WMS meta-tile render` log line, and
   `geotiff_decoded_chunk_cache_{hits,misses}_total` from `/metrics` before
   and after each request.
