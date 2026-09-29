//! Geostationary satellite imagery (#819): GOES-R ABI NetCDF-4 scans served
//! through WMS, OGC API Maps and Tiles.
//!
//! One collection is one satellite and sector; each configured product (an
//! ABI band, or an L2 field such as cloud top temperature) is a parameter
//! with its own time axis. The poll loop lists the source, downloads each
//! new scan whole and keeps it in memory ([`cache::FRAMES`]); renders
//! decode only the blocks they touch ([`cache::STRIPS`]) or, zoomed out,
//! sample the overview built at ingest.

mod cache;
mod frame;
mod naming;
mod render;
mod source;

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use ds_core::config::SatelliteConfig;
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::feature::{
    check_area_budget, check_mask_budget, parse_area_coords, parse_point_coords, MAX_AREA_DIM,
};
use ds_core::map_engine::{
    select_common_time, CompositeDef, MapEngine, OutputCrs, ParameterInfo, RasterInfo, RasterTile,
};
use ds_core::model::{
    CoverageResponse, DomainDescription, Location, NdArray, ParameterDescription, QueryResult,
};
use ds_core::resample::ProjectionGrid;
use ds_poll::{FirstTick, Shutdown};
use ds_storage::discovery::TimeWindow;
use ds_storage::object_store::path::Path as ObjectPath;

pub use cache::{frame_metrics, strip_metrics};

use cache::{BlockKey, FrameKey, FRAMES, STRIPS};
use frame::{Frame, FrameOptions};
use naming::Naming;
use source::Source;

/// New scans ingested per product per poll, newest first. Bootstrapping a
/// window of many scans spreads over a few polls instead of one long
/// sequential download (Critical Rule 9).
const MAX_INGEST_PER_POLL: usize = 4;

/// Scans one EDR query may download because the cache evicted them (a
/// 2 km full disk is ~25 MB): checked before the first fetch, so an
/// unfiltered query on a long catalog cannot chain sequential downloads
/// (Critical Rule 9).
const MAX_QUERY_FETCHES: usize = 8;

/// Blocks one EDR query may decode, over all its products and scans (~0.7 ms
/// and ~260 KB each for a 2 km full-width GOES-R strip). A position reads one
/// block per scan; an area the blocks its polygon's bbox covers, per product
/// grid.
const MAX_QUERY_BLOCKS: usize = 1024;

/// The files of one scan: one, or a mosaic's tiles.
type Scan = Arc<[ObjectPath]>;

/// How long after its start a tiled scan is ingested even if tiles are
/// still missing: a Himawari full disk is published within ~9 minutes.
const TILED_SCAN_SETTLE: chrono::Duration = chrono::Duration::minutes(15);

/// Whether a tiled scan starting at `time` with `tiles` files listed is
/// complete enough to ingest: it has as many tiles as earlier scans had
/// (`expected`), a newer scan has started publishing, or it has settled.
/// Until then it may still be arriving, and a mosaic ingested early would
/// keep its holes.
fn tiled_scan_ready(
    time: DateTime<Utc>,
    tiles: usize,
    expected: Option<usize>,
    newest: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> bool {
    expected.is_some_and(|n| tiles >= n)
        || newest.is_some_and(|t| t > time)
        || now - time >= TILED_SCAN_SETTLE
}

/// One configured product, served as one parameter.
struct Product {
    parameter: Arc<str>,
    variable: String,
    naming: Naming,
    /// Packed validity for files that declare none (ISatSS).
    valid_fallback: Option<(i32, i32)>,
}

/// A consistent snapshot of what is served, swapped whole by each poll.
struct Catalog {
    /// Per product (config order): scan start → file.
    frames: Vec<BTreeMap<DateTime<Utc>, Scan>>,
    /// Per product: its scan starts, for `parameter_times` (O(1)).
    times: Vec<Arc<[DateTime<Utc>]>>,
    /// Per composite: the scans every one of its bands has, for
    /// `parameter_times` (O(1)). Rebuilt with the snapshot, so it follows
    /// every ingest and eviction.
    composite_times: Vec<Arc<[DateTime<Utc>]>>,
    /// Per product: the CRS84 extent of its grid, from its first scan.
    extents: Vec<Option<[f64; 4]>>,
    /// Per product: its grid size, from its first scan.
    grids: Vec<Option<[u32; 2]>>,
    info: Arc<RasterInfo>,
    /// When the last poll finished listing every product.
    polled_at: Option<DateTime<Utc>>,
}

pub struct SatelliteEngine {
    collection_id: Arc<str>,
    source: Source,
    products: Vec<Product>,
    parameters: Vec<ParameterInfo>,
    /// RGB composites served as their own layers, in config order.
    composites: Arc<[CompositeDef]>,
    /// Per composite: the product indices of its bands, in
    /// [`CompositeDef::parameters`] order.
    composite_bands: Vec<Vec<usize>>,
    window: Option<TimeWindow>,
    poll_interval: Duration,
    catalog: ArcSwap<Catalog>,
    shutdown: Shutdown,
}

impl SatelliteEngine {
    /// Build the engine. No I/O: the poll loop's first, immediate tick lists
    /// the source and ingests the newest scans.
    pub fn new(collection_id: &str, config: &SatelliteConfig) -> Result<Self, DataServerError> {
        ds_core::config::validate_satellite(collection_id, config)?;
        let window = config
            .time_window
            .as_deref()
            .map(TimeWindow::parse)
            .transpose()?;
        let products: Vec<Product> = config
            .products
            .iter()
            .map(|p| Product {
                parameter: p.parameter.as_str().into(),
                variable: p.variable.clone(),
                naming: match config.provider.as_str() {
                    "isatss" => Naming::isatss(
                        &p.product,
                        p.band.expect("validate_satellite requires an ISatSS band"),
                    ),
                    _ => Naming::goes_r(&p.product, p.band),
                },
                // ISatSS writes space as ~0 K and declares no fill or range:
                // below the packing offset is no measurement.
                valid_fallback: (config.provider == "isatss").then_some((0, i16::MAX as i32)),
            })
            .collect();
        let source = match (&config.data_path, &config.endpoint, &config.bucket) {
            (Some(path), _, _) => {
                let (store, base) = ds_storage::build_store(path)?;
                Source::Directory { store, base }
            }
            (None, Some(endpoint), Some(bucket)) => {
                for product in &products {
                    product.naming.validate_window(window.as_ref())?;
                }
                Source::Bucket {
                    store: ds_storage::build_s3_store_from_parts(endpoint, bucket)?,
                }
            }
            _ => unreachable!("validate_satellite requires a source"),
        };
        let parameters: Vec<ParameterInfo> = config
            .products
            .iter()
            .map(|p| ParameterInfo {
                name: p.parameter.clone(),
                title: p.title.clone(),
                unit: p.unit.clone(),
            })
            .collect();
        // A recipe resolves to the channels it stands for here, through the
        // same call `validate_satellite` checked it with.
        let composites: Arc<[CompositeDef]> = config
            .composites
            .iter()
            .map(|c| ds_core::config::satellite_composite_def(collection_id, config, c))
            .collect::<Result<_, _>>()?;
        let composite_bands: Vec<Vec<usize>> = composites
            .iter()
            .map(|def| {
                def.parameters()
                    .into_iter()
                    .map(|name| {
                        products
                            .iter()
                            .position(|p| &*p.parameter == name)
                            .expect("validate_satellite requires composites to read products")
                    })
                    .collect()
            })
            .collect();
        let empty = vec![BTreeMap::new(); products.len()];
        let catalog = Catalog::build(
            empty,
            &parameters,
            vec![None; products.len()],
            vec![None; products.len()],
            &composite_bands,
        );
        Ok(Self {
            collection_id: collection_id.into(),
            source,
            products,
            parameters,
            composites,
            composite_bands,
            window,
            poll_interval: Duration::from_secs(config.poll_interval_secs),
            catalog: ArcSwap::from_pointee(catalog),
            shutdown: Shutdown::new(),
        })
    }

    /// Poll until [`Self::shutdown`]. Run on the background poll runtime
    /// (Critical Rule 6): each tick does blocking listing and downloads.
    pub async fn poll_loop(&self) {
        let mut ticker = self
            .shutdown
            .ticker(self.poll_interval, FirstTick::Immediate);
        while ticker.tick().await {
            self.poll_once();
        }
        tracing::info!("[{}] satellite poll loop shutting down", self.collection_id);
    }

    pub fn shutdown(&self) {
        self.shutdown.shutdown();
    }

    pub fn collection_id(&self) -> &str {
        &self.collection_id
    }

    /// Live `/health` status: ready while every product has a scan and the
    /// source was listed recently.
    pub fn live_health(&self) -> Option<ds_core::health::LiveStatus> {
        use ds_core::health::LiveStatus;
        let catalog = self.catalog.load();
        let Some(polled_at) = catalog.polled_at else {
            return Some(LiveStatus::Degraded {
                reason: "waiting for the first poll",
            });
        };
        let stale_after = chrono::Duration::from_std(self.poll_interval * 5)
            .unwrap_or(chrono::Duration::MAX)
            .max(chrono::Duration::minutes(10));
        Some(if Utc::now() - polled_at > stale_after {
            LiveStatus::Degraded {
                reason: "polling the source has failed repeatedly",
            }
        } else if catalog.frames.iter().any(BTreeMap::is_empty) {
            LiveStatus::Degraded {
                reason: "a product has no scans in the time window",
            }
        } else {
            LiveStatus::Ready
        })
    }

    /// Age of the newest scan, for `/health`'s `data_age_secs`.
    pub fn data_age(&self) -> Option<chrono::Duration> {
        let newest = self.catalog.load().info.times.last().copied()?;
        Some(Utc::now() - newest)
    }

    /// Every served scan start (the union over products), for `/health`.
    pub fn times(&self) -> Vec<DateTime<Utc>> {
        self.catalog.load().info.times.clone()
    }

    /// `(products with at least one scan, products, newest scan start, last
    /// completed poll)`, for `/health`.
    pub fn status(&self) -> (usize, usize, Option<DateTime<Utc>>, Option<DateTime<Utc>>) {
        let catalog = self.catalog.load();
        let with_data = catalog.frames.iter().filter(|f| !f.is_empty()).count();
        (
            with_data,
            self.products.len(),
            catalog.info.times.last().copied(),
            catalog.polled_at,
        )
    }

    /// List every product, drop scans that left the window, and ingest up
    /// to [`MAX_INGEST_PER_POLL`] new scans per product, newest first.
    pub fn poll_once(&self) {
        let now = Utc::now();
        let window = self.window.as_ref().map(|w| w.to_range(now));
        let old = self.catalog.load();
        let mut frames = old.frames.clone();
        let mut extents = old.extents.clone();
        let mut grids = old.grids.clone();
        let mut complete = true;
        let mut listings = source::Listings::new();
        for (index, product) in self.products.iter().enumerate() {
            let known = |time| frames[index].contains_key(&time);
            let found = match self
                .source
                .list(&product.naming, window, known, &mut listings)
            {
                Ok(found) => found,
                Err(e) => {
                    complete = false;
                    tracing::warn!(
                        "[{}] listing '{}' failed (keeping its scans): {e}",
                        self.collection_id,
                        product.parameter
                    );
                    continue;
                }
            };
            if let Some((start, _)) = window {
                frames[index].retain(|time, _| *time >= start);
            }
            let known = &frames[index];
            // Evidence of a complete scan's tile count: the scans held and
            // those listed before the newest (which may still be arriving).
            let expected = known
                .values()
                .map(|scan| scan.len())
                .chain(found.iter().rev().skip(1).map(|f| f.paths.len()))
                .max();
            let newest = found.last().map(|f| f.time);
            let mut new: Vec<_> = found
                .into_iter()
                .filter(|f| {
                    !known.contains_key(&f.time)
                        && (!product.naming.tiled()
                            || tiled_scan_ready(f.time, f.paths.len(), expected, newest, now))
                })
                .collect();
            new.reverse();
            new.truncate(MAX_INGEST_PER_POLL);
            for scan in new {
                match self.ingest(index, scan.time, &scan.paths) {
                    Ok(frame) => {
                        if extents[index].is_none() {
                            extents[index] = ds_core::geo::crs84_extent(frame.gt.bbox());
                        }
                        grids[index].get_or_insert([frame.gt.width, frame.gt.height]);
                        frames[index].insert(scan.time, scan.paths);
                    }
                    Err(e) => tracing::warn!(
                        "[{}] skipping {} scan {} ({} file(s) from {}): {e}",
                        self.collection_id,
                        product.parameter,
                        scan.time,
                        scan.paths.len(),
                        scan.paths[0]
                    ),
                }
            }
        }
        let polled_at = if complete { Some(now) } else { old.polled_at };
        let added: usize = frames
            .iter()
            .zip(&old.frames)
            .map(|(new, old)| new.keys().filter(|t| !old.contains_key(t)).count())
            .sum();
        if added > 0 {
            tracing::info!(
                "[{}] ingested {added} scan(s); serving {}",
                self.collection_id,
                self.products
                    .iter()
                    .zip(&frames)
                    .map(|(p, f)| format!("{} ×{}", p.parameter, f.len()))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let catalog = Catalog::build(
            frames,
            &self.parameters,
            extents,
            grids,
            &self.composite_bands,
        );
        self.catalog.store(Arc::new(Catalog {
            polled_at,
            ..catalog
        }));
    }

    /// Download one scan, parse it and cache it.
    fn ingest(
        &self,
        index: usize,
        time: DateTime<Utc>,
        scan: &Scan,
    ) -> Result<Arc<Frame>, DataServerError> {
        let frame = Arc::new(self.open(index, scan)?);
        FRAMES.insert(self.frame_key(index, time), frame.clone());
        Ok(frame)
    }

    /// Download a scan's files and parse them.
    fn open(&self, index: usize, scan: &Scan) -> Result<Frame, DataServerError> {
        let product = &self.products[index];
        let options = FrameOptions {
            variable: &product.variable,
            valid_fallback: product.valid_fallback,
        };
        Frame::open(self.source.fetch(scan)?, options).map_err(DataServerError::Engine)
    }

    fn frame_key(&self, index: usize, time: DateTime<Utc>) -> FrameKey {
        FrameKey {
            collection: self.collection_id.clone(),
            parameter: self.products[index].parameter.clone(),
            time: time.timestamp(),
        }
    }

    /// The product a request names; `None` is the first.
    fn product_index(&self, parameter: Option<&str>) -> Result<usize, DataServerError> {
        match parameter {
            None => Ok(0),
            Some(name) => self
                .product_position(name)
                .ok_or_else(|| self.not_a_band(name)),
        }
    }

    /// The product named `name`, without building an error: for the
    /// per-request time lookups.
    fn product_position(&self, name: &str) -> Option<usize> {
        self.products.iter().position(|p| &*p.parameter == name)
    }

    /// The composite named `name`.
    fn composite_index(&self, name: &str) -> Option<usize> {
        self.composites.iter().position(|c| c.name == name)
    }

    /// Why `name` cannot be read as one band. A composite has no values of
    /// its own: the API layer renders its bands with `get_raster_tiles`.
    fn not_a_band(&self, name: &str) -> DataServerError {
        DataServerError::InvalidParameter(match self.composite_index(name) {
            Some(c) => format!(
                "'{name}' is an RGB composite of '{}', not a band: it has no values of its \
                 own and renders from its bands {} together",
                self.collection_id,
                self.composites[c].parameters().join(", ")
            ),
            None => format!(
                "parameter '{name}' is not served by '{}'",
                self.collection_id
            ),
        })
    }

    /// The products a multi-band request names, in its order.
    fn product_indices(&self, parameters: &[&str]) -> Result<Vec<usize>, DataServerError> {
        parameters
            .iter()
            .map(|name| self.product_index(Some(name)))
            .collect()
    }

    /// The scan `get_raster_tile` renders for a product at `time`: the
    /// latest at or before it, the first when `time` precedes every scan,
    /// the newest when `time` is `None`. The one selection both rendering
    /// and [`MapEngine::resolve_parameter_time`] use (#507).
    fn select(
        catalog: &Catalog,
        index: usize,
        time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        let frames = &catalog.frames[index];
        match time {
            Some(time) => frames
                .range(..=time)
                .next_back()
                .or_else(|| frames.iter().next())
                .map(|(t, _)| *t),
            None => frames.keys().next_back().copied(),
        }
    }

    /// The scan a multi-band render of the products `indices` uses:
    /// [`Self::select`]'s rule over the scans they all have
    /// ([`select_common_time`]). The one selection `get_raster_tiles`,
    /// [`MapEngine::resolve_parameters_time`] and
    /// [`MapEngine::resolve_parameter_time`] of a composite use (#507).
    fn select_common(
        catalog: &Catalog,
        indices: &[usize],
        time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        let axes: Vec<&[DateTime<Utc>]> = indices.iter().map(|&i| &*catalog.times[i]).collect();
        select_common_time(&axes, time)
    }

    /// The scans an EDR query addresses for one product: every scan for no
    /// `datetime`, the scan [`Self::select`] renders for an instant, and the
    /// scans inside an interval.
    fn query_times(
        catalog: &Catalog,
        index: usize,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
    ) -> Vec<DateTime<Utc>> {
        match datetime {
            None => catalog.frames[index].keys().copied().collect(),
            Some((start, end)) if start == end => Self::select(catalog, index, Some(start))
                .into_iter()
                .collect(),
            Some((start, end)) => catalog.frames[index]
                .range(start..=end)
                .map(|(time, _)| *time)
                .collect(),
        }
    }

    /// The products an EDR query names (all for `None`), each with the scans
    /// it addresses, and the query's time axis: the union of those scans. A
    /// product without a scan at one of the union's times reads null there.
    #[allow(clippy::type_complexity)]
    fn query_plan(
        &self,
        catalog: &Catalog,
        parameters: Option<&[String]>,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
    ) -> Result<(Vec<(usize, Vec<DateTime<Utc>>)>, Vec<DateTime<Utc>>), DataServerError> {
        let mut indices: Vec<usize> = match parameters {
            None => (0..self.products.len()).collect(),
            Some(names) => names
                .iter()
                .map(|name| self.product_index(Some(name)))
                .collect::<Result<_, _>>()?,
        };
        indices.sort_unstable();
        indices.dedup();
        let plan: Vec<(usize, Vec<DateTime<Utc>>)> = indices
            .into_iter()
            .map(|index| (index, Self::query_times(catalog, index, datetime)))
            .collect();
        let mut times: Vec<DateTime<Utc>> =
            plan.iter().flat_map(|(_, t)| t.iter().copied()).collect();
        times.sort_unstable();
        times.dedup();
        if times.is_empty() {
            return Err(DataServerError::InvalidParameter(
                "No scans available for the requested time range".into(),
            ));
        }
        Ok((plan, times))
    }

    /// Reject a query that would download more than [`MAX_QUERY_FETCHES`]
    /// evicted scans, before it fetches any.
    fn check_fetch_budget(
        &self,
        plan: &[(usize, Vec<DateTime<Utc>>)],
    ) -> Result<(), DataServerError> {
        let missing = plan
            .iter()
            .flat_map(|(index, own)| own.iter().map(move |time| (*index, *time)))
            .filter(|(index, time)| !FRAMES.contains_key(&self.frame_key(*index, *time)))
            .count();
        if missing > MAX_QUERY_FETCHES {
            return Err(DataServerError::QueryTooLarge(format!(
                "The query addresses {missing} scans that are not held in memory; at most \
                 {MAX_QUERY_FETCHES} per request — narrow the datetime window"
            )));
        }
        Ok(())
    }

    fn check_block_budget(blocks: usize) -> Result<(), DataServerError> {
        if blocks > MAX_QUERY_BLOCKS {
            return Err(DataServerError::QueryTooLarge(format!(
                "The query would decode {blocks} image blocks; at most {MAX_QUERY_BLOCKS} per \
                 request — narrow the datetime window, the polygon or the parameters"
            )));
        }
        Ok(())
    }

    fn description(&self, index: usize) -> ParameterDescription {
        let parameter = &self.parameters[index];
        ParameterDescription {
            label: parameter.title.clone(),
            unit: parameter.unit.clone(),
            observed_property: parameter.name.clone(),
            standard_name: None,
        }
    }

    /// The scan, from the cache or fetched again after eviction.
    fn frame(
        &self,
        index: usize,
        time: DateTime<Utc>,
        scan: &Scan,
    ) -> Result<Arc<Frame>, DataServerError> {
        FRAMES.get_or_insert_with(&self.frame_key(index, time), || {
            self.open(index, scan).map(Arc::new)
        })
    }
}

impl Catalog {
    /// `composite_bands`: per composite, the product indices of its bands.
    fn build(
        frames: Vec<BTreeMap<DateTime<Utc>, Scan>>,
        parameters: &[ParameterInfo],
        extents: Vec<Option<[f64; 4]>>,
        grids: Vec<Option<[u32; 2]>>,
        composite_bands: &[Vec<usize>],
    ) -> Catalog {
        // A grid size is advertised only when every product with a scan
        // shares it: a 0.5 km band next to 2 km products has no one grid.
        let mut sizes = grids.iter().flatten();
        let grid_size = sizes
            .next()
            .copied()
            .filter(|first| sizes.all(|size| size == first));
        let spatial_extent = union_extent(extents.iter().flatten());
        let times: Vec<Arc<[DateTime<Utc>]>> = frames
            .iter()
            .map(|f| f.keys().copied().collect::<Vec<_>>().into())
            .collect();
        let mut union: Vec<DateTime<Utc>> = times.iter().flat_map(|t| t.iter().copied()).collect();
        union.sort_unstable();
        union.dedup();
        let composite_times = composite_bands
            .iter()
            .map(|bands| shared_times(&times, bands))
            .collect();
        let info = RasterInfo {
            native_crs: "geos".to_string(),
            spatial_extent,
            times: union,
            parameter: parameters[0].name.clone(),
            unit: parameters[0].unit.clone(),
            parameters: parameters.to_vec(),
            vertical: None,
            grid_size,
            layer_subtitle: None,
            reference_times: Vec::new(),
        };
        Catalog {
            frames,
            times,
            composite_times,
            extents,
            grids,
            info: Arc::new(info),
            polled_at: None,
        }
    }
}

/// The scans every product in `bands` has, ascending: a composite's time
/// axis, the scans [`SatelliteEngine::select_common`] can pick for it.
fn shared_times(times: &[Arc<[DateTime<Utc>]>], bands: &[usize]) -> Arc<[DateTime<Utc>]> {
    let Some(&shortest) = bands.iter().min_by_key(|&&band| times[band].len()) else {
        return Arc::from([]);
    };
    times[shortest]
        .iter()
        .filter(|time| {
            bands
                .iter()
                .all(|&band| times[band].binary_search(time).is_ok())
        })
        .copied()
        .collect()
}

/// The CRS84 box covering every product's extent. Longitudes are measured
/// from the first box's centre so disks crossing the antimeridian (GOES-West)
/// union to a `west > east` box, not a −180…180 one.
fn union_extent<'a>(mut extents: impl Iterator<Item = &'a [f64; 4]>) -> Option<[f64; 4]> {
    let first = *extents.next()?;
    let width = |[w, _, e, _]: [f64; 4]| if e >= w { e - w } else { e + 360.0 - w };
    let reference = first[0] + width(first) / 2.0;
    let relative = |lon: f64| ds_core::geo::wrap_lon(lon - reference);
    let [mut w, mut s, mut e, mut n] = [
        relative(first[0]),
        first[1],
        relative(first[0]) + width(first),
        first[3],
    ];
    for extent in extents {
        let west = relative(extent[0]);
        w = w.min(west);
        e = e.max(west + width(*extent));
        s = s.min(extent[1]);
        n = n.max(extent[3]);
    }
    if e - w >= 360.0 {
        return Some([-180.0, s, 180.0, n]);
    }
    Some([
        ds_core::geo::wrap_lon(reference + w),
        s,
        ds_core::geo::wrap_lon(reference + e),
        n,
    ])
}

/// Reads full-resolution pixels of one scan through the block cache,
/// remembering the blocks already fetched for this request.
struct PixelReader<'a> {
    key: FrameKey,
    frame: &'a Frame,
    blocks: Vec<Option<Arc<[u16]>>>,
}

impl<'a> PixelReader<'a> {
    fn new(key: FrameKey, frame: &'a Frame) -> Self {
        PixelReader {
            key,
            frame,
            blocks: vec![None; frame.block_count() as usize],
        }
    }

    /// The stored integer of pixel `(row, col)`.
    fn raw(&mut self, row: u32, col: u32) -> Result<u16, DataServerError> {
        let frame = self.frame;
        let (index, offset) = frame.locate(row, col);
        let block = match &mut self.blocks[index as usize] {
            Some(block) => block,
            slot => slot.insert(STRIPS.get_or_insert_with(
                &BlockKey {
                    frame: self.key.clone(),
                    block: index,
                },
                || frame.read_block(index).map_err(DataServerError::Engine),
            )?),
        };
        Ok(block[offset])
    }

    /// The physical value of pixel `(row, col)`.
    fn value(&mut self, row: u32, col: u32) -> Result<Option<f64>, DataServerError> {
        Ok(self.frame.packing.decode(self.raw(row, col)?))
    }
}

/// EDR over the scans: a position is a time series of the pixel under the
/// point, an area a CRS84 grid sampled at the nadir resolution. Queries run
/// on async request workers; a scan the cache evicted is fetched through
/// `Source::fetch`, which is safe there.
impl EdrEngine for SatelliteEngine {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        Ok(Vec::new())
    }

    fn query_location(
        &self,
        location_id: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Err(DataServerError::LocationNotFound(format!(
            "'{}' has no named locations (requested '{location_id}')",
            self.collection_id
        )))
    }

    fn get_parameters(&self) -> Vec<String> {
        self.parameters.iter().map(|p| p.name.clone()).collect()
    }

    fn get_parameter_descriptions(&self) -> HashMap<String, ParameterDescription> {
        (0..self.parameters.len())
            .map(|index| (self.parameters[index].name.clone(), self.description(index)))
            .collect()
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        let info = self.catalog.load().info.clone();
        Some((*info.times.first()?, *info.times.last()?))
    }

    fn get_available_times(&self) -> Option<Vec<DateTime<Utc>>> {
        Some(self.catalog.load().info.times.clone())
    }

    fn get_parameter_available_times(&self, parameter: &str) -> Option<Vec<DateTime<Utc>>> {
        let index = self.product_index(Some(parameter)).ok()?;
        Some(self.catalog.load().times[index].to_vec())
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        self.catalog.load().info.spatial_extent
    }

    fn supported_query_types(&self) -> Vec<String> {
        vec!["position".into(), "area".into(), "radius".into()]
    }

    fn query_position(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        ds_core::deadline::check()?;
        let (lat, lon) = parse_point_coords(coords)?;
        let catalog = self.catalog.load();
        let (plan, times) = self.query_plan(&catalog, parameters, datetime)?;
        self.check_fetch_budget(&plan)?;
        Self::check_block_budget(plan.iter().map(|(_, own)| own.len()).sum())?;
        let mut on_disk = false;
        let mut descriptions = HashMap::new();
        let mut ranges = HashMap::new();
        for (index, own) in plan {
            let mut values = vec![None; times.len()];
            for time in own {
                ds_core::deadline::check()?;
                let frame = self.frame(index, time, &catalog.frames[index][&time])?;
                let Some((col, row)) = frame.gt.world_to_pixel(lon, lat) else {
                    continue;
                };
                on_disk = true;
                let slot = times.binary_search(&time).expect("time is in the union");
                values[slot] =
                    PixelReader::new(self.frame_key(index, time), &frame).value(row, col)?;
            }
            let name = self.parameters[index].name.clone();
            descriptions.insert(name.clone(), self.description(index));
            ranges.insert(
                name,
                NdArray {
                    shape: vec![times.len()],
                    axis_names: vec!["t".into()],
                    values,
                },
            );
        }
        if !on_disk {
            return Err(DataServerError::LocationNotFound(format!(
                "POINT({lon} {lat}) is not on the Earth disk '{}' sees",
                self.collection_id
            )));
        }
        Ok(CoverageResponse::Single(QueryResult {
            domain: DomainDescription::PointSeries {
                x: lon,
                y: lat,
                t: times,
                z: None,
            },
            parameters: descriptions,
            ranges,
        }))
    }

    fn query_area(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        ds_core::deadline::check()?;
        let polygon = parse_area_coords(coords)?;
        let catalog = self.catalog.load();
        let (plan, times) = self.query_plan(&catalog, parameters, datetime)?;
        let seen = catalog
            .info
            .spatial_extent
            .is_some_and(|extent| polygon.bbox.intersects_bbox(&extent));
        if !seen {
            return Err(DataServerError::LocationNotFound(format!(
                "The polygon lies outside the Earth disk '{}' sees",
                self.collection_id
            )));
        }

        self.check_fetch_budget(&plan)?;
        // Each product has its own grid (ABI bands are 0.5, 1 or 2 km): the
        // decode budget sums the blocks its polygon's bbox covers on each
        // grid, and the output grid samples at the finest nadir pixel size.
        let b = &polygon.bbox;
        let mut resolution = f64::INFINITY;
        let mut blocks = 0usize;
        for (index, own) in &plan {
            let Some(&first) = own.first() else {
                continue;
            };
            let probe = self.frame(*index, first, &catalog.frames[*index][&first])?;
            resolution = resolution.min(probe.gt.pixel_width / 111_320.0);
            if let Some((c0, r0, c1, r1)) =
                probe.gt.bbox_to_pixels(b.west, b.south, b.east, b.north)
            {
                let per_scan = probe.blocks_in(c0, r0, c1, r1);
                blocks = blocks.saturating_add(per_scan.saturating_mul(own.len()));
            }
        }
        Self::check_block_budget(blocks)?;
        let axes = polygon.sample_grid(resolution, resolution, MAX_AREA_DIM);
        let (nx, ny) = axes.dims();
        check_area_budget(times.len(), ny, nx, plan.len())?;
        check_mask_budget(nx * ny, &polygon)?;
        let mask = polygon.cell_mask(&axes);

        let span = if b.crosses_antimeridian() {
            b.east + 360.0 - b.west
        } else {
            b.east - b.west
        };
        let has_time = times.len() > 1;
        let mut descriptions = HashMap::new();
        let mut ranges = HashMap::new();
        for (index, own) in plan {
            let mut values = vec![None; times.len() * ny * nx];
            for time in own {
                ds_core::deadline::check()?;
                let frame = self.frame(index, time, &catalog.frames[index][&time])?;
                let gt = &frame.gt;
                // Output cells → source pixels on a coarse grid (the
                // geostationary forward transform is the expensive step).
                let grid = ProjectionGrid::build_2d(
                    nx as u32,
                    ny as u32,
                    gt.width,
                    gt.height,
                    |fx, fy| (b.west + fx * span, b.north - fy * (b.north - b.south)),
                    |lon, lat| gt.world_to_pixel_f64(lon, lat),
                );
                let mut reader = PixelReader::new(self.frame_key(index, time), &frame);
                let offset = times.binary_search(&time).expect("time is in the union") * ny * nx;
                for iy in 0..ny {
                    ds_core::deadline::check()?;
                    for ix in 0..nx {
                        let cell = axes.index(ix, iy);
                        if !mask[cell] {
                            continue;
                        }
                        let (c, r) = grid.sample(ix as u32, iy as u32);
                        let inside = c.is_finite()
                            && r.is_finite()
                            && c >= 0.0
                            && r >= 0.0
                            && c < gt.width as f64
                            && r < gt.height as f64;
                        if inside {
                            values[offset + cell] = reader.value(r as u32, c as u32)?;
                        }
                    }
                }
            }
            let name = self.parameters[index].name.clone();
            descriptions.insert(name.clone(), self.description(index));
            let (shape, axis_names) = if has_time {
                (vec![times.len(), ny, nx], vec!["t", "y", "x"])
            } else {
                (vec![ny, nx], vec!["y", "x"])
            };
            ranges.insert(
                name,
                NdArray {
                    shape,
                    axis_names: axis_names.into_iter().map(String::from).collect(),
                    values,
                },
            );
        }
        Ok(CoverageResponse::Single(QueryResult {
            domain: DomainDescription::Grid {
                x: axes.x,
                y: axes.y,
                t: has_time.then_some(times),
                z: None,
            },
            parameters: descriptions,
            ranges,
        }))
    }
}

impl MapEngine for SatelliteEngine {
    fn get_raster_tile(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        output_crs: &OutputCrs,
        parameter: Option<&str>,
        _z: Option<f64>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        self.render(bbox, width, height, time, output_crs, parameter)
    }

    /// Every band from one scan, sharing the coordinate map between bands
    /// on the same grid (`render.rs`).
    fn get_raster_tiles(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        output_crs: &OutputCrs,
        parameters: &[&str],
        _z: Option<f64>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<Vec<RasterTile>, DataServerError> {
        self.render_bands(bbox, width, height, time, output_crs, parameters)
    }

    fn raster_info(&self) -> RasterInfo {
        (*self.catalog.load().info).clone()
    }

    fn raster_info_shared(&self) -> Arc<RasterInfo> {
        self.catalog.load().info.clone()
    }

    fn resolve_time(
        &self,
        time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        self.resolve_parameter_time(None, time, reference_time)
    }

    /// A product's scans, or a composite's: the scans every band has, from
    /// the snapshot.
    fn parameter_times(&self, parameter: &str) -> Option<Arc<[DateTime<Utc>]>> {
        let catalog = self.catalog.load();
        match self.product_position(parameter) {
            Some(index) => Some(catalog.times[index].clone()),
            None => self
                .composite_index(parameter)
                .map(|c| catalog.composite_times[c].clone()),
        }
    }

    /// A composite resolves as `resolve_parameters_time` over its bands
    /// does, through the same `select_common`.
    fn resolve_parameter_time(
        &self,
        parameter: Option<&str>,
        time: Option<DateTime<Utc>>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        let catalog = self.catalog.load();
        if let Some(c) = parameter.and_then(|name| self.composite_index(name)) {
            return Self::select_common(&catalog, &self.composite_bands[c], time);
        }
        match parameter.map_or(Some(0), |name| self.product_position(name)) {
            Some(index) => Self::select(&catalog, index, time).or(time),
            None => time,
        }
    }

    fn resolve_parameters_time(
        &self,
        parameters: &[&str],
        time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        if parameters.is_empty() {
            return self.resolve_parameter_time(None, time, reference_time);
        }
        let Ok(indices) = self.product_indices(parameters) else {
            return time;
        };
        Self::select_common(&self.catalog.load(), &indices, time)
    }

    fn composites(&self) -> Arc<[CompositeDef]> {
        self.composites.clone()
    }
}

impl std::fmt::Debug for SatelliteEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SatelliteEngine")
            .field("collection_id", &self.collection_id)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::{union_extent, Catalog, TimeWindow};
    use ds_core::map_engine::ParameterInfo;
    use std::collections::BTreeMap;

    #[test]
    fn tiled_scans_wait_for_their_tiles() {
        let at = |s: &str| s.parse::<chrono::DateTime<chrono::Utc>>().unwrap();
        let (scan, now) = (at("2026-09-27T19:20:00Z"), at("2026-09-27T19:28:00Z"));
        let ready = |tiles, expected, newest, now| {
            super::tiled_scan_ready(scan, tiles, expected, newest, now)
        };
        // Still arriving: fewer tiles than before, nothing newer, recent.
        assert!(!ready(80, Some(88), Some(scan), now));
        assert!(!ready(88, None, Some(scan), now));
        // Complete, superseded, or settled.
        assert!(ready(88, Some(88), Some(scan), now));
        assert!(ready(80, Some(88), Some(at("2026-09-27T19:30:00Z")), now));
        assert!(ready(80, Some(88), Some(scan), at("2026-09-27T19:35:00Z")));
    }

    #[test]
    fn block_budget_caps_decode_work() {
        assert!(super::SatelliteEngine::check_block_budget(super::MAX_QUERY_BLOCKS).is_ok());
        assert!(matches!(
            super::SatelliteEngine::check_block_budget(super::MAX_QUERY_BLOCKS + 1),
            Err(ds_core::error::DataServerError::QueryTooLarge(_))
        ));
    }

    /// A grid size is advertised only when the products share it.
    #[test]
    fn grid_size_only_when_products_agree() {
        let parameter = |name: &str| ParameterInfo {
            name: name.into(),
            title: name.into(),
            unit: "K".into(),
        };
        let parameters = [parameter("ir"), parameter("vis")];
        let build = |grids: Vec<Option<[u32; 2]>>| {
            Catalog::build(
                vec![BTreeMap::new(); 2],
                &parameters,
                vec![None; 2],
                grids,
                &[],
            )
            .info
            .grid_size
        };
        assert_eq!(build(vec![Some([5424, 5424]), None]), Some([5424, 5424]));
        assert_eq!(
            build(vec![Some([5424, 5424]), Some([5424, 5424])]),
            Some([5424, 5424])
        );
        assert_eq!(build(vec![Some([5424, 5424]), Some([21696, 21696])]), None);
        assert_eq!(build(vec![None, None]), None);
    }

    #[test]
    fn union_extent_is_seam_aware() {
        let a = [150.0, -10.0, 170.0, 10.0];
        let b = [175.0, -20.0, -160.0, 5.0]; // crosses the antimeridian
        assert_eq!(
            union_extent([a, b].iter()),
            Some([150.0, -20.0, -160.0, 10.0])
        );
        assert_eq!(
            union_extent([[-10.0, 0.0, 10.0, 5.0], [20.0, 1.0, 30.0, 6.0]].iter()),
            Some([-10.0, 0.0, 30.0, 6.0])
        );
        assert_eq!(union_extent(std::iter::empty()), None);
    }

    fn at(s: &str) -> chrono::DateTime<chrono::Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn shared_times_intersect_the_band_axes() {
        let axis =
            |times: &[&str]| -> std::sync::Arc<[_]> { times.iter().map(|t| at(t)).collect() };
        let times = [
            axis(&["2026-09-25T19:00:00Z", "2026-09-25T19:10:00Z"]),
            axis(&[
                "2026-09-25T19:00:00Z",
                "2026-09-25T19:10:00Z",
                "2026-09-25T19:20:00Z",
            ]),
            axis(&["2026-09-25T19:10:00Z", "2026-09-25T19:20:00Z"]),
            axis(&[]),
        ];
        let shared = |bands: &[usize]| super::shared_times(&times, bands).to_vec();
        assert_eq!(
            shared(&[0, 1]),
            [at("2026-09-25T19:00:00Z"), at("2026-09-25T19:10:00Z")]
        );
        assert_eq!(
            shared(&[1, 2, 1]),
            [at("2026-09-25T19:10:00Z"), at("2026-09-25T19:20:00Z")]
        );
        assert_eq!(shared(&[0, 1, 2]), [at("2026-09-25T19:10:00Z")]);
        assert_eq!(shared(&[2]), times[2].to_vec());
        assert!(shared(&[0, 3]).is_empty());
        assert!(shared(&[]).is_empty());
    }

    /// A composite's axis is part of the snapshot each poll swaps in: a
    /// scan leaving the time window leaves the composite too.
    #[test]
    fn composite_times_follow_eviction() {
        use ds_core::config::{
            SatelliteCompositeChannel, SatelliteCompositeConfig, SatelliteConfig,
            SatelliteProductConfig,
        };
        use ds_core::map_engine::MapEngine;
        use std::path::Path;

        const C13: &str =
            "OR_ABI-L2-CMIPF-M6C13_G19_s20262681900199_e20262681909519_c20262681909592.nc";
        const ACHT: &str =
            "OR_ABI-L2-ACHTF-M6_G19_s20262681900199_e20262681909507_c20262681912337.nc";
        let dir = tempfile::tempdir().unwrap();
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/goes19-abi");
        // Both bands at 19:00 and 19:10, republished under the later stamp.
        for name in [C13, ACHT] {
            std::fs::copy(fixtures.join(name), dir.path().join(name)).unwrap();
            let later = name.replace("_s20262681900199_", "_s20262681910199_");
            std::fs::copy(fixtures.join(name), dir.path().join(later)).unwrap();
        }
        let product =
            |parameter: &str, product: &str, band, variable: &str| SatelliteProductConfig {
                parameter: parameter.into(),
                title: parameter.into(),
                unit: "K".into(),
                product: product.into(),
                band,
                variable: variable.into(),
            };
        let channel = |parameter: &str| {
            Some(SatelliteCompositeChannel {
                parameter: parameter.into(),
                minus: None,
                min: 180.0,
                max: 330.0,
                gamma: 1.0,
            })
        };
        let config = SatelliteConfig {
            provider: "goes-r".into(),
            data_path: Some(dir.path().to_string_lossy().into_owned()),
            endpoint: None,
            bucket: None,
            time_window: None,
            poll_interval_secs: 60,
            products: vec![
                product("ir", "ABI-L2-CMIPF", Some(13), "CMI"),
                product("cloud", "ABI-L2-ACHTF", None, "TEMP"),
            ],
            composites: vec![SatelliteCompositeConfig {
                name: "rgb".into(),
                title: None,
                recipe: None,
                red: channel("ir"),
                green: channel("cloud"),
                blue: channel("ir"),
            }],
        };
        let mut engine = super::SatelliteEngine::new("goes19-evict", &config).unwrap();
        engine.poll_once();
        let (t0, t1) = (at("2026-09-25T19:00:00Z"), at("2026-09-25T19:10:00Z"));
        assert_eq!(&*engine.parameter_times("rgb").unwrap(), [t0, t1]);
        assert_eq!(
            engine.resolve_parameter_time(Some("rgb"), None, None),
            Some(t1)
        );

        // Time moves on: the window now starts between the two scans.
        let since = chrono::Utc::now() - at("2026-09-25T19:05:00Z");
        engine.window = Some(TimeWindow::parse(&format!("-PT{}S", since.num_seconds())).unwrap());
        engine.poll_once();
        assert_eq!(&*engine.parameter_times("ir").unwrap(), [t1]);
        assert_eq!(&*engine.parameter_times("rgb").unwrap(), [t1]);
        assert_eq!(
            engine.resolve_parameter_time(Some("rgb"), Some(t0), None),
            Some(t1)
        );
    }
}
