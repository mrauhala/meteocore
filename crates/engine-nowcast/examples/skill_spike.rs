//! Phase-0 hindcast harness for the nowcasting epic (#519 / #520).
//!
//! Loads a directory of composite GeoTIFF frames through the public
//! `GeoTiffEngine` (the same decode path phase 1 will consume via
//! `Arc<dyn MapEngine>`), estimates motion from each consecutive frame pair,
//! extrapolates, and scores the extrapolation against the frame that actually
//! followed — next to a persistence baseline.
//!
//! Gate: nowcast CSI must beat persistence CSI at the gate threshold, lead 1.
//!
//! ```text
//! cargo run --release -p engine-nowcast --example skill_spike -- \
//!     --dir testdata/smhi-radar-geotiff-4326 \
//!     --template "%Y%m%d%H%M%S_smhi_radar.tif"
//! ```

use std::process::ExitCode;
use std::time::Instant;

use ds_core::config::GeoTiffConfig;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterValues};
use engine_geotiff::GeoTiffEngine;
use engine_nowcast::motion::{estimate_motion, MotionOptions};
use engine_nowcast::skill::{score, Contingency};
use engine_nowcast::{advect::advect, Grid};

/// Keep the working grid at most this many pixels (halve dims until it fits).
const MAX_PIXELS: usize = 6_000_000;

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
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        dir: String::new(),
        template: String::new(),
        thresholds: vec![10.0, 20.0, 35.0],
        gate_threshold: 20.0,
        min_echo: 10.0,
        block: 32,
        search: 20,
        substeps: 4,
        nodata: None,
        scale: None,
        offset: None,
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
            other => return Err(format!("unknown flag {other}")),
        }
    }
    if args.dir.is_empty() || args.template.is_empty() {
        return Err(
            "usage: skill_spike --dir <fixture dir> --template <strftime filename> \
                    [--thresholds 10,20,35] [--gate-threshold 20] [--min-echo 10] \
                    [--block 32] [--search 20] [--substeps 4] \
                    [--nodata <raw>] [--scale <gain>] [--offset <off>]"
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

    let [mut w, mut h] = info.grid_size.unwrap_or([1024, 1024]);
    while (w as usize) * (h as usize) > MAX_PIXELS {
        w /= 2;
        h /= 2;
    }
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
        println!("cadence: {}s between frames", min);
        if max as f64 > min as f64 * 1.05 {
            eprintln!(
                "warning: irregular cadence ({min}–{max}s); vectors are per-interval and will \
                 mix speeds"
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
        let (min, max) = finite
            .iter()
            .fold((f32::MAX, f32::MIN), |(lo, hi), &v| (lo.min(v), hi.max(v)));
        println!(
            "  {t}: {} finite px, range [{min:.1}, {max:.1}] {}, {} px >= {} (echo)",
            finite.len(),
            info.unit,
            echo,
            args.min_echo
        );
        frames.push(grid);
    }

    let opts = MotionOptions {
        block: args.block,
        search_radius: args.search,
        min_echo: args.min_echo,
        ..MotionOptions::default()
    };

    // Aggregate (lead, threshold) → nowcast + persistence tables across every
    // usable anchor frame i (motion from i-1→i, verify at i+lead).
    let max_lead = frames.len() - 2;
    let mut nowcast = vec![vec![Contingency::default(); args.thresholds.len()]; max_lead];
    let mut persistence = vec![vec![Contingency::default(); args.thresholds.len()]; max_lead];

    for i in 1..frames.len() - 1 {
        let started = Instant::now();
        let field = estimate_motion(&frames[i - 1], &frames[i], &opts);
        let motion_ms = started.elapsed().as_millis();
        let measured = field.measured.iter().filter(|&&m| m).count();
        println!(
            "anchor {}: motion {}ms, {} of {} blocks measured",
            info.times[i],
            motion_ms,
            measured,
            field.measured.len()
        );

        for lead in 1..=(frames.len() - 1 - i) {
            let started = Instant::now();
            let forecast = advect(&frames[i], &field, lead as f32, args.substeps);
            let advect_ms = started.elapsed().as_millis();
            println!("  lead +{lead}: advection {advect_ms}ms");
            for (k, &thr) in args.thresholds.iter().enumerate() {
                nowcast[lead - 1][k].merge(&score(&forecast, &frames[i + lead], thr));
                persistence[lead - 1][k].merge(&score(&frames[i], &frames[i + lead], thr));
            }
        }
    }

    println!();
    println!(
        "lead  thr({})   CSI nowcast  CSI persist  POD nowcast  FAR nowcast",
        info.unit
    );
    for (li, row) in nowcast.iter().enumerate() {
        for (k, &thr) in args.thresholds.iter().enumerate() {
            println!(
                "  +{:<3} {:>6.1}   {:>11} {:>12} {:>12} {:>12}",
                li + 1,
                thr,
                fmt_ratio(row[k].csi()),
                fmt_ratio(persistence[li][k].csi()),
                fmt_ratio(row[k].pod()),
                fmt_ratio(row[k].far()),
            );
        }
    }

    // The gate (#520): beat persistence at the gate threshold, lead 1.
    let gate_idx = args
        .thresholds
        .iter()
        .position(|&t| t == args.gate_threshold)
        .unwrap_or(0);
    let n = nowcast[0][gate_idx].csi();
    let p = persistence[0][gate_idx].csi();
    println!();
    match (n, p) {
        (Some(n), Some(p)) if n > p => {
            println!(
                "GATE PASS: lead-1 CSI {} > persistence {} at {} {}",
                fmt_ratio(Some(n)),
                fmt_ratio(Some(p)),
                args.thresholds[gate_idx],
                info.unit
            );
            ExitCode::SUCCESS
        }
        (n, p) => {
            println!(
                "GATE FAIL: lead-1 CSI {} vs persistence {} at {} {}",
                fmt_ratio(n),
                fmt_ratio(p),
                args.thresholds[gate_idx],
                info.unit
            );
            ExitCode::FAILURE
        }
    }
}
