//! The snapshot a poll publishes, and the one selection of a run and step
//! that the render and both cache-key resolvers go through.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use ds_core::error::DataServerError;
use ds_core::geo::Crs;
use ds_core::map_engine::{ParameterInfo, RasterInfo};
use ds_core::native_grid::NativeGrid;
use ds_core::resample::{CurvilinearGrid, SourceGrid};
use ds_core::units::{cf_display_conversion, DisplayConversion};
use ds_core::vertical::{VerticalDimension, VerticalKind};
use ds_core::wind::{GridAxes, ParameterFacts, VectorFrame, WindFacts};
use ds_storage::ObjectVersion;

use crate::ingest::{FieldRead, FileGrid, Variable};
use crate::source::Reader;

/// Which version of a file the catalog read: its size and modification
/// time (a local file's mtime, a remote one's `Last-Modified`), and a
/// remote ETag where the server sends one; through OPeNDAP, its THREDDS
/// catalog's rounded size and modification date. A file whose version
/// changes is another file, ingested again (#1067 decision 7).
pub(crate) type Identity = ObjectVersion;

/// The identity of a local file from its metadata.
pub(crate) fn local_identity(metadata: &std::fs::Metadata) -> Identity {
    ObjectVersion {
        size: metadata.len(),
        last_modified: metadata.modified().ok().map(DateTime::<Utc>::from),
        e_tag: None,
    }
}

/// A catalogued file, open for as long as the catalog keeps it. Requests
/// read through its handle and never open a file.
pub(crate) struct OpenFile {
    /// Unique in the process: the field cache's key ([`crate::field`]).
    pub id: u64,
    /// The local path, or the object key of a remote file.
    pub path: PathBuf,
    pub identity: Identity,
    pub reader: Reader,
    pub run: Option<DateTime<Utc>>,
    /// Valid time of each index along the time axis.
    pub times: Vec<DateTime<Utc>>,
    /// How to read each of [`Layout::variables`], in its order.
    pub reads: Vec<FieldRead>,
    /// The estimated heap this open file holds, its reader's metadata
    /// with it, weighed at the open (#1307): `netcdf_open_file_bytes`.
    pub heap_bytes: u64,
}

impl OpenFile {
    /// The heap of this file's own parts, its reader aside: the open
    /// file behind its `Arc`, its path, times and reads (#1307).
    pub(crate) fn own_heap_bytes(&self) -> u64 {
        use ds_core::heap::{HeapSize, ARC_COUNTS};
        use std::mem::size_of;
        let reads = self.reads.capacity() * size_of::<FieldRead>()
            + self
                .reads
                .iter()
                .map(|read| {
                    read.name.heap_size()
                        + read.dims.capacity() * size_of::<crate::ingest::Dim>()
                        + read.packing.fill.heap_size()
                        + read.levels.heap_size()
                })
                .sum::<usize>();
        (ARC_COUNTS + size_of::<OpenFile>() + self.path.capacity() + self.times.heap_size() + reads)
            as u64
    }
}

/// The collection's variables and grid, fixed by the first file ingested:
/// every later file must carry the same variables on the same grid.
pub(crate) struct Layout {
    pub variables: Vec<Variable>,
    /// Each variable's display conversion (#1209), in `variables` order,
    /// from its CF `units` and `standard_name` through
    /// `ds_core::units::cf_display_conversion`, the rules of engine-bufr
    /// and engine-grib: K to °C, Pa to hPa, kg m-2 to mm, kg m-2 s-1 to
    /// mm/h, in any spelling, and CF's `1` to % where the
    /// standard name is a fraction (`relative_humidity`, any
    /// `…_area_fraction`). `None` for a unit without a rule (m/s, `1` of
    /// any other quantity), served as stored. Every value the engine serves
    /// is sampled from the stored field first and converted after, in
    /// `f64` ([`Self::to_display`]), so a map pixel and an EDR value agree.
    pub display: Vec<Option<DisplayConversion>>,
    pub grid: FileGrid,
    /// The sampler over the grid, built once: a curvilinear grid's lookup
    /// index (#1174) is built here, once per layout.
    pub source: SourceGrid,
    /// What the files state about wind components, for
    /// `ds_core::wind::DerivedWind`: built once with the layout, so the
    /// snapshot is the same `Arc` for the engine's life (#897).
    pub wind: Arc<WindFacts>,
    /// The collection's vertical axis (#1067 PR 8): its variables' pressure
    /// levels in hPa, bottom first ([`Variable::pressure`]), as
    /// `RasterInfo::vertical` and EDR's vertical extent advertise it. `None`
    /// when the variables have one level each.
    pub vertical: Option<VerticalDimension>,
    /// The per-step amounts derived from run-start accumulations (#1165),
    /// served after the variables ([`per_step_amounts`]); empty with
    /// `derive_per_step = false`.
    pub per_step: Vec<PerStep>,
}

/// A per-step amount derived from a run-start accumulation (#1165): MEPS's
/// `precipitation_amount_acc`, accumulated since the run started, gives
/// `precipitation_amount`, the amount of each step. Its value at a step is
/// the accumulation there less the accumulation at the run's step before,
/// each sampled and converted to the served unit, clamped at zero
/// ([`step_amount`]); a run's first step has no step before and is its own
/// accumulation. Its run, steps and levels are its accumulation's.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PerStep {
    /// The served name: the accumulation's without its `_acc`.
    pub name: Arc<str>,
    /// The accumulation, an index into [`Layout::variables`].
    pub source: usize,
    /// What it is, in words, before the step length the title adds
    /// ([`per_step_title`]): the accumulation's CF `standard_name`, else
    /// the served name (`Precipitation amount`).
    pub label: String,
}

/// A parameter a layout serves: a stored variable, or a per-step amount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Param {
    /// [`Layout::variables`] entry `i`, as stored.
    Stored(usize),
    /// [`Layout::per_step`] entry `i`.
    PerStep(usize),
}

impl Layout {
    /// The layout of `variables` on `grid`, deriving the per-step amount
    /// of each run-start accumulation when `derive_per_step` is set.
    pub(crate) fn new(variables: Vec<Variable>, grid: FileGrid, derive_per_step: bool) -> Self {
        let display: Vec<Option<DisplayConversion>> = variables
            .iter()
            .map(|v| cf_display_conversion(&v.units, v.standard_name.as_deref()))
            .collect();
        // A curvilinear grid's axes follow no projection: whether they are
        // orthogonal on the ground is unknown (#1174).
        let axes = grid
            .transform()
            .map_or(GridAxes::Skewed, |transform| GridAxes::of(&transform.crs));
        let wind = Arc::new(wind_facts(&variables, &display, axes));
        // Every variable of a layout is on the same levels, or none is
        // (`ingest::one_vertical_axis`).
        let vertical = variables
            .first()
            .and_then(|v| v.pressure.clone())
            .map(|levels| VerticalDimension::new(VerticalKind::Pressure, levels));
        let per_step = if derive_per_step {
            per_step_amounts(&variables, &display)
        } else {
            Vec::new()
        };
        let source = match &grid {
            FileGrid::Regular { transform, .. } => SourceGrid::new(transform.clone()),
            FileGrid::Curvilinear(nodes) => {
                SourceGrid::from_curvilinear(Arc::new(CurvilinearGrid::new(nodes.clone())))
            }
        };
        Self {
            source,
            variables,
            display,
            grid,
            wind,
            vertical,
            per_step,
        }
    }

    /// The unit variable `index` is served in: its display unit (`°C`,
    /// `hPa`, `mm`, `mm/h`, `%`), else its `units` as the file spells them.
    pub(crate) fn unit(&self, index: usize) -> &str {
        served_unit(&self.variables[index], self.display[index])
    }

    /// `raw`, sampled from variable `index`'s stored field, in the unit it
    /// is served in ([`Self::unit`]).
    pub(crate) fn to_display(&self, index: usize, raw: f64) -> f64 {
        self.display[index].map_or(raw, |display| display.convert(raw))
    }

    /// Every parameter served, in `RasterInfo.parameters`' order: the
    /// variables, then the per-step amounts.
    pub(crate) fn params(&self) -> impl Iterator<Item = Param> + '_ {
        (0..self.variables.len())
            .map(Param::Stored)
            .chain((0..self.per_step.len()).map(Param::PerStep))
    }

    /// The parameter served as `name`.
    pub(crate) fn param(&self, name: &str) -> Option<Param> {
        self.params().find(|&param| self.name(param) == name)
    }

    /// The name `param` is served as.
    pub(crate) fn name(&self, param: Param) -> &str {
        match param {
            Param::Stored(i) => &self.variables[i].name,
            Param::PerStep(i) => &self.per_step[i].name,
        }
    }

    /// The variable `param` reads: itself, or a per-step amount's
    /// accumulation. Its unit, display conversion and standard name are
    /// `param`'s.
    pub(crate) fn source_of(&self, param: Param) -> usize {
        match param {
            Param::Stored(i) => i,
            Param::PerStep(i) => self.per_step[i].source,
        }
    }

    /// Where `param` is in `RasterInfo.parameters` ([`Self::params`]).
    pub(crate) fn info_index(&self, param: Param) -> usize {
        match param {
            Param::Stored(i) => i,
            Param::PerStep(i) => self.variables.len() + i,
        }
    }

    /// The level a map draws, as an index into the vertical axis's levels:
    /// `z` (WMS `ELEVATION`, Maps/Tiles `elevation`), which must be one of
    /// them exactly, else the first, the bottom level. 0 without a vertical
    /// axis, where the API layers refuse a `z` before it gets here.
    ///
    /// The render's one level selection, so the default
    /// `MapEngine::resolve_elevation` (`z`, else the first advertised level)
    /// names the level drawn: GetFeatureInfo queries that level, and the
    /// rendered caches, which key on the requested `z`, hold one level per
    /// key.
    pub(crate) fn map_level(&self, z: Option<f64>) -> Result<usize, DataServerError> {
        let (Some(vertical), Some(z)) = (&self.vertical, z) else {
            return Ok(0);
        };
        vertical
            .levels
            .iter()
            .position(|&level| level == z)
            .ok_or_else(|| {
                DataServerError::InvalidParameter(format!(
                    "Level {z} is not one of this collection's pressure levels: {} hPa",
                    crate::ingest::list_levels(&vertical.levels)
                ))
            })
    }

    /// The levels an EDR query reads, as indices into the vertical axis's
    /// levels: every level without `z` (`/req/edr/z-response` F, as
    /// engine-grib's level views answer); else those `z` names, in request
    /// order and each once, a level the collection lacks dropped
    /// (clause B), and none of them a 400. `[0]` without a vertical axis,
    /// where api-edr has already ignored any `z` (clause A).
    pub(crate) fn query_levels(&self, z: Option<&[f64]>) -> Result<Vec<usize>, DataServerError> {
        let Some(vertical) = &self.vertical else {
            return Ok(vec![0]);
        };
        let Some(z) = z else {
            return Ok((0..vertical.levels.len()).collect());
        };
        let mut levels = Vec::with_capacity(z.len());
        for value in z {
            if let Some(i) = vertical.levels.iter().position(|level| level == value) {
                if !levels.contains(&i) {
                    levels.push(i);
                }
            }
        }
        if levels.is_empty() {
            return Err(DataServerError::InvalidParameter(format!(
                "The requested vertical levels select none of this collection's pressure \
                 levels: {} hPa",
                crate::ingest::list_levels(&vertical.levels)
            )));
        }
        Ok(levels)
    }
}

/// The facts of a layout the derived parameters read (wind #897 and #1067
/// decision 6, geopotential height #948, humidity #898): each variable's CF
/// `standard_name` says whether it is a component and in which frame
/// (`x_wind`/`y_wind`: along the grid's axes; `eastward_wind`/
/// `northward_wind`: along east and north), its single level is the
/// pairing level, and the grid's projection says how its axes lie. A
/// variable on the pressure axis has no pairing level (`level: None`, as
/// `ParameterFacts` asks of a parameter spanning the vertical axis):
/// `DerivedWind` reads u and v at the one `z` it is asked for, so
/// `x_wind_pl`/`y_wind_pl` pair level by level (#1067 PR 8), and its
/// `vertical` says the level is a pressure, which a humidity derived from
/// specific humidity reads (#898). MEPS's
/// Lambert grid is `GridAxes::Rotated`, so its grid-relative 10 m wind
/// gives a speed and no direction until the components are turned by the
/// convergence angle (a follow-up on #897); a curvilinear grid's are
/// `GridAxes::Skewed` (#1174), so grid-relative components give nothing
/// there, while earth-relative ones give both. Nothing is taken from a
/// name.
/// The unit is the one served ([`Layout::unit`]): `DerivedWind` combines
/// the values the engine serves.
pub(crate) fn wind_facts(
    variables: &[Variable],
    display: &[Option<DisplayConversion>],
    grid: GridAxes,
) -> WindFacts {
    WindFacts {
        grid,
        parameters: variables
            .iter()
            .zip(display)
            .map(|(v, &display)| {
                let level = (!v.levels.is_empty()).then(|| {
                    v.levels
                        .iter()
                        .map(|l| format!("{} {}", l.value, l.units).trim().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                });
                ParameterFacts {
                    standard_name: v.standard_name.clone(),
                    frame: v
                        .standard_name
                        .as_deref()
                        .map_or(VectorFrame::Unknown, VectorFrame::from_standard_name),
                    level_label: level.clone(),
                    level,
                    // The pressure axis: a humidity from specific humidity
                    // reads the level's pressure (#898).
                    vertical: v.pressure.is_some().then_some(VerticalKind::Pressure),
                    unit: served_unit(v, display).to_string(),
                    ..ParameterFacts::new(v.name.to_string())
                }
            })
            .collect(),
    }
}

/// The unit `variable` is served in under `display`: the display unit,
/// else its `units` as the file spells them.
fn served_unit(variable: &Variable, display: Option<DisplayConversion>) -> &str {
    display.map_or(variable.units.as_str(), |display| display.unit)
}

/// The per-step amounts of `variables` (#1165): one for each variable whose
/// name ends in `_acc`, MET Norway's mark of an accumulation since the run
/// start (MEPS's `precipitation_amount_acc`, and `rainfall_`, `snowfall_`
/// and `graupelfall_amount_acc`), that is served in mm: an amount of water,
/// stored in kg m-2 or mm. Only an amount is derived, since a step's is
/// clamped at zero, which would corrupt a signed accumulation such as a
/// net radiation in J m-2. Named without the `_acc`: MEPS's
/// `precipitation_amount`, CF's standard name for the precipitation and
/// the name MET Norway's Locationforecast gives its hourly amount. Not
/// derived when the layout serves a variable of that name itself.
fn per_step_amounts(variables: &[Variable], display: &[Option<DisplayConversion>]) -> Vec<PerStep> {
    variables
        .iter()
        .zip(display)
        .enumerate()
        .filter_map(|(source, (variable, &display))| {
            let name = variable
                .name
                .strip_suffix("_acc")
                .filter(|n| !n.is_empty())?;
            let amount = ds_core::units::same_unit(served_unit(variable, display), "mm");
            let taken = variables.iter().any(|v| &*v.name == name);
            if !amount || taken {
                tracing::debug!(
                    "not deriving per-step '{name}' from '{}': {}",
                    variable.name,
                    if taken {
                        "a variable has its name"
                    } else {
                        "not an amount in mm"
                    }
                );
                return None;
            }
            Some(PerStep {
                name: name.into(),
                source,
                label: in_words(variable.standard_name.as_deref().unwrap_or(name)),
            })
        })
        .collect()
}

/// `precipitation_amount` as `Precipitation amount`.
fn in_words(name: &str) -> String {
    let words = name.replace('_', " ");
    let mut chars = words.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => words,
    }
}

/// One step's amount at a point (#1165), from `now`, the run-start
/// accumulation sampled at the step in its served unit, and `before`, the
/// same at the run's step before, `None` at a run's first step, whose
/// accumulation is its own: `now` less `before`, clamped at zero, so an
/// accumulation that decreases between steps (MEPS's does, by up to
/// 0.0005 mm in the committed sample) is no amount, never a negative one.
/// `None` where either sample is, nodata or off the grid. The map and EDR
/// both call this on the samples they read, so a pixel and a position
/// value agree.
pub(crate) fn step_amount(now: Option<f64>, before: Option<Option<f64>>) -> Option<f64> {
    let before = before.unwrap_or(Some(0.0))?;
    Some((now? - before).max(0.0))
}

/// The steps a per-step amount spans in `runs` (#1165): between
/// consecutive steps of each run, and from a run's reference time to its
/// first step (none for the run-less timeline, and none for a first step
/// at the reference time, whose accumulation is zero). MEPS's are all one
/// hour.
fn step_lengths(runs: &BTreeMap<RunKey, Run>) -> std::collections::BTreeSet<chrono::Duration> {
    let mut lengths = std::collections::BTreeSet::new();
    for run in runs.values() {
        let mut before = run.reference_time;
        for &valid in run.steps.keys() {
            if let Some(before) = before.filter(|&before| before < valid) {
                lengths.insert(valid - before);
            }
            before = Some(valid);
        }
    }
    lengths
}

/// A per-step amount's title (#1165): its `label` and the step it spans,
/// as engine-grib titles a GRIB accumulation (`Precipitation amount (1 h
/// accumulation)`), or the shortest and longest when the steps differ, as
/// in a model whose steps lengthen with the lead, or a run missing a lead.
fn per_step_title(label: &str, lengths: &std::collections::BTreeSet<chrono::Duration>) -> String {
    match (lengths.first(), lengths.last()) {
        (Some(&first), Some(&last)) if first == last => {
            format!("{label} ({} accumulation)", step_words(first))
        }
        (Some(&first), Some(&last)) => format!(
            "{label} ({} to {} accumulation per forecast step)",
            step_words(first),
            step_words(last)
        ),
        _ => format!("{label} (accumulation per forecast step)"),
    }
}

/// A step's length in words: `1 h`, `15 min`, `90 s`.
fn step_words(length: chrono::Duration) -> String {
    let seconds = length.num_seconds();
    if seconds % 3600 == 0 {
        format!("{} h", seconds / 3600)
    } else if seconds % 60 == 0 {
        format!("{} min", seconds / 60)
    } else {
        format!("{seconds} s")
    }
}

/// Whether a step `old` and `new` both hold has another step before it in
/// `new` (#1165): a lead that arrived late inside a run, or was removed,
/// changes the per-step amount of the step after it, which the cache keys
/// (the step's valid time and run) do not name. The content version moves
/// then. A step appended after a run's last, or a run added or dropped,
/// changes no amount drawn before.
pub(crate) fn steps_before_changed(
    old: &BTreeMap<RunKey, Run>,
    new: &BTreeMap<RunKey, Run>,
) -> bool {
    let key = |step: Option<(DateTime<Utc>, &Step)>| {
        step.map(|(valid, step)| (valid, step.file.id, step.index))
    };
    new.iter().any(|(run_key, run)| {
        old.get(run_key).is_some_and(|before| {
            run.steps.keys().any(|&valid| {
                before.steps.contains_key(&valid)
                    && key(before.previous(valid)) != key(run.previous(valid))
            })
        })
    })
}

/// A run's key: its reference time, `None` for the timeline of files
/// without one.
pub(crate) type RunKey = Option<DateTime<Utc>>;

/// One field's place: a file and an index along its time axis.
#[derive(Clone)]
pub(crate) struct Step {
    pub file: Arc<OpenFile>,
    pub index: u32,
}

/// The valid times of one model run, or of the one timeline of files
/// without a run.
pub(crate) struct Run {
    pub reference_time: Option<DateTime<Utc>>,
    pub steps: BTreeMap<DateTime<Utc>, Step>,
}

impl Run {
    /// Whether `time` lies within the run's first and last step.
    fn covers(&self, time: DateTime<Utc>) -> bool {
        match (self.steps.first_key_value(), self.steps.last_key_value()) {
            (Some((&first, _)), Some((&last, _))) => first <= time && time <= last,
            _ => false,
        }
    }

    /// The step nearest `time` within the run, the earlier on a tie;
    /// `None` outside the run.
    fn nearest(&self, time: DateTime<Utc>) -> Option<(DateTime<Utc>, &Step)> {
        if !self.covers(time) {
            return None;
        }
        let before = self.steps.range(..=time).next_back();
        let after = self.steps.range(time..).next();
        before
            .into_iter()
            .chain(after)
            .min_by_key(|(&valid, _)| (valid - time).abs())
            .map(|(&valid, step)| (valid, step))
    }

    /// The run's step before the one valid at `valid`, `None` at its first:
    /// the step a per-step amount (#1165) is differenced against.
    pub(crate) fn previous(&self, valid: DateTime<Utc>) -> Option<(DateTime<Utc>, &Step)> {
        self.steps
            .range(..valid)
            .next_back()
            .map(|(&before, step)| (before, step))
    }
}

/// A step [`Catalog::select`] chose.
pub(crate) struct Selected<'a> {
    pub reference_time: Option<DateTime<Utc>>,
    pub valid: DateTime<Utc>,
    pub step: &'a Step,
    /// The run's step before it, `None` at the run's first: what a
    /// per-step amount (#1165) is differenced against.
    pub previous: Option<&'a Step>,
}

/// What the engine serves, swapped whole by each poll.
pub(crate) struct Catalog {
    pub layout: Option<Arc<Layout>>,
    /// By reference time, ascending. Files without one form the single
    /// timeline under `None`; a catalog never holds both kinds.
    pub runs: BTreeMap<RunKey, Run>,
    pub info: Arc<RasterInfo>,
    /// [`ds_core::map_engine::MapEngine::content_version`]: moves when a
    /// catalogued file is replaced on disk.
    pub content_version: u64,
    /// When the last poll listed the directory.
    pub polled_at: Option<DateTime<Utc>>,
}

impl Catalog {
    /// The catalog before the first scan: no layers, no data.
    pub(crate) fn empty(content_version: u64) -> Self {
        Self {
            layout: None,
            runs: BTreeMap::new(),
            info: Arc::new(raster_info(None, &BTreeMap::new(), &BTreeMap::new())),
            content_version,
            polled_at: None,
        }
    }

    /// The run and step a map of `time` from `reference_time` draws, as
    /// engine-grib's default layer selects them: a pinned run is exactly that
    /// run, absent ⇒ [`DataServerError::ReferenceTimeNotFound`] (404), and
    /// never falls back to another. Unpinned, the newest run whose steps
    /// span `time`, so an incomplete newer run does not hide an older one
    /// that covers it; without a time, the newest run. Within the run, the
    /// step nearest `time` (the earlier on a tie), or its last step without
    /// one. A time outside the chosen run is
    /// [`DataServerError::InvalidParameter`].
    ///
    /// The render, `resolve_time` and `resolve_reference_time` all call
    /// this, so the cache keys name exactly what is drawn (#507, #521):
    /// pinning the run it picks picks the same step again.
    pub(crate) fn select(
        &self,
        reference_time: Option<DateTime<Utc>>,
        time: Option<DateTime<Utc>>,
    ) -> Result<Selected<'_>, DataServerError> {
        if self.runs.is_empty() {
            return Err(DataServerError::Engine(
                "No NetCDF data available yet".to_string(),
            ));
        }
        let run = self.covering_run(reference_time, time)?;
        let (valid, step) = match time {
            Some(t) => run.nearest(t).ok_or_else(|| {
                DataServerError::InvalidParameter(format!("No forecast step for time {t}"))
            })?,
            None => run
                .steps
                .last_key_value()
                .map(|(&valid, step)| (valid, step))
                .ok_or_else(|| DataServerError::Engine("forecast run has no steps".into()))?,
        };
        Ok(Selected {
            reference_time: run.reference_time,
            valid,
            step,
            previous: run.previous(valid).map(|(_, step)| step),
        })
    }

    /// The run [`Self::select`] draws `time` from: a pinned run is exactly
    /// that run (absent ⇒ [`DataServerError::ReferenceTimeNotFound`], 404);
    /// unpinned, the newest run whose steps span `time` (none ⇒
    /// [`DataServerError::InvalidParameter`], 400), or the newest run
    /// without a time. An EDR trajectory `M` path selects its run by its
    /// earliest vertex time here, as a map selects one by `TIME`
    /// (engine-grib's `covering_run`), and its samples then snap to that
    /// run's steps.
    pub(crate) fn covering_run(
        &self,
        reference_time: Option<DateTime<Utc>>,
        time: Option<DateTime<Utc>>,
    ) -> Result<&Run, DataServerError> {
        match (reference_time, time) {
            (Some(rt), _) => self.runs.get(&Some(rt)).ok_or_else(|| {
                DataServerError::ReferenceTimeNotFound(format!(
                    "no forecast run for reference time {rt}"
                ))
            }),
            (None, Some(t)) => self
                .runs
                .values()
                .rev()
                .find(|run| run.covers(t))
                .ok_or_else(|| {
                    DataServerError::InvalidParameter(format!("No forecast run covers time {t}"))
                }),
            (None, None) => self
                .runs
                .values()
                .next_back()
                .ok_or_else(|| DataServerError::Engine("No NetCDF data available yet".into())),
        }
    }

    /// The run an EDR query reads, as engine-grib selects it
    /// (`/req/core/datetime-response` A and F): a pinned run is exactly
    /// that run (absent ⇒ [`DataServerError::ReferenceTimeNotFound`], 404).
    /// Unpinned with a `datetime` window, the newest run with a step inside
    /// it that also spans the window's start, else the newest with any step
    /// inside, which is how an open start (`../end`) or a window starting
    /// before a run finds one; none ⇒ [`no_step_within`] (404), so a
    /// `datetime` list skips the instant (#968). Without a window, the
    /// newest run. An instant `(t, t)` needs a step valid at exactly `t`,
    /// never the nearest: GetFeatureInfo asks for the step its map drew,
    /// in the run the map drew it from.
    pub(crate) fn query_run(
        &self,
        reference_time: Option<DateTime<Utc>>,
        window: Option<(DateTime<Utc>, DateTime<Utc>)>,
    ) -> Result<&Run, DataServerError> {
        if self.runs.is_empty() {
            return Err(DataServerError::LocationNotFound(
                "No NetCDF data available yet".into(),
            ));
        }
        if let Some(rt) = reference_time {
            return self.runs.get(&Some(rt)).ok_or_else(|| {
                DataServerError::ReferenceTimeNotFound(format!(
                    "no forecast run for reference time {rt}"
                ))
            });
        }
        let Some((start, end)) = window else {
            return Ok(self.runs.values().next_back().expect("runs is not empty"));
        };
        let newest_first = || {
            self.runs
                .values()
                .rev()
                .filter(move |run| run.steps_within(start, end).next().is_some())
        };
        newest_first()
            .find(|run| run.covers(start))
            .or_else(|| newest_first().next())
            .ok_or_else(|| no_step_within(start, end))
    }
}

impl Run {
    /// The steps valid inside `[start, end]`, both ends included, ascending.
    pub(crate) fn steps_within(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> impl Iterator<Item = (DateTime<Utc>, &Step)> + '_ {
        // `range` panics on a reversed range, which the API refuses (#932).
        let start = start.min(end);
        self.steps
            .range(start..=end)
            .map(|(&valid, step)| (valid, step))
    }
}

/// No step is valid inside the requested `datetime`: the 404 an instant
/// without data gets, which a `datetime` list skips (#968).
pub(crate) fn no_step_within(start: DateTime<Utc>, end: DateTime<Utc>) -> DataServerError {
    let bound = |time: DateTime<Utc>| {
        if time == DateTime::<Utc>::MIN_UTC || time == DateTime::<Utc>::MAX_UTC {
            "..".to_string()
        } else {
            time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        }
    };
    let datetime = if start == end {
        bound(start)
    } else {
        format!("{}/{}", bound(start), bound(end))
    };
    DataServerError::LocationNotFound(format!("No forecast step matches datetime {datetime}"))
}

/// The `RasterInfo` of a catalog, built once per poll (Critical Rule 10).
pub(crate) fn raster_info(
    layout: Option<&Layout>,
    runs: &BTreeMap<RunKey, Run>,
    titles: &BTreeMap<String, String>,
) -> RasterInfo {
    let mut times: Vec<DateTime<Utc>> = runs
        .values()
        .flat_map(|run| run.steps.keys().copied())
        .collect();
    times.sort_unstable();
    times.dedup();
    let parameters: Vec<ParameterInfo> = layout
        .map(|layout| {
            let lengths = step_lengths(runs);
            let variables = layout
                .variables
                .iter()
                .enumerate()
                .map(|(index, v)| ParameterInfo {
                    name: v.name.to_string(),
                    title: titles
                        .get(&*v.name)
                        .cloned()
                        .unwrap_or_else(|| v.title.clone()),
                    unit: layout.unit(index).to_string(),
                });
            // A per-step amount's title says the step it spans, from the
            // runs this catalog holds; its unit is its accumulation's.
            let per_step = layout.per_step.iter().map(|p| ParameterInfo {
                name: p.name.to_string(),
                title: per_step_title(&p.label, &lengths),
                unit: layout.unit(p.source).to_string(),
            });
            variables.chain(per_step).collect()
        })
        .unwrap_or_default();
    let grid = layout.map(|layout| &layout.grid);
    let transform = grid.and_then(FileGrid::transform);
    RasterInfo {
        native_crs: match grid {
            // Lon-first degrees, OGC `storageCrs` CRS84. A projected grid
            // has no stable OGC URI, so its label leaves `storageCrs` and
            // the Common grid out, as engine-querydata's does; a
            // curvilinear one (#1174) has no CRS at all.
            None => "CRS:84",
            Some(FileGrid::Curvilinear(_)) => "curvilinear",
            Some(FileGrid::Regular { transform, .. }) => match transform.crs {
                Crs::Wgs84 => "CRS:84",
                Crs::Geostationary { .. } => "geos",
                _ => "projected",
            },
        }
        .to_string(),
        // A polar grid's edges circle its pole: the extent reaches the
        // pole. A curvilinear grid's comes from its outer nodes, computed
        // once with its index.
        spatial_extent: layout.and_then(|layout| match &layout.grid {
            FileGrid::Regular { transform, .. } => {
                ds_core::geo::crs84_extent(transform.bbox_with_poles())
            }
            FileGrid::Curvilinear(_) => layout.source.curvilinear()?.extent(),
        }),
        times,
        parameter: parameters
            .first()
            .map(|p| p.name.clone())
            .unwrap_or_default(),
        unit: parameters
            .first()
            .map(|p| p.unit.clone())
            .unwrap_or_default(),
        parameters,
        vertical: layout.and_then(|layout| layout.vertical.clone()),
        grid_size: grid.map(|g| {
            let (width, height) = g.size();
            [width, height]
        }),
        layer_subtitle: None,
        reference_times: runs.keys().flatten().copied().collect(),
        // The CF grid as read, in its own CRS (#1237). CF states no EPSG
        // code MeteoCore reads, so the CRS is described by its parameters.
        // A curvilinear grid has no CRS to describe it in.
        native_grid: transform.and_then(|t| NativeGrid::from_geo_transform(t, None)),
    }
}

#[cfg(test)]
mod tests;
