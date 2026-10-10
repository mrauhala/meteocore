//! A storage error on a remote COG's header read is not a non-COG (#1003):
//! the scan leaves the file out of the catalog, without downloading it
//! whole, and the next poll reads its header again. Only a header that
//! arrives but does not parse takes the full-download fallback.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use ds_storage::discovery::{FilenameMatcher, ScanSpec};
use ds_storage::object_store::{
    self, memory::InMemory, path::Path as ObjectPath, ObjectStore, ObjectStoreExt, PutPayload,
};
use ds_storage::DataStore;
use futures::stream::BoxStream;

use crate::catalog::{self, Catalog, FileEntry};
use crate::reader::{DataSource, TiffMetadata};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../testdata/radar/radar_20260324T2315Z.tif"
);
const KEY: &str = "d/radar_20260324T2315Z.tif";

/// An in-memory store whose next `fail_ranges` range reads fail the way a
/// stalled S3 body does, counting range reads and whole-object GETs.
#[derive(Debug, Default)]
struct FlakyStore {
    inner: InMemory,
    fail_ranges: AtomicUsize,
    /// Answer every range read the way object_store does when an HTTP
    /// origin ignores `Range`.
    ignore_ranges: std::sync::atomic::AtomicBool,
    range_reads: AtomicUsize,
    full_gets: AtomicUsize,
}

impl FlakyStore {
    fn with_object(bytes: Vec<u8>) -> Arc<Self> {
        let store = Self::default();
        futures::executor::block_on(
            store
                .inner
                .put(&ObjectPath::from(KEY), PutPayload::from(bytes)),
        )
        .unwrap();
        Arc::new(store)
    }

    /// `(range reads, whole-object GETs)` so far.
    fn counts(&self) -> (usize, usize) {
        (
            self.range_reads.load(Ordering::SeqCst),
            self.full_gets.load(Ordering::SeqCst),
        )
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
        if !options.head {
            if options.range.is_none() {
                self.full_gets.fetch_add(1, Ordering::SeqCst);
            } else {
                self.range_reads.fetch_add(1, Ordering::SeqCst);
                if self.ignore_ranges.load(Ordering::SeqCst) {
                    return Err(object_store::Error::Generic {
                        store: "HTTP",
                        source: "Received non-partial response when range requested".into(),
                    });
                }
                let fail = self
                    .fail_ranges
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                    .is_ok();
                if fail {
                    return Err(object_store::Error::Generic {
                        store: "S3",
                        source: "error reading response body: operation timed out".into(),
                    });
                }
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
