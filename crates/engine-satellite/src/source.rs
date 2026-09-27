//! Where scans come from: a public S3 bucket (GOES-R, Himawari on AWS), or a
//! local directory mirroring it (tests, offline use).

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use ds_core::error::DataServerError;
use ds_storage::object_store::path::Path as ObjectPath;
use ds_storage::DataStore;

use crate::naming::Naming;

/// Tiles of one scan downloaded at once: a Himawari full disk is 88 files
/// of ~0.3 MB, which one after another would take seconds of round trips
/// (Critical Rule 9).
const TILE_FETCH_CONCURRENCY: usize = 16;

pub(crate) enum Source {
    /// A bucket partitioned by [`Naming::prefixes`].
    Bucket { store: DataStore },
    /// A directory listed recursively; the file name alone selects scans.
    Directory { store: DataStore, base: ObjectPath },
}

/// One scan found by a listing: its file, or its tiles in tile order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Found {
    pub time: DateTime<Utc>,
    pub paths: Arc<[ObjectPath]>,
}

impl Source {
    /// The scans of one product with a scan start in `window` (every scan
    /// when `None`), oldest first. `known` names scans already ingested: a
    /// bucket skips the prefixes that hold nothing else.
    ///
    /// Each prefix is one sequential `list` on the background runtime; a
    /// window's prefixes are capped by [`Naming::validate_window`]
    /// (Critical Rule 9).
    pub fn list(
        &self,
        naming: &Naming,
        window: Option<(DateTime<Utc>, DateTime<Utc>)>,
        known: impl Fn(DateTime<Utc>) -> bool,
    ) -> Result<Vec<Found>, DataServerError> {
        let listed = match self {
            Source::Bucket { store } => {
                let (start, end) = window.ok_or_else(|| {
                    DataServerError::Config("a bucket source needs a time_window".into())
                })?;
                let mut listed = Vec::new();
                for prefix in naming.prefixes(start, end, known)? {
                    listed.extend(store.list(&ObjectPath::from(prefix))?);
                }
                listed
            }
            Source::Directory { store, base } => store.list(base)?,
        };
        // Scan start → tile (0 for a single-file scan) → file. A file
        // re-published under a new creation stamp keeps its scan start and
        // tile; keep the lexicographically greatest name.
        let mut scans: BTreeMap<DateTime<Utc>, BTreeMap<u32, ObjectPath>> = BTreeMap::new();
        for meta in listed {
            let Some(name) = meta.location.filename() else {
                continue;
            };
            let Some(time) = naming.scan_start(name) else {
                continue;
            };
            if !window.is_none_or(|(start, end)| start <= time && time <= end) {
                continue;
            }
            let tile = if naming.tiled() {
                match naming.tile(name) {
                    Some(tile) => tile,
                    None => continue,
                }
            } else {
                0
            };
            let files = scans.entry(time).or_default();
            if files
                .get(&tile)
                .is_none_or(|kept| kept.as_ref() < meta.location.as_ref())
            {
                files.insert(tile, meta.location);
            }
        }
        Ok(scans
            .into_iter()
            .map(|(time, files)| Found {
                time,
                paths: files.into_values().collect(),
            })
            .collect())
    }

    /// The whole files at `paths`, in order: a tiled scan's tiles are
    /// fetched [`TILE_FETCH_CONCURRENCY`] at a time, and any failure fails
    /// the scan (the next poll retries it).
    ///
    /// Called from the poll loop (background runtime), from render jobs
    /// (blocking workers) and from EDR queries (async request workers) when
    /// a scan was evicted. `DataStore::get` and `get_many` serve all three:
    /// their bridge yields an async worker via `block_in_place` and runs
    /// directly on a blocking one — the plain-Zarr exception to Critical
    /// Rule 7. An explicit `get_on` handle would panic on the EDR path.
    pub fn fetch(&self, paths: &[ObjectPath]) -> Result<Vec<Vec<u8>>, DataServerError> {
        let store = match self {
            Source::Bucket { store } | Source::Directory { store, .. } => store,
        };
        if let [path] = paths {
            return Ok(vec![store.get(path)?.to_vec()]);
        }
        store
            .get_many(paths, TILE_FETCH_CONCURRENCY, None)?
            .into_iter()
            .map(|file| file.map(|bytes| bytes.to_vec()))
            .collect()
    }
}
