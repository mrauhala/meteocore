//! A storage error on a remote COG's header read is not a non-COG (#1003):
//! the scan leaves the file out of the catalog, without downloading it
//! whole, and the next poll reads its header again. Only a header that
//! arrives but does not parse takes the full-download fallback. A STAC
//! asset's header read over direct HTTP makes the same distinction (#1025)
//! and types a refusal as tile reads do (#1119).

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ds_storage::discovery::{FilenameMatcher, ScanSpec};
use ds_storage::object_store::{
    self, memory::InMemory, path::Path as ObjectPath, ObjectStore, ObjectStoreExt, PutPayload,
};
use ds_storage::DataStore;
use futures::stream::BoxStream;

use crate::catalog::{self, Catalog, FileEntry};
use crate::reader::{DataSource, HttpHeaderRead, TiffMetadata};
use ds_storage::RangeRefusal;

pub(crate) const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../testdata/radar/radar_20260324T2315Z.tif"
);
const KEY: &str = "d/radar_20260324T2315Z.tif";

/// An in-memory store whose next `fail_ranges` range reads fail the way a
/// stalled S3 body does, counting range reads and whole-object GETs. Each
/// GET takes `delay`, and the store records how many were in flight at
/// most and which keys they read, in the order they started (#1042). Also
/// the failing store of `tile_fetch_failure_tests` (#1051).
#[derive(Debug, Default)]
pub(crate) struct FlakyStore {
    inner: InMemory,
    pub(crate) fail_ranges: AtomicUsize,
    /// Answer every range read the way object_store does when an HTTP
    /// origin ignores `Range`.
    ignore_ranges: std::sync::atomic::AtomicBool,
    delay: Duration,
    range_reads: AtomicUsize,
    full_gets: AtomicUsize,
    in_flight: AtomicUsize,
    peak: AtomicUsize,
    started: Mutex<Vec<String>>,
}

/// Counts one GET in flight until it finishes or is dropped.
struct InFlight<'a>(&'a AtomicUsize);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl FlakyStore {
    pub(crate) fn with_object(bytes: Vec<u8>) -> Arc<Self> {
        Self::with_objects(&[KEY.to_string()], bytes, Duration::ZERO)
    }

    /// `bytes` under each of `keys`, every GET taking `delay`.
    fn with_objects(keys: &[String], bytes: Vec<u8>, delay: Duration) -> Arc<Self> {
        let store = Self {
            delay,
            ..Self::default()
        };
        for key in keys {
            futures::executor::block_on(store.inner.put(
                &ObjectPath::from(key.as_str()),
                PutPayload::from(bytes.clone()),
            ))
            .unwrap();
        }
        Arc::new(store)
    }

    /// `(range reads, whole-object GETs)` so far.
    pub(crate) fn counts(&self) -> (usize, usize) {
        (
            self.range_reads.load(Ordering::SeqCst),
            self.full_gets.load(Ordering::SeqCst),
        )
    }

    /// The most GETs ever in flight at once.
    fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }

    /// The keys GET since the last call, in the order the GETs started.
    fn take_started(&self) -> Vec<String> {
        std::mem::take(&mut *self.started.lock().unwrap())
    }
}

impl std::fmt::Display for FlakyStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FlakyStore")
    }
}

#[async_trait::async_trait]
impl ObjectStore for FlakyStore {
    async fn get_opts(
        &self,
        path: &ObjectPath,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        if options.head {
            return self.inner.get_opts(path, options).await;
        }
        self.started.lock().unwrap().push(path.to_string());
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        let _in_flight = InFlight(&self.in_flight);
        match options.range {
            None => self.full_gets.fetch_add(1, Ordering::SeqCst),
            Some(_) => self.range_reads.fetch_add(1, Ordering::SeqCst),
        };
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        if options.range.is_some() {
            if self.ignore_ranges.load(Ordering::SeqCst) {
                return Err(object_store::Error::Generic {
                    store: "HTTP",
                    source: "Received non-partial response when range requested".into(),
                });
            }
            // One decrement per failed read, so concurrent reads fail
            // exactly `fail_ranges` of them; a count far above the reads a
            // test makes fails every read.
            let fail = self
                .fail_ranges
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok();
            if fail {
                return Err(object_store::Error::Generic {
                    store: "S3",
                    source: "error reading response body: operation timed out".into(),
                });
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

/// One poll's remote scan of prefix `d`, reusing `previous` the way
/// `do_scan` does.
fn poll(store: &DataStore, previous: &Catalog) -> Catalog {
    let matcher = FilenameMatcher::from_template("radar_%Y%m%dT%H%MZ.tif").unwrap();
    let index: HashMap<&Path, &FileEntry> = previous
        .entries
        .values()
        .map(|e| (e.path.as_path(), e))
        .collect();
    let (catalog, failed) = catalog::scan_remote(
        store,
        &[ObjectPath::from("d")],
        &ScanSpec::new(&matcher, "radar"),
        &index,
    )
    .unwrap();
    assert!(failed.is_empty());
    catalog
}

/// The single catalogued file's source, or `None` when nothing is.
fn only_source(catalog: &Catalog) -> Option<Arc<DataSource>> {
    assert!(catalog.entries.len() <= 1);
    let entry = catalog.entries.values().next()?;
    assert_eq!(entry.path, Path::new(KEY));
    Some(Arc::clone(entry.source().expect("a loaded entry")))
}

/// The fixture COG rewritten into a non-COG layout: IFD 0 copied past the
/// end of the image data and the header pointed at the copy, as a plain
/// GeoTIFF writer lays a file out. Every offset inside the IFD is absolute
/// into the unchanged prefix, so the whole file still parses; its first
/// `HEADER_READ_SIZE` bytes no longer hold an IFD.
fn ifd_at_end(mut tiff: Vec<u8>) -> Vec<u8> {
    assert_eq!(&tiff[..4], b"II*\0", "a little-endian classic TIFF");
    let ifd = u32::from_le_bytes(tiff[4..8].try_into().unwrap()) as usize;
    let entries = u16::from_le_bytes(tiff[ifd..ifd + 2].try_into().unwrap()) as usize;
    let ifd_bytes = tiff[ifd..ifd + 2 + 12 * entries + 4].to_vec();
    if tiff.len() % 2 == 1 {
        tiff.push(0);
    }
    let moved = tiff.len();
    assert!(moved > crate::reader::HEADER_READ_SIZE);
    tiff.extend_from_slice(&ifd_bytes);
    tiff[4..8].copy_from_slice(&u32::try_from(moved).unwrap().to_le_bytes());
    tiff
}

#[test]
fn storage_error_on_the_header_read_skips_the_file_until_the_next_poll() {
    let flaky = FlakyStore::with_object(std::fs::read(FIXTURE).unwrap());
    let store = DataStore::new(flaky.clone());

    // The header read itself fails, which is not proof of a non-COG.
    flaky.fail_ranges.store(1, Ordering::SeqCst);
    let first = poll(&store, &Catalog::empty());
    assert!(
        only_source(&first).is_none(),
        "a file whose header read failed must not be catalogued"
    );
    assert_eq!(
        flaky.counts(),
        (1, 0),
        "no full download after a storage error"
    );

    // The next poll reads the header again and catalogues it by range.
    let second = poll(&store, &first);
    let source = only_source(&second).expect("the next poll catalogues the file");
    assert!(
        matches!(*source, DataSource::Remote { .. }),
        "served by range reads, not from a download"
    );
    assert_eq!(flaky.counts(), (2, 0));
}

#[test]
fn header_read_reports_a_storage_error_apart_from_a_non_cog() {
    let cog = std::fs::read(FIXTURE).unwrap();
    let size = cog.len() as u64;
    let flaky = FlakyStore::with_object(cog);
    let store = DataStore::new(flaky.clone());
    let path = ObjectPath::from(KEY);

    flaky.fail_ranges.store(1, Ordering::SeqCst);
    assert!(TiffMetadata::from_header_read(&store, &path, size).is_err());
    assert!(TiffMetadata::from_header_read(&store, &path, size)
        .unwrap()
        .is_some());

    let not_cog = ifd_at_end(std::fs::read(FIXTURE).unwrap());
    let size = not_cog.len() as u64;
    let store = DataStore::new(FlakyStore::with_object(not_cog));
    assert!(TiffMetadata::from_header_read(&store, &path, size)
        .unwrap()
        .is_none());
}

#[test]
fn a_header_that_does_not_parse_still_takes_the_full_download() {
    let cog = std::fs::read(FIXTURE).unwrap();
    let expected = TiffMetadata::from_source(&DataSource::from_bytes(cog.clone())).unwrap();
    let flaky = FlakyStore::with_object(ifd_at_end(cog));
    let store = DataStore::new(flaky.clone());

    let catalog = poll(&store, &Catalog::empty());
    let source = only_source(&catalog).expect("the non-COG is catalogued");
    assert!(
        matches!(*source, DataSource::InMemory(_)),
        "served from the full download"
    );
    assert_eq!(flaky.counts(), (1, 1));
    let entry = catalog.entries.values().next().unwrap();
    let metadata = entry.metadata().unwrap();
    assert_eq!(
        (metadata.width, metadata.height, metadata.overviews.len()),
        (expected.width, expected.height, expected.overviews.len())
    );
}

#[test]
fn a_source_that_ignores_range_still_takes_the_full_download() {
    let cog = std::fs::read(FIXTURE).unwrap();
    let flaky = FlakyStore::with_object(cog);
    flaky.ignore_ranges.store(true, Ordering::SeqCst);
    let store = DataStore::new(flaky.clone());

    let catalog = poll(&store, &Catalog::empty());
    let source = only_source(&catalog).expect("the file is catalogued");
    assert!(
        matches!(*source, DataSource::InMemory(_)),
        "served from the full download"
    );
    assert_eq!(flaky.counts(), (1, 1));
}

/// The shipped object_store HTTP client against a real origin that ignores
/// `Range` and answers 200 with the whole object: its error is the one
/// `range_not_honoured` matches. Pins the match against the client itself,
/// not against the mock above, so a reworded error after a dependency bump
/// fails here instead of leaving such a source uncatalogued.
#[tokio::test(flavor = "multi_thread")]
async fn object_store_reports_an_origin_ignoring_range_as_range_not_honoured() {
    let bytes = std::fs::read(FIXTURE).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new().route(
        "/d/radar.tif",
        axum::routing::get(move || {
            let bytes = bytes.clone();
            async move { bytes }
        }),
    );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let (store, path) = ds_storage::build_http_store(&format!("{base}/d/radar.tif")).unwrap();
    let err = store
        .get_range(&path, 0..1024)
        .expect_err("a 200 answer to a range request is an error");
    assert!(catalog::range_not_honoured(&err), "unexpected error: {err}");
}

/// The STAC header read over direct HTTP (#1025, #1119), against an origin
/// with one path per answer. A header that parses is a COG; bytes that do
/// not, and a 200 for the whole object, take the full download; a 404, 403,
/// 416 or an object shorter than the listed size is a typed refusal; a 503
/// and a body cut short are failed reads, never a full download.
#[tokio::test(flavor = "multi_thread")]
async fn http_header_read_tells_a_failed_read_and_a_refusal_from_a_non_cog() {
    use axum::http::{header, HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    const SMALL: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../testdata/radar-tm35fin/radar_tm35_20260406T0640Z.tif"
    );
    let cog = std::fs::read(FIXTURE).unwrap();
    let size = cog.len() as u64;
    let small = std::fs::metadata(SMALL).unwrap().len();
    // A 206 of `bytes`' first `n` bytes, the `Range: bytes=0-(n-1)` asked.
    fn partial(bytes: &[u8], headers: &HeaderMap) -> Response {
        let range = headers[header::RANGE].to_str().unwrap();
        let last: usize = range.strip_prefix("bytes=0-").unwrap().parse().unwrap();
        let total = bytes.len();
        (
            StatusCode::PARTIAL_CONTENT,
            [(header::CONTENT_RANGE, format!("bytes 0-{last}/{total}"))],
            bytes[..=last].to_vec(),
        )
            .into_response()
    }
    let whole = cog.clone();
    let garbage = vec![0x55u8; cog.len()];
    let app = axum::Router::new()
        .route_service("/cog", tower_http::services::ServeFile::new(FIXTURE))
        .route_service("/small", tower_http::services::ServeFile::new(SMALL))
        .route(
            "/not-cog",
            axum::routing::get(
                move |headers: HeaderMap| async move { partial(&garbage, &headers) },
            ),
        )
        .route(
            "/ignores-range",
            axum::routing::get(move || async move { whole }),
        )
        .route(
            "/gone",
            axum::routing::get(|| async { StatusCode::NOT_FOUND }),
        )
        .route(
            "/forbidden",
            axum::routing::get(|| async { StatusCode::FORBIDDEN }),
        )
        .route(
            "/unsatisfiable",
            axum::routing::get(|| async { StatusCode::RANGE_NOT_SATISFIABLE }),
        )
        .route(
            "/unavailable",
            axum::routing::get(|| async { StatusCode::SERVICE_UNAVAILABLE }),
        )
        .route(
            "/cut-short",
            axum::routing::get(move || async move {
                let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = vec![
                    Ok(bytes::Bytes::from(vec![0u8; 1024])),
                    Err(std::io::Error::other("connection cut")),
                ];
                Response::builder()
                    .status(StatusCode::PARTIAL_CONTENT)
                    .header(header::CONTENT_LENGTH, crate::reader::HEADER_READ_SIZE)
                    .body(axum::body::Body::from_stream(futures::stream::iter(chunks)))
                    .unwrap()
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let http = reqwest::Client::new();
    let read = |path: &str, size: u64| {
        let (http, url) = (http.clone(), format!("{base}{path}"));
        async move { TiffMetadata::from_http_header_read(&http, &url, size).await }
    };

    assert!(matches!(
        read("/cog", size).await,
        Ok(HttpHeaderRead::Cog(..))
    ));
    assert!(matches!(
        read("/small", small).await,
        Ok(HttpHeaderRead::Cog(..))
    ));
    assert!(matches!(
        read("/not-cog", size).await,
        Ok(HttpHeaderRead::NotCog)
    ));
    assert!(matches!(read("/cog", 0).await, Ok(HttpHeaderRead::NotCog)));
    assert!(matches!(
        read("/ignores-range", size).await,
        Ok(HttpHeaderRead::RangeIgnored)
    ));
    for (path, listed, refusal) in [
        ("/gone", size, RangeRefusal::NotFound),
        ("/forbidden", size, RangeRefusal::Forbidden),
        ("/unsatisfiable", size, RangeRefusal::Truncated),
        // The listing says 4 KiB more than the object holds.
        ("/small", small + 4096, RangeRefusal::Truncated),
    ] {
        match read(path, listed).await {
            Ok(HttpHeaderRead::Refused(r)) => assert_eq!(r, refusal, "{path}"),
            other => panic!("{path}: expected {refusal:?}, got {other:?}"),
        }
    }
    for path in ["/unavailable", "/cut-short"] {
        assert!(read(path, size).await.is_err(), "{path}: a failed read");
    }
}

/// A scan whose metadata read budget is spent leaves new files to the next
/// poll without reading them, while a file it already catalogued stays
/// (#1011): a storage stall across many new files cannot hold one poll for
/// files × the background fetch budget.
#[test]
fn a_spent_read_budget_defers_new_files_to_the_next_poll() {
    let cog = std::fs::read(FIXTURE).unwrap();
    let flaky = FlakyStore::with_object(cog);
    let store = DataStore::new(flaky.clone());
    let matcher = FilenameMatcher::from_template("radar_%Y%m%dT%H%MZ.tif").unwrap();
    let scan = |previous: &Catalog, budget: std::time::Duration| {
        let index: HashMap<&Path, &FileEntry> = previous
            .entries
            .values()
            .map(|e| (e.path.as_path(), e))
            .collect();
        catalog::scan_remote_within(
            &store,
            &[ObjectPath::from("d")],
            &ScanSpec::new(&matcher, "radar"),
            &index,
            budget,
        )
        .unwrap()
        .0
    };

    let deferred = scan(&Catalog::empty(), std::time::Duration::ZERO);
    assert!(deferred.entries.is_empty(), "the new file waits");
    assert_eq!(flaky.counts(), (0, 0), "and nothing was read for it");

    let first = scan(&Catalog::empty(), std::time::Duration::from_secs(60));
    assert!(only_source(&first).is_some());
    let reused = scan(&first, std::time::Duration::ZERO);
    assert!(only_source(&reused).is_some(), "a catalogued file stays");
    assert_eq!(flaky.counts(), (1, 0), "without another read");
}

/// `count` keys of five-minute frames under prefix `d`, oldest first.
fn frame_keys(count: usize) -> Vec<String> {
    let first = chrono::DateTime::parse_from_rfc3339("2026-03-24T23:00:00Z").unwrap();
    (0..count)
        .map(|i| {
            let time = first + chrono::Duration::minutes(5 * i as i64);
            format!("d/radar_{}.tif", time.format("%Y%m%dT%H%MZ"))
        })
        .collect()
}

/// The catalogued keys, oldest first.
fn catalogued(catalog: &Catalog) -> Vec<String> {
    catalog
        .entries
        .values()
        .map(|entry| entry.path.to_str().unwrap().to_string())
        .collect()
}

/// `keys` newest first.
fn newest_first(keys: &[String]) -> Vec<String> {
    keys.iter().rev().cloned().collect()
}

/// A scan reads its new files' headers `SCAN_READ_CONCURRENCY` at a time,
/// newest first, never one after another and never more at once (#1042,
/// root Critical Rule 9): twelve 150 ms reads take about three rounds,
/// not twelve.
#[test]
fn a_scan_overlaps_its_header_reads_up_to_the_bound() {
    let keys = frame_keys(12);
    let delay = Duration::from_millis(150);
    let flaky = FlakyStore::with_objects(&keys, std::fs::read(FIXTURE).unwrap(), delay);
    let store = DataStore::new(flaky.clone());

    let started = Instant::now();
    let catalog = poll(&store, &Catalog::empty());
    let elapsed = started.elapsed();
    assert_eq!(catalogued(&catalog), keys);
    assert_eq!(flaky.counts(), (12, 0));
    assert_eq!(flaky.peak(), catalog::SCAN_READ_CONCURRENCY);
    assert!(
        elapsed < delay * 6,
        "{elapsed:?}: the header reads did not overlap"
    );
    assert_eq!(flaky.take_started(), newest_first(&keys));
}

/// A scan whose read budget runs out has catalogued the newest files and
/// leaves the oldest to the next poll (#1042): a client wants the latest
/// frame. The next poll reads only the files it left.
#[test]
fn a_spent_read_budget_leaves_the_oldest_files_to_the_next_poll() {
    let keys = frame_keys(8);
    let delay = Duration::from_millis(600);
    let flaky = FlakyStore::with_objects(&keys, std::fs::read(FIXTURE).unwrap(), delay);
    let store = DataStore::new(flaky.clone());
    let matcher = FilenameMatcher::from_template("radar_%Y%m%dT%H%MZ.tif").unwrap();
    let scan = |previous: &Catalog, budget: Duration| {
        let index: HashMap<&Path, &FileEntry> = previous
            .entries
            .values()
            .map(|e| (e.path.as_path(), e))
            .collect();
        catalog::scan_remote_within(
            &store,
            &[ObjectPath::from("d")],
            &ScanSpec::new(&matcher, "radar"),
            &index,
            budget,
        )
        .unwrap()
        .0
    };

    // Time for one round of four reads: the newest four finish at 600 ms,
    // the next four are cut off at 900 ms.
    let first = scan(&Catalog::empty(), Duration::from_millis(900));
    assert_eq!(catalogued(&first), keys[4..]);
    assert_eq!(flaky.take_started(), newest_first(&keys));

    let second = scan(&first, Duration::from_secs(60));
    assert_eq!(catalogued(&second), keys);
    assert_eq!(flaky.take_started(), newest_first(&keys[..4]));
}

/// Files a header read cannot serve are downloaded whole, newest first and
/// `SCAN_READ_CONCURRENCY` at a time, not one after another (#1042).
#[test]
fn full_downloads_run_newest_first_up_to_the_bound() {
    let keys = frame_keys(6);
    let not_cog = ifd_at_end(std::fs::read(FIXTURE).unwrap());
    let flaky = FlakyStore::with_objects(&keys, not_cog, Duration::from_millis(100));
    let store = DataStore::new(flaky.clone());

    let catalog = poll(&store, &Catalog::empty());
    assert_eq!(catalogued(&catalog), keys);
    assert!(catalog
        .entries
        .values()
        .all(|entry| matches!(**entry.source().unwrap(), DataSource::InMemory(_))));
    assert_eq!(flaky.counts(), (6, 6));
    assert_eq!(flaky.peak(), catalog::SCAN_READ_CONCURRENCY);
    let started = flaky.take_started();
    assert_eq!(started[..6], newest_first(&keys), "the header reads");
    assert_eq!(started[6..], newest_first(&keys), "the downloads");
}

/// #1011, #1055: the poll scan reads a header, and downloads a non-COG
/// whole, under the background budget. A GET whose body stalls past the
/// request budget still has its file catalogued, by range or from the
/// download; under the request budget the file would wait for the next
/// poll.
#[test]
fn the_header_read_and_the_full_download_take_the_background_budget() {
    use ds_storage::test_util::{run_stalled, StalledBody};
    let cog = std::fs::read(FIXTURE).unwrap();
    for (bytes, by_range) in [(ifd_at_end(cog.clone()), false), (cog, true)] {
        let stalled = StalledBody::with_objects([(KEY, bytes)]);
        let store = DataStore::new(stalled.clone());
        let catalog = run_stalled(&stalled, move || poll(&store, &Catalog::empty()));
        let source = only_source(&catalog).expect("the stalled file is catalogued");
        assert_eq!(
            matches!(*source, DataSource::Remote { .. }),
            by_range,
            "{source:?}"
        );
        // The header read, then for the non-COG the download.
        assert_eq!(stalled.stalled(), if by_range { 1 } else { 2 });
    }
}
