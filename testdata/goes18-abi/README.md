# GOES-18 ABI fixture

Cropped from a real NOAA GOES-18 (GOES-West, 137.0°W) full-disk scan of
2026-09-27 18:50 UTC (`s3://noaa-goes18`, NOAA Open Data Dissemination),
keeping the original packed integers, attributes, chunking and compression:

- `OR_ABI-L2-CMIPF-M6C13_G18_s20262701850224_…nc` — band 13 (10.3 µm)
  brightness temperature, rows 1880–2120 × cols 560–880 of the 5424² grid,
  straddling the antimeridian at about 11–16°N (173.9°E → 174.8°W).

The x/y `add_offset` are shifted so packed index 0 is the first cropped
pixel; the `meteocore_fixture` global attribute records the crop. Built the
same way as `../goes19-abi`: the window is read from the source file with
HTTP range requests and written as CDL through `ncgen -k nc4`.
