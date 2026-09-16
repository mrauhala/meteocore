# Nowcast estimator verification — 2026-09-16 (#640)

`skill_spike` now compares the historical single-pair baseline with the motion
pipeline used by `NowcastEngine::generate`. The live engine's estimator policy
is extracted, not retuned. Both consumers call `motion_pipeline` for the
physical search radius (40 m/s), deliberate coarsening, multi-pair estimation,
last-interval normalization and cadence-rescaled previous-generation EMA.
The same working-grid budget helper is used by both consumers.

## Running the comparison

```sh
cargo run --offline --release -p engine-nowcast --example skill_spike -- \
  --dir /path/to/fixture --template '%Y%m%d%H%M%S_smhi_live.tif' \
  --nodata 255 --scale 0.4 --offset -30 --max-lead 3
```

Use decoding overrides appropriate to the source: these local SMHI byte
fixtures were evaluated with `dBZ = raw * 0.4 - 30`, byte 255 missing. The
unconverted byte values must not be scored as dBZ. The fixture TIFFs have no
embedded gain/offset. These are local preprocessed inputs, not an independently
validated ingest dataset; no claim about radar calibration follows from this
comparison.

The harness prints decode settings, grid size, object counts and per-anchor
history/search/coarsening/EMA status. Its two arms see exactly the same frames,
working grid, thresholds, object matching and forecast timestamps. Only the
production arm retains previous motion fields. Both start at the second frame
with one pair; later anchors use at most `--history-frames` (default 3, cap 8).
No future frame is used in motion estimation. Object classes are propagated
through observed matches only for scoring.

`--block` and `--search` affect only the single-pair baseline. Production reads
its estimator policy from the shared helper. `--max-pixels` defaults to the
actual collection-config default (4,000,000), as do history and echo threshold.
The old harness used a 6M budget and halved both axes; the current shared helper
halves the larger axis. Therefore old published scores are not automatically
comparable, even though the single-pair estimator is retained. These two
1006×671 fixtures require no budget coarsening under either policy.

Lead rows count subsequent observations. Displacement uses actual elapsed
seconds divided by the anchor's source interval, including missing frames.
On these regular-cadence fixtures +1/+2/+3 are +5/+10/+15 minutes.
`--max-lead` caps the scored leads, not the history used for motion.

The process exit code gates **production lead-1 pixel CSI versus persistence**
at `--gate-threshold` (default 20 dBZ). Object scores are reported for review;
there is no implicit object pass/fail threshold. `n/a` means insufficient
objects/denominator, not zero skill. Optional `--growth-decay` keeps the
historical experimental tracking arm independently for each estimator; this
is not a validation of production track replay/coasting, joins, or raw-byte
forecast encoding. Growth/decay remains disabled by default.

## Recorded results

Source baseline: main `126fede` after #729. Both runs use the command above,
production history 3, min echo 10 dBZ, baseline block 32/search 20,
4 substeps, object threshold 35 dBZ/min area 5 pixels/matching gate 20 km,
and growth/decay off. Fixture bytes are not committed; hashes below identify
the exact local inputs. The working grid is 1006×671, about 1.28×2.71 km at
mid-latitude. Production selects search radius 10 pixels and no extra coarsening.

### Three-frame April sequence

`testdata/smhi-radar-geotiff-4326`, template
`%Y%m%d%H%M%S_smhi_radar.tif`, 2026-04-01 07:00–07:10 UTC. One evaluable
anchor: this tests the first-generation comparison, not multi-pair or EMA.

```text
lead  thr(dBZ)   CSI baseline  CSI production  CSI persist  POD production  FAR production
  +1     10.0          0.641          0.641        0.615          0.796          0.232
  +1     20.0          0.586          0.587        0.576          0.726          0.247
  +1     35.0          0.075          0.075        0.103          0.150          0.870

lead  objCSI base/prod/pers  growPOD base/prod/pers  decayPOD base/prod/pers  cent.err km base/prod
  +1   n/a/n/a/n/a  n/a/n/a/n/a  n/a/n/a/n/a  n/a/n/a

GATE PASS: production lead-1 CSI 0.586700 > persistence 0.576458 at 20 dBZ
```

### Eight-frame July sequence

Copied the first eight TIFFs of `testdata/smhi-live-seq` into a dedicated
fixture directory, 2026-07-20 20:50–21:25 UTC. Six evaluable anchors, with
6/5/4 forecasts at +5/+10/+15 minutes. The last five anchors use two history
pairs and the previous production field. Observed object counts:
6, 9, 6, 7, 6, 4, 6, 4.

```text
lead  thr(dBZ)   CSI baseline  CSI production  CSI persist  POD production  FAR production
  +1     10.0          0.781          0.781        0.728          0.885          0.131
  +1     20.0          0.699          0.699        0.639          0.828          0.183
  +1     35.0          0.265          0.264        0.201          0.426          0.589
  +2     10.0          0.720          0.721        0.630          0.851          0.175
  +2     20.0          0.635          0.635        0.539          0.787          0.233
  +2     35.0          0.193          0.191        0.128          0.333          0.690
  +3     10.0          0.671          0.675        0.571          0.825          0.211
  +3     20.0          0.586          0.587        0.482          0.755          0.274
  +3     35.0          0.177          0.183        0.089          0.326          0.706

lead  objCSI base/prod/pers  growPOD base/prod/pers  decayPOD base/prod/pers  cent.err km base/prod
  +1   0.543/0.543/0.543  1.000/1.000/1.000  1.000/1.000/1.000  2.681/2.719
  +2   0.500/0.513/0.513  1.000/1.000/1.000  1.000/1.000/1.000  2.794/2.850
  +3   0.455/0.455/0.455  1.000/1.000/1.000  1.000/1.000/1.000  2.278/2.325

GATE PASS: production lead-1 CSI 0.698519 > persistence 0.638605 at 20 dBZ
```

Production beats persistence at the 20 dBZ lead-1 gate on both sequences.
This is not evidence that it beats the single-pair estimator everywhere:
July 35 dBZ CSI is slightly lower at +5/+10 min and higher at +15 min;
April 35 dBZ CSI loses to persistence. July object CSI ties at two leads and
improves at one; centroid error is slightly worse. Few objects and a 20 km
matching gate make class POD=1 uninformative as a general quality claim.
A broader convective dataset is still required before estimator/model changes.

## Automated regression coverage

- Irregular pair intervals preserve velocity in the last interval's units.
- Coarsening returns vectors and block sizes in working-grid units.
- Both measured and filled EMA weights rescale prior cadence; incompatible
  block geometry skips blending.
- Elongated working grids remain within the pixel budget.
- Three successive live generations, including a skipped frame, expose the
  same EDR motion field as the harness helper, at the API's served precision.

## Fixture SHA-256

```text
ee8a7268d5b8e04a5c633ee9981f54cb276ba5865f898c3331c6397a77498a77  20260401070000_smhi_radar.tif
ae40b7f167ecd6a431feaaf2d64a922d741ad651609808da693ccd4e2841c5e5  20260401070500_smhi_radar.tif
efa86a7b10d73ca7b3b7d30240c3978c10131afa0f5206f23e754eabbd5daf7f  20260401071000_smhi_radar.tif
3ce941ee01f36a42edc10f7ed4ea1d44a00a9619c0d326ddd12519e75aa9dd6b  20260720205000_smhi_live.tif
0f38cb39b148b8dec35303d1380e9d7931e41f766e32ac81f7c4d8dc83fd33ee  20260720205500_smhi_live.tif
1103c5b5f844f4a638c34c65ca7fa0aa17a5b72cabef57c4608962532b26e1e9  20260720210000_smhi_live.tif
ef06fbf69ccc57477a711e4573169beab10acc6a1166b2f64675a9526b0a147e  20260720210500_smhi_live.tif
882691b3573f3d701f3da185f1bd4dc460f100ec932af79afec1e5a49ed7d96c  20260720211000_smhi_live.tif
1a3e869f822bc485d9e6473006fc9f2c5b70a4ac830fd5a21b864679bab5ec65  20260720211500_smhi_live.tif
cf555d36e8659ab8dbe49bec05257a7332b36c70b940cb32b63bf5c64409fe13  20260720212000_smhi_live.tif
84459f95bcf110b066e9108a453321cc4ff305cd1e11001d86a58ecbe0d635d7  20260720212500_smhi_live.tif
```
