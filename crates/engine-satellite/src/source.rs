//! Where scans come from: a public S3 bucket (GOES-R on AWS), or a local
//! directory mirroring it (tests, offline use).

use chrono::{DateTime, Utc};
use ds_core::error::DataServerError;
use ds_storage::discovery::expand_prefix_for_range;
use ds_storage::object_store::path::Path as ObjectPath;
use ds_storage::DataStore;

use crate::naming::Naming;

pub(crate) enum Source {
    /// A bucket partitioned by the hourly prefixes of [`Naming::prefix`].
    Bucket { store: DataStore },
    /// A directory listed recursively; the file name alone selects scans.
    Directory { store: DataStore, base: ObjectPath },
}

/// One scan file found by a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Found {
    pub time: DateTime<Utc>,
    pub path: ObjectPath,
}

impl Source {
    /// The scans of one product with a scan start in `window` (every scan
    /// when `None`).
    ///
    /// A bucket lists one hourly prefix per hour of the window
    /// (`validate_prefix_pattern` caps it at 24): each is one sequential
    /// `list` on the background runtime (Critical Rule 9).
    pub fn list(
        &self,
        naming: &Naming,
        window: Option<(DateTime<Utc>, DateTime<Utc>)>,
    ) -> Result<Vec<Found>, DataServerError> {
        let listed = match self {
            Source::Bucket { store } => {
                let (start, end) = window.ok_or_else(|| {
                    DataServerError::Config("a bucket source needs a time_window".into())
                })?;
                let mut listed = Vec::new();
                for prefix in expand_prefix_for_range(&naming.prefix, start, end)? {
                    listed.extend(store.list(&ObjectPath::from(prefix))?);
                }
                listed
            }
            Source::Directory { store, base } => store.list(base)?,
        };
        let mut found: Vec<Found> = listed
            .into_iter()
            .filter_map(|meta| {
                let name = meta.location.filename()?;
                let time = naming.scan_start(name)?;
                let in_window = window.is_none_or(|(start, end)| start <= time && time <= end);
                in_window.then_some(Found {
                    time,
                    path: meta.location,
                })
            })
            .collect();
        // A scan re-published under a new creation stamp keeps its start
        // time; keep the last-listed (lexicographically greatest) name.
        found.sort_by(|a, b| {
            a.time
                .cmp(&b.time)
                .then(a.path.as_ref().cmp(b.path.as_ref()))
        });
        found.dedup_by(|later, earlier| {
            if later.time == earlier.time {
                *earlier = later.clone();
                true
            } else {
                false
            }
        });
        Ok(found)
    }

    /// The whole file at `path`.
    ///
    /// Called from the poll loop (background runtime), from render jobs
    /// (blocking workers) and from EDR queries (async request workers) when
    /// a scan was evicted. `DataStore::get` serves all three: its bridge
    /// yields an async worker via `block_in_place` and runs directly on a
    /// blocking one — the plain-Zarr exception to Critical Rule 7. An
    /// explicit `get_on` handle would panic on the EDR path.
    pub fn fetch(&self, path: &ObjectPath) -> Result<Vec<u8>, DataServerError> {
        let store = match self {
            Source::Bucket { store } | Source::Directory { store, .. } => store,
        };
        Ok(store.get(path)?.to_vec())
    }
}
