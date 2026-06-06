# QueryData (.sqd) test fixtures

These are **`qdcrop` subsets** of full FMI QueryData files, shrunk so they can be
committed and so the engine tests actually run (they previously skipped because
the multi-GB originals weren't committed). `qdcrop` is part of the FMI
`smartmet-qdtools` package (`brew install` / build from source).

The full originals are FMI open data and re-downloadable if a larger fixture is
ever needed.

## `ecmwf-kenya/` — WGS84 lat/lon surface fields

Exercises the geographic (WGS84) parse + EDR/Maps path.

```bash
qdcrop -G 34,-5,42,5 -d 2x2 -p Pressure,Temperature,Precipitation1h -T 0,9,3 \
       <full_ecmwf_kenya_surface.sqd> 202604042019_202604040600_ecmwf_kenya_surface.sqd
```

- 486 MB → ~16 KB
- Kenya box, decimated 2×, params msl/2t/precip (in that order), times +0/+3/+6/+9h
- Result: 3 params, 16×21 grid, 4 timesteps

## `meps/` — Lambert Conformal Conic surface fields

The **only** fixture exercising the projected (LCC) parse path
(`parse_meps_lcc`). `qdcrop` preserves the source projection (no `-P`).

```bash
qdcrop -G 10,60,20,65 -d 2x2 -p Temperature,Pressure -T 0,6,3 \
       <full_meps_northeurope_surface.sqd> 2026-04-05T18:00:00Z_meps_northeurope_surface.sqd
```

- 2.99 GB → ~277 KB
- Central North-Europe box, decimated 2×, 2 params, times +0/+3/+6h
- Result: LCC (lat₀=63.3°, lon₀=15°), 2 params, 3 timesteps
