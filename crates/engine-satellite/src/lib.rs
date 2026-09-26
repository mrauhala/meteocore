//! Geostationary satellite imagery (#819): GOES-R ABI NetCDF-4 scans served
//! through WMS, OGC API Maps and Tiles.
//!
//! One collection is one satellite and sector; each configured product (an
//! ABI band, or an L2 field such as cloud top temperature) is a parameter
//! with its own time axis. The poll loop lists the source, downloads each
//! new scan whole and keeps it in memory ([`cache::FRAMES`]); renders
//! decode only the strips they touch ([`cache::STRIPS`]) or, zoomed out,
//! sample the overview built at ingest.

mod cache;
mod frame;
mod naming;
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
use ds_core::map_engine::{MapEngine, OutputCrs, ParameterInfo, RasterInfo, RasterTile};
use ds_core::model::{
    CoverageResponse, DomainDescription, Location, NdArray, ParameterDescription, QueryResult,
};
use ds_core::resample::ProjectionGrid;
use ds_poll::{FirstTick, Shutdown};
use ds_storage::discovery::{validate_prefix_pattern, TimeWindow};
use ds_storage::object_store::path::Path as ObjectPath;

pub use cache::{frame_metrics, strip_metrics};

use cache::{FrameKey, StripKey, FRAMES, STRIPS};
use frame::{Frame, OVERVIEW_FACTOR};
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

/// Strips one EDR query may decode, over all its products and scans (~0.7 ms
/// and ~260 KB each for a 2 km full-width strip). A position reads one strip
/// per scan; an area the strips its polygon's rows cross, per product grid.
const MAX_QUERY_STRIPS: usize = 1024;

/// One configured product, served as one parameter.
struct Product {
    parameter: Arc<str>,
    variable: String,
    naming: Naming,
}

/// A consistent snapshot of what is served, swapped whole by each poll.
struct Catalog {
    /// Per product (config order): scan start → file.
    frames: Vec<BTreeMap<DateTime<Utc>, ObjectPath>>,
    /// Per product: its scan starts, for `parameter_times` (O(1)).
    times: Vec<Arc<[DateTime<Utc>]>>,
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
                naming: Naming::goes_r(&p.product, p.band),
            })
            .collect();
        let source = match (&config.data_path, &config.endpoint, &config.bucket) {
            (Some(path), _, _) => {
                let (store, base) = ds_storage::build_store(path)?;
                Source::Directory { store, base }
            }
            (None, Some(endpoint), Some(bucket)) => {
                for product in &products {
                    validate_prefix_pattern(&product.naming.prefix, window.as_ref())?;
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
        let empty = vec![BTreeMap::new(); products.len()];
        let catalog = Catalog::build(
            empty,
            &parameters,
            vec![None; products.len()],
            vec![None; products.len()],
        );
        Ok(Self {
            collection_id: collection_id.into(),
            source,
            products,
            parameters,
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
        for (index, product) in self.products.iter().enumerate() {
            let found = match self.source.list(&product.naming, window) {
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
            let mut new: Vec<_> = found
                .into_iter()
                .filter(|f| !known.contains_key(&f.time))
                .collect();
            new.reverse();
            new.truncate(MAX_INGEST_PER_POLL);
            for scan in new {
                match self.ingest(index, scan.time, &scan.path) {
                    Ok(frame) => {
                        if extents[index].is_none() {
                            extents[index] = ds_core::geo::crs84_extent(frame.gt.bbox());
                        }
                        grids[index].get_or_insert([frame.gt.width, frame.gt.height]);
                        frames[index].insert(scan.time, scan.path);
                    }
                    Err(e) => tracing::warn!(
                        "[{}] skipping {} scan {}: {e}",
                        self.collection_id,
                        product.parameter,
                        scan.path
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
        let catalog = Catalog::build(frames, &self.parameters, extents, grids);
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
        path: &ObjectPath,
    ) -> Result<Arc<Frame>, DataServerError> {
        let key = self.frame_key(index, time);
        let bytes = self.source.fetch(path)?;
        let frame = Arc::new(
            Frame::open(bytes, &self.products[index].variable).map_err(DataServerError::Engine)?,
        );
        FRAMES.insert(key, frame.clone());
        Ok(frame)
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
                .products
                .iter()
                .position(|p| &*p.parameter == name)
                .ok_or_else(|| {
                    DataServerError::InvalidParameter(format!(
                        "parameter '{name}' is not served by '{}'",
                        self.collection_id
                    ))
                }),
        }
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

    fn check_strip_budget(strips: usize) -> Result<(), DataServerError> {
        if strips > MAX_QUERY_STRIPS {
            return Err(DataServerError::QueryTooLarge(format!(
                "The query would decode {strips} image strips; at most {MAX_QUERY_STRIPS} per \
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
        }
    }

    /// The scan, from the cache or fetched again after eviction.
    fn frame(
        &self,
        index: usize,
        time: DateTime<Utc>,
        path: &ObjectPath,
    ) -> Result<Arc<Frame>, DataServerError> {
        FRAMES.get_or_insert_with(&self.frame_key(index, time), || {
            let bytes = self.source.fetch(path)?;
            Frame::open(bytes, &self.products[index].variable)
                .map(Arc::new)
                .map_err(DataServerError::Engine)
        })
    }
}

impl Catalog {
    fn build(
        frames: Vec<BTreeMap<DateTime<Utc>, ObjectPath>>,
        parameters: &[ParameterInfo],
        extents: Vec<Option<[f64; 4]>>,
        grids: Vec<Option<[u32; 2]>>,
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
            extents,
            grids,
            info: Arc::new(info),
            polled_at: None,
        }
    }
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

/// Reads pixels of one scan through the strip cache, remembering the strips
/// already fetched for this request.
struct PixelReader<'a> {
    key: FrameKey,
    frame: &'a Frame,
    strips: Vec<Option<Arc<[u16]>>>,
}

impl<'a> PixelReader<'a> {
    fn new(key: FrameKey, frame: &'a Frame) -> Self {
        PixelReader {
            key,
            frame,
            strips: vec![None; frame.strip_count() as usize],
        }
    }

    /// The physical value of full-resolution pixel `(row, col)`.
    fn value(&mut self, row: u32, col: u32) -> Result<Option<f64>, DataServerError> {
        let frame = self.frame;
        let index = (row / frame.strip_rows) as usize;
        if self.strips[index].is_none() {
            let strip = STRIPS.get_or_insert_with(
                &StripKey {
                    frame: self.key.clone(),
                    strip: index as u32,
                },
                || {
                    frame
                        .read_strip(index as u32)
                        .map_err(DataServerError::Engine)
                },
            )?;
            self.strips[index] = Some(strip);
        }
        let strip = self.strips[index].as_ref().expect("filled above");
        let raw = strip[(row % frame.strip_rows) as usize * frame.gt.width as usize + col as usize];
        Ok(frame.packing.decode(raw))
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
        Self::check_strip_budget(plan.iter().map(|(_, own)| own.len()).sum())?;
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
        // decode budget sums the strips its polygon rows cross on each grid,
        // and the output grid samples at the finest nadir pixel size.
        let b = &polygon.bbox;
        let mut resolution = f64::INFINITY;
        let mut strips = 0usize;
        for (index, own) in &plan {
            let Some(&first) = own.first() else {
                continue;
            };
            let probe = self.frame(*index, first, &catalog.frames[*index][&first])?;
            resolution = resolution.min(probe.gt.pixel_width / 111_320.0);
            if let Some((_, r0, _, r1)) = probe.gt.bbox_to_pixels(b.west, b.south, b.east, b.north)
            {
                let rows = r0 / probe.strip_rows..=(r1 - 1) / probe.strip_rows;
                strips = strips.saturating_add(rows.count().saturating_mul(own.len()));
            }
        }
        Self::check_strip_budget(strips)?;
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
        let index = self.product_index(parameter)?;
        let catalog = self.catalog.load();
        let empty = || RasterTile {
            width,
            height,
            values: vec![None; width as usize * height as usize].into(),
        };
        let Some(time) = Self::select(&catalog, index, time) else {
            return Ok(empty());
        };
        let frame = self.frame(index, time, &catalog.frames[index][&time])?;

        // The part of the disk the request sees decides the level: sample
        // the overview when a full-resolution read would take several
        // source pixels per output pixel.
        let [west, south, east, north] = bbox;
        let Some((c0, r0, c1, r1)) = frame.gt.bbox_to_pixels(west, south, east, north) else {
            return Ok(empty());
        };
        let density = f64::max(
            (c1 - c0) as f64 / width as f64,
            (r1 - r0) as f64 / height as f64,
        );
        let overview = density >= OVERVIEW_FACTOR as f64;
        let gt = if overview {
            &frame.overview.gt
        } else {
            &frame.gt
        };

        // Output→source mapping on a coarse grid (Critical Rule 5). Points
        // the satellite cannot see project to NaN; the grid refines cells on
        // the limb, and no footprint guard is needed: there is no far side
        // for a coarse cell to alias onto.
        let grid = ProjectionGrid::build_2d(
            width,
            height,
            gt.width,
            gt.height,
            |fx, fy| output_crs.project_node(bbox, fx, fy),
            |lon, lat| gt.world_to_pixel_f64(lon, lat),
        );

        let key = self.frame_key(index, time);
        let mut strips: Vec<Option<Arc<[u16]>>> = vec![None; frame.strip_count() as usize];
        let mut values = Vec::with_capacity(width as usize * height as usize);
        for oy in 0..height {
            for ox in 0..width {
                let (c, r) = grid.sample(ox, oy);
                let inside = c.is_finite()
                    && r.is_finite()
                    && c >= 0.0
                    && r >= 0.0
                    && c < gt.width as f64
                    && r < gt.height as f64;
                if !inside {
                    values.push(None);
                    continue;
                }
                let (col, row) = (c as usize, r as u32);
                let raw = if overview {
                    frame.overview.raw[row as usize * gt.width as usize + col]
                } else {
                    let index = (row / frame.strip_rows) as usize;
                    if strips[index].is_none() {
                        let strip = STRIPS.get_or_insert_with(
                            &StripKey {
                                frame: key.clone(),
                                strip: index as u32,
                            },
                            || {
                                frame
                                    .read_strip(index as u32)
                                    .map_err(DataServerError::Engine)
                            },
                        )?;
                        strips[index] = Some(strip);
                    }
                    let strip = strips[index].as_ref().expect("filled above");
                    strip[(row % frame.strip_rows) as usize * gt.width as usize + col]
                };
                values.push(frame.packing.decode(raw));
            }
        }
        Ok(RasterTile {
            width,
            height,
            values: values.into(),
        })
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

    fn parameter_times(&self, parameter: &str) -> Option<Arc<[DateTime<Utc>]>> {
        let index = self.product_index(Some(parameter)).ok()?;
        Some(self.catalog.load().times[index].clone())
    }

    fn resolve_parameter_time(
        &self,
        parameter: Option<&str>,
        time: Option<DateTime<Utc>>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        let Ok(index) = self.product_index(parameter) else {
            return time;
        };
        Self::select(&self.catalog.load(), index, time).or(time)
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
    use super::{union_extent, Catalog};
    use ds_core::map_engine::ParameterInfo;
    use std::collections::BTreeMap;

    #[test]
    fn strip_budget_caps_decode_work() {
        assert!(super::SatelliteEngine::check_strip_budget(super::MAX_QUERY_STRIPS).is_ok());
        assert!(matches!(
            super::SatelliteEngine::check_strip_budget(super::MAX_QUERY_STRIPS + 1),
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
            Catalog::build(vec![BTreeMap::new(); 2], &parameters, vec![None; 2], grids)
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
}
