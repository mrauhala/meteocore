//! Remote objects a tile read found gone after the scan (#1074).
//!
//! A range read that the store refuses for good, with a 404 or a range past
//! the object's end, marks the object's [`ObjectHealth`], which every clone
//! of its catalog entry shares. Every catalog accessor skips a marked entry
//! at once ([`FileEntry::is_gone`]), so its timestep leaves `resolve_time`,
//! `get_raster_tile`, `RasterInfo` and EDR without the request path building
//! a catalog. The next catalog swap removes the entry through
//! [`GoneObjects::screen`], which keeps it so that a listing still naming
//! the same object does not bring it back:
//!
//! - An object-store scan reuses a kept entry for a key it lists with the
//!   same size, without reading the header, and the screen removes it again.
//!   A key the listing no longer names, or names with another size, ends the
//!   record.
//! - A STAC listing that still names the item's asset href with the same
//!   `file:size` adds a stub, which the screen removes. Another href or size
//!   is a new object.
//!
//! Renders cached while a timestep was in the catalog are keyed on it, and
//! the request that met the refusal drew nodata. When a dropped timestep
//! comes back as a new object, the version moves, so the API layers no
//! longer read those keys ([`GoneObjects::version`]).
//!
//! [`ObjectHealth`]: crate::reader::ObjectHealth

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::catalog::{Catalog, FileEntry};

/// Most records of each kind [`GoneObjects`] keeps. Past it, the kept entry
/// with the oldest timestep is forgotten, and the dropped timesteps are
/// cleared with a version move.
const MAX_RECORDS: usize = 1024;

/// The remote objects of one engine that a tile read found gone.
#[derive(Default)]
pub(crate) struct GoneObjects {
    records: Mutex<Records>,
    /// `MapEngine::content_version`: 0 until a dropped timestep comes back.
    version: AtomicU64,
}

#[derive(Default)]
struct Records {
    /// Entries removed as gone whose object a listing may still name.
    kept: Vec<(DateTime<Utc>, FileEntry)>,
    /// Timesteps removed as gone since the version last moved.
    dropped: BTreeSet<DateTime<Utc>>,
}

/// What one [`GoneObjects::screen`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Screened {
    /// Entries removed: gone objects, and stubs of kept ones.
    pub removed: usize,
    /// Whether a dropped timestep came back and the version moved.
    pub version_moved: bool,
}

/// Whether two entries name the same object: its key or asset href, and the
/// size the listing gave it.
fn same_object(a: &FileEntry, b: &FileEntry) -> bool {
    a.path == b.path && a.file_size == b.file_size
}

impl GoneObjects {
    /// The engine's `MapEngine::content_version`. O(1).
    pub(crate) fn version(&self) -> u64 {
        self.version.load(Ordering::SeqCst)
    }

    /// The kept entries, for an object-store scan's reuse index: a key
    /// listed with the same size reuses one instead of reading its header.
    pub(crate) fn kept_entries(&self) -> Vec<FileEntry> {
        let records = self.records.lock().unwrap_or_else(|e| e.into_inner());
        records
            .kept
            .iter()
            .map(|(_, entry)| entry.clone())
            .collect()
    }

    /// Screen `catalog` before it is stored: remove the entries whose object
    /// a read found gone, keeping them here, and the STAC stubs of kept
    /// objects. A timestep removed earlier that holds an entry again came
    /// back as a new object, so the version moves before the catalog is
    /// stored. `complete`: the catalog comes from a scan that listed the
    /// whole source window, so a kept entry it did not reuse is no longer
    /// listed as it was, and the extents are recomputed.
    pub(crate) fn screen(&self, catalog: &mut Catalog, complete: bool) -> Screened {
        let mut records = self.records.lock().unwrap_or_else(|e| e.into_inner());
        let Records { kept, dropped } = &mut *records;
        if kept.is_empty()
            && dropped.is_empty()
            && !catalog.entries.values().any(FileEntry::is_gone)
        {
            return Screened::default();
        }
        let mut seen = vec![false; kept.len()];
        let mut removed = 0;
        catalog.entries.retain(|timestamp, entry| {
            let known = kept.iter().position(|(_, k)| same_object(k, entry));
            if entry.is_gone() {
                match known {
                    Some(i) => seen[i] = true,
                    None => {
                        kept.push((*timestamp, entry.clone()));
                        seen.push(true);
                    }
                }
                dropped.insert(*timestamp);
            } else if let Some(i) = known.filter(|_| entry.stac_stub_info().is_some()) {
                seen[i] = true;
            } else {
                return true;
            }
            removed += 1;
            false
        });

        // A kept object whose timestep holds another entry was replaced in
        // the listing; one a complete scan did not reuse is no longer listed.
        let mut i = 0;
        kept.retain(|(timestamp, _)| {
            let keep = (seen[i] || !complete) && !catalog.entries.contains_key(timestamp);
            i += 1;
            keep
        });
        if kept.len() > MAX_RECORDS {
            kept.sort_by_key(|(timestamp, _)| *timestamp);
            kept.drain(..kept.len() - MAX_RECORDS);
        }

        let came_back = dropped.iter().any(|t| catalog.entries.contains_key(t));
        let version_moved = came_back || dropped.len() > MAX_RECORDS;
        if version_moved {
            // Every key cached under the old version goes unread, so no
            // dropped timestep needs watching any more.
            self.version.fetch_add(1, Ordering::SeqCst);
            dropped.clear();
        }
        if removed > 0 && complete {
            catalog.recompute_extents();
        }
        Screened {
            removed,
            version_moved,
        }
    }
}

/// At most one forbidden-range WARN per collection in this long.
const FORBIDDEN_WARN_INTERVAL: Duration = Duration::from_secs(60);

/// Rate limit for the WARN about 401/403 range refusals (#1074): one per
/// collection per [`FORBIDDEN_WARN_INTERVAL`], counting the refusals since
/// the last one.
#[derive(Default)]
pub(crate) struct ForbiddenWarnings {
    last: Mutex<Option<Instant>>,
    pending: AtomicU64,
}

impl ForbiddenWarnings {
    /// Count `refusals`; `Some(total)` when a WARN is due, with the refusals
    /// since the last one.
    pub(crate) fn note(&self, refusals: u64) -> Option<u64> {
        self.pending.fetch_add(refusals, Ordering::Relaxed);
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|at| at.elapsed() < FORBIDDEN_WARN_INTERVAL) {
            return None;
        }
        *last = Some(Instant::now());
        Some(self.pending.swap(0, Ordering::Relaxed))
    }
}
