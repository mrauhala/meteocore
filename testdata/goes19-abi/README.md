# GOES-19 ABI fixtures

Cropped from real NOAA GOES-19 full-disk scans of 2026-09-25 19:00 UTC
(`s3://noaa-goes19`, NOAA Open Data Dissemination), keeping the original
packed integers, attributes, chunking and compression:

- `OR_ABI-L2-CMIPF-M6C13_G19_s20262681900199_…nc` — band 13 (10.3 µm)
  brightness temperature, rows 720–960 × cols 4480–4800 of the 5424² grid,
  across the north-east limb (~41 % space).
- `OR_ABI-L2-ACHTF-M6_G19_s20262681900199_…nc` — L2 cloud top temperature,
  rows 2256–2496 × cols 3100–3420, disk interior (~50 % clear sky).

The x/y `add_offset` are shifted so packed index 0 is the first cropped
pixel; each file's `meteocore_fixture` global attribute records its crop.
Built by reading the window from the source file and writing CDL through
`ncgen -k nc4`.
