//! A cold `CELLS` map viewport on a remote radar volume source (#1155).
//!
//! Its own test binary: it admits renders through the process-wide render
//! slots at their default count, which nothing else here may resize. The
//! tests share those slots, so they run one at a time.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ds_core::error::DataServerError;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterTile};
use ds_executor::ExecutionError;
use ds_storage::object_store::memory::InMemory;
use ds_storage::object_store::path::Path as ObjectPath;
use ds_storage::object_store::throttle::{ThrottleConfig, ThrottledStore};
use ds_storage::object_store::{ObjectStoreExt, PutPayload};
use engine_odim::cells::CELLS_PARAMETER;
use engine_odim::PolarVolumeSiteView;

/// Two sweeps of `DBZH` and `VRAD` over 8 km, timed 2026-09-16 12:00 UTC.
const FIXTURE: &[u8] = include_bytes!("../../../testdata/pvol-cold-batch.h5");

/// A `CELLS` tile tracks six scans behind the one it draws.
const SCANS: usize = 7;

/// One test at a time on the shared render slots.
static SERIAL: Mutex<()> = Mutex::new(());

/// The fixture re-timestamped to 12:`minute`: its `/what/time` attribute is
/// the one `120000` in the file, a fixed-length string.
fn scan_at(minute: usize) -> Vec<u8> {
    let at = FIXTURE
        .windows(6)
        .position(|w| w == b"120000")
        .expect("the fixture's /what/time");
    let mut bytes = FIXTURE.to_vec();
    bytes[at..at + 6].copy_from_slice(format!("12{minute:02}00").as_bytes());
    bytes
}

/// Site `test` with [`SCANS`] volumes five minutes apart under `prefix` of a
/// store whose every GET first waits `delay`, scanned as a remote source.
/// Each test takes a prefix of its own: the cell-set cache is process-wide.
fn remote_site(prefix: &str, delay: Duration) -> (Arc<PolarVolumeSiteView>, ds_storage::DataStore) {
    let inner = InMemory::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    for i in 0..SCANS {
        let minute = 5 * i;
        let key = ObjectPath::from(format!("{prefix}2026091612{minute:02}_test_PVOL.h5"));
        runtime
            .block_on(inner.put(&key, PutPayload::from(scan_at(minute))))
            .unwrap();
    }
    let config = ThrottleConfig {
        wait_get_per_call: delay,
        ..ThrottleConfig::default()
    };
    let store = ds_storage::DataStore::new(Arc::new(ThrottledStore::new(inner, config)));
    let engine = engine_odim::PolarVolumeEngine::new_remote_for_test(
        "cells-viewport",
        store.clone(),
        prefix,
    )
    .expect("scan the store");
    let view = Arc::new(engine.site_view("test", "cells-viewport-test"));
    (view, store)
}

/// A multi-thread runtime serving the renders, as the server's does.
fn serving_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
}

type Rendered = Result<Result<RasterTile, DataServerError>, ExecutionError>;

/// One map tile of `parameter` over `bbox`, admitted and run as the map
/// APIs run a cache miss: through the shared render slots and deadline.
async fn render(
    view: Arc<PolarVolumeSiteView>,
    bbox: [f64; 4],
    size: u32,
    parameter: &'static str,
) -> Rendered {
    let (job, memory) =
        ds_executor::RenderJob::acquire_raster(ds_executor::RENDER_SLOTS.clone(), size, size)
            .await?;
    job.run(move || {
        let _memory = memory;
        view.get_raster_tile(
            bbox,
            size,
            size,
            None,
            &OutputCrs::Wgs84,
            Some(parameter),
            None,
            None,
        )
    })
    .await
}

/// One z7-sized tile of the viewport: a 7 × 5 grid around the site.
fn tile_bbox(i: usize) -> [f64; 4] {
    let (col, row) = ((i % 7) as f64 - 3.0, (i / 7) as f64 - 2.0);
    let (lon, lat) = (24.5 + 2.8 * col, 60.3 + 1.4 * row);
    [lon, lat, lon + 2.8, lat + 1.4]
}

/// #1155: a cold `CELLS` viewport of 35 tiles finishes within the render
/// deadline at the default slot count. Every tile needs the cell sets of the
/// same seven cold volumes, each a whole-file GET from the store. The tiles
/// build them in parallel, one GET per volume, and wait for one another's
/// builds without a render slot; built in turn, the seven GETs alone outlast
/// the deadline and every tile is a 503.
///
/// A tile may be shed at admission only when the viewport outnumbers the
/// queue (fewer than twelve slots at the default queue of three per slot).
#[test]
fn cold_cells_viewport_finishes_within_the_render_deadline() {
    const TILES: usize = 35;
    const GET_DELAY: Duration = Duration::from_millis(500);
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (view, store) = remote_site("viewport/", GET_DELAY);
    let scanned = store.bytes_read();

    let started = Instant::now();
    let results: Vec<Rendered> = serving_runtime().block_on(async {
        let tiles: Vec<_> = (0..TILES)
            .map(|i| tokio::spawn(render(view.clone(), tile_bbox(i), 256, CELLS_PARAMETER)))
            .collect();
        let mut results = Vec::with_capacity(TILES);
        for tile in tiles {
            results.push(tile.await.unwrap());
        }
        results
    });
    let elapsed = started.elapsed();

    let slots = ds_executor::render_concurrency();
    let queue = ds_executor::metrics().capacity;
    let (mut served, mut shed, mut late) = (0, 0, 0);
    for (i, result) in results.into_iter().enumerate() {
        match result {
            Ok(Ok(tile)) => {
                assert_eq!((tile.width, tile.height), (256, 256));
                served += 1;
            }
            Err(ExecutionError::Busy) => shed += 1,
            Ok(Err(DataServerError::DeadlineExceeded)) | Err(ExecutionError::Timeout) => late += 1,
            Ok(Err(e)) => panic!("tile {i}: {e}"),
            Err(e) => panic!("tile {i}: {e}"),
        }
    }
    assert_eq!(
        late, 0,
        "{late} of {TILES} tiles outlasted the render deadline, {elapsed:?} for the \
         viewport with {slots} slots"
    );
    // A tile is shed only when it finds every queue place held, each by a
    // tile that is not shed, so at least `queue` tiles are not shed. The
    // slots add no guarantee: a running tile can hold a queue place and its
    // slot at once, as `wait_without_slot` takes the place before it drops
    // the slot and the slot again before it drops the place (#1208). Up to
    // one tile per slot more than `TILES - slots - queue` may then be shed.
    assert!(
        shed <= TILES.saturating_sub(queue),
        "{shed} tiles shed with {slots} slots and {queue} queue places"
    );
    assert_eq!(served + shed, TILES);
    assert!(
        elapsed < GET_DELAY * 3,
        "the viewport took {elapsed:?}, more than three GETs, with {slots} slots"
    );
    assert_eq!(
        store.bytes_read() - scanned,
        (SCANS * FIXTURE.len()) as u64,
        "one whole-file GET per volume"
    );
}

/// #1155: tiles of another layer render while a cold `CELLS` window builds.
/// Seven tiles build the window's volumes and hold seven slots; the other
/// `CELLS` tiles wait for those builds without one, so the rest of the
/// slots stay free. Twice as many `CELLS` tiles as slots fill every slot
/// when the waiting tiles keep theirs, and the `DBZH` tiles then wait for
/// the builds' GETs.
#[test]
fn other_layers_render_while_a_cold_cells_window_builds() {
    const GET_DELAY: Duration = Duration::from_millis(1000);
    const OTHER_TILES: usize = 4;
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (view, _store) = remote_site("other-layers/", GET_DELAY);
    let cells_tiles = 2 * ds_executor::render_concurrency();

    let (cells, others) = serving_runtime().block_on(async {
        let cells: Vec<_> = (0..cells_tiles)
            .map(|i| tokio::spawn(render(view.clone(), tile_bbox(i), 256, CELLS_PARAMETER)))
            .collect();
        // Every `CELLS` tile admitted, the window's GETs under way.
        tokio::time::sleep(GET_DELAY / 5).await;
        let others: Vec<_> = (0..OTHER_TILES)
            .map(|i| {
                let view = view.clone();
                tokio::spawn(async move {
                    let started = Instant::now();
                    let x = 24.45 + 0.05 * i as f64;
                    let tile = render(view, [x, 60.25, x + 0.05, 60.35], 64, "DBZH").await;
                    (tile, started.elapsed())
                })
            })
            .collect();
        let mut done = Vec::new();
        for tile in others {
            done.push(tile.await.unwrap());
        }
        let mut cells_done = Vec::new();
        for tile in cells {
            cells_done.push(tile.await.unwrap());
        }
        (cells_done, done)
    });

    for (i, (tile, elapsed)) in others.into_iter().enumerate() {
        let tile = tile
            .unwrap_or_else(|e| panic!("DBZH tile {i}: {e}"))
            .unwrap_or_else(|e| panic!("DBZH tile {i}: {e}"));
        assert!(!tile.is_empty(), "DBZH tile {i} samples the sweep");
        assert!(
            elapsed < GET_DELAY * 2 / 5,
            "DBZH tile {i} took {elapsed:?} behind a CELLS window building in {GET_DELAY:?} \
             GETs"
        );
    }
    for (i, tile) in cells.into_iter().enumerate() {
        tile.unwrap_or_else(|e| panic!("CELLS tile {i}: {e}"))
            .unwrap_or_else(|e| panic!("CELLS tile {i}: {e}"));
    }
}
