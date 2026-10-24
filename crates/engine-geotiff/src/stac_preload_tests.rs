//! STAC metadata loading against a mock STAC API whose assets are the
//! committed TM35FIN COG fixture, served with Range support: the poll-cycle
//! preload (#90), a query's bounded, concurrent cold loads (#889), the
//! budgets and listing cap checked before them (#1075), an item whose asset
//! is gone (#1119) and a header read that fails (#1025).

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
    /// Answer this item's asset requests with 503.
    failing: Option<usize>,
    /// Serve `/items` in pages of the request's `limit`, each linking the
    /// next, as a STAC API does.
    paged: bool,
    /// Give each item the fixture's `file:size`, so a load reads the header
    /// range first instead of a HEAD.
    sized: bool,
    /// How the asset host answers range requests at first
    /// ([`MockStac::range_fault`]).
    range_fault: Option<RangeFault>,
}

/// How a mock asset host answers range requests, while it serves whole
/// GETs as before.
#[derive(Debug, Clone, Copy)]
enum RangeFault {
    /// 503.
    Unavailable,
    /// A 206 whose body stops after 1 KiB, short of its `Content-Length`.
    CutShort,
    /// 403.
    Forbidden,
    /// The whole object, 200, as an origin that ignores `Range` answers.
    Ignored,
}

/// The asset requests a mock STAC host served, and the most it had in
/// flight at once.
#[derive(Default)]
struct AssetLoad {
    requests: AtomicUsize,
    /// GETs without a `Range` header: whole-object downloads.
    full_gets: AtomicUsize,
    /// The requests per asset path.
    paths: std::sync::Mutex<std::collections::HashMap<String, usize>>,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
}

struct MockStac {
    items_url: String,
    allowlist: String,
    times: Vec<DateTime<Utc>>,
    assets: Arc<AssetLoad>,
    /// The `/items` pages it served.
    pages: Arc<AtomicUsize>,
    /// How the asset host answers range requests now; `None` serves them.
    range_fault: Arc<std::sync::Mutex<Option<RangeFault>>>,
}

/// The answer of a mock asset host whose range requests meet `fault`, or
/// `None` to serve the request as it is.
fn faulty_range_answer(
    fault: RangeFault,
    request: &mut axum::extract::Request,
) -> Option<axum::response::Response> {
    use axum::http::{header, StatusCode};
    use axum::response::IntoResponse;
    match fault {
        RangeFault::Unavailable => Some(StatusCode::SERVICE_UNAVAILABLE.into_response()),
        RangeFault::Forbidden => Some(StatusCode::FORBIDDEN.into_response()),
        RangeFault::CutShort => {
            let size = std::fs::metadata(FIXTURE).unwrap().len();
            let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = vec![
                Ok(bytes::Bytes::from(vec![0u8; 1024])),
                Err(std::io::Error::other("connection cut")),
            ];
            let response = axum::http::Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_LENGTH, size)
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes 0-{}/{size}", size - 1),
                )
                .body(axum::body::Body::from_stream(futures::stream::iter(chunks)))
                .unwrap();
            Some(response)
        }
        RangeFault::Ignored => {
            request.headers_mut().remove(header::RANGE);
            None
        }
    }
}

/// [`mock_stac`] shaped by `options`, whose asset host counts its requests
/// and the most it had in flight at once.
async fn mock_stac_with(count: usize, options: MockOptions) -> MockStac {
    let MockOptions {
        delay,
        filter_datetime,
        missing,
        failing,
        paged,
        sized,
        range_fault,
    } = options;
    let fixture_size = std::fs::metadata(FIXTURE).unwrap().len();
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
                let mut feature = serde_json::json!({
                    "type": "Feature",
                    "id": format!("item-{i}"),
                    "properties": { "datetime": t.to_rfc3339() },
                    // No `file:size`: the loader HEADs the asset first.
                    "assets": { "data": { "href": format!("{base}/data/item-{i}.tif") } },
                });
                if sized {
                    feature["assets"]["data"]["file:size"] = fixture_size.into();
                }
                (*t, feature)
            })
            .collect(),
    );
    let pages = Arc::new(AtomicUsize::new(0));
    let served = Arc::clone(&pages);
    let items_url = format!("{base}/collections/radar/items");
    let page_base = items_url.clone();
    let items = move |axum::extract::Query(query): axum::extract::Query<
        std::collections::HashMap<String, String>,
    >| {
        let features = Arc::clone(&features);
        served.fetch_add(1, Ordering::SeqCst);
        let page_base = page_base.clone();
        async move {
            let window = query
                .get("datetime")
                .filter(|_| filter_datetime)
                .and_then(|interval| interval.split_once('/'))
                .map(|(start, end)| {
                    let parse = |t: &str| t.parse::<DateTime<Utc>>().expect("RFC 3339");
                    (parse(start), parse(end))
                });
            let mut features: Vec<&serde_json::Value> = features
                .iter()
                .filter(|(t, _)| window.is_none_or(|(start, end)| (start..=end).contains(t)))
                .map(|(_, feature)| feature)
                .collect();
            let mut links = Vec::new();
            if paged {
                let number = |key: &str| query.get(key).map(|n| n.parse::<usize>().unwrap());
                let limit = number("limit").expect("the client sends a limit");
                let offset = number("offset").unwrap_or(0);
                let next = offset + limit;
                if next < features.len() {
                    let mut params =
                        vec![("limit", limit.to_string()), ("offset", next.to_string())];
                    params.extend(query.get("datetime").map(|dt| ("datetime", dt.clone())));
                    let href = url::Url::parse_with_params(&page_base, &params).unwrap();
                    links.push(serde_json::json!({ "rel": "next", "href": href.as_str() }));
                }
                features = features.into_iter().skip(offset).take(limit).collect();
            }
            axum::Json(serde_json::json!({
                "type": "FeatureCollection",
                "features": features,
                "links": links,
            }))
        }
    };
    let missing = missing.map(|i| format!("/data/item-{i}.tif"));
    let failing = failing.map(|i| format!("/data/item-{i}.tif"));
    let range_fault = Arc::new(std::sync::Mutex::new(range_fault));
    let faults = Arc::clone(&range_fault);
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
            move |mut request: axum::extract::Request, next: axum::middleware::Next| {
                let load = Arc::clone(&load);
                let missing = missing.clone();
                let failing = failing.clone();
                let ranged = request.headers().contains_key(axum::http::header::RANGE);
                let fault = (*faults.lock().unwrap()).filter(|_| ranged);
                async move {
                    load.requests.fetch_add(1, Ordering::SeqCst);
                    *load
                        .paths
                        .lock()
                        .unwrap()
                        .entry(request.uri().path().to_string())
                        .or_default() += 1;
                    if request.method() == axum::http::Method::GET && !ranged {
                        load.full_gets.fetch_add(1, Ordering::SeqCst);
                    }
                    let now = load.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    load.max_in_flight.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(delay).await;
                    let path = Some(request.uri().path());
                    let response = if missing.as_deref() == path {
                        axum::response::IntoResponse::into_response(
                            axum::http::StatusCode::NOT_FOUND,
                        )
                    } else if failing.as_deref() == path {
                        axum::response::IntoResponse::into_response(
                            axum::http::StatusCode::SERVICE_UNAVAILABLE,
                        )
                    } else if let Some(answer) =
                        fault.and_then(|fault| faulty_range_answer(fault, &mut request))
                    {
                        answer
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
        items_url,
        allowlist: format!("{base}/data/"),
        times,
        assets,
        pages,
        range_fault,
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
    stac_engine_with(items_url, allowlist, tile_cache_mb, None)
}

/// [`stac_engine_with_cache`] whose catalog keeps at most `max_files`.
fn stac_engine_with(
    items_url: String,
    allowlist: String,
    tile_cache_mb: u64,
    max_files: Option<usize>,
) -> GeoTiffEngine {
    let config = GeoTiffConfig {
        filename_template: None,
        filename_pattern: None,
        timestamp_format: None,
        parameter: "reflectivity".to_string(),
        unit: "dBZ".to_string(),
        poll_interval_secs: 3600,
        exclude_patterns: vec![],
        max_files,
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
                    engine.tile_cache.contains_untracked(
                        &entry.path,
                        info.generation,
                        chunk as u32,
                        ifd
                    ),
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

fn full_gets(mock: &MockStac) -> usize {
    mock.assets.full_gets.load(Ordering::SeqCst)
}

/// The asset requests for item `i` so far.
fn item_requests(mock: &MockStac, i: usize) -> usize {
    let paths = mock.assets.paths.lock().unwrap();
    paths
        .get(&format!("/data/item-{i}.tif"))
        .copied()
        .unwrap_or(0)
}

/// The source the catalog entry at `time` reads its tiles from.
fn source_at(engine: &GeoTiffEngine, time: DateTime<Utc>) -> Arc<reader::DataSource> {
    let catalog = engine.catalog.load();
    Arc::clone(catalog.entries[&time].source().expect("loaded"))
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
/// claim and load those items. A server error fails the load with the 503
/// a tile read gives, without a whole download (#1025); an asset that is
/// gone does not fail it (#1119, below).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_cold_load_fails_the_query_and_releases_every_claim() {
    let options = MockOptions {
        failing: Some(2),
        ..MockOptions::default()
    };
    let mock = mock_stac_with(6, options).await;
    let times = mock.times.clone();
    let engine = Arc::new(stac_engine(mock.items_url.clone(), mock.allowlist.clone()));

    let err = query_position(&engine, (times[0], times[5]))
        .await
        .expect_err("an item whose asset host fails fails the query");
    assert!(matches!(err, DataServerError::ResourceExhausted), "{err:?}");
    assert!(!engine.is_metadata_loaded(&times[2]));
    assert_eq!(full_gets(&mock), 0, "no whole download after a failed HEAD");
    assert!(
        engine.loading_in_flight.paths.lock().unwrap().is_empty(),
        "no claim outlives the query"
    );

    for window in [(times[3], times[5]), (times[0], times[1])] {
        query_position(&engine, window)
            .await
            .expect("a window without the failing item loads");
    }
    let mut loaded = times.clone();
    loaded.remove(2);
    assert_eq!(loaded_times(&engine), loaded);
}

/// #1119: an item whose asset answers 404 to its first read is read once,
/// then dropped. The query answers without it, and no later query or poll
/// reads the asset again while the listing names the same href and size:
/// the item's own window finds nothing, twice, the second time through a
/// fresh stub of the same listing. With `file:size` the first read is the
/// header range read; without, it is the HEAD.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_item_whose_asset_is_gone_is_read_once_then_dropped() {
    for sized in [true, false] {
        let options = MockOptions {
            missing: Some(2),
            sized,
            ..MockOptions::default()
        };
        let mock = mock_stac_with(6, options).await;
        let times = mock.times.clone();
        let engine = Arc::new(stac_engine(mock.items_url.clone(), mock.allowlist.clone()));
        let window = (times[0], times[5]);
        let mut live = times.clone();
        live.remove(2);

        let response = query_position(&engine, window)
            .await
            .expect("an item whose asset is gone fails no query");
        assert_eq!(series_times(response), live, "sized {sized}");
        assert_eq!(item_requests(&mock, 2), 1, "sized {sized}: read once");
        assert_eq!(engine.raster_info().times, live, "sized {sized}");
        assert_eq!(engine.resolve_time(Some(times[2]), None), Some(times[1]));

        let response = query_position(&engine, window).await.unwrap();
        assert_eq!(series_times(response), live, "sized {sized}");
        for _ in 0..2 {
            let err = query_position(&engine, (times[2], times[2]))
                .await
                .expect_err("the item's own window holds nothing");
            assert!(
                matches!(err, DataServerError::LocationNotFound(_)),
                "sized {sized}: {err:?}"
            );
        }
        engine.poll_cycle().await;
        assert_eq!(
            engine
                .catalog
                .load()
                .entries
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            live,
            "sized {sized}: the listing that names it adds nothing"
        );
        assert_eq!(
            item_requests(&mock, 2),
            1,
            "sized {sized}: never read again"
        );
        assert_eq!(full_gets(&mock), 0, "sized {sized}");
    }
}

/// #1025: a header read that fails, a 503, a body cut short or a 403, fails
/// this load without downloading the asset whole: the query is the 503 a
/// tile read gives for the same failures (`ResourceExhausted`, #1051,
/// #1074), the item stays a stub, and no whole-object GET reaches the host.
/// Once the reads succeed, the next query loads the item by range.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_header_read_fails_the_load_without_a_full_download() {
    for fault in [
        RangeFault::Unavailable,
        RangeFault::CutShort,
        RangeFault::Forbidden,
    ] {
        let options = MockOptions {
            sized: true,
            range_fault: Some(fault),
            ..MockOptions::default()
        };
        let mock = mock_stac_with(1, options).await;
        let time = mock.times[0];
        let engine = Arc::new(stac_engine(mock.items_url.clone(), mock.allowlist.clone()));

        let err = query_position(&engine, (time, time))
            .await
            .expect_err("the load fails");
        assert!(
            matches!(err, DataServerError::ResourceExhausted),
            "{fault:?}: {err:?}"
        );
        assert_eq!(asset_requests(&mock), 1, "{fault:?}: the header read only");
        assert_eq!(full_gets(&mock), 0, "{fault:?}: no whole download");
        assert!(
            !engine.is_metadata_loaded(&time),
            "{fault:?}: a stub, not gone"
        );

        *mock.range_fault.lock().unwrap() = None;
        let response = query_position(&engine, (time, time))
            .await
            .expect("the next load reads the header");
        assert_eq!(series_times(response), vec![time]);
        assert_eq!(full_gets(&mock), 0, "{fault:?}");
        assert!(
            matches!(
                *source_at(&engine, time),
                reader::DataSource::HttpDirect { .. }
            ),
            "{fault:?}: read by range"
        );
    }
}

/// #1025: an origin that ignores `Range` still takes the whole download, as
/// before: only that, or a header that does not parse, can serve the item.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_origin_ignoring_range_still_takes_the_full_download() {
    let options = MockOptions {
        sized: true,
        range_fault: Some(RangeFault::Ignored),
        ..MockOptions::default()
    };
    let mock = mock_stac_with(1, options).await;
    let time = mock.times[0];
    let engine = Arc::new(stac_engine(mock.items_url.clone(), mock.allowlist.clone()));

    let response = query_position(&engine, (time, time))
        .await
        .expect("the whole download serves it");
    assert_eq!(series_times(response), vec![time]);
    assert_eq!(full_gets(&mock), 1);
    assert!(matches!(
        *source_at(&engine, time),
        reader::DataSource::InMemory(_)
    ));
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

/// A rectangle over the whole fixture grid, 480 × 360 cells: six timesteps
/// of it are over `MAX_AREA_VALUES`, five are within.
const WHOLE_GRID: &str = "POLYGON((0 50, 50 50, 50 80, 0 80, 0 50))";
const WHOLE_GRID_CELLS: usize = 480 * 360;

/// Run an EDR area query the way the EDR executor does, like
/// [`query_position`].
async fn query_area(
    engine: &Arc<GeoTiffEngine>,
    coords: &str,
    window: (DateTime<Utc>, DateTime<Utc>),
) -> Result<CoverageResponse, DataServerError> {
    let engine = Arc::clone(engine);
    let coords = coords.to_string();
    tokio::spawn(async move { engine.query_area(&coords, Some(window), None, None, None) })
        .await
        .unwrap()
}

/// The timesteps of an area query's answer.
fn grid_times(response: CoverageResponse) -> Vec<DateTime<Utc>> {
    let CoverageResponse::Single(grid) = response else {
        panic!("expected one coverage");
    };
    let DomainDescription::Grid { t, .. } = grid.domain else {
        panic!("expected a Grid, got {:?}", grid.domain);
    };
    t.expect("several timesteps")
}

fn expect_too_large(result: Result<CoverageResponse, DataServerError>, needle: &str) -> String {
    match result {
        Err(DataServerError::QueryTooLarge(msg)) => {
            assert!(msg.contains(needle), "{msg}");
            msg
        }
        Err(e) => panic!("expected QueryTooLarge, got {e:?}"),
        Ok(_) => panic!("expected QueryTooLarge, got a coverage"),
    }
}

/// #1075: the area budget runs on the window's grid before the window's
/// cold items load. With every item cold, only the first loads, the one
/// whose grid the answer uses; once it is loaded, the same query refuses
/// without fetching any item metadata. A window within the budget loads
/// and answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_over_budget_area_query_loads_at_most_its_first_item() {
    let options = MockOptions {
        filter_datetime: true,
        ..MockOptions::default()
    };
    let mock = mock_stac_with(6, options).await;
    let times = mock.times.clone();
    let engine = Arc::new(stac_engine(mock.items_url.clone(), mock.allowlist.clone()));
    let window = (times[0], times[5]);
    assert!(ds_core::feature::check_area_budget(6, 360, 480, 1).is_err());

    let refused = query_area(&engine, WHOLE_GRID, window).await;
    let msg = expect_too_large(refused, "Area query would return");
    assert!(msg.contains("6 timesteps × 360 × 480 cells"), "{msg}");
    assert_eq!(
        loaded_times(&engine),
        vec![times[0]],
        "only the first item, whose grid the budget needs"
    );

    let after_first = asset_requests(&mock);
    let refused = query_area(&engine, WHOLE_GRID, window).await;
    expect_too_large(refused, "Area query would return");
    assert_eq!(
        asset_requests(&mock),
        after_first,
        "no item metadata fetched"
    );
    assert_eq!(loaded_times(&engine), vec![times[0]]);

    let response = query_area(&engine, WHOLE_GRID, (times[0], times[4]))
        .await
        .expect("five timesteps of the grid are within the budget");
    assert_eq!(grid_times(response), times[..5].to_vec());
}

/// A polygon of `vertices` points on an ellipse around the fixture's grid,
/// its bbox covering all of it.
fn ring_over_the_grid(vertices: usize) -> String {
    let points: Vec<String> = (0..=vertices)
        .map(|i| {
            let angle = std::f64::consts::TAU * (i % vertices) as f64 / vertices as f64;
            format!(
                "{} {}",
                25.0 + 20.0 * angle.cos(),
                64.0 + 12.0 * angle.sin()
            )
        })
        .collect();
    format!("POLYGON(({}))", points.join(", "))
}

/// #1075: the polygon mask budget runs before the window's cold items load
/// as well. With the first item loaded, a polygon whose mask is over the
/// budget fetches no item metadata.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_over_budget_polygon_mask_fetches_no_item_metadata() {
    // Unfiltered: the first query's listing installs every item as a stub.
    let mock = mock_stac_with(3, MockOptions::default()).await;
    let times = mock.times.clone();
    let engine = Arc::new(stac_engine(mock.items_url.clone(), mock.allowlist.clone()));
    query_position(&engine, (times[0], times[0])).await.unwrap();
    assert_eq!(loaded_times(&engine), vec![times[0]]);
    assert_eq!(cold_stac_items(&engine.catalog.load(), None).len(), 2);
    let polygon = ring_over_the_grid(200);
    // Within the area budget, over the mask budget.
    assert!(ds_core::feature::check_area_budget(3, 360, 480, 1).is_ok());
    let ring = ds_core::feature::parse_area_coords(&polygon).unwrap();
    assert!(ds_core::feature::check_mask_budget(WHOLE_GRID_CELLS, &ring).is_err());

    let before = asset_requests(&mock);
    let refused = query_area(&engine, &polygon, (times[0], times[2])).await;
    expect_too_large(refused, "edge tests");
    assert_eq!(asset_requests(&mock), before, "no item metadata fetched");
    assert_eq!(loaded_times(&engine), vec![times[0]]);
}

/// #1075: the grid of a loaded entry outside the window only estimates the
/// window's. Here it is finer than the window's own, so its estimate is
/// over the budget while the window's grid is within: the query loads its
/// first item, checks that grid and answers. Never a refusal the checks
/// after the loads would not make.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_grid_estimate_never_refuses_what_the_window_grid_admits() {
    let options = MockOptions {
        filter_datetime: true,
        ..MockOptions::default()
    };
    let mock = mock_stac_with(6, options).await;
    let times = mock.times.clone();
    let engine = Arc::new(stac_engine(mock.items_url.clone(), mock.allowlist.clone()));
    query_position(&engine, (times[5], times[5])).await.unwrap();
    // The newest item now claims a grid of twice the resolution.
    engine.catalog.rcu(|current| {
        let mut next = (**current).clone();
        let entry = next.entries.get_mut(&times[5]).unwrap();
        let mut finer = (**entry.metadata().unwrap()).clone();
        let gt = &mut finer.geo_transform;
        (gt.pixel_width, gt.pixel_height) = (gt.pixel_width / 2.0, gt.pixel_height / 2.0);
        (gt.width, gt.height) = (gt.width * 2, gt.height * 2);
        let source = Arc::clone(entry.source().unwrap());
        entry.set_loaded(Arc::new(finer), source);
        next
    });
    assert!(ds_core::feature::check_area_budget(2, 720, 960, 1).is_err());
    assert!(ds_core::feature::check_area_budget(2, 360, 480, 1).is_ok());

    let response = query_area(&engine, WHOLE_GRID, (times[0], times[1]))
        .await
        .expect("the window's own grid is within the budget");
    assert_eq!(grid_times(response), times[..2].to_vec());
}

/// #1075: an on-demand listing over a long window stops paging once the
/// window holds more than `STAC_REQUEST_MAX_COLD_ITEMS` items. The 400
/// gives the count so far as a lower bound, and nothing is installed or
/// loaded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_long_on_demand_listing_stops_paging_past_the_cap() {
    let count = 3 * STAC_REQUEST_MAX_COLD_ITEMS;
    let options = MockOptions {
        filter_datetime: true,
        paged: true,
        ..MockOptions::default()
    };
    let mock = mock_stac_with(count, options).await;
    let times = mock.times.clone();
    let engine = Arc::new(stac_engine(mock.items_url.clone(), mock.allowlist.clone()));
    let page = stac::STAC_PAGE_LIMIT as usize;
    // The first full page that takes the count past the cap.
    let pages = STAC_REQUEST_MAX_COLD_ITEMS / page + 1;

    let refused = query_position(&engine, (times[0], times[count - 1])).await;
    let msg = expect_too_large(refused, &format!("at least {} STAC items", pages * page));
    assert!(msg.ends_with("narrow the datetime window"), "{msg}");
    assert_eq!(mock.pages.load(Ordering::SeqCst), pages);
    assert!(engine.catalog.load().entries.is_empty());
    assert_eq!(asset_requests(&mock), 0);
}

/// #1075: a `max_files` trim can drop window items after the listing, so
/// such a catalog lists the window in full and counts after the trim, as
/// before: a long window trimmed to its newest items is answered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_max_files_catalog_lists_the_window_in_full() {
    let count = 6 * stac::STAC_PAGE_LIMIT as usize;
    let options = MockOptions {
        filter_datetime: true,
        paged: true,
        ..MockOptions::default()
    };
    let mock = mock_stac_with(count, options).await;
    let times = mock.times.clone();
    let engine = Arc::new(stac_engine_with(
        mock.items_url.clone(),
        mock.allowlist.clone(),
        0,
        Some(10),
    ));

    let response = query_position(&engine, (times[0], times[count - 1]))
        .await
        .expect("the trimmed window is within the cap");
    assert_eq!(series_times(response), times[count - 10..].to_vec());
    assert_eq!(mock.pages.load(Ordering::SeqCst), 6);
}

/// #1055: a STAC timestep whose item a scan finds republished under it, with
/// another asset href, while the old item's load finished during that scan,
/// stays a stub. It loads afresh, renders the new asset's tiles, and moves
/// `content_version`. Carrying the old item's loaded header over would draw
/// the replaced asset.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stac_item_republished_under_its_timestep_loads_afresh() {
    let radar = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/radar");
    let (old_file, new_file) = (
        radar.join("radar_20260324T2315Z.tif"),
        radar.join("radar_20260324T2320Z.tif"),
    );
    let app = axum::Router::new()
        .route_service(
            "/data/old.tif",
            tower_http::services::ServeFile::new(&old_file),
        )
        .route_service(
            "/data/new.tif",
            tower_http::services::ServeFile::new(&new_file),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let engine = || {
        Arc::new(stac_engine_with_cache(
            format!("{base}/collections/radar/items"),
            format!("{base}/data/"),
            64,
        ))
    };
    let stub = |name: &str, file: &Path| {
        let href = format!("{base}/data/{name}");
        catalog::FileEntry::stac_stub(
            PathBuf::from(&href),
            std::fs::metadata(file).unwrap().len(),
            catalog::StacStub {
                bbox: None,
                asset_url: href,
            },
        )
    };
    let time: DateTime<Utc> = "2026-03-24T23:15:00Z".parse().unwrap();
    let with = |entry| {
        let mut catalog = Catalog::empty();
        catalog.entries.insert(time, entry);
        Arc::new(catalog)
    };
    let view = |engine: &Arc<GeoTiffEngine>, bbox| {
        let engine = Arc::clone(engine);
        tokio::task::spawn_blocking(move || -> Vec<Option<f64>> {
            engine
                .get_raster_tile(
                    bbox,
                    64,
                    64,
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
        })
    };

    let republished = engine();
    republished.catalog.store(with(stub("old.tif", &old_file)));
    republished.preload_stac_metadata(&Catalog::empty()).await;
    assert!(republished.is_metadata_loaded(&time));
    let bbox = republished.catalog.load().spatial_extent.unwrap();
    let before = view(&republished, bbox).await.unwrap();

    // The scan lists the new item where the old one, loaded, still is.
    let current = republished.catalog.load_full();
    let mut scanned = (*current).clone();
    scanned.entries.insert(time, stub("new.tif", &new_file));
    republished.install_scan(&current, scanned);
    assert!(
        !republished.is_metadata_loaded(&time),
        "the new item is not given the old item's header"
    );
    assert_eq!(republished.content_version(), 1);

    republished.preload_stac_metadata(&current).await;
    assert!(republished.is_metadata_loaded(&time));
    let after = view(&republished, bbox).await.unwrap();
    assert_ne!(after, before);

    let fresh = engine();
    fresh.catalog.store(with(stub("new.tif", &new_file)));
    fresh.preload_stac_metadata(&Catalog::empty()).await;
    assert_eq!(
        after,
        view(&fresh, bbox).await.unwrap(),
        "the new asset's pixels"
    );
}
