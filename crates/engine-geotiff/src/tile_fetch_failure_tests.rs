//! A remote COG tile whose fetch still fails after its retries fails the
//! read (#1051). The map render, EDR position and EDR area all answer
//! `ResourceExhausted` (HTTP 503) instead of a nodata gap with HTTP 200, so
//! neither the no-TTL rendered and meta-tile caches nor a browser keep a
//! transparent hole, and the engine's own caches hold nothing for the frame.
//! Once the store answers again, the same requests read the data. Bytes
//! that arrive but do not decode stay nulls in EDR, read once.

use std::sync::atomic::Ordering;

use ds_core::map_engine::{MapEngine, OutputCrs, RasterTile};
use ds_storage::object_store::path::Path as ObjectPath;
use ds_storage::object_store::{ObjectStoreExt, PutPayload};
use ds_storage::DataStore;

use super::*;
use crate::reader::{DataSource, TiffCompression, TILE_FETCH_RETRIES};
use crate::remote_header_tests::{FlakyStore, FIXTURE};

/// Range reads one tile makes when every fetch fails: the first attempt and
/// each retry.
const FAILED_TILE_READS: usize = 1 + TILE_FETCH_RETRIES as usize;

/// More failures than any test makes reads: every range read fails.
const EVERY_READ: usize = usize::MAX / 2;

/// An empty local directory for the engine to start on, removed on drop.
struct EmptyDir(PathBuf);

impl EmptyDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "meteocore_tile_fetch_failure_{}_{}",
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

/// An engine reading the fixture by byte range from `flaky` under the prefix
/// `d`. `new` builds a remote engine only for s3:// or http(s):// URLs, so it
/// starts on an empty local directory and is then pointed at the store,
/// with the tile cache a remote source gets; the poll's header read
/// catalogues the frame.
fn remote_engine(local: &EmptyDir, flaky: &Arc<FlakyStore>) -> GeoTiffEngine {
    let config: GeoTiffConfig = serde_json::from_value(serde_json::json!({
        "filename_template": "radar_%Y%m%dT%H%MZ.tif",
        "parameter": "reflectivity",
        "unit": "dBZ",
    }))
    .unwrap();
    let mut engine = GeoTiffEngine::new("tile-fetch-failure", local.0.to_str(), &config).unwrap();
    engine.store_mode = StoreMode::Remote {
        store: DataStore::new(flaky.clone()),
        prefix: ObjectPath::from("d"),
    };
    engine.tile_cache = cache::TileCache::new(config.tile_cache_mb * 1024 * 1024);
    engine.poll_once();
    assert_eq!(
        engine.catalog.load().entries.len(),
        1,
        "the header read catalogues the frame"
    );
    engine
}

/// The whole frame as one map view.
fn render(engine: &GeoTiffEngine) -> Result<RasterTile, DataServerError> {
    let bbox = engine.catalog.load().spatial_extent.unwrap();
    engine.get_raster_tile(bbox, 400, 220, None, &OutputCrs::Wgs84, None, None, None)
}

/// The `(lon, lat)` of full-resolution pixel `(col, row)`.
fn pixel(engine: &GeoTiffEngine, col: u32, row: u32) -> (f64, f64) {
    let catalog = engine.catalog.load();
    let entry = catalog.entries.values().next().unwrap();
    entry
        .metadata()
        .unwrap()
        .geo_transform
        .pixel_to_world(col, row)
}

/// A `(lon, lat)` inside the frame, in its first full-resolution tile.
fn inside(engine: &GeoTiffEngine) -> (f64, f64) {
    pixel(engine, 50, 50)
}

/// How many of the frame's tiles, every level, the engine's compressed tile
/// cache or the shared decoded-chunk cache holds.
fn cached_tiles(engine: &GeoTiffEngine) -> usize {
    let catalog = engine.catalog.load();
    let entry = catalog.entries.values().next().unwrap();
    let metadata = entry.metadata().unwrap();
    let DataSource::Remote { tile_info, .. } = &**entry.source().unwrap() else {
        panic!("a remote source");
    };
    let levels = std::iter::once((0, tile_info)).chain(
        metadata
            .overviews
            .iter()
            .map(|ov| (ov.ifd_index as u16, ov.tile_info.as_ref().unwrap())),
    );
    levels
        .flat_map(|(ifd, info)| {
            let scope = crate::decoded_cache::FileScope::remote(
                entry.path.to_string_lossy().as_ref(),
                engine.tile_cache.namespace(),
                info.generation,
                engine.band_index,
            );
            (0..info.tile_byte_counts.len() as u32)
                .map(move |c| (ifd, info.generation, scope.clone(), c))
        })
        .filter(|(ifd, generation, scope, chunk)| {
            engine
                .tile_cache
                .contains_untracked(&entry.path, *generation, *chunk, *ifd)
                || crate::decoded_cache::contains(scope, *ifd, *chunk)
        })
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_tile_fetch_fails_the_render_and_edr_and_a_retry_reads() {
    let local = EmptyDir::new();
    let flaky = FlakyStore::with_object(std::fs::read(FIXTURE).unwrap());
    let engine = remote_engine(&local, &flaky);
    let (lon, lat) = inside(&engine);

    flaky.fail_ranges.store(EVERY_READ, Ordering::SeqCst);
    assert!(
        matches!(render(&engine), Err(DataServerError::ResourceExhausted)),
        "the render fails instead of returning a tile with a gap"
    );
    // Position and this area each read one full-resolution tile, which no
    // failed render cached: every range read is that tile's first attempt or
    // one of its retries.
    let reads = flaky.counts().0;
    assert!(
        matches!(
            engine.query_point(lat, lon, None, None),
            Err(DataServerError::ResourceExhausted)
        ),
        "EDR position is a 503, not a null with 200"
    );
    assert_eq!(
        flaky.counts().0 - reads,
        FAILED_TILE_READS,
        "position fetches its tile, then retries it before failing"
    );
    let reads = flaky.counts().0;
    assert!(
        matches!(
            engine.query_bbox(lon, lat, lon + 0.01, lat + 0.01, None, None),
            Err(DataServerError::ResourceExhausted)
        ),
        "EDR area is a 503, not a timestep of nulls with 200"
    );
    assert_eq!(
        flaky.counts().0 - reads,
        FAILED_TILE_READS,
        "the area fetches its one tile, then retries it before failing"
    );
    assert_eq!(cached_tiles(&engine), 0, "no cache holds the failed tiles");

    // The store answers again: the same requests read the data.
    flaky.fail_ranges.store(0, Ordering::SeqCst);
    let tile = render(&engine).expect("a retry renders");
    assert!(!tile.values.is_all_nodata(), "the fixture's echoes render");
    assert!(cached_tiles(&engine) > 0);
    engine
        .query_point(lat, lon, None, None)
        .expect("a retry reads the position");
    engine
        .query_bbox(lon, lat, lon + 0.01, lat + 0.01, None, None)
        .expect("a retry reads the area");
}

/// The decode half of #1051, by the maintainer's decision: remote tile bytes
/// that arrive but do not decode are nulls in EDR, with `Ok`, not an error.
/// The tile is attempted once, since a retry would decode the same bytes. A
/// retry would show as a compressed-cache hit (the fetched bytes are cached
/// before they decode), or as a second range read without that cache.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undecodable_tile_bytes_are_edr_nulls_read_once() {
    let local = EmptyDir::new();
    let fixture = std::fs::read(FIXTURE).unwrap();
    let flaky = FlakyStore::with_object(fixture.clone());
    let engine = remote_engine(&local, &flaky);

    // The scan has parsed the header. Overwrite the bytes of every
    // full-resolution tile in the stored object with bytes that are no zlib
    // stream, so every tile range read returns bytes that do not decode.
    {
        let catalog = engine.catalog.load();
        let entry = catalog.entries.values().next().unwrap();
        let DataSource::Remote {
            path, tile_info, ..
        } = &**entry.source().unwrap()
        else {
            panic!("a remote source");
        };
        assert_eq!(tile_info.compression, TiffCompression::Deflate);
        let mut corrupt = fixture;
        for (&offset, &count) in tile_info
            .tile_offsets
            .iter()
            .zip(&tile_info.tile_byte_counts)
        {
            corrupt[offset as usize..(offset + count) as usize].fill(0xFF);
        }
        flaky.put(path, PutPayload::from(corrupt)).await.unwrap();
    }

    // Position, in the first tile.
    let (lon, lat) = inside(&engine);
    let (reads, (hits, misses)) = (flaky.counts().0, engine.tile_cache.stats());
    let point = engine
        .query_point(lat, lon, None, None)
        .expect("an undecodable tile is no error");
    assert_eq!(point.ranges["reflectivity"].values, [None]);
    assert_eq!(
        flaky.counts().0 - reads,
        1,
        "position fetches its tile once"
    );
    assert_eq!(
        engine.tile_cache.stats(),
        (hits, misses + 1),
        "position attempts its tile once, without retries"
    );

    // Area, inside the second tile of the first row, which no query read.
    let (lon, lat) = pixel(&engine, 700, 200);
    let (reads, (hits, misses)) = (flaky.counts().0, engine.tile_cache.stats());
    let area = engine
        .query_bbox(lon - 0.03, lat - 0.03, lon + 0.03, lat + 0.03, None, None)
        .expect("an undecodable tile is no error");
    let values = &area.ranges["reflectivity"].values;
    assert!(!values.is_empty());
    assert!(
        values.iter().all(Option::is_none),
        "the tile's cells are nulls"
    );
    assert_eq!(
        flaky.counts().0 - reads,
        1,
        "the area fetches its tile once"
    );
    assert_eq!(
        engine.tile_cache.stats(),
        (hits, misses + 1),
        "the area attempts its tile once, without retries"
    );
}
