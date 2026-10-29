//! OGC API - EDR position, area, radius, trajectory and cube queries
//! (#1067 PR 7, #1169), and the model runs as EDR instances.
//!
//! A query selects one run and its steps ([`Catalog::query_run`]) and, on a
//! pressure-level collection, its levels ([`Layout::query_levels`]: `z`, or
//! every level), plans its reads ([`NetcdfEngine::plan_reads`]) and checks
//! them against both caps, [`QUERY_BYTES`] and [`QUERY_FIELD_READS`], before
//! reading anything, then reads each field (parameter, step, level) once
//! through `ds_executor::field_jobs` (at most four reads at once, each under
//! the query's deadline) and samples it at every point the query asks for:
//! each point, each cell centre of an area's or a cube's CRS84 grid, or
//! each sample of a trajectory, is projected onto the source grid once
//! ([`SourceGrid::position`]) and sampled as a map draws it
//! ([`SourceGrid::value_at`]) and converted to the unit served as the map
//! converts it ([`Layout::to_display`], #1209), so GetFeatureInfo answers
//! what GetMap drew. Field reads go through [`field::read_uncached`]: a
//! decoded field the render cache holds is used, any other is decoded for
//! the query alone. Trajectories ([`trajectory`]) and cubes ([`cube`]) read
//! only the fields they sample, and the cap counts exactly those.
//!
//! Through OPeNDAP a query reads only the window of each field its
//! positions sample (#1171): the cells around every position on the grid
//! ([`SourceGrid::cells_read`]), joined into one hyperslab per field, at
//! most half the field ([`worth_a_window`]), and samples the window to the
//! value the whole field gives ([`field::QueryField::value_at`]). A 67-lead
//! MEPS series at a point reads 67 replies of 428 bytes, not 67 of
//! 4.06 MB. The levels a query asks of one variable at one step are one
//! read: one hyperslab over the stored levels from the lowest asked to the
//! highest, over the window joining theirs, while that reads at most half
//! of what their fields whole hold. A 13-level MEPS series over a 67-lead
//! run is 67 requests, not 871: thredds.met.no answers a 2 × 2 window of
//! one level in about 0.27 s and of all 13 in about 0.53 s (measured from a
//! server's network, 2026-10-24). And the windows of several variables at
//! one step are one request (#1274), one hyperslab each, at most
//! [`MAX_REQUEST_PLANES`] planes (variables × the levels of their
//! hyperslabs) per request: a six-variable surface meteogram over a MEPS
//! run is 67 requests, not 402.
//!
//! A per-step amount (#1165) is sampled from its run-start accumulation at
//! each step and at the run's step before, at the same positions, and the
//! samples differenced ([`crate::catalog::step_amount`]), as the map
//! differences them. Every (variable, step, level) field a query needs, the
//! steps before included, is read once ([`NetcdfEngine::sample_fields`]),
//! and the cap counts the steps before.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, LazyLock};

use chrono::{DateTime, Utc};
use ds_core::cube::CubeResolution;
use ds_core::edr_engine::{select_parameters, EdrEngine};
use ds_core::error::DataServerError;
use ds_core::feature::{
    check_area_budget, check_mask_budget, parse_area_coords, parse_point_coords, Bbox, MAX_AREA_DIM,
};
use ds_core::instances::RunInfo;
use ds_core::model::{
    CoverageResponse, DomainDescription, LocationSnapshot, NdArray, ParameterDescription,
    QueryResult, VerticalCoord,
};
use ds_core::resample::{CellWindow, SourceGrid};
use ds_core::vertical::VerticalDimension;

use crate::catalog::{step_amount, Catalog, Layout, Param, Run, Step};
use crate::field;
use crate::NetcdfEngine;

mod cube;
mod trajectory;

/// A query's `datetime` window, both ends included.
type Window = Option<(DateTime<Utc>, DateTime<Utc>)>;

/// The most decoded bytes one EDR query may read, `MC_NETCDF_EDR_QUERY_MB`
/// (default 544 MiB): every read the query plans counted at what it
/// decodes ([`Member::bytes`]), from the metadata, before any read. A whole
/// field counts [`crate::ingest::FieldRead::decoded_bytes`] (4.06 MB for
/// MEPS), a window read through OPeNDAP its cells × the levels of its
/// hyperslab × 4 bytes (a MEPS point's 2 × 2 cells over 13 levels, 208).
/// The default admits one parameter over one MEPS run read whole, as the
/// client sees it, including a derived wind speed. The engine counts the
/// variables it reads, and a derived wind speed
/// (`ds_core::wind::DerivedWind`) asks for both of its components: one
/// over a MEPS run is 134 fields of 4.06 MB (519 MiB), while three
/// variables over a run (201 fields) are refused, as is one variable over
/// a run at all 13 pressure levels (871 fields) read whole. Windows hardly
/// count: for them [`QUERY_FIELD_READS`] binds first. A cold MEPS series read
/// whole by range is about 100 MB of compressed chunks per variable read:
/// 3.4 MB/s sustained to answer within the 30 s EDR deadline, twice that
/// for a derived wind speed.
pub(crate) static QUERY_BYTES: LazyLock<u64> = LazyLock::new(|| {
    ds_cache::env_mb("MC_NETCDF_EDR_QUERY_MB", DEFAULT_QUERY_MB).saturating_mul(ds_cache::MIB)
});

/// [`QUERY_BYTES`]' default, in MiB.
const DEFAULT_QUERY_MB: u64 = 544;

/// The most field reads one EDR query may make, `MC_NETCDF_EDR_QUERY_READS`
/// (default 200), counted from its plan before any read, for every access:
/// a whole field of one level is one read, and through OPeNDAP one request
/// is one read, however many windows it holds: the levels of a variable
/// at a step in one hyperslab, and the windows of several variables at a
/// step in one constraint, up to [`MAX_REQUEST_PLANES`] planes. A
/// six-variable surface meteogram over a 67-lead MEPS run is 67 reads; one
/// variable at every pressure level over the run 67, a derived wind speed
/// (both components, a request each) at every level 134. At about 0.55 s
/// at most per request (a 13-level window, or 16 planes), four at a time,
/// 200 reads take about 27 s: inside the 30 s EDR deadline. Three
/// variables at every level over a whole run (201 reads, each 13-level
/// variable a request of its own) are just over it. Read whole (a local
/// file, by HTTP range, a window past half the field), [`QUERY_BYTES`]
/// binds first: 140 MEPS fields. `0` refuses every query that reads a
/// field. Not [`field::QUERY_READS`], the pool a query's detached reads run
/// in.
pub(crate) static QUERY_FIELD_READS: LazyLock<usize> =
    LazyLock::new(|| env_count("MC_NETCDF_EDR_QUERY_READS", DEFAULT_QUERY_FIELD_READS));

/// [`QUERY_FIELD_READS`]' default.
const DEFAULT_QUERY_FIELD_READS: usize = 200;

/// The most planes one OPeNDAP request of a query reads (#1274): the
/// windows of several variables at one step are one request while their
/// planes, each variable's stored levels in its hyperslab, add up to at
/// most this. Measured from a server near thredds.met.no on 2026-10-24, 2 ×
/// 2 windows of MEPS `pl` files: a request costs about 0.25 s plus 0.016 s
/// per plane (1 variable × 1 level 0.23-0.34 s, 3 × 1 0.35-0.42 s, 1 × 13
/// 0.49-0.59 s, 5 × 13 1.1-1.6 s). At 16 planes a request takes about as
/// long as the 13-level requests [`QUERY_FIELD_READS`]' default was sized
/// for: sixteen single-level variables share a request, while a 13-level
/// variable fills one alone.
pub(crate) const MAX_REQUEST_PLANES: u64 = 16;

/// A count from the environment, parsed as `ds_cache::env_mb` parses a
/// size: trimmed, `default` when unset or not a number.
fn env_count(var: &str, default: usize) -> usize {
    std::env::var(var)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(default)
}

/// The caps one query's planned reads must fit, both checked before any
/// read ([`Tally::check`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    /// Decoded bytes ([`QUERY_BYTES`]).
    pub bytes: u64,
    /// Field reads ([`QUERY_FIELD_READS`]).
    pub reads: usize,
}

impl Limits {
    /// `MC_NETCDF_EDR_QUERY_MB` and `MC_NETCDF_EDR_QUERY_READS`.
    fn configured() -> Self {
        Limits {
            bytes: *QUERY_BYTES,
            reads: *QUERY_FIELD_READS,
        }
    }
}

/// Where a query samples: a point's fractional source position, `None` for
/// a point off the grid or an area cell masked out of the polygon.
type Positions = Arc<Vec<Option<(f64, f64)>>>;

/// The fractional source position of WGS84 `(lon, lat)`, projected once,
/// or `None` off the grid, where every field samples null
/// ([`SourceGrid::covers`]).
fn on_grid(source: &SourceGrid, lon: f64, lat: f64) -> Option<(f64, f64)> {
    let position = source.position(lon, lat);
    source.covers(position).then_some(position)
}

/// The cells sampling `positions` reads (#1171): each position's
/// [`SourceGrid::cells_read`], joined; `None` with none on the grid.
fn cells_read(source: &SourceGrid, positions: &Positions) -> Option<CellWindow> {
    positions
        .iter()
        .flatten()
        .filter_map(|&position| source.cells_read(position))
        .reduce(CellWindow::union)
}

/// Whether a query reads `window` of `levels` stored levels (its
/// hyperslab's) rather than the `asked` levels' fields whole (#1171): while
/// the window reads at most half the cells those fields hold. A whole
/// field's reply, kept in the chunk cache, serves every later query and map
/// of the field, a window's only the same window again; with no telling
/// whether the field will be wanted whole, reading a window and then the
/// field costs window + field, and the window alone saves only while it is
/// under half the field. Point queries are far under it (a MEPS point is
/// 2 × 2 of 949 × 1069 cells); an area or a cube passes it over more than
/// half the grid, and reads the field whole. For one level `levels` and
/// `asked` are 1; a hyperslab over several stored levels also reads the
/// levels between those asked, so it passes the test only while they are
/// cheap.
fn worth_a_window(window: CellWindow, levels: u64, asked: usize, source: &SourceGrid) -> bool {
    let (columns, rows) = source.size();
    let field = u64::from(columns) * u64::from(rows);
    window.cells().saturating_mul(levels).saturating_mul(2) <= field.saturating_mul(asked as u64)
}

/// The reads of `planned`, each a variable's read at a step: a whole
/// field is a read of its own; the windows of one step (one file, one time
/// index) are packed into requests ([`pack`], #1274), each holding a
/// variable at most once and at most [`MAX_REQUEST_PLANES`] planes. A
/// step's windows are sorted by variable index (then stored level) before
/// packing, so the same query always makes the same requests: it joins
/// their in-flight reads and finds their replies cached. A query asking a
/// subset of the variables makes requests of its own.
fn requests_of(planned: Vec<(DateTime<Utc>, Step, Member)>) -> Vec<Job> {
    let mut jobs = Vec::new();
    // Each step's windows, the steps in the order first planned.
    let mut steps: Vec<(DateTime<Utc>, Step, Vec<Member>)> = Vec::new();
    let mut at: HashMap<DateTime<Utc>, usize> = HashMap::new();
    for (valid, step, member) in planned {
        if member.window.is_none() {
            jobs.push(Job {
                valid,
                step,
                members: vec![member],
            });
            continue;
        }
        let s = *at.entry(valid).or_insert_with(|| {
            steps.push((valid, step, Vec::new()));
            steps.len() - 1
        });
        steps[s].2.push(member);
    }
    for (valid, step, mut members) in steps {
        let first = |member: &Member| step.file.reads[member.index].span(&member.levels).first;
        members.sort_by_key(|member| (member.index, first(member)));
        let sizes: Vec<(usize, u64)> = members
            .iter()
            .map(|member| (member.index, member.planes(&step)))
            .collect();
        let mut members: Vec<Option<Member>> = members.into_iter().map(Some).collect();
        for request in pack(&sizes) {
            jobs.push(Job {
                valid,
                step: step.clone(),
                members: request
                    .into_iter()
                    .filter_map(|i| members[i].take())
                    .collect(),
            });
        }
    }
    jobs
}

/// `members`, each a `(variable index, planes)`, packed into requests in
/// their order, first fit: each joins the first request that does not hold
/// its variable yet and has room for its planes ([`MAX_REQUEST_PLANES`] in
/// all), else starts one. A variable of more planes is a request alone. A
/// request's positions into `members`, ascending.
fn pack(members: &[(usize, u64)]) -> Vec<Vec<usize>> {
    let mut requests: Vec<(u64, Vec<usize>)> = Vec::new();
    for (i, &(index, planes)) in members.iter().enumerate() {
        let room = requests.iter_mut().find(|(held, request)| {
            held.saturating_add(planes) <= MAX_REQUEST_PLANES
                && request.iter().all(|&j| members[j].0 != index)
        });
        match room {
            Some((held, request)) => {
                *held += planes;
                request.push(i);
            }
            None => requests.push((planes, vec![i])),
        }
    }
    requests.into_iter().map(|(_, request)| request).collect()
}

/// One query's field reads: the selected parameters at the selected steps
/// and levels of one run.
struct Reads<'a> {
    layout: &'a Arc<Layout>,
    /// The run the steps are of: a per-step amount's step before is this
    /// run's.
    run: &'a Run,
    steps: Vec<(DateTime<Utc>, &'a Step)>,
    /// Indices into the layout's pressure levels, in the order the
    /// response lists them; `[0]` without a vertical axis.
    levels: Vec<usize>,
    params: Vec<Param>,
}

impl<'a> Reads<'a> {
    /// The parameters `parameters` selects (every one without, the
    /// per-step amounts included) and the levels `z` selects
    /// ([`Layout::query_levels`]) at `steps` of `run`.
    fn new(
        layout: &'a Arc<Layout>,
        run: &'a Run,
        steps: Vec<(DateTime<Utc>, &'a Step)>,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
    ) -> Result<Self, DataServerError> {
        let names: Vec<&str> = layout.params().map(|p| layout.name(p)).collect();
        let params: Vec<Param> = select_parameters(parameters, &names)?
            .into_iter()
            .filter_map(|name| layout.param(name))
            .collect();
        if params.is_empty() {
            return Err(DataServerError::InvalidParameter(
                "No parameters available".into(),
            ));
        }
        let levels = layout.query_levels(z)?;
        Ok(Self {
            layout,
            run,
            steps,
            levels,
            params,
        })
    }

    /// The response's vertical coordinate: the selected levels, `None`
    /// without a vertical axis.
    fn vertical(&self) -> Option<VerticalCoord> {
        let vertical: &VerticalDimension = self.layout.vertical.as_ref()?;
        Some(VerticalCoord {
            kind: vertical.kind,
            values: self.levels.iter().map(|&l| vertical.levels[l]).collect(),
        })
    }
}

/// One field a query reads and where it samples it: a step (its valid time
/// and place in the run), a level (an index into the layout's pressure
/// levels, 0 without them) and the positions ([`on_grid`]). Position, area
/// and cube queries sample every field at the same positions; a trajectory
/// samples each at its own. A per-step amount also reads the run's step
/// before `valid`, at the same level and positions.
struct Sampling<'a> {
    valid: DateTime<Utc>,
    step: &'a Step,
    level: usize,
    positions: Positions,
}

impl Sampling<'_> {
    /// Whether the field is read: a field with no position on the grid
    /// samples nothing but nulls, so it is neither read nor counted.
    fn is_read(&self) -> bool {
        self.positions.iter().any(Option::is_some)
    }
}

/// Per parameter and field: the slot of its samples, and a per-step
/// amount's of the step before (`None` at the run's first step); `None`
/// for a field not read.
type Slots = Option<(usize, Option<usize>)>;

/// One read a query makes ([`field::read_uncached`]) at one step: one
/// member, a variable's field whole; or through OPeNDAP one request of the
/// windows of one or more variables, each over its own levels (#1274).
struct Job {
    valid: DateTime<Utc>,
    step: Step,
    /// By ascending variable index, each variable at most once: the order
    /// of the request's constraint, so the same members always make the
    /// same request.
    members: Vec<Member>,
}

/// One variable a [`Job`] reads: variable `index` at one served level,
/// whole or a window of it, or at several levels whose window is read in
/// one hyperslab over their stored levels; and the requests of each level,
/// each a positions list and the slot of its samples.
struct Member {
    index: usize,
    /// Served levels, distinct, in the order first asked; one unless the
    /// read is a window of several.
    levels: Vec<usize>,
    window: Option<CellWindow>,
    /// Per level of `levels`.
    asked: Vec<Vec<(usize, Positions)>>,
}

impl Member {
    /// The bytes the read of it at `step` decodes, from the metadata: a
    /// whole field its [`crate::ingest::FieldRead::decoded_bytes`] (the
    /// `f32` field, or its chunk when larger), a window its cells × the
    /// stored levels its hyperslab spans × the 4 bytes of an `f32` decoded.
    fn bytes(&self, step: &Step) -> u64 {
        let read = &step.file.reads[self.index];
        match self.window {
            None => read
                .decoded_bytes()
                .saturating_mul(self.levels.len() as u64),
            Some(window) => window
                .cells()
                .saturating_mul(self.planes(step))
                .saturating_mul(std::mem::size_of::<f32>() as u64),
        }
    }

    /// The planes the read of it at `step` holds: the stored levels its
    /// hyperslab spans.
    fn planes(&self, step: &Step) -> u64 {
        step.file.reads[self.index].span(&self.levels).levels()
    }
}

/// A query's reads, planned before any of them
/// ([`NetcdfEngine::plan_reads`]).
struct Plan {
    jobs: Vec<Job>,
    /// The sample slots the jobs fill.
    requests: usize,
    /// Per parameter, per field ([`Slots`]).
    slots: Vec<Vec<Slots>>,
}

impl Plan {
    /// What the caps count of the plan.
    fn tally(&self) -> Tally {
        let mut fields = 0;
        let mut steps = HashSet::new();
        let mut levels = HashSet::new();
        let mut pairs = HashSet::new();
        let mut variables = HashSet::new();
        let mut groups = HashSet::new();
        let mut members = 0;
        let mut bytes = 0u64;
        for job in &self.jobs {
            steps.insert(job.valid);
            for member in &job.members {
                members += 1;
                fields += member.levels.len();
                variables.insert(member.index);
                groups.insert((member.index, job.valid));
                for &level in &member.levels {
                    levels.insert(level);
                    pairs.insert((job.valid, level));
                }
                bytes = bytes.saturating_add(member.bytes(&job.step));
            }
        }
        Tally {
            fields,
            steps: steps.len(),
            levels: levels.len(),
            pairs: pairs.len(),
            variables: variables.len(),
            groups: groups.len(),
            members,
            reads: self.jobs.len(),
            bytes,
        }
    }
}

/// What a query's planned reads add up to, as the caps count them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Tally {
    /// (variable, step, level) fields read.
    fields: usize,
    /// Distinct timesteps, levels and (timestep, level) pairs read: the
    /// steps a per-step amount reads before its own included.
    steps: usize,
    levels: usize,
    pairs: usize,
    /// Source variables read: a derived wind speed's two components.
    variables: usize,
    /// Distinct (variable, timestep) read.
    groups: usize,
    /// Variables read, one per read of each ([`Member`]s): a window of
    /// several levels is one.
    members: usize,
    /// Reads ([`Job`]s): a window of several levels is one, and so is a
    /// request of the windows of several variables.
    reads: usize,
    /// Bytes decoded ([`Member::bytes`]).
    bytes: u64,
}

impl Tally {
    /// Both caps, before any read: `limits.bytes` against the bytes the
    /// reads decode, then `limits.reads` against the reads. Over either, a
    /// 400 `QueryTooLarge` naming what the query would read, the limit and
    /// how to narrow it; `time` names what to narrow for fewer steps.
    fn check(&self, limits: Limits, time: &str) -> Result<(), DataServerError> {
        // Every level of a variable's step in one read.
        let batched = self.members == self.groups;
        let narrow = |levels_help: bool| {
            if levels_help {
                ", name fewer parameters or fewer pressure levels (z)"
            } else {
                " or name fewer parameters"
            }
        };
        const DERIVED: &str = "A derived parameter reads each variable it is derived from: a wind \
                               speed reads both wind components, and a per-step amount its \
                               accumulation at the step before as well";
        if self.bytes > limits.bytes {
            return Err(DataServerError::QueryTooLarge(format!(
                "Query would decode {} fields ({}, {}); the limit is {} — narrow {time}{}. \
                 {DERIVED}",
                self.fields,
                shape(
                    self.fields,
                    self.steps,
                    self.levels,
                    self.pairs,
                    self.variables
                ),
                size(self.bytes),
                size(limits.bytes),
                narrow(self.levels > 1),
            )));
        }
        if self.reads > limits.reads {
            let shape = if batched {
                // Every level of a variable's step in one read.
                shape(self.reads, self.steps, 1, self.steps, self.variables)
            } else {
                shape(
                    self.reads,
                    self.steps,
                    self.levels,
                    self.pairs,
                    self.variables,
                )
            };
            // Where each step is one read, holding every variable and
            // level it reads (#1274), only fewer steps make fewer reads.
            // Otherwise fewer variables do too, and on pressure levels fewer
            // levels: read one by one each is a read, read together each is
            // a plane, and a request holds MAX_REQUEST_PLANES.
            let narrow = if self.reads > self.steps {
                narrow(self.levels > 1)
            } else {
                ""
            };
            return Err(DataServerError::QueryTooLarge(format!(
                "Query would make {} field reads ({shape}); the limit is {} reads — narrow \
                 {time}{narrow}. {DERIVED}",
                self.reads, limits.reads,
            )));
        }
        Ok(())
    }
}

/// How `n` fields or reads spread over `steps` timesteps and `levels`
/// levels (`pairs` distinct pairs of them) of `variables` source
/// variables: their product where they make `n`, else what they span. A
/// trajectory picks its (step, level) pairs per sample, so they are named
/// as pairs; a per-step amount reads steps before that the other variables
/// do not, so a query asking for both reads fewer than the product.
fn shape(n: usize, steps: usize, levels: usize, pairs: usize, variables: usize) -> String {
    let per = if pairs != steps.saturating_mul(levels) {
        format!("{pairs} timestep and level pairs")
    } else if levels > 1 {
        format!("{steps} timesteps × {levels} levels")
    } else {
        format!("{steps} timesteps")
    };
    let by = if n == pairs.saturating_mul(variables) {
        "×"
    } else {
        "of"
    };
    format!("{per} {by} {variables} source variables")
}

/// `bytes` in MiB, or in bytes below one: windows decode a few hundred.
fn size(bytes: u64) -> String {
    if bytes >= ds_cache::MIB {
        format!("{:.0} MiB", bytes as f64 / ds_cache::MIB as f64)
    } else {
        format!("{bytes} bytes")
    }
}

/// The layout every query reads through, or the 404 of a collection
/// before its first file.
fn layout(catalog: &Catalog) -> Result<&Arc<Layout>, DataServerError> {
    catalog
        .layout
        .as_ref()
        .ok_or_else(|| DataServerError::LocationNotFound("No NetCDF data available yet".into()))
}

impl NetcdfEngine {
    /// The run, steps, levels and variables a query selects. `all_steps`: a
    /// position series (and a 2-D or Z trajectory) reads every step of the
    /// run without a `datetime` window; an area (and a cube) reads the step
    /// a map without `TIME` draws, the run's last. Levels: `z`'s, or every
    /// level ([`Layout::query_levels`]). The caller checks the cap over
    /// what it reads of them.
    fn select<'a>(
        catalog: &'a Catalog,
        reference_time: Option<DateTime<Utc>>,
        window: Window,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        all_steps: bool,
    ) -> Result<Reads<'a>, DataServerError> {
        let layout = layout(catalog)?;
        let run: &Run = catalog.query_run(reference_time, window)?;
        let steps: Vec<(DateTime<Utc>, &Step)> = match window {
            Some((start, end)) => run.steps_within(start, end).collect(),
            None if all_steps => run.steps.iter().map(|(&t, step)| (t, step)).collect(),
            None => run
                .steps
                .iter()
                .next_back()
                .map(|(&t, step)| (t, step))
                .into_iter()
                .collect(),
        };
        if steps.is_empty() {
            // A pinned run without a step in the window: the 404 of an
            // unpinned one, which a datetime list skips (#968).
            return Err(match window {
                Some((start, end)) => crate::catalog::no_step_within(start, end),
                None => DataServerError::LocationNotFound("The forecast run has no steps".into()),
            });
        }
        Reads::new(layout, run, steps, parameters, z)
    }

    /// Read every selected field and sample it at `positions`:
    /// `[parameter][step × levels + level]` → one value per position
    /// ([`Self::sample_fields`] over every step × level, step-major), the
    /// reads checked against `limits` first.
    fn sample(
        &self,
        reads: &Reads,
        positions: &Positions,
        limits: Limits,
    ) -> Result<Vec<Vec<Vec<Option<f64>>>>, DataServerError> {
        let fields: Vec<Sampling> = reads
            .steps
            .iter()
            .flat_map(|&(valid, step)| {
                reads.levels.iter().map(move |&level| Sampling {
                    valid,
                    step,
                    level,
                    positions: positions.clone(),
                })
            })
            .collect();
        self.sample_fields(reads, &fields, limits, "the datetime window")
    }

    /// Each selected parameter at each of `fields`, sampled at the field's
    /// positions: `[parameter][field]` → one value per position. A variable
    /// is its field's samples; a per-step amount (#1165) the difference of
    /// its accumulation's samples at the field's step and at the run's step
    /// before, at the same level and positions ([`step_amount`]; a run's
    /// first step is its own accumulation).
    ///
    /// The reads are planned first ([`Self::plan_reads`]) and checked
    /// against both caps, `limits`, before any of them; `time` is how a
    /// refusal says to read fewer steps. Every (variable, step, level) field
    /// this needs is read once, however many parameters and fields sample it
    /// (an accumulation and its per-step amount, or the step before one step
    /// that is another step), and sampled once at each distinct list of
    /// positions asked of it. Fields are read through
    /// `ds_executor::field_jobs`, four reads at a time, each under the
    /// query's deadline; a failed read fails the query, never a null: a
    /// replaced file or the decode budget is a 503, so is a host that kept
    /// refusing the read (`Unavailable`, #1355), the deadline a 504, any
    /// other failure a 500. A field with no position on the grid is not
    /// read ([`Sampling::is_read`]), nor is its step before: its values are
    /// null, as the read would sample them, and a point far off a remote
    /// grid costs no fetch.
    fn sample_fields(
        &self,
        reads: &Reads,
        fields: &[Sampling],
        limits: Limits,
        time: &str,
    ) -> Result<Vec<Vec<Vec<Option<f64>>>>, DataServerError> {
        let plan = Self::plan_reads(reads, fields);
        plan.tally().check(limits, time)?;
        self.read_planned(reads, fields, plan)
    }

    /// The reads `fields` need, before any of them (#1171): one [`Job`] per
    /// read, and where each parameter's samples land.
    ///
    /// Each (variable, step, level) field is read once. Where the file
    /// reads windows (OPeNDAP), each field's window joins the cells of
    /// every positions list asked of it, each list's cells found once,
    /// and the levels asked of one variable at one step are one read: one
    /// hyperslab over the stored levels from the lowest asked to the
    /// highest, over the window joining theirs, while it is
    /// [`worth_a_window`] against their fields whole. Position, area and
    /// cube queries sample every level at the same positions, so their
    /// levels share one window; a trajectory's `Z` path samples each level
    /// at its own, and its levels join only while the joined window stays
    /// worth it. Otherwise each level is a read of its own, a window while
    /// worth it, else the field whole; a file that reads no windows (local,
    /// by HTTP range) reads every field whole, one level per read. Then the
    /// windows of one step, of every variable, are one request
    /// ([`requests_of`], #1274), up to [`MAX_REQUEST_PLANES`] planes each.
    fn plan_reads(reads: &Reads, fields: &[Sampling]) -> Plan {
        let layout = reads.layout;
        // The fields to read, each once: its step, variable and level, and
        // the requests of it, each a positions list and the slot of its
        // samples.
        type Read = (DateTime<Utc>, Step, usize, usize, Vec<(usize, Positions)>);
        let mut to_read: Vec<Read> = Vec::new();
        let mut keys: HashMap<(usize, DateTime<Utc>, usize), usize> = HashMap::new();
        let mut requests = 0usize;
        let mut request = |index: usize,
                           (valid, step): (DateTime<Utc>, &Step),
                           level: usize,
                           positions: &Positions| {
            let k = *keys.entry((index, valid, level)).or_insert_with(|| {
                to_read.push((valid, step.clone(), index, level, Vec::new()));
                to_read.len() - 1
            });
            let asked = &mut to_read[k].4;
            if let Some(&(slot, _)) = asked.iter().find(|(_, p)| Arc::ptr_eq(p, positions)) {
                return slot;
            }
            asked.push((requests, positions.clone()));
            requests += 1;
            requests - 1
        };
        let mut slots: Vec<Vec<Slots>> = Vec::new();
        for &param in &reads.params {
            let index = layout.source_of(param);
            let mut per_field = Vec::with_capacity(fields.len());
            for field in fields {
                if !field.is_read() {
                    per_field.push(None);
                    continue;
                }
                let now = request(
                    index,
                    (field.valid, field.step),
                    field.level,
                    &field.positions,
                );
                let before = match param {
                    Param::Stored(_) => None,
                    Param::PerStep(_) => reads
                        .run
                        .previous(field.valid)
                        .map(|before| request(index, before, field.level, &field.positions)),
                };
                per_field.push(Some((now, before)));
            }
            slots.push(per_field);
        }

        // The levels of each (variable, step), in the order first asked.
        type Group = (
            DateTime<Utc>,
            Step,
            usize,
            Vec<(usize, Vec<(usize, Positions)>)>,
        );
        let mut groups: Vec<Group> = Vec::new();
        let mut group_of: HashMap<(usize, DateTime<Utc>), usize> = HashMap::new();
        for (valid, step, index, level, asked) in to_read {
            let g = *group_of.entry((index, valid)).or_insert_with(|| {
                groups.push((valid, step, index, Vec::new()));
                groups.len() - 1
            });
            groups[g].3.push((level, asked));
        }
        // Each positions list's cells, keyed by the list's address, found
        // once however many fields sample it.
        let mut cells: HashMap<*const (), Option<CellWindow>> = HashMap::new();
        let mut jobs = Vec::new();
        for (valid, step, index, levels) in groups {
            let read = &step.file.reads[index];
            let windows: Vec<Option<CellWindow>> = if step.file.reader.reads_windows() {
                levels
                    .iter()
                    .map(|(_, asked)| {
                        asked
                            .iter()
                            .filter_map(|(_, positions)| {
                                *cells
                                    .entry(Arc::as_ptr(positions).cast())
                                    .or_insert_with(|| cells_read(&layout.source, positions))
                            })
                            .reduce(CellWindow::union)
                    })
                    .collect()
            } else {
                vec![None; levels.len()]
            };
            let served: Vec<usize> = levels.iter().map(|&(level, _)| level).collect();
            let joined = windows
                .iter()
                .copied()
                .collect::<Option<Vec<CellWindow>>>()
                .and_then(|windows| windows.into_iter().reduce(CellWindow::union))
                .filter(|&window| {
                    levels.len() > 1
                        && worth_a_window(
                            window,
                            read.span(&served).levels(),
                            levels.len(),
                            &layout.source,
                        )
                });
            if let Some(window) = joined {
                jobs.push((
                    valid,
                    step,
                    Member {
                        index,
                        levels: served,
                        window: Some(window),
                        asked: levels.into_iter().map(|(_, asked)| asked).collect(),
                    },
                ));
                continue;
            }
            for ((level, asked), window) in levels.into_iter().zip(windows) {
                jobs.push((
                    valid,
                    step.clone(),
                    Member {
                        index,
                        levels: vec![level],
                        window: window.filter(|&w| worth_a_window(w, 1, 1, &layout.source)),
                        asked: vec![asked],
                    },
                ));
            }
        }
        Plan {
            jobs: requests_of(jobs),
            requests,
            slots,
        }
    }

    /// Make `plan`'s reads, four at a time, and sample each parameter at
    /// each of `fields` from them ([`Self::sample_fields`]).
    fn read_planned(
        &self,
        reads: &Reads,
        fields: &[Sampling],
        plan: Plan,
    ) -> Result<Vec<Vec<Vec<Option<f64>>>>, DataServerError> {
        let layout = reads.layout;
        let Plan {
            jobs,
            requests,
            slots,
        } = plan;
        let mut samples: Vec<Vec<Option<f64>>> = vec![Vec::new(); requests];
        if !jobs.is_empty() {
            let flipped = layout.grid.flipped();
            let shared = layout.clone();
            let decodes: Arc<AtomicU64> = self.decodes.clone();
            ds_executor::field_jobs::run_field_jobs(
                ds_executor::query_runtime(),
                ds_executor::field_jobs::FIELD_CONCURRENCY,
                jobs.into_iter(),
                move |job: Job| {
                    let read = {
                        let members: Vec<field::Member> = job
                            .members
                            .iter()
                            .map(|member| field::Member {
                                index: member.index,
                                levels: &member.levels,
                                window: member.window,
                            })
                            .collect();
                        field::read_uncached(
                            &job.step.file,
                            job.step.index,
                            &members,
                            flipped,
                            &decodes,
                        )?
                    };
                    // Sampled from the stored field, then converted to the
                    // unit served, as the map converts each pixel (#1209).
                    let shared = &shared;
                    let sampled = read
                        .iter()
                        .zip(job.members)
                        .flat_map(|(fields, member)| {
                            let index = member.index;
                            fields
                                .iter()
                                .zip(member.asked)
                                .flat_map(move |(field, asked)| {
                                    asked.into_iter().map(move |(slot, positions)| {
                                        let values = positions
                                            .iter()
                                            .map(|position| {
                                                position
                                                    .and_then(|p| field.value_at(&shared.source, p))
                                                    .map(|raw| shared.to_display(index, raw))
                                            })
                                            .collect::<Vec<_>>();
                                        (slot, values)
                                    })
                                })
                        })
                        .collect::<Vec<_>>();
                    Ok(sampled)
                },
                |sampled| {
                    for (slot, values) in sampled {
                        samples[slot] = values;
                    }
                    Ok(())
                },
            )?;
        }

        // The per-step amounts first, from samples they may share with their
        // accumulation; then each variable takes its own samples.
        let mut out: Vec<Option<Vec<Vec<Option<f64>>>>> = reads
            .params
            .iter()
            .zip(&slots)
            .map(|(&param, per_field)| {
                let Param::PerStep(_) = param else {
                    return None;
                };
                let amounts = fields
                    .iter()
                    .zip(per_field)
                    .map(|(field, slot)| match slot {
                        None => vec![None; field.positions.len()],
                        Some((now, before)) => {
                            let now = &samples[*now];
                            let before = before.map(|before| &samples[before]);
                            (0..now.len())
                                .map(|p| step_amount(now[p], before.map(|b| b[p])))
                                .collect()
                        }
                    });
                Some(amounts.collect())
            })
            .collect();
        Ok(out
            .iter_mut()
            .zip(&slots)
            .map(|(amounts, per_field)| match amounts.take() {
                Some(amounts) => amounts,
                None => fields
                    .iter()
                    .zip(per_field)
                    .map(|(field, slot)| match slot {
                        None => vec![None; field.positions.len()],
                        Some((now, _)) => std::mem::take(&mut samples[*now]),
                    })
                    .collect(),
            })
            .collect())
    }

    /// Label, unit and CF standard name of each parameter a query returns,
    /// as the collection describes them: the unit served, after the
    /// display conversion ([`Layout::unit`]), as the map layers report it.
    /// A per-step amount (#1165) has its accumulation's unit and standard
    /// name (`precipitation_amount`: CF's name holds for any period) and
    /// its own title, which says the step.
    fn descriptions(
        catalog: &Catalog,
        layout: &Layout,
        params: &[Param],
    ) -> HashMap<String, ParameterDescription> {
        Self::described(catalog, layout, params)
            .into_iter()
            .collect()
    }

    /// [`Self::descriptions`] in `params` order, the order a trajectory's
    /// values are laid out in.
    fn described(
        catalog: &Catalog,
        layout: &Layout,
        params: &[Param],
    ) -> Vec<(String, ParameterDescription)> {
        params
            .iter()
            .map(|&param| {
                let index = layout.source_of(param);
                let name = layout.name(param);
                let label = catalog
                    .info
                    .parameters
                    .get(layout.info_index(param))
                    .map_or_else(|| name.to_string(), |p| p.title.clone());
                (
                    name.to_string(),
                    ParameterDescription {
                        label,
                        unit: layout.unit(index).to_string(),
                        observed_property: name.to_string(),
                        standard_name: layout.variables[index].standard_name.clone(),
                    },
                )
            })
            .collect()
    }

    /// Position series at every point of `points`, each field read once for
    /// all of them. One level (or none) is a `PointSeries` per point, with
    /// a one-value `z` axis on a pressure-level collection; several levels
    /// are a vertical profile per step, as engine-grib's level views answer.
    /// The reads are capped at `limits` (the configured ones from the
    /// trait).
    #[allow(clippy::too_many_arguments)] // the trait's selectors plus the caps
    pub(crate) fn positions(
        &self,
        points: &[(f64, f64)],
        window: Window,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
        limits: Limits,
        emit: &mut dyn FnMut(CoverageResponse) -> Result<(), DataServerError>,
    ) -> Result<(), DataServerError> {
        ds_core::deadline::check()?;
        if points.is_empty() {
            return Err(DataServerError::InvalidParameter(
                "No positions supplied".into(),
            ));
        }
        let catalog = self.catalog.load_full();
        let reads = Self::select(&catalog, reference_time, window, parameters, z, true)?;
        // The response's values, before any is read or allocated.
        check_area_budget(
            reads.steps.len() * reads.levels.len(),
            1,
            points.len(),
            reads.params.len(),
        )?;
        let source = &reads.layout.source;
        let positions: Positions = Arc::new(
            points
                .iter()
                .map(|&(lon, lat)| on_grid(source, lon, lat))
                .collect(),
        );
        let sampled = self.sample(&reads, &positions, limits)?;
        let times: Vec<DateTime<Utc>> = reads.steps.iter().map(|(t, _)| *t).collect();
        let descriptions = Self::descriptions(&catalog, reads.layout, &reads.params);
        let vertical = reads.vertical();
        let n_levels = reads.levels.len();
        // One parameter's values at point `p` in `slots` (of `sample`'s
        // step-major `[t][z]` order), as a range on `axis`.
        let range = |param: Param, slots: &[Vec<Option<f64>>], p: usize, axis: &str| {
            let values: Vec<Option<f64>> = slots.iter().map(|samples| samples[p]).collect();
            (
                reads.layout.name(param).to_string(),
                NdArray {
                    shape: vec![values.len()],
                    axis_names: vec![axis.to_string()],
                    values,
                    precision: field::VALUE_PRECISION,
                },
            )
        };
        for (p, &(lon, lat)) in points.iter().enumerate() {
            ds_core::deadline::check()?;
            let response = match &vertical {
                Some(z) if n_levels > 1 => CoverageResponse::Collection(
                    times
                        .iter()
                        .enumerate()
                        .map(|(s, &t)| QueryResult {
                            domain: DomainDescription::VerticalProfile {
                                x: lon,
                                y: lat,
                                t: Some(t),
                                z: z.clone(),
                            },
                            parameters: descriptions.clone(),
                            ranges: reads
                                .params
                                .iter()
                                .zip(&sampled)
                                .map(|(&param, slots)| {
                                    let step = &slots[s * n_levels..(s + 1) * n_levels];
                                    range(param, step, p, "z")
                                })
                                .collect(),
                        })
                        .collect(),
                ),
                _ => CoverageResponse::Single(QueryResult {
                    domain: DomainDescription::PointSeries {
                        x: lon,
                        y: lat,
                        t: times.clone(),
                        z: vertical.clone(),
                    },
                    parameters: descriptions.clone(),
                    ranges: reads
                        .params
                        .iter()
                        .zip(&sampled)
                        .map(|(&param, slots)| range(param, slots, p, "t"))
                        .collect(),
                }),
            };
            emit(response)?;
        }
        Ok(())
    }

    /// The area query, its reads capped at `limits` (the configured ones
    /// from the trait): see [`EdrEngine::query_area`].
    pub(crate) fn area(
        &self,
        coords: &str,
        datetime: Window,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
        limits: Limits,
    ) -> Result<CoverageResponse, DataServerError> {
        ds_core::deadline::check()?;
        let polygon = parse_area_coords(coords)?;
        let catalog = self.catalog.load_full();
        let reads = Self::select(&catalog, reference_time, datetime, parameters, z, false)?;
        let extent = catalog.info.spatial_extent.ok_or_else(|| {
            DataServerError::LocationNotFound("No NetCDF data available yet".into())
        })?;
        if !polygon.bbox.intersects_bbox(&extent) {
            return Err(DataServerError::LocationNotFound(
                "The polygon lies outside the collection's spatial extent".into(),
            ));
        }
        let (columns, rows) = reads.layout.source.size();
        let (lon_span, lat_span) = ds_core::geo::crs84_bbox_spans(extent);
        let axes = polygon.sample_grid(
            lon_span / f64::from(columns.max(1)),
            lat_span / f64::from(rows.max(1)),
            MAX_AREA_DIM,
        );
        let (nx, ny) = axes.dims();
        check_area_budget(
            reads.steps.len() * reads.levels.len(),
            ny,
            nx,
            reads.params.len(),
        )?;
        check_mask_budget(nx * ny, &polygon)?;
        let mask = polygon.cell_mask(&axes);
        let source = &reads.layout.source;
        let positions: Positions = Arc::new(
            axes.y
                .iter()
                .enumerate()
                .flat_map(|(iy, &y)| {
                    let (axes, mask) = (&axes, &mask);
                    axes.x.iter().enumerate().map(move |(ix, &x)| {
                        mask[axes.index(ix, iy)]
                            .then(|| on_grid(source, x, y))
                            .flatten()
                    })
                })
                .collect(),
        );
        let sampled = self.sample(&reads, &positions, limits)?;
        let time_axis = datetime.is_some_and(|(start, end)| start < end);
        let times: Vec<DateTime<Utc>> = reads.steps.iter().map(|(t, _)| *t).collect();
        let vertical = reads.vertical();
        let ranges = reads
            .params
            .iter()
            .zip(sampled)
            .map(|(&param, slots)| {
                // `sample`'s slots are step-major `[t][z]`, each `[y][x]`.
                let values: Vec<Option<f64>> = slots.into_iter().flatten().collect();
                let mut shape = vec![ny, nx];
                let mut axis_names = vec!["y", "x"];
                if vertical.is_some() {
                    shape.insert(0, reads.levels.len());
                    axis_names.insert(0, "z");
                }
                if time_axis {
                    shape.insert(0, times.len());
                    axis_names.insert(0, "t");
                }
                (
                    reads.layout.name(param).to_string(),
                    NdArray {
                        shape,
                        axis_names: axis_names.into_iter().map(String::from).collect(),
                        values,
                        precision: field::VALUE_PRECISION,
                    },
                )
            })
            .collect();
        Ok(CoverageResponse::Single(QueryResult {
            domain: DomainDescription::Grid {
                x: axes.x,
                y: axes.y,
                t: time_axis.then_some(times),
                z: vertical,
            },
            parameters: Self::descriptions(&catalog, reads.layout, &reads.params),
            ranges,
        }))
    }
}

impl EdrEngine for NetcdfEngine {
    fn get_locations(&self) -> Result<Arc<LocationSnapshot>, DataServerError> {
        Ok(LocationSnapshot::empty())
    }

    /// Each run is an instance, ascending (`ds_core::instances`); files
    /// without `forecast_reference_time` form one timeline and no instance.
    fn get_instances(&self) -> Vec<RunInfo> {
        let catalog = self.catalog.load();
        catalog
            .runs
            .iter()
            .filter_map(|(reference_time, run)| {
                Some(RunInfo {
                    reference_time: (*reference_time)?,
                    valid_times: run.steps.keys().copied().collect(),
                })
            })
            .collect()
    }

    /// O(1): a catalog holds runs or the run-less timeline, never both.
    fn has_instances(&self) -> bool {
        self.catalog
            .load()
            .runs
            .keys()
            .next_back()
            .is_some_and(Option::is_some)
    }

    fn instance_reference_times(&self) -> Vec<DateTime<Utc>> {
        self.catalog.load().runs.keys().flatten().copied().collect()
    }

    fn find_instance(&self, reference_time: DateTime<Utc>) -> Option<RunInfo> {
        let catalog = self.catalog.load();
        catalog.runs.get(&Some(reference_time)).map(|run| RunInfo {
            reference_time,
            valid_times: run.steps.keys().copied().collect(),
        })
    }

    fn query_location(
        &self,
        _location_id: &str,
        _datetime: Window,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Err(DataServerError::InvalidParameter(
            "NetCDF collections have no locations; use position or area instead".into(),
        ))
    }

    fn get_parameters(&self) -> Vec<String> {
        self.catalog
            .load()
            .info
            .parameters
            .iter()
            .map(|p| p.name.clone())
            .collect()
    }

    fn get_parameter_descriptions(&self) -> HashMap<String, ParameterDescription> {
        let catalog = self.catalog.load();
        let Some(layout) = catalog.layout.as_ref() else {
            return HashMap::new();
        };
        let all: Vec<Param> = layout.params().collect();
        Self::descriptions(&catalog, layout, &all)
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        let catalog = self.catalog.load();
        let times = &catalog.info.times;
        Some((*times.first()?, *times.last()?))
    }

    /// Every served valid time, over all runs: the map layers' axis.
    fn get_available_times(&self) -> Option<Vec<DateTime<Utc>>> {
        let times = self.catalog.load().info.times.clone();
        (!times.is_empty()).then_some(times)
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        self.catalog.load().info.spatial_extent
    }

    /// The pressure levels of a pressure-level collection (#1067 PR 8), in
    /// hPa, bottom first: the axis the map layers' `ELEVATION` advertises.
    fn get_vertical_extent(&self) -> Option<VerticalDimension> {
        self.catalog.load().info.vertical.clone()
    }

    /// Trajectory along a path, and cube over a bbox (#1169): on a
    /// single-level collection a cube is a `t`, `y`, `x` grid, as a station
    /// grid's is.
    fn supported_query_types(&self) -> Vec<String> {
        ["position", "area", "radius", "trajectory", "cube"]
            .map(String::from)
            .to_vec()
    }

    /// A time series at the point: every step of the run in the `datetime`
    /// window (the newest run's every step without one), bilinear in the
    /// grid's x/y like the map. A point off the grid is null, and a query
    /// with no point on it reads nothing. On pressure levels, at the `z`
    /// levels (every level without `z`): one level is a series with a
    /// one-value `z` axis, several a vertical profile per step.
    fn query_position(
        &self,
        coords: &str,
        datetime: Window,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        let (lat, lon) = parse_point_coords(coords)?;
        let mut response = None;
        self.positions(
            &[(lon, lat)],
            datetime,
            parameters,
            z,
            reference_time,
            Limits::configured(),
            &mut |r| {
                response = Some(r);
                Ok(())
            },
        )?;
        response.ok_or_else(|| DataServerError::Engine("position query returned nothing".into()))
    }

    /// Every point sampled from each field read once.
    fn query_positions(
        &self,
        points: &[String],
        datetime: Window,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
        emit: &mut dyn FnMut(CoverageResponse) -> Result<(), DataServerError>,
    ) -> Result<(), DataServerError> {
        let points: Vec<(f64, f64)> = points
            .iter()
            .map(|p| parse_point_coords(p).map(|(lat, lon)| (lon, lat)))
            .collect::<Result<_, _>>()?;
        self.positions(
            &points,
            datetime,
            parameters,
            z,
            reference_time,
            Limits::configured(),
            emit,
        )
    }

    /// A CRS84 `Grid` over the polygon's bbox at the source's mean cell
    /// size in degrees (≤ `MAX_AREA_DIM` cells per axis), each cell centre
    /// projected onto the source grid once and sampled as the map is;
    /// cells outside the polygon are null (#671). A polygon wholly outside
    /// the grid's extent is a 404. Without `datetime` the step a map without
    /// `TIME` draws (the newest run's last); a `datetime` interval puts every
    /// step inside it on a `t` axis, even one. On pressure levels the `z`
    /// levels (every level without `z`) are a `z` axis, even one, as
    /// engine-grib's level views answer: `[t, z, y, x]` or `[z, y, x]`.
    fn query_area(
        &self,
        coords: &str,
        datetime: Window,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        self.area(
            coords,
            datetime,
            parameters,
            z,
            reference_time,
            Limits::configured(),
        )
    }

    /// Values along a WKT `LINESTRING`, `Z`, `M` or `ZM` path (#1169):
    /// `ds_core::trajectory` plans the samples (about one per source cell
    /// crossed), their steps and levels; each sample is projected once and
    /// sampled as a position query samples a point, and each field the plan
    /// names is read once for all its samples. See [`trajectory`].
    fn query_trajectory(
        &self,
        coords: &str,
        datetime: Window,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        self.trajectory(
            coords,
            datetime,
            parameters,
            z,
            reference_time,
            Limits::configured(),
        )
    }

    /// A CRS84 `Grid` over `bbox` with a `t` axis and, on pressure levels,
    /// a `z` axis (#1169): the steps an area reads, `z`'s levels (every
    /// level without), and cells at the source's mean cell size or
    /// `resolution`'s counts, each sampled as a position query samples a
    /// point. See [`cube`].
    fn query_cube(
        &self,
        bbox: &Bbox,
        datetime: Window,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        resolution: CubeResolution,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        self.cube(
            bbox,
            datetime,
            parameters,
            z,
            resolution,
            reference_time,
            Limits::configured(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A MEPS field: 949 × 1069 `f32`, in one chunk.
    const MEPS_FIELD: u64 = 949 * 1069 * 4;

    /// The defaults of both caps.
    const DEFAULTS: Limits = Limits {
        bytes: DEFAULT_QUERY_MB * ds_cache::MIB,
        reads: DEFAULT_QUERY_FIELD_READS,
    };

    /// `bytes` as the byte cap, the read cap out of the way.
    fn bytes(bytes: u64) -> Limits {
        Limits {
            bytes,
            reads: usize::MAX,
        }
    }

    /// `reads` as the read cap, the byte cap out of the way.
    fn reads(reads: usize) -> Limits {
        Limits {
            bytes: u64::MAX,
            reads,
        }
    }

    /// What `steps` × `levels` fields of each of `variables` variables
    /// read whole, `field` bytes each, add up to: a read per field, as from
    /// a local file or by HTTP range.
    fn whole(steps: usize, levels: usize, variables: usize, field: u64) -> Tally {
        let pairs = steps * levels;
        Tally {
            fields: pairs * variables,
            steps,
            levels,
            pairs,
            variables,
            groups: steps * variables,
            members: pairs * variables,
            reads: pairs * variables,
            bytes: field * (pairs * variables) as u64,
        }
    }

    /// The same fields read through OPeNDAP as windows of `cells` cells,
    /// every level of a variable's step in one hyperslab, and the
    /// variables of a step in requests of at most [`MAX_REQUEST_PLANES`]
    /// planes ([`pack`]).
    fn windows(steps: usize, levels: usize, variables: usize, cells: u64) -> Tally {
        let step: Vec<(usize, u64)> = (0..variables).map(|v| (v, levels as u64)).collect();
        let per_step = pack(&step).len();
        Tally {
            members: steps * variables,
            reads: steps * per_step,
            bytes: cells * levels as u64 * 4 * (steps * variables) as u64,
            ..whole(steps, levels, variables, 0)
        }
    }

    /// The refusal's message, or a panic naming what came instead.
    fn too_large<T: std::fmt::Debug>(result: Result<T, DataServerError>) -> String {
        match result {
            Err(DataServerError::QueryTooLarge(message)) => message,
            other => panic!("expected QueryTooLarge, got {other:?}"),
        }
    }

    /// The default admits one parameter over one MEPS run read whole (67
    /// leads) as the client sees it, including a derived wind speed, which
    /// reads its two components (134 fields, 519 MiB), as the docs say.
    /// Three variables over a run are refused, and the refusal names the
    /// fields, the bytes and the limit.
    #[test]
    fn the_default_cap_admits_one_parameter_over_one_meps_run() {
        let ok = |tally: Tally| tally.check(DEFAULTS, "the datetime window");
        ok(whole(67, 1, 1, MEPS_FIELD)).unwrap();
        // A derived wind speed: x and y wind over the whole run.
        ok(whole(67, 1, 2, MEPS_FIELD)).unwrap();
        // Eight parameters at one step, or five over a day of leads.
        ok(whole(1, 1, 8, MEPS_FIELD)).unwrap();
        ok(whole(24, 1, 5, MEPS_FIELD)).unwrap();
        // 140 MEPS fields fit, 141 do not: the byte cap binds before the
        // read cap on whole fields.
        ok(whole(140, 1, 1, MEPS_FIELD)).unwrap();
        ok(whole(141, 1, 1, MEPS_FIELD)).unwrap_err();
        let message = too_large(ok(whole(67, 1, 3, MEPS_FIELD)));
        assert!(
            message.contains("201 fields")
                && message.contains("67 timesteps × 3 source variables")
                && message.contains("778 MiB")
                && message.contains("544 MiB")
                && message.contains("a wind speed reads both wind components")
                && message.contains("narrow the datetime window or name fewer parameters.")
                && !message.contains("level"),
            "no levels on one level: {message}"
        );
    }

    /// Each pressure level is a field of its own (#1067 PR 8): read whole,
    /// the byte cap counts steps × levels × variables. One MEPS step at all
    /// 13 levels of three variables fits, a whole run at one level fits, a
    /// whole run at every level does not, and the refusal names the levels.
    /// Read as windows through OPeNDAP the same run at every level counts
    /// what the windows decode, and fits both caps: 67 reads.
    #[test]
    fn the_cap_counts_pressure_levels() {
        let ok = |tally: Tally| tally.check(DEFAULTS, "the datetime window");
        ok(whole(1, 13, 3, MEPS_FIELD)).unwrap();
        ok(whole(67, 1, 1, MEPS_FIELD)).unwrap();
        ok(whole(10, 13, 1, MEPS_FIELD)).unwrap();
        ok(whole(11, 13, 1, MEPS_FIELD)).unwrap_err();
        let message = too_large(ok(whole(67, 13, 1, MEPS_FIELD)));
        assert!(
            message.contains("871 fields")
                && message.contains("67 timesteps × 13 levels × 1 source variables")
                && message.contains("name fewer parameters or fewer pressure levels (z)."),
            "{message}"
        );
        // A point's 2 × 2 cells: 67 reads of 13 levels, 13 936 bytes.
        let point = windows(67, 13, 1, 4);
        assert_eq!((point.fields, point.reads, point.bytes), (871, 67, 13_936));
        ok(point).unwrap();
        // A derived wind speed at every level over the run: 134 reads.
        ok(windows(67, 13, 2, 4)).unwrap();
    }

    /// The read cap (`MC_NETCDF_EDR_QUERY_READS`, default 200) counts each
    /// read once, a window of every level of a variable's step one: three
    /// variables at every level over a whole MEPS run are 201 reads (each
    /// 13-level variable a request of its own), one over the default, a 400
    /// naming the reads and the limit that asks for fewer parameters or
    /// fewer levels: at 8 levels the three share requests. Levels read one
    /// by one are reads each, and then fewer levels help too. Read whole,
    /// the byte cap refuses first.
    #[test]
    fn the_read_cap_counts_each_read_once() {
        let ok = |tally: Tally| tally.check(DEFAULTS, "the datetime window");
        ok(windows(100, 13, 2, 4)).unwrap();
        let message = too_large(ok(windows(67, 13, 3, 4)));
        assert_eq!(
            message,
            "Query would make 201 field reads (67 timesteps × 3 source variables); the limit \
             is 200 reads — narrow the datetime window, name fewer parameters or fewer \
             pressure levels (z). A derived parameter reads each variable it is derived from: \
             a wind speed reads both wind components, and a per-step amount its accumulation \
             at the step before as well"
        );
        ok(windows(67, 8, 3, 4)).unwrap();
        // Each level a read of its own (windows past what a hyperslab over
        // their levels is worth).
        let apart = Tally {
            members: 20 * 13,
            reads: 20 * 13,
            ..windows(20, 13, 1, 4)
        };
        let message = too_large(ok(apart));
        assert!(
            message.contains("260 field reads (20 timesteps × 13 levels × 1 source variables)")
                && message.contains("fewer pressure levels (z)"),
            "{message}"
        );
        // Fixture-sized fields read whole: 48 reads over a limit of 40,
        // one a step, so only fewer steps make fewer.
        let message = too_large(whole(48, 1, 1, 440).check(reads(40), "the path's time span"));
        assert!(
            message.contains("48 field reads (48 timesteps × 1 source variables)")
                && message.contains("the limit is 40 reads — narrow the path's time span. A"),
            "{message}"
        );
        // Over both: the bytes are named first.
        let message = too_large(ok(whole(67, 13, 3, MEPS_FIELD)));
        assert!(
            message.starts_with("Query would decode 2613 fields"),
            "{message}"
        );
        // What a refusal says of sizes under a MiB.
        assert_eq!(size(13_936), "13936 bytes");
        assert_eq!(size(DEFAULTS.bytes), "544 MiB");
    }

    /// Through OPeNDAP the windows of several variables at one step are one
    /// read (#1274): a six-variable surface meteogram over a 67-lead MEPS
    /// run is 67 reads, not 402, and fits the default. Sixteen single-level
    /// variables share a request; forty are three a step, 201 reads over a
    /// run, a 400 that asks for fewer parameters. A series of one request a
    /// step longer than the cap can only be narrowed in time.
    #[test]
    fn a_meteogram_is_one_read_per_step() {
        let ok = |tally: Tally| tally.check(DEFAULTS, "the datetime window");
        let meteogram = windows(67, 1, 6, 4);
        assert_eq!((meteogram.fields, meteogram.reads), (402, 67));
        ok(meteogram).unwrap();
        assert_eq!(windows(67, 1, 16, 4).reads, 67);
        assert_eq!(windows(67, 1, 17, 4).reads, 134);
        let message = too_large(ok(windows(67, 1, 40, 4)));
        assert!(
            message.contains("201 field reads (67 timesteps of 40 source variables)")
                && message.contains("narrow the datetime window or name fewer parameters.")
                && !message.contains("level"),
            "{message}"
        );
        let message = too_large(ok(windows(201, 1, 6, 4)));
        assert!(
            message.contains("201 field reads (201 timesteps of 6 source variables)")
                && message.contains("narrow the datetime window. A derived"),
            "{message}"
        );
    }

    /// A step's windows are packed into requests first fit, in order: each
    /// variable at most once a request, at most 16 planes. Single-level
    /// variables share requests of 16; a 13-level variable fills one, a
    /// single-level one taking the room left; one past 16 planes is a
    /// request alone; a variable's levels read one by one (a Z path's) go to
    /// requests of their own, each joined by another variable's level.
    #[test]
    fn requests_hold_each_variable_once_and_at_most_16_planes() {
        let singles: Vec<(usize, u64)> = (0..20).map(|v| (v, 1)).collect();
        let packed = pack(&singles);
        assert_eq!(packed.len(), 2);
        assert_eq!(packed[0], (0..16).collect::<Vec<_>>());
        assert_eq!(packed[1], (16..20).collect::<Vec<_>>());
        assert_eq!(
            pack(&[(0, 13), (1, 13), (2, 1), (3, 1), (4, 1), (5, 1)]),
            [vec![0, 2, 3, 4], vec![1, 5]]
        );
        assert_eq!(pack(&[(0, 20), (1, 1)]), [vec![0], vec![1]]);
        assert_eq!(
            pack(&[(0, 1), (0, 1), (0, 1), (1, 1), (1, 1)]),
            [vec![0, 3], vec![1, 4], vec![2]]
        );
        assert_eq!(pack(&[(0, 8), (1, 8)]), [vec![0, 1]]);
        assert_eq!(pack(&[(0, 9), (1, 8)]), [vec![0], vec![1]]);
        assert!(pack(&[]).is_empty());
    }

    /// The engine over `testdata/<dir>` with its family's file names and
    /// `variables`.
    fn fixture(dir: &str, family: &str, variables: &str) -> NetcdfEngine {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata")
            .join(dir);
        let config: ds_core::config::NetcdfConfig = toml::from_str(&format!(
            "data_path = {:?}\nfilename_template = \"meps_{family}_{{lead}}_%Y%m%dT{{run}}Z.nc\"\n\
             variables = {variables}",
            dir.display().to_string()
        ))
        .unwrap();
        NetcdfEngine::new(family, &config).unwrap()
    }

    /// A position query at the middle of `engine`'s grid, capped at
    /// `limits`.
    fn middle(
        engine: &NetcdfEngine,
        window: Window,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
        limits: Limits,
    ) -> Result<(), DataServerError> {
        let [w, s, e, n] = engine.catalog.load().info.spatial_extent.unwrap();
        let point = [((w + e) / 2.0, (s + n) / 2.0)];
        engine.positions(
            &point,
            window,
            parameters,
            z,
            reference_time,
            limits,
            &mut |_| Ok(()),
        )
    }

    /// A query over the fixture is refused at either cap from the files'
    /// metadata, before any read, with the field size a read decodes
    /// (11 × 10 `f32`, whole: a local file reads no windows), and admitted
    /// at it.
    #[test]
    fn a_query_is_checked_against_both_caps_from_the_metadata() {
        let engine = fixture(
            "meps-netcdf",
            "sfc",
            r#"["air_temperature_2m", "cloud_area_fraction"]"#,
        );
        let ask = |limits| middle(&engine, None, None, None, None, limits);
        let fields = 3 * 2 * FIXTURE_FIELD;
        let message = too_large(ask(bytes(fields - 1)));
        assert!(
            message.contains("6 fields (3 timesteps × 2 source variables, 2640 bytes)")
                && message.contains("the limit is 2639 bytes"),
            "{message}"
        );
        let message = too_large(ask(reads(5)));
        assert!(
            message.contains("6 field reads (3 timesteps × 2 source variables)"),
            "{message}"
        );
        assert_eq!(engine.field_decodes(), 0, "nothing read");
        ask(Limits {
            bytes: fields,
            reads: 6,
        })
        .unwrap();
        assert_eq!(engine.field_decodes(), 6);
    }

    /// A query over the pressure-level fixture reads every level without
    /// `z` and `z`'s levels with it, and the caps count them from the
    /// metadata: each level a field and, from a local file, a read.
    #[test]
    fn a_query_counts_the_levels_it_reads() {
        let engine = fixture(
            "meps-netcdf-pl",
            "pl",
            r#"["air_temperature_pl", "geopotential_pl"]"#,
        );
        let catalog = engine.catalog.load_full();
        let every = NetcdfEngine::select(&catalog, None, None, None, None, true).unwrap();
        assert_eq!(
            (every.steps.len(), every.levels.len(), every.params.len()),
            (2, 4, 2),
            "the newest run's two leads at all four levels"
        );
        let ask = |z: Option<&[f64]>, limits| middle(&engine, None, None, z, None, limits);
        let fields = 2 * 4 * 2 * FIXTURE_FIELD;
        ask(None, bytes(fields)).unwrap();
        let message = too_large(ask(None, bytes(fields - 1)));
        assert!(
            message.contains("16 fields (2 timesteps × 4 levels × 2 source variables")
                && message.contains("fewer pressure levels (z)"),
            "{message}"
        );
        let message = too_large(ask(None, reads(15)));
        assert!(
            message.contains("16 field reads (2 timesteps × 4 levels × 2 source variables)")
                && message.contains("fewer pressure levels (z)"),
            "each level a read of its own: {message}"
        );
        // `z` in request order, each once; a level the collection lacks is
        // dropped, and none of them is a 400.
        let z = [500.0, 1000.0, 500.0, 600.0];
        let some = NetcdfEngine::select(&catalog, None, None, None, Some(&z), true).unwrap();
        assert_eq!(some.levels, [2, 0]);
        assert_eq!(some.vertical().map(|z| z.values), Some(vec![500.0, 1000.0]));
        let decodes = engine.field_decodes();
        ask(Some(&z), bytes(2 * 2 * 2 * FIXTURE_FIELD)).unwrap();
        assert_eq!(engine.field_decodes() - decodes, 8);
        let none = NetcdfEngine::select(&catalog, None, None, None, Some(&[600.0]), true);
        assert!(
            matches!(&none, Err(DataServerError::InvalidParameter(m)) if m.contains("1000, 850, 500, 300 hPa")),
            "{:?}",
            none.err()
        );
    }

    /// A query reads a window of a field while it is at most half the
    /// field's cells (#1171), and the field whole past that. A window over
    /// several stored levels is worth it while it reads at most half of
    /// what the levels asked hold whole: levels between them count.
    #[test]
    fn a_window_is_read_up_to_half_the_field() {
        use ds_core::geo::{Crs, GeoTransform};
        let gt = GeoTransform::from_cell_centres(0.0, 1.0, 10.0, -1.0, 10, 11, Crs::Wgs84);
        let source = SourceGrid::new(gt.unwrap());
        let window = |columns: u32, rows: u32| CellWindow {
            columns: (0, columns - 1),
            rows: (0, rows - 1),
        };
        let one = |w| worth_a_window(w, 1, 1, &source);
        assert!(one(window(2, 2)));
        assert!(one(window(5, 11)), "55 of 110 cells");
        assert!(!one(window(8, 7)), "56 of 110");
        assert!(!one(window(10, 11)));
        // Four levels asked, four read: half of each.
        assert!(worth_a_window(window(5, 11), 4, 4, &source));
        assert!(!worth_a_window(window(8, 7), 4, 4, &source));
        // Two asked, the eleven between them read too.
        assert!(worth_a_window(window(2, 2), 13, 2, &source));
        assert!(!worth_a_window(window(5, 11), 5, 4, &source));
        assert!(!worth_a_window(window(4, 4), 13, 2, &source), "208 of 220");
        assert!(worth_a_window(window(4, 4), 13, 13, &source));
    }

    /// One fixture field: 11 × 10 `f32`.
    const FIXTURE_FIELD: u64 = 110 * 4;

    fn epoch(s: &str) -> i64 {
        DateTime::parse_from_rfc3339(s).unwrap().timestamp()
    }

    /// A trajectory's cap counts the fields its plan reads, each (step,
    /// level) once per variable, before any read (#1169): a 2-D path every
    /// step of the newest run, an M path only the steps its samples snap
    /// to, a ZM path its (step, level) pairs. Over the limit it is the 400
    /// a position query gets, and nothing is decoded.
    #[test]
    fn a_trajectory_is_capped_by_the_fields_its_plan_reads() {
        let engine = fixture(
            "meps-netcdf",
            "sfc",
            r#"["air_temperature_2m", "cloud_area_fraction"]"#,
        );
        let path = "LINESTRING(5 58, 30 66)";
        let fields = 3 * 2 * FIXTURE_FIELD;
        let message = too_large(engine.trajectory(path, None, None, None, None, bytes(fields - 1)));
        assert!(
            message.contains("6 fields (3 timesteps × 2 source variables")
                && message.contains("narrow the datetime window or name fewer parameters"),
            "{message}"
        );
        assert_eq!(engine.field_decodes(), 0, "refused before any read");
        let message = too_large(engine.trajectory(path, None, None, None, None, reads(5)));
        assert!(
            message.contains("6 field reads (3 timesteps × 2 source variables)"),
            "{message}"
        );
        assert_eq!(engine.field_decodes(), 0, "refused before any read");
        engine
            .trajectory(path, None, None, None, None, bytes(fields))
            .unwrap();
        assert_eq!(engine.field_decodes(), 6);

        // 09:00 to 10:00 snaps to two of the run's three steps.
        let m_path = format!(
            "LINESTRING M(5 58 {}, 30 66 {})",
            epoch("2026-10-15T09:00:00Z"),
            epoch("2026-10-15T10:00:00Z")
        );
        let fields = 2 * 2 * FIXTURE_FIELD;
        let message =
            too_large(engine.trajectory(&m_path, None, None, None, None, bytes(fields - 1)));
        assert!(
            message.contains("4 fields (2 timesteps × 2 source variables")
                && message.contains("narrow the path's time span"),
            "{message}"
        );
        engine
            .trajectory(&m_path, None, None, None, None, bytes(fields))
            .unwrap();

        let pl = fixture("meps-netcdf-pl", "pl", r#"["air_temperature_pl"]"#);
        let zm = format!(
            "LINESTRING ZM(5 58 1000 {}, 30 66 500 {})",
            epoch("2026-10-15T09:00:00Z"),
            epoch("2026-10-15T10:00:00Z")
        );
        let CoverageResponse::Single(result) = pl
            .trajectory(&zm, None, None, None, None, bytes(u64::MAX))
            .unwrap()
        else {
            panic!("a ZM path is one coverage")
        };
        let DomainDescription::Trajectory { nodes, node_z, .. } = &result.domain else {
            panic!("{:?}", result.domain)
        };
        let levels = &node_z.as_ref().unwrap().values;
        let pairs: HashSet<(DateTime<Utc>, u64)> = nodes
            .iter()
            .zip(levels)
            .map(|(node, level)| (node.0, level.to_bits()))
            .collect();
        let steps: HashSet<DateTime<Utc>> = nodes.iter().map(|node| node.0).collect();
        let distinct: HashSet<u64> = levels.iter().map(|l| l.to_bits()).collect();
        assert!(
            pairs.len() < steps.len() * distinct.len(),
            "fewer pairs than steps × levels: {pairs:?}"
        );
        assert_eq!(pl.field_decodes(), pairs.len() as u64, "each pair once");
        let fields = pairs.len() as u64 * FIXTURE_FIELD;
        pl.trajectory(&zm, None, None, None, None, bytes(fields))
            .unwrap();
        let message = too_large(pl.trajectory(&zm, None, None, None, None, bytes(fields - 1)));
        assert!(
            message.contains(&format!(
                "{} fields ({} timestep and level pairs × 1 source variables",
                pairs.len(),
                pairs.len()
            )),
            "{message}"
        );
    }

    /// A cube's cap counts steps × the levels it reads × variables: with
    /// `resolution-z`, only the levels some position takes. Over the limit
    /// it is a 400 before any read.
    #[test]
    fn a_cube_is_capped_by_the_levels_it_reads() {
        let pl = fixture(
            "meps-netcdf-pl",
            "pl",
            r#"["air_temperature_pl", "geopotential_pl"]"#,
        );
        let bbox = Bbox::new(10.0, 58.0, 25.0, 66.0).unwrap();
        // 300, 650 and 1000 hPa take 300, 500 and 1000: three of four levels.
        let resolution = CubeResolution {
            z: Some(3),
            ..CubeResolution::default()
        };
        let fields = 3 * 2 * FIXTURE_FIELD;
        let message =
            too_large(pl.cube(&bbox, None, None, None, resolution, None, bytes(fields - 1)));
        assert!(
            message.contains("6 fields (1 timesteps × 3 levels × 2 source variables")
                && message.contains("fewer pressure levels (z)"),
            "{message}"
        );
        let message = too_large(pl.cube(&bbox, None, None, None, resolution, None, reads(5)));
        assert!(
            message.contains("6 field reads (1 timesteps × 3 levels × 2 source variables)"),
            "{message}"
        );
        assert_eq!(pl.field_decodes(), 0, "refused before any read");
        pl.cube(&bbox, None, None, None, resolution, None, bytes(fields))
            .unwrap();
        assert_eq!(pl.field_decodes(), 6);
        // Every level read at native resolution: four.
        let all = CubeResolution::default();
        too_large(pl.cube(&bbox, None, None, None, all, None, bytes(fields)));
        pl.cube(
            &bbox,
            None,
            None,
            None,
            all,
            None,
            bytes(4 * 2 * FIXTURE_FIELD),
        )
        .unwrap();
    }

    /// A per-step amount (#1165) reads its accumulation at the step before
    /// each step too, and an accumulation asked for with its amount is one
    /// variable read: every query's caps count those fields, from the
    /// metadata, before any read. A position window of 08Z-09Z in the 06Z
    /// run reads 07Z as well; every step of the run reads none more; an M
    /// path and a cube over 08Z-09Z read 07Z too. Only the fields read
    /// count: with 2 m temperature, which reads no step before, a window
    /// is five fields of two variables over three steps.
    #[test]
    fn the_cap_counts_the_step_before_a_per_step_amount() {
        let engine = fixture(
            "meps-netcdf",
            "sfc",
            r#"["air_temperature_2m", "precipitation_amount_acc"]"#,
        );
        let run = DateTime::parse_from_rfc3339("2026-10-15T06:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let hours = |h: i64| run + chrono::Duration::hours(h);
        let window = Some((hours(2), hours(3)));
        let ask = |window: Window, parameters: &[String], limits| {
            middle(&engine, window, Some(parameters), None, Some(run), limits)
        };
        let both = ["precipitation_amount", "precipitation_amount_acc"].map(String::from);
        let message = too_large(ask(window, &both, bytes(3 * FIXTURE_FIELD - 1)));
        assert!(
            message.contains("3 fields (3 timesteps × 1 source variables")
                && message.contains("a per-step amount its accumulation at the step before"),
            "08Z and 09Z, and 07Z before them, of one variable: {message}"
        );
        let message = too_large(ask(None, &both, bytes(4 * FIXTURE_FIELD - 1)));
        assert!(
            message.contains("4 fields (4 timesteps × 1 source variables"),
            "every step of the run, none before: {message}"
        );
        // The accumulation alone reads no step before.
        let raw = ["precipitation_amount_acc".to_string()];
        let message = too_large(ask(window, &raw, reads(1)));
        assert!(
            message.contains("2 field reads (2 timesteps × 1 source variables)"),
            "{message}"
        );
        // 2 m temperature at 08Z and 09Z, the accumulation at 07Z too.
        let mixed = ["air_temperature_2m", "precipitation_amount"].map(String::from);
        let message = too_large(ask(window, &mixed, bytes(5 * FIXTURE_FIELD - 1)));
        assert!(
            message.contains("5 fields (3 timesteps of 2 source variables, 2200 bytes)"),
            "{message}"
        );
        assert_eq!(engine.field_decodes(), 0, "refused before any read");
        ask(window, &both, bytes(3 * FIXTURE_FIELD)).unwrap();
        assert_eq!(engine.field_decodes(), 3);
        ask(window, &mixed, bytes(5 * FIXTURE_FIELD)).unwrap();
        assert_eq!(engine.field_decodes(), 8);

        let amount = ["precipitation_amount".to_string()];
        let m_path = format!(
            "LINESTRING M(5 58 {}, 30 66 {})",
            epoch("2026-10-15T08:00:00Z"),
            epoch("2026-10-15T09:00:00Z")
        );
        let fields = 3 * FIXTURE_FIELD;
        let message = too_large(engine.trajectory(
            &m_path,
            None,
            Some(&amount),
            None,
            None,
            bytes(fields - 1),
        ));
        assert!(
            message.contains("3 fields (3 timesteps × 1 source variables"),
            "{message}"
        );
        let bbox = Bbox::new(5.0, 58.0, 25.0, 66.0).unwrap();
        let cube = |limits| {
            engine.cube(
                &bbox,
                window,
                Some(&amount),
                None,
                CubeResolution::default(),
                Some(run),
                limits,
            )
        };
        let message = too_large(cube(bytes(fields - 1)));
        assert!(
            message.contains("3 fields (3 timesteps × 1 source variables"),
            "{message}"
        );
        assert_eq!(engine.field_decodes(), 8, "refused before any read");
        engine
            .trajectory(&m_path, None, Some(&amount), None, None, bytes(fields))
            .unwrap();
        assert_eq!(engine.field_decodes(), 11, "07Z, 08Z and 09Z, once each");
        cube(bytes(fields)).unwrap();
        assert_eq!(engine.field_decodes(), 14);
    }
}
