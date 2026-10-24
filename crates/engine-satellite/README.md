# engine-satellite

Serves geostationary satellite imagery as map layers (WMS, OGC API Maps,
Tiles) and EDR coverages: GOES-R ABI scans, Himawari-9 AHI scans published
as ISatSS tiles, GK2A AMI full-disk scans, and NOAA's hourly GMGSI global
mosaic.

## What it solves

Agencies publish each scan as large NetCDF-4 files in public object
storage (about 25 MB per band for a GOES full disk, 88 tiles for a
Himawari one), on the satellite's view geometry or a Mercator grid only
2-D lat/lon arrays describe; GK2A's hold raw instrument counts. This
crate keeps a short time window of scans in memory and serves any part
of them, in any supported output CRS, fast enough for map animation:
while the window fits in memory, no request reads the bucket.

## How it works

- **One collection is one satellite and sector** (or the GMGSI mosaic).
  Each `[[satellite.products]]` entry (a band, or an L2 field such as cloud
  top temperature) is one parameter with its own time axis. The
  collection's time axis is their union.
- **Discovery.** A background poll loop lists the source every
  `poll_interval_secs`: in a bucket the prefixes `time_window` covers
  (hourly for GOES-R, GMGSI and GK2A, the ten-minute scan directories not
  yet ingested for ISatSS), a local directory recursively, skipping hidden
  folders. File names select scans. A scan's time is its start truncated
  to the minute, so `19:00` finds the scan that started about 20 s later;
  a GK2A scan's is the nominal slot its file name carries.
- **Ingest.** Each new scan is downloaded whole, newest first, at most 4
  per product per poll, and none starts once the poll has spent two
  minutes ingesting: the rest wait for the next poll, so a stalled source
  does not hold the catalog. The compressed file stays in memory together
  with an overview of every 4th pixel built at ingest. A Himawari scan's 88
  tiles are fetched 16 at a time and placed on one lattice from their
  coordinates. A tiled scan waits until it has as many tiles as earlier
  scans, a newer scan appears, or 15 minutes after its start.
- **Memory.** Scans live in one byte-bounded cache shared by every
  satellite collection; each poll drops those that left the window. While
  all windows fit, the poll downloads again any in-window scan the cache
  evicted; otherwise only into free room, with one WARN per poll.
- **Rendering.** Output pixels map to source pixels through a coarse
  `ProjectionGrid`; each takes the source pixel under it. Only the blocks
  a render touches (one NetCDF chunk, or one ISatSS tile) are decoded, and
  cached. A render spanning 4 or more source pixels per output pixel on
  either axis samples the overview instead. CF packing and
  `_FillValue`/`valid_range` apply; space beyond the limb is no data.
- **Projections.** GOES-R and ISatSS use the file's CF geostationary grid
  mapping. GMGSI has none: its lat/lon arrays are checked at ingest to
  form a regular global EPSG:3857 grid whose columns wrap around the
  globe. GK2A files are not CF: their grid comes from the CGMS navigation
  attributes, checked against the corner scan angles the file states.
  GOES-West's, Himawari's and GK2A's extents cross the antimeridian.
- **Native grid.** The default parameter's (first product's) grid, from
  its first scan, is the collection's native grid (#1237). The collection
  description's `meteocore:nativeGrid` member gives it in the view's
  metres (scan angle × satellite height) with its WKT2. Other bands may be
  finer. GMGSI's Mercator is recognised, not stated, so its member's CRS is
  WKT2 too, while `storageCrs` stays EPSG:3857.
- **GK2A calibration.** A GK2A file stores 16-bit words, each a quality
  flag and a count. A table built per scan from the file's own
  calibration coefficients maps counts to brightness temperature (K);
  only pixels flagged good are served.

## What it serves

| Trait | APIs | Content |
|---|---|---|
| `MapEngine` | WMS, Maps, Tiles | One layer per product, plus RGB composites |
| `EdrEngine` | EDR | Products only: `position`, `area`, `radius` |

**Maps.** Each product advertises its own time axis. A request renders
the latest scan at or before its time, the first scan when it is earlier
than all of them, and the newest scan without a time; before a product's
first scan the tile is empty. **RGB composites** are layers of their own,
not parameters (WMS `<collection>/<composite>`, Maps and Tiles
`parameter-name=<composite>`). Every band a composite reads comes from the
same scan, and its time axis holds only the scans every band has.

**EDR.** `position` is a time series of the pixel under the point (off the
disk: 404). `area` is a CRS84 grid at the finest selected product's nadir
pixel size, at most 256 cells per axis, null outside the polygon; `radius`
is an area over the circle. The time axis is the union of the selected
products' scans, null where a product lacks one. A `datetime` instant
matches only a scan at exactly that time; no scan in range is 404. A
query may download 8 evicted scans and decode about 133 million pixels of
blocks at most (1024 GOES-R 2 km strips, or 70 GK2A chunks), else 400. No
named locations, vertical levels or model runs.

## Configuration

Runnable examples in `collections.d/`, all on NOAA's public buckets:
`goes19-fd.toml`, `goes18-fd.toml` (crosses the antimeridian),
`himawari9-fd.toml`, `gk2a-fd.toml` (crosses the antimeridian),
`gmgsi-global.toml` and `goes19-fd-rgb.toml` (both recipes). Inline in
`config.toml` the section is `[collections.satellite]`.

```toml
id = "goes19-fd"
title = "GOES-19 full disk"
description = "GOES-19 ABI full-disk imagery every 10 minutes"
engine_type = "satellite"
apis = ["edr", "wms", "maps", "tiles"]

[satellite]
provider = "goes-r"     # "goes-r" | "isatss" | "gmgsi" | "gk2a"
endpoint = "https://s3.us-east-1.amazonaws.com"
bucket = "noaa-goes19"  # or data_path = "<directory>"
time_window = "-PT1H"
poll_interval_secs = 60

[[satellite.products]]
parameter = "ir_10_3"
title = "IR 10.3 µm brightness temperature"
unit = "K"
product = "ABI-L2-CMIPF"
band = 13
variable = "CMI"
```

| Field | Default | Meaning |
|---|---|---|
| `provider` | `"goes-r"` | `goes-r` (ABI), `isatss` (Himawari AHI tiles), `gmgsi` or `gk2a` (AMI L1B) |
| `data_path` | – | Local directory, listed recursively; or `endpoint` + `bucket` |
| `endpoint`, `bucket` | – | A public S3 bucket; requests are unsigned |
| `time_window` | – | e.g. `"-PT1H"`. Required for a bucket: at most 24 h (`goes-r`, `gmgsi`, `gk2a`) or 6 h (`isatss`). Without it a directory serves every scan |
| `poll_interval_secs` | `60` | Must be > 0 |

A product needs `parameter` (`^[a-z0-9_]+$`, unique), `title`, `unit`
(declared, because styles resolve before the first scan), `product` and
`variable`; `band` (1–16) picks one band of a per-band product.

- `goes-r`: e.g. `product = "ABI-L2-CMIPF"` with a `band` and
  `variable = "CMI"`, or `"ABI-L2-ACHTF"` with `variable = "TEMP"`.
- `isatss`: `product = "HFD"` (full disk), `band` required,
  `variable = "Sectorized_CMI"`.
- `gmgsi`: `product` is `LW`, `SW`, `WV` or `VIS`, no `band`,
  `variable = "data"`. The values are 8-bit display counts (0–255) though
  the file says `units = "K"`: declare `unit = "1"`.
- `gk2a`: `product = "FD"` (full disk), `band = 13` (IR105, the only
  channel served) and `variable = "image_pixel_values"`, `unit = "K"`.

A `[[satellite.composites]]` entry has a unique `name` (`^[a-z0-9_]+$`,
not a product's), an optional `title`, and `red`, `green` and `blue`
channels of `{ parameter, minus, min, max, gamma }`: `min` gives intensity
0 and `max` full (`min > max` inverts), `minus` makes a band difference,
`gamma` defaults to 1. With ABI bands 8, 10, 12 and 13 named as in
`goes19-fd-rgb.toml`, this is the Airmass RGB:

```toml
[[satellite.composites]]
name = "airmass"
red   = { parameter = "wv_6_2", minus = "wv_7_3", min = -26.2, max = 0.6 }
green = { parameter = "ir_9_6", minus = "ir_10_3", min = -43.2, max = 6.7 }
blue  = { parameter = "wv_6_2", min = 243.9, max = 208.5 }
```

A `recipe` instead of the three channels uses satpy's values
(`ds_core::satellite_recipes`), finding each band by the products' `band`
number, in `K`. Recipes exist for `goes-r` (ABI) and `isatss` (AHI) only:
`airmass` reads ABI bands 8, 10, 12, 13 (AHI 8, 10, 12, 14),
`night_microphysics` ABI 7, 13, 15 (AHI 7, 13, 14, 15). Unknown keys in a
composite are a load error (`ds_core::config::validate_satellite`).

`MC_SATELLITE_FRAME_CACHE_MB` (default 1024) bounds the scans in memory
for all satellite collections together: size it above their windows
combined. `MC_SATELLITE_STRIP_CACHE_MB` (default 256) bounds decoded
blocks. `/health` reports a collection degraded until its first complete
poll, when no poll has listed every product for 5 poll intervals (at
least 10 minutes), or while a product has no scan in the window.
`/metrics` has `satellite_frame_cache_*`, `satellite_strip_cache_*`,
`satellite_frame_window_bytes`, `satellite_frame_window_resident_bytes`
and `satellite_frame_reingests_total`.

## Limits

- Only these four file layouts are read. A field must be stored as
  `short` or `ushort`, or as `float` holding whole counts, on a north-up
  grid; a Mercator grid must span the globe.
- Every scan is downloaded whole: about 20–30 MB per GOES product scan,
  26 MB per Himawari band scan, 35 MB per GK2A IR105 scan, 7.4 MB per
  hourly GMGSI longwave file. The first polls after start ingest the
  whole window.
- A scan missing from memory is downloaded within the request's deadline,
  so an animation whose frames do not fit the cache can return 503.
- GK2A serves only band 13 (IR105) of the full disk: converting another
  channel's counts needs its central wavenumber, which the files do not
  carry.
- Composites have no values of their own, so EDR does not serve them.

## Testing

```bash
cargo test -p engine-satellite
cargo test -p server --test satellite_edr --test satellite_composites
```

The fixtures are small cuts of real scans with the original packing,
each with a README: `testdata/goes19-abi` (band 13 across the limb, cloud
top temperature), `testdata/goes18-abi` (band 13 across the
antimeridian), `testdata/himawari9-isatss` (three tile crops around a
lattice cell with no tile), `testdata/gk2a-ami` (an IR105 window across
the antimeridian with its original count words) and `testdata/gmgsi` (a
decimated longwave mosaic, still a global Mercator grid). `tests/gk2a.rs`
checks pixel positions against PROJ and temperatures against an
independent calibration. `tests/refetch.rs` and
`tests/window_capacity.rs` use zero-size and 1 MB caches to cover
eviction and re-download. Contributor rules: [CLAUDE.md](CLAUDE.md).
