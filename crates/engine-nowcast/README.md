# engine-nowcast

Radar nowcasting: moves another collection's latest radar frame forward along
its measured motion, tracks storm cells, and serves the motion field itself.

## What it solves

A radar composite shows where precipitation is now. For the next hour or so
most echo keeps moving with the flow, so a useful short-range forecast is to
measure that motion and carry the latest frame along it. This crate turns an
existing raster collection into a derived collection with `TIME` values in
the future. It also serves two by-products: tracked storm cells ranked by
significance, and the motion vectors for particle-animation clients.

## How it works

- **Derived collection.** The engine wraps the `MapEngine` of a `source`
  collection; the server builds it after all base engines. A poll loop on
  the background runtime checks the source every `poll_interval_secs`. Each
  new latest source frame (the *anchor*) starts one *generation*.
- **Working grid.** A generation reads the last `history_frames` source
  frames through the source's `get_raster_tile`, on a regular WGS84 grid
  over the source extent: the source's cell count, halved along the longer
  axis until it fits `max_pixels`. All frames come from one source run.
- **Motion.** Block matching on every consecutive frame pair, with a
  sub-pixel fit. Only blocks with echo at or above `min_echo` yield vectors;
  outliers are dropped and empty blocks filled from neighbours. Frames are
  coarsened so the search window for 40 m/s motion stays small, and the
  field is blended with the previous generation's to damp noise.
- **Extrapolation.** Semi-Lagrangian backward advection: each output pixel
  traces back along the field and takes one nearest-neighbour sample of the
  anchor frame, so raw values and discrete palettes survive. Pixels traced
  from outside the domain become nodata. A byte source with a nodata byte
  stays 1 byte per pixel; other sources are held as `f32`. Each generation
  is an immutable model run whose reference time is its anchor.
- **Cells.** The anchor frame is segmented at 35 dBZ (at least 2.5 km²).
  Cells are matched across generations on predicted position plus size and
  intensity, described, and ranked with `ds_core::significance`. At start,
  up to 8 earlier source frames are replayed to rebuild tracks.

## What it serves

| Trait | APIs | Content |
|---|---|---|
| `MapEngine` | WMS, Maps, Tiles | Extrapolated frames of the source's default parameter |
| `FeatureEngine` | Features, EDR `items` | Tracked storm cells as Point features |
| `EdrEngine` | EDR | Motion field: `area` and `radius` queries, instances |

**Map layer.** `time` lists the anchor and one frame per `step` until
`horizon` is covered. Without `TIME` a request draws the furthest lead, so
clients should send `TIME`. WMS lists retained generations as the
`reference_time` dimension (`DIM_REFERENCE_TIME`); Maps and Tiles draw the
latest.

**Storm cells.** One Point feature per cell; the id is the track id. The
cells of the last 48 generations are kept. Without `datetime` the latest
are served; with it, the newest generation anchored inside the interval,
else none. A by-id request serves the latest generation. The
[api-mcp](../api-mcp/README.md) tools read these cells.

- Properties: `severity` (`weak` … `very_severe`: one step each for max
  dBZ ≥ 45, 50, 55 and area ≥ 50 km²), `max_dbz`, `area_km2`, `observed`,
  `track_age`, `age_minutes`, `speed_ms`, `bearing_deg`,
  `net_displacement_km`, `path_straightness`, `deviant_mover`,
  `likely_clutter`, `volume_trend`, `intensity_trend_dbz_min`,
  `significance` (0–1), `significance_rank`, `significance_contributions`
  (signed `{term, value}` list) and `significance_reasons` (top three terms).
- Each optional group exists only when its source is configured: lightning
  (`flash_count`, `flash_rate_per_min`, `flash_density_per_km2`,
  `first_flash`, `cg_count`, `ic_count`, `cg_polarity_known`,
  `positive_cg_fraction`, `jump_sigma`, `lightning_jump`,
  `lightning_coverage`), impact (`impact_over`, `impact_approaching`,
  `impact_eta_minutes`, `impact_exposure`) and radar (`nearest_radar_id`,
  `nearest_radar_name`, `nearest_radar_distance_km`, `in_radar_coverage`,
  `beam_height_m`, `beam_elevation_deg`). When a join is skipped for a
  generation, its group is present with `null` values (`lightning_coverage`
  keeps its value). An unmeasured value is `null`, never `0`.
- Every property except `significance_contributions` is filterable.
  `sortby` takes `significance`, `significance_rank`, `max_dbz`, `area_km2`,
  `track_age`, `speed_ms`, `bearing_deg`, `intensity_trend_dbz_min`,
  `net_displacement_km`, `path_straightness`, `likely_clutter`, and with
  their sources: lightning properties except `lightning_jump` and
  `lightning_coverage`, `impact_eta_minutes`, `nearest_radar_distance_km`,
  `beam_height_m`.
- `likely_clutter`: seen for at least 6 frames, slower than 3 m/s and
  within 3 km of its start. Such a cell is demoted, not removed.

**Motion field (EDR).** Parameters `motion_u` and `motion_v` (eastward and
northward precipitation motion, m/s; echo motion, not wind) and
`motion_quality` (1 block-matched, 0 filled). `area` returns a CoverageJSON
`Grid` of block centres over the polygon's bounding box plus one block each
side, `y` ascending south to north; blocks outside the area are `null`.
`radius` runs as a circle polygon. Instances are the retained generations,
each valid only at its anchor. Without an instance, `datetime` picks the
newest generation anchored inside it. Each cell in EDR `items` names a
radius query centred on it. Other query types answer 404. See the
[EDR matrix](../api-edr/README.md#per-engine-query-type-matrix).

## Configuration

Inline in `config.toml` (per-collection files use `[nowcast]`; runnable
example: [`radar-fi-nowcast.toml`](../../collections.d/radar-fi-nowcast.toml)):

```toml
[[collections]]
id = "radar-nowcast"
title = "Radar nowcast"
description = "Motion-extrapolated radar reflectivity"
engine_type = "nowcast"
apis = ["wms", "maps", "tiles", "features", "edr"]

[collections.nowcast]
source = "radar"
max_generations = 2
# impact_source = "municipalities"

[collections.wms]
colormap = "radar_dbz"
```

| Field | Default | Meaning |
|---|---|---|
| `source` | required | Raster collection in the same config, not a nowcast, with `wms`, `maps` or `tiles` in its `apis` |
| `horizon` | `"PT2H"` | How far ahead, ISO 8601 duration |
| `step` | source cadence | Lead spacing. Over 96 leads is a load error; with the default, leads past 96 are dropped with a warning |
| `history_frames` | `3` | Source frames per generation for motion, 2–8 |
| `poll_interval_secs` | `30` | Source check interval, at least 5 |
| `max_generations` | `6` | Generations retained, at least 1 |
| `max_pixels` | `4000000` | Working-grid budget, clamped to 65 536–16 000 000 |
| `min_echo` | `10.0` | Echo threshold for motion and skill, in source units |
| `growth_decay` | `false` | Experimental: adds each cell's intensity trend along the leads (a second sampling pass per lead) |
| `lightning_source` | unset | Events-shape `postgis` collection |
| `impact_source` | unset | Polygon collection with `features` in its `apis`, not a nowcast |
| `impact_name_property` | `"name"` | Area display-name property; areas without it are ignored |
| `impact_weight_property` | unset | Numeric property weighting exposure on a log scale |
| `radar_source` | unset | `odim-volume` collection |

A named source that is missing or of the wrong kind fails the collection.

- **Lightning.** Strikes since the previous generation join the cell under
  them, else the nearest cell within 10 km. `lightning_jump` is true when
  the flash rate is at least 10/min and more than 2σ (`jump_sigma`) above
  the track's last few rates. Flash numbers need the cell, plus that 10 km
  radius, inside the source's `observations.coverage_bbox`
  ([engine-postgis](../engine-postgis/README.md)); elsewhere they are `null`.
- **Impact.** One fetch per generation, at most 5 000 areas. `impact_over`
  is the area under the cell; `impact_approaching` and `impact_eta_minutes`
  the next area along its motion within 60 minutes.
- **Radar.** Read from the volume collection's site catalog, without
  volume reads. `beam_height_m` is the lowest sweep's centre above mean sea
  level (4/3-Earth model), set only within that sweep's range.
- **Significance.** `[collections.nowcast.significance]` overrides weights
  by term name. The defaults (`ds_core::cell_facts::DEFAULT_CELL_WEIGHTS`)
  are a weighted mean of `severity` 1.0, `max_dbz` 0.6, `area` 0.3,
  `impact` 1.5, `flash_rate` 0.5, `positive_cg` 0.6, `vil` 0.7, `echo_top`
  0.5 and `beam_coverage` 0.4 over the terms a cell has data for; bonuses
  `intensifying` 0.5, `deviant_mover` 0.4 and `lightning_jump` 0.9; and
  discounts `weakening` −0.15 and `clutter` −0.9 (the fraction removed). An
  unknown name, or `severity`, `max_dbz` and `area` all 0, fails the load.

## Operations

- `/health` reports the collection degraded until the first generation.
- `/metrics` exports per-collection `nowcast_*` series: generation counts,
  duration and source lag, tracker counters, and the previous generation's
  lead-1 CSI next to persistence (`nowcast_lead1_csi_permille`).
- A reload with unchanged config and source engine keeps generations, cell
  history and track ids.

## Limits

- Advection only: echo does not grow, decay or form along the leads
  (`growth_decay` is an experimental exception).
- Extrapolates the source's default parameter at its default level only.
- Frame memory ≈ working pixels × (leads + 1) × `max_generations` × 1 byte
  for a byte source with a nodata byte, else 4 bytes: up to 600 MB per byte
  source at the defaults and a 5-minute cadence (24 leads).
- Cell rules (35 dBZ, severity steps) assume a dBZ source. Cells describe
  the anchor frame only; there are no forecast cell positions.
- EDR serves the motion field, not the extrapolated values. WMS
  GetFeatureInfo does not answer for these layers, and cells are not served
  as vector tiles.
- No volume join exists, so the `vil`, `echo_top` and `beam_coverage`
  weights have no effect.
- A lightning window of 200 000 strikes or more skips that generation's
  join.

## Testing

```sh
cargo test -p engine-nowcast
```

- `tests/integration.rs`: a mock source draws a disc moving at constant
  speed for any time, so extrapolated frames are checked against the true
  future. Also covers instances, cell tracking, ranking and the joins.
- `tests/clutter_regression.rs`: two captured cell responses in
  `tests/fixtures/`, one frame of pure clutter and one of widespread rain.
- `examples/skill_spike.rs`: hindcast over a directory of GeoTIFF frames
  (not in the repository); fails unless lead-1 CSI beats persistence. See
  [nowcast-estimator-parity](../../docs/nowcast-estimator-parity.md).

Contributor notes: [CLAUDE.md](CLAUDE.md).
