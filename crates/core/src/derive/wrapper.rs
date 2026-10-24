//! [`Derived`]: serves a [`Plan`]'s parameters over any engine's
//! `MapEngine` + `EdrEngine`, the way a nowcast wraps its source.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use chrono::{DateTime, Utc};

use super::{DerivationLog, Derivations, DerivedParameter, Outcome, Plan};
use crate::cube::CubeResolution;
use crate::edr_engine::{EdrEngine, ItemRadius, LocationFilter, TrajectoryShape};
use crate::error::DataServerError;
use crate::feature::{Bbox, DatetimeInterval, Feature, MAX_AREA_VALUES};
use crate::instances::RunInfo;
use crate::map_engine::{
    CompositeDef, MapEngine, OutputCrs, ParameterInfo, RasterInfo, RasterTile, RasterValues,
};
use crate::model::{
    CoverageResponse, LocationSnapshot, NdArray, ParameterDescription, QueryResult,
};
use crate::vertical::VerticalDimension;
use crate::wind::{OutcomeLog, WindFacts, WindSource};

type Times = Arc<[DateTime<Utc>]>;
type Datetime = Option<(DateTime<Utc>, DateTime<Utc>)>;
type OutcomeKey = (&'static str, String, String);

/// An engine with parameters derived from its own (wind speed and direction
/// from u/v components, #897; geopotential height from geopotential, #948).
/// Every `MapEngine` and `EdrEngine` method delegates to the wrapped
/// engine; the derived parameters are added on top:
///
/// - **Map APIs**: the derived parameters a palette can draw
///   ([`super::Formula::on_map`]: wind speed, geopotential height), as
///   extra `RasterInfo::parameters`. A derived tile is its input tiles of
///   the same request (bbox, size, time, z, run), combined per pixel: no
///   second projection, and the engine's own sampling.
/// - **EDR**: every derived parameter in `parameter_names` and every
///   query. The inputs are queried in place of the derived names and
///   combined per value; inputs the client did not ask for are dropped
///   again. The response, derived values included, must fit the shared
///   value budget ([`MAX_AREA_VALUES`]); it is counted before anything is
///   derived.
///
/// Time, run and content resolve as the inputs do (root `CLAUDE.md`,
/// adding-an-engine step 7). A parameter derived from one input resolves
/// exactly as that input does. One derived from several has the
/// intersection of their time axes, and its cache-key time is
/// `resolve_parameters_time` over them, so a cached tile never names a
/// timestep that was not rendered (#507/#521).
///
/// Every capability accessor stays O(1) (Critical Rule 10): the plan is
/// rebuilt only when the source's [`WindFacts`] snapshot changes, and the
/// extended `RasterInfo` only when the plan or the engine's snapshot does.
pub struct Derived {
    collection_id: String,
    map: Arc<dyn MapEngine>,
    edr: Arc<dyn EdrEngine>,
    source: Arc<dyn WindSource>,
    derive: Derivations,
    log: DerivationLog,
    plan: RwLock<Option<(Arc<WindFacts>, Arc<Plan>)>>,
    raster: RwLock<Option<RasterSnapshot>>,
    axes: RwLock<HashMap<String, AxisSnapshot>>,
    logged: Mutex<HashMap<OutcomeKey, Outcome>>,
}

struct RasterSnapshot {
    inner: Arc<RasterInfo>,
    plan: Arc<Plan>,
    info: Arc<RasterInfo>,
}

struct AxisSnapshot {
    inputs: Vec<Times>,
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

impl Derived {
    /// Wrap `engine` for wind only (#897), reporting each u/v pair's
    /// outcome to `log`. The server registers through [`Derived::with`],
    /// which takes the collection's derivations.
    pub fn new<E>(collection_id: impl Into<String>, engine: Arc<E>, log: OutcomeLog) -> Self
    where
        E: MapEngine + EdrEngine + WindSource + 'static,
    {
        let log: DerivationLog = Arc::new(move |id: &str, outcome: &Outcome| {
            if let Outcome::Wind(pair) = outcome {
                log(id, pair);
            }
        });
        Self::with(collection_id, engine, Derivations::WIND, log)
    }

    /// Wrap `engine` for the derivations `derive` turns on, plan them and
    /// report each candidate's outcome.
    pub fn with<E>(
        collection_id: impl Into<String>,
        engine: Arc<E>,
        derive: Derivations,
        log: DerivationLog,
    ) -> Self
    where
        E: MapEngine + EdrEngine + WindSource + 'static,
    {
        let wrapper = Self {
            collection_id: collection_id.into(),
            map: engine.clone(),
            edr: engine.clone(),
            source: engine,
            derive,
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
    pub fn plan(&self) -> Arc<Plan> {
        let facts = self.source.wind_facts();
        if let Some((cached, plan)) = &*read(&self.plan) {
            if Arc::ptr_eq(cached, &facts) {
                return plan.clone();
            }
        }
        let plan = Arc::new(Plan::build(&facts, self.derive));
        self.report(&plan);
        *write(&self.plan) = Some((facts, plan.clone()));
        plan
    }

    fn report(&self, plan: &Plan) {
        let mut logged = lock(&self.logged);
        for outcome in &plan.outcomes {
            let (kind, a, b) = outcome.key();
            let key = (kind, a.to_string(), b.to_string());
            if logged.get(&key) != Some(outcome) {
                (self.log)(&self.collection_id, outcome);
                logged.insert(key, outcome.clone());
            }
        }
    }

    /// A derived tile: its input tiles of the request, combined per pixel.
    /// Each input but the last is converted as it arrives, so a boxed `F64`
    /// input is never held twice.
    #[allow(clippy::too_many_arguments)] // mirrors get_raster_tile
    fn derived_tile(
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
        let names: Vec<&str> = d.inputs.iter().map(String::as_str).collect();
        // One input renders as the engine renders it. Several render from
        // one run: the one the cache key names
        // (`resolve_parameter_reference_time`). Unpinned, an engine whose
        // run depends on the parameter would otherwise pick one for each,
        // and pair a new run's u with an older run's v (#1026).
        let (time, reference_time) = if names.len() < 2 {
            (time, reference_time)
        } else {
            let reference_time = reference_time.or_else(|| {
                self.map
                    .resolve_parameters_reference_time(&names, time, None)
            });
            // Inputs on time axes of their own render from one timestep
            // they share: the one the cache key names
            // (`resolve_parameter_time`).
            let time = if names.iter().any(|p| self.map.parameter_times(p).is_some()) {
                let shared = self
                    .map
                    .resolve_parameters_time(&names, time, reference_time)
                    .ok_or_else(|| {
                        DataServerError::InvalidParameter(format!(
                            "{} share no timestep",
                            DerivedParameter::quoted(&d.inputs)
                        ))
                    })?;
                Some(shared)
            } else {
                time
            };
            (time, reference_time)
        };
        let pixels = width as usize * height as usize;
        let input = |name: &str| {
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
                    "{} '{name}' returned a {}×{} tile for a {width}×{height} request",
                    d.formula.input_noun(),
                    tile.width,
                    tile.height
                )));
            }
            Ok(tile)
        };
        let Some((&last, first)) = names.split_last() else {
            return Err(DataServerError::Engine(format!(
                "'{}' is derived from no parameter",
                d.name
            )));
        };
        let mut converted: Vec<Vec<f64>> = Vec::with_capacity(first.len());
        for &name in first {
            let tile = input(name)?;
            converted.push(
                tile.values
                    .iter_values()
                    .map(|value| value.unwrap_or(f64::NAN))
                    .collect(),
            );
            crate::deadline::check()?;
        }
        let last = input(last)?;
        let mut samples: Vec<Option<f64>> = Vec::with_capacity(names.len());
        let data = last
            .values
            .iter_values()
            .enumerate()
            .map(|(i, value)| {
                samples.clear();
                samples.extend(converted.iter().map(|values| Some(values[i])));
                samples.push(value);
                d.value(&samples).map_or(f32::NAN, |value| value as f32)
            })
            .collect();
        Ok(RasterTile {
            width,
            height,
            values: RasterValues::F32 { data, nodata: None },
        })
    }

    /// The timesteps several inputs share: cached until any axis snapshot
    /// changes.
    fn common_axis(&self, name: &str, inputs: Vec<Times>) -> Times {
        if let Some(axis) = read(&self.axes).get(name) {
            if axis.inputs.len() == inputs.len()
                && axis
                    .inputs
                    .iter()
                    .zip(&inputs)
                    .all(|(a, b)| Arc::ptr_eq(a, b))
            {
                return axis.common.clone();
            }
        }
        let common: Times = intersect(&inputs).into();
        write(&self.axes).insert(
            name.to_string(),
            AxisSnapshot {
                inputs,
                common: common.clone(),
            },
        );
        common
    }

    /// Run an EDR query, deriving the requested derived parameters from the
    /// inputs the wrapped engine returns.
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

/// `parameters` with each derived one replaced by its inputs, each name
/// once, in order: what the wrapped engine renders.
fn inputs<'a>(plan: &'a Plan, parameters: &[&'a str]) -> Vec<&'a str> {
    let mut expanded: Vec<&str> = Vec::with_capacity(parameters.len() + 2);
    for &p in parameters {
        let names: Vec<&str> = match plan.get(p) {
            Some(d) => d.inputs.iter().map(String::as_str).collect(),
            None => vec![p],
        };
        for name in names {
            if !expanded.contains(&name) {
                expanded.push(name);
            }
        }
    }
    expanded
}

/// Ascending timesteps present in every one of the ascending axes.
fn intersect(axes: &[impl AsRef<[DateTime<Utc>]>]) -> Vec<DateTime<Utc>> {
    let Some((first, rest)) = axes.split_first() else {
        return Vec::new();
    };
    first
        .as_ref()
        .iter()
        .filter(|t| {
            rest.iter()
                .all(|axis| axis.as_ref().binary_search(t).is_ok())
        })
        .copied()
        .collect()
}

/// An EDR request rewritten for the wrapped engine.
struct EdrSelection {
    /// The `parameter-name` list the wrapped engine sees: the requested
    /// names, derived ones replaced by their inputs.
    inner: Option<Vec<String>>,
    derive: Vec<DerivedParameter>,
    /// Inputs only the derivation asked for.
    drop: Vec<String>,
}

impl EdrSelection {
    /// `None` when the request names no derived parameter (pass through).
    fn new(plan: &Plan, requested: Option<&[String]>) -> Option<Self> {
        if plan.derived.is_empty() {
            return None;
        }
        let Some(names) = requested else {
            // Every parameter: the inputs are in the response anyway.
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
            for input in &d.edr_inputs {
                if !inner.iter().any(|n| n.eq_ignore_ascii_case(input)) {
                    inner.push(input.clone());
                    drop.push(input.clone());
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
    /// The wrapped engine checked its own request, which named the inputs
    /// in place of the derived parameters; the response adds a range per
    /// derived parameter and drops the inputs only the derivation asked
    /// for. Its values must still fit the shared response budget
    /// ([`MAX_AREA_VALUES`]), or the query is too large: without
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
                let input = d.edr_inputs.iter().find_map(|name| result.ranges.get(name));
                if let Some(range) = input {
                    derived = derived.saturating_add(range.values.len());
                }
            }
        }
        let total = served.saturating_add(derived);
        if total > limit {
            // `derived wind values` when every derived parameter is wind's.
            let quantity = match self.derive.split_first() {
                Some((first, rest))
                    if rest
                        .iter()
                        .all(|d| d.formula.quantity() == first.formula.quantity()) =>
                {
                    format!("{} ", first.formula.quantity())
                }
                _ => String::new(),
            };
            return Err(DataServerError::QueryTooLarge(format!(
                "Query would return {total} values ({served} queried + {derived} derived \
                 {quantity}values); the limit is {limit} — narrow the datetime window, the \
                 area, z or the parameters"
            )));
        }
        Ok(())
    }

    /// Combine the input ranges value by value, after sampling: a derived
    /// value is never interpolated.
    fn finish_result(&self, result: &mut QueryResult) -> Result<(), DataServerError> {
        for d in &self.derive {
            let ranges: Vec<Option<&NdArray>> = d
                .edr_inputs
                .iter()
                .map(|name| result.ranges.get(name))
                .collect();
            let Some(first) = ranges.iter().flatten().next().copied() else {
                continue;
            };
            let range = if ranges.iter().all(Option::is_some) {
                let ranges: Vec<&NdArray> = ranges.into_iter().flatten().collect();
                if ranges
                    .iter()
                    .any(|r| r.shape != first.shape || r.values.len() != first.values.len())
                {
                    return Err(DataServerError::Engine(format!(
                        "{}s {} returned ranges of different shapes",
                        d.formula.input_noun(),
                        DerivedParameter::quoted(&d.edr_inputs)
                    )));
                }
                let mut samples: Vec<Option<f64>> = Vec::with_capacity(ranges.len());
                NdArray {
                    shape: first.shape.clone(),
                    axis_names: first.axis_names.clone(),
                    values: (0..first.values.len())
                        .map(|i| {
                            samples.clear();
                            samples.extend(ranges.iter().map(|r| r.values[i]));
                            d.value(&samples)
                        })
                        .collect(),
                }
            } else {
                // An input missing throughout: missing values.
                NdArray {
                    shape: first.shape.clone(),
                    axis_names: first.axis_names.clone(),
                    values: vec![None; first.values.len()],
                }
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
        standard_name: Some(d.formula.standard_name().to_string()),
    }
}

impl MapEngine for Derived {
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
                if !d.formula.on_map() {
                    return Err(DataServerError::InvalidParameter(format!(
                        "'{name}' is a wind direction, which only EDR serves"
                    )));
                }
                return self.derived_tile(
                    d,
                    bbox,
                    width,
                    height,
                    time,
                    output_crs,
                    z,
                    reference_time,
                );
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
        if !plan.derived.iter().any(|d| d.formula.on_map()) {
            return inner;
        }
        if let Some(snapshot) = &*read(&self.raster) {
            if Arc::ptr_eq(&snapshot.inner, &inner) && Arc::ptr_eq(&snapshot.plan, &plan) {
                return snapshot.info.clone();
            }
        }
        let mut info = (*inner).clone();
        for d in plan.derived.iter().filter(|d| d.formula.on_map()) {
            let has = |name: &str| info.parameters.iter().any(|p| p.name == name);
            if d.inputs.iter().all(|input| has(input)) && !has(&d.name) {
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

    /// A parameter derived from one input renders from that input's run. One
    /// derived from several renders from the run they are rendered together
    /// from (`resolve_parameters_reference_time`): a u component only an
    /// older run still has pairs from that run (#1026).
    /// `resolve_parameter_time` then picks the step they all have within
    /// that run, so the cache key keeps naming what is rendered.
    fn resolve_parameter_reference_time(
        &self,
        parameter: Option<&str>,
        time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        let plan = self.plan();
        match parameter.and_then(|p| plan.get(p)) {
            Some(d) => match d.inputs.as_slice() {
                [input] => {
                    self.map
                        .resolve_parameter_reference_time(Some(input), time, reference_time)
                }
                inputs => {
                    let names: Vec<&str> = inputs.iter().map(String::as_str).collect();
                    self.map
                        .resolve_parameters_reference_time(&names, time, reference_time)
                }
            },
            None => self
                .map
                .resolve_parameter_reference_time(parameter, time, reference_time),
        }
    }

    fn resolve_parameters_reference_time(
        &self,
        parameters: &[&str],
        time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        let plan = self.plan();
        self.map
            .resolve_parameters_reference_time(&inputs(&plan, parameters), time, reference_time)
    }

    fn content_version(&self) -> u64 {
        self.map.content_version()
    }

    fn parameter_times(&self, parameter: &str) -> Option<Times> {
        let plan = self.plan();
        let Some(d) = plan.get(parameter) else {
            return self.map.parameter_times(parameter);
        };
        let mut axes: Vec<Times> = d
            .inputs
            .iter()
            .filter_map(|input| self.map.parameter_times(input))
            .collect();
        match axes.len() {
            0 => None,
            // An input without an axis of its own has every time.
            1 => axes.pop(),
            _ => Some(self.common_axis(&d.name, axes)),
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
            Some(d) => match d.inputs.as_slice() {
                [input] => self
                    .map
                    .resolve_parameter_time(Some(input), time, reference_time),
                inputs => {
                    let names: Vec<&str> = inputs.iter().map(String::as_str).collect();
                    self.map
                        .resolve_parameters_time(&names, time, reference_time)
                }
            },
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
        self.map
            .resolve_parameters_time(&inputs(&plan, parameters), time, reference_time)
    }

    fn composites(&self) -> Arc<[CompositeDef]> {
        self.map.composites()
    }

    /// A derived parameter shows its own values, which the wrapped position
    /// query derives too; every other layer is the wrapped engine's.
    fn layer_shows_values(&self, parameter: &str) -> bool {
        self.plan().get(parameter).is_some() || self.map.layer_shows_values(parameter)
    }

    /// A derived parameter is drawn from its inputs at the `z` it is passed,
    /// so it is drawn at the level the wrapped engine draws its first input
    /// (u, geopotential) at; every other layer is the wrapped engine's.
    fn resolve_elevation(
        &self,
        parameter: Option<&str>,
        z: Option<f64>,
        time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<f64> {
        let plan = self.plan();
        let parameter = match parameter.and_then(|p| plan.get(p)) {
            Some(d) => d.inputs.first().map(String::as_str),
            None => parameter,
        };
        self.map
            .resolve_elevation(parameter, z, time, reference_time)
    }
}

impl EdrEngine for Derived {
    fn get_locations(&self) -> Result<Arc<LocationSnapshot>, DataServerError> {
        self.edr.get_locations()
    }

    fn location_time_filter<'a>(
        &'a self,
        intervals: &'a [DatetimeInterval],
    ) -> Option<LocationFilter<'a>> {
        self.edr.location_time_filter(intervals)
    }

    fn item_radius(&self, feature: &Feature) -> Option<ItemRadius> {
        self.edr.item_radius(feature)
    }

    fn get_instances(&self) -> Vec<RunInfo> {
        self.edr.get_instances()
    }

    fn has_instances(&self) -> bool {
        self.edr.has_instances()
    }

    fn instance_reference_times(&self) -> Vec<DateTime<Utc>> {
        self.edr.instance_reference_times()
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
            if d.edr_inputs.iter().all(|input| names.contains(input)) && !names.contains(&d.name) {
                names.push(d.name.clone());
            }
        }
        names
    }

    fn get_parameter_descriptions(&self) -> HashMap<String, ParameterDescription> {
        let mut descriptions = self.edr.get_parameter_descriptions();
        let plan = self.plan();
        for d in &plan.derived {
            if d.edr_inputs
                .iter()
                .all(|input| descriptions.contains_key(input))
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

    fn observation_instants(&self) -> Option<Arc<[DateTime<Utc>]>> {
        self.edr.observation_instants()
    }

    fn get_parameter_available_times(&self, parameter: &str) -> Option<Vec<DateTime<Utc>>> {
        let plan = self.plan();
        let Some(d) = plan.get(parameter) else {
            return self.edr.get_parameter_available_times(parameter);
        };
        let mut axes: Vec<Vec<DateTime<Utc>>> = d
            .edr_inputs
            .iter()
            .filter_map(|input| self.edr.get_parameter_available_times(input))
            .collect();
        match axes.len() {
            0 => None,
            1 => axes.pop(),
            _ => Some(intersect(&axes)),
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
