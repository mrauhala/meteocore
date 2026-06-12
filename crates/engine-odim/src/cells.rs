//! Storm-cell extraction + tracking for PVOL per-site collections (#367).
//!
//! The algorithms live in `ds_core::cells` (pure geometry/physics on a
//! [`ds_core::volume::VoxelGrid`]); this module adds the engine-side
//! plumbing: a process-global memo of per-volume [`CellSet`]s (a volume file
//! is immutable, so a scan's segmentation never goes stale) and the
//! window-walk that feeds [`ds_core::cells::track_cells`].
//!
//! [`PolarVolumeSiteView::cell_product`] is the one entry point; the
//! [`ds_core::volume::VolumeEngine::read_cells`] override delegates here
//! with the `spawn_blocking` runtime bridge, while engine-internal callers
//! on a request worker (the Features path) pass `handle: None`.

use crate::quantities;
use crate::volume_engine::{pixel_cache_id, PolarVolumeSiteView, DEFAULT_VOXEL_DIMS};
use ds_core::cells::{extract_cells, track_cells, CellSet, MAX_TRACK_SCANS};
use ds_core::error::DataServerError;
use ds_core::volume::{CellProduct, CellQuery};
use std::sync::Arc;

/// Default [`CELL_SET_CACHE`] size (MB) when `MC_PVOL_CELL_SET_CACHE_MB` is
/// unset. An entry's dominant cost is the per-cell footprint ring; a typical
/// scan's `CellSet` is well under ~10 KB, the 256-cell cap with complex
/// rings ~400 KB — 64 MB holds whole networks' worth of either, and the
/// byte weighting (mirroring `VOXEL_GRID_CACHE`) keeps the ceiling exact
/// regardless of storm activity.
const DEFAULT_CELL_SET_CACHE_MB: u64 = 64;

/// Cache key: source-qualified volume file id + quantity + grid dims + the
/// extraction options **verbatim** (threshold/min-volume bit patterns and
/// the cell cap — not a hash of them, so distinct options can never collide
/// onto one entry). Volumes are immutable once scanned, so the key needs no
/// data-version (the `VOXEL_GRID_CACHE` argument).
type CellSetKey = (Arc<str>, Arc<str>, [usize; 3], (u64, u64, u64));

/// Approximate resident bytes of a cached [`CellSet`]: the struct itself,
/// per-cell fixed fields, and each footprint ring's vertices — the term that
/// actually varies (everything else is a few hundred bytes).
fn cell_set_weight_bytes(key: &CellSetKey, set: &Arc<CellSet>) -> u64 {
    let cells: u64 = set
        .cells
        .iter()
        .map(|c| (std::mem::size_of_val(c) + c.footprint.len() * 16) as u64)
        .sum();
    cells + (std::mem::size_of::<CellSet>() + key.0.len() + key.1.len() + 64) as u64
}

/// Byte-weights each entry (see [`cell_set_weight_bytes`]).
#[derive(Clone)]
struct CellSetWeighter;

impl quick_cache::Weighter<CellSetKey, Arc<CellSet>> for CellSetWeighter {
    fn weight(&self, key: &CellSetKey, val: &Arc<CellSet>) -> u64 {
        cell_set_weight_bytes(key, val)
    }
}

/// Process-global memo of per-volume segmentations, shared across every PVOL
/// collection, byte-bounded like `VOXEL_GRID_CACHE`.
/// `MC_PVOL_CELL_SET_CACHE_MB=0` *effectively* disables it — quick_cache has
/// no zero capacity, so a 1-byte budget rejects every real entry on insert
/// (every request re-segments).
static CELL_SET_CACHE: std::sync::LazyLock<
    quick_cache::sync::Cache<CellSetKey, Arc<CellSet>, CellSetWeighter>,
> = std::sync::LazyLock::new(|| {
    let capacity_bytes = std::env::var("MC_PVOL_CELL_SET_CACHE_MB")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_CELL_SET_CACHE_MB)
        .saturating_mul(1024 * 1024);
    // Estimate item slots at ~16 KB each; `max(...)` keeps a zero capacity
    // valid (an effectively-disabled cache that can hold nothing).
    let estimated_items = ((capacity_bytes / (16 * 1024)).max(4)) as usize;
    quick_cache::sync::Cache::with_weighter(estimated_items, capacity_bytes.max(1), CellSetWeighter)
});

/// Cumulative `(hits, misses)` of [`CELL_SET_CACHE`], for `/metrics`.
static CELL_SET_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static CELL_SET_MISSES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Snapshot of the process-global cell-set cache for `/metrics`:
/// `(hits, misses, resident_bytes, capacity_bytes)`.
pub fn cell_set_cache_metrics() -> (u64, u64, u64, u64) {
    (
        CELL_SET_HITS.load(std::sync::atomic::Ordering::Relaxed),
        CELL_SET_MISSES.load(std::sync::atomic::Ordering::Relaxed),
        CELL_SET_CACHE.weight(),
        CELL_SET_CACHE.capacity(),
    )
}

/// The extraction options as an exact key component: two requests share an
/// entry iff every knob is bit-identical (no hash, no collision).
fn extraction_key(opts: &ds_core::cells::CellExtractionOptions) -> (u64, u64, u64) {
    (
        opts.threshold.to_bits(),
        opts.min_volume_km3.to_bits(),
        opts.max_cells as u64,
    )
}

impl PolarVolumeSiteView {
    /// Segment + track storm cells for this site (the engine-side
    /// [`ds_core::volume::VolumeEngine::read_cells`], plus a `handle`
    /// parameter so both runtime contexts can call it — see
    /// `volume_engine::blocking_pixel_handle` for the contract; the trait
    /// override passes the `spawn_blocking` bridge, a request-worker caller
    /// passes `None`).
    ///
    /// Cells are reflectivity physics (linear-Z weighting, VIL), so only
    /// dBZ-unit quantities are accepted — anything else is
    /// [`DataServerError::InvalidParameter`] (→ 400).
    ///
    /// Each scan's segmentation is memoized in the process-global
    /// [`CELL_SET_CACHE`]; in steady state only the newest volume pays the
    /// resample + extraction, so a poll-cadence animation window stays cheap.
    pub fn cell_product(
        &self,
        query: &CellQuery,
        handle: Option<&tokio::runtime::Handle>,
    ) -> Result<CellProduct, DataServerError> {
        let catalog = self.catalog.load();
        // Validates the quantity, resolves the default, selects the volume
        // nearest `query.time` (latest if `None`), guards the antenna.
        let (target, quantity) =
            self.select_entry_and_quantity(&catalog, query.quantity.as_deref(), query.time)?;
        if quantities::quantity_unit(&quantity) != "dBZ" {
            return Err(DataServerError::InvalidParameter(format!(
                "[{}] storm cells require a reflectivity (dBZ) quantity, `{}` is not one",
                self.collection_id, quantity
            )));
        }
        let volumes = catalog.by_site.get(&self.nod).ok_or_else(|| {
            DataServerError::LocationNotFound(format!(
                "[{}] radar site `{}` has no current volumes",
                self.collection_id, self.nod
            ))
        })?;
        // The tracking window: up to `track_scans` volumes preceding the
        // target, from the site's time-sorted list.
        let target_time = target.volume.time;
        let target_idx = volumes
            .iter()
            .position(|e| e.volume.time == target_time)
            .ok_or_else(|| {
                // `target` came from this same slice, so a miss is a logic
                // error — surface it rather than silently tracking against
                // the wrong window.
                DataServerError::Engine(format!(
                    "[{}] selected volume at {target_time} not found in site `{}` volume list",
                    self.collection_id, self.nod
                ))
            })?;
        // Same window ceiling as the trait default: even with the per-scan
        // memo, a cold window is one grid resample per scan.
        let start = target_idx.saturating_sub(query.track_scans.min(MAX_TRACK_SCANS));
        let dims = query.dims.unwrap_or(DEFAULT_VOXEL_DIMS);
        let opts_key = extraction_key(&query.extraction);

        let mut cell_sets = Vec::with_capacity(target_idx - start + 1);
        for entry in &volumes[start..=target_idx] {
            let key: CellSetKey = (
                Arc::from(pixel_cache_id(&self.source, &entry.id).as_ref()),
                Arc::from(quantity.as_str()),
                dims,
                opts_key,
            );
            let mut computed = false;
            let set = CELL_SET_CACHE.get_or_insert_with(&key, || {
                computed = true;
                let (grid, valid) = self.voxel_grid_cached(entry, &quantity, dims, handle)?;
                let set = if valid == 0 {
                    // No echo anywhere in the volume: a valid empty scan
                    // (tracking sees the death) — cached like any other.
                    CellSet::empty(
                        entry.volume.time,
                        quantity.as_str(),
                        query.extraction.threshold,
                        [grid.origin_lon, grid.origin_lat, grid.origin_height],
                    )
                } else {
                    extract_cells(&grid, entry.volume.time, &query.extraction)
                };
                Ok::<_, DataServerError>(Arc::new(set))
            })?;
            if computed {
                CELL_SET_MISSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            } else {
                CELL_SET_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            cell_sets.push((entry.volume.time, set));
        }
        let tracks = track_cells(&cell_sets, &query.tracking);
        Ok(CellProduct { cell_sets, tracks })
    }
}
