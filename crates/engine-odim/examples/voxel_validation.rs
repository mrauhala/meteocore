//! Offline native-versus-voxel diagnostic report (#641); see examples/README.md.
use std::collections::HashMap;
use std::path::Path;

use ds_core::config::OdimConfig;
use ds_core::volume::VolumeEngine;
use engine_odim::pvol::{read_polar_volume, visit_moments_pixels};
use engine_odim::voxel_diagnostics::{compare, NativeSweep};
use engine_odim::PolarVolumeEngine;

fn value(v: Option<f64>) -> String {
    v.map(|v| format!("{v:.3}")).unwrap_or_else(|| "n/a".into())
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args.next().ok_or(
        "usage: voxel_validation FILE.h5 [QUANTITY=DBZH] [BEAMWIDTH_DEG=1] [NR,NA,NH;...]",
    )?;
    let quantity = args.next().unwrap_or_else(|| "DBZH".into());
    if engine_odim::quantities::quantity_unit(&quantity) != "dBZ" {
        return Err("quantity must have dBZ units; no fallback to TH or another moment".into());
    }
    let beamwidth: f64 = args.next().unwrap_or_else(|| "1".into()).parse()?;
    let dims = args
        .next()
        .unwrap_or_else(|| "48,180,24;128,360,48;256,360,96".into());
    if args.next().is_some() {
        return Err("too many arguments".into());
    }
    let dims: Vec<[usize; 3]> = dims
        .split(';')
        .map(|s| {
            let values: Result<Vec<usize>, _> = s.split(',').map(str::parse).collect();
            let values = values.map_err(|e| e.to_string())?;
            values
                .try_into()
                .map_err(|_| "each grid needs three dimensions".to_string())
        })
        .collect::<Result<_, _>>()?;
    if !beamwidth.is_finite() || beamwidth <= 0.0 || beamwidth > 10.0 {
        return Err("beam width must be in (0, 10] degrees".into());
    }
    // Bound the offline tool too: don't silently sample or truncate native data.
    if std::fs::metadata(&path)?.len() > 256 * 1024 * 1024 {
        return Err("input exceeds the offline 256 MiB file limit".into());
    }
    let bytes = std::fs::read(&path)?;
    let volume = read_polar_volume(&bytes)?;
    let requested: Vec<_> = volume
        .sweeps
        .iter()
        .filter_map(|s| {
            s.moments
                .iter()
                .find(|m| m.quantity == quantity)
                .map(|m| (s, m))
        })
        .collect();
    let samples = requested
        .iter()
        .try_fold(0usize, |n, (s, _)| {
            s.nrays.checked_mul(s.nbins).and_then(|v| n.checked_add(v))
        })
        .ok_or("native sample count overflow")?;
    if samples > 32_000_000 {
        return Err("input exceeds the offline 32M native-sample limit".into());
    }
    let mut pixels = HashMap::new();
    let mut decode_error = None;
    visit_moments_pixels(
        &bytes,
        requested
            .iter()
            .map(|(s, m)| (m.dataset_path.as_str(), s.nrays, s.nbins)),
        |path, result| match result {
            Ok(p) => {
                pixels.insert(path.to_string(), p);
            }
            Err(e) => {
                decode_error = Some(format!("{path}: {e}"));
            }
        },
    )?;
    if let Some(e) = decode_error {
        return Err(e.into());
    }
    let native: Vec<_> = requested
        .iter()
        .map(|(sweep, moment)| NativeSweep {
            sweep,
            moment,
            pixels: &pixels[&moment.dataset_path],
        })
        .collect();

    // One-file directory prevents nearest-time selection or a sibling volume
    // with the same timestamp from accidentally becoming the comparison target.
    let isolated = tempfile::tempdir()?;
    std::fs::copy(&path, isolated.path().join("input.h5"))?;
    let config = OdimConfig {
        filename_template: None,
        filename_pattern: None,
        timestamp_format: None,
        parameter: None,
        unit: None,
        nodata: None,
        gain: None,
        offset: None,
        poll_interval_secs: 30,
        max_files: None,
        endpoint: None,
        bucket: None,
        prefix_pattern: None,
        time_window: None,
        discovery: None,
        cadence_secs: None,
        resampling: Default::default(),
        prewarm_sweeps: 0,
    };
    let engine = PolarVolumeEngine::new("voxel-validation", isolated.path().to_str(), &config)?;
    let sites = engine.sites();
    let nod = sites.first().ok_or("input has no radar site")?.0.as_str();
    let view = engine.site_view(nod, "voxel-validation-site");
    println!(
        "file={} site={} time={} quantity={} native_sweeps={} native_samples={}",
        Path::new(&path).display(),
        nod,
        volume.time,
        quantity,
        native.len(),
        samples
    );
    println!("reference=constant-value beam support; assumed full beam width={beamwidth}deg; heights above antenna; no quantity fallback; first requested-quantity sweep per repeated tilt; no integration across beam gaps");
    println!("dims,range_km,native_gates,native_peak_dbz,first_tilt_peak_dbz,voxel_peak_dbz,native_top18_m,voxel_top18_m,native_top45_m,voxel_top45_m,native_top50_m,voxel_top50_m,finite_voxels,finite_without_beam_support,columns,max_ref_integral_kg_m2,max_voxel_integral_kg_m2,max_member35_integral_kg_m2,max_paired_integral_error_kg_m2");
    for dims in dims {
        let grid = view.read_voxel_grid(Some(&quantity), Some(volume.time), Some(dims), None)?;
        let report = compare(&grid, &native, beamwidth)?;
        for (band, row) in ["0-50", "50-100", "100-150", "150+"].iter().zip(report) {
            println!(
                "{}x{}x{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{:.6},{:.6},{:.6},{:.6}",
                dims[0],
                dims[1],
                dims[2],
                band,
                row.native_echo_gates,
                value(row.native_peak_dbz),
                value(row.first_tilt_peak_dbz),
                value(row.voxel_peak_dbz),
                value(row.native_top_m[0]),
                value(row.voxel_top_m[0]),
                value(row.native_top_m[1]),
                value(row.voxel_top_m[1]),
                value(row.native_top_m[2]),
                value(row.voxel_top_m[2]),
                row.finite_voxels,
                row.finite_without_beam_support,
                row.columns,
                row.max_reference_integral_kg_m2,
                row.max_voxel_integral_kg_m2,
                row.max_voxel_member_integral_kg_m2,
                row.max_abs_integral_difference_kg_m2
            );
        }
    }
    Ok(())
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("voxel_validation: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
