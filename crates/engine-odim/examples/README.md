# engine-odim examples — 3D Tiles radar-volume demo

Proof-of-concept tooling for **OGC 3D Tiles** delivery of radar polar volumes
(epic [#346], Phase 1 = [#347]). These are standalone `cargo run` examples — they
do **not** change any engine trait; they validate the geometry pipeline
end-to-end before the `VolumeEngine` trait + `ds-3dtiles` encoder are built
([#348]).

## `gen_3dtiles` — polar volume → 3D Tiles point cloud

Reads an ODIM polar volume, projects **every** echo cell through the engine's
4/3-Earth beam model → geodetic → ECEF, and writes a 3D Tiles point cloud plus a
token-free CesiumJS viewer.

```bash
# Uses the default FMI Vihti fixture (see "Fixture" below):
cargo run -p engine-odim --example gen_3dtiles

# Or point it at any ODIM polar volume, with an output dir + min dBZ threshold:
cargo run -p engine-odim --example gen_3dtiles -- path/to/volume.h5 target/3dtiles 5.0
```

Output (default `target/3dtiles-fivih/`):

| File | What |
|------|------|
| `content.pnts` | the point cloud (3D Tiles 1.0 Point Cloud format) |
| `tileset.json` | the tileset (single tile, `region` bounding volume) |
| `index.html`   | a self-contained CesiumJS viewer (CARTO Dark Matter basemap) |

### Viewing

CesiumJS needs the files served over HTTP (not `file://`):

```bash
cd target/3dtiles-fivih && python3 -m http.server 8777
# then open http://localhost:8777/index.html
```

The colours are an NWS-style reflectivity ramp; the dark basemap makes them pop.
You'll see the conical sweep structure, the echo tops, and the "cone of silence"
directly above the antenna.

## `volume_stats` — reflectivity probe

Prints per-sweep + whole-volume reflectivity stats (coverage, max dBZ, convective
fraction, approximate echo top). Used to pick a demo timestep with interesting
weather.

```bash
cargo run -p engine-odim --example volume_stats -- path/to/volume.h5 [QUANTITY]
```

## Why `.pnts` and not `.glb` (and other gotchas)

Hard-won while building the PoC — these inform how `ds-3dtiles` should encode:

- **Use `.pnts`, not a `.glb` with POINTS-mode.** A glb point primitive routes
  through CesiumJS's `Model` renderer, which draws fixed **1px** points and
  **ignores both `pointSize` styling and vertex `COLOR_0`** (renders faint
  white). The `.pnts` FeatureTable (POSITION + RGB) honours `pointSize` and
  per-point colour.
- **The tileset's top-level `geometricError` must be `> 0`.** With it `0`,
  CesiumJS never refines to the root and **never even requests the content
  tile** — nothing renders, no error. (This example uses tileset `100000`,
  root `1000`.)
- **`.pnts` `POSITION` is ECEF-native** (offsets from `RTC_CENTER`) — no glTF
  Y-up→Z-up flip. A glb path *would* need to bake `(x, z, -y)`.
- **`region` bounding volume** is geodetic (EPSG:4979) and ignores the tile
  transform, sidestepping local-frame confusion.
- The Cesium Ion default imagery needs a token → black globe; the viewer uses
  CARTO Dark Matter via `UrlTemplateImageryProvider` (token-free).

## Fixture

The default input — `testdata/radar-fmi-pvol/202605191050_fivih_PVOL.h5` (FMI
Vihti, 2026-05-19 10:50Z) — is the same volume the integration tests use and is
**not committed to git** (15 MB). Provide your own ODIM polar volume, or drop one
under `testdata/radar-fmi-pvol/`. It's the best fivih volume we measured for a
demo: 59 dBZ peak, 35.7% coverage, discrete convective cores.

[#346]: https://github.com/mrauhala/meteocore/issues/346
[#347]: https://github.com/mrauhala/meteocore/issues/347
[#348]: https://github.com/mrauhala/meteocore/issues/348
