//! A remote COG whose tile ranges the store refuses for good after the scan
//! (#1074). A 404, a 416 or an object now shorter than the range is not
//! retried: the request draws nodata, and the timestep leaves the catalog at
//! once, so later requests snap to one that exists. The next poll removes
//! the entry, and a listing that still names the same object does not bring
//! it back; a new object at that timestep does, and moves
//! `content_version`. A 403 is not retried either: it is a 503, which no
//! cache keeps, and the timestep stays.

use std::collections::HashMap;
use std::ops::Range;

use ds_core::map_engine::{MapEngine, OutputCrs, RasterTile};
use ds_storage::object_store::{
    self, memory::InMemory, path::Path as ObjectPath, ObjectStore, ObjectStoreExt, PutPayload,
};
use ds_storage::DataStore;
use futures::stream::BoxStream;

use super::*;
use crate::reader::DataSource;
use crate::remote_header_tests::FIXTURE;

const OLDER: &str = "d/radar_20260324T2310Z.tif";
const NEWER: &str = "d/radar_20260324T2315Z.tif";

fn at(s: &str) -> DateTime<Utc> {
    s.parse().unwrap()
}

fn older() -> DateTime<Utc> {
    at("2026-03-24T23:10:00Z")
}

fn newer() -> DateTime<Utc> {
    at("2026-03-24T23:15:00Z")
}

/// How the store refuses a key's range reads, each in the shape
/// object_store's S3 client gives it.
#[derive(Debug, Clone, Copy)]
enum Refusal {
    NotFound,
    Forbidden,
    /// 416, which object_store reports only in its request error's text.
    Unsatisfiable,
}

impl Refusal {
    fn error(self, path: &ObjectPath) -> object_store::Error {
        match self {
            Refusal::NotFound => object_store::Error::NotFound {
                path: path.to_string(),
                source: "404 Not Found".into(),
            },
            Refusal::Forbidden => object_store::Error::PermissionDenied {
                path: path.to_string(),
                source: "403 Forbidden".into(),
            },
            Refusal::Unsatisfiable => object_store::Error::Generic {
                store: "S3",
                source: "Server returned non-2xx status code: 416 Range Not Satisfiable: \
                         <Error><Code>InvalidRange</Code></Error>"
                    .into(),
            },
        }
    }
}

/// An in-memory store that refuses the range reads of the keys in
/// `refusals` and records every range read.
#[derive(Debug, Default)]
struct RefusingStore {
    inner: InMemory,
    refusals: std::sync::Mutex<HashMap<String, Refusal>>,
    range_reads: std::sync::Mutex<Vec<(String, Range<u64>)>>,
}

impl RefusingStore {
    fn refuse(&self, key: &str, refusal: Option<Refusal>) {
        let mut refusals = self.refusals.lock().unwrap();
        match refusal {
            Some(refusal) => refusals.insert(key.to_string(), refusal),
            None => refusals.remove(key),
        };
    }

    /// The range reads of `key` so far.
    fn reads(&self, key: &str) -> Vec<Range<u64>> {
        let reads = self.range_reads.lock().unwrap();
        reads
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, range)| range.clone())
            .collect()
    }
}

impl std::fmt::Display for RefusingStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RefusingStore")
    }
}

#[async_trait::async_trait]
impl ObjectStore for RefusingStore {
    async fn get_opts(
        &self,
        path: &ObjectPath,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        if let (false, Some(object_store::GetRange::Bounded(range))) =
            (options.head, &options.range)
        {
            self.range_reads
                .lock()
                .unwrap()
                .push((path.to_string(), range.clone()));
            let refusal = self.refusals.lock().unwrap().get(path.as_ref()).copied();
            if let Some(refusal) = refusal {
                return Err(refusal.error(path));
            }
        }
        self.inner.get_opts(path, options).await
    }

    async fn put_opts(
        &self,
        path: &ObjectPath,
        payload: PutPayload,
        options: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        self.inner.put_opts(path, payload, options).await
    }

    async fn put_multipart_opts(
        &self,
        path: &ObjectPath,
        options: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(path, options).await
    }

    fn delete_stream(
        &self,
        paths: BoxStream<'static, object_store::Result<ObjectPath>>,
    ) -> BoxStream<'static, object_store::Result<ObjectPath>> {
        self.inner.delete_stream(paths)
    }

    fn list(
        &self,
        prefix: Option<&ObjectPath>,
    ) -> BoxStream<'static, object_store::Result<object_store::ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&ObjectPath>,
    ) -> object_store::Result<object_store::ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &ObjectPath,
        to: &ObjectPath,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

/// An empty local directory for the engine to start on, removed on drop.
struct EmptyDir(PathBuf);

impl EmptyDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "meteocore_gone_object_{}_{}",
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

/// An engine over two frames of the fixture in `store`, five minutes apart.
/// `new` builds a remote engine only for s3:// or http(s):// URLs, so it
/// starts on an empty local directory and is then pointed at the store; the
/// poll's header reads catalogue both frames.
fn two_frame_engine(local: &EmptyDir, store: &Arc<RefusingStore>) -> GeoTiffEngine {
    let fixture = std::fs::read(FIXTURE).unwrap();
    for key in [OLDER, NEWER] {
        futures::executor::block_on(
            store
                .inner
                .put(&ObjectPath::from(key), PutPayload::from(fixture.clone())),
        )
        .unwrap();
    }
    let config: GeoTiffConfig = serde_json::from_value(serde_json::json!({
        "filename_template": "radar_%Y%m%dT%H%MZ.tif",
        "parameter": "reflectivity",
        "unit": "dBZ",
    }))
    .unwrap();
    let mut engine = GeoTiffEngine::new("gone-object", local.0.to_str(), &config).unwrap();
    engine.store_mode = StoreMode::Remote {
        store: DataStore::new(store.clone()),
        prefix: ObjectPath::from("d"),
    };
    engine.poll_once();
    assert_eq!(times(&engine), [older(), newer()]);
    engine
}

/// The catalog's timesteps, gone ones included.
fn times(engine: &GeoTiffEngine) -> Vec<DateTime<Utc>> {
    engine.catalog.load().entries.keys().copied().collect()
}

/// A `(lon, lat)` in the first full-resolution tile.
fn inside(engine: &GeoTiffEngine) -> (f64, f64) {
    let catalog = engine.catalog.load();
    let entry = catalog.entries.values().next().unwrap();
    entry
        .metadata()
        .unwrap()
        .geo_transform
        .pixel_to_world(50, 50)
}

/// The whole frame as one map view at `time`.
fn render(engine: &GeoTiffEngine, time: DateTime<Utc>) -> RasterTile {
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
        .expect("a refused range is no error")
}

/// The first full-resolution tile's encoded range in the newer frame.
fn first_tile(engine: &GeoTiffEngine) -> Range<usize> {
    let catalog = engine.catalog.load();
    let DataSource::Remote { tile_info, .. } = &**catalog.entries[&newer()].source().unwrap()
    else {
        panic!("a remote source");
    };
    let offset = tile_info.tile_offsets[0] as usize;
    offset..offset + tile_info.tile_byte_counts[0] as usize
}

/// A 404, a 416 and an object now ending inside the tile: the position read
/// is one range read, not retried, and answers null with `Ok`. The timestep
/// then leaves the catalog's accessors at once, before any poll: time
/// resolution, the `RasterInfo` snapshot, EDR, and the render, which snaps
/// to the older frame and makes no further read of the gone object.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gone_object_reads_null_once_and_its_timestep_leaves_at_once() {
    for refusal in [Some(Refusal::NotFound), Some(Refusal::Unsatisfiable), None] {
        let local = EmptyDir::new();
        let store = Arc::new(RefusingStore::default());
        let engine = two_frame_engine(&local, &store);
        let (lon, lat) = inside(&engine);
        match refusal {
            Some(refusal) => store.refuse(NEWER, Some(refusal)),
            // Truncated in place: the store answers the tile's range with
            // a shorter object.
            None => {
                let tile = first_tile(&engine);
                let mut bytes = std::fs::read(FIXTURE).unwrap();
                bytes.truncate(tile.start + 1);
                store
                    .inner
                    .put(&ObjectPath::from(NEWER), PutPayload::from(bytes))
                    .await
                    .unwrap();
            }
        }
        let reads = store.reads(NEWER).len();

        let point = engine
            .query_point(lat, lon, Some((newer(), newer())), None)
            .expect("a refused range is a null, not an error");
        assert_eq!(point.ranges["reflectivity"].values, [None], "{refusal:?}");
        assert_eq!(
            store.reads(NEWER).len() - reads,
            1,
            "{refusal:?}: one read, no retries"
        );

        assert_eq!(engine.resolve_time(Some(newer()), None), Some(older()));
        assert_eq!(engine.resolve_time(None, None), Some(older()));
        assert_eq!(engine.raster_info().times, [older()]);
        let series = engine.query_point(lat, lon, None, None).unwrap();
        let DomainDescription::PointSeries { t, .. } = &series.domain else {
            panic!("a point series");
        };
        assert_eq!(t, &[older()], "{refusal:?}: EDR lists the older frame only");
        assert!(matches!(
            engine.query_point(lat, lon, Some((newer(), newer())), None),
            Err(DataServerError::LocationNotFound(_))
        ));
        let tile = render(&engine, newer());
        assert!(
            !tile.values.is_all_nodata(),
            "{refusal:?}: the newer time renders the older frame"
        );
        assert_eq!(
            store.reads(NEWER).len() - reads,
            1,
            "{refusal:?}: the gone object is not read again"
        );
        assert_eq!(engine.content_version(), 0);
    }
}

/// The request that meets the refusal draws nodata with `Ok`, each tile
/// read once. The next poll removes the entry for real without reading the
/// header of the object its listing still names. A new object at that
/// timestep (another size) is catalogued again and moves `content_version`,
/// so renders cached while the timestep was gone are not served.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gone_frame_renders_nodata_then_the_poll_removes_it_until_it_changes() {
    let local = EmptyDir::new();
    let store = Arc::new(RefusingStore::default());
    let engine = two_frame_engine(&local, &store);
    store.refuse(NEWER, Some(Refusal::NotFound));
    let reads = store.reads(NEWER).len();

    let tile = render(&engine, newer());
    assert!(tile.values.is_all_nodata(), "the gone frame draws nodata");
    let tiles = store.reads(NEWER)[reads..].to_vec();
    assert!(!tiles.is_empty());
    let mut distinct = tiles.clone();
    distinct.sort_by_key(|r| (r.start, r.end));
    distinct.dedup();
    assert_eq!(distinct.len(), tiles.len(), "no tile read is retried");
    assert_eq!(engine.resolve_time(Some(newer()), None), Some(older()));

    // The listing still names the object with the same size.
    engine.poll_once();
    assert_eq!(times(&engine), [older()], "the poll removes the entry");
    engine.poll_once();
    assert_eq!(times(&engine), [older()]);
    assert_eq!(
        store.reads(NEWER).len(),
        reads + tiles.len(),
        "no header read of an object still listed as it was"
    );
    assert_eq!(engine.content_version(), 0);

    // A new object at that timestep, under the same key.
    store.refuse(NEWER, None);
    let mut bytes = std::fs::read(FIXTURE).unwrap();
    bytes.extend_from_slice(&[0; 16]);
    store
        .inner
        .put(&ObjectPath::from(NEWER), PutPayload::from(bytes))
        .await
        .unwrap();
    engine.poll_once();
    assert_eq!(times(&engine), [older(), newer()]);
    assert_eq!(engine.content_version(), 1, "cached renders go unread");
    assert_eq!(engine.resolve_time(None, None), Some(newer()));
    assert!(!render(&engine, newer()).values.is_all_nodata());
    engine.poll_once();
    assert_eq!(
        engine.content_version(),
        1,
        "an unchanged poll moves nothing"
    );
}

/// A 403 is not retried. It is the server's credentials, not the data, so
/// it fails the request with `ResourceExhausted` (503), which no rendered or
/// meta-tile cache keeps, in the map render and in EDR alike, and the
/// timestep stays. Once the store serves the key again, the same requests
/// read it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forbidden_range_is_a_503_and_keeps_the_timestep() {
    let local = EmptyDir::new();
    let store = Arc::new(RefusingStore::default());
    let engine = two_frame_engine(&local, &store);
    let (lon, lat) = inside(&engine);
    store.refuse(NEWER, Some(Refusal::Forbidden));

    for _ in 0..2 {
        let reads = store.reads(NEWER).len();
        assert!(matches!(
            engine.query_point(lat, lon, Some((newer(), newer())), None),
            Err(DataServerError::ResourceExhausted)
        ));
        assert_eq!(store.reads(NEWER).len() - reads, 1, "one read, no retries");
        assert_eq!(engine.resolve_time(None, None), Some(newer()));
        assert_eq!(engine.raster_info().times, [older(), newer()]);
    }
    assert!(matches!(
        engine.query_bbox(
            lon,
            lat,
            lon + 0.01,
            lat + 0.01,
            Some((newer(), newer())),
            None
        ),
        Err(DataServerError::ResourceExhausted)
    ));
    let reads = store.reads(NEWER).len();
    let bbox = engine.catalog.load().spatial_extent.unwrap();
    assert!(matches!(
        engine.get_raster_tile(
            bbox,
            400,
            220,
            Some(newer()),
            &OutputCrs::Wgs84,
            None,
            None,
            None
        ),
        Err(DataServerError::ResourceExhausted)
    ));
    let tiles = store.reads(NEWER)[reads..].to_vec();
    let mut distinct = tiles.clone();
    distinct.sort_by_key(|r| (r.start, r.end));
    distinct.dedup();
    assert_eq!(distinct.len(), tiles.len(), "no tile read is retried");
    engine.poll_once();
    assert_eq!(times(&engine), [older(), newer()], "the poll keeps it");

    store.refuse(NEWER, None);
    assert!(!render(&engine, newer()).values.is_all_nodata());
    assert_eq!(engine.content_version(), 0);
}

/// One forbidden-range WARN per collection per interval, counting the
/// refusals since the last.
#[test]
fn forbidden_warnings_are_rate_limited() {
    let warnings = gone::ForbiddenWarnings::default();
    assert_eq!(warnings.note(2), Some(2));
    assert_eq!(warnings.note(1), None);
    assert_eq!(warnings.note(3), None);
}

const STAC_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../testdata/radar-tm35fin/radar_tm35_20260406T0640Z.tif"
);

/// A STAC API over two items five minutes apart whose assets are the
/// TM35FIN fixture. The newer item's href is `newer_asset`; an asset path
/// in `gone` answers 404. Counts the requests per asset path.
/// [`MockStac::start_with`] gives the items a `file:size`.
struct MockStac {
    items_url: String,
    allowlist: String,
    times: [DateTime<Utc>; 2],
    newer_asset: Arc<std::sync::Mutex<String>>,
    gone: Arc<std::sync::Mutex<Vec<String>>>,
    requests: Arc<std::sync::Mutex<HashMap<String, usize>>>,
}

impl MockStac {
    async fn start() -> Self {
        Self::start_with(false).await
    }

    /// `sized`: each item states the fixture's `file:size`, so a metadata
    /// load reads the header range first, without a HEAD.
    async fn start_with(sized: bool) -> Self {
        let size = sized.then(|| std::fs::metadata(STAC_FIXTURE).unwrap().len());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let times = [at("2026-04-06T06:35:00Z"), at("2026-04-06T06:40:00Z")];
        let newer_asset = Arc::new(std::sync::Mutex::new("/data/item-1.tif".to_string()));
        let gone = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let requests = Arc::new(std::sync::Mutex::new(HashMap::<String, usize>::new()));

        let (listed, href_base) = (Arc::clone(&newer_asset), base.clone());
        let items = move || {
            let newer = listed.lock().unwrap().clone();
            let hrefs = [
                format!("{href_base}/data/item-0.tif"),
                format!("{href_base}{newer}"),
            ];
            let features: Vec<serde_json::Value> = times
                .iter()
                .zip(hrefs)
                .enumerate()
                .map(|(i, (t, href))| {
                    let mut asset = serde_json::json!({ "href": href });
                    if let Some(size) = size {
                        asset["file:size"] = size.into();
                    }
                    serde_json::json!({
                        "type": "Feature",
                        "id": format!("item-{i}"),
                        "properties": { "datetime": t.to_rfc3339() },
                        "assets": { "data": asset },
                    })
                })
                .collect();
            async move {
                axum::Json(serde_json::json!({
                    "type": "FeatureCollection",
                    "features": features,
                    "links": [],
                }))
            }
        };
        let collection = serde_json::json!({
            "extent": {
                "spatial": { "bbox": [[19.0, 59.0, 32.0, 70.5]] },
                "temporal": { "interval": [[times[0].to_rfc3339(), null]] },
            },
        });
        let (refused, counted) = (Arc::clone(&gone), Arc::clone(&requests));
        let data = axum::Router::new()
            .route_service(
                "/data/{name}",
                tower_http::services::ServeFile::new(STAC_FIXTURE),
            )
            .layer(axum::middleware::from_fn(
                move |request: axum::extract::Request, next: axum::middleware::Next| {
                    let path = request.uri().path().to_string();
                    *counted.lock().unwrap().entry(path.clone()).or_default() += 1;
                    let gone = refused.lock().unwrap().contains(&path);
                    async move {
                        if gone {
                            axum::response::IntoResponse::into_response(
                                axum::http::StatusCode::NOT_FOUND,
                            )
                        } else {
                            next.run(request).await
                        }
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
            newer_asset,
            gone,
            requests,
        }
    }

    fn requests(&self, path: &str) -> usize {
        self.requests
            .lock()
            .unwrap()
            .get(path)
            .copied()
            .unwrap_or(0)
    }

    /// A STAC engine over this API, without a tile cache, so the poll
    /// pre-warms nothing.
    fn engine(&self) -> GeoTiffEngine {
        let config: GeoTiffConfig = serde_json::from_value(serde_json::json!({
            "parameter": "reflectivity",
            "unit": "dBZ",
            "tile_cache_mb": 0,
            "stac_url": self.items_url,
            "stac_asset_key": "data",
            "stac_asset_allowlist": [self.allowlist],
        }))
        .unwrap();
        GeoTiffEngine::new("gone-stac", None, &config).unwrap()
    }
}

/// A map view of the STAC fixture at `time`, rendered on a blocking worker
/// as the render executor does.
async fn render_stac(engine: &Arc<GeoTiffEngine>, time: DateTime<Utc>) -> RasterTile {
    let engine = Arc::clone(engine);
    tokio::task::spawn_blocking(move || {
        engine.get_raster_tile(
            [20.0, 60.0, 30.0, 70.0],
            128,
            128,
            Some(time),
            &OutputCrs::Wgs84,
            None,
            None,
            None,
        )
    })
    .await
    .unwrap()
    .expect("a refused range is no error")
}

/// A STAC item whose asset a tile read found gone draws nodata, leaves the
/// catalog at once, and stays out while the listing names the same href:
/// across a poll, which reads nothing from the asset, and across a request's
/// on-demand STAC fetch. Once the listing names another href, the item is
/// catalogued again and `content_version` moves.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stac_item_with_a_gone_asset_stays_out_until_its_listing_changes() {
    let stac = MockStac::start().await;
    let [t0, t1] = stac.times;
    let engine = Arc::new(stac.engine());
    engine.poll_cycle().await;
    assert!(engine.is_metadata_loaded(&t0) && engine.is_metadata_loaded(&t1));
    assert_eq!(engine.raster_info().times, [t0, t1]);

    stac.gone.lock().unwrap().push("/data/item-1.tif".into());
    assert!(render_stac(&engine, t1).await.values.is_all_nodata());
    let requests = stac.requests("/data/item-1.tif");
    assert!(requests > 0);
    assert_eq!(engine.resolve_time(None, None), Some(t0));
    assert_eq!(engine.raster_info().times, [t0]);

    engine.poll_cycle().await;
    assert_eq!(times(&engine), [t0], "the listed item stays out");
    let StoreMode::RemoteStac { client } = &engine.store_mode else {
        unreachable!()
    };
    let fetched = catalog::fetch_stac_range(
        client,
        &engine.catalog.load(),
        (t0 - chrono::Duration::hours(1), t1),
        "gone-stac",
    )
    .unwrap();
    engine.store_stac_catalog(fetched);
    assert_eq!(times(&engine), [t0], "and out of a request's STAC fetch");
    // This poll asks for the items after the older one: the gone one again.
    engine.poll_cycle().await;
    assert_eq!(times(&engine), [t0], "and out of the next poll's listing");
    assert_eq!(
        stac.requests("/data/item-1.tif"),
        requests,
        "nothing reads the gone asset again"
    );
    assert!(!render_stac(&engine, t1).await.values.is_all_nodata());
    assert_eq!(engine.content_version(), 0);

    *stac.newer_asset.lock().unwrap() = "/data/item-1-v2.tif".into();
    engine.poll_cycle().await;
    assert_eq!(times(&engine), [t0, t1], "a new href is a new object");
    assert!(engine.is_metadata_loaded(&t1));
    assert_eq!(engine.content_version(), 1);
    assert_eq!(engine.resolve_time(None, None), Some(t1));
    assert!(!render_stac(&engine, t1).await.values.is_all_nodata());
    assert!(stac.requests("/data/item-1-v2.tif") > 0);
}

/// #1119: a STAC item whose asset was gone before its header was ever read
/// answers 404 to that first read, the header range read of a map request.
/// It is read once, then dropped: the request draws nodata and the timestep
/// leaves at once. No later map, poll or on-demand STAC fetch reads the
/// asset again while the listing names the same href and size. Another
/// href is a new object, catalogued again, and `content_version` moves.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stac_item_gone_before_its_header_is_read_is_read_once_then_dropped() {
    let stac = MockStac::start_with(true).await;
    let [t0, t1] = stac.times;
    stac.gone.lock().unwrap().push("/data/item-1.tif".into());
    let engine = Arc::new(stac.engine());
    assert!(engine.catalog.load().entries.is_empty());

    // The render lists the items on demand and loads the one it selects.
    assert!(render_stac(&engine, t1).await.values.is_all_nodata());
    assert_eq!(stac.requests("/data/item-1.tif"), 1, "read once");
    assert_eq!(engine.resolve_time(None, None), Some(t0));
    assert_eq!(engine.raster_info().times, [t0]);
    assert!(!render_stac(&engine, t1).await.values.is_all_nodata());

    engine.poll_cycle().await;
    assert_eq!(times(&engine), [t0], "the listed item stays out");
    let StoreMode::RemoteStac { client } = &engine.store_mode else {
        unreachable!()
    };
    let fetched = catalog::fetch_stac_range(
        client,
        &engine.catalog.load(),
        (t0 - chrono::Duration::hours(1), t1),
        "gone-stac",
    )
    .unwrap();
    engine.store_stac_catalog(fetched);
    assert_eq!(times(&engine), [t0], "and out of a request's STAC fetch");
    engine.poll_cycle().await;
    assert_eq!(times(&engine), [t0], "and out of the next poll's listing");
    assert!(!render_stac(&engine, t1).await.values.is_all_nodata());
    assert_eq!(
        stac.requests("/data/item-1.tif"),
        1,
        "nothing reads the gone asset again"
    );
    assert_eq!(engine.content_version(), 0);

    *stac.newer_asset.lock().unwrap() = "/data/item-1-v2.tif".into();
    engine.poll_cycle().await;
    assert_eq!(times(&engine), [t0, t1], "a new href is a new object");
    assert!(engine.is_metadata_loaded(&t1));
    assert_eq!(engine.content_version(), 1);
    assert!(!render_stac(&engine, t1).await.values.is_all_nodata());
    assert!(stac.requests("/data/item-1-v2.tif") > 0);
}
