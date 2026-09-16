# ODIM voxel quantitative baseline (#641)

The production sampler selects the nearest elevation and a single native
ray/bin at each cylindrical voxel centre. It does not average or interpolate.
This baseline measures the resulting peak loss, echo-top differences, and
vertical integration sensitivity before choosing a replacement. It does not
certify VIL, change serving behaviour, or enable the nowcast `VolumeFacts` join
(#642).

## Reproduce

```sh
cargo run --offline --release -p engine-odim --example voxel_validation -- \
  testdata/radar-fmi-pvol/202605191050_fivih_PVOL.h5 DBZH 1
# Optional fourth argument: semicolon-separated radius,azimuth,height counts.
cargo run --offline --release -p engine-odim --example voxel_validation -- \
  testdata/radar-fmi-pvol/202605191050_fivih_PVOL.h5 DBZH 0.5 '128,360,48'
```

The tool reads every gate of the exact requested quantity, with no TH fallback,
then runs `VolumeEngine::read_voxel_grid` on an isolated copy of the same file
at its exact timestamp. It fails on absent/undecodable quantities or malformed
comparison inputs. Limits are 256 MiB input, 32M native samples, and the engine's
32M voxels. Missing local fixtures are errors in this tool, not skipped runs.

Default grids: coarse `48×180×24`, production default `128×360×48`, and finer
`256×360×96`. All retain the engine's radius extent and 0–20 km above-antenna
height domain. Height steps are 833.33, 416.67, and 208.33 m respectively.
The angular/radial centres change too, so these grids are not nested samples.

## What the columns mean

- **Native peaks/tops:** all finite echo gates whose beam centres lie in the
  grid domain, split by ground range (0–50, 50–100, 100–150, 150+ km). A native
  peak is an observed value, not necessarily meteorological truth; these runs
  apply no additional clutter or quality filtering.
- **First-tilt peak:** retain only the first requested-quantity sweep at each
  repeated elevation. All repeated sweeps remain in the all-native statistics.
  This separates duplicate-sweep selection from grid loss. The native and
  first-tilt band peaks coincide in all three fixtures below.
- **Echo tops:** highest gate/voxel *centre* above the antenna at 18, 45, and
  50 dBZ. `n/a` means no qualifying sample, not a zero-height top. These are
  band-wide threshold diagnostics, not matched storm-cell attributes. The
  cell product's top-face convention is different.
- **Beam support model:** each tilt has a declared full angular width (default
  1°; an assumption, not retrieved beam metadata). At each output column it
  holds the native ray/bin value constant across that beam. Overlaps are split
  at adjacent-tilt midpoints; gaps remain unobserved. Repeated tilts contribute
  depth once, using the first requested-quantity sweep. This agrees with the
  sampler's tie policy when its first tied sweep has the requested quantity;
  a quantity absent from that sweep remains a separate sampling discrepancy.
  Boundaries use the shared core 4/3-Earth `beam_height_at_ground` helper and
  its ground/cos(elevation) slant approximation. This is an explicit reference
  policy, not an independent physical calibration of beam geometry.
- **Finite without beam support:** finite voxel centres outside the model's
  valid native segments, including nodata/quantity gaps. Undetect gates provide
  valid support at zero echo; masked gates provide no measured depth. The
  reported fraction is voxel-count-weighted, includes clear-air voxels, and is
  neither physical volume nor an operational radar-quality percentage.
- **Vertical integrals:** integrate `3.44e-6 × Z^(4/7)` over depth with a 56 dBZ
  cap. The no-echo floor contributes zero. The reference integrates the beam
  segments exactly; the voxel version sums value × voxel height. The >=35 dBZ
  version exposes the storm-cell member cutoff without doing segmentation.
  These are **VIL-like proxies**, not measured VIL. A gap contributes no
  *measured* depth, which must not be interpreted as observed zero water.
  Each band reports separate maxima and the largest absolute **paired-column**
  difference. The two maxima need not belong to the same column. Horizontal
  sampling can still miss a native core in both integrals.

## Measured fixtures (2026-09-16)

Inputs are local, uncommitted ODIM files. All runs request DBZH. Full per-band
results, including the 0.5° sensitivity run, are in
[odim-voxel-validation-results.csv](odim-voxel-validation-results.csv).

| Radar / UTC timestamp | File under `testdata/` | SHA-256 |
|---|---|---|
| FMI Vihti / 2026-05-19 10:50 | `radar-fmi-pvol/202605191050_fivih_PVOL.h5` | `609071f28d76e86903573c2c5123f91821874408c307eabc23b5df6099698371` |
| DMI Stevns / 2026-06-10 15:00 | `radar-dmi-pvol/dkste_202606101500.vol.h5` | `59a15a83812e1e0f1f28b93a7e5a2883663df2eb5c57f7758384d9bec5facc4a` |
| SMHI Hemse / 2026-06-11 10:05 | `radar-smhi-pvol/radar_hemse_qcvol_202606111005.h5` | `e26a7aea87b7b5357e64bafca6b369688de179c5a9dc83d48f21cbc07bd5d56f` |

Whole-domain peak reflectivity (dBZ):

| Fixture | All native | Coarse | Default | Finer |
|---|---:|---:|---:|---:|
| Vihti | 59.39 | 43.26 | 45.86 | 45.86 |
| Stevns | 68.00 | 55.50 | 64.00 | 57.50 |
| Hemse | 44.49 | 40.64 | 44.49 | 44.32 |

Resolution is not a monotonic peak-preservation guarantee. Vihti's strongest
native echo lies below 344 m above the antenna in the 0–50 km band; all three
grids miss the >=50 dBZ echo. Repeated-tilt peak selection does not explain that
band's loss. This supports the narrow peak-loss finding, not a claim that all
storms lose the same number of dBZ or that MAX/interpolation would be preferable.

Vihti, **default grid**, assumed full beam width 1°:

| Range km | Native / voxel peak dBZ | Native / voxel top18 m | Finite centres outside support | Max reference / voxel integral kg/m² | Max paired absolute difference kg/m² |
|---|---|---|---:|---|---:|
| 0–50 | 59.39 / 35.90 | 4944.6 / 6041.7 | 34.40% | 0.185 / 0.192 | 0.076 |
| 50–100 | 40.63 / 40.57 | 7291.7 / 6458.3 | 34.64% | 1.353 / 1.385 | 0.169 |
| 100–150 | 42.92 / 41.33 | 6548.3 / 7291.7 | 28.78% | 1.883 / 2.583 | 0.700 |
| 150+ | 46.60 / 45.86 | 9926.9 / 10208.3 | 18.60% | 3.887 / 5.476 | 2.288 |

Changing only the assumed beam width to 0.5° changes the last band's maximum
reference integral to 2.436 kg/m² and the largest paired difference to 3.625
kg/m²; the voxel integral stays 5.476. This is why these comparisons cannot
establish a general VIL inflation factor. With the 1° model, whole-domain
unsupported-centre fractions at the default grid are 26.06% (Vihti), 45.59%
(Stevns), and 52.02% (Hemse). Their near constancy across grid sizes indicates
that grid refinement alone does not resolve this model-support discrepancy.

## Regression coverage and next decisions

Committed synthetic tests run without HDF5 fixtures. They check analytic
constant-beam integration, undetect versus nodata, gaps and overlap ownership,
repeated tilts, malformed input, and narrow-peak loss/recovery through the actual
production sampler. Run `cargo test -p engine-odim --lib voxel_diagnostics`.
These tests pin the diagnostic and known sampler behaviour; they are not
acceptance thresholds for a new sampler.

A replacement needs separate acceptance criteria for peak preservation, echo
location/extent, threshold top accuracy, and integrated water. Evaluate the
same range bands with representative quality-controlled storms and known beam
metadata, and compare spatially matched cells before tuning. MAX can enlarge
cores; interpolation can reduce extrema. Neither is selected by these results.
Keep the nowcast volume join gated until its quantitative features have that
validation. The baseline completes #641's triaged measurement scope; choosing
and validating a resampling change remains subsequent work.
