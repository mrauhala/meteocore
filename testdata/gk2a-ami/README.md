# GK2A AMI fixture

Cropped from a real KMA GK2A (GEO-KOMPSAT-2A, 128.2°E) AMI L1B full disk,
IR105 channel, 2026-09-28 12:00 UTC:
`s3://noaa-gk2a-pds/AMI/L1B/FD/202609/28/12/gk2a_ami_le1b_ir105_fd020ge_202609281200.nc`
(NOAA Open Data Dissemination, KMA data).

- `gk2a_ami_le1b_ir105_fd020ge_202609281200.nc` keeps the original
  `image_pixel_values` count words of full-disk lines 1946–2041 and columns
  4914–5073, numbered from 1 as CGMS does. That is 96 × 160 pixels
  straddling 180° at about 14–16°N, all good quality, counts 3147–4746.
- Every global attribute is the original, including the navigation
  (`cfac`, `lfac`, `sub_longitude`, …) and calibration
  (`DN_to_Radiance_*`, `Teff_to_Tbb_*`, constants). These are rewritten
  for the window: `number_of_columns` 160, `number_of_lines` 96,
  `coff` −2162.5, `loff` 805.5, and the `image_upperleft_*` and
  `image_lowerright_*` scan angles of its first and last pixel. The pixel
  statistics and `image_center_*` still describe the full disk.
- The other variables (orbit, navigation residuals, GSICS) are dropped.
- The field is chunked 32 × 48 so that blocks clip at the right edge.
  The original chunks are 1375 × 1375.
- The `meteocore_fixture` global attribute records the crop.

The window was read from the full file with an `h5dump` hyperslab
(start 1945, 4913, count 96, 160, 0-based) and written as CDL,
`gk2a_ami_le1b_ir105_fd020ge_202609281200.cdl`, through `ncgen -k nc4`.
