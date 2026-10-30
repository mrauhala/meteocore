//! #476 and #1309 acceptance: a PVOL source whose pre-warm floor exceeds
//! `MC_PVOL_PIXEL_CACHE_MB` logs one WARN naming its sites, volumes,
//! sweeps, quantities and sweep size, and no more while the floor stays
//! within the size it suggested: not at the next poll.
//!
//! A separate test binary: the pixel cache is sized once per process from
//! the environment, and the WARN is caught by a global subscriber, since
//! the poll logs from a runtime worker. One test only.
//!
//! The committed fixture is the only PVOL in the tree, and tiny (4 rays of
//! 8 bins), so the source is a thousand links to it: one site, a thousand
//! volumes, two pre-warmed sweeps of two quantities each, over 1 MiB.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ds_core::config::OdimConfig;
use engine_odim::PolarVolumeEngine;

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl LogBuffer {
    fn lines(&self) -> Vec<String> {
        String::from_utf8_lossy(&self.0.lock().unwrap())
            .lines()
            .map(String::from)
            .collect()
    }
}

fn config() -> OdimConfig {
    OdimConfig {
        filename_template: None,
        filename_pattern: None,
        timestamp_format: None,
        parameter: None,
        unit: None,
        nodata: None,
        gain: None,
        offset: None,
        poll_interval_secs: 1,
        max_files: Some(5000),
        endpoint: None,
        bucket: None,
        prefix_pattern: None,
        time_window: None,
        discovery: None,
        cadence_secs: None,
        resampling: Default::default(),
        prewarm_sweeps: 2,
        prewarm_quantities: Some(vec!["DBZH".into(), "VRAD".into()]),
    }
}

/// The fixture as `dir/pvol_<i>.h5`: a hard link, or a copy where the
/// temporary directory is on another filesystem.
fn link(fixture: &Path, dir: &Path, i: usize) {
    let to = dir.join(format!("pvol_{i:04}.h5"));
    std::fs::hard_link(fixture, &to)
        .or_else(|_| std::fs::copy(fixture, &to).map(drop))
        .unwrap();
}

#[test]
fn a_floor_above_the_pixel_cache_warns_once() {
    // Before the pixel cache is first used.
    std::env::set_var("MC_PVOL_PIXEL_CACHE_MB", "1");
    let log = LogBuffer::default();
    let writer = log.clone();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || writer.clone())
            .finish(),
    )
    .unwrap();

    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/pvol-cold-batch.h5");
    let dir = tempfile::tempdir().unwrap();
    for i in 0..1000 {
        link(&fixture, dir.path(), i);
    }
    let path = dir.path().to_str().unwrap().to_string();
    let engine = Arc::new(PolarVolumeEngine::new("pvol-floor", Some(&path), &config()).unwrap());

    let floor = engine.pixel_cache_floor().expect("published by the scan");
    assert!(floor.floor);
    assert!(floor.bytes > 1 << 20, "{floor:?}");
    let input = |name: &str| {
        floor
            .inputs
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| *v)
    };
    assert_eq!(input("sites"), Some(1));
    assert_eq!(input("volumes"), Some(1000));
    assert_eq!(input("sweeps"), Some(2));
    assert_eq!(input("quantities"), Some(2));
    // A u8 moment of 4 × 8 cells, its key and overhead: what the cache
    // weighs each pre-warmed entry.
    let sweep = input("sweep_bytes").unwrap();
    assert!((32 + 64..32 + 64 + 300).contains(&sweep), "{sweep}");
    assert_eq!(floor.bytes, 2 * 1000 * 4 * sweep, "{floor:?}");

    let warnings = log.lines();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    let warning = &warnings[0];
    for expected in [
        "WARN",
        "pvol_pixel_cache is 1 MiB (env:MC_PVOL_PIXEL_CACHE_MB), below its floor of",
        "pvol-floor ",
        "from sites 1, volumes 1000, sweeps 2, quantities 2, sweep_bytes ",
        "set MC_PVOL_PIXEL_CACHE_MB to ",
    ] {
        assert!(warning.contains(expected), "{expected:?} not in {warning}");
    }

    // A volume more: the next poll publishes a floor a thousandth larger,
    // within the size the WARN suggested, and logs nothing.
    link(&fixture, dir.path(), 1000);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let poller = engine.clone();
        let task = tokio::spawn(async move { poller.poll_loop().await });
        let until = Instant::now() + Duration::from_secs(30);
        let volumes = |engine: &PolarVolumeEngine| {
            engine.pixel_cache_floor().and_then(|floor| {
                floor
                    .inputs
                    .iter()
                    .find(|(n, _)| *n == "volumes")
                    .map(|(_, v)| *v)
            })
        };
        while volumes(&engine) != Some(1001) {
            assert!(Instant::now() < until, "no poll published the new volume");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        engine.shutdown();
        task.await.unwrap();
    });
    let warnings = log.lines();
    assert_eq!(warnings.len(), 1, "the poll warned again: {warnings:?}");
}
