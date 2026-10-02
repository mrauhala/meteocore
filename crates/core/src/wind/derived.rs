//! [`DerivedWind`]: serves a [`WindPlan`]'s parameters over any engine's
//! `MapEngine` + `EdrEngine`, the way a nowcast wraps its source.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use chrono::{DateTime, Utc};

use super::{DerivedKind, DerivedParameter, PairOutcome, WindFacts, WindPlan, WindSource};
use crate::cube::CubeResolution;
use crate::edr_engine::{EdrEngine, LocationFilter, TrajectoryShape};
use crate::error::DataServerError;
use crate::feature::{Bbox, DatetimeInterval, MAX_AREA_VALUES};
use crate::instances::RunInfo;
use crate::map_engine::{
    CompositeDef, MapEngine, OutputCrs, ParameterInfo, RasterInfo, RasterTile, RasterValues,
};
use crate::model::{CoverageResponse, Location, NdArray, ParameterDescription, QueryResult};
use crate::vertical::VerticalDimension;

/// Reports one pair's outcome: `(collection id, outcome)`. Called for every
/// pair when the wrapper is built, then only when a pair's outcome changes
/// (a refreshed snapshot that now states the frame, a new run without a
/// component). ds-core has no logging dependency; the server logs.
pub type OutcomeLog = Arc<dyn Fn(&str, &PairOutcome) + Send + Sync>;

type Times = Arc<[DateTime<Utc>]>;
type Datetime = Option<(DateTime<Utc>, DateTime<Utc>)>;

/// An engine with wind speed and direction derived from its u/v components
/// (#897). Every `MapEngine` and `EdrEngine` method delegates to the wrapped
/// engine; the derived parameters are added on top:
///
/// - **Map APIs**: the derived speeds only, as extra
///   `RasterInfo::parameters`. A direction needs a cyclic palette and is not
///   a map layer. A speed tile is the two component tiles of the same
///   request (bbox, size, time, z, run), combined per pixel: no second
///   projection, and the engine's own sampling.
/// - **EDR**: speed and direction in `parameter_names` and every query. The
///   components are queried in place of the derived names and combined per
///   value; components the client did not ask for are dropped again. The
///   response, derived values included, must fit the shared value budget
///   ([`MAX_AREA_VALUES`]); it is counted before anything is derived.
///
/// Time, run and content resolve as the components do (root `CLAUDE.md`,
/// adding-an-engine step 7): a derived parameter's time axis is the
/// intersection of its components', and its cache-key time is
/// `resolve_parameters_time` over them, so a cached tile never names a
/// timestep that was not rendered (#507/#521).
///
/// Every capability accessor stays O(1) (Critical Rule 10): the plan is
/// rebuilt only when the source's [`WindFacts`] snapshot changes, and the
/// extended `RasterInfo` only when the plan or the engine's snapshot does.
pub struct DerivedWind {
    collection_id: String,
    map: Arc<dyn MapEngine>,
    edr: Arc<dyn EdrEngine>,
    source: Arc<dyn WindSource>,
    log: OutcomeLog,
    plan: RwLock<Option<(Arc<WindFacts>, Arc<WindPlan>)>>,
    raster: RwLock<Option<RasterSnapshot>>,
    axes: RwLock<HashMap<String, AxisSnapshot>>,
    logged: Mutex<HashMap<(String, String), PairOutcome>>,
}

struct RasterSnapshot {
    inner: Arc<RasterInfo>,
    plan: Arc<WindPlan>,
    info: Arc<RasterInfo>,
}

struct AxisSnapshot {
    u: Times,
    v: Times,
    common: Times,
}

fn read<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|e| e.into_inner())
}

fn write<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|e| e.into_inner())
}

fn lock<T>(lock: &Mutex<T>) -> MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(|e| e.into_inner())
}

impl DerivedWind {
    /// Wrap `engine`, plan its pairs and report each pair's outcome.
    pub fn new<E>(collection_id: impl Into<String>, engine: Arc<E>, log: OutcomeLog) -> Self
    where
        E: MapEngine + EdrEngine + WindSource + 'static,
    {
        let wrapper = Self {
            collection_id: collection_id.into(),
            map: engine.clone(),
            edr: engine.clone(),
            source: engine,
            log,
            plan: RwLock::new(None),
            raster: RwLock::new(None),
            axes: RwLock::new(HashMap::new()),
            logged: Mutex::new(HashMap::new()),
        };
        wrapper.plan();
        wrapper
    }

    /// The collection this wrapper serves.
    pub fn collection_id(&self) -> &str {
        &self.collection_id
    }

    /// The plan for the source's current snapshot.
    pub fn plan(&self) -> Arc<WindPlan> {
        let facts = self.source.wind_facts();
        if let Some((cached, plan)) = &*read(&self.plan) {
            if Arc::ptr_eq(cached, &facts) {
                return plan.clone();
            }
        }
        let plan = Arc::new(WindPlan::build(&facts));
        self.report(&plan);
        *write(&self.plan) = Some((facts, plan.clone()));
        plan
    }

    fn report(&self, plan: &WindPlan) {
        let mut logged = lock(&self.logged);
        for outcome in &plan.outcomes {
            let key = (outcome.u.clone(), outcome.v.clone());
            if logged.get(&key) != Some(outcome) {
                (self.log)(&self.collection_id, outcome);
                logged.insert(key, outcome.clone());
            }
        }
    }

    /// A derived speed tile: both component tiles of the request, combined
    /// per pixel. Each component is converted as it arrives, so a boxed
    /// `F64` component is never held twice.
    #[allow(clippy::too_many_arguments)] // mirrors get_raster_tile
    fn speed_tile(
        &self,
        d: &DerivedParameter,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        output_crs: &OutputCrs,
        z: Option<f64>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        // Components on time axes of their own render from one timestep
        // they share: the one the cache key names (`resolve_parameter_time`).
        let time = if self.map.parameter_times(&d.u).is_some()
            || self.map.parameter_times(&d.v).is_some()
        {
            let shared = self
                .map
                .resolve_parameters_time(&[&d.u, &d.v], time, reference_time)
                .ok_or_else(|| {
                    DataServerError::InvalidParameter(format!(
                        "'{}' and '{}' share no timestep",
                        d.u, d.v
                    ))
                })?;
            Some(shared)
        } else {
            time
        };
        let pixels = width as usize * height as usize;
        let component = |name: &str| {
            let tile = self.map.get_raster_tile(
                bbox,
                width,
                height,
                time,
                output_crs,
                Some(name),
                z,
                reference_time,
            )?;
            if tile.width != width || tile.height != height || tile.values.len() != pixels {
                return Err(DataServerError::Engine(format!(
                    "wind component '{name}' returned a {}×{} tile for a {width}×{height} request",
                    tile.width, tile.height
                )));
            }
            Ok(tile)
        };
        let u: Vec<f64> = {
            let tile = component(&d.u)?;
            tile.values
                .iter_values()
                .map(|u| u.unwrap_or(f64::NAN))
                .collect()
        };
        crate::deadline::check()?;
        let v = component(&d.v)?;
        let data = u
            .into_iter()
            .zip(v.values.iter_values())
            .map(|(u, v)| d.value(Some(u), v).map_or(f32::NAN, |value| value as f32))
            .collect();
        Ok(RasterTile {
            width,
            height,
            values: RasterValues::F32 { data, nodata: None },
        })
    }

    /// The timesteps two components share: cached until either axis
    /// snapshot changes.
    fn common_axis(&self, name: &str, u: Times, v: Times) -> Times {
        if let Some(axis) = read(&self.axes).get(name) {
            if Arc::ptr_eq(&axis.u, &u) && Arc::ptr_eq(&axis.v, &v) {
                return axis.common.clone();
            }
        }
        let common: Times = intersect(&u, &v).into();
        write(&self.axes).insert(
            name.to_string(),
            AxisSnapshot {
                u,
                v,
                common: common.clone(),
            },
        );
        common
    }

    /// Run an EDR query, deriving the requested derived parameters from the
    /// components the wrapped engine returns.
    fn derive_query(
        &self,
        parameters: Option<&[String]>,
        query: impl FnOnce(Option<&[String]>) -> Result<CoverageResponse, DataServerError>,
    ) -> Result<CoverageResponse, DataServerError> {
        let plan = self.plan();
        match EdrSelection::new(&plan, parameters) {
            None => query(parameters),
            Some(selection) => selection.finish(query(selection.inner.as_deref())?),
        }
    }
}

/// Ascending timesteps present in both ascending axes.
fn intersect(u: &[DateTime<Utc>], v: &[DateTime<Utc>]) -> Vec<DateTime<Utc>> {
    u.iter()
        .filter(|t| v.binary_search(t).is_ok())
        .copied()
        .collect()
}

/// An EDR request rewritten for the wrapped engine.
struct EdrSelection {
    /// The `parameter-name` list the wrapped engine sees: the requested
    /// names, derived ones replaced by their components.
    inner: Option<Vec<String>>,
    derive: Vec<DerivedParameter>,
    /// Components only the derivation asked for.
    drop: Vec<String>,
}

impl EdrSelection {
    /// `None` when the request names no derived parameter (pass through).
    fn new(plan: &WindPlan, requested: Option<&[String]>) -> Option<Self> {
        if plan.derived.is_empty() {
            return None;
        }
        let Some(names) = requested else {
            // Every parameter: the components are in the response anyway.
            return Some(Self {
                inner: None,
                derive: plan.derived.clone(),
                drop: Vec::new(),
            });
        };
        let mut inner: Vec<String> = Vec::with_capacity(names.len() + 2);
        let mut derive: Vec<DerivedParameter> = Vec::new();
        for name in names {
            match plan.edr_parameter(name) {
                Some(d) if derive.iter().any(|x| x.name == d.name) => {}
                Some(d) => derive.push(d.clone()),
                None => inner.push(name.clone()),
            }
        }
        if derive.is_empty() {
            return None;
        }
        let mut drop = Vec::new();
        for d in &derive {
            for component in [&d.edr_u, &d.edr_v] {
                if !inner.iter().any(|n| n.eq_ignore_ascii_case(component)) {
                    inner.push(component.clone());
                    drop.push(component.clone());
                }
            }
        }
        Some(Self {
            inner: Some(inner),
            derive,
            drop,
        })
    }

    fn finish(&self, response: CoverageResponse) -> Result<CoverageResponse, DataServerError> {
        let results = match &response {
            CoverageResponse::Single(result) => std::slice::from_ref(result),
            CoverageResponse::Collection(results) => results.as_slice(),
        };
        self.check_budget(results, MAX_AREA_VALUES)?;
        Ok(match response {
            CoverageResponse::Single(mut result) => {
                self.finish_result(&mut result)?;
                CoverageResponse::Single(result)
            }
            CoverageResponse::Collection(mut results) => {
                for result in &mut results {
                    self.finish_result(result)?;
                }
                CoverageResponse::Collection(results)
            }
        })
    }

    /// The response budget, counted before any derived value is computed.
    /// The wrapped engine checked its own request, which named the
    /// components in place of the derived parameters; the response adds a
    /// range per derived parameter and drops the components only the
    /// derivation asked for. Its values must still fit the shared response
    /// budget ([`MAX_AREA_VALUES`]), or the query is too large: without
    /// `parameter-name` a collection holding little but one u/v pair would
    /// otherwise answer with twice what its engine allowed.
    fn check_budget(&self, results: &[QueryResult], limit: usize) -> Result<(), DataServerError> {
        let (mut served, mut derived) = (0usize, 0usize);
        for result in results {
            for (name, range) in &result.ranges {
                if !self.drop.contains(name) {
                    served = served.saturating_add(range.values.len());
                }
            }
            for d in &self.derive {
                let component = result
                    .ranges
                    .get(&d.edr_u)
                    .or_else(|| result.ranges.get(&d.edr_v));
                if let Some(range) = component {
                    derived = derived.saturating_add(range.values.len());
                }
            }
        }
        let total = served.saturating_add(derived);
        if total > limit {
            return Err(DataServerError::QueryTooLarge(format!(
                "Query would return {total} values ({served} queried + {derived} derived wind \
                 values); the limit is {limit} — narrow the datetime window, the area, z or \
                 the parameters"
            )));
        }
        Ok(())
    }

    /// Combine the component ranges value by value, after sampling: a
    /// derived value is never interpolated.
    fn finish_result(&self, result: &mut QueryResult) -> Result<(), DataServerError> {
        for d in &self.derive {
            let range = match (result.ranges.get(&d.edr_u), result.ranges.get(&d.edr_v)) {
                (Some(u), Some(v)) => {
                    if u.shape != v.shape || u.values.len() != v.values.len() {
                        return Err(DataServerError::Engine(format!(
                            "wind components '{}' and '{}' returned ranges of different shapes",
                            d.edr_u, d.edr_v
                        )));
                    }
                    NdArray {
                        shape: u.shape.clone(),
                        axis_names: u.axis_names.clone(),
                        values: u
                            .values
                            .iter()
                            .zip(&v.values)
                            .map(|(&u, &v)| d.value(u, v))
                            .collect(),
                    }
                }
                // One component missing throughout: missing values.
                (Some(one), None) | (None, Some(one)) => NdArray {
                    shape: one.shape.clone(),
                    axis_names: one.axis_names.clone(),
                    values: vec![None; one.values.len()],
                },
                (None, None) => continue,
            };
            result.ranges.insert(d.name.clone(), range);
            result.parameters.insert(d.name.clone(), description(d));
        }
        for name in &self.drop {
            result.ranges.remove(name);
            result.parameters.remove(name);
        }
        Ok(())
    }
}

fn description(d: &DerivedParameter) -> ParameterDescription {
    ParameterDescription {
        label: d.title.clone(),
        unit: d.unit.clone(),
        observed_property: d.name.clone(),
        // Asserted by construction: this is the quantity computed.
        standard_name: Some(d.kind.standard_name().to_string()),
    }
}

impl MapEngine for DerivedWind {
    #[allow(clippy::too_many_arguments)] // mirrors the trait
    fn get_raster_tile(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        output_crs: &OutputCrs,
        parameter: Option<&str>,
        z: Option<f64>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        if let Some(name) = parameter {
            let plan = self.plan();
            if let Some(d) = plan.get(name) {
                return match d.kind {
                    DerivedKind::Speed => {
                        self.speed_tile(d, bbox, width, height, time, output_crs, z, reference_time)
                    }
                    DerivedKind::Direction => Err(DataServerError::InvalidParameter(format!(
                        "'{name}' is a wind direction, which only EDR serves"
                    ))),
                };
            }
        }
        self.map.get_raster_tile(
            bbox,
            width,
            height,
            time,
            output_crs,
            parameter,
            z,
            reference_time,
        )
    }

    #[allow(clippy::too_many_arguments)] // mirrors the trait
    fn get_raster_tiles(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        output_crs: &OutputCrs,
        parameters: &[&str],
        z: Option<f64>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<Vec<RasterTile>, DataServerError> {
        let plan = self.plan();
        if !parameters.iter().any(|p| plan.get(p).is_some()) {
            return self.map.get_raster_tiles(
                bbox,
                width,
                height,
                time,
                output_crs,
                parameters,
                z,
                reference_time,
            );
        }
        // `time` is the one timestep `resolve_parameters_time` picked for
        // every band, so each band renders from exactly it.
        parameters
            .iter()
            .map(|p| {
                self.get_raster_tile(
                    bbox,
                    width,
                    height,
                    time,
                    output_crs,
                    Some(p),
                    z,
                    reference_time,
                )
            })
            .collect()
    }

    fn raster_info(&self) -> RasterInfo {
        (*self.raster_info_shared()).clone()
    }

    fn raster_info_shared(&self) -> Arc<RasterInfo> {
        let inner = self.map.raster_info_shared();
        let plan = self.plan();
        if !plan.derived.iter().any(|d| d.kind == DerivedKind::Speed) {
            return inner;
        }
        if let Some(snapshot) = &*read(&self.raster) {
            if Arc::ptr_eq(&snapshot.inner, &inner) && Arc::ptr_eq(&snapshot.plan, &plan) {
                return snapshot.info.clone();
            }
        }
        let mut info = (*inner).clone();
        for d in plan.derived.iter().filter(|d| d.kind == DerivedKind::Speed) {
            let has = |name: &str| info.parameters.iter().any(|p| p.name == name);
            if has(&d.u) && has(&d.v) && !has(&d.name) {
                info.parameters.push(ParameterInfo {
                    name: d.name.clone(),
                    title: d.title.clone(),
                    unit: d.unit.clone(),
                });
            }
        }
        let info = Arc::new(info);
        *write(&self.raster) = Some(RasterSnapshot {
            inner,
            plan,
            info: info.clone(),
        });
        info
    }

    fn default_time(&self) -> Option<DateTime<Utc>> {
        self.map.default_time()
    }

    fn resolve_time(
        &self,
        time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        self.map.resolve_time(time, reference_time)
    }

    fn resolve_reference_time(
        &self,
        time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        self.map.resolve_reference_time(time, reference_time)
    }

    fn content_version(&self) -> u64 {
        self.map.content_version()
    }

    fn parameter_times(&self, parameter: &str) -> Option<Times> {
        let plan = self.plan();
        let Some(d) = plan.get(parameter) else {
            return self.map.parameter_times(parameter);
        };
        match (
            self.map.parameter_times(&d.u),
            self.map.parameter_times(&d.v),
        ) {
            (None, None) => None,
            // A component without an axis of its own has every time.
            (Some(axis), None) | (None, Some(axis)) => Some(axis),
            (Some(u), Some(v)) => Some(self.common_axis(&d.name, u, v)),
        }
    }

    fn resolve_parameter_time(
        &self,
        parameter: Option<&str>,
        time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        let plan = self.plan();
        match parameter.and_then(|p| plan.get(p)) {
            Some(d) => self
                .map
                .resolve_parameters_time(&[&d.u, &d.v], time, reference_time),
            None => self
                .map
                .resolve_parameter_time(parameter, time, reference_time),
        }
    }

    fn resolve_parameters_time(
        &self,
        parameters: &[&str],
        time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        let plan = self.plan();
        if !parameters.iter().any(|p| plan.get(p).is_some()) {
            return self
                .map
                .resolve_parameters_time(parameters, time, reference_time);
        }
        let mut expanded: Vec<&str> = Vec::with_capacity(parameters.len() + 2);
        for p in parameters {
            let names = match plan.get(p) {
                Some(d) => vec![d.u.as_str(), d.v.as_str()],
                None => vec![*p],
            };
            for name in names {
                if !expanded.contains(&name) {
                    expanded.push(name);
                }
            }
        }
        self.map
            .resolve_parameters_time(&expanded, time, reference_time)
    }

    fn composites(&self) -> Arc<[CompositeDef]> {
        self.map.composites()
    }
}

impl EdrEngine for DerivedWind {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        self.edr.get_locations()
    }

    fn location_time_filter<'a>(
        &'a self,
        intervals: &'a [DatetimeInterval],
    ) -> Option<LocationFilter<'a>> {
        self.edr.location_time_filter(intervals)
    }

    fn get_instances(&self) -> Vec<RunInfo> {
        self.edr.get_instances()
    }

    fn has_instances(&self) -> bool {
        self.edr.has_instances()
    }

    fn find_instance(&self, reference_time: DateTime<Utc>) -> Option<RunInfo> {
        self.edr.find_instance(reference_time)
    }

    fn query_location(
        &self,
        location_id: &str,
        datetime: Datetime,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        self.derive_query(parameters, |parameters| {
            self.edr
                .query_location(location_id, datetime, parameters, z, reference_time)
        })
    }

    fn get_parameters(&self) -> Vec<String> {
        let mut names = self.edr.get_parameters();
        let plan = self.plan();
        for d in &plan.derived {
            if names.contains(&d.edr_u) && names.contains(&d.edr_v) && !names.contains(&d.name) {
                names.push(d.name.clone());
            }
        }
        names
    }

    fn get_parameter_descriptions(&self) -> HashMap<String, ParameterDescription> {
        let mut descriptions = self.edr.get_parameter_descriptions();
        let plan = self.plan();
        for d in &plan.derived {
            if descriptions.contains_key(&d.edr_u)
                && descriptions.contains_key(&d.edr_v)
                && !descriptions.contains_key(&d.name)
            {
                descriptions.insert(d.name.clone(), description(d));
            }
        }
        descriptions
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        self.edr.get_temporal_extent()
    }

    fn get_available_times(&self) -> Option<Vec<DateTime<Utc>>> {
        self.edr.get_available_times()
    }

    fn get_parameter_available_times(&self, parameter: &str) -> Option<Vec<DateTime<Utc>>> {
        let plan = self.plan();
        let Some(d) = plan.get(parameter) else {
            return self.edr.get_parameter_available_times(parameter);
        };
        match (
            self.edr.get_parameter_available_times(&d.edr_u),
            self.edr.get_parameter_available_times(&d.edr_v),
        ) {
            (None, None) => None,
            (Some(axis), None) | (None, Some(axis)) => Some(axis),
            (Some(u), Some(v)) => Some(intersect(&u, &v)),
        }
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        self.edr.get_spatial_extent()
    }

    fn get_vertical_extent(&self) -> Option<VerticalDimension> {
        self.edr.get_vertical_extent()
    }

    fn supported_query_types(&self) -> Vec<String> {
        self.edr.supported_query_types()
    }

    fn serves_station_series(&self) -> bool {
        self.edr.serves_station_series()
    }

    fn query_area(
        &self,
        coords: &str,
        datetime: Datetime,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        self.derive_query(parameters, |parameters| {
            self.edr
                .query_area(coords, datetime, parameters, z, reference_time)
        })
    }

    fn query_radius(
        &self,
        coords: &str,
        within_m: f64,
        datetime: Datetime,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        self.derive_query(parameters, |parameters| {
            self.edr
                .query_radius(coords, within_m, datetime, parameters, z, reference_time)
        })
    }

    fn query_position(
        &self,
        coords: &str,
        datetime: Datetime,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        self.derive_query(parameters, |parameters| {
            self.edr
                .query_position(coords, datetime, parameters, z, reference_time)
        })
    }

    fn query_positions(
        &self,
        points: &[String],
        datetime: Datetime,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
        emit: &mut dyn FnMut(CoverageResponse) -> Result<(), DataServerError>,
    ) -> Result<(), DataServerError> {
        let plan = self.plan();
        let Some(selection) = EdrSelection::new(&plan, parameters) else {
            return self
                .edr
                .query_positions(points, datetime, parameters, z, reference_time, emit);
        };
        self.edr.query_positions(
            points,
            datetime,
            selection.inner.as_deref(),
            z,
            reference_time,
            &mut |response| emit(selection.finish(response)?),
        )
    }

    fn query_cube(
        &self,
        bbox: &Bbox,
        datetime: Datetime,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        resolution: CubeResolution,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        self.derive_query(parameters, |parameters| {
            self.edr
                .query_cube(bbox, datetime, parameters, z, resolution, reference_time)
        })
    }

    fn trajectory_shape(&self) -> TrajectoryShape {
        self.edr.trajectory_shape()
    }

    fn query_trajectory(
        &self,
        coords: &str,
        datetime: Datetime,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        self.derive_query(parameters, |parameters| {
            self.edr
                .query_trajectory(coords, datetime, parameters, z, reference_time)
        })
    }
}
