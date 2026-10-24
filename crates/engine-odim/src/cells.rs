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
//!
//! [`PolarVolumeSiteView::cells_raster_tile`] renders the product as the
//! derived **`CELLS`** WMS/Maps/Tiles layer: footprint outlines, centroid
//! markers, and track polylines painted into a [`RasterTile`] at the cell's
//! max dBZ, so the ordinary radar colormap styles the overlay.
//!
//! A tile segments every volume of its tracking window. The poll loop builds
//! those cell sets ahead of the tiles ([`warm_from_bytes`],
//! [`warm_windows`]), and the tiles of a cold window build its volumes in
//! parallel, each waiting for the others' builds without a render slot
//! (#1155).

use crate::pvol::PolarVolume;
use crate::quantities;
use crate::volume_engine::{
    blocking_pixel_handle, pixel_cache_id, read_volume_files, voxel_grid_from_bytes, Catalog,
    PolarVolumeSiteView, Source, VolumeEntry, DEFAULT_VOXEL_DIMS,
};
use chrono::{DateTime, SecondsFormat, Utc};
use ds_cache::{CacheEntry, CacheFillGuard};
use ds_core::cells::{
    extract_cells, track_cells, CellExtractionOptions, CellSet, StormCell, Track, MAX_TRACK_SCANS,
};
use ds_core::error::DataServerError;
use ds_core::feature::{Geometry, PropertyValue};
use ds_core::feature_pick::{FeaturePick, PickedFeature, PickedFeatures};
use ds_core::map_engine::{OutputCrs, RasterTile};
use ds_core::raster_paint::Canvas;
use ds_core::volume::{CellProduct, CellQuery, VoxelGrid};
use ds_storage::bytes::Bytes;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Derived map-layer parameter id for the storm-cell overlay. Uppercase like
/// the bare ODIM quantities it sits alongside in the WMS layer list, but
/// never a real moment — `get_raster_tile` intercepts it before quantity
/// resolution, and it is **not** added to the EDR quantity list or the
/// 3D Tiles quantity menu.
pub const CELLS_PARAMETER: &str = "CELLS";
/// Human-readable layer title for [`CELLS_PARAMETER`].
pub(crate) const CELLS_PARAMETER_TITLE: &str = "Storm cells";
/// Scans tracked behind the rendered one for the overlay's trajectories —
/// half an hour at 5-minute cadence.
const CELLS_TRACK_SCANS: usize = 6;
/// The volumes one `CELLS` tile segments: the scan it draws and the scans
/// it tracks behind it.
const LAYER_WINDOW: usize = CELLS_TRACK_SCANS + 1;
const _: () = assert!(CELLS_TRACK_SCANS <= MAX_TRACK_SCANS);
/// Most cell sets one poll builds for the `CELLS` layer's windows by
/// downloading their volumes again ([`warm_windows`], #1155): the volumes a
/// poll itself downloads are segmented from the bytes it holds and are not
/// counted. A process start or a poll after an outage has every window
/// cold; the windows fill over the next polls, newest volumes first, while
/// the poll loop spends at most this many builds on them: about 50 ms of
/// CPU each for a 25–40 MB volume, plus its download.
const MAX_WARMS_PER_POLL: usize = 32;
/// Volume files [`warm_windows`] downloads at once, so a batch holds at most
/// this many times `MAX_REMOTE_FILE_SIZE` (64 MiB) of bytes (root CLAUDE.md
/// Critical Rule 9).
const WARM_DOWNLOADS: usize = 4;
/// Stroke width (px) for footprint rings.
const STROKE_PX: u32 = 2;
/// Stroke width (px) for track trails — one thinner than the outlines so a
/// trail reads as subordinate to the cell it leads to.
const TRACK_STROKE_PX: u32 = 1;
/// Centroid marker arm length (px).
const MARKER_HALF_PX: u32 = 3;

/// Reserved raster value painted along track trails in the `CELLS` overlay.
/// It is **not** a dBZ measurement — a fixed sentinel far outside any real
/// reflectivity, rendered as a single neutral colour by the
/// [`ds_render::OverlayColorMap`] the `CELLS` layer is styled with (so trails
/// are visually distinct from the dBZ-coloured cell outlines, not blended
/// into them). The styling side matches it by exact `f64` equality, so this
/// value must be painted verbatim.
pub const CELLS_TRACK_SENTINEL: f64 = -9999.0;
/// Neutral colour the `CELLS` track trails render as (dark grey — contrasts
/// with both the warm dBZ outline colours and light basemaps). RGBA.
pub const CELLS_TRACK_COLOR: [u8; 4] = [60, 60, 60, 255];

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
/// the cell array, and each footprint ring's vertices — the term that
/// actually varies (everything else is a few hundred bytes).
///
/// Both `Vec`s are weighed by **capacity**, not length (#1052): the cell
/// array grows by `push`, and `ds_core::cells::footprint_ring` closes each
/// exact-size ring with one more `push`, which roughly doubles its
/// allocation. Counting the length would leave that slack outside the
/// cache budget.
fn cell_set_weight_bytes(key: &CellSetKey, set: &Arc<CellSet>) -> u64 {
    let footprints: usize = set
        .cells
        .iter()
        .map(|c| c.footprint.capacity() * std::mem::size_of::<[f64; 2]>())
        .sum();
    let cells = set.cells.capacity() * std::mem::size_of::<ds_core::cells::StormCell>();
    (cells + footprints + std::mem::size_of::<CellSet>() + key.0.len() + key.1.len() + 64) as u64
}

/// Process-global memo of per-volume segmentations, shared across every PVOL
/// collection, byte-bounded like `VOXEL_GRID_CACHE`.
/// `MC_PVOL_CELL_SET_CACHE_MB=0` disables retention (every request
/// re-segments). Item slots are estimated at ~16 KB each.
static CELL_SET_CACHE: std::sync::LazyLock<ds_cache::ByteBoundedCache<CellSetKey, Arc<CellSet>>> =
    std::sync::LazyLock::new(|| {
        ds_cache::ByteBoundedCache::from_env(
            "MC_PVOL_CELL_SET_CACHE_MB",
            DEFAULT_CELL_SET_CACHE_MB,
            16 * 1024,
            cell_set_weight_bytes,
        )
    });

/// Snapshot of the process-global cell-set cache for `/metrics`.
pub fn cell_set_cache_metrics() -> ds_cache::CacheMetrics {
    CELL_SET_CACHE.metrics()
}

/// The extraction options as an exact key component: two requests share an
/// entry iff every knob is bit-identical (no hash, no collision).
fn extraction_key(opts: &CellExtractionOptions) -> (u64, u64, u64) {
    (
        opts.threshold.to_bits(),
        opts.min_volume_km3.to_bits(),
        opts.max_cells as u64,
    )
}

/// The [`CELL_SET_CACHE`] key of one volume file's segmentation. The one
/// home for it, so the poll-time warm-up fills the entries tiles read.
fn cell_set_key(
    source: &Source,
    file_id: &str,
    quantity: &str,
    dims: [usize; 3],
    extraction: &CellExtractionOptions,
) -> CellSetKey {
    (
        Arc::from(pixel_cache_id(source, file_id).as_ref()),
        Arc::from(quantity),
        dims,
        extraction_key(extraction),
    )
}

/// Segment one volume's resampled `grid`, holding `valid` echo cells.
fn segment(
    grid: &VoxelGrid,
    valid: usize,
    time: DateTime<Utc>,
    quantity: &str,
    extraction: &CellExtractionOptions,
) -> CellSet {
    if valid == 0 {
        // No echo anywhere in the volume: a valid empty scan (tracking sees
        // the death), cached like any other.
        CellSet::empty(
            time,
            quantity,
            extraction.threshold,
            [grid.origin_lon, grid.origin_lat, grid.origin_height],
        )
    } else {
        extract_cells(grid, time, extraction)
    }
}

/// The quantity the `CELLS` layer segments, from a site's or a volume's
/// sorted quantities: corrected reflectivity `DBZH`, then total `TH`, then
/// any other dBZ-unit moment.
fn layer_quantity<Q: AsRef<str>>(quantities: &[Q]) -> Option<&str> {
    let quantities = || quantities.iter().map(AsRef::as_ref);
    ["DBZH", "TH"]
        .into_iter()
        .find_map(|preferred| quantities().find(|q| *q == preferred))
        .or_else(|| quantities().find(|q| quantities::quantity_unit(q) == "dBZ"))
}

/// What a `CELLS` tile at `time` asks [`PolarVolumeSiteView::cell_product`]
/// for: the one home of the layer's grid and extraction options, which the
/// warm-up keys its entries by.
fn layer_query(quantity: &str, time: Option<DateTime<Utc>>) -> CellQuery {
    CellQuery {
        quantity: Some(quantity.to_string()),
        time,
        dims: None, // engine default (low) — cells don't need a fine grid
        track_scans: CELLS_TRACK_SCANS,
        ..CellQuery::default()
    }
}

/// How long a tile may wait for another caller's build: until its deadline,
/// or for good without one.
fn wait_budget() -> Duration {
    ds_core::deadline::current().map_or(Duration::MAX, |deadline| {
        deadline.saturating_duration_since(Instant::now())
    })
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
    ///
    /// A cold window is built in parallel by the callers that need it
    /// (#1155): each builds the volumes no other caller is building, then
    /// waits for the rest through [`ds_executor::wait_without_slot`], so the
    /// tiles of one viewport share one build per volume instead of building
    /// the window one volume after another, and a tile does not hold a
    /// render slot while another tile's build runs.
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
        let window = &volumes[start..=target_idx];
        let keys: Vec<CellSetKey> = window
            .iter()
            .map(|entry| cell_set_key(&self.source, &entry.id, &quantity, dims, &query.extraction))
            .collect();

        let mut sets: Vec<Option<Arc<CellSet>>> = vec![None; window.len()];
        loop {
            // Take what is cached and build what nobody is building; note
            // the first set another caller is building.
            let mut building = None;
            for (i, (entry, key)) in window.iter().zip(&keys).enumerate() {
                if sets[i].is_some() {
                    continue;
                }
                match CELL_SET_CACHE.get_value_or_guard_untracked(key, Duration::ZERO) {
                    CacheEntry::Value(set) => {
                        CELL_SET_CACHE.record_hit();
                        sets[i] = Some(set);
                    }
                    CacheEntry::Vacant(fill) => {
                        let set =
                            self.fill_cell_set(fill, entry, &quantity, dims, query, handle)?;
                        sets[i] = Some(set);
                    }
                    CacheEntry::Timeout => {
                        building.get_or_insert(i);
                    }
                }
            }
            let Some(i) = building else { break };
            // The CPU is the builder's: wait for it without the slot. A
            // builder that fails hands its claim to a waiter; that claim is
            // let go before the wait for a slot, so no tile holds a claim
            // the others wait on while it queues, and the next round builds
            // the set or waits for whoever claimed it first.
            let budget = wait_budget();
            let waited = ds_executor::wait_without_slot(|| {
                match CELL_SET_CACHE.get_value_or_guard_untracked(&keys[i], budget) {
                    CacheEntry::Value(set) => Ok(Some(set)),
                    CacheEntry::Vacant(_) => Ok(None),
                    CacheEntry::Timeout => Err(DataServerError::DeadlineExceeded),
                }
            })??;
            if let Some(set) = waited {
                CELL_SET_CACHE.record_hit();
                sets[i] = Some(set);
            }
        }
        let cell_sets: Vec<_> = window
            .iter()
            .zip(sets)
            .map(|(entry, set)| (entry.volume.time, set.expect("every set taken or built")))
            .collect();
        let tracks = track_cells(&cell_sets, &query.tracking);
        Ok(CellProduct { cell_sets, tracks })
    }

    /// Build the cell set a caller claimed (`fill`) and publish it to the
    /// callers waiting for it. On an error the claim is dropped, and one of
    /// them builds the set instead.
    fn fill_cell_set(
        &self,
        fill: CacheFillGuard<'_, CellSetKey, Arc<CellSet>>,
        entry: &VolumeEntry,
        quantity: &str,
        dims: [usize; 3],
        query: &CellQuery,
        handle: Option<&tokio::runtime::Handle>,
    ) -> Result<Arc<CellSet>, DataServerError> {
        CELL_SET_CACHE.record_miss();
        let (grid, valid) = self.voxel_grid_cached(entry, quantity, dims, handle)?;
        let set = Arc::new(segment(
            &grid,
            valid,
            entry.volume.time,
            quantity,
            &query.extraction,
        ));
        let _ = fill.insert(set.clone());
        Ok(set)
    }

    /// Render the derived **`CELLS`** overlay layer: the selected scan's
    /// footprint outlines + centroid markers, and the tracking window's
    /// centroid trajectories, painted into a [`RasterTile`] at each cell's
    /// max dBZ (so the collection's radar colormap styles the overlay; a
    /// `[[wms.parameters]]` entry named `CELLS` overrides it).
    ///
    /// Geometry vertices are projected **per-vertex** via
    /// [`OutputCrs::world_to_fraction`] (a few hundred points, never per
    /// pixel); painting clips to the tile, so an off-coverage viewport
    /// renders an empty (transparent) tile. Runs under `spawn_blocking`
    /// like every `get_raster_tile` path.
    pub(crate) fn cells_raster_tile(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<chrono::DateTime<chrono::Utc>>,
        output_crs: &OutputCrs,
    ) -> Result<RasterTile, DataServerError> {
        let query = layer_query(&self.cells_quantity()?, time);
        let handle = blocking_pixel_handle();
        let product = self.cell_product(&query, handle.as_ref())?;

        let mut values = vec![None; width as usize * height as usize];
        let mut canvas = Canvas::new(&mut values, width, height).ok_or_else(|| {
            DataServerError::Engine(format!(
                "[{}] invalid raster dimensions {width}×{height}",
                self.collection_id
            ))
        })?;
        paint_cell_product(&mut canvas, &product, |lon, lat| {
            let (fx, fy) = output_crs.world_to_fraction(bbox, lon, lat);
            (fx * width as f64, fy * height as f64)
        });
        Ok(RasterTile {
            width,
            height,
            values: values.into(),
        })
    }
}

impl PolarVolumeSiteView {
    /// The `CELLS` overlay's source quantity: corrected reflectivity first,
    /// then total, then any other dBZ-unit moment. The CELLS parameter is
    /// only advertised for sites that have one, so a miss here is a catalog
    /// race, reported as no-data like the other paths.
    fn cells_quantity(&self) -> Result<String, DataServerError> {
        let catalog = self.catalog.load();
        let meta = catalog.by_site_meta.get(&self.nod).ok_or_else(|| {
            DataServerError::LocationNotFound(format!(
                "[{}] radar site `{}` has no current volumes",
                self.collection_id, self.nod
            ))
        })?;
        layer_quantity(&meta.quantities)
            .map(str::to_string)
            .ok_or_else(|| {
                DataServerError::LocationNotFound(format!(
                    "[{}] no reflectivity quantity to derive storm cells from",
                    self.collection_id
                ))
            })
    }

    /// The storm cells the `CELLS` overlay draws at a picked pixel, for WMS
    /// GetFeatureInfo (#1100): from the product a tile at `pick.time` draws
    /// (`layer_query`, the same scan and tracking window, through the same
    /// [`Self::cell_product`] and its cache), the target scan's cells whose
    /// footprint outline contains the pixel's centre. Footprints never
    /// overlap, so that is at most one cell, unless simplification makes
    /// two outlines touch; then the larger first (label order).
    ///
    /// Runs on the query runtime GetFeatureInfo reads on, where
    /// `block_in_place` is valid, so it passes no `spawn_blocking` handle
    /// (`blocking_pixel_handle`'s contract).
    pub(crate) fn pick_cells(
        &self,
        pick: &FeaturePick<'_>,
    ) -> Result<PickedFeatures, DataServerError> {
        let query = layer_query(&self.cells_quantity()?, pick.time);
        let product = self.cell_product(&query, None)?;
        Ok(picked_cells(&product, pick))
    }
}

/// The target scan's cells of `product` whose outline holds `pick`'s
/// centre, each with its track's motion: the Features-style properties of
/// a tracked cell that the per-site product has.
fn picked_cells(product: &CellProduct, pick: &FeaturePick<'_>) -> PickedFeatures {
    let (target_time, target) = product.target();
    let hits: Vec<(&StormCell, Geometry)> = target
        .cells
        .iter()
        .map(|cell| {
            let outline = Geometry::Polygon {
                exterior: cell.footprint.clone(),
                holes: Vec::new(),
            };
            (cell, outline)
        })
        .filter(|(_, outline)| pick.hits(outline))
        .collect();
    let number_matched = hits.len();
    let features = hits
        .into_iter()
        .take(pick.limit)
        .map(|(cell, outline)| {
            let track = product.tracks.track_for(*target_time, cell.label);
            picked_cell(cell, outline, track, *target_time)
        })
        .collect();
    PickedFeatures {
        features,
        number_matched,
    }
}

/// One picked cell: its track's id, its outline, and its properties under
/// the names the nowcast's cell Features use where it has the same fact,
/// at the precision a reader needs. Severity and significance are the
/// nowcast tracker's facts and are not served here: the per-site product
/// has neither.
fn picked_cell(
    cell: &StormCell,
    outline: Geometry,
    track: Option<&Track>,
    observed: DateTime<Utc>,
) -> PickedFeature {
    fn round(value: f64, places: i32) -> PropertyValue {
        let scale = 10f64.powi(places);
        PropertyValue::Float((value * scale).round() / scale)
    }
    let motion = track.and_then(Track::speed_direction);
    let id = track.map_or_else(
        || format!("{}-{}", observed.format("%Y%m%dT%H%M%SZ"), cell.label),
        |track| track.id.clone(),
    );
    PickedFeature {
        id,
        geometry: Arc::new(outline),
        properties: vec![
            (
                "observed".into(),
                PropertyValue::String(observed.to_rfc3339_opts(SecondsFormat::Secs, true)),
            ),
            ("max_dbz".into(), round(cell.max_dbz, 1)),
            ("area_km2".into(), round(cell.area_km2, 1)),
            (
                "speed_ms".into(),
                motion.map_or(PropertyValue::Null, |(speed, _)| round(speed, 1)),
            ),
            (
                "bearing_deg".into(),
                motion.map_or(PropertyValue::Null, |(_, bearing)| round(bearing, 0)),
            ),
            ("echo_top_m".into(), round(cell.echo_top_m, 0)),
            ("base_m".into(), round(cell.base_m, 0)),
            ("volume_km3".into(), round(cell.volume_km3, 1)),
            ("vil_kg_m2".into(), round(cell.max_vil_kg_m2, 1)),
        ],
    }
}

/// Segment `volume` for the `CELLS` layer from its file's `bytes` and
/// publish the cell set to `fill`, off the request path (#1155). The
/// quantity's sweeps are decoded into memory of the build's own, not the
/// pixel cache, and the grid is not kept: only the cell set, a few KB, is.
/// Best-effort: a volume that does not decode is left to the tiles.
fn warm_one(
    fill: CacheFillGuard<'_, CellSetKey, Arc<CellSet>>,
    source: &Source,
    file_id: &str,
    volume: &PolarVolume,
    quantity: &str,
    bytes: &Bytes,
) -> bool {
    let query = layer_query(quantity, None);
    let dims = query.dims.unwrap_or(DEFAULT_VOXEL_DIMS);
    match voxel_grid_from_bytes(source, file_id, volume, quantity, dims, bytes) {
        Ok((grid, valid)) => {
            let set = segment(&grid, valid, volume.time, quantity, &query.extraction);
            let _ = fill.insert(Arc::new(set));
            true
        }
        Err(e) => {
            tracing::debug!("PVOL cell-set warm-up of `{file_id}` skipped: {e}");
            false
        }
    }
}

/// The key of `file_id`'s cell set in the `CELLS` layer's tracking windows.
fn layer_key(source: &Source, file_id: &str, quantity: &str) -> CellSetKey {
    let query = layer_query(quantity, None);
    let dims = query.dims.unwrap_or(DEFAULT_VOXEL_DIMS);
    cell_set_key(source, file_id, quantity, dims, &query.extraction)
}

/// Build the `CELLS` layer's cell set of a volume the poll loop just
/// downloaded, from the `bytes` it holds, unless it is cached or a tile is
/// building it (#1155). On the background poll runtime, never a request
/// worker. Returns whether it built one.
pub(crate) fn warm_from_bytes(
    source: &Source,
    file_id: &str,
    volume: &PolarVolume,
    bytes: &Bytes,
) -> bool {
    let mut quantities: Vec<&str> = volume
        .sweeps
        .iter()
        .flat_map(|sweep| sweep.moments.iter().map(|m| m.quantity.as_str()))
        .collect();
    quantities.sort_unstable();
    quantities.dedup();
    let Some(quantity) = layer_quantity(&quantities) else {
        return false;
    };
    let key = layer_key(source, file_id, quantity);
    match CELL_SET_CACHE.get_value_or_guard_untracked(&key, Duration::ZERO) {
        CacheEntry::Vacant(fill) => warm_one(fill, source, file_id, volume, quantity, bytes),
        CacheEntry::Value(_) | CacheEntry::Timeout => false,
    }
}

/// Build the cell sets the newest `CELLS` window of each site still lacks
/// (#1155): every site's newest volume first, then every site's second
/// newest, and so on, at most [`MAX_WARMS_PER_POLL`] per call, downloading
/// [`WARM_DOWNLOADS`] volume files at a time. The poll loop runs it on the
/// background runtime after each scan, and once as it starts, so a viewport
/// rarely meets a cold window. A volume is claimed only once downloaded, for
/// its segmentation alone, so no tile waits on a download that has no
/// deadline; one a tile claimed meanwhile is left to it. Returns the number
/// of cell sets built.
pub(crate) fn warm_windows(source: &Source, catalog: &Catalog) -> usize {
    let windows: Vec<(&str, &[VolumeEntry])> = catalog
        .sorted_nods
        .iter()
        .filter_map(|nod| {
            let quantity = layer_quantity(&catalog.by_site_meta.get(nod)?.quantities)?;
            let volumes = catalog.by_site.get(nod)?;
            Some((
                quantity,
                &volumes[volumes.len().saturating_sub(LAYER_WINDOW)..],
            ))
        })
        .collect();
    let mut cold = Vec::new();
    'newest_first: for depth in 0..LAYER_WINDOW {
        for &(quantity, window) in &windows {
            let Some(entry) = window.iter().rev().nth(depth) else {
                continue;
            };
            let key = layer_key(source, &entry.id, quantity);
            if CELL_SET_CACHE.contains_key(&key) {
                continue;
            }
            cold.push((quantity, entry, key));
            if cold.len() == MAX_WARMS_PER_POLL {
                break 'newest_first;
            }
        }
    }
    let mut built = 0;
    for batch in cold.chunks(WARM_DOWNLOADS) {
        let ids: Vec<&str> = batch
            .iter()
            .map(|(_, entry, _)| entry.id.as_str())
            .collect();
        let files = read_volume_files(source, &ids);
        for ((quantity, entry, key), bytes) in batch.iter().zip(files) {
            let bytes = match bytes {
                Ok(bytes) => bytes,
                Err(e) => {
                    tracing::debug!("PVOL cell-set warm-up of `{}` skipped: {e}", entry.id);
                    continue;
                }
            };
            if let CacheEntry::Vacant(fill) =
                CELL_SET_CACHE.get_value_or_guard_untracked(key, Duration::ZERO)
            {
                built += usize::from(warm_one(
                    fill,
                    source,
                    &entry.id,
                    &entry.volume,
                    quantity,
                    &bytes,
                ));
            }
        }
    }
    built
}

/// Paint a [`CellProduct`] into `canvas`: the target scan's footprint
/// outlines + centroid markers (at each cell's max dBZ), and the **connected**
/// track trails (at the reserved [`CELLS_TRACK_SENTINEL`]). `project` maps a
/// WGS84 `(lon, lat)` to output pixel coordinates.
///
/// Free function (no engine state) so the rendering rules — especially the
/// orphan-trail filter — are unit-testable against a synthetic `CellProduct`
/// without a multi-volume fixture.
fn paint_cell_product(
    canvas: &mut Canvas,
    product: &CellProduct,
    project: impl Fn(f64, f64) -> (f64, f64),
) {
    let (target_time, target) = product.target();
    for cell in &target.cells {
        // An empty footprint (degenerate mask — unreachable for a
        // BFS-produced component) simply paints nothing.
        let ring: Vec<(f64, f64)> = cell.footprint.iter().map(|v| project(v[0], v[1])).collect();
        canvas.stroke_ring(&ring, STROKE_PX, cell.max_dbz);
        canvas.paint_marker(
            project(cell.centroid[0], cell.centroid[1]),
            MARKER_HALF_PX,
            cell.max_dbz,
        );
    }
    // Trails only for cells that are present in the rendered (target) scan —
    // a trail must terminate at a visible outline. Drawing every track in the
    // window painted orphan lines for cells that had died or weakened out of
    // the latest scan (user-reported clutter). Trails are painted at the
    // reserved sentinel so the overlay colormap renders them one neutral
    // colour, not the cell's dBZ colour.
    for track in &product.tracks.tracks {
        if track.points.len() < 2 {
            continue;
        }
        if track.points.last().expect("non-empty track").time != *target_time {
            continue;
        }
        let line: Vec<(f64, f64)> = track.points.iter().map(|p| project(p.lon, p.lat)).collect();
        canvas.stroke_polyline(&line, TRACK_STROKE_PX, CELLS_TRACK_SENTINEL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use ds_core::cells::{CellSet, StormCell, Track, TrackPoint, TrackSet};
    use std::sync::Arc;

    fn t(min: i64) -> chrono::DateTime<chrono::Utc> {
        Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap() + chrono::Duration::minutes(min)
    }

    fn cell(label: u32, lon: f64, lat: f64) -> StormCell {
        // A small square footprint around (lon, lat); attributes are nominal.
        StormCell {
            label,
            max_dbz: 45.0,
            max_dbz_pos: [lon, lat, 3_000.0],
            centroid: [lon, lat, 3_000.0],
            echo_top_m: 8_000.0,
            base_m: 1_000.0,
            volume_km3: 50.0,
            area_km2: 25.0,
            max_vil_kg_m2: 10.0,
            footprint: vec![
                [lon - 0.2, lat - 0.2],
                [lon + 0.2, lat - 0.2],
                [lon + 0.2, lat + 0.2],
                [lon - 0.2, lat + 0.2],
                [lon - 0.2, lat - 0.2],
            ],
            bbox_enu_m: [0.0; 6],
        }
    }

    /// A track moving in a straight line from `(lon0, lat0)` at `t(first)` to
    /// `(lon1, lat1)` at `t(last)`, one point per 5-minute step. Distinct
    /// endpoints make the trail a real multi-pixel polyline (not a degenerate
    /// zero-length one), so the orphan-filter assertions are unambiguous.
    fn track(
        first: i64,
        last: i64,
        label: u32,
        (lon0, lat0): (f64, f64),
        (lon1, lat1): (f64, f64),
    ) -> Track {
        let steps: Vec<i64> = (first..=last).step_by(5).collect();
        let span = (steps.len().max(2) - 1) as f64;
        let points = steps
            .iter()
            .enumerate()
            .map(|(i, &m)| {
                let f = i as f64 / span;
                TrackPoint {
                    time: t(m),
                    lon: lon0 + f * (lon1 - lon0),
                    lat: lat0 + f * (lat1 - lat0),
                    height_m: 3_000.0,
                    label,
                    max_dbz: 45.0,
                }
            })
            .collect::<Vec<_>>();
        Track {
            id: format!("trk-{label}"),
            points,
            motion_ms: Some((1.0, 0.0)),
        }
    }

    /// WMS GetFeatureInfo on `CELLS` (#1100): the target scan's cell whose
    /// outline holds the pixel's centre, with its track's id and motion; a
    /// cell of an earlier scan of the window is neither drawn nor picked.
    #[test]
    fn picks_the_cell_whose_outline_holds_the_pixel() {
        use ds_core::feature::PropertyValue;
        let target_time = t(10);
        let earlier = Arc::new(CellSet {
            time: t(5),
            quantity: "DBZH".into(),
            threshold: 35.0,
            origin: [25.0, 60.0, 100.0],
            cells: vec![cell(1, 24.5, 60.25)],
        });
        let target = Arc::new(CellSet {
            time: target_time,
            quantity: "DBZH".into(),
            threshold: 35.0,
            origin: [25.0, 60.0, 100.0],
            cells: vec![cell(1, 25.0, 60.0), cell(2, 24.0, 60.5)],
        });
        let product = CellProduct {
            cell_sets: vec![(t(5), earlier), (target_time, target)],
            tracks: TrackSet {
                // Moving east at 1 m/s; cell 2 has no track.
                tracks: vec![track(0, 10, 1, (25.4, 60.0), (25.0, 60.0))],
            },
        };
        let pick = |lon: f64, lat: f64, limit: usize| FeaturePick {
            parameter: Some(CELLS_PARAMETER),
            map: ds_core::feature_pick::PickMap {
                output_crs: &OutputCrs::Wgs84,
                wgs84_bbox: [lon - 0.001, lat - 0.001, lon + 0.001, lat + 0.001],
                width: 1,
                height: 1,
            },
            pixel: (0, 0),
            point: (lon, lat),
            time: None,
            reference_time: None,
            z: None,
            limit,
        };
        let property = |feature: &PickedFeature, name: &str| {
            feature
                .properties
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| panic!("no {name}"))
        };

        let picked = picked_cells(&product, &pick(25.1, 60.1, 10));
        assert_eq!(picked.number_matched, 1);
        let cell = &picked.features[0];
        assert_eq!(cell.id, "trk-1");
        let names: Vec<&str> = cell.properties.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "observed",
                "max_dbz",
                "area_km2",
                "speed_ms",
                "bearing_deg",
                "echo_top_m",
                "base_m",
                "volume_km3",
                "vil_kg_m2"
            ]
        );
        assert_eq!(
            property(cell, "observed"),
            PropertyValue::String("2026-06-01T12:10:00Z".into())
        );
        assert_eq!(property(cell, "max_dbz"), PropertyValue::Float(45.0));
        assert_eq!(property(cell, "area_km2"), PropertyValue::Float(25.0));
        assert_eq!(property(cell, "speed_ms"), PropertyValue::Float(1.0));
        assert_eq!(property(cell, "bearing_deg"), PropertyValue::Float(90.0));
        assert_eq!(property(cell, "echo_top_m"), PropertyValue::Float(8000.0));
        assert_eq!(property(cell, "vil_kg_m2"), PropertyValue::Float(10.0));
        // The outline is the footprint ring.
        match &*cell.geometry {
            Geometry::Polygon { exterior, holes } => {
                assert_eq!(exterior.len(), 5);
                assert!(holes.is_empty());
            }
            other => panic!("{other:?}"),
        }

        // Untracked: an id from the scan and label, and no motion known.
        let picked = picked_cells(&product, &pick(24.0, 60.5, 10));
        assert_eq!(picked.features[0].id, "20260601T121000Z-2");
        assert_eq!(
            property(&picked.features[0], "speed_ms"),
            PropertyValue::Null
        );
        assert_eq!(
            property(&picked.features[0], "bearing_deg"),
            PropertyValue::Null
        );

        // Between the outlines, where only the earlier scan had a cell.
        let picked = picked_cells(&product, &pick(24.5, 60.25, 10));
        assert_eq!(picked.number_matched, 0);
        assert!(picked.features.is_empty());
    }

    /// The orphan-trail fix (#367 user report): a trail is painted only when
    /// its last observation is the rendered scan. A track that ended earlier
    /// (a dead/weakened cell) must NOT leave a line. The connected track's
    /// trail is painted at the sentinel; the orphan's is not.
    #[test]
    fn only_connected_trails_are_painted() {
        let target_time = t(10);
        // One cell visible in the target scan, at the centre.
        let target = Arc::new(CellSet {
            time: target_time,
            quantity: "DBZH".into(),
            threshold: 35.0,
            origin: [25.0, 60.0, 100.0],
            cells: vec![cell(1, 25.0, 60.0)],
        });
        let product = CellProduct {
            cell_sets: vec![(target_time, target)],
            tracks: TrackSet {
                tracks: vec![
                    // Connected: a real line ending at the target time, near
                    // the visible cell (right half of the canvas).
                    track(0, 10, 1, (25.4, 60.0), (25.0, 60.0)),
                    // Orphan: a real line that ended at t(5), in the left half
                    // — well clear of the connected trail.
                    track(0, 5, 2, (23.5, 60.6), (24.2, 60.4)),
                ],
            },
        };

        // 100×100 canvas over a bbox that contains both tracks.
        let mut values = vec![None; 100 * 100];
        let bbox = [23.0, 59.0, 26.0, 61.0];
        let project = |lon: f64, lat: f64| {
            let fx = (lon - bbox[0]) / (bbox[2] - bbox[0]);
            let fy = (bbox[3] - lat) / (bbox[3] - bbox[1]);
            (fx * 100.0, fy * 100.0)
        };
        {
            let mut canvas = Canvas::new(&mut values, 100, 100).unwrap();
            paint_cell_product(&mut canvas, &product, project);
        }

        let sentinel_px = values
            .iter()
            .filter(|v| **v == Some(CELLS_TRACK_SENTINEL))
            .count();
        let outline_px = values.iter().filter(|v| **v == Some(45.0)).count();
        assert!(
            outline_px > 0,
            "the visible cell's outline + marker is painted"
        );
        // The connected trail is a multi-pixel line, not one degenerate pixel.
        assert!(
            sentinel_px > 2,
            "the connected track leaves a real sentinel-valued trail, got {sentinel_px}px"
        );

        // No sentinel pixel anywhere along the orphan track's path. Sample its
        // every point: a broken filter would draw a clear line through these.
        let orphan = track(0, 5, 2, (23.5, 60.6), (24.2, 60.4));
        for p in &orphan.points {
            let (px, py) = project(p.lon, p.lat);
            let (cx, cy) = (px.round() as i64, py.round() as i64);
            // Check a small neighbourhood around each orphan vertex.
            for dy in -2..=2 {
                for dx in -2..=2 {
                    let (x, y) = (cx + dx, cy + dy);
                    if (0..100).contains(&x) && (0..100).contains(&y) {
                        assert_ne!(
                            values[(y * 100 + x) as usize],
                            Some(CELLS_TRACK_SENTINEL),
                            "orphan track (ended before the rendered scan) must leave no trail \
                             near ({x}, {y})"
                        );
                    }
                }
            }
        }
    }

    /// A single-observation track (just born) never draws a trail.
    #[test]
    fn single_point_track_draws_no_trail() {
        let target_time = t(0);
        let target = Arc::new(CellSet {
            time: target_time,
            quantity: "DBZH".into(),
            threshold: 35.0,
            origin: [25.0, 60.0, 100.0],
            cells: vec![cell(1, 25.0, 60.0)],
        });
        let product = CellProduct {
            cell_sets: vec![(target_time, target)],
            tracks: TrackSet {
                tracks: vec![track(0, 0, 1, (25.0, 60.0), (25.0, 60.0))], // one point
            },
        };
        let mut values = vec![None; 64 * 64];
        {
            let mut canvas = Canvas::new(&mut values, 64, 64).unwrap();
            paint_cell_product(&mut canvas, &product, |lon, lat| {
                let fx = (lon - 23.0) / 3.0;
                let fy = (61.0 - lat) / 2.0;
                (fx * 64.0, fy * 64.0)
            });
        }
        assert_eq!(
            values
                .iter()
                .filter(|v| **v == Some(CELLS_TRACK_SENTINEL))
                .count(),
            0,
            "a one-point track has no trajectory to draw"
        );
    }

    /// Heap bytes a cached [`CellSet`] actually holds: the cell array and
    /// every footprint ring, at their allocated capacity.
    fn cell_set_allocation(set: &CellSet) -> usize {
        set.cells.capacity() * std::mem::size_of::<StormCell>()
            + set
                .cells
                .iter()
                .map(|c| c.footprint.capacity() * std::mem::size_of::<[f64; 2]>())
                .sum::<usize>()
    }

    /// #1052: the cell-set cache weight must cover the allocation an entry
    /// holds, not just its length. Real segmentation output grows its cell
    /// array by `push` and closes each footprint ring with a `push` onto an
    /// exact-size `Vec`, so both carry slack.
    #[test]
    fn cell_set_weight_covers_its_allocation() {
        let key: CellSetKey = (
            Arc::from("src|vol.h5"),
            Arc::from("DBZH"),
            [20, 36, 10],
            (0, 0, 0),
        );

        // Real segmentation output: one 45 dBZ blob.
        let (n_r, n_a, n_h) = (20, 36, 10);
        let mut values = vec![f32::NAN; n_r * n_a * n_h];
        for i_r in 4..8 {
            for i_a in 9..12 {
                for i_h in 2..6 {
                    values[(i_r * n_a + i_a) * n_h + i_h] = 45.0;
                }
            }
        }
        let grid = ds_core::volume::VoxelGrid {
            origin_lon: 24.5,
            origin_lat: 60.5,
            origin_height: 100.0,
            dims: [n_r, n_a, n_h],
            radius_range: [0.0, 100_000.0],
            angle_range: [0.0, std::f64::consts::TAU],
            height_range: [0.0, 10_000.0],
            values,
            quantity: "DBZH".into(),
            unit: "dBZ".into(),
        };
        let opts = ds_core::cells::CellExtractionOptions {
            threshold: 35.0,
            min_volume_km3: 0.0,
            max_cells: 256,
        };
        let extracted = Arc::new(extract_cells(&grid, t(0), &opts));
        assert_eq!(extracted.cells.len(), 1, "the blob segments as one cell");

        // Spare capacity made explicit, so the footprint term is pinned on
        // its own whatever `ds_core::cells` happens to reserve.
        let mut spare = cell(1, 25.0, 60.0);
        spare.footprint.reserve_exact(64);
        let mut cells = Vec::with_capacity(8);
        cells.push(spare);
        let spare = Arc::new(CellSet {
            time: t(0),
            quantity: "DBZH".into(),
            threshold: 35.0,
            origin: [25.0, 60.0, 100.0],
            cells,
        });

        for set in [&extracted, &spare] {
            let weight = cell_set_weight_bytes(&key, set);
            let allocated = cell_set_allocation(set) as u64;
            assert!(
                weight >= allocated,
                "weight {weight} B under-counts a {allocated} B allocation"
            );
        }
    }

    /// The cold-batch fixture (two sweeps of `DBZH` and `VRAD`) as a volume
    /// of site `nod`, four characters, timed 12:`minute` on its date: its
    /// `/what/source` and `/what/time` attributes are fixed-length strings
    /// that occur once in the file.
    fn scan_bytes(nod: &str, minute: usize) -> Vec<u8> {
        const FIXTURE: &[u8] = include_bytes!("../../../testdata/pvol-cold-batch.h5");
        let mut bytes = FIXTURE.to_vec();
        for (from, to) in [
            ("NOD:test".to_string(), format!("NOD:{nod}")),
            ("120000".to_string(), format!("12{minute:02}00")),
        ] {
            assert_eq!(from.len(), to.len());
            let at = bytes
                .windows(from.len())
                .position(|w| w == from.as_bytes())
                .unwrap();
            bytes[at..at + to.len()].copy_from_slice(to.as_bytes());
        }
        bytes
    }

    /// An engine over a fresh in-memory store under a prefix of its own,
    /// holding `scans` volumes five minutes apart for each site of `nods`,
    /// plus the store to add volumes to and the engine's view of it.
    fn remote_engine(
        nods: &[&str],
        scans: usize,
    ) -> (
        crate::PolarVolumeEngine,
        Arc<ds_storage::object_store::memory::InMemory>,
        ds_storage::DataStore,
        String,
    ) {
        remote_engine_with_delay(nods, scans, Duration::ZERO)
    }

    /// [`remote_engine`] whose store waits `delay` before every GET.
    fn remote_engine_with_delay(
        nods: &[&str],
        scans: usize,
        delay: Duration,
    ) -> (
        crate::PolarVolumeEngine,
        Arc<ds_storage::object_store::memory::InMemory>,
        ds_storage::DataStore,
        String,
    ) {
        use ds_storage::object_store::throttle::{ThrottleConfig, ThrottledStore};
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let prefix = format!("warm-{}-{n}/", std::process::id());
        let inner = Arc::new(ds_storage::object_store::memory::InMemory::new());
        for nod in nods {
            for i in 0..scans {
                put_scan(&inner, &prefix, nod, 5 * i);
            }
        }
        let config = ThrottleConfig {
            wait_get_per_call: delay,
            ..ThrottleConfig::default()
        };
        let throttled = ThrottledStore::new(inner.clone(), config);
        let store = ds_storage::DataStore::new(Arc::new(throttled));
        let engine =
            crate::PolarVolumeEngine::new_remote_for_test("warm", store.clone(), &prefix).unwrap();
        (engine, inner, store, prefix)
    }

    fn put_scan(
        store: &ds_storage::object_store::memory::InMemory,
        prefix: &str,
        nod: &str,
        minute: usize,
    ) {
        use ds_storage::object_store::{ObjectStoreExt, PutPayload};
        let key = format!("{prefix}2026091612{minute:02}_{nod}_PVOL.h5");
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(store.put(
                &ds_storage::object_store::path::Path::from(key),
                PutPayload::from(scan_bytes(nod, minute)),
            ))
            .unwrap();
    }

    /// Whether the `CELLS` layer's cell set of each volume of `nod` is
    /// cached, oldest first.
    fn warm_by_volume(engine: &crate::PolarVolumeEngine, nod: &str) -> Vec<bool> {
        engine.catalog.load().by_site[nod]
            .iter()
            .map(|e| CELL_SET_CACHE.contains_key(&layer_key(&engine.source, &e.id, "DBZH")))
            .collect()
    }

    /// #1155: the poll-time warm-up builds the cell sets of every site's
    /// newest `CELLS` window, newest volumes first across sites, at most
    /// `MAX_WARMS_PER_POLL` per call; the next call finishes the windows.
    /// It reads each file once, keeps nothing in the pixel cache beyond
    /// the scan's pre-warmed base sweep, and a `CELLS` tile then reads no
    /// volume file.
    #[test]
    fn warm_up_builds_the_newest_windows_first_within_its_bound() {
        let nods = ["sa01", "sa02", "sa03", "sa04", "sa05"];
        assert!(nods.len() * LAYER_WINDOW > MAX_WARMS_PER_POLL);
        let (engine, _, store, _) = remote_engine(&nods, LAYER_WINDOW + 1);
        let file = scan_bytes("sa01", 0).len() as u64;
        let read = store.bytes_read();
        let catalog = engine.catalog.load();

        assert_eq!(warm_windows(&engine.source, &catalog), MAX_WARMS_PER_POLL);
        assert_eq!(store.bytes_read() - read, MAX_WARMS_PER_POLL as u64 * file);
        // Volume 0 is outside every window; volume 1 is the oldest in it.
        // Its build waits for the next call in the sites sorted last.
        for (i, nod) in nods.iter().enumerate() {
            let oldest_warm = i < MAX_WARMS_PER_POLL - (LAYER_WINDOW - 1) * nods.len();
            let mut want = vec![false, oldest_warm];
            want.extend([true; LAYER_WINDOW - 1]);
            assert_eq!(warm_by_volume(&engine, nod), want, "{nod}");
        }
        assert_eq!(warm_windows(&engine.source, &catalog), 3);
        assert_eq!(warm_windows(&engine.source, &catalog), 0);
        for nod in nods {
            let mut want = vec![true; LAYER_WINDOW + 1];
            want[0] = false;
            assert_eq!(warm_by_volume(&engine, nod), want, "{nod}");
        }

        let volume = &catalog.by_site["sa01"][1];
        let cache_id = pixel_cache_id(&engine.source, &volume.id);
        let pixels = crate::volume_engine::pixel_cache();
        for sweep in &volume.volume.sweeps[1..] {
            for moment in &sweep.moments {
                assert!(
                    !pixels.contains(&cache_id, &moment.dataset_path),
                    "the warm-up decoded `{}` into the pixel cache",
                    moment.dataset_path
                );
            }
        }

        let read = store.bytes_read();
        let view = engine.site_view("sa03", "warm-sa03");
        let product = view.cell_product(&layer_query("DBZH", None), None).unwrap();
        assert_eq!(product.cell_sets.len(), LAYER_WINDOW);
        assert_eq!(store.bytes_read(), read, "a warm window reads no file");
    }

    /// #1155: a poll that downloads a site's new newest volume segments it
    /// from the bytes it downloaded: the new window is warm after one GET
    /// of the new file, not a second one.
    #[test]
    fn a_poll_segments_its_new_volume_from_the_bytes_in_hand() {
        let (engine, inner, store, prefix) = remote_engine(&["sb01"], LAYER_WINDOW);
        let file = scan_bytes("sb01", 0).len() as u64;
        assert_eq!(
            warm_windows(&engine.source, &engine.catalog.load()),
            LAYER_WINDOW
        );
        put_scan(&inner, &prefix, "sb01", 5 * LAYER_WINDOW);
        let read = store.bytes_read();
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
            .block_on(engine.poll_once());
        assert_eq!(
            engine.catalog.load().by_site["sb01"].len(),
            LAYER_WINDOW + 1
        );
        assert!(warm_by_volume(&engine, "sb01").iter().all(|&warm| warm));
        assert_eq!(store.bytes_read() - read, file, "one GET of the new volume");
    }

    /// #1155: when the tile building a cell set fails, here at its
    /// deadline, a tile waiting for that build builds the set itself
    /// instead of failing with it.
    #[test]
    fn a_waiting_tile_builds_the_set_when_its_builder_fails() {
        let get = Duration::from_millis(300);
        let (engine, ..) = remote_engine_with_delay(&["sc01"], 1, get);
        let view = Arc::new(engine.site_view("sc01", "warm-sc01"));
        let query = layer_query("DBZH", None);
        let builder = {
            let (view, query) = (view.clone(), query.clone());
            std::thread::spawn(move || {
                let _deadline = ds_core::deadline::enter(Some(Instant::now() + get / 3));
                view.cell_product(&query, None).map(|_| ())
            })
        };
        // The builder holds the claim, its GET under way.
        std::thread::sleep(get / 10);
        let started = Instant::now();
        let product = view.cell_product(&query, None).unwrap();
        assert!(matches!(
            builder.join().unwrap(),
            Err(DataServerError::DeadlineExceeded)
        ));
        assert_eq!(product.cell_sets.len(), 1);
        assert!(
            started.elapsed() >= get,
            "the waiter downloaded the volume itself"
        );
    }
}
