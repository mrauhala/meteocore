# GMGSI fixture

A decimated copy of one real NOAA GMGSI longwave-IR mosaic, 2026-09-28
12:00 UTC (`s3://noaa-gmgsi-pds/GMGSI_LW/2026/09/28/12/`, NOAA Open Data
Dissemination):

- `GLOBCOMPLIR_v3r0_blend_s202609281200000_…nc`: every 50th row
  (0–2950) and every 49th column (0–4949) of the 3000 × 4999 source grid,
  i.e. 60 × 102 pixels.
- `….cdl`: the source the `.nc` is built from, `ncgen -k nc4 -o <name>.nc
  <name>.cdl`.

The decimated grid is still what the engine recognises:
- a global spherical-Mercator grid, from the 2-D `lat`/`lon` arrays;
- its 102 columns of 49 source pixels (3.53°) fall 1.378 source pixels
  (0.028 of a fixture pixel) short of 360°, so it has a seam sliver just
  west of 180°, like the source's 0.38 px;
- longitude starts at 179.99962° and latitude runs 72.72°N → 71.66°S.

What is kept from the source:
- `data`, `dqf`, `lat` and `lon` are the source values at those pixels,
  unchanged. `data` is float counts 13–255.
- The header is the source's (`ncdump -hs` over an HTTP range read),
  including `units = "K"` on what are display counts.
- The chunking is scaled down. `data` is chunked 1 × 16 × 27, so the grid
  is 4 × 4 blocks with clipped edges, like the source's 1 × 793 × 1322.
  `lat`/`lon` are one chunk each, as in the source.
- The `geospatial_*` attributes still describe the source.
- The `meteocore_fixture` global attribute records the decimation.

The CDL was written from the source arrays, read in file row order. Each
float is printed as the shortest text that reads back as the same float32.
