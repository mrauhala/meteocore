# engine-satellite — geostationary satellite imagery

Read the root CLAUDE.md. Epic #819 holds the plan, the provider survey
(GOES, Himawari, GK2A, MTG, GMGSI) and the licensing decisions.

## Load-bearing rules

- **One collection = one satellite + sector; one parameter = one product**
  with its own time axis. `parameter_times` and `resolve_parameter_time`
  MUST go through `SatelliteEngine::select`, the same selection
  `get_raster_tile` renders with (latest scan at or before, first when
  earlier, newest for `None`). Drift reintroduces the #507 cache poisoning.
  `RasterInfo.times` is the union over products.
- **Scan time = scan start truncated to the minute** (`naming.rs`). A
  full-disk scan starts ~20 s past its ten-minute slot; keying on the
  second would make a nominal `TIME=…T19:00:00Z` snap to 18:50.
- **Storage type ≠ value signedness.** GOES-R CMI is stored `short` with
  `_Unsigned = "true"`. Read strips with the *storage* type (hdf5-reader 0.9
  rejects a signedness mismatch), carry values as `u16` bit patterns, and
  interpret them per `_Unsigned`. `_FillValue`/`valid_range` are in the
  storage type: a `short` fill of −1 is `0xFFFF` (`frame.rs` `as_raw`).
- **Strips are the chunk rows** (`Dataset::chunks()`, via hdf5-reader on the
  same in-memory storage as netcdf-reader, so no second copy). Reading a
  strip inflates each chunk once; the reader's own chunk cache is off
  because decoded strips are cached in `cache::STRIPS`.
- **No request-time S3 in the steady state.** The poll loop downloads each
  new scan whole and inserts it into `cache::FRAMES`. A render only fetches
  a scan the cache evicted, on its blocking worker with
  `Handle::try_current()` → `DataStore::get_on` (Critical Rule 7).
- **Ingest is capped per poll** (`MAX_INGEST_PER_POLL`, newest first):
  bootstrapping a window spreads over polls (Critical Rule 9).
- **No footprint guard.** `Crs::Geostationary::forward` is NaN behind the
  Earth, so no coarse projection cell can alias far-away output onto the
  disk; `ProjectionGrid` refines cells on the limb (#821). The collection
  extent is the seam-aware union of product extents (`union_extent`).
- **Overview**: a render whose source window spans ≥ `OVERVIEW_FACTOR`
  (4) source pixels per output pixel samples the ingest-time overview
  instead of decoding strips. A full-disk decode is ~160 ms.

## Config

`[satellite]`: `provider = "goes-r"`, `data_path` XOR `endpoint`+`bucket`,
`time_window` (required for a bucket, ≤ 24 h because prefixes are hourly),
`poll_interval_secs`, and `[[satellite.products]]` with `parameter`, `title`,
`unit` (declared: styles resolve at load, before any scan), `product`,
`band`, `variable`. Validation: `ds_core::config::validate_satellite`.

## Bandwidth

Each scan is downloaded whole: GOES-19 band 13 ~24 MB, cloud top temperature
~30 MB per 10 minutes; startup ingests the whole window. The user is often
on a metered connection — never run a bucket-backed collection for tests
without asking; use the local fixtures.

## Fixtures

`testdata/goes19-abi/OR_ABI-L2-{CMIPF-M6C13,ACHTF-M6}_G19_s20262681900199_*.nc`:
real GOES-19 scans cropped to 240×320 (C13 across the NE limb, ACHT from the
disk interior) with the original packed integers, chunking and compression;
the global `meteocore_fixture` attribute records each crop. Tests copy them
into a nested temp directory (local discovery lists recursively).

## Not yet

EDR (position/area, per-parameter `extent` in `parameter_names`) is phase
2c. Other providers (Himawari ISatSS µrad tiles, GK2A CGMS navigation,
MTG) are phases 3 and 5.
