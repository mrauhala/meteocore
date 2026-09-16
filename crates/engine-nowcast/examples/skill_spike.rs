//! Phase-0 hindcast harness for the nowcasting epic (#519 / #520).
//!
//! Loads a directory of composite GeoTIFF frames through the public
//! `GeoTiffEngine` (the same decode path phase 1 will consume via
//! `Arc<dyn MapEngine>`), compares the historical single-pair estimator with
//! the shared production multi-pair/coarsening/EMA pipeline, and scores both
//! against the same observations and persistence baseline.
//!
//! Gate: production-estimator CSI must beat persistence at the gate threshold,
//! lead 1. Object metrics are reported alongside it; they have no binary gate.
//!
//! ```text
//! cargo run --release -p engine-nowcast --example skill_spike -- \
//!     --dir testdata/smhi-radar-geotiff-4326 \
//!     --template "%Y%m%d%H%M%S_smhi_radar.tif" \
//!     --nodata 255 --scale 0.4 --offset -30
//! ```

use std::process::ExitCode;
use std::time::Instant;

use ds_core::config::{GeoTiffConfig, NowcastConfig};
use ds_core::map_engine::{MapEngine, OutputCrs, RasterValues};
use engine_geotiff::GeoTiffEngine;
use engine_nowcast::advect::advect_u8;
use engine_nowcast::cells2d::{advance_tracks, CellTrack};
use engine_nowcast::motion::{estimate_motion, MotionOptions};
use engine_nowcast::motion_pipeline::{
    estimate_production_motion, working_grid_size, MotionEstimate, MAX_HISTORY_FRAMES,
};
use engine_nowcast::objects::{
    classify_growth, match_cells, score_objects, segment_cells, segment_cells_labeled, CellBlob,
    GrowthClass, ObjectScores, PixelScale,
};
use engine_nowcast::skill::{score, Contingency};
use engine_nowcast::tendency::EFOLD_INTERVALS;
use engine_nowcast::{advect::advect, Grid};

struct Args {
    dir: String,
    template: String,
    thresholds: Vec<f32>,
    gate_threshold: f32,
    min_echo: f32,
    block: usize,
    search: i32,
    substeps: usize,
    nodata: Option<f64>,
    scale: Option<f64>,
    offset: Option<f64>,
    object_threshold: f32,
    min_area: usize,
    gate_km: f64,
    growth_decay: bool,
    history_frames: usize,
    max_pixels: usize,
    max_lead: Option<usize>,
}

fn parse_args() -> Result<Args, String> {
    // Read actual config defaults so harness defaults cannot drift from serving.
    let defaults: NowcastConfig = serde_json::from_value(serde_json::json!({"source": "fixture"}))
        .expect("nowcast config defaults");
    let mut args = Args {
        dir: String::new(),
        template: String::new(),
        thresholds: vec![10.0, 20.0, 35.0],
        gate_threshold: 20.0,
        min_echo: defaults.min_echo as f32,
        block: 32,
        search: 20,
        substeps: 4,
        nodata: None,
        scale: None,
        offset: None,
        object_threshold: 35.0,
        min_area: 5,
        gate_km: 20.0,
        growth_decay: false,
        history_frames: defaults.history_frames,
        max_pixels: defaults.max_pixels,
        max_lead: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("missing value for {name}"));
        match flag.as_str() {
            "--dir" => args.dir = value("--dir")?,
            "--template" => args.template = value("--template")?,
            "--thresholds" => {
                args.thresholds = value("--thresholds")?
                    .split(',')
                    .map(|s| s.trim().parse::<f32>().map_err(|e| e.to_string()))
                    .collect::<Result<_, _>>()?
            }
            "--gate-threshold" => {
                args.gate_threshold = value("--gate-threshold")?
                    .parse()
                    .map_err(|e: std::num::ParseFloatError| e.to_string())?
            }
            "--min-echo" => {
                args.min_echo = value("--min-echo")?
                    .parse()
                    .map_err(|e: std::num::ParseFloatError| e.to_string())?
            }
            "--block" => {
                args.block = value("--block")?
                    .parse()
                    .map_err(|e: std::num::ParseIntError| e.to_string())?
            }
            "--search" => {
                args.search = value("--search")?
                    .parse()
                    .map_err(|e: std::num::ParseIntError| e.to_string())?
            }
            "--substeps" => {
                args.substeps = value("--substeps")?
                    .parse()
                    .map_err(|e: std::num::ParseIntError| e.to_string())?
            }
            "--nodata" => {
                args.nodata = Some(
                    value("--nodata")?
                        .parse()
                        .map_err(|e: std::num::ParseFloatError| e.to_string())?,
                )
            }
            "--scale" => {
                args.scale = Some(
                    value("--scale")?
                        .parse()
                        .map_err(|e: std::num::ParseFloatError| e.to_string())?,
                )
            }
            "--offset" => {
                args.offset = Some(
                    value("--offset")?
                        .parse()
                        .map_err(|e: std::num::ParseFloatError| e.to_string())?,
                )
            }
            "--object-threshold" => {
                args.object_threshold = value("--object-threshold")?
                    .parse()
                    .map_err(|e: std::num::ParseFloatError| e.to_string())?
            }
            "--min-area" => {
                args.min_area = value("--min-area")?
                    .parse()
                    .map_err(|e: std::num::ParseIntError| e.to_string())?
            }
            "--gate-km" => {
                args.gate_km = value("--gate-km")?
                    .parse()
                    .map_err(|e: std::num::ParseFloatError| e.to_string())?
            }
            "--growth-decay" => args.growth_decay = true,
            "--history-frames" => {
                args.history_frames = value("--history-frames")?
                    .parse::<usize>()
                    .map_err(|e| e.to_string())?
            }
            "--max-pixels" => {
                args.max_pixels = value("--max-pixels")?
                    .parse::<usize>()
                    .map_err(|e| e.to_string())?
            }
            "--max-lead" => {
                args.max_lead = Some(
                    value("--max-lead")?
                        .parse::<usize>()
                        .map_err(|e| e.to_string())?,
                )
            }
            other => return Err(format!("unknown flag {other}")),
        }
    }
    if args.dir.is_empty() || args.template.is_empty() {
        return Err(
            "usage: skill_spike --dir <fixture dir> --template <strftime filename> \
                    [--thresholds 10,20,35] [--gate-threshold 20] [--min-echo 10] \
                    [--block 32] [--search 20] [--substeps 4] \
                    [--nodata <raw>] [--scale <gain>] [--offset <off>] \
                    [--object-threshold 35] [--min-area 5] [--gate-km 20] \
                    [--history-frames 3] [--max-pixels 4000000] [--max-lead <frames>] \
                    [--growth-decay]"
                .into(),
        );
    }
    // The PASS/FAIL exit code is this harness's automated signal — a gate
    // threshold that silently fell back to another entry would be misleading.
    if !args.thresholds.contains(&args.gate_threshold) {
        return Err(format!(
            "--gate-threshold {} is not among --thresholds {:?}",
            args.gate_threshold, args.thresholds
        ));
    }
    if !(2..=MAX_HISTORY_FRAMES).contains(&args.history_frames) {
        return Err(format!(
            "--history-frames must be in 2..={MAX_HISTORY_FRAMES}"
        ));
    }
    if args.max_pixels == 0
        || args.max_lead == Some(0)
        || args.block == 0
        || args.search < 0
        || args.substeps == 0
    {
        return Err(
            "pixel budget, lead, block and substeps must be positive; search must be nonnegative"
                .into(),
        );
    }
    Ok(args)
}

fn tile_to_grid(values: RasterValues, width: usize, height: usize) -> Grid {
    let data: Vec<f32> = match values {
        RasterValues::F64(v) => v
            .into_iter()
            .map(|o| o.map(|x| x as f32).unwrap_or(f32::NAN))
            .collect(),
        RasterValues::U8 {
            data,
            nodata,
            gain,
            offset,
        } => data
            .into_iter()
            .map(|raw| {
                if nodata == Some(raw) {
                    f32::NAN
                } else {
                    (raw as f64 * gain + offset) as f32
                }
            })
            .collect(),
    };
    Grid::new(width, height, data)
}

fn fmt_ratio(r: Option<f64>) -> String {
    r.map(|v| format!("{v:.3}")).unwrap_or_else(|| "n/a".into())
}

fn fmt_gate_ratio(r: Option<f64>) -> String {
    r.map(|v| format!("{v:.6}")).unwrap_or_else(|| "n/a".into())
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    let config = GeoTiffConfig {
        filename_template: Some(args.template.clone()),
        filename_pattern: None,
        timestamp_format: None,
        parameter: "reflectivity".to_string(),
        unit: "dBZ".to_string(),
        poll_interval_secs: 3600,
        tile_cache_mb: 256,
        band: 1,
        max_files: None,
        nodata: args.nodata,
        scale: args.scale,
        offset: args.offset,
        exclude_patterns: vec![],
        endpoint: None,
        bucket: None,
        prefix_pattern: None,
        time_window: None,
        scan_days: None,
        stac_url: None,
        stac_asset_key: "data".to_string(),
        stac_asset_allowlist: None,
    };
    let engine = match GeoTiffEngine::new("skill-spike", Some(&args.dir), &config) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("failed to open {}: {e}", args.dir);
            return ExitCode::FAILURE;
        }
    };

    let info = engine.raster_info();
    if info.times.len() < 3 {
        eprintln!(
            "need at least 3 frames for a hindcast, found {} in {}",
            info.times.len(),
            args.dir
        );
        return ExitCode::FAILURE;
    }
    let Some(extent) = info.spatial_extent else {
        eprintln!("source reports no spatial extent");
        return ExitCode::FAILURE;
    };

    let [w, h] = working_grid_size(info.grid_size.unwrap_or([1024, 1024]), args.max_pixels);
    println!(
        "source: {} frames, native CRS {}, sampling {}x{} over {:?}",
        info.times.len(),
        info.native_crs,
        w,
        h,
        extent
    );

    let deltas: Vec<i64> = info
        .times
        .windows(2)
        .map(|p| (p[1] - p[0]).num_seconds())
        .collect();
    if let (Some(&min), Some(&max)) = (deltas.iter().min(), deltas.iter().max()) {
        if min < 1 {
            eprintln!("source cadence must be at least 1 second and ascending");
            return ExitCode::FAILURE;
        }
        println!("cadence: {min}–{max}s between frames; leads use actual elapsed time");
        if max as f64 > min as f64 * 1.05 {
            eprintln!(
                "note: irregular cadence; lead rows count observations, not fixed-duration steps"
            );
        }
    }

    let mut frames: Vec<Grid> = Vec::with_capacity(info.times.len());
    for t in &info.times {
        let tile = match engine.get_raster_tile(
            extent,
            w,
            h,
            Some(*t),
            &OutputCrs::Wgs84,
            None,
            None,
            None,
        ) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("failed to read frame {t}: {e}");
                return ExitCode::FAILURE;
            }
        };
        let grid = tile_to_grid(tile.values, tile.width as usize, tile.height as usize);
        let finite: Vec<f32> = grid
            .data
            .iter()
            .copied()
            .filter(|v| v.is_finite())
            .collect();
        let echo = finite.iter().filter(|&&v| v >= args.min_echo).count();
        let range = if finite.is_empty() {
            "range n/a (all nodata)".to_string()
        } else {
            let (min, max) = finite
                .iter()
                .fold((f32::MAX, f32::MIN), |(lo, hi), &v| (lo.min(v), hi.max(v)));
            format!("range [{min:.1}, {max:.1}] {}", info.unit)
        };
        println!(
            "  {t}: {} finite px, {range}, {} px >= {} (echo)",
            finite.len(),
            echo,
            args.min_echo
        );
        frames.push(grid);
    }

    println!(
        "decode overrides: nodata={:?}, scale={:?}, offset={:?}; substeps={}",
        args.nodata, args.scale, args.offset, args.substeps
    );
    let opts = MotionOptions {
        block: args.block,
        search_radius: args.search,
        min_echo: args.min_echo,
        ..MotionOptions::default()
    };

    let max_lead = args
        .max_lead
        .unwrap_or(frames.len() - 2)
        .min(frames.len() - 2);
    let mut persistence = vec![vec![Contingency::default(); args.thresholds.len()]; max_lead];

    // Object-based verification (#542, after Ritvanen et al. GMD 2025):
    // segment cells once per frame; per anchor, classify observed cells as
    // growing/decaying at forecast creation and chain that class forward
    // through observed-cell matches so each lead's scores stratify by the
    // creation-time class.
    // Per-axis km/px: on a regular lat/lon grid only the east–west axis
    // carries the cos(lat) factor — at Nordic latitudes y covers ~2–3× more
    // km per pixel than x, so distances are computed anisotropically in km.
    let (px_km_x, px_km_y) = engine_nowcast::lonlat_grid_km_per_px(extent, w, h);
    let scale = PixelScale::uniform(px_km_x as f32, px_km_y as f32);
    let gate_km = args.gate_km as f32;
    let obs_cells: Vec<Vec<CellBlob>> = frames
        .iter()
        .map(|f| segment_cells(f, args.object_threshold, args.min_area))
        .collect();
    println!(
        "objects: threshold {} {}, min area {} px, match gate {:.0} km \
         (px {:.2}x{:.2} km), cells per frame {:?}",
        args.object_threshold,
        info.unit,
        args.min_area,
        args.gate_km,
        px_km_x,
        px_km_y,
        obs_cells.iter().map(Vec::len).collect::<Vec<_>>()
    );
    let mut obj_pers = vec![[ObjectScores::default(); 3]; max_lead];

    println!("comparison: baseline single-pair block={} search={}px; production history={} physical radius + multi-pair + EMA; max_pixels={}; growth_decay={}", args.block, args.search, args.history_frames, args.max_pixels, args.growth_decay);
    let mut results = Vec::new();
    for production in [false, true] {
        println!(
            "estimator: {}",
            if production { "production" } else { "baseline" }
        );
        let mut nowcast = vec![vec![Contingency::default(); args.thresholds.len()]; max_lead];
        let mut obj_now = vec![[ObjectScores::default(); 3]; max_lead];
        let mut previous: Option<MotionEstimate> = None;
        // Separate track state for each estimator's experimental growth/decay arm.
        let mut tracks: Vec<CellTrack> = Vec::new();
        let mut next_track_id: u64 = 0;

        for i in 1..frames.len() - 1 {
            let started = Instant::now();
            let interval_secs = deltas[i - 1] as f32;
            let field = if production {
                let start = (i + 1).saturating_sub(args.history_frames);
                let refs: Vec<&Grid> = frames[start..=i].iter().collect();
                let intervals: Vec<f32> = deltas[start..i].iter().map(|dt| *dt as f32).collect();
                let estimate = estimate_production_motion(
                    &refs,
                    &intervals,
                    px_km_x * 1000.0,
                    args.min_echo,
                    previous.as_ref().map(|p| (&p.field, p.interval_secs)),
                );
                println!("  production history={} pairs={} coarsening={} search={}px interval={}s ema={}",
                refs.len(), intervals.len(), estimate.coarsening, estimate.search_radius,
                estimate.interval_secs, previous.is_some());
                previous = Some(estimate);
                &previous.as_ref().expect("estimate stored").field
            } else {
                &estimate_motion(&frames[i - 1], &frames[i], &opts)
            };
            let motion_ms = started.elapsed().as_millis();
            let measured = field.measured.iter().filter(|&&m| m).count();
            println!(
                "anchor {}: motion {}ms, {} of {} blocks measured",
                info.times[i],
                motion_ms,
                measured,
                field.measured.len()
            );

            // Growth/decay class of each observed cell at forecast creation,
            // then chained forward through observed-track matches per lead.
            let mut classes = classify_growth(&obs_cells[i - 1], &obs_cells[i], scale, gate_km);

            // Preserve the optional historical growth/decay experiment separately
            // for both estimators. This harness is not a parity test of production
            // track replay/coasting, joins, or the raw-byte forecast representation.
            let gd = args.growth_decay.then(|| {
                let (blobs, labels) =
                    segment_cells_labeled(&frames[i], args.object_threshold, args.min_area);
                let elapsed = (info.times[i] - info.times[i - 1]).num_seconds() as f32;
                tracks = advance_tracks(&tracks, blobs, scale, field, elapsed, elapsed, || {
                    next_track_id += 1;
                    next_track_id
                });
                // Labels beyond the u8 range fall back to 0 = pure advection
                // (mirrors the engine; clamping onto 255 would borrow cell
                // #254's tendency for every overflow cell).
                let label_map: Vec<u8> = labels
                    .iter()
                    .map(|&l| if l <= 254 { l as u8 } else { 0 })
                    .collect();
                let mut tend = [0f32; 256];
                for (k, t) in tracks.iter().take(254).enumerate() {
                    // Per-interval units to pair with the lead damp below.
                    tend[k + 1] = t.intensity_tendency * elapsed;
                }
                (tend, label_map)
            });

            for lead in 1..=(frames.len() - 1 - i).min(max_lead) {
                let lead_intervals =
                    (info.times[i + lead] - info.times[i]).num_seconds() as f32 / interval_secs;
                let started = Instant::now();
                let mut forecast = advect(&frames[i], field, lead_intervals, args.substeps);
                if let Some((tend, label_map)) = &gd {
                    let moved = advect_u8(
                        label_map,
                        w as usize,
                        h as usize,
                        0,
                        field,
                        lead_intervals,
                        args.substeps,
                    );
                    let damp =
                        EFOLD_INTERVALS * (1.0 - (-(lead_intervals) / EFOLD_INTERVALS).exp());
                    for (v, k) in forecast.data.iter_mut().zip(&moved) {
                        if v.is_finite() && *k > 0 {
                            *v += tend[*k as usize] * damp;
                        }
                    }
                }
                let advect_ms = started.elapsed().as_millis();
                println!("  lead +{lead}: advection {advect_ms}ms");
                for (k, &thr) in args.thresholds.iter().enumerate() {
                    nowcast[lead - 1][k].merge(&score(&forecast, &frames[i + lead], thr));
                    if !production {
                        persistence[lead - 1][k].merge(&score(&frames[i], &frames[i + lead], thr));
                    }
                }

                // Carry creation-time classes to this lead's observed cells.
                let prev_obs = &obs_cells[i + lead - 1];
                let cur_obs = &obs_cells[i + lead];
                let mut next_classes = vec![GrowthClass::Unknown; cur_obs.len()];
                for (pi, ci) in match_cells(prev_obs, cur_obs, scale, gate_km) {
                    next_classes[ci] = classes[pi];
                }
                classes = next_classes;

                let fc_cells = segment_cells(&forecast, args.object_threshold, args.min_area);
                let (o, g, d) = score_objects(&fc_cells, cur_obs, Some(&classes), scale, gate_km);
                obj_now[lead - 1][0].merge(&o);
                obj_now[lead - 1][1].merge(&g);
                obj_now[lead - 1][2].merge(&d);
                if !production {
                    let (po, pg, pd) =
                        score_objects(&obs_cells[i], cur_obs, Some(&classes), scale, gate_km);
                    obj_pers[lead - 1][0].merge(&po);
                    obj_pers[lead - 1][1].merge(&pg);
                    obj_pers[lead - 1][2].merge(&pd);
                }
            }
        }
        results.push((nowcast, obj_now));
    }
    let (baseline, obj_baseline) = &results[0];
    let (nowcast, obj_now) = &results[1];

    println!();
    println!(
        "lead  thr({})   CSI baseline  CSI production  CSI persist  POD production  FAR production",
        info.unit
    );
    for (li, row) in nowcast.iter().enumerate() {
        for (k, &thr) in args.thresholds.iter().enumerate() {
            println!(
                "  +{:<3} {:>6.1}   {:>12} {:>14} {:>12} {:>14} {:>14}",
                li + 1,
                thr,
                fmt_ratio(baseline[li][k].csi()),
                fmt_ratio(row[k].csi()),
                fmt_ratio(persistence[li][k].csi()),
                fmt_ratio(row[k].pod()),
                fmt_ratio(row[k].far()),
            );
        }
    }

    // Object-based table (#542): cell-level CSI by lead, overall and
    // stratified by the creation-time growing/decaying class, next to the
    // persistence baseline — the metric that exposes growth/decay blindness.
    println!();
    // Per-class columns are POD (hits/(hits+misses)): a spurious forecast
    // cell has no observed class, so false alarms exist only in the overall
    // CSI — labeling per-class columns "CSI" would silently print POD anyway.
    println!("lead  objCSI base/prod/pers  growPOD base/prod/pers  decayPOD base/prod/pers  cent.err km base/prod");
    for li in 0..max_lead {
        println!(
            "  +{:<3} {}/{}/{}  {}/{}/{}  {}/{}/{}  {}/{}",
            li + 1,
            fmt_ratio(obj_baseline[li][0].csi()),
            fmt_ratio(obj_now[li][0].csi()),
            fmt_ratio(obj_pers[li][0].csi()),
            fmt_ratio(obj_baseline[li][1].pod()),
            fmt_ratio(obj_now[li][1].pod()),
            fmt_ratio(obj_pers[li][1].pod()),
            fmt_ratio(obj_baseline[li][2].pod()),
            fmt_ratio(obj_now[li][2].pod()),
            fmt_ratio(obj_pers[li][2].pod()),
            fmt_ratio(obj_baseline[li][0].mean_centroid_error()),
            fmt_ratio(obj_now[li][0].mean_centroid_error()),
        );
    }

    // The gate (#520): beat persistence at the gate threshold, lead 1.
    // Membership was validated in parse_args, so position() always finds it.
    let gate_idx = args
        .thresholds
        .iter()
        .position(|&t| t == args.gate_threshold)
        .expect("gate threshold validated against thresholds at arg parse");
    let n = nowcast[0][gate_idx].csi();
    let p = persistence[0][gate_idx].csi();
    println!();
    match (n, p) {
        (Some(n), Some(p)) if n > p => {
            println!(
                "GATE PASS: production lead-1 CSI {} > persistence {} at {} {}",
                fmt_gate_ratio(Some(n)),
                fmt_gate_ratio(Some(p)),
                args.thresholds[gate_idx],
                info.unit
            );
            ExitCode::SUCCESS
        }
        (n, p) => {
            println!(
                "GATE FAIL: production lead-1 CSI {} vs persistence {} at {} {}",
                fmt_gate_ratio(n),
                fmt_gate_ratio(p),
                args.thresholds[gate_idx],
                info.unit
            );
            ExitCode::FAILURE
        }
    }
}
