//! Poll-cycle STAC metadata preload (#90) against a mock STAC API whose
//! assets are the committed TM35FIN COG fixture, served with Range support.

use super::*;
use ds_core::map_engine::{MapEngine, OutputCrs};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../testdata/radar-tm35fin/radar_tm35_20260406T0640Z.tif"
);

/// Serve a STAC collection whose `/items` lists `count` items five minutes
/// apart, each with its own asset URL (the single-flight key) pointing at the
/// fixture. Filters are ignored: every poll sees every item. Returns the
/// items URL, the asset allowlist and the item times, oldest first.
async fn mock_stac(count: usize) -> (String, String, Vec<DateTime<Utc>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let t0: DateTime<Utc> = "2026-04-06T04:00:00Z".parse().unwrap();
    let times: Vec<DateTime<Utc>> = (0..count)
        .map(|i| t0 + chrono::Duration::minutes(5 * i as i64))
        .collect();
    let features: Vec<serde_json::Value> = times
        .iter()
        .enumerate()
        .map(|(i, t)| {
            serde_json::json!({
                "type": "Feature",
                "id": format!("item-{i}"),
                "properties": { "datetime": t.to_rfc3339() },
                // No `file:size`: the loader HEADs the asset first.
                "assets": { "data": { "href": format!("{base}/data/item-{i}.tif") } },
            })
        })
        .collect();
    let items = serde_json::json!({
        "type": "FeatureCollection",
        "features": features,
        "links": [],
    });
    let collection = serde_json::json!({
        "extent": {
            "spatial": { "bbox": [[19.0, 59.0, 32.0, 70.5]] },
            "temporal": { "interval": [[t0.to_rfc3339(), null]] },
        },
    });
    let app = axum::Router::new()
        .route(
            "/collections/radar",
            axum::routing::get(move || async move { axum::Json(collection) }),
        )
        .route(
            "/collections/radar/items",
            axum::routing::get(move || async move { axum::Json(items) }),
        )
        .route_service(
            "/data/{name}",
            tower_http::services::ServeFile::new(FIXTURE),
        );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (
        format!("{base}/collections/radar/items"),
        format!("{base}/data/"),
        times,
    )
}

fn stac_engine(items_url: String, allowlist: String) -> GeoTiffEngine {
    stac_engine_with_cache(items_url, allowlist, 0)
}

/// A STAC engine whose tile cache holds `tile_cache_mb`; 0 also turns the
/// poll-time tile pre-warm (#1004) off.
fn stac_engine_with_cache(
    items_url: String,
    allowlist: String,
    tile_cache_mb: u64,
) -> GeoTiffEngine {
    let config = GeoTiffConfig {
        filename_template: None,
        filename_pattern: None,
        timestamp_format: None,
        parameter: "reflectivity".to_string(),
        unit: "dBZ".to_string(),
        poll_interval_secs: 3600,
        exclude_patterns: vec![],
        max_files: None,
        tile_cache_mb,
        band: 1,
        nodata: None,
        scale: None,
        offset: None,
        stac_url: Some(items_url),
        stac_asset_key: "data".to_string(),
        stac_asset_allowlist: Some(vec![allowlist]),
        endpoint: None,
        bucket: None,
        prefix_pattern: None,
        time_window: None,
        scan_days: None,
    };
    GeoTiffEngine::new("stac-preload", None, &config).expect("STAC engine builds")
}

fn loaded_times(engine: &GeoTiffEngine) -> Vec<DateTime<Utc>> {
    let catalog = engine.catalog.load();
    catalog
        .entries
        .iter()
        .filter(|(_, entry)| entry.is_loaded())
        .map(|(ts, _)| *ts)
        .collect()
}

/// The poll loop is spawned on the multi-thread poll runtime, so its future
/// (now awaiting the preload) must stay `Send`.
#[allow(dead_code)]
fn poll_loop_future_is_send(engine: &GeoTiffEngine) {
    fn assert_send<T: Send>(_: T) {}
    assert_send(engine.poll_loop());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn poll_preloads_new_stac_items_before_any_request() {
    let (items_url, allowlist, times) = mock_stac(3).await;
    let engine = stac_engine(items_url, allowlist);
    // Startup reads the collection extent only: no items, placeholder CRS.
    assert!(engine.catalog.load().entries.is_empty());
    assert_eq!(engine.raster_info().native_crs, "CRS:84");

    engine.poll_cycle().await;

    assert_eq!(
        loaded_times(&engine),
        times,
        "every discovered item is loaded by the poll, before any request"
    );
    // The snapshot reflects the loaded metadata with no render (#322).
    let info = engine.raster_info();
    assert_eq!(info.native_crs, "EPSG:3067");
    assert!(info.grid_size.is_some());
    // A second poll discovers nothing new and has nothing to preload.
    engine.poll_cycle().await;
    assert_eq!(loaded_times(&engine), times);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preload_is_capped_to_newest_and_lazy_path_loads_the_rest() {
    let (items_url, allowlist, times) = mock_stac(STAC_PRELOAD_MAX_ITEMS + 2).await;
    let engine = Arc::new(stac_engine(items_url, allowlist));

    engine.poll_cycle().await;

    // A discovered backlog preloads only its newest STAC_PRELOAD_MAX_ITEMS.
    assert_eq!(engine.catalog.load().entries.len(), times.len());
    assert_eq!(loaded_times(&engine), times[2..].to_vec());

    // A request for an item the poll skipped still loads it lazily, on a
    // blocking worker like the render executor's.
    let oldest = times[0];
    let render = Arc::clone(&engine);
    let tile = tokio::task::spawn_blocking(move || {
        render.get_raster_tile(
            [20.0, 60.0, 30.0, 70.0],
            16,
            16,
            Some(oldest),
            &OutputCrs::Wgs84,
            None,
            None,
            None,
        )
    })
    .await
    .unwrap()
    .expect("request-path fallback renders the skipped item");
    assert_eq!((tile.width, tile.height), (16, 16));
    assert!(engine.is_metadata_loaded(&oldest));
    assert!(!engine.is_metadata_loaded(&times[1]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preload_skips_an_item_a_request_is_loading() {
    let (items_url, allowlist, times) = mock_stac(2).await;
    let engine = stac_engine(items_url, allowlist.clone());
    // A request holds the newest item's load claim when the poll runs.
    let claimed = PathBuf::from(format!("{allowlist}item-1.tif"));
    let request = engine
        .loading_in_flight
        .try_claim(&claimed, || false)
        .expect("unclaimed");

    engine.poll_cycle().await;

    // The preload neither parks on that claim nor fetches the item again.
    assert_eq!(loaded_times(&engine), vec![times[0]]);
    drop(request);
}

/// #1004: the poll pre-warms the tiles of the STAC items it preloads, read
/// through the direct HTTP source, so their first render misses nothing in
/// the tile cache.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn poll_prewarms_the_tiles_of_new_stac_items() {
    let (items_url, allowlist, times) = mock_stac(2).await;
    let engine = Arc::new(stac_engine_with_cache(items_url, allowlist, 64));

    engine.poll_cycle().await;

    assert_eq!(loaded_times(&engine), times);
    for time in &times {
        let catalog = engine.catalog.load();
        let entry = &catalog.entries[time];
        let (metadata, source) = (entry.metadata().unwrap(), entry.source().unwrap());
        let reader::DataSource::HttpDirect { tile_info, .. } = source.as_ref() else {
            panic!("a STAC item reads over direct HTTP");
        };
        let levels = std::iter::once((0u16, tile_info)).chain(
            metadata
                .overviews
                .iter()
                .filter_map(|ov| Some((ov.ifd_index as u16, ov.tile_info.as_ref()?))),
        );
        let mut tiles = 0;
        for (ifd, info) in levels {
            for (chunk, _) in info
                .tile_byte_counts
                .iter()
                .enumerate()
                .filter(|(_, &count)| count > 0)
            {
                tiles += 1;
                assert!(
                    engine
                        .tile_cache
                        .contains_untracked(&entry.path, chunk as u32, ifd),
                    "{time}: IFD {ifd} tile {chunk} warm"
                );
            }
        }
        assert!(tiles > 0);
    }

    let (hits, misses) = engine.tile_cache_stats();
    let newest = *times.last().unwrap();
    let render = Arc::clone(&engine);
    tokio::task::spawn_blocking(move || {
        render.get_raster_tile(
            [20.0, 60.0, 30.0, 70.0],
            64,
            64,
            Some(newest),
            &OutputCrs::Wgs84,
            None,
            None,
            None,
        )
    })
    .await
    .unwrap()
    .expect("renders the pre-warmed item");
    let after = engine.tile_cache_stats();
    assert!(after.0 > hits, "the render read tiles");
    assert_eq!(
        after.1, misses,
        "the first render finds every tile it reads in the cache"
    );
}
