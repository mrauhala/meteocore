//! A composite read that fails after the store's retries fails the request
//! (#1496), in EDR and in the map path alike: a store that kept answering
//! `503 SlowDown` is `Unavailable` (503 with `Retry-After: 10`), any other
//! failed read `ResourceExhausted` (503), never a null timestep that the
//! answer's caches would keep as "no data", nor the WMS error tile. Nothing
//! failed is cached, so the same request reads the file once the store
//! answers again. A file whose bytes do not decode, or that is gone since
//! the scan, is still a null timestep in EDR. A map of a file gone since
//! the scan, or grown past the size limit, is a 503 (`ResourceExhausted`)
//! until the next scan drops it (#1506); of one that does not decode, an
//! `Engine` error, the WMS error tile.

use std::sync::atomic::{AtomicUsize, Ordering};

use ds_core::edr_engine::EdrEngine;
use ds_core::error::ErrorClass;
use ds_core::model::CoverageResponse;
use ds_storage::object_store::{self, memory::InMemory, path::Path, ObjectStore, ObjectStoreExt};
use futures::stream::BoxStream;

use super::*;

/// The committed DMI composite.
const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../testdata/odim-dmi-fixture.h5"
);

/// The older of two timesteps: the newer is the engine's seed composite,
/// read at construction and cached, so the older is what a request reads.
const OLDER: &str = "2026-01-20T11:25:00Z";
const NEWER: &str = "2026-01-20T11:30:00Z";

/// How [`FailingStore`] answers a HEAD or a GET.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Answer {
    #[default]
    Serve,
    /// As a bucket that kept answering `503 SlowDown` through object_store's
    /// retries, in object_store's wording, which ds-storage reads the status
    /// from (#1355).
    Throttle,
    /// As a connection that kept resetting: a failed read the store does not
    /// call unavailable.
    Fail,
    /// As S3 refuses a key these credentials may not read, a 403 (#1046).
    Forbid,
}

/// An in-memory store whose HEADs and GETs answer as told.
#[derive(Debug, Default)]
struct FailingStore {
    inner: InMemory,
    heads: Mutex<Answer>,
    gets: Mutex<Answer>,
    /// HEADs and GETs asked for.
    reads: AtomicUsize,
}

impl FailingStore {
    fn put_file(&self, key: &str, bytes: Vec<u8>) {
        futures::executor::block_on(self.inner.put(&Path::from(key), bytes.into())).unwrap();
    }

    fn remove(&self, key: &str) {
        futures::executor::block_on(self.inner.delete(&Path::from(key))).unwrap();
    }

    /// Answer every HEAD with `heads` and every GET with `gets`.
    fn answer(&self, heads: Answer, gets: Answer) {
        *self.heads.lock().unwrap() = heads;
        *self.gets.lock().unwrap() = gets;
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}

impl std::fmt::Display for FailingStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FailingStore")
    }
}

#[async_trait::async_trait]
impl ObjectStore for FailingStore {
    async fn get_opts(
        &self,
        path: &Path,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let answer = match options.head {
            true => *self.heads.lock().unwrap(),
            false => *self.gets.lock().unwrap(),
        };
        match answer {
            Answer::Serve => self.inner.get_opts(path, options).await,
            Answer::Throttle => Err(object_store::Error::Generic {
                store: "S3",
                source: "Server returned non-2xx status code: 503 Service Unavailable: \
                         <Error><Code>SlowDown</Code></Error>"
                    .into(),
            }),
            Answer::Fail => Err(object_store::Error::Generic {
                store: "S3",
                source: "error sending request: connection reset by peer".into(),
            }),
            Answer::Forbid => Err(object_store::Error::PermissionDenied {
                path: path.to_string(),
                source: "Access Denied".into(),
            }),
        }
    }

    async fn put_opts(
        &self,
        path: &Path,
        payload: object_store::PutPayload,
        options: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        self.inner.put_opts(path, payload, options).await
    }

    async fn put_multipart_opts(
        &self,
        path: &Path,
        options: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(path, options).await
    }

    fn delete_stream(
        &self,
        paths: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(paths)
    }

    fn list(
        &self,
        prefix: Option<&Path>,
    ) -> BoxStream<'static, object_store::Result<object_store::ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&Path>,
    ) -> object_store::Result<object_store::ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

/// The key of the timestep `time` (`HHMM`) under `prefix`.
fn key(prefix: &str, time: &str) -> String {
    format!("{prefix}20260120T{time}_radar.h5")
}

/// An engine over `files`, each `(HHMM, bytes)` under `prefix`, a prefix
/// of the test's own: the composite cache is process-wide and keyed by the
/// object key.
fn engine_over(store: &Arc<FailingStore>, prefix: &str, files: &[(&str, Vec<u8>)]) -> OdimEngine {
    for (time, bytes) in files {
        store.put_file(&key(prefix, time), bytes.clone());
    }
    let config = super::tests::config_with(None, None, None, None);
    OdimEngine::new_remote_for_test(
        "read-failure",
        ds_storage::DataStore::new(store.clone()),
        prefix,
        "reflectivity",
        "dBZ",
        &config,
    )
    .expect("the engine reads its seed composite")
}

fn at(time: &str) -> DateTime<Utc> {
    time.parse().unwrap()
}

/// The values of a single coverage's one range.
fn values(response: CoverageResponse) -> Vec<Option<f64>> {
    let CoverageResponse::Single(result) = response else {
        panic!("a single coverage");
    };
    result.ranges["reflectivity"].values.clone()
}

/// The `(lon, lat)` of the first pixel centre of `engine`'s seed composite
/// that holds an echo: the fixture's echoes are sparse.
fn echo(engine: &OdimEngine) -> (f64, f64) {
    let seed = engine.catalog.load().last().unwrap().location.clone();
    let composite = engine.load_composite(&seed).unwrap();
    let [w, s, e, n] = composite.bbox;
    let (dx, dy) = (
        (e - w) / composite.xsize as f64,
        (n - s) / composite.ysize as f64,
    );
    let (rows, cols) = composite.pixels.shape();
    (0..rows)
        .flat_map(|row| (0..cols).map(move |col| (row, col)))
        .find(|&(row, col)| {
            let c = &composite;
            c.pixels
                .sample(row, col, c.gain, c.offset, c.nodata, c.undetect)
                .is_some()
        })
        .and_then(|(row, col)| {
            let x = w + (col as f64 + 0.5) * dx;
            let y = n - (row as f64 + 0.5) * dy;
            composite.crs.inverse(x, y)
        })
        .expect("the fixture holds an echo")
}

/// The point, area and map views of one engine, at an echo of its seed
/// composite.
struct Views<'a> {
    engine: &'a OdimEngine,
    point: String,
    area: String,
    /// A map well inside the echo's pixel.
    bbox: [f64; 4],
}

impl<'a> Views<'a> {
    fn new(engine: &'a OdimEngine) -> Self {
        let (x, y) = echo(engine);
        let [w, s, e, n] = engine.seed_spatial_extent;
        let (dx, dy) = ((e - w) / 8.0, (n - s) / 8.0);
        let (x0, x1, y0, y1) = (x - dx, x + dx, y - dy, y + dy);
        Self {
            engine,
            point: format!("POINT({x} {y})"),
            area: format!("POLYGON(({x0} {y0},{x1} {y0},{x1} {y1},{x0} {y1},{x0} {y0}))"),
            bbox: [x - 0.001, y - 0.001, x + 0.001, y + 0.001],
        }
    }

    fn position(&self, from: &str, to: &str) -> Result<Vec<Option<f64>>, DataServerError> {
        self.engine
            .query_position(&self.point, Some((at(from), at(to))), None, None, None)
            .map(values)
    }

    fn area(&self, from: &str, to: &str) -> Result<Vec<Option<f64>>, DataServerError> {
        self.engine
            .query_area(&self.area, Some((at(from), at(to))), None, None, None)
            .map(values)
    }

    /// The echo's surroundings at `time`, as a map draws them.
    fn map(&self, time: &str) -> Result<RasterTile, DataServerError> {
        self.engine.get_raster_tile(
            self.bbox,
            8,
            8,
            Some(at(time)),
            &OutputCrs::Wgs84,
            None,
            None,
            None,
        )
    }
}

/// Each of position, area and map fails with `want` and reads the store,
/// and the cache keeps nothing for the composite.
fn every_view_fails(
    views: &Views,
    store: &FailingStore,
    cached: &str,
    want: fn(&DataServerError) -> bool,
    what: &str,
) {
    type View<'v> = &'v dyn Fn() -> Result<(), DataServerError>;
    let answers: [(&str, View); 3] = [
        ("position", &|| views.position(OLDER, NEWER).map(drop)),
        ("area", &|| views.area(OLDER, NEWER).map(drop)),
        ("map", &|| views.map(OLDER).map(drop)),
    ];
    for (view, answer) in answers {
        let reads = store.reads();
        match answer() {
            Err(e) if want(&e) => assert!(e.class().is_transient(), "{what} {view}: {e:?}"),
            other => panic!("{what}: {view} answered {other:?}"),
        }
        assert!(store.reads() > reads, "{what}: {view} asked the store");
    }
    assert!(
        !COMPOSITE_CACHE.contains_key(cached),
        "{what}: nothing failed is cached"
    );
}

/// The acceptance case of #1496: under a store that keeps answering
/// `503 SlowDown`, EDR position and area and the map answer `Unavailable`
/// (`SourceUnavailable`: 503 with `Retry-After: 10`) with the file and the
/// upstream's status in its detail, for the WARN line; whether the HEAD or
/// the GET is throttled. A read that fails otherwise, or that the store
/// refuses (403), is `ResourceExhausted`. Once the store answers, the same
/// requests answer with the file's data.
#[test]
fn a_failed_composite_read_fails_edr_and_the_map_and_a_retry_reads() {
    let prefix = "read-failure-1496-throttled/";
    let fixture = std::fs::read(FIXTURE).unwrap();
    let store = Arc::new(FailingStore::default());
    let engine = engine_over(
        &store,
        prefix,
        &[("1125", fixture.clone()), ("1130", fixture.clone())],
    );
    let views = Views::new(&engine);
    let older = key(prefix, "1125");

    let unavailable = |e: &DataServerError| match e {
        DataServerError::Unavailable(detail) => {
            assert!(
                detail.starts_with("[read-failure] ODIM file `read-failure-1496-throttled/"),
                "{detail}"
            );
            assert!(detail.contains("503 Service Unavailable"), "{detail}");
            assert_eq!(e.class(), ErrorClass::SourceUnavailable);
            true
        }
        _ => false,
    };
    let busy = |e: &DataServerError| matches!(e, DataServerError::ResourceExhausted);

    store.answer(Answer::Throttle, Answer::Throttle);
    every_view_fails(&views, &store, &older, unavailable, "a throttled HEAD");
    store.answer(Answer::Serve, Answer::Throttle);
    every_view_fails(&views, &store, &older, unavailable, "a throttled GET");
    store.answer(Answer::Fail, Answer::Fail);
    every_view_fails(&views, &store, &older, busy, "a failed HEAD");
    store.answer(Answer::Serve, Answer::Fail);
    every_view_fails(&views, &store, &older, busy, "a failed GET");
    store.answer(Answer::Forbid, Answer::Forbid);
    every_view_fails(&views, &store, &older, busy, "a refused read");

    // The store answers again: the older timestep reads as the newer one,
    // the same file, which the engine read at construction.
    store.answer(Answer::Serve, Answer::Serve);
    let position = views
        .position(OLDER, NEWER)
        .expect("a retry reads the position");
    assert!(position[0].is_some(), "the echo is data: {position:?}");
    assert_eq!(position[0], position[1]);
    let area = views.area(OLDER, NEWER).expect("a retry reads the area");
    let (older_cells, newer_cells) = area.split_at(area.len() / 2);
    assert!(older_cells.iter().any(Option::is_some), "the echo is data");
    assert_eq!(older_cells, newer_cells);
    let map = views.map(OLDER).expect("a retry renders");
    assert!(!map.values.is_all_nodata(), "the echo renders");
    assert!(COMPOSITE_CACHE.contains_key(older.as_str()));
}

/// A file whose bytes do not decode, or that is gone since the scan (a
/// 404, told apart by type), stays a null timestep in EDR, next to the
/// timesteps that read: neither is a read that failed.
#[test]
fn undecodable_and_gone_composites_are_edr_null_timesteps() {
    let prefix = "read-failure-1496-unusable/";
    let fixture = std::fs::read(FIXTURE).unwrap();
    let store = Arc::new(FailingStore::default());
    let engine = engine_over(
        &store,
        prefix,
        &[
            ("1115", fixture.clone()),
            ("1120", vec![0x55; fixture.len()]),
            ("1125", fixture.clone()),
            ("1130", fixture.clone()),
        ],
    );
    store.remove(&key(prefix, "1115"));
    let views = Views::new(&engine);
    let from = "2026-01-20T11:15:00Z";

    let position = views.position(from, NEWER).expect("the series answers");
    assert_eq!(&position[..2], [None, None], "{position:?}");
    assert!(position[2].is_some(), "the echo is data: {position:?}");
    assert_eq!(position[2], position[3]);

    let area = views.area(from, NEWER).expect("the area answers");
    let cells = area.len() / 4;
    assert!(area[..2 * cells].iter().all(Option::is_none));
    assert!(area[2 * cells..3 * cells].iter().any(Option::is_some));
    assert_eq!(area[2 * cells..3 * cells], area[3 * cells..]);

    // A map of the gone file is a 503 until the next scan drops it, never
    // the error tile of a server fault (#1506); one of the bytes that do
    // not decode is that error tile.
    let gone = views.map(from).map(drop);
    assert!(
        matches!(gone, Err(DataServerError::ResourceExhausted)),
        "{gone:?}"
    );
    let undecodable = views.map("2026-01-20T11:20:00Z").map(drop);
    assert!(
        matches!(&undecodable, Err(e @ DataServerError::Engine(_)) if e.class() == ErrorClass::Internal),
        "{undecodable:?}"
    );
    for file in ["1115", "1120"] {
        assert!(
            !COMPOSITE_CACHE.contains_key(key(prefix, file).as_str()),
            "{file}"
        );
    }
}

/// A file grown past the size limit since the scan listed it is never
/// read: a null timestep in EDR, and a map of it the 503 a gone file's is
/// (#1506), with nothing cached.
#[test]
fn a_composite_grown_past_the_limit_is_an_edr_null_and_a_map_503() {
    let prefix = "read-failure-1506-grown/";
    let fixture = std::fs::read(FIXTURE).unwrap();
    let store = Arc::new(FailingStore::default());
    let engine = engine_over(
        &store,
        prefix,
        &[
            ("1120", fixture.clone()),
            ("1125", fixture.clone()),
            ("1130", fixture.clone()),
        ],
    );
    let grown = key(prefix, "1120");
    let limit = usize::try_from(crate::catalog::MAX_REMOTE_FILE_SIZE).unwrap();
    store.put_file(&grown, vec![0; limit + 1]);
    let views = Views::new(&engine);
    let from = "2026-01-20T11:20:00Z";

    let position = views.position(from, NEWER).expect("the series answers");
    assert_eq!(position[0], None, "{position:?}");
    assert!(position[1].is_some(), "the echo is data: {position:?}");
    let map = views.map(from).map(drop);
    assert!(
        matches!(map, Err(DataServerError::ResourceExhausted)),
        "{map:?}"
    );
    assert!(!COMPOSITE_CACHE.contains_key(grown.as_str()));
}

/// A local file the server cannot read (here a directory in its place) is
/// a failed read, 503; once it is gone, it is a null timestep.
#[test]
fn a_local_composite_that_cannot_be_read_fails_and_a_gone_one_is_null() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = std::fs::read(FIXTURE).unwrap();
    for time in ["1120", "1125", "1130"] {
        std::fs::write(dir.path().join(key("", time)), &fixture).unwrap();
    }
    let config = super::tests::config_with(None, None, None, None);
    let engine = OdimEngine::new("read-failure-local", dir.path().to_str(), &config).unwrap();
    let views = Views::new(&engine);
    let from = "2026-01-20T11:20:00Z";

    let unreadable = dir.path().join(key("", "1120"));
    std::fs::remove_file(&unreadable).unwrap();
    std::fs::create_dir(&unreadable).unwrap();
    for answer in [
        views.position(from, NEWER).map(drop),
        views.area(from, NEWER).map(drop),
        views.map(from).map(drop),
    ] {
        assert!(
            matches!(answer, Err(DataServerError::ResourceExhausted)),
            "{answer:?}"
        );
    }

    std::fs::remove_dir(&unreadable).unwrap();
    let position = views.position(from, NEWER).expect("a gone file is a gap");
    assert_eq!(position[0], None);
    assert!(position[1].is_some(), "the echo is data: {position:?}");
    assert_eq!(position[1], position[2]);
    // A map of it is a 503 until the next scan drops it (#1506).
    let map = views.map(from).map(drop);
    assert!(
        matches!(map, Err(DataServerError::ResourceExhausted)),
        "{map:?}"
    );
}

/// A template probe never re-HEADs a file it found (#287), so a map that
/// finds one gone has it HEADed again on the next poll, which drops it:
/// the same time then draws its nearest file, as a listing source's next
/// scan has it do (#1506).
#[test]
fn a_template_probe_drops_a_withdrawn_composite_on_its_next_poll() {
    use chrono::DurationRound;

    let fixture = std::fs::read(FIXTURE).unwrap();
    let store = Arc::new(FailingStore::default());
    let base = "read-failure-1506-template";
    let config = super::tests::config_with(None, None, None, Some("-PT30M"));
    let template = config.filename_template.clone().unwrap();
    // The two newest slots of the probe's window.
    let cadence = chrono::Duration::minutes(5);
    let newest = Utc::now().duration_trunc(cadence).unwrap();
    let older = newest - cadence;
    let key_at = |time: DateTime<Utc>| format!("{base}/{}", time.format(&template));
    for time in [older, newest] {
        store.put_file(&key_at(time), fixture.clone());
    }
    let prober = template_prober(
        "read-failure-template",
        base.to_owned(),
        template.clone(),
        300,
        Some(TimeWindow::parse("-PT30M").unwrap()),
        None,
    )
    .unwrap();
    let source = Source::TemplateHttp {
        store: ds_storage::DataStore::new(store.clone()),
        base_url: "test://template".into(),
        prober: Arc::new(Mutex::new(prober)),
    };
    let engine = OdimEngine::assemble(
        "read-failure-template",
        "reflectivity".into(),
        "dBZ".into(),
        Overrides {
            gain: None,
            offset: None,
            nodata: None,
        },
        source,
        build_matcher(&config).unwrap(),
        None,
        30,
    )
    .expect("the engine reads its seed composite");
    let times = || -> Vec<DateTime<Utc>> { engine.catalog.load().iter().map(|e| e.time).collect() };
    assert_eq!(times(), [older, newest]);
    let views = Views::new(&engine);
    let older_time = older.to_rfc3339();

    store.remove(&key_at(older));
    let map = views.map(&older_time).map(drop);
    assert!(
        matches!(map, Err(DataServerError::ResourceExhausted)),
        "{map:?}"
    );
    engine.poll_once();
    assert_eq!(times(), [newest], "the next poll drops the gone file");
    let map = views
        .map(&older_time)
        .expect("the time draws its nearest file");
    assert!(!map.values.is_all_nodata(), "the echo renders");
}
