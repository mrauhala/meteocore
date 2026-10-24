//! Poll-cycle pre-warm of new remote COG frames (#1004), over the committed
//! radar fixtures read back by byte range through a filesystem object store.

use super::*;
use ds_core::map_engine::{MapEngine, OutputCrs};
use ds_storage::object_store::path::Path as ObjectPath;

use crate::cache::TileCache;
use crate::reader::{DataSource, RemoteTileInfo, TiffMetadata};

const FRAMES: [&str; 2] = ["radar_20260324T2315Z.tif", "radar_20260324T2320Z.tif"];

fn fixture_dir() -> PathBuf {
    ["testdata/radar", "../../testdata/radar"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_dir())
        .expect("testdata/radar fixture")
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "meteocore_{tag}_{}_{}",
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

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn config() -> GeoTiffConfig {
    GeoTiffConfig {
        filename_template: Some("radar_%Y%m%dT%H%MZ.tif".to_string()),
        filename_pattern: None,
        timestamp_format: None,
        parameter: "reflectivity".to_string(),
        unit: "dBZ".to_string(),
        poll_interval_secs: 3600,
        exclude_patterns: vec![],
        max_files: None,
        tile_cache_mb: 64,
        band: 1,
        nodata: None,
        scale: None,
        offset: None,
        stac_url: None,
        stac_asset_key: "data".to_string(),
        stac_asset_allowlist: None,
        endpoint: None,
        bucket: None,
        prefix_pattern: None,
        time_window: None,
        scan_days: None,
    }
}

/// A frame copied into the "bucket" directory `remote/d`.
fn publish(remote: &TempDir, frame: &str) {
    std::fs::create_dir_all(remote.0.join("d")).unwrap();
    std::fs::copy(fixture_dir().join(frame), remote.0.join("d").join(frame)).unwrap();
}

/// An engine whose catalog is the object store under `remote/d`, read by
/// byte range like a bucket. `new` builds a remote engine only for s3:// or
/// http(s):// URLs, so it starts on an empty local directory and is then
/// pointed at the store.
fn remote_engine(local: &TempDir, remote: &TempDir) -> GeoTiffEngine {
    let mut engine = GeoTiffEngine::new("prewarm", local.0.to_str(), &config()).unwrap();
    let (store, _) = ds_storage::build_store(remote.0.to_str().unwrap()).unwrap();
    engine.store_mode = StoreMode::Remote {
        store,
        prefix: ObjectPath::from("d"),
    };
    engine
}

/// Every non-empty tile of a frame, as `(ifd, layout generation, chunk)`,
/// all levels.
fn frame_tiles(metadata: &TiffMetadata, source: &DataSource) -> Vec<(u16, u64, u32)> {
    let DataSource::Remote { tile_info, .. } = source else {
        panic!("remote source expected");
    };
    let levels = std::iter::once((0, tile_info)).chain(
        metadata
            .overviews
            .iter()
            .map(|ov| (ov.ifd_index as u16, ov.tile_info.as_ref().unwrap())),
    );
    levels
        .flat_map(|(ifd, info): (u16, &RemoteTileInfo)| {
            info.tile_byte_counts
                .iter()
                .enumerate()
                .filter(|(_, &count)| count > 0)
                .map(move |(chunk, _)| (ifd, info.generation, chunk as u32))
        })
        .collect()
}

/// How many of the frame at `time`'s tiles the engine's tile cache holds,
/// out of how many.
fn cached_tiles(engine: &GeoTiffEngine, time: DateTime<Utc>) -> (usize, usize) {
    let catalog = engine.catalog.load();
    let entry = &catalog.entries[&time];
    let tiles = frame_tiles(entry.metadata().unwrap(), entry.source().unwrap());
    let cached = tiles
        .iter()
        .filter(|(ifd, generation, chunk)| {
            engine
                .tile_cache
                .contains_untracked(&entry.path, *generation, *chunk, *ifd)
        })
        .count();
    (cached, tiles.len())
}

fn frame_time(frame: &str) -> DateTime<Utc> {
    let stamp = &frame["radar_".len()..frame.len() - "Z.tif".len()];
    chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%dT%H%M")
        .unwrap()
        .and_utc()
}

/// One map view of the whole frame at `time`.
fn render(engine: &GeoTiffEngine, time: DateTime<Utc>) {
    let bbox = engine.catalog.load().spatial_extent.unwrap();
    engine
        .get_raster_tile(
            bbox,
            400,
            220,
            Some(time),
            &OutputCrs::Wgs84,
            None,
            None,
            None,
        )
        .unwrap();
}

/// #1004: the poll that discovers a frame reads its tiles into the cache, so
/// its first view does no storage I/O. A frame catalogued before that poll
/// is not one it discovered and is left alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_poll_warms_the_frame_it_discovers_and_not_catalogued_ones() {
    let (local, remote) = (
        TempDir::new("prewarm_local"),
        TempDir::new("prewarm_remote"),
    );
    publish(&remote, FRAMES[0]);
    let engine = remote_engine(&local, &remote);
    // Catalogued by a scan that warms nothing, like the startup scan.
    engine.poll_once();
    let (old, new) = (frame_time(FRAMES[0]), frame_time(FRAMES[1]));
    assert_eq!(cached_tiles(&engine, old).0, 0);

    publish(&remote, FRAMES[1]);
    engine.poll_cycle().await;
    let (cached, total) = cached_tiles(&engine, new);
    assert!(total > 0);
    assert_eq!(cached, total, "every level of the new frame is warm");
    assert_eq!(
        cached_tiles(&engine, old).0,
        0,
        "the catalogued frame is not"
    );

    let before = engine.storage_bytes_read();
    render(&engine, new);
    assert_eq!(
        engine.storage_bytes_read(),
        before,
        "the first view of the new frame reads nothing from storage"
    );
    render(&engine, old);
    assert!(
        engine.storage_bytes_read() > before,
        "the old frame is cold"
    );

    // A later poll that discovers nothing fetches nothing.
    let before = engine.storage_bytes_read();
    let previous = engine.catalog.load_full();
    engine.poll_once();
    assert!(engine.prewarm_new_frames(&previous).await.is_empty());
    assert_eq!(engine.storage_bytes_read(), before);
}

/// #1004: the poll loop warms what the startup scan catalogued before its
/// first sleep, newest frames first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_poll_loop_warms_the_startup_catalog_first() {
    let (local, remote) = (
        TempDir::new("prewarm_local"),
        TempDir::new("prewarm_remote"),
    );
    publish(&remote, FRAMES[0]);
    publish(&remote, FRAMES[1]);
    let engine = Arc::new(remote_engine(&local, &remote));
    engine.poll_once();
    let poller = Arc::clone(&engine);
    let task = tokio::spawn(async move { poller.poll_loop().await });
    let warm = |engine: &GeoTiffEngine| {
        FRAMES.iter().all(|frame| {
            let (cached, total) = cached_tiles(engine, frame_time(frame));
            cached == total
        })
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !warm(&engine) {
        assert!(
            std::time::Instant::now() < deadline,
            "startup frames warmed"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    engine.shutdown();
    task.await.unwrap();
}

/// The fixture as a remote object: `(store, source, metadata, cache path)`.
fn remote_frame() -> (
    ds_storage::DataStore,
    Arc<DataSource>,
    TiffMetadata,
    PathBuf,
) {
    let dir = fixture_dir();
    let (store, _) = ds_storage::build_store(dir.to_str().unwrap()).unwrap();
    let path = ObjectPath::from(FRAMES[0]);
    let size = std::fs::metadata(dir.join(FRAMES[0])).unwrap().len();
    let (metadata, tile_info) = TiffMetadata::from_header_read(&store, &path, size)
        .expect("the header range read succeeds")
        .expect("the fixture is a COG");
    let source = Arc::new(DataSource::remote(store.clone(), path, tile_info));
    (store, source, metadata, PathBuf::from(FRAMES[0]))
}

/// #1004: the per-frame cap keeps the coarsest levels, every warmed level is
/// read back exactly as a cold read with no storage I/O, and a frame whose
/// tiles are cached plans nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_cap_keeps_coarse_levels_and_warm_tiles_serve_reads_without_io() {
    let (store, source, metadata, path) = remote_frame();
    let cache = TileCache::new(64 * 1024 * 1024);
    let overview_bytes: usize = metadata
        .overviews
        .iter()
        .flat_map(|ov| &ov.tile_info.as_ref().unwrap().tile_byte_counts)
        .map(|&n| n as usize)
        .sum();
    let budget = Duration::from_secs(60);

    let plan = prewarm::plan_frame(&path, &metadata, &source, &cache, overview_bytes).unwrap();
    let outcome = prewarm::warm(&[plan], &cache, budget, || false).await;
    assert_eq!(
        outcome.capped_levels, 1,
        "the full resolution is over the cap"
    );
    assert_eq!(outcome.bytes, overview_bytes);
    assert_eq!(
        (outcome.failed, outcome.deferred, outcome.unfinished),
        (0, 0, 0)
    );
    assert!(
        outcome.reads < outcome.tiles,
        "contiguous tiles are read together"
    );

    let fetched = store.bytes_read();
    let warm_reads: Vec<_> = metadata
        .overviews
        .iter()
        .map(|ov| {
            reader::read_bbox_overview(
                &source,
                &metadata,
                ov,
                0,
                0,
                ov.width,
                ov.height,
                Some(&cache),
                &path,
                0,
            )
            .unwrap()
        })
        .collect();
    assert_eq!(
        store.bytes_read(),
        fetched,
        "warm overviews read no storage"
    );
    let DataSource::Remote { tile_info, .. } = source.as_ref() else {
        panic!("remote source expected");
    };
    assert!(!cache.contains_untracked(&path, tile_info.generation, 0, 0));
    for (ov, warm) in metadata.overviews.iter().zip(&warm_reads) {
        let cold = reader::read_bbox_overview(
            &source,
            &metadata,
            ov,
            0,
            0,
            ov.width,
            ov.height,
            Some(&TileCache::new(0)),
            &path,
            0,
        )
        .unwrap();
        assert_eq!(&cold, warm, "IFD {} reads back exactly", ov.ifd_index);
    }

    // Lifting the cap fetches the full resolution only.
    let plan = prewarm::plan_frame(&path, &metadata, &source, &cache, usize::MAX).unwrap();
    let outcome = prewarm::warm(&[plan], &cache, budget, || false).await;
    let DataSource::Remote { tile_info, .. } = source.as_ref() else {
        unreachable!()
    };
    let full: Vec<u64> = tile_info
        .tile_byte_counts
        .iter()
        .copied()
        .filter(|&n| n > 0)
        .collect();
    assert_eq!(outcome.tiles, full.len());
    assert_eq!(outcome.bytes as u64, full.iter().sum::<u64>());
    let fetched = store.bytes_read();
    let (cols, rows) = (
        (metadata.tile_width * 2).min(metadata.width),
        (metadata.tile_height * 2).min(metadata.height),
    );
    reader::read_bbox_map(&source, &metadata, 0, 0, cols, rows, Some(&cache), &path, 0).unwrap();
    assert_eq!(
        store.bytes_read(),
        fetched,
        "warm full resolution reads no storage"
    );

    assert!(
        prewarm::plan_frame(&path, &metadata, &source, &cache, usize::MAX)
            .unwrap()
            .is_empty()
    );
    let local = Arc::new(DataSource::from_path(&fixture_dir().join(FRAMES[0])));
    assert!(prewarm::plan_frame(&path, &metadata, &local, &cache, usize::MAX).is_none());
}

/// #1004: a stop request (engine shutdown) defers every read not yet
/// started, and a zero cap plans no level.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_defers_reads_and_a_zero_cap_plans_nothing() {
    let (store, source, metadata, path) = remote_frame();
    let cache = TileCache::new(64 * 1024 * 1024);
    let fetched = store.bytes_read();
    let plan = prewarm::plan_frame(&path, &metadata, &source, &cache, usize::MAX).unwrap();
    let outcome = prewarm::warm(&[plan], &cache, Duration::from_secs(60), || true).await;
    assert_eq!(outcome.reads, 0);
    assert!(outcome.deferred > 0);
    assert_eq!(store.bytes_read(), fetched);

    let plan = prewarm::plan_frame(&path, &metadata, &source, &cache, 0).unwrap();
    assert!(plan.is_empty());
}

/// The uncapped plan of the fixture frame and its read count, 0 once every
/// tile is cached.
fn full_plan(
    source: &Arc<DataSource>,
    metadata: &TiffMetadata,
    path: &Path,
    cache: &TileCache,
) -> (prewarm::FramePlan, usize) {
    let plan = prewarm::plan_frame(path, metadata, source, cache, usize::MAX).unwrap();
    let reads = plan.read_count();
    (plan, reads)
}

/// #1004 review: requests holding more than half the decode budget delay a
/// read instead of dropping it, so a burst of renders at poll time does not
/// leave the new frame cold.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_busy_decode_budget_delays_reads_instead_of_skipping_them() {
    let (_store, source, metadata, path) = remote_frame();
    let cache = TileCache::new(64 * 1024 * 1024);
    let (plan, planned) = full_plan(&source, &metadata, &path, &cache);
    assert!(planned > 0);
    let decode = Arc::new(decode_budget::Budget::new(64 * 1024 * 1024));
    let requests = decode.reserve(decode.background_limit() + 1).unwrap();
    let release = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(requests);
    });
    let outcome = prewarm::warm_with(
        &[plan],
        &cache,
        &decode,
        Duration::from_secs(30),
        Duration::from_secs(60),
        || false,
    )
    .await;
    release.await.unwrap();
    assert_eq!(outcome.reads, planned, "every read waited and landed");
    assert_eq!((outcome.busy, outcome.deferred), (0, 0));
    assert_eq!(
        full_plan(&source, &metadata, &path, &cache).1,
        0,
        "the frame is warm"
    );
}

/// #1004 review: a decode budget that stays saturated is reported once the
/// short busy wait runs out, not waited out for the whole time budget, and a
/// read that can never fit the background half is skipped at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_saturated_or_tiny_decode_budget_gives_up_without_stalling_the_poll() {
    let (store, source, metadata, path) = remote_frame();
    let cache = TileCache::new(64 * 1024 * 1024);
    let fetched = store.bytes_read();

    let (plan, planned) = full_plan(&source, &metadata, &path, &cache);
    assert!(planned > 0);
    let decode = Arc::new(decode_budget::Budget::new(64 * 1024 * 1024));
    let _requests = decode.reserve(decode.background_limit() + 1).unwrap();
    let started = std::time::Instant::now();
    let outcome = prewarm::warm_with(
        &[plan],
        &cache,
        &decode,
        Duration::from_millis(30),
        Duration::from_secs(60),
        || false,
    )
    .await;
    assert!(started.elapsed() < Duration::from_secs(20), "no 60 s stall");
    assert_eq!((outcome.reads, outcome.busy), (0, planned));

    let (plan, planned) = full_plan(&source, &metadata, &path, &cache);
    let tiny = Arc::new(decode_budget::Budget::new(2));
    let started = std::time::Instant::now();
    let outcome = prewarm::warm_with(
        &[plan],
        &cache,
        &tiny,
        Duration::from_secs(60),
        Duration::from_secs(60),
        || false,
    )
    .await;
    assert!(started.elapsed() < Duration::from_secs(20), "no busy wait");
    assert_eq!((outcome.reads, outcome.deferred), (0, planned));
    assert_eq!(store.bytes_read(), fetched, "nothing was fetched");
}

/// Cold versus pre-warmed first view of one frame, timed against a local
/// HTTP origin that delays every request by `MC_COG_FIRST_FRAME_LATENCY_MS`
/// (default 25). `MC_COG_FIRST_FRAME_FILE` is the COG to serve; the view is
/// a 2848 × 1405 EPSG:3857 viewport walked as the WMS meta-tile loop walks
/// it, one 256 px tile after another. See
/// `docs/performance/cog-first-frame.md`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a COG file; see docs/performance/cog-first-frame.md"]
async fn benchmark_first_view_over_latency() {
    use ds_core::web_mercator::{lon_to_x, x_to_lon, y_to_lat};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let file = PathBuf::from(std::env::var("MC_COG_FIRST_FRAME_FILE").unwrap());
    let latency = Duration::from_millis(
        std::env::var("MC_COG_FIRST_FRAME_LATENCY_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(25),
    );
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    let app = axum::Router::new()
        .route_service("/frame.tif", tower_http::services::ServeFile::new(&file))
        .layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let counter = Arc::clone(&counter);
                async move {
                    counter.fetch_add(1, Ordering::Relaxed);
                    tokio::time::sleep(latency).await;
                    next.run(request).await
                }
            },
        ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (store, _) = ds_storage::build_store(&base).unwrap();
    let size = std::fs::metadata(&file).unwrap().len();
    let time: DateTime<Utc> = "2026-10-09T15:30:00Z".parse().unwrap();
    let local = TempDir::new("prewarm_bench");

    // An engine serving the one frame from the origin.
    let engine = || {
        let mut engine = GeoTiffEngine::new("bench", local.0.to_str(), &config()).unwrap();
        let path = ObjectPath::from("frame.tif");
        let (metadata, tile_info) = TiffMetadata::from_header_read(&store, &path, size)
            .expect("the header range read succeeds")
            .expect("the fixture is a COG");
        let source = DataSource::remote(store.clone(), path, tile_info);
        let mut catalog = Catalog::empty();
        catalog.entries.insert(
            time,
            catalog::FileEntry::loaded(
                PathBuf::from("frame.tif"),
                source,
                metadata,
                size,
                None,
                None,
            ),
        );
        catalog.recompute_extents();
        engine.store_mode = StoreMode::Remote {
            store: store.clone(),
            prefix: ObjectPath::from(""),
        };
        engine.catalog.store(Arc::new(catalog));
        engine
    };
    // A map client's z7 view over northern Europe: the meta-tile loop's 256 px
    // tiles at the half-octave level it snaps to, in its row-major order.
    let view = |engine: &GeoTiffEngine| {
        let [west, south, east, north]: [f64; 4] =
            [811_446.90, 7_675_481.28, 4_294_529.40, 9_393_785.68];
        let (width, height): (f64, f64) = (2848.0, 1405.0);
        let origin = lon_to_x(180.0);
        let z0 = 2.0 * origin / 256.0;
        let res = ((east - west) / width).min((north - south) / height);
        let level = (2.0 * (z0 / res).log2() - 1e-6).ceil();
        let span = 256.0 * z0 / 2f64.powf(level / 2.0);
        let cols =
            ((west + origin) / span).floor() as i64..=((east + origin) / span).floor() as i64;
        let rows =
            ((origin - north) / span).floor() as i64..=((origin - south) / span).floor() as i64;
        let mut tiles = 0;
        for row in rows {
            for col in cols.clone() {
                let x0 = -origin + col as f64 * span;
                let y1 = origin - row as f64 * span;
                let bbox = [
                    x_to_lon(x0),
                    y_to_lat(y1 - span),
                    x_to_lon(x0 + span),
                    y_to_lat(y1),
                ];
                engine
                    .get_raster_tile(
                        bbox,
                        256,
                        256,
                        None,
                        &OutputCrs::WebMercator,
                        None,
                        None,
                        None,
                    )
                    .unwrap();
                tiles += 1;
            }
        }
        tiles
    };
    let timed = |label: &str, run: &dyn Fn() -> usize| {
        let before = requests.load(Ordering::Relaxed);
        let started = std::time::Instant::now();
        let tiles = run();
        eprintln!(
            "FIRST_FRAME {label}: {:.0} ms, {} origin requests, {tiles} meta-tiles, latency {} ms",
            started.elapsed().as_secs_f64() * 1000.0,
            requests.load(Ordering::Relaxed) - before,
            latency.as_millis()
        );
    };

    let cold = engine();
    timed("cold view", &|| view(&cold));
    timed("repeat view", &|| view(&cold));

    let warmed = engine();
    let before = requests.load(Ordering::Relaxed);
    let started = std::time::Instant::now();
    let frames = warmed.prewarm_new_frames(&Catalog::empty()).await;
    eprintln!(
        "FIRST_FRAME pre-warm: {:.0} ms, {} origin requests, {} frame(s)",
        started.elapsed().as_secs_f64() * 1000.0,
        requests.load(Ordering::Relaxed) - before,
        frames.len()
    );
    timed("pre-warmed first view", &|| view(&warmed));
}

// ---- Frames republished under a catalogued timestamp (#1055) ----

/// The frame at `time` rendered over `bbox`, as values.
fn view(engine: &GeoTiffEngine, bbox: [f64; 4], time: DateTime<Utc>) -> Vec<Option<f64>> {
    engine
        .get_raster_tile(
            bbox,
            400,
            220,
            Some(time),
            &OutputCrs::Wgs84,
            None,
            None,
            None,
        )
        .unwrap()
        .values
        .iter_values()
        .collect()
}

/// `bytes` published as `key` under `remote/d`, as a bucket.
fn put(remote: &TempDir, key: &str, bytes: &[u8]) {
    let path = remote.0.join("d").join(key);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn fixture(frame: &str) -> Vec<u8> {
    std::fs::read(fixture_dir().join(frame)).unwrap()
}

/// What a fresh engine renders over `bbox` for `bytes` published as
/// `FRAMES[0]`: the view a republished frame must give.
fn fresh_view(bytes: &[u8], bbox: [f64; 4]) -> Vec<Option<f64>> {
    let (local, remote) = (
        TempDir::new("republish_fresh_local"),
        TempDir::new("republish_fresh_remote"),
    );
    put(&remote, FRAMES[0], bytes);
    let engine = remote_engine(&local, &remote);
    engine.poll_once();
    view(&engine, bbox, frame_time(FRAMES[0]))
}

/// #1055: an object overwritten in place with a new size has its header
/// read again, and the poll warms it, so its first view reads no storage
/// and draws the new bytes, never the tiles cached from the old ones. The
/// version moves once, so the API layers' rendered and meta-tile caches no
/// longer serve the old image. A later poll that finds it unchanged does
/// neither again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_object_overwritten_in_place_is_warmed_and_renders_its_new_tiles() {
    let (local, remote) = (
        TempDir::new("republish_local"),
        TempDir::new("republish_remote"),
    );
    let (old, new) = (fixture(FRAMES[0]), fixture(FRAMES[1]));
    assert_ne!(old.len(), new.len());
    put(&remote, FRAMES[0], &old);
    let engine = remote_engine(&local, &remote);
    engine.poll_once();
    let time = frame_time(FRAMES[0]);
    let bbox = engine.catalog.load().spatial_extent.unwrap();
    let before = view(&engine, bbox, time);
    assert_eq!(engine.content_version(), 0);

    put(&remote, FRAMES[0], &new);
    engine.poll_cycle().await;
    let (cached, total) = cached_tiles(&engine, time);
    assert!(total > 0);
    assert_eq!(
        cached, total,
        "every level of the republished frame is warm"
    );
    assert_eq!(
        engine.content_version(),
        1,
        "renders cached from the old object are not served"
    );
    let read = engine.storage_bytes_read();
    let after = view(&engine, bbox, time);
    assert_eq!(
        engine.storage_bytes_read(),
        read,
        "the first view reads nothing from storage"
    );
    assert_ne!(after, before);
    assert_eq!(after, fresh_view(&new, bbox), "the new object's pixels");

    let previous = engine.catalog.load_full();
    engine.poll_once();
    assert!(engine.prewarm_new_frames(&previous).await.is_empty());
    assert_eq!(
        engine.content_version(),
        1,
        "an unchanged object moves nothing"
    );
}

/// #1055: an object overwritten in place with bytes of the same size is a
/// new object too: its listing names another `Last-Modified` and ETag, so
/// the scan reads its header again instead of reusing the old layout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_same_size_overwrite_is_read_again() {
    let (local, remote) = (
        TempDir::new("republish_same_size_local"),
        TempDir::new("republish_same_size_remote"),
    );
    let (old, mut new) = (fixture(FRAMES[1]), fixture(FRAMES[0]));
    // Bytes past the last tile are never read: the same frame, padded to
    // the size of the object it replaces.
    assert!(new.len() < old.len());
    new.resize(old.len(), 0);
    put(&remote, FRAMES[0], &old);
    let engine = remote_engine(&local, &remote);
    engine.poll_once();
    let time = frame_time(FRAMES[0]);
    let bbox = engine.catalog.load().spatial_extent.unwrap();
    let before = view(&engine, bbox, time);

    put(&remote, FRAMES[0], &new);
    engine.poll_cycle().await;
    assert_eq!(engine.content_version(), 1);
    let after = view(&engine, bbox, time);
    assert_ne!(after, before);
    assert_eq!(after, fresh_view(&new, bbox), "the new object's pixels");
}

/// #1055: a new object under another key that wins a catalogued timestamp
/// (the scan keeps the greatest key per timestamp) is warmed like a new
/// frame and moves the version.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_key_for_a_catalogued_timestamp_is_warmed() {
    let (local, remote) = (
        TempDir::new("republish_key_local"),
        TempDir::new("republish_key_remote"),
    );
    let (old, new) = (fixture(FRAMES[0]), fixture(FRAMES[1]));
    put(&remote, FRAMES[0], &old);
    let engine = remote_engine(&local, &remote);
    engine.poll_once();
    let time = frame_time(FRAMES[0]);
    let bbox = engine.catalog.load().spatial_extent.unwrap();
    let before = view(&engine, bbox, time);

    let key = format!("v2/{}", FRAMES[0]);
    put(&remote, &key, &new);
    engine.poll_cycle().await;
    assert_eq!(
        engine.catalog.load().entries[&time].path,
        PathBuf::from(format!("d/{key}"))
    );
    let (cached, total) = cached_tiles(&engine, time);
    assert_eq!(cached, total, "the new object is warm");
    assert_eq!(engine.content_version(), 1);
    let after = view(&engine, bbox, time);
    assert_ne!(after, before);
    assert_eq!(after, fresh_view(&new, bbox), "the new object's pixels");
}

/// #1055: a local file replaced under its name, by the atomic rename the
/// engine asks publishers for, moves the version too: the rendered and
/// meta-tile caches would otherwise keep the old file's image. A poll that
/// finds it unchanged does not.
#[test]
fn a_local_file_replaced_under_its_name_moves_the_version() {
    let dir = TempDir::new("republish_local_file");
    let path = dir.0.join(FRAMES[0]);
    std::fs::write(&path, fixture(FRAMES[0])).unwrap();
    let engine = GeoTiffEngine::new("republish", dir.0.to_str(), &config()).unwrap();
    engine.poll_once();
    assert_eq!(engine.content_version(), 0);

    let staged = dir.0.join("staged.tmp");
    std::fs::write(&staged, fixture(FRAMES[1])).unwrap();
    std::fs::rename(&staged, &path).unwrap();
    engine.poll_once();
    assert_eq!(engine.content_version(), 1);
    engine.poll_once();
    assert_eq!(engine.content_version(), 1);
}
