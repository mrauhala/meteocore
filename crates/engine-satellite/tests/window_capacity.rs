//! Time windows larger than the scan cache: 1 MB here, about ten of the
//! cropped fixture scans. The polls must not download evicted scans in a
//! loop, and the collection that lost scans says so in one WARN per poll.
//! Once the windows fit again, its poll downloads what it lost, and
//! requests read every scan from memory.
//!
//! A separate test binary: the cache size is read once per process, and
//! the windows of every engine alive in the process count. One test only.

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

use ds_core::config::{SatelliteConfig, SatelliteProductConfig};
use ds_core::edr_engine::EdrEngine;
use ds_core::map_engine::MapEngine;
use ds_core::model::CoverageResponse;
use engine_satellite::{frame_metrics, frame_reingests, SatelliteEngine};

const C13: &str = "OR_ABI-L2-CMIPF-M6C13_G19_s20262681900199_e20262681909519_c20262681909592.nc";

/// A directory of GOES-19 band 13 scans on 2026-09-25, one per `HHMM`.
fn scans(slots: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/goes19-abi")
        .join(C13);
    for slot in slots {
        let name = C13.replace("_s20262681900199_", &format!("_s2026268{slot}199_"));
        std::fs::copy(&fixture, dir.path().join(name)).unwrap();
    }
    dir
}

fn engine(id: &str, dir: &Path) -> SatelliteEngine {
    let config = SatelliteConfig {
        provider: "goes-r".into(),
        data_path: Some(dir.to_string_lossy().into_owned()),
        endpoint: None,
        bucket: None,
        time_window: None,
        poll_interval_secs: 60,
        composites: Vec::new(),
        products: vec![SatelliteProductConfig {
            parameter: "ir_10_3".into(),
            title: "IR 10.3 µm brightness temperature".into(),
            unit: "K".into(),
            product: "ABI-L2-CMIPF".into(),
            band: Some(13),
            variable: "CMI".into(),
        }],
    };
    SatelliteEngine::new(id, &config).unwrap()
}

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

/// Poll once, returning the WARN lines the poll logged.
fn poll_warnings(engine: &SatelliteEngine) -> Vec<String> {
    let log = LogBuffer::default();
    let writer = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || engine.poll_once());
    let bytes = log.0.lock().unwrap();
    String::from_utf8_lossy(&bytes)
        .lines()
        .map(String::from)
        .collect()
}

#[test]
fn windows_larger_than_the_cache_are_not_downloaded_in_a_loop() {
    // Before the cache is first used.
    std::env::set_var("MC_SATELLITE_FRAME_CACHE_MB", "1");

    // B's five scans fit; each poll ingests four, newest first.
    let b_dir = scans(&["1900", "1910", "1920", "1930", "1940"]);
    let b = engine("sat-b", b_dir.path());
    b.poll_once();
    b.poll_once();
    let b_window = b.frame_window();
    assert_eq!(b_window.resident_bytes, b_window.bytes, "B alone fits");

    // A's eight scans fit alone, but not next to B's: its last ones
    // ingested, the oldest, are evicted to make room.
    let a_dir = scans(&[
        "2000", "2010", "2020", "2030", "2040", "2050", "2100", "2110",
    ]);
    let a = engine("sat-a", a_dir.path());
    a.poll_once();
    a.poll_once();
    let a_window = a.frame_window();
    let capacity = frame_metrics().capacity_bytes;
    assert_eq!(capacity, 1 << 20);
    assert!(a_window.bytes <= capacity, "A alone fits");
    assert!(a_window.bytes + b_window.bytes > capacity, "A and B do not");
    assert!(a_window.resident_bytes < a_window.bytes, "A lost scans");
    assert_eq!(b.frame_window(), b_window, "B kept its scans");

    // Downloading A's scans back would evict others in a window, which the
    // next poll would download back in turn. No poll downloads anything;
    // A, which lost scans, warns once per poll, and B nothing.
    let reingests = frame_reingests();
    for _ in 0..3 {
        let warnings = poll_warnings(&a);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let warning = &warnings[0];
        assert!(warning.contains("WARN"), "{warning}");
        assert!(warning.contains("[sat-a] 3 scan(s)"), "{warning}");
        assert!(warning.contains("sat-b 0.5 MB"), "{warning}");
        assert!(
            warning.contains("more than MC_SATELLITE_FRAME_CACHE_MB=1"),
            "{warning}"
        );
        let warnings = poll_warnings(&b);
        assert!(warnings.is_empty(), "{warnings:?}");
    }
    assert_eq!(frame_reingests(), reingests, "no scan downloaded again");
    assert_eq!(a.frame_window(), a_window);
    assert_eq!(b.frame_window(), b_window);

    // B's collection goes away (a reload removed it): its scans leave
    // memory, A's window fits, and A's next poll downloads what it lost.
    let misses = frame_metrics().misses;
    drop(b);
    let warnings = poll_warnings(&a);
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(a.frame_window().resident_bytes, a_window.bytes);
    assert_eq!(frame_reingests(), reingests + 3);

    // Requests read every scan from memory: the source is gone, and
    // nothing is downloaded.
    for entry in std::fs::read_dir(a_dir.path()).unwrap() {
        std::fs::remove_file(entry.unwrap().path()).unwrap();
    }
    let [w, s, e, n] = a.raster_info().spatial_extent.unwrap();
    let point = format!("POINT({} {})", (w + e) / 2.0, (s + n) / 2.0);
    let CoverageResponse::Single(series) = a
        .query_position(&point, None, None, None, None)
        .expect("every scan is in memory")
    else {
        panic!("one coverage");
    };
    assert_eq!(series.ranges["ir_10_3"].values.len(), 8);
    assert_eq!(frame_metrics().misses, misses, "no request downloaded");
}
