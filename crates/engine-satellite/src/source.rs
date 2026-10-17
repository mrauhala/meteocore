//! Where scans come from: a public S3 bucket (GOES-R, Himawari on AWS), or a
//! local directory mirroring it (tests, offline use).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use ds_core::error::DataServerError;
use ds_core::temp_files;
use ds_storage::object_store::path::Path as ObjectPath;
use ds_storage::object_store::ObjectMeta;
use ds_storage::{DataStore, FetchBudget};

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

/// Listings made during one poll, by prefix. An ISatSS slot directory
/// holds every band's tiles, so the products of one collection list it
/// once, not once each (Critical Rule 9).
pub(crate) type Listings = HashMap<String, Arc<[ObjectMeta]>>;

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
    /// Each prefix is one sequential `list` on the background runtime,
    /// made once per poll whichever products need it (`listings`); a
    /// window's prefixes are capped by [`Naming::validate_window`]
    /// (Critical Rule 9).
    ///
    /// The `list` is recursive and the file name alone selects a scan, so a
    /// file in a hidden folder below the prefix is dropped from the listing:
    /// rsync `--delay-updates` stages a mirror's files under their final
    /// names in `.~tmp~/` while they are written (#1029).
    pub fn list(
        &self,
        naming: &Naming,
        window: Option<(DateTime<Utc>, DateTime<Utc>)>,
        known: impl Fn(DateTime<Utc>) -> bool,
        listings: &mut Listings,
    ) -> Result<Vec<Found>, DataServerError> {
        let mut list = |store: &DataStore, prefix: String| -> Result<_, DataServerError> {
            if let Some(listed) = listings.get(&prefix) {
                return Ok(listed.clone());
            }
            let path = ObjectPath::from(prefix.as_str());
            let listed: Arc<[ObjectMeta]> = store
                .list(&path)?
                .into_iter()
                .filter(|meta| !temp_files::in_hidden_folder(path.as_ref(), meta.location.as_ref()))
                .collect();
            listings.insert(prefix, listed.clone());
            Ok(listed)
        };
        let mut listed = Vec::new();
        match self {
            Source::Bucket { store } => {
                let (start, end) = window.ok_or_else(|| {
                    DataServerError::Config("a bucket source needs a time_window".into())
                })?;
                for prefix in naming.prefixes(start, end, known)? {
                    listed.extend(list(store, prefix)?.iter().cloned());
                }
            }
            Source::Directory { store, base } => {
                listed.extend(list(store, base.to_string())?.iter().cloned());
            }
        }
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
    /// the scan (the next poll retries it). `budget` limits each file:
    /// [`FetchBudget::Background`] for the poll's ingest, which nobody waits
    /// on, so a stalled attempt is retried instead of skipping the scan
    /// until the next poll (#1011); [`FetchBudget::Request`] wherever a
    /// request may wait.
    ///
    /// Called from the poll loop (background runtime), from render jobs
    /// (blocking workers) and from EDR queries (async request workers) when
    /// a scan was evicted. `DataStore::get_with_budget` and
    /// `get_many_with_budget` serve all three: their bridge yields an async
    /// worker via `block_in_place` and runs directly on a blocking one — the
    /// plain-Zarr exception to Critical Rule 7. An explicit `get_on` handle
    /// would panic on the EDR path.
    pub fn fetch(
        &self,
        paths: &[ObjectPath],
        budget: FetchBudget,
    ) -> Result<Vec<Vec<u8>>, DataServerError> {
        let store = match self {
            Source::Bucket { store } | Source::Directory { store, .. } => store,
        };
        if let [path] = paths {
            return Ok(vec![store.get_with_budget(path, budget)?.to_vec()]);
        }
        store
            .get_many_with_budget(paths, TILE_FETCH_CONCURRENCY, None, budget)?
            .into_iter()
            .map(|file| file.map(|bytes| bytes.to_vec()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ds_storage::object_store::memory::InMemory;
    use ds_storage::object_store::path::Path as ObjectPath;
    use ds_storage::object_store::ObjectMeta;
    use ds_storage::DataStore;

    use super::{Listings, Source};
    use crate::naming::Naming;

    /// Two bands of one ISatSS collection share a slot directory: listed
    /// once per poll, the second band reads the first band's listing. The
    /// store here is empty, so each band finding its tiles proves nothing
    /// was listed twice.
    #[test]
    fn products_share_a_slot_listing() {
        let slot = "AHI-L2-FLDK-ISatSS/2026/09/27/1920";
        let meta = |band: u8, tile: u32| {
            ObjectMeta {
            location: ObjectPath::from(format!(
                "{slot}/OR_HFD-020-B12-M1C{band:02}-T{tile:03}_GH9_s20262701920000_c20262701928140.nc"
            )),
            last_modified: chrono::Utc::now(),
            size: 1,
            e_tag: None,
            version: None,
        }
        };
        let mut listings = Listings::new();
        listings.insert(
            slot.to_string(),
            [meta(13, 1), meta(13, 2), meta(14, 1), meta(14, 2)].into(),
        );
        let source = Source::Bucket {
            store: DataStore::new(Arc::new(InMemory::new())),
        };
        let at = "2026-09-27T19:20:00Z".parse().unwrap();
        for band in [13, 14] {
            let found = source
                .list(
                    &Naming::isatss("HFD", band),
                    Some((at, at)),
                    |_| false,
                    &mut listings,
                )
                .unwrap();
            assert_eq!(found.len(), 1, "band {band}");
            assert_eq!(found[0].time, at);
            assert_eq!(found[0].paths.len(), 2, "band {band}");
            assert!(found[0].paths[0]
                .as_ref()
                .contains(&format!("C{band}-T001")));
        }
        assert_eq!(listings.len(), 1);
    }

    /// A local mirror filled by rsync `--delay-updates` holds each file
    /// under its final name in a `.~tmp~/` folder until the transfer ends.
    /// The directory is listed recursively and the name alone selects a
    /// scan, so a scan there, at any depth, is never found; a normal
    /// subfolder's is (#1029).
    #[test]
    fn a_directory_never_finds_scans_in_hidden_folders() {
        let dir = tempfile::tempdir().unwrap();
        let file = |minute: &str| {
            format!(
                "OR_ABI-L2-CMIPF-M6C13_G19_s202626819{minute}199_e20262681959519_c20262681959592.nc"
            )
        };
        for (folder, minute) in [
            ("2026/268/19", "00"),
            ("2026/268/19/.~tmp~", "10"),
            (".~tmp~", "20"),
        ] {
            let folder = dir.path().join(folder);
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(folder.join(file(minute)), b"x").unwrap();
        }
        let (store, base) = ds_storage::build_store(dir.path().to_str().unwrap()).unwrap();
        let source = Source::Directory { store, base };

        let found = source
            .list(
                &Naming::goes_r("ABI-L2-CMIPF", Some(13)),
                None,
                |_| false,
                &mut Listings::new(),
            )
            .unwrap();
        let times: Vec<_> = found.iter().map(|f| f.time.to_rfc3339()).collect();
        assert_eq!(times, ["2026-09-25T19:00:00+00:00"]);
        assert!(found[0].paths[0].as_ref().starts_with("2026/268/19/OR_"));
    }
}
