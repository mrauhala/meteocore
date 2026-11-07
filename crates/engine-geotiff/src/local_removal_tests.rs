//! A local file removed or made unreadable after the scan catalogued it
//! (#234). The scan maps every local file it catalogues, and every read uses
//! that mapping (#204), never the path again: a map render, the u8 fast
//! path, overviews and EDR. So a request never meets a missing file. A file
//! deleted after the scan serves the bytes catalogued until the next scan,
//! whose listing no longer names it; a file re-created under the same name is
//! a new file again. A permission change after the scan changes nothing.

use ds_core::map_engine::{MapEngine, OutputCrs};

use super::*;

const OLDER: &str = "radar_20260324T2315Z.tif";
const NEWER: &str = "radar_20260324T2320Z.tif";

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/radar")
}

fn older() -> DateTime<Utc> {
    "2026-03-24T23:15:00Z".parse().unwrap()
}

fn newer() -> DateTime<Utc> {
    "2026-03-24T23:20:00Z".parse().unwrap()
}

/// A directory holding two frames of the radar fixture, removed on drop.
struct TwoFrames(PathBuf);

impl TwoFrames {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "meteocore_local_removal_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for name in [OLDER, NEWER] {
            std::fs::copy(fixture_dir().join(name), dir.join(name)).unwrap();
        }
        Self(dir)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TwoFrames {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(self.file(NEWER), std::fs::Permissions::from_mode(0o644));
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn engine(dir: &Path) -> GeoTiffEngine {
    let config: GeoTiffConfig = serde_json::from_value(serde_json::json!({
        "filename_template": "radar_%Y%m%dT%H%MZ.tif",
        "parameter": "reflectivity",
        "unit": "dBZ",
    }))
    .unwrap();
    GeoTiffEngine::new("local-removal", dir.to_str(), &config).unwrap()
}

/// The frame at `time` as one map view, every pixel.
fn render(engine: &GeoTiffEngine, time: DateTime<Utc>) -> Vec<Option<f64>> {
    let (width, height) = (400, 220);
    let tile = engine
        .get_raster_tile(
            [19.0, 59.0, 32.0, 70.0],
            width,
            height,
            Some(time),
            &OutputCrs::Wgs84,
            None,
            None,
            None,
        )
        .expect("a local frame renders");
    assert!(!tile.values.is_all_nodata());
    (0..(width * height) as usize)
        .map(|i| tile.values.value_at(i))
        .collect()
}

/// The area query's values at `time`.
fn area(engine: &GeoTiffEngine, time: DateTime<Utc>) -> Vec<Option<f64>> {
    let answer = engine
        .query_bbox(24.0, 60.0, 26.0, 62.0, Some((time, time)), None)
        .expect("a local frame answers");
    answer.ranges["reflectivity"].values.clone()
}

/// Deleted before any request read it, the newer frame still renders and
/// answers EDR exactly as the original file does, with no error. The next
/// poll's listing leaves it out, so its timestep is gone: time resolution
/// snaps to the older frame and EDR finds nothing at it. Re-created under
/// the same name, it is a new file: catalogued once the readiness check
/// settles, and served again.
#[test]
fn a_local_file_deleted_after_the_scan_serves_its_mapping_until_the_next_scan() {
    let reference = engine(&fixture_dir());
    let dir = TwoFrames::new();
    let engine = engine(&dir.0);
    assert_eq!(engine.raster_info().times, [older(), newer()]);

    std::fs::remove_file(dir.file(NEWER)).unwrap();
    assert_eq!(render(&engine, newer()), render(&reference, newer()));
    assert_eq!(area(&engine, newer()), area(&reference, newer()));
    assert_eq!(engine.resolve_time(Some(newer()), None), Some(newer()));

    engine.poll_once();
    assert_eq!(engine.raster_info().times, [older()]);
    assert_eq!(engine.resolve_time(Some(newer()), None), Some(older()));
    assert_eq!(engine.resolve_time(None, None), Some(older()));
    assert_eq!(render(&engine, newer()), render(&reference, older()));
    assert!(matches!(
        engine.query_bbox(24.0, 60.0, 26.0, 62.0, Some((newer(), newer())), None),
        Err(DataServerError::LocationNotFound(_))
    ));

    std::fs::copy(fixture_dir().join(NEWER), dir.file(NEWER)).unwrap();
    for _ in 0..3 {
        engine.poll_once();
    }
    assert_eq!(engine.raster_info().times, [older(), newer()]);
    assert_eq!(engine.resolve_time(None, None), Some(newer()));
    assert_eq!(render(&engine, newer()), render(&reference, newer()));
    assert_eq!(area(&engine, newer()), area(&reference, newer()));
}

/// A catalogued file made unreadable keeps its timestep: requests read the
/// mapping, and the poll reuses the entry, whose size, mtime and inode a
/// permission change leaves as they were.
#[test]
fn a_permission_change_after_the_scan_keeps_the_timestep() {
    use std::os::unix::fs::PermissionsExt;
    let reference = engine(&fixture_dir());
    let dir = TwoFrames::new();
    let engine = engine(&dir.0);
    std::fs::set_permissions(dir.file(NEWER), std::fs::Permissions::from_mode(0o000)).unwrap();

    assert_eq!(render(&engine, newer()), render(&reference, newer()));
    engine.poll_once();
    assert_eq!(engine.raster_info().times, [older(), newer()]);
    assert_eq!(area(&engine, newer()), area(&reference, newer()));
}
