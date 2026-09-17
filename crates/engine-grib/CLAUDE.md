# engine-grib crate — Claude Instructions

GRIB2 NWP data engine. Read the root `CLAUDE.md` first — Critical Rules 6–7
and 9 (the new-run probe once did 32 sequential blocking reads on one
thread) were learned here.

Only regular lat/lon grids (Template 0). Multi-parameter collections —
unlike GeoTIFF's one band per collection.

## Discovery & sources

- Discovers data via **index sidecar files** on S3/HTTP or a local
  directory; fetches messages via byte-range reads. The engine NEVER builds
  indexes itself.
- **Data source (mutually exclusive):** remote
  `endpoint`+`bucket`+`prefix_pattern` (S3 with strftime/run-hour date
  templating), or local `data_path` (a directory of `.grib2` + index
  sidecars; also accepts an `s3://`/`http(s)://` fixed-prefix URL). For
  `data_path`, `prefix_pattern` is optional and literal (no date templating);
  index/data files must share a basename (`X.index` ↔ `X.grib2`).
- **Index formats** via `index_format`: `"ecmwf-json"` (default, JSON-lines
  as shipped by ECMWF open data) and `"wgrib2"` (colon-separated text as
  shipped by NOAA GFS).
- Wgrib2 indexes carry only byte offsets — the last record's length is
  resolved via `DataStore::head()`. If HEAD fails or the size suggests a
  partial upload, the index is skipped and retried next poll.
- Parameter metadata populates lazily: `scan_once` runs a bounded
  eager-probe (≤32 messages per scan) across the newest run's step
  files so `/collections` metadata is ready by the first poll cycle.

## Unit conversion (source-driven — never hardcode parameter names)

- Conversions are driven by the WMO `(discipline, category,
  parameter_number)` triple read from every decoded message, not by
  short-name tables. Source units come from WMO Code Table 4.2
  (`src/units.rs`) plus per-center overlays for local parameter numbers
  192–254.
- Display conversions are mechanical: K→°C, Pa→hPa, kg m⁻²→mm, m² s⁻²→gpm,
  proportion→%. Colormap ranges use display units. The string-keyed twin
  of this table lives in `ds_core::units` (used by engine-bufr); keep the
  two rule for rule.
- **Per-provider vocabularies are not needed.** A new provider only needs
  overlay entries if it uses local parameter numbers. ECMWF-`tcc` vs
  GFS-`TCDC`, `z` vs `HGT` are handled by construction (different triples).

## Model runs (#337)

Catalog keeps a `runs` map (`BTreeMap` keyed by reference time) and
implements the shared `ds_core::instances` contract (see root CLAUDE.md).

## v1 limitations (GFS)

- Only regular lat/lon grids (Template 0) — gaussian-grid products
  (`gdas.*`) fail loudly.
- Hour-window `acc fcst` and `ave fcst` records are preserved (#80).
  They use distinct keys (`APCP_acc_6h`, `DSWRF_avg_6h`), so two window
  lengths or an instantaneous field at the same valid time cannot collide.
  The `MessageEntry` retains the original start/end; EDR reports the window
  end and labels carry the duration. Config `parameters = ["APCP"]` includes
  its window variants; an exact key selects one duration. Discover keys
  from the collection before querying. Existing max/min keys remain unchanged.
  Repeated identical catalog keys at different offsets are ambiguous (the real
  GFS fixture has two `APCP_acc_6h` surface records). Scan logs a warning with
  both offsets; queries preserve first-record selection. The index alone cannot
  establish payload equivalence or expose an omitted product discriminator.
  Log the optional vertical level as `grib_level`, never `level`: flattened
  JSON reserves `level` for severity, and a duplicate key hides WARN in Loki.
- Source units still come from the decoded WMO triple: no automatic division
  by window length. Precipitation kg/m² displays as mm; already-averaged flux
  W/m² stays W/m², and energy in J/m² stays energy. ECMWF JSON sidecars retain
  their existing naming/semantics; this change does not infer missing windows.
- Parameter discovery and bounded metadata probes include every step in the
  newest run: the analysis step commonly has no aggregates. An aggregate
  absent from a time step is a null in a position series, never a zero.
  A wgrib2 index with mixed window-end times is rejected rather than assigning
  all its records the first record's time.
- Strongly advise a `parameters` filter with `index_format = "wgrib2"` — a
  single GFS 0.25° file has ~700 messages.
- CCSDS/AEC compression needs the `libaec` C library (via `libaec-sys`).

Default Maps/area parameter selection preserves the first previously supported
near-surface product before considering acc/ave records, regardless of index
ordering. Aggregate-only collections fall back to their first aggregate;
upper-air-only collections to their first message. Raster metadata and actual
rendering share `StepFile::default_message` so default labels/units agree.
