//! An EDR query's frames, read as jobs of their own (#1495): against a
//! store whose every read takes a set time, a position query over 24
//! remote frames keeps [`frames::REMOTE_FRAME_READS`] reads in flight and
//! takes about 24 ÷ 8 delays, not 24, while an area query reads one frame
//! at a time. Each frame of the answer is what that frame alone answers, in
//! the catalog's order, for position and for area on a geographic and a
//! projected source. Past the deadline no further read starts and the query
//! is `DeadlineExceeded`, the 504 of EDR.
//!
//! The tiles these tests decode would stay in the process-wide
//! decoded-chunk cache for the rest of the suite, where a test that reads
//! its own chunks back fails once a shard is full of others'. So they read
//! few, and each engine's chunks are dropped with it ([`Engine`]).

use std::time::{Duration, Instant};

use ds_storage::object_store::path::Path as ObjectPath;
use ds_storage::test_util::SlowReads;
use ds_storage::DataStore;

use super::*;
use crate::frames::{Frame, AREA_FRAME_READS, REMOTE_FRAME_READS};
use crate::reader::DataSource;

/// Frames of a position query in the in-flight tests: as many as the
/// issue's case.
const FRAMES: usize = 24;

/// An empty local directory for the engine to start on, removed on drop.
struct EmptyDir(PathBuf);

impl EmptyDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "meteocore_frame_reads_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for EmptyDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The `i`th timestep of a test's frames, five minutes apart from 22:00.
fn time_of(i: usize) -> DateTime<Utc> {
    "2026-03-24T22:00:00Z".parse::<DateTime<Utc>>().unwrap()
        + chrono::Duration::minutes(5 * i as i64)
}

/// A committed fixture's frames, as a remote engine serves them.
#[derive(Clone, Copy)]
struct Fixture {
    /// Its directory under `testdata/`, and its files there.
    dir: &'static str,
    files: &'static [&'static str],
    /// Its files' names: `{stem}_<time>.{extension}`.
    stem: &'static str,
    extension: &'static str,
    /// A pixel holding data in every file, not the same value in each:
    /// an answer placing a frame's value at another frame's step differs
    /// from the right one (found with `gdallocationinfo`).
    varying: (u32, u32),
    /// The half-width and half-height in degrees of an area around it that
    /// stays in the pixel's tile.
    around: (f64, f64),
}

/// The geographic fixture's five frames, 512 × 512 Float32 tiles: -14,
/// -9, -9.5, -12.5 and -15.5 dBZ at `varying`.
const GEOGRAPHIC: Fixture = Fixture {
    dir: "radar",
    files: &[
        "radar_20260324T2315Z.tif",
        "radar_20260324T2320Z.tif",
        "radar_20260324T2325Z.tif",
        "radar_20260324T2330Z.tif",
        "radar_20260324T2335Z.tif",
    ],
    stem: "radar",
    extension: "tif",
    varying: (1827, 328),
    around: (0.4, 0.3),
};

/// The Lambert conformal fixture's two frames: -3 and -2 dBZ at
/// `varying`, a projected source whose area cells each frame's read
/// resamples.
const PROJECTED: Fixture = Fixture {
    dir: "radar-native",
    files: &["norway_20260324T2315Z.tiff", "norway_20260324T2320Z.tiff"],
    stem: "norway",
    extension: "tiff",
    varying: (944, 320),
    around: (0.2, 0.1),
};

/// The TM35FIN fixture's one frame, 256 × 256 byte tiles: 64 KiB decoded,
/// a sixteenth of a Float32 tile, for the tests that count reads rather
/// than values.
const SMALL_TILES: Fixture = Fixture {
    dir: "radar-tm35fin",
    files: &["radar_tm35_20260406T0640Z.tif"],
    stem: "radar_tm35",
    extension: "tif",
    varying: (240, 180),
    around: (0.0, 0.0),
};

/// A remote engine of the tests, whose decoded chunks leave the
/// process-wide cache with it.
struct Engine(GeoTiffEngine);

impl std::ops::Deref for Engine {
    type Target = GeoTiffEngine;
    fn deref(&self) -> &GeoTiffEngine {
        &self.0
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        crate::decoded_cache::forget_for_test(self.0.tile_cache.namespace());
    }
}

/// A remote engine over `frames` frames in `store`, frame `i` holding the
/// fixture's file `i` modulo its count. `new` builds a remote engine only
/// for s3:// or http(s):// URLs, so it starts on an empty local directory
/// and is then pointed at the store, with the tile cache a remote source
/// gets; the poll's header reads catalogue the frames. The store answers at
/// once until a test sets its delay.
fn remote_engine(
    local: &EmptyDir,
    store: &Arc<SlowReads>,
    fixture: Fixture,
    frames: usize,
) -> Engine {
    let Fixture {
        dir,
        files,
        stem,
        extension,
        ..
    } = fixture;
    let testdata = concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata");
    let bytes: Vec<Vec<u8>> = files
        .iter()
        .map(|name| std::fs::read(format!("{testdata}/{dir}/{name}")).unwrap())
        .collect();
    for i in 0..frames {
        let key = format!("d/{stem}_{}.{extension}", time_of(i).format("%Y%m%dT%H%MZ"));
        store.put(&key, bytes[i % bytes.len()].clone());
    }
    let config: GeoTiffConfig = serde_json::from_value(serde_json::json!({
        "filename_template": format!("{stem}_%Y%m%dT%H%MZ.{extension}"),
        "parameter": "reflectivity",
        "unit": "dBZ",
    }))
    .unwrap();
    let mut engine = GeoTiffEngine::new("frame-reads", local.0.to_str(), &config).unwrap();
    engine.store_mode = StoreMode::Remote {
        store: DataStore::new(store.clone()),
        prefix: ObjectPath::from("d"),
    };
    engine.tile_cache = Arc::new(cache::TileCache::new(
        config.effective_tile_cache_mb() * 1024 * 1024,
    ));
    engine.poll_once();
    let times: Vec<_> = engine.catalog.load().entries.keys().copied().collect();
    assert_eq!(times, (0..frames).map(time_of).collect::<Vec<_>>());
    Engine(engine)
}

/// The `(lon, lat)` of pixel `(col, row)` of the engine's first frame.
fn pixel(engine: &GeoTiffEngine, (col, row): (u32, u32)) -> (f64, f64) {
    let catalog = engine.catalog.load();
    let metadata = catalog.entries.values().next().unwrap().metadata().unwrap();
    metadata.geo_transform.pixel_to_world(col, row)
}

/// The one range of a single coverage, and its `t` axis.
fn series(result: QueryResult) -> (Vec<DateTime<Utc>>, Vec<Option<f64>>) {
    let t = match result.domain {
        DomainDescription::PointSeries { t, .. } => t,
        DomainDescription::Grid { t, .. } => t.unwrap(),
        other => panic!("{other:?}"),
    };
    (t, result.ranges["reflectivity"].values.clone())
}

/// The acceptance case of #1495: a position query over 24 remote frames,
/// every range read taking 250 ms, keeps eight reads in flight and answers
/// in about three delays. One frame at a time took 24 (6 s); the bound
/// here, half of that, is loose enough for a loaded CI host.
#[test]
fn a_position_query_reads_its_remote_frames_eight_at_a_time() {
    let local = EmptyDir::new();
    let store = Arc::new(SlowReads::default());
    let engine = remote_engine(&local, &store, SMALL_TILES, FRAMES);
    let (lon, lat) = pixel(&engine, SMALL_TILES.varying);
    let delay = Duration::from_millis(250);
    store.reset();
    store.set_delay(delay);

    let started = Instant::now();
    let (times, values) = series(engine.query_point(lat, lon, None, None).unwrap());
    let elapsed = started.elapsed();

    assert_eq!(times.len(), FRAMES);
    assert_eq!(values.len(), FRAMES);
    assert!(
        values.iter().all(|v| *v == values[0]),
        "one file: {values:?}"
    );
    assert_eq!(store.reads(), FRAMES, "one tile read per frame");
    assert_eq!(store.peak(), REMOTE_FRAME_READS, "frames in flight");
    let rounds = FRAMES.div_ceil(REMOTE_FRAME_READS) as u32;
    assert!(
        elapsed >= delay * rounds && elapsed < delay * FRAMES as u32 / 2,
        "{elapsed:?} for {FRAMES} frames of {delay:?}, {REMOTE_FRAME_READS} at a time"
    );
}

/// An area query reads its frames one at a time: their tiles are fetched on
/// the tile-fetch pool the cold map renders share. Each frame's window here
/// is one tile, so one read at a time is one frame at a time.
#[test]
fn an_area_query_reads_one_frame_at_a_time() {
    let frames = 6;
    let local = EmptyDir::new();
    let store = Arc::new(SlowReads::default());
    let engine = remote_engine(&local, &store, SMALL_TILES, frames);
    let (lon, lat) = pixel(&engine, (100, 100));
    let bbox = ds_core::feature::Bbox {
        west: lon - 0.1,
        south: lat - 0.05,
        east: lon + 0.1,
        north: lat + 0.05,
    };
    let delay = Duration::from_millis(50);
    store.reset();
    store.set_delay(delay);

    let started = Instant::now();
    let (times, _) = series(engine.query_grid(&bbox, None, None, None).unwrap());
    let elapsed = started.elapsed();

    assert_eq!(times.len(), frames);
    assert_eq!(store.reads(), frames, "one tile read per frame");
    assert_eq!(store.peak(), AREA_FRAME_READS, "frames in flight");
    assert!(elapsed >= delay * frames as u32, "{elapsed:?}");
}

/// Each frame of a position answer, read eight at a time, and of an area
/// answer, read in turn, is what a query of that frame alone answers, at
/// its own step: ten frames of the geographic fixture's five distinct ones,
/// a position query's read in two rounds, and
/// four of the Lambert conformal one's two, whose area cells each frame's
/// read resamples.
#[test]
fn every_frame_answers_at_its_own_step() {
    for (fixture, frames) in [(GEOGRAPHIC, 10), (PROJECTED, 4)] {
        let Fixture {
            dir,
            files,
            varying,
            around: (dx, dy),
            ..
        } = fixture;
        let local = EmptyDir::new();
        let store = Arc::new(SlowReads::default());
        let engine = remote_engine(&local, &store, fixture, frames);
        let alone = |i: usize| Some((time_of(i), time_of(i)));

        let (lon, lat) = pixel(&engine, varying);
        let (times, values) = series(engine.query_point(lat, lon, None, None).unwrap());
        assert_eq!(times, (0..frames).map(time_of).collect::<Vec<_>>());
        assert!(values.iter().all(Option::is_some), "{dir}: {values:?}");
        assert!(
            values[..files.len()].windows(2).all(|w| w[0] != w[1]),
            "{dir}: the frames differ: {values:?}"
        );
        for (i, value) in values.iter().enumerate() {
            let (_, one) = series(engine.query_point(lat, lon, alone(i), None).unwrap());
            assert_eq!(one, [*value], "{dir}: position frame {i}");
        }

        let bbox = ds_core::feature::Bbox {
            west: lon - dx,
            south: lat - dy,
            east: lon + dx,
            north: lat + dy,
        };
        let (times, cells) = series(engine.query_grid(&bbox, None, None, None).unwrap());
        assert_eq!(times.len(), frames);
        let steps: Vec<_> = cells.chunks(cells.len() / frames).collect();
        assert!(
            steps[..files.len()].windows(2).all(|w| w[0] != w[1]),
            "{dir}: the frames differ"
        );
        for (i, step) in steps.iter().enumerate() {
            let (_, one) = series(engine.query_grid(&bbox, None, alone(i), None).unwrap());
            assert_eq!(one, *step, "{dir}: area frame {i}");
        }
    }
}

/// Past the deadline no further frame is read: the eight reads in flight
/// end at it, the other sixteen never start, and the query is
/// `DeadlineExceeded`, which EDR answers with 504.
#[test]
fn a_query_past_its_deadline_starts_no_more_reads() {
    let local = EmptyDir::new();
    let store = Arc::new(SlowReads::default());
    let engine = remote_engine(&local, &store, SMALL_TILES, FRAMES);
    let (lon, lat) = pixel(&engine, SMALL_TILES.varying);
    store.reset();
    store.set_delay(Duration::from_millis(500));

    let _deadline = ds_core::deadline::enter(Some(Instant::now() + Duration::from_millis(100)));
    let answer = engine.query_point(lat, lon, None, None);
    assert!(
        matches!(answer, Err(DataServerError::DeadlineExceeded)),
        "{answer:?}"
    );
    let reads = store.reads();
    assert!(
        (1..=REMOTE_FRAME_READS).contains(&reads),
        "{reads} reads started"
    );
    assert_eq!(store.active.load(Ordering::SeqCst), 0, "none left running");
}

/// A position query reads the frames an object store serves eight at a
/// time. One holding a frame decoded in process, a local file or a remote
/// file the scan downloaded whole, reads one at a time, as before: its cold
/// decode reserves the decoder's 64 MiB cap of the decode budget. So does
/// one holding a STAC asset read by direct HTTP, which no host limit or
/// 429 back-off bounds.
#[test]
fn only_object_store_frames_are_read_together() {
    let local = EmptyDir::new();
    let store = Arc::new(SlowReads::default());
    let engine = remote_engine(&local, &store, SMALL_TILES, 2);
    let catalog = engine.catalog.load();
    let entries: Vec<_> = catalog.entries.iter().collect();
    let remote = frames::frames(&entries);
    assert_eq!(remote.len(), 2);
    let with = |source: DataSource| Frame {
        index: 0,
        metadata: remote[0].metadata.clone(),
        source: Arc::new(source),
        path: remote[0].path.clone(),
    };
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../testdata/radar-tm35fin/radar_tm35_20260406T0640Z.tif"
    );
    let local_file = with(DataSource::from_path(Path::new(fixture)));
    let in_memory = with(DataSource::from_bytes(std::fs::read(fixture).unwrap()));
    let DataSource::Remote { tile_info, .. } = &*remote[0].source else {
        panic!("a remote frame");
    };
    let stac_asset = with(DataSource::http(
        "https://stac.example/asset.tif".into(),
        Arc::new(reqwest::Client::new()),
        tile_info.clone(),
    ));

    assert_eq!(frames::position_frame_reads(&remote), REMOTE_FRAME_READS);
    assert_eq!(frames::position_frame_reads(&[local_file]), 1);
    assert_eq!(frames::position_frame_reads(&[stac_asset]), 1);
    let mut mixed = frames::frames(&entries);
    mixed.push(in_memory);
    assert_eq!(frames::position_frame_reads(&mixed), 1);
}
