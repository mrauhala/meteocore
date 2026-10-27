//! `NetcdfEngine::open_file_bytes`, the estimated heap of a collection's
//! open files, against the heap they leave allocated, measured with a
//! counting allocator (#1307): within 25 %. One test: every thread's
//! allocations count, so nothing may run alongside.
//!
//! A collection holds more than its open files (its layout and catalog),
//! so each check takes the difference between the same collection over
//! all of a fixture's files and over the first alone: what the other open
//! files hold, with the little their catalog entries add. The model behind
//! the estimate was fitted to these fixtures; the check pins it.

use std::path::{Path, PathBuf};

use ds_core::config::NetcdfConfig;
use ds_core::heap::Counting;
use engine_netcdf::NetcdfEngine;

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// `(bytes held, gauge)` of a collection over `files`, copied into a
/// directory of their own, each under a name of its own.
fn collection(files: &[PathBuf]) -> (f64, f64) {
    let dir = tempfile::tempdir().unwrap();
    for (i, file) in files.iter().enumerate() {
        let name = file.file_name().unwrap().to_string_lossy();
        std::fs::copy(file, dir.path().join(format!("{i:02}_{name}"))).unwrap();
    }
    let config: NetcdfConfig = toml::from_str(&format!(
        "data_path = {:?}\nmax_runs = 100\nmax_files = 100000\n",
        dir.path().to_str().unwrap()
    ))
    .unwrap();
    let before = Counting::live();
    let engine = NetcdfEngine::new("netcdf-heap", &config).unwrap();
    let held = (Counting::live() - before) as f64;
    let gauge = engine.open_file_bytes() as f64;
    drop(engine);
    (held, gauge)
}

fn fixtures(dir: &str) -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(dir);
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "nc"))
        .collect();
    files.sort();
    files
}

#[test]
fn open_file_estimate_matches_the_heap_held() {
    // A first build initialises the process-wide statics an ingest touches.
    collection(&fixtures("meps-netcdf"));
    // A fixture of one file is ingested twice, under two names: files
    // without a run, or of the same run, are all kept open.
    for dir in [
        "meps-netcdf",
        "meps-netcdf-pl",
        "netcdf-remote",
        "netcdf-global",
        "netcdf-pressure-13",
        "netcdf-pressure-pa",
        "netcdf-curvilinear",
        "netcdf-projections/laea",
    ] {
        let mut files = fixtures(dir);
        if files.len() == 1 {
            files.push(files[0].clone());
        }
        let (one_held, one_gauge) = collection(&files[..1]);
        let (all_held, all_gauge) = collection(&files);
        let (held, estimate) = (all_held - one_held, all_gauge - one_gauge);
        let error = (estimate - held) / held;
        println!(
            "{dir}: {} more open files hold {held} bytes, estimated {estimate} ({:+.1} %); \
             the first file's collection {one_held}, of it the open file {one_gauge}",
            files.len() - 1,
            100.0 * error
        );
        assert!(
            error.abs() <= 0.25,
            "{dir}: the estimate {estimate} is {:.1} % off the {held} bytes held",
            100.0 * error
        );
    }
}
