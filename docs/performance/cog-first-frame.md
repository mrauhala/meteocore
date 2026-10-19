# First view of a new remote COG frame (#1004)

On a live deployment a remote OPERA reflectivity COG collection caused 62
of 117 WMS 503s in a week, every one at the 3000 ms render deadline,
typically on the first request for a newly published frame. This page
records where that first render spends its time and what the poll-time
pre-warm changes.

## What the failing requests look like

- A map client asks for each new frame with an explicit `TIME`, on a fixed
  five-minute schedule: at 15:35:00 for 15:30, at 15:40:00 for 15:35. The
  collection's poll had catalogued each of those frames about 30 s earlier:
  polls ran at :m4:30 and :m9:30.
- The view is 2848 × 1405 EPSG:3857 at zoom 7–9 over northern Europe, for
  example `BBOX=811446.90,7675481.28,4294529.40,9393785.68`: 1223 m per pixel in
  Mercator, about 610 m on the ground at 60° N. OPERA's grid is 1 km, so
  every meta-tile of these views reads **full-resolution** tiles. A
  pre-warm of the overviews alone would not have helped them.
- Bursts of 503s in one second were animations of the last three frames.
- Successful renders of the same layer logged as slow, at least 400 ms, over
  the same week: median 682 ms, p90 1107 ms, p99 1691 ms, maximum 2965 ms.
  The slow ones re-rendered all 84 meta-tiles.

## The OPERA file

One header read of `OPERA@20261010T0330@0@DBZH.tiff`, 4,018,922 bytes, from
the public `openradar-24h` bucket:

| IFD | Size | Tiles | Encoded bytes | Largest tile | File offsets |
|---:|---|---:|---:|---:|---|
| 4 | 237 × 275 | 1 | 32,046 | 32,046 | 5,677–37,723 |
| 3 | 475 × 550 | 2 | 94,783 | 91,148 | 37,731–132,522 |
| 2 | 950 × 1100 | 6 | 337,133 | 96,928 | 132,530–469,703 |
| 1 | 1900 × 2200 | 20 | 1,347,570 | 284,698 | 469,711–1,817,433 |
| 0 | 3800 × 4400 | 72 | 2,200,909 | 174,054 | 1,817,441–4,018,918 |

Two Float32 bands, pixel-interleaved, DEFLATE, 512 × 512 tiles, no empty
tiles. Each level is contiguous in the file and its tiles sit 8 bytes apart,
GDAL's tile leader and trailer, so one coalesced read covers many tiles. The
whole frame is 4.0 MB of tiles.

The catalog's 512 KB header read already downloads IFDs 2–4 and the start
of IFD 1 and throws them away. Keeping them would save those reads; it is
left for later because the header read is being changed separately (#1003).

## Round trips from the deployment

Range reads of this object from inside the server's container: TCP connect
19 ms; first byte 63–84 ms on a new TLS connection and 21–26 ms on a reused
one; a whole 1 MB range in 30–44 ms. Bandwidth is not the limit. Round trips
are.

## Where a cold first view spends its time

The header, the IFDs and the overview tile offsets are read and parsed by the
poll, not the request; overview selection is arithmetic. What the request
pays is the WMS meta-tile loop (`ds_render::metatile`): it renders the
viewport's 256 px tiles **one after another**, each a `get_raster_tile`. A
meta-tile fetches its own missing source tiles in parallel on the fetch
pool, but the next meta-tile waits for it. So a cold view costs one storage
round trip, plus a decode, for every meta-tile that reaches a source tile no
earlier one fetched, back to back. `MC_COG_TILE_CONCURRENCY` cannot shorten
that chain.

Reproduction: the ignored `prewarm_tests::benchmark_first_view_over_latency`
serves a COG through a local HTTP origin that delays every request, and
walks the view above the way the meta-tile loop does. The file is a
synthetic OPERA-like COG with the real file's grid, CRS, tiling, levels and
band layout; its tiles compress smaller, 1.5 MB in all. Release build, Apple
M2 Max.

| Origin latency per request | Cold first view | Origin requests | Repeat view | Pre-warm | Pre-warmed first view |
|---:|---:|---:|---:|---:|---:|
| 25 ms | 402 ms | 14 | 81 ms | 90 ms, 10 requests | 101 ms, 0 requests |
| 80 ms | 1001 ms | 14 | 69 ms | 252 ms, 10 requests | 76 ms, 0 requests |

The cold view minus the repeat view is about 12 round trips at either
latency: 12 of the 14 source tiles are fetched one after another. On the
deployment the same chain met slower reads under load. A cold view that
renders in 1.5–3 s where its warm re-render takes about 0.6 s works out at
70–180 ms per read, and a few slower reads push it past the 3 s deadline.

A render cut off by the deadline is not wasted: every tile it fetched is
cached as it lands, and only reads still in flight are dropped, so the
client's retry continues from there. That is why later requests for the same
frame succeed. `a_deadline_cut_read_keeps_fetched_tiles_for_the_retry` pins
it.

## Change

When a poll catalogues a remote frame, it reads the frame's encoded tiles
into the collection's compressed tile cache before the next poll
(`src/prewarm.rs`):

- the newest 4 frames that were not in the previous catalog; the poll loop
  also warms the startup catalog's newest frames before its first sleep;
- levels coarsest first while the frame stays within `MC_COG_PREWARM_MB`
  (default 32 MiB, `0` turns it off) and an eighth of the tile cache, so a
  file too large for the cap still gets its overviews. An OPERA frame is
  4 MB and fits whole;
- tiles coalesced with the request path's planner, at most 16 per read and
  1 MiB per read, 4 reads in flight on the poll runtime's blocking pool,
  60 s at most per poll;
- each read is charged to the decode budget as background work, which never
  takes the half of the budget that requests keep and does not count as a
  rejection. While requests hold more than half, a read waits up to 2 s for
  them, so a burst of renders at poll time delays the warm instead of
  skipping it, and is then left to the request path and reported in a WARN
  line.

Only the compressed cache is filled. A decoded OPERA frame is about 100 MB of
the decoded-chunk cache that every GeoTIFF collection shares, and pre-warming
one per poll would evict other collections' chunks, while the request path
decodes the 10–20 tiles a view touches in milliseconds.

The frame is published before it is warmed. The client asks for a new frame
about 30 s after the poll publishes it and the warm takes a fraction of a
second, and a request that races it still finds the tiles that have landed.

Each poll that warms something logs
`Pre-warmed N new frame(s) for first views: T tiles, B in R range reads, M ms`.
After deploying, the collection's `tile_cache_bytes` holds the newest frames
and the cold-render 503s and slow first views should disappear; the
remaining first-view cost is the decode and render, about 0.6 s for this
viewport on the deployment.

## `MC_COG_TILE_CONCURRENCY`

The fetch pool is shared by every render. One cold meta-tiled render keeps
only the new tiles of one meta-tile in flight, 1–4 for these views, so 16
threads are enough for a few concurrent cold renders even at 80 ms per read,
and more threads do not make such a first view faster. A render that is not
meta-tiled, WMS in EPSG:4326 or a Maps image, asks for all its missing tiles
in one call and does wait on the pool when they outnumber its threads. Keep
the default of 16.
Raise it to 32–64 only on a high-latency store where many renders run cold
at once and wait for pool threads; the threads mostly wait on the network,
so the cost is small. With the pre-warm, the new frames that clients ask for
most no longer reach the pool at all.

## Reproducing

1. Get a COG with the same layout. Either download one recent DBZH frame,
   about 4 MB, from `https://s3.waw3-1.cloudferro.com/openradar-24h/<yyyy>/<mm>/<dd>/OPERA/COMP/`,
   or write a synthetic one with GDAL: a 3800 × 4400, two-band Float32
   raster in `+proj=laea +lat_0=55 +lon_0=10 +x_0=1950000 +y_0=-2100000 +ellps=WGS84`
   with origin −500, 500 and 1000 m pixels, written with
   `gdal_translate -of COG -co COMPRESS=DEFLATE -co BLOCKSIZE=512 -co INTERLEAVE=PIXEL`.
2. Run, once per latency:

   ```sh
   MC_COG_FIRST_FRAME_FILE=/path/to/frame.tif MC_COG_FIRST_FRAME_LATENCY_MS=80 \
   cargo test --release -p engine-geotiff --lib -- --ignored \
     benchmark_first_view_over_latency --nocapture
   ```

   and read the `FIRST_FRAME` lines.
