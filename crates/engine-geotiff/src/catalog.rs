use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use ds_core::error::DataServerError;
use ds_storage::discovery::{self, FilenameMatcher, PendingFile, RemoteFile, RemoteScan, ScanSpec};
use ds_storage::object_store::path::Path as ObjectPath;
use ds_storage::object_store::ObjectMeta;
use ds_storage::ObjectVersion;

use crate::reader::{DataSource, ObjectHealth, TiffMetadata};

/// Lightweight stub for STAC entries that haven't had their GeoTIFF metadata loaded yet.
#[derive(Debug, Clone)]
pub struct StacStub {
    pub bbox: Option<[f64; 4]>,
    pub asset_url: String,
}

/// The loading state of a file entry.
///
/// Non-STAC entries (local/remote) are always `Loaded` since metadata is parsed
/// during the scan. STAC entries start as `Stub` and transition to `Loaded` when
/// the poll cycle preloads their GeoTIFF metadata (#90) or a request fetches it
/// on demand.
#[derive(Debug, Clone)]
pub enum FileState {
    /// STAC stub: only STAC metadata available, GeoTIFF not yet loaded.
    /// `health` is what its metadata load learned about the asset (#1119),
    /// shared by every clone of the entry as a loaded source's is.
    Stub {
        stub: StacStub,
        health: ObjectHealth,
    },
    /// Fully loaded with GeoTIFF metadata and data source.
    Loaded {
        metadata: Arc<TiffMetadata>,
        source: Arc<DataSource>,
    },
}

/// An entry in the file catalog: one GeoTIFF file with a parsed timestamp.
///
/// `metadata` and `source` are wrapped in `Arc` so that cloning the catalog's
/// `BTreeMap` (required for COW semantics with `ArcSwap`) is cheap — just
/// pointer bumps instead of deep-cloning potentially large `DataSource` buffers.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: PathBuf,
    pub file_size: u64,
    /// File modification time captured at scan. `None` for remote/STAC entries
    /// (no filesystem mtime). Used together with `file_size` and `inode` to
    /// detect when a local file's bytes have been replaced.
    pub mtime: Option<std::time::SystemTime>,
    /// Inode number captured at scan (Unix only). `None` for remote/STAC
    /// entries and on platforms without `MetadataExt::ino()`. Closes the
    /// same-second atomic-rename gap that `mtime` alone can't catch on
    /// 1-second-resolution filesystems (HFS+/APFS/FAT/some NFS) — see #253
    /// round-3 finding.
    pub inode: Option<u64>,
    /// The version a remote object-store listing named for the object: its
    /// size, `Last-Modified` and ETag (#1055). `None` for local files and
    /// STAC items. The scan reuses the entry only for a listing that can
    /// name the same version ([`FileEntry::lists_as`]), so an object
    /// overwritten in place with other bytes, even of the same size, has
    /// its header read again.
    pub listed: Option<ObjectVersion>,
    pub state: FileState,
}

impl FileEntry {
    /// Create a fully loaded entry (local or remote scan).
    pub fn loaded(
        path: PathBuf,
        source: DataSource,
        metadata: TiffMetadata,
        file_size: u64,
        mtime: Option<std::time::SystemTime>,
        inode: Option<u64>,
    ) -> Self {
        Self {
            path,
            file_size,
            mtime,
            inode,
            listed: None,
            state: FileState::Loaded {
                metadata: Arc::new(metadata),
                source: Arc::new(source),
            },
        }
    }

    /// Create a stub entry from STAC metadata (no GeoTIFF data loaded yet).
    pub fn stac_stub(path: PathBuf, file_size: u64, stub: StacStub) -> Self {
        Self {
            path,
            file_size,
            mtime: None,
            inode: None,
            listed: None,
            state: FileState::Stub {
                stub,
                health: ObjectHealth::default(),
            },
        }
    }

    /// This entry, for an object a remote listing named as `version`
    /// ([`FileEntry::listed`]).
    pub(crate) fn listed_as(mut self, version: ObjectVersion) -> Self {
        self.listed = Some(version);
        self
    }

    /// Whether a remote listing's `object` can be the object this entry
    /// read ([`same_version`], #1055). The scan reuses the entry, without
    /// reading the header, only then.
    pub(crate) fn lists_as(&self, object: &ObjectMeta) -> bool {
        self.file_size == object.size
            && self
                .listed
                .as_ref()
                .is_none_or(|version| same_version(version, &ObjectVersion::from(object)))
    }

    /// Whether `other` holds the object this entry holds: the same path and
    /// size, and the same listed version, mtime and inode where both carry
    /// one. A timestep a poll finds holding another object was republished
    /// (#1055): a new object under another key won its timestamp, or the
    /// object was overwritten in place.
    pub(crate) fn same_object(&self, other: &FileEntry) -> bool {
        fn agree<T: PartialEq>(a: &Option<T>, b: &Option<T>) -> bool {
            match (a, b) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            }
        }
        self.path == other.path
            && self.file_size == other.file_size
            && match (&self.listed, &other.listed) {
                (Some(a), Some(b)) => same_version(a, b),
                _ => true,
            }
            && agree(&self.mtime, &other.mtime)
            && agree(&self.inode, &other.inode)
    }

    /// Whether this entry has full GeoTIFF metadata loaded.
    pub fn is_loaded(&self) -> bool {
        matches!(self.state, FileState::Loaded { .. })
    }

    /// Get metadata if loaded, None if stub.
    pub fn metadata(&self) -> Option<&Arc<TiffMetadata>> {
        match &self.state {
            FileState::Loaded { metadata, .. } => Some(metadata),
            FileState::Stub { .. } => None,
        }
    }

    /// Get source if loaded, None if stub.
    pub fn source(&self) -> Option<&Arc<DataSource>> {
        match &self.state {
            FileState::Loaded { source, .. } => Some(source),
            FileState::Stub { .. } => None,
        }
    }

    /// Whether a read found this entry's remote object gone: a tile read
    /// after the scan (#1074), or a STAC item's metadata load before its
    /// header was ever read (#1119). A 404, or a range past its end. Every
    /// catalog accessor skips such an entry, and the next catalog swap
    /// removes it ([`crate::gone`]).
    pub fn is_gone(&self) -> bool {
        self.health().is_some_and(|health| health.gone().is_some())
    }

    /// What reads learned about the entry's remote object: its tile reads'
    /// once loaded (#1074), its metadata load's while a STAC stub (#1119).
    /// `None` for a local or in-memory source.
    pub(crate) fn health(&self) -> Option<&ObjectHealth> {
        match &self.state {
            FileState::Loaded { source, .. } => source.health(),
            FileState::Stub { health, .. } => Some(health),
        }
    }

    /// Get the STAC stub if this is a stub entry.
    pub fn stac_stub_info(&self) -> Option<&StacStub> {
        match &self.state {
            FileState::Stub { stub, .. } => Some(stub),
            FileState::Loaded { .. } => None,
        }
    }

    /// Promote a stub to loaded state with metadata and source.
    pub fn set_loaded(&mut self, metadata: Arc<TiffMetadata>, source: Arc<DataSource>) {
        self.state = FileState::Loaded { metadata, source };
    }

    /// Get mutable access to metadata (for applying overrides). Returns None if stub.
    pub fn metadata_mut(&mut self) -> Option<&mut Arc<TiffMetadata>> {
        match &mut self.state {
            FileState::Loaded { metadata, .. } => Some(metadata),
            FileState::Stub { .. } => None,
        }
    }
}

/// Whether two listings can name the same version of an object (#1055): the
/// same size, and the same ETag where both name one, else the same
/// `Last-Modified` where both name one. The ETag comes first because it
/// names the bytes: an object written again with the same bytes, as a
/// mirror that uploads its last hour again on every run does, keeps its
/// ETag on S3 and stays the same object, so its new `Last-Modified` neither
/// costs a header read nor moves `content_version`.
fn same_version(a: &ObjectVersion, b: &ObjectVersion) -> bool {
    a.size == b.size
        && match (&a.e_tag, &b.e_tag) {
            (Some(a), Some(b)) => a == b,
            _ => match (a.last_modified, b.last_modified) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            },
        }
}

/// Immutable snapshot of discovered GeoTIFF files, sorted by timestamp.
#[derive(Debug, Clone)]
pub struct Catalog {
    pub entries: BTreeMap<DateTime<Utc>, FileEntry>,
    pub temporal_extent: Option<(DateTime<Utc>, DateTime<Utc>)>,
    pub spatial_extent: Option<[f64; 4]>,
}

impl Catalog {
    pub fn empty() -> Self {
        Catalog {
            entries: BTreeMap::new(),
            temporal_extent: None,
            spatial_extent: None,
        }
    }

    /// Recompute temporal and spatial extents from current entries.
    pub fn recompute_extents(&mut self) {
        self.temporal_extent = match (self.entries.keys().next(), self.entries.keys().next_back()) {
            (Some(&first), Some(&last)) => Some((first, last)),
            _ => None,
        };
        self.spatial_extent = compute_spatial_union(self.entries.values());
    }

    /// The entries no read has found gone (#1074, #1119), oldest first.
    pub fn live(&self) -> impl DoubleEndedIterator<Item = (&DateTime<Utc>, &FileEntry)> {
        self.entries.iter().filter(|(_, entry)| !entry.is_gone())
    }

    /// The timestep `get_raster_tile` renders for a requested `time` (#507):
    /// the newest entry not after `time`, falling back to the earliest entry
    /// for pre-range requests; `None` request ⇒ the newest entry. Entries
    /// whose object a read found gone are skipped (#1074). Returns `None`
    /// only when no entry is left. This is the ONLY implementation of
    /// that selection — `MapEngine::resolve_time` (the cache-key authority)
    /// and the render path both call it, so they cannot drift.
    pub fn select_timestamp(&self, time: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
        match time {
            Some(t) => self
                .entries
                .range(..=t)
                .rev()
                .find(|(_, entry)| !entry.is_gone())
                .or_else(|| self.live().next())
                .map(|(ts, _)| *ts),
            None => self.live().next_back().map(|(ts, _)| *ts),
        }
    }

    /// The entries, newest first, at timesteps `previous` did not hold or
    /// held another object at ([`FileEntry::same_object`]): what a poll
    /// discovered, a frame republished under a catalogued timestep included
    /// (#1055). The STAC preload and the pre-warm take their frames from it.
    pub(crate) fn fresh_since<'a>(
        &'a self,
        previous: &'a Catalog,
    ) -> impl Iterator<Item = (&'a DateTime<Utc>, &'a FileEntry)> + 'a {
        self.entries.iter().rev().filter(|(timestamp, entry)| {
            previous
                .entries
                .get(timestamp)
                .is_none_or(|before| !before.same_object(entry))
        })
    }

    /// Carry into this catalog, which a poll's scan built from `current`,
    /// the loads that finished during the scan: an entry this catalog holds
    /// as a stub takes the loaded state `current` has at its timestep, only
    /// when both name the same object ([`FileEntry::same_object`]: for a
    /// STAC item its asset href and `file:size`, as the gone screen keys
    /// it). A stub of another object at that timestep, an item republished
    /// under it, stays a stub: the old object's header and tile layout
    /// would serve stale tiles, and the stub loads afresh (#1055). Cheap:
    /// the states are `Arc`s.
    pub(crate) fn keep_loads_from(&mut self, current: &Catalog) {
        for (timestamp, before) in &current.entries {
            if let Some(entry) = self.entries.get_mut(timestamp) {
                if before.is_loaded() && !entry.is_loaded() && entry.same_object(before) {
                    entry.state = before.state.clone();
                }
            }
        }
    }

    /// The timesteps both `previous` and this catalog hold, each with
    /// another object here (#1055), newest first: frames republished since
    /// `previous`, whose renders were cached from the object they replaced.
    pub(crate) fn republished_since(&self, previous: &Catalog) -> Vec<DateTime<Utc>> {
        self.entries
            .iter()
            .rev()
            .filter(|(timestamp, entry)| {
                previous
                    .entries
                    .get(timestamp)
                    .is_some_and(|before| !before.same_object(entry))
            })
            .map(|(timestamp, _)| *timestamp)
            .collect()
    }

    /// Trim to keep only the most recent `max` entries by timestamp.
    pub fn trim_to_latest(&mut self, max: usize) {
        if self.entries.len() <= max {
            return;
        }
        let keep_from = self.entries.keys().rev().nth(max - 1).copied();
        if let Some(cutoff) = keep_from {
            self.entries = self.entries.split_off(&cutoff);
        }
        self.recompute_extents();
    }
}

/// Compute the union bounding box across all file entries: each loaded
/// file's grid extent, seam- and pole-aware (`GeoTransform::bbox_with_poles`,
/// #1194), or a STAC stub's bbox, united across the antimeridian
/// (`ds_core::geo::crs84_union`). A grid crossing it is `west > east`, and
/// one holding a pole reaches it at every longitude. Returns None if there
/// are no entries, or none has an extent.
fn compute_spatial_union<'a>(entries: impl Iterator<Item = &'a FileEntry>) -> Option<[f64; 4]> {
    ds_core::geo::crs84_union(entries.filter_map(|entry| match &entry.state {
        FileState::Loaded { metadata, .. } => {
            ds_core::geo::crs84_extent(metadata.geo_transform.bbox_with_poles())
        }
        FileState::Stub { stub, .. } => stub.bbox,
    }))
}

/// Scan a directory for GeoTIFF files matching a filename pattern.
///
/// Lists and matches with the shared catalog scan
/// ([`discovery::scan_local`]): regular files only, `exclude_patterns`
/// dropped first, one per timestamp (the lexicographically-last path wins). Returns a new Catalog containing all
/// valid files. Files that fail to parse are logged and skipped.
///
/// `existing` provides a path-based index of entries already in the catalog.
/// Files with unchanged size reuse their cached metadata (no re-parse).
pub fn scan_directory(
    dir: &Path,
    matcher: &FilenameMatcher,
    exclude_patterns: &[String],
    pending: &mut BTreeMap<PathBuf, PendingFile>,
    existing: &HashMap<&Path, &FileEntry>,
) -> Result<Catalog, DataServerError> {
    // Excluded names are dropped inside the scan, before the dedup, so an
    // excluded partial upload never displaces its finished file. Symlinks
    // stay skipped: the reuse test below compares the directory entry's own
    // size, mtime and inode, which a symlink's target can change under.
    let label = dir.display().to_string();
    let spec = ScanSpec {
        exclude: exclude_patterns,
        ..ScanSpec::new(matcher, &label)
    };
    let files = discovery::scan_local(dir, &spec).map_err(|e| {
        DataServerError::Engine(format!("Cannot read directory {}: {e}", dir.display()))
    })?;

    let mut entries = BTreeMap::new();

    for file in files {
        let datetime = file.time;
        let path = file.path;

        // File size, mtime and inode from the scan's one stat. mtime is
        // `Option<SystemTime>` because some filesystems (FAT, exotic NFS
        // configs) don't expose it; inode is Unix-only. Together they catch
        // the same-second atomic-rename case that mtime alone misses on
        // 1-second-resolution filesystems (#253 round-3 finding).
        let dir_meta = file.metadata;
        let file_size = dir_meta.len();
        let current_mtime = dir_meta.modified().ok();
        #[cfg(unix)]
        let current_inode = {
            use std::os::unix::fs::MetadataExt;
            Some(dir_meta.ino())
        };
        #[cfg(not(unix))]
        let current_inode: Option<u64> = None;

        // File readiness: check size stability for genuinely NEW files only.
        // Files already in the catalog (existing) skip the readiness check.
        // The shared `PendingFile` settles a file once its size is unchanged
        // on two polls after the first; a size change restarts the count.
        let existing_entry = existing.get(path.as_path());

        if existing_entry.is_none() {
            if let Some(prev) = pending.get_mut(&path) {
                if !prev.observe(ObjectVersion::sized(file_size)) {
                    continue;
                }
                pending.remove(&path);
            } else if !existing.is_empty() {
                // Genuinely new file during a poll cycle — add to pending, skip this cycle
                pending.insert(
                    path.clone(),
                    PendingFile::new(ObjectVersion::sized(file_size)),
                );
                continue;
            }
            // else: initial scan (existing empty) — accept immediately
        }

        // Reuse the prior entry's `DataSource` (and its warm mmap cache from
        // #204) when the file is unchanged — otherwise the OnceLock would be
        // replaced every poll cycle and the mmap re-issued on the next render.
        //
        // The unchanged-test compares `file_size` + `mtime` + `inode`. Size
        // alone misses a same-byte-count atomic replacement (#253 round-2);
        // mtime alone misses the same-second case on 1-second-resolution
        // filesystems because `rename(2)` doesn't bump mtime — only ctime
        // and the parent dir's mtime (#253 round-3). Inode comparison closes
        // that gap on Unix: every atomic rename produces a new inode number.
        // If any side's mtime or inode is `None` (unusual filesystem,
        // non-Unix), we conservatively rebuild the source — costs one extra
        // mmap, never serves stale data.
        let cached_unchanged = existing_entry
            .filter(|e| {
                e.file_size == file_size
                    && match (e.mtime, current_mtime) {
                        (Some(prev), Some(now)) => prev == now,
                        _ => false,
                    }
                    && match (e.inode, current_inode) {
                        (Some(prev), Some(now)) => prev == now,
                        // On platforms where we never have inode info, fall
                        // back to size+mtime only.
                        (None, None) => true,
                        _ => false,
                    }
            })
            .and_then(|e| e.source().map(|s| (e, s)));

        let (source, metadata) = if let Some((entry, old_source)) = cached_unchanged {
            // `cached_unchanged` is only `Some` when `entry.source()` is
            // `Some`, which only happens for `FileState::Loaded { metadata,
            // source }`. `FileState::Loaded` always carries both fields
            // together, so `metadata()` here is guaranteed `Some`.
            let meta_arc = entry
                .metadata()
                .expect("Loaded entry always carries metadata alongside source");
            ((**old_source).clone(), (**meta_arc).clone())
        } else {
            let source = DataSource::from_path(&path);
            match TiffMetadata::from_source(&source) {
                Ok(m) => (source, m),
                Err(e) => {
                    tracing::warn!("Skipping {}: {e}", path.display());
                    continue;
                }
            }
        };

        entries.insert(
            datetime,
            FileEntry::loaded(
                path,
                source,
                metadata,
                file_size,
                current_mtime,
                current_inode,
            ),
        );
    }

    // Clean pending files that no longer exist in the directory
    pending.retain(|p, _| p.exists());

    // Compute extents
    let temporal_extent = match (entries.keys().next(), entries.keys().next_back()) {
        (Some(&first), Some(&last)) => Some((first, last)),
        _ => None,
    };

    let spatial_extent = compute_spatial_union(entries.values());

    Ok(Catalog {
        entries,
        temporal_extent,
        spatial_extent,
    })
}

/// Maximum file size for remote downloads (50 MB).
/// `u64` to match `ObjectMeta::size`, which object_store widened from `usize`
/// in 0.14 so 32-bit targets can still describe large objects.
pub(crate) const MAX_REMOTE_FILE_SIZE: u64 = 50 * 1024 * 1024;

/// Reads in flight while a remote scan reads new files' metadata (#1042),
/// as many as the STAC preload keeps: enough to overlap round trips
/// without fanning a backlog out at the store.
pub(crate) const SCAN_READ_CONCURRENCY: usize = 4;

/// New files whose headers one batch reads before they are parsed (#1042):
/// a backlog holds at most 32 headers of 512 KiB at a time.
const SCAN_READ_BATCH: usize = 32;

/// A prefix whose LIST failed during [`scan_remote`], with the error.
pub type FailedPrefix = (ObjectPath, DataServerError);

/// Scan a remote object store's prefixes for GeoTIFF files matching a
/// filename pattern.
///
/// Lists and matches with the shared catalog scan
/// ([`discovery::scan_remote`]) under `spec`: the prefixes are listed
/// concurrently, at most [`discovery::MAX_CONCURRENT_LISTS`] at a time, and
/// the matches are filtered by `spec.exclude`, windowed, deduplicated per
/// timestamp and capped before any metadata is read. Objects over
/// [`MAX_REMOTE_FILE_SIZE`] are skipped. `spec.label` is the collection id
/// the log lines name.
///
/// Uses COG-style byte-range reads to fetch only the IFD metadata (the first
/// 512 KB) instead of downloading entire files. Falls back to full download
/// only if the header bytes do not parse as a COG (e.g., non-COG layout or
/// unsupported compression). A storage error on the header read instead
/// leaves the file out of this catalog, so the next scan retries it (#1003).
///
/// `existing` provides a path-based index of entries already in the catalog.
/// A file listed with the version its entry read reuses that entry, without
/// a header read ([`FileEntry::lists_as`]): the same size, and the same ETag
/// or, without one, the same `Last-Modified`, where the store lists them.
/// Any other version is a new object and is read again (#1055).
///
/// Returns the catalog and the prefixes whose LIST failed; whether a
/// failure fails the scan is the caller's call.
pub fn scan_remote(
    store: &ds_storage::DataStore,
    prefixes: &[ObjectPath],
    spec: &ScanSpec<'_>,
    existing: &HashMap<&Path, &FileEntry>,
) -> Result<(Catalog, Vec<FailedPrefix>), DataServerError> {
    scan_remote_within(
        store,
        prefixes,
        spec,
        existing,
        ds_storage::DataStore::BACKGROUND_TIMEOUT,
    )
}

/// [`scan_remote`] with `read_budget`: the time one scan may spend reading
/// new files' metadata before it defers the rest to the next poll. Each
/// read has the background fetch budget (#1011), so without a bound a
/// storage stall across many new files would hold the poll for files ×
/// that budget. The reads go newest file first, [`SCAN_READ_CONCURRENCY`]
/// at a time, so the files a spent budget defers are the oldest (#1042).
/// Already-catalogued files never read.
pub(crate) fn scan_remote_within(
    store: &ds_storage::DataStore,
    prefixes: &[ObjectPath],
    spec: &ScanSpec<'_>,
    existing: &HashMap<&Path, &FileEntry>,
    read_budget: std::time::Duration,
) -> Result<(Catalog, Vec<FailedPrefix>), DataServerError> {
    let collection_id = spec.label;
    let spec = ScanSpec {
        max_size: Some(MAX_REMOTE_FILE_SIZE),
        ..*spec
    };
    let RemoteScan {
        entries: files,
        prefixes: reports,
    } = discovery::scan_remote(store, prefixes, &spec)?;

    let mut failed = Vec::new();
    for report in reports {
        let listed = match report.listed {
            Ok(listed) => listed,
            Err(e) => {
                failed.push((report.prefix, e));
                continue;
            }
        };
        if spec.time_filter.is_some() {
            tracing::info!(
                "[{}] Prefix '{}': {} listed, {} within time window",
                collection_id,
                report.prefix,
                listed,
                report.kept
            );
        } else {
            tracing::info!(
                "[{}] Prefix '{}': {} listed, {} matching",
                collection_id,
                report.prefix,
                listed,
                report.kept
            );
        }
    }

    Ok((
        load_remote(store, files, existing, collection_id, read_budget),
        failed,
    ))
}

/// Build the catalog of the files a remote scan kept, reading each new
/// one's metadata (#1042): newest first, in batches of [`SCAN_READ_BATCH`],
/// until `read_budget` runs out.
fn load_remote(
    store: &ds_storage::DataStore,
    files: Vec<RemoteFile>,
    existing: &HashMap<&Path, &FileEntry>,
    collection_id: &str,
    read_budget: std::time::Duration,
) -> Catalog {
    let until = std::time::Instant::now() + read_budget;
    let mut entries = BTreeMap::new();

    // Reuse the entry of an object listed as the version it read (#1055).
    let mut new_files = Vec::new();
    for file in &files {
        match existing.get(Path::new(file.key())) {
            Some(entry) if entry.lists_as(&file.object) => {
                entries.insert(file.time, (*entry).clone());
            }
            _ => new_files.push(file),
        }
    }

    // Newest first, so the files a spent budget leaves to the next poll are
    // the oldest: a client wants the latest timestep.
    new_files.sort_by_key(|file| std::cmp::Reverse(file.time));
    let deferred: usize = new_files
        .chunks(SCAN_READ_BATCH)
        .map(|batch| read_new_files(store, batch, collection_id, until, &mut entries))
        .sum();
    if deferred > 0 {
        tracing::warn!(
            "[{}] {deferred} new file(s) left to the next poll: this scan spent over {}s reading remote metadata",
            collection_id,
            read_budget.as_secs()
        );
    }

    let temporal_extent = match (entries.keys().next(), entries.keys().next_back()) {
        (Some(&first), Some(&last)) => Some((first, last)),
        _ => None,
    };

    let spatial_extent = compute_spatial_union(entries.values());

    Catalog {
        entries,
        temporal_extent,
        spatial_extent,
    }
}

/// Read the metadata of `files`, new to the catalog and newest first, into
/// `entries`: each header by one range read, [`SCAN_READ_CONCURRENCY`] in
/// flight, then the files a header cannot serve whole. A header read that
/// has not finished by `until` is cut off, and nothing starts after it.
/// Returns how many files that left to the next poll.
fn read_new_files(
    store: &ds_storage::DataStore,
    files: &[&RemoteFile],
    collection_id: &str,
    until: std::time::Instant,
    entries: &mut BTreeMap<DateTime<Utc>, FileEntry>,
) -> usize {
    if std::time::Instant::now() >= until {
        return files.len();
    }
    // An empty object has no header to read: it goes to the full download,
    // as a header that does not parse does.
    let mut whole = Vec::new();
    let mut headed = Vec::new();
    let mut reads = Vec::new();
    for &file in files {
        match TiffMetadata::header_range(file.object.size) {
            Some(range) => {
                reads.push((file.object.location.clone(), range));
                headed.push(file);
            }
            None => whole.push(file),
        }
    }
    // Only the poll scan (and engine construction) reads headers before
    // cataloguing, so a stalled attempt may retry under the background
    // budget (#1011).
    let headers = match store.get_range_many(
        &reads,
        SCAN_READ_CONCURRENCY,
        ds_storage::FetchBudget::Background,
        until,
    ) {
        Ok(headers) => headers,
        Err(e) => {
            tracing::warn!(
                "[{}] {} header read(s) failed, retrying next poll: {e}",
                collection_id,
                reads.len()
            );
            return 0;
        }
    };

    let mut deferred = 0;
    for (file, header) in headed.into_iter().zip(headers) {
        let key = file.key();
        let file_size = file.object.size;
        match header {
            // The budget ran out before this read finished.
            None => deferred += 1,
            Some(Ok(bytes)) => {
                match TiffMetadata::from_header_bytes(bytes, &file.object.location) {
                    Some((metadata, tile_info)) => {
                        tracing::debug!("[{}] {} — range read OK", collection_id, key);
                        let source = DataSource::remote(
                            store.clone(),
                            file.object.location.clone(),
                            tile_info,
                        );
                        entries.insert(
                            file.time,
                            FileEntry::loaded(
                                PathBuf::from(key),
                                source,
                                metadata,
                                file_size,
                                None,
                                None,
                            )
                            .listed_as(ObjectVersion::from(&file.object)),
                        );
                    }
                    // The header arrived but is not a COG: download it whole below.
                    None => {
                        tracing::warn!(
                            "[{}] {} — header is not a COG, falling back to full download ({}). \
                         Convert to COG for faster serving.",
                            collection_id,
                            key,
                            super::format_bytes(file_size)
                        );
                        whole.push(file);
                    }
                }
            }
            // A source that ignores Range answers the header read with the
            // whole object, which object_store rejects. Only a full download
            // can serve it, as before #1003.
            Some(Err(e)) if range_not_honoured(&e) => {
                tracing::warn!(
                    "[{}] {} — the source ignores range requests, falling back to full download ({})",
                    collection_id,
                    key,
                    super::format_bytes(file_size)
                );
                whole.push(file);
            }
            // The storage failed, not the format (#1003): a full download
            // would go to the same storage and could stall the same way.
            // The file stays out of this catalog, so the next scan retries
            // its header read.
            Some(Err(e)) => {
                tracing::warn!(
                    "[{}] {} — header read failed, retrying next poll: {e}",
                    collection_id,
                    key
                );
            }
        }
    }

    // Newest first again: the empty objects went in ahead of the rest.
    whole.sort_by_key(|file| std::cmp::Reverse(file.time));
    deferred + download_whole(store, &whole, collection_id, until, entries)
}

/// The fallback of [`read_new_files`]: download `files` whole, newest first,
/// [`SCAN_READ_CONCURRENCY`] at a time, and parse them into `entries`. Full
/// downloads are much slower and cost more on S3. Convert to COG:
/// gdal_translate -of COG input.tif output.tif
///
/// No group of downloads starts after `until`, but one that started runs
/// to its end: the poll runtime (or engine construction) waits on it, not
/// a request, and the background budget lets a stalled attempt retry
/// (#1011). Returns how many files that left to the next poll.
fn download_whole(
    store: &ds_storage::DataStore,
    files: &[&RemoteFile],
    collection_id: &str,
    until: std::time::Instant,
    entries: &mut BTreeMap<DateTime<Utc>, FileEntry>,
) -> usize {
    for (started, group) in files.chunks(SCAN_READ_CONCURRENCY).enumerate() {
        if std::time::Instant::now() >= until {
            return files.len() - started * SCAN_READ_CONCURRENCY;
        }
        let paths: Vec<ObjectPath> = group.iter().map(|f| f.object.location.clone()).collect();
        let downloads = match store.get_many_with_budget(
            &paths,
            SCAN_READ_CONCURRENCY,
            None,
            ds_storage::FetchBudget::Background,
        ) {
            Ok(downloads) => downloads,
            Err(e) => {
                tracing::warn!(
                    "[{}] Failed to download {} file(s): {e}",
                    collection_id,
                    group.len()
                );
                continue;
            }
        };
        for (file, data) in group.iter().zip(downloads) {
            let key = file.key();
            let data = match data {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!("[{}] Failed to download {}: {e}", collection_id, key);
                    continue;
                }
            };
            let source = DataSource::from_bytes(data);
            let metadata = match TiffMetadata::from_source(&source) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("[{}] Skipping {}: {e}", collection_id, key);
                    continue;
                }
            };
            entries.insert(
                file.time,
                FileEntry::loaded(
                    PathBuf::from(key),
                    source,
                    metadata,
                    file.object.size,
                    None,
                    None,
                )
                .listed_as(ObjectVersion::from(&file.object)),
            );
        }
    }
    0
}

/// Whether a header range read failed because the source does not honour
/// `Range`. object_store reports it in two ways: its HTTP store as
/// "Range request not supported by <path>" (`NotSupported`, pinned against
/// a real origin in `remote_header_tests`), its other clients as
/// "Received non-partial response when range requested" (`NotPartial`).
/// ds-storage flattens object_store errors to text, so this matches both.
pub(crate) fn range_not_honoured(e: &DataServerError) -> bool {
    let text = e.to_string();
    text.contains("Range request not supported")
        || text.contains("non-partial response when range requested")
}

/// Create a catalog seeded from STAC collection extent — no items fetched.
///
/// This is called at engine startup. The catalog has no entries but carries
/// the spatial and temporal extent from the STAC collection metadata.
/// Items are fetched on-demand when queries arrive.
pub fn init_stac_from_extent(extent: &crate::stac::StacExtent) -> Catalog {
    let temporal_extent = extent.temporal_start.map(|start| {
        let end = extent.temporal_end.unwrap_or_else(Utc::now);
        (start, end)
    });

    Catalog {
        entries: BTreeMap::new(),
        temporal_extent,
        spatial_extent: extent.spatial_bbox,
    }
}

/// Fetch STAC items for a specific datetime range and merge into the catalog.
///
/// Called on-demand from query path when entries are needed for a time range.
/// Creates lightweight stub entries — no GeoTIFF downloads.
/// Existing entries (both stubs and loaded) are preserved.
pub fn fetch_stac_range(
    client: &crate::stac::StacClient,
    existing_catalog: &Catalog,
    time_range: (chrono::DateTime<Utc>, chrono::DateTime<Utc>),
    collection_id: &str,
) -> Result<Catalog, DataServerError> {
    let items = client.fetch_items(Some(time_range), None)?;
    Ok(merge_stac_range(
        existing_catalog,
        &items,
        time_range,
        collection_id,
    ))
}

/// Merge the STAC items listed for `time_range` into a copy of
/// `existing_catalog` as stub entries, the second half of
/// [`fetch_stac_range`] for a caller that lists the items itself (an EDR
/// query's bounded listing, #1075). Existing entries are preserved.
pub fn merge_stac_range(
    existing_catalog: &Catalog,
    items: &[crate::stac::StacItem],
    time_range: (chrono::DateTime<Utc>, chrono::DateTime<Utc>),
    collection_id: &str,
) -> Catalog {
    tracing::info!(
        "[{}] STAC: fetched {} items for range {}/{}",
        collection_id,
        items.len(),
        time_range.0.format("%Y-%m-%dT%H:%MZ"),
        time_range.1.format("%Y-%m-%dT%H:%MZ"),
    );

    // Start with existing entries
    let mut entries = existing_catalog.entries.clone();

    for item in items {
        // Skip if we already have this timestamp
        if entries.contains_key(&item.datetime) {
            continue;
        }

        let pseudo_path = PathBuf::from(&item.asset_url);
        let file_size = item.file_size.unwrap_or(0);

        entries.insert(
            item.datetime,
            FileEntry::stac_stub(
                pseudo_path,
                file_size,
                StacStub {
                    bbox: item.bbox,
                    asset_url: item.asset_url.clone(),
                },
            ),
        );
    }

    let temporal_extent = existing_catalog.temporal_extent;
    let spatial_extent = existing_catalog.spatial_extent;

    Catalog {
        entries,
        temporal_extent,
        spatial_extent,
    }
}

/// Poll STAC for new items since the latest catalog entry.
///
/// Used by the poll loop to pick up newly published data.
/// Only fetches items newer than the most recent existing entry.
pub fn poll_stac_latest(
    client: &crate::stac::StacClient,
    existing_catalog: &Catalog,
    collection_id: &str,
) -> Result<Catalog, DataServerError> {
    // Refresh extent (temporal end may have advanced)
    let extent = client.fetch_extent().unwrap_or_else(|e| {
        tracing::warn!("[{}] STAC extent refresh failed: {}", collection_id, e);
        crate::stac::StacExtent {
            spatial_bbox: existing_catalog.spatial_extent,
            temporal_start: existing_catalog.temporal_extent.map(|(s, _)| s),
            temporal_end: None,
        }
    });

    // Fetch items newer than our latest entry
    let since = existing_catalog
        .entries
        .keys()
        .next_back()
        .map(|latest| *latest + chrono::Duration::seconds(1))
        .unwrap_or_else(|| Utc::now() - chrono::Duration::hours(1));

    let items = client.fetch_items(Some((since, Utc::now())), None)?;

    let new_count = items.len();
    if new_count > 0 {
        tracing::info!(
            "[{}] STAC poll: {} new items (total: {})",
            collection_id,
            new_count,
            existing_catalog.entries.len() + new_count
        );
    }

    let mut entries = existing_catalog.entries.clone();

    for item in &items {
        if entries.contains_key(&item.datetime) {
            continue;
        }
        let pseudo_path = PathBuf::from(&item.asset_url);
        let file_size = item.file_size.unwrap_or(0);
        entries.insert(
            item.datetime,
            FileEntry::stac_stub(
                pseudo_path,
                file_size,
                StacStub {
                    bbox: item.bbox,
                    asset_url: item.asset_url.clone(),
                },
            ),
        );
    }

    // Update temporal extent from collection metadata
    let temporal_extent = extent.temporal_start.map(|start| {
        let end = extent.temporal_end.unwrap_or_else(Utc::now);
        (start, end)
    });

    Ok(Catalog {
        entries,
        temporal_extent,
        spatial_extent: extent.spatial_bbox.or(existing_catalog.spatial_extent),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::{DataSource, TiffMetadata};
    use ds_core::geo::{Crs, GeoTransform};
    use ds_storage::discovery::is_excluded;

    #[test]
    fn exclude_patterns() {
        assert!(is_excluded("data.tmp", &["*.tmp".into()]));
        assert!(is_excluded("data.part", &["*.part".into()]));
        assert!(is_excluded(".hidden", &[".*".into()]));
        assert!(!is_excluded(
            "radar_20240101T0000Z.tif",
            &["*.tmp".into(), "*.part".into()]
        ));
    }

    fn dummy_metadata() -> TiffMetadata {
        TiffMetadata {
            width: 100,
            height: 100,
            tile_width: 256,
            tile_height: 256,
            tiles_across: 1,
            tiles_down: 1,
            samples_per_pixel: 1,
            geo_transform: GeoTransform {
                origin_x: 0.0,
                origin_y: 0.0,
                pixel_width: 0.01,
                pixel_height: -0.01,
                width: 100,
                height: 100,
                crs: Crs::Wgs84,
            },
            nodata: None,
            scale: None,
            offset: None,
            overviews: vec![],
            epsg: None,
        }
    }

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn catalog_with(times: &[&str]) -> Catalog {
        let mut c = Catalog::empty();
        for t in times {
            let path = PathBuf::from(format!("/tmp/{t}.tif"));
            let source = DataSource::from_path(&path);
            c.entries.insert(
                ts(t),
                FileEntry::loaded(path, source, dummy_metadata(), 1, None, None),
            );
        }
        c
    }

    /// The render-path / cache-key time selection (#507). The
    /// "not-yet-ingested time snaps to the previous entry" case is the exact
    /// poison window the resolved-timestep cache keys exist to close.
    #[test]
    fn select_timestamp_selection_rules() {
        let c = catalog_with(&["2026-07-11T20:10:00Z", "2026-07-11T20:15:00Z"]);
        // Exact hit.
        assert_eq!(
            c.select_timestamp(Some(ts("2026-07-11T20:15:00Z"))),
            Some(ts("2026-07-11T20:15:00Z"))
        );
        // Not-yet-ingested / between steps → latest-not-after.
        assert_eq!(
            c.select_timestamp(Some(ts("2026-07-11T20:20:00Z"))),
            Some(ts("2026-07-11T20:15:00Z"))
        );
        // Before the whole range → earliest entry.
        assert_eq!(
            c.select_timestamp(Some(ts("2026-07-11T20:00:00Z"))),
            Some(ts("2026-07-11T20:10:00Z"))
        );
        // No time → newest entry.
        assert_eq!(c.select_timestamp(None), Some(ts("2026-07-11T20:15:00Z")));
        // Empty catalog → nothing to resolve.
        assert_eq!(Catalog::empty().select_timestamp(None), None);
        assert_eq!(
            Catalog::empty().select_timestamp(Some(ts("2026-07-11T20:20:00Z"))),
            None
        );
    }

    #[test]
    fn file_entry_loaded_constructor() {
        let path = PathBuf::from("/tmp/test.tif");
        let source = DataSource::from_path(&path);
        let metadata = dummy_metadata();
        let entry = FileEntry::loaded(path.clone(), source, metadata, 1024, None, None);

        assert!(entry.metadata().is_some());
        assert!(entry.source().is_some());
        assert!(entry.stac_stub_info().is_none());
        assert_eq!(entry.file_size, 1024);
        assert_eq!(entry.path, path);
    }

    #[test]
    fn file_entry_stac_stub_constructor() {
        let path = PathBuf::from("https://example.com/radar/file.tif");
        let stub = StacStub {
            bbox: Some([10.0, 55.0, 30.0, 72.0]),
            asset_url: "https://example.com/radar/file.tif".to_string(),
        };
        let entry = FileEntry::stac_stub(path.clone(), 2048, stub);

        assert!(entry.metadata().is_none());
        assert!(entry.source().is_none());
        assert!(entry.stac_stub_info().is_some());
        assert_eq!(entry.file_size, 2048);
        assert_eq!(entry.path, path);
        let stub = entry.stac_stub_info().unwrap();
        assert_eq!(stub.bbox, Some([10.0, 55.0, 30.0, 72.0]));
        assert_eq!(stub.asset_url, "https://example.com/radar/file.tif");
    }

    #[test]
    fn file_entry_is_loaded() {
        // A loaded entry should return true
        let path = PathBuf::from("/tmp/test.tif");
        let source = DataSource::from_path(&path);
        let metadata = dummy_metadata();
        let loaded = FileEntry::loaded(path, source, metadata, 1024, None, None);
        assert!(loaded.is_loaded());

        // A stub entry should return false
        let stub_path = PathBuf::from("https://example.com/file.tif");
        let stub = StacStub {
            bbox: None,
            asset_url: "https://example.com/file.tif".to_string(),
        };
        let unloaded = FileEntry::stac_stub(stub_path, 0, stub);
        assert!(!unloaded.is_loaded());
    }

    /// Regression test for the #253 reviewer catch (#204 follow-up): a poll
    /// cycle that re-scans the same directory must reuse the prior
    /// `DataSource` for unchanged files, so the `mmap_cache` from #204
    /// survives across scans instead of being thrown away on every cycle.
    #[test]
    fn scan_directory_reuses_data_source_across_polls() {
        use std::collections::HashMap;
        use std::sync::Arc;

        // Find a directory of real radar TIFFs.
        let dir = ["testdata/radar", "../../testdata/radar"]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_dir())
            .expect("testdata/radar fixture");

        let pattern = FilenameMatcher::from_pattern(
            r"radar_(?P<timestamp>\d{8}T\d{4}Z)\.tif",
            "%Y%m%dT%H%MZ",
        )
        .unwrap();
        let mut pending = BTreeMap::new();

        // First scan — `existing` is empty, all files freshly mmapped.
        let first =
            scan_directory(&dir, &pattern, &[], &mut pending, &HashMap::new()).expect("first scan");
        assert!(!first.entries.is_empty(), "fixture should contain TIFFs");

        // Force the lazy mmap_cache to populate on one entry so we can prove
        // its Arc identity survives the second scan. `from_source` opens a
        // decoder internally → triggers `load_mmap` → populates the OnceLock.
        let (sample_ts, sample_entry) = first.entries.iter().next().unwrap();
        let sample_source = sample_entry.source().expect("loaded entry has source");
        let _ = TiffMetadata::from_source(sample_source)
            .expect("from_source should succeed and populate the mmap cache");

        // Snapshot the Arc<Mmap> pointer that lives inside the LocalFile.
        let cached_mmap_ptr = match &**sample_source {
            DataSource::LocalFile { mmap_cache, .. } => {
                let cached = mmap_cache.get().expect("mmap_cache populated");
                let (mmap, _) = cached.as_ref().expect("mmap succeeded");
                Arc::as_ptr(mmap)
            }
            _ => panic!("expected LocalFile"),
        };

        // Second scan — pass the first catalog as `existing`.
        let path_index: HashMap<&Path, &FileEntry> = first
            .entries
            .values()
            .map(|e| (e.path.as_path(), e))
            .collect();
        let second =
            scan_directory(&dir, &pattern, &[], &mut pending, &path_index).expect("second scan");

        // The entry for the same timestamp must reuse the prior source (same
        // mmap_cache OnceLock, populated with the same Arc<Mmap>).
        let reused_entry = second
            .entries
            .get(sample_ts)
            .expect("second scan keeps the same entry");
        let reused_source = reused_entry.source().expect("loaded after reuse");
        let reused_mmap_ptr = match &**reused_source {
            DataSource::LocalFile { mmap_cache, .. } => {
                // If get() returns None here, EITHER scan_directory rebuilt
                // the DataSource (the bug this test guards against) OR the
                // committed fixture's mtime/inode changed between the two
                // scans (e.g. CI ran `git lfs pull` or another process
                // touched testdata/radar between the two scan calls).
                // Surface both possibilities so a failing CI run isn't
                // misleading.
                let cached = match mmap_cache.get() {
                    Some(c) => c,
                    None => panic!(
                        "second-scan mmap_cache is empty — either scan_directory \
                         rebuilt the DataSource instead of reusing it (the regression \
                         this test guards against), OR the fixture's mtime/inode was \
                         touched between the two scans. Verify the fixture wasn't \
                         modified by checking `stat testdata/radar/*.tif`."
                    ),
                };
                let (mmap, _) = cached
                    .as_ref()
                    .expect("mmap was successful on the first scan");
                Arc::as_ptr(mmap)
            }
            _ => panic!("expected LocalFile after reuse"),
        };

        // The strongest assertion is that the inner `Arc<Mmap>` is literally
        // the same allocation across scans: if `scan_directory` had rebuilt
        // the `DataSource` from scratch (the bug), the new OnceLock would be
        // empty and we couldn't even reach the assertion above — `.get()`
        // would return None. As a belt-and-braces check, also assert the
        // OnceLock allocation itself is the same.
        assert_eq!(
            cached_mmap_ptr, reused_mmap_ptr,
            "scan_directory must reuse the prior DataSource's mmap, not re-issue Mmap::map"
        );

        let cached_lock_ptr = match &**sample_source {
            DataSource::LocalFile { mmap_cache, .. } => Arc::as_ptr(mmap_cache),
            _ => unreachable!(),
        };
        let reused_lock_ptr = match &**reused_source {
            DataSource::LocalFile { mmap_cache, .. } => Arc::as_ptr(mmap_cache),
            _ => unreachable!(),
        };
        assert_eq!(
            cached_lock_ptr, reused_lock_ptr,
            "scan_directory must clone the prior DataSource (sharing Arc<OnceLock<...>>) for unchanged files"
        );
    }

    /// A file that appears between polls waits in `pending` until the
    /// shared `PendingFile` has seen its size unchanged on two polls after
    /// the first; a size change restarts the count. The initial scan
    /// accepts every file at once.
    #[test]
    fn scan_directory_holds_a_new_file_until_its_size_is_stable() {
        use std::io::Write;
        use std::time::SystemTime;

        let src_dir = ["testdata/radar", "../../testdata/radar"]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_dir())
            .expect("testdata/radar fixture");
        let tmp_dir = std::env::temp_dir().join(format!(
            "meteocore_pending_test_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(tmp_dir.clone());
        let copy = |name: &str| {
            std::fs::copy(src_dir.join(name), tmp_dir.join(name)).unwrap();
        };
        let pattern = FilenameMatcher::from_pattern(
            r"radar_(?P<timestamp>\d{8}T\d{4}Z)\.tif",
            "%Y%m%dT%H%MZ",
        )
        .unwrap();
        let mut pending = BTreeMap::new();
        let mut scan = |previous: &Catalog| {
            let existing: HashMap<&Path, &FileEntry> = previous
                .entries
                .values()
                .map(|e| (e.path.as_path(), e))
                .collect();
            scan_directory(&tmp_dir, &pattern, &[], &mut pending, &existing).unwrap()
        };

        copy("radar_20260324T2315Z.tif");
        let catalog = scan(&Catalog::empty());
        assert_eq!(catalog.entries.len(), 1, "the initial scan accepts at once");

        copy("radar_20260324T2320Z.tif");
        let catalog = scan(&catalog);
        assert_eq!(catalog.entries.len(), 1, "first seen: pending");
        // Still being written: the size grows, which restarts the count.
        std::fs::OpenOptions::new()
            .append(true)
            .open(tmp_dir.join("radar_20260324T2320Z.tif"))
            .unwrap()
            .write_all(b"tail")
            .unwrap();
        let catalog = scan(&catalog);
        let catalog = scan(&catalog);
        assert_eq!(catalog.entries.len(), 1, "one unchanged poll is not enough");
        let catalog = scan(&catalog);
        assert_eq!(catalog.entries.len(), 2, "two unchanged polls settle it");
    }

    /// Regression test for the #253 round-2 reviewer catch: when a local file
    /// is atomically replaced with a new file of the SAME byte count, the
    /// cached mmap must NOT be reused — otherwise renders would keep serving
    /// the prior inode's pixels until the next size-changing publish.
    ///
    /// Reproduces by copying a real fixture into a tempdir, scanning it,
    /// bumping the file's mtime by 2 s (mtime resolution on macOS is 1 s),
    /// and scanning again. The second scan must produce a different
    /// `Arc<OnceLock<...>>` (the source was rebuilt, not cloned).
    #[test]
    fn scan_directory_rebuilds_source_when_mtime_changes() {
        use std::collections::HashMap;
        use std::process;
        use std::sync::Arc;
        use std::time::{Duration, SystemTime};

        // Copy a real fixture into a unique tempdir so we can mutate its
        // mtime without touching the committed testdata.
        let src_dir = ["testdata/radar", "../../testdata/radar"]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_dir())
            .expect("testdata/radar fixture");
        let src_file = std::fs::read_dir(&src_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| {
                e.path()
                    .extension()
                    .and_then(|x| x.to_str())
                    .map(|s| s == "tif")
                    .unwrap_or(false)
            })
            .expect("at least one .tif in fixture");

        let tmp_dir = std::env::temp_dir().join(format!(
            "meteocore_mtime_test_{}_{}",
            process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        // Always clean up.
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(tmp_dir.clone());

        let dst_name = src_file.file_name();
        let dst_path = tmp_dir.join(&dst_name);
        std::fs::copy(src_file.path(), &dst_path).unwrap();

        let pattern = FilenameMatcher::from_pattern(
            r"radar_(?P<timestamp>\d{8}T\d{4}Z)\.tif",
            "%Y%m%dT%H%MZ",
        )
        .unwrap();
        let mut pending = BTreeMap::new();

        let first = scan_directory(&tmp_dir, &pattern, &[], &mut pending, &HashMap::new())
            .expect("first scan");
        assert_eq!(first.entries.len(), 1);

        let first_entry = first.entries.values().next().unwrap();
        let first_lock_ptr = match &**first_entry.source().unwrap() {
            DataSource::LocalFile { mmap_cache, .. } => Arc::as_ptr(mmap_cache),
            _ => panic!(),
        };
        let original_size = first_entry.file_size;

        // Bump mtime by 2 s. macOS HFS+/APFS have 1 s resolution; +2 s clears
        // the boundary safely. Write the SAME byte count so size stays equal.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&dst_path)
            .unwrap();
        let new_mtime = SystemTime::now() + Duration::from_secs(2);
        file.set_modified(new_mtime).unwrap();
        drop(file);
        // Verify size didn't change — that's the whole point of this test.
        assert_eq!(
            std::fs::metadata(&dst_path).unwrap().len(),
            original_size,
            "this test only exercises the bug when size stays equal"
        );

        // Second scan with the previous catalog as `existing`.
        let path_index: HashMap<&Path, &FileEntry> = first
            .entries
            .values()
            .map(|e| (e.path.as_path(), e))
            .collect();
        let second = scan_directory(&tmp_dir, &pattern, &[], &mut pending, &path_index)
            .expect("second scan");

        let second_entry = second.entries.values().next().unwrap();
        let second_lock_ptr = match &**second_entry.source().unwrap() {
            DataSource::LocalFile { mmap_cache, .. } => Arc::as_ptr(mmap_cache),
            _ => panic!(),
        };

        assert_ne!(
            first_lock_ptr, second_lock_ptr,
            "mtime change must rebuild the DataSource so the next render picks up the new inode"
        );
        // The new entry must also record the new mtime so a third scan with
        // no further change reuses the cache again.
        assert!(second_entry.mtime.is_some());
        assert_ne!(first_entry.mtime, second_entry.mtime);
    }

    /// Regression test for the #253 round-3 reviewer catch: same-size,
    /// same-mtime, different-inode atomic replacement (the case that mtime
    /// alone misses on 1-second-resolution filesystems). Inode comparison
    /// must catch this and force a rebuild.
    ///
    /// Reproduces by writing two same-size, same-mtime files to a tempdir
    /// across two scans, asserting the second scan rebuilds the DataSource
    /// because the inode number differs.
    #[cfg(unix)]
    #[test]
    fn scan_directory_rebuilds_source_when_inode_changes_same_size_same_mtime() {
        use std::collections::HashMap;
        use std::os::unix::fs::MetadataExt;
        use std::process;
        use std::sync::Arc;
        use std::time::SystemTime;

        let src_dir = ["testdata/radar", "../../testdata/radar"]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_dir())
            .expect("testdata/radar fixture");
        let src_file = std::fs::read_dir(&src_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| {
                e.path()
                    .extension()
                    .and_then(|x| x.to_str())
                    .map(|s| s == "tif")
                    .unwrap_or(false)
            })
            .expect(".tif in fixture");

        let tmp_dir = std::env::temp_dir().join(format!(
            "meteocore_inode_test_{}_{}",
            process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(tmp_dir.clone());

        let dst_path = tmp_dir.join(src_file.file_name());
        std::fs::copy(src_file.path(), &dst_path).unwrap();

        let pattern = FilenameMatcher::from_pattern(
            r"radar_(?P<timestamp>\d{8}T\d{4}Z)\.tif",
            "%Y%m%dT%H%MZ",
        )
        .unwrap();
        let mut pending = BTreeMap::new();

        let first = scan_directory(&tmp_dir, &pattern, &[], &mut pending, &HashMap::new())
            .expect("first scan");
        let first_entry = first.entries.values().next().unwrap();
        let first_inode = first_entry.inode.expect("Unix scan captures inode");
        let first_mtime = first_entry.mtime.expect("Unix scan captures mtime");
        let first_lock_ptr = match &**first_entry.source().unwrap() {
            DataSource::LocalFile { mmap_cache, .. } => Arc::as_ptr(mmap_cache),
            _ => panic!(),
        };

        // Atomically replace the file with a new copy at the SAME byte count.
        // Write to a sibling tempfile, restore the same mtime, then rename
        // over the original — same path, same size, same mtime, NEW inode.
        let replacement = tmp_dir.join("replacement.tif.tmp");
        std::fs::copy(src_file.path(), &replacement).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_modified(first_mtime)
            .unwrap();
        std::fs::rename(&replacement, &dst_path).unwrap();

        let after_meta = std::fs::metadata(&dst_path).unwrap();
        assert_eq!(after_meta.len(), first_entry.file_size, "size unchanged");
        // Test is only meaningful if the inode actually changed — sanity check.
        // (On the unusual filesystem where rename preserves inode, skip.)
        if after_meta.ino() == first_inode {
            eprintln!(
                "skip: filesystem preserves inode across rename ({}), test inapplicable",
                first_inode
            );
            return;
        }
        // If the kernel happened to give the replacement a coarser mtime than
        // our set_modified, the mtime test alone would already catch it and
        // this test wouldn't exercise inode-specifically. Force same mtime
        // explicitly post-rename.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&dst_path)
            .unwrap()
            .set_modified(first_mtime)
            .unwrap();

        let path_index: HashMap<&Path, &FileEntry> = first
            .entries
            .values()
            .map(|e| (e.path.as_path(), e))
            .collect();
        let second = scan_directory(&tmp_dir, &pattern, &[], &mut pending, &path_index)
            .expect("second scan");

        let second_entry = second.entries.values().next().unwrap();
        let second_lock_ptr = match &**second_entry.source().unwrap() {
            DataSource::LocalFile { mmap_cache, .. } => Arc::as_ptr(mmap_cache),
            _ => panic!(),
        };
        let second_inode = second_entry.inode.expect("Unix scan captures inode");

        assert_ne!(
            first_inode, second_inode,
            "test setup: replacement must change inode"
        );
        assert_ne!(
            first_lock_ptr, second_lock_ptr,
            "inode change must rebuild the DataSource even when size+mtime are unchanged"
        );
    }

    /// #1055: two listings name one version when the sizes agree and the
    /// ETags do, or, without an ETag on both, the modification times.
    #[test]
    fn listings_compare_the_etag_before_the_modification_time() {
        let at = |secs| DateTime::<Utc>::from_timestamp(secs, 0);
        let version = |size, last_modified, e_tag: Option<&str>| ObjectVersion {
            size,
            last_modified,
            e_tag: e_tag.map(str::to_string),
        };
        let listed = version(10, at(1), Some("a"));
        assert!(
            same_version(&listed, &version(10, at(2), Some("a"))),
            "the same bytes written again"
        );
        assert!(!same_version(&listed, &version(10, at(1), Some("b"))));
        assert!(!same_version(&listed, &version(11, at(1), Some("a"))));
        assert!(
            !same_version(&listed, &version(10, at(2), None)),
            "without an ETag the modification time decides"
        );
        assert!(
            same_version(&listed, &version(10, None, None)),
            "with neither, the size"
        );
    }

    /// #1055: a poll discovered the timesteps the previous catalog did not
    /// hold and those it held with another object, a STAC stub with another
    /// href or size too; only the latter were republished.
    #[test]
    fn fresh_since_takes_new_and_republished_timesteps() {
        let t = |minute: i64| DateTime::<Utc>::from_timestamp(minute * 60, 0).unwrap();
        let stub = |href: &str, size| {
            FileEntry::stac_stub(
                PathBuf::from(href),
                size,
                StacStub {
                    bbox: None,
                    asset_url: href.to_string(),
                },
            )
        };
        let catalog = |entries: Vec<(DateTime<Utc>, FileEntry)>| Catalog {
            entries: entries.into_iter().collect(),
            temporal_extent: None,
            spatial_extent: None,
        };
        let previous = catalog(vec![
            (t(0), stub("a", 1)),
            (t(1), stub("b", 1)),
            (t(2), stub("c", 1)),
        ]);
        let current = catalog(vec![
            (t(0), stub("a", 1)),
            (t(1), stub("b2", 1)),
            (t(2), stub("c", 2)),
            (t(3), stub("d", 1)),
        ]);
        let fresh: Vec<_> = current
            .fresh_since(&previous)
            .map(|(time, _)| *time)
            .collect();
        assert_eq!(fresh, [t(3), t(2), t(1)]);
        assert_eq!(current.republished_since(&previous), [t(2), t(1)]);
        assert!(current.republished_since(&current).is_empty());
    }

    /// #1055: a poll keeps a load that finished during its scan for the
    /// same object only. A stub with another href or size at that
    /// timestep, an item republished under it, stays a stub.
    #[test]
    fn a_poll_keeps_loads_of_the_same_object_only() {
        let t = |minute: i64| DateTime::<Utc>::from_timestamp(minute * 60, 0).unwrap();
        let stub = |href: &str, size| {
            FileEntry::stac_stub(
                PathBuf::from(href),
                size,
                StacStub {
                    bbox: None,
                    asset_url: href.to_string(),
                },
            )
        };
        let loaded = |href: &str, size| {
            let mut entry = stub(href, size);
            entry.set_loaded(
                Arc::new(dummy_metadata()),
                Arc::new(DataSource::from_bytes(Vec::<u8>::new())),
            );
            entry
        };
        let catalog = |entries: Vec<(DateTime<Utc>, FileEntry)>| Catalog {
            entries: entries.into_iter().collect(),
            temporal_extent: None,
            spatial_extent: None,
        };
        let current = catalog(vec![
            (t(0), loaded("a", 1)),
            (t(1), loaded("b", 1)),
            (t(2), loaded("c", 1)),
        ]);
        let mut scanned = catalog(vec![
            (t(0), stub("a", 1)),
            (t(1), stub("b2", 1)),
            (t(2), stub("c", 2)),
        ]);
        scanned.keep_loads_from(&current);
        let loads: Vec<bool> = scanned.entries.values().map(FileEntry::is_loaded).collect();
        assert_eq!(loads, [true, false, false]);
    }
}
