//! STAC metadata loading against a mock STAC API whose assets are the
//! committed TM35FIN COG fixture, served with Range support: the poll-cycle
//! preload (#90) and a query's bounded, concurrent cold loads (#889).

use super::*;
use ds_core::map_engine::{MapEngine, OutputCrs};
use std::sync::atomic::AtomicUsize;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../testdata/radar-tm35fin/radar_tm35_20260406T0640Z.tif"
);

/// Serve a STAC collection whose `/items` lists `count` items five minutes
/// apart, each with its own asset URL (the single-flight key) pointing at the
/// fixture. Filters are ignored: every poll sees every item. Returns the
/// items URL, the asset allowlist and the item times, oldest first.
async fn mock_stac(count: usize) -> (String, String, Vec<DateTime<Utc>>) {
    let mock = mock_stac_with(count, MockOptions::default()).await;
    (mock.items_url, mock.allowlist, mock.times)
}

/// How a [`mock_stac_with`] host departs from [`mock_stac`]'s.
#[derive(Default)]
struct MockOptions {
    /// Hold each asset request this long before answering, so concurrent
    /// loads overlap there.
    delay: Duration,
    /// List only the items in the request's `datetime` interval, ends
    /// included, as a STAC API does.
    filter_datetime: bool,
    /// Answer this item's asset requests with 404.
    missing: Option<usize>,
}

/// The asset requests a mock STAC host served, and the most it had in
/// flight at once.
#[derive(Default)]
struct AssetLoad {
    requests: AtomicUsize,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
}

struct MockStac {
    items_url: String,
    allowlist: String,
    times: Vec<DateTime<Utc>>,
    assets: Arc<AssetLoad>,
}

/// [`mock_stac`] shaped by `options`, whose asset host counts its requests
/// and the most it had in flight at once.
async fn mock_stac_with(count: usize, options: MockOptions) -> MockStac {
    let MockOptions {
        delay,
        filter_datetime,
        missing,
    } = options;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let t0: DateTime<Utc> = "2026-04-06T04:00:00Z".parse().unwrap();
    let times: Vec<DateTime<Utc>> = (0..count)
        .map(|i| t0 + chrono::Duration::minutes(5 * i as i64))
        .collect();
    let features: Arc<Vec<(DateTime<Utc>, serde_json::Value)>> = Arc::new(
        times
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let feature = serde_json::json!({
                    "type": "Feature",
                    "id": format!("item-{i}"),
                    "properties": { "datetime": t.to_rfc3339() },
                    // No `file:size`: the loader HEADs the asset first.
                    "assets": { "data": { "href": format!("{base}/data/item-{i}.tif") } },
                });
                (*t, feature)
            })
            .collect(),
    );
    let items = move |axum::extract::Query(query): axum::extract::Query<
        std::collections::HashMap<String, String>,
    >| {
        let features = Arc::clone(&features);
        async move {
            let window = query
                .get("datetime")
                .filter(|_| filter_datetime)
                .and_then(|interval| interval.split_once('/'))
                .map(|(start, end)| {
                    let parse = |t: &str| t.parse::<DateTime<Utc>>().expect("RFC 3339");
                    (parse(start), parse(end))
                });
            let features: Vec<&serde_json::Value> = features
                .iter()
                .filter(|(t, _)| window.is_none_or(|(start, end)| (start..=end).contains(t)))
                .map(|(_, feature)| feature)
                .collect();
            axum::Json(serde_json::json!({
                "type": "FeatureCollection",
                "features": features,
                "links": [],
            }))
        }
    };
    let missing = missing.map(|i| format!("/data/item-{i}.tif"));
    let collection = serde_json::json!({
        "extent": {
            "spatial": { "bbox": [[19.0, 59.0, 32.0, 70.5]] },
            "temporal": { "interval": [[t0.to_rfc3339(), null]] },
        },
    });
    let assets = Arc::new(AssetLoad::default());
    let load = Arc::clone(&assets);
    let data = axum::Router::new()
        .route_service(
            "/data/{name}",
            tower_http::services::ServeFile::new(FIXTURE),
        )
        .layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let load = Arc::clone(&load);
                let missing = missing.clone();
                async move {
                    load.requests.fetch_add(1, Ordering::SeqCst);
                    let now = load.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    load.max_in_flight.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(delay).await;
                    let response = if missing.as_deref() == Some(request.uri().path()) {
                        axum::response::IntoResponse::into_response(
                            axum::http::StatusCode::NOT_FOUND,
                        )
                    } else {
                        next.run(request).await
                    };
                    load.in_flight.fetch_sub(1, Ordering::SeqCst);
                    response
                }
            },
        ));
    let app = axum::Router::new()
        .route(
            "/collections/radar",
            axum::routing::get(move || async move { axum::Json(collection) }),
        )
        .route("/collections/radar/items", axum::routing::get(items))
        .merge(data);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    MockStac {
        items_url: format!("{base}/collections/radar/items"),
        allowlist: format!("{base}/data/"),
        times,
        assets,
    }
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

/// A point inside the fixture's TM35FIN extent.
const POINT: &str = "POINT(25 65)";

/// Run an EDR position query the way the EDR executor does: synchronously
/// on an async worker of a multi-thread runtime.
async fn query_position(
    engine: &Arc<GeoTiffEngine>,
    window: (DateTime<Utc>, DateTime<Utc>),
) -> Result<CoverageResponse, DataServerError> {
    query_position_at(engine, Some(window), None).await
}

/// [`query_position`] with an optional `datetime` and the request deadline
/// the EDR executor installs on the worker thread.
async fn query_position_at(
    engine: &Arc<GeoTiffEngine>,
    datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
    deadline: Option<std::time::Instant>,
) -> Result<CoverageResponse, DataServerError> {
    let engine = Arc::clone(engine);
    tokio::spawn(async move {
        let _deadline = ds_core::deadline::enter(deadline);
        engine.query_position(POINT, datetime, None, None, None)
    })
    .await
    .unwrap()
}

/// The timesteps of a position query's answer.
fn series_times(response: CoverageResponse) -> Vec<DateTime<Utc>> {
    let CoverageResponse::Single(series) = response else {
        panic!("expected one coverage");
    };
    let DomainDescription::PointSeries { t, .. } = series.domain else {
        panic!("expected a PointSeries, got {:?}", series.domain);
    };
    t
}

fn asset_requests(mock: &MockStac) -> usize {
    mock.assets.requests.load(Ordering::SeqCst)
}

/// #889: a query over cold STAC items loads their metadata concurrently,
/// at most `STAC_PRELOAD_CONCURRENCY` in flight, never one item after
/// another on the request worker.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn query_loads_cold_stac_items_concurrently() {
    let options = MockOptions {
        delay: Duration::from_millis(100),
        ..MockOptions::default()
    };
    let mock = mock_stac_with(8, options).await;
    let times = mock.times.clone();
    let engine = Arc::new(stac_engine(mock.items_url.clone(), mock.allowlist.clone()));

    let response = query_position(&engine, (times[0], times[7])).await.unwrap();
    assert_eq!(series_times(response), times);
    assert_eq!(loaded_times(&engine), times);
    let most = mock.assets.max_in_flight.load(Ordering::SeqCst);
    assert!(
        (2..=STAC_PRELOAD_CONCURRENCY).contains(&most),
        "{most} asset requests in flight at most"
    );
}

/// #889: the cold-item cap is checked on the STAC response's items before
/// they are installed, as a 400 naming the count and the cap: a refused
/// query leaves no stubs behind and loads nothing. A narrower window then
/// loads, and a query without `datetime` over what it loaded is answered
/// instead of counting the refused window's items.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cold_items_over_the_cap_are_refused_before_they_are_installed() {
    let count = STAC_REQUEST_MAX_COLD_ITEMS + 4;
    let options = MockOptions {
        filter_datetime: true,
        ..MockOptions::default()
    };
    let mock = mock_stac_with(count, options).await;
    let times = mock.times.clone();
    let engine = Arc::new(stac_engine(mock.items_url.clone(), mock.allowlist.clone()));

    match query_position(&engine, (times[0], times[count - 1])).await {
        Err(DataServerError::QueryTooLarge(msg)) => {
            assert!(msg.contains(&format!("{count} STAC items")), "{msg}");
            let cap = format!("at most {STAC_REQUEST_MAX_COLD_ITEMS}");
            assert!(msg.contains(&cap), "names the cap: {msg}");
            assert!(msg.ends_with("narrow the datetime window"), "{msg}");
        }
        Err(e) => panic!("expected QueryTooLarge, got {e:?}"),
        Ok(_) => panic!("expected QueryTooLarge, got a coverage"),
    }
    assert!(
        engine.catalog.load().entries.is_empty(),
        "the refused items are not installed"
    );
    assert_eq!(asset_requests(&mock), 0);

    let last_three = times[count - 3..].to_vec();
    query_position(&engine, (times[count - 3], times[count - 1]))
        .await
        .expect("three cold items are within the cap");
    assert_eq!(loaded_times(&engine), last_three);

    let response = query_position_at(&engine, None, None)
        .await
        .expect("a query without datetime counts only the catalog's stubs");
    assert_eq!(series_times(response), last_three);
}

/// #889: a closed one-day window of five-minute frames, 289 items, is
/// within the cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_closed_day_of_five_minute_frames_is_within_the_cap() {
    let count = 24 * 12 + 1;
    let mock = mock_stac_with(count, MockOptions::default()).await;
    let times = mock.times.clone();
    let day = (times[0], times[count - 1]);
    assert_eq!(day.1 - day.0, chrono::Duration::hours(24));
    let engine = Arc::new(stac_engine(mock.items_url.clone(), mock.allowlist.clone()));

    let response = query_position(&engine, day)
        .await
        .expect("a day of five-minute frames loads");
    assert_eq!(series_times(response), times);
    assert_eq!(loaded_times(&engine), times);
}

/// #889: a query without `datetime` counts every stub in the catalog, here
/// a poll backlog past the preload cap. Over the cap it is the 400 telling
/// the client to add a datetime window, and loads nothing; a window over
/// part of the backlog loads.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_query_without_datetime_over_a_stub_backlog_asks_for_a_window() {
    let count = STAC_PRELOAD_MAX_ITEMS + STAC_REQUEST_MAX_COLD_ITEMS + 1;
    let mock = mock_stac_with(count, MockOptions::default()).await;
    let times = mock.times.clone();
    let engine = Arc::new(stac_engine(mock.items_url.clone(), mock.allowlist.clone()));
    engine.poll_cycle().await;
    let stubs = count - STAC_PRELOAD_MAX_ITEMS;
    assert_eq!(loaded_times(&engine), times[stubs..].to_vec());
    let after_poll = asset_requests(&mock);

    match query_position_at(&engine, None, None).await {
        Err(DataServerError::QueryTooLarge(msg)) => {
            assert!(msg.contains(&format!("{stubs} STAC items")), "{msg}");
            assert!(msg.ends_with("add a datetime window"), "{msg}");
        }
        Err(e) => panic!("expected QueryTooLarge, got {e:?}"),
        Ok(_) => panic!("expected QueryTooLarge, got a coverage"),
    }
    assert_eq!(asset_requests(&mock), after_poll, "nothing loaded");

    query_position(&engine, (times[0], times[2]))
        .await
        .expect("a window over three stubs loads");
    assert!(engine.is_metadata_loaded(&times[0]));
}

#[test]
fn cold_stac_budget_allows_the_cap() {
    assert!(check_cold_stac_budget(STAC_REQUEST_MAX_COLD_ITEMS, true).is_ok());
    for (windowed, remedy) in [
        (true, "narrow the datetime window"),
        (false, "add a datetime window"),
    ] {
        match check_cold_stac_budget(STAC_REQUEST_MAX_COLD_ITEMS + 1, windowed) {
            Err(DataServerError::QueryTooLarge(msg)) => assert!(msg.ends_with(remedy), "{msg}"),
            other => panic!("expected QueryTooLarge, got {other:?}"),
        }
    }
}

/// #889: the first failed cold load fails the query, and the loads still
/// in flight are dropped with their claims released, so a later query can
/// claim and load those items.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_cold_load_fails_the_query_and_releases_every_claim() {
    let options = MockOptions {
        missing: Some(2),
        ..MockOptions::default()
    };
    let mock = mock_stac_with(6, options).await;
    let times = mock.times.clone();
    let engine = Arc::new(stac_engine(mock.items_url.clone(), mock.allowlist.clone()));

    let err = query_position(&engine, (times[0], times[5]))
        .await
        .expect_err("an item whose asset is missing fails the query");
    assert!(matches!(err, DataServerError::Engine(_)), "{err:?}");
    assert!(!engine.is_metadata_loaded(&times[2]));
    assert!(
        engine.loading_in_flight.paths.lock().unwrap().is_empty(),
        "no claim outlives the query"
    );

    for window in [(times[3], times[5]), (times[0], times[1])] {
        query_position(&engine, window)
            .await
            .expect("a window without the missing item loads");
    }
    let mut loaded = times.clone();
    loaded.remove(2);
    assert_eq!(loaded_times(&engine), loaded);
}

/// #889 review: no cold load starts once the request deadline has passed.
/// The query is the 504 without an asset request; the items stay stubs and
/// a later query loads them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_cold_load_starts_past_the_request_deadline() {
    let mock = mock_stac_with(3, MockOptions::default()).await;
    let times = mock.times.clone();
    let engine = Arc::new(stac_engine(mock.items_url.clone(), mock.allowlist.clone()));
    let window = Some((times[0], times[2]));

    let err = query_position_at(&engine, window, Some(std::time::Instant::now()))
        .await
        .expect_err("past the deadline");
    assert!(matches!(err, DataServerError::DeadlineExceeded), "{err:?}");
    assert_eq!(asset_requests(&mock), 0);
    assert_eq!(engine.catalog.load().entries.len(), 3);
    assert!(loaded_times(&engine).is_empty());

    query_position_at(&engine, window, None)
        .await
        .expect("a query within its deadline loads them");
    assert_eq!(loaded_times(&engine), times);
}

/// #889: an item another loader holds does not stall the query's other
/// loads. The query loads the rest first, then waits for that loader with
/// no claim of its own, and loads the item itself once the claim is
/// released unfinished.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn query_loads_the_rest_while_another_loader_holds_an_item() {
    let mock = mock_stac_with(3, MockOptions::default()).await;
    let times = mock.times.clone();
    let claimed = PathBuf::from(format!("{}item-2.tif", mock.allowlist));
    let engine = Arc::new(stac_engine(mock.items_url, mock.allowlist));
    let elsewhere = engine
        .loading_in_flight
        .try_claim(&claimed, || false)
        .expect("unclaimed");

    let query = tokio::spawn({
        let engine = Arc::clone(&engine);
        let window = (times[0], times[2]);
        async move { query_position(&engine, window).await }
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while loaded_times(&engine) != times[..2] {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the unclaimed items load while one is held elsewhere: {:?}",
            loaded_times(&engine)
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!query.is_finished(), "the query waits for the held item");

    drop(elsewhere);
    query
        .await
        .unwrap()
        .expect("the query loads the released item");
    assert_eq!(loaded_times(&engine), times);
}
