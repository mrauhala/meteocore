//! OGC API - EDR position, area, radius, trajectory and cube queries
//! (#1067 PR 7, #1169), and the model runs as EDR instances.
//!
//! A query selects one run and its steps ([`Catalog::query_run`]) and, on a
//! pressure-level collection, its levels ([`Layout::query_levels`]: `z`, or
//! every level), checks what it will decode against [`QUERY_BYTES`] before
//! reading anything, then reads one field per (parameter, step, level)
//! through `ds_executor::field_jobs` (at most four at once, each under the
//! query's deadline) and samples it at every point the query asks for:
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
//! 4.06 MB.
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
/// (default 544 MiB): fields touched (steps × levels × parameters) × the
/// bytes one field read decodes ([`crate::ingest::FieldRead::decoded_bytes`]),
/// from the metadata, before any read. The default admits one parameter
/// over one MEPS run as the client sees it, including a derived wind speed.
/// The engine counts the variables it reads, and a derived wind speed
/// (`ds_core::wind::DerivedWind`) asks for both of its components: one
/// over a MEPS run is 134 fields of 4.06 MB (519 MiB), while three
/// variables over a run (201 fields) are refused. A cold remote MEPS
/// series is about 100 MB of compressed chunks per variable read: 3.4 MB/s
/// sustained to answer within the 30 s EDR deadline, twice that for a
/// derived wind speed.
pub(crate) static QUERY_BYTES: LazyLock<u64> = LazyLock::new(|| {
    ds_cache::env_mb("MC_NETCDF_EDR_QUERY_MB", DEFAULT_QUERY_MB).saturating_mul(ds_cache::MIB)
});

/// [`QUERY_BYTES`]' default, in MiB.
const DEFAULT_QUERY_MB: u64 = 544;

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

/// Whether a query reads `window` of a field on `source` rather than the
/// whole field (#1171): while it is at most half the field's cells. A
/// whole field's reply, kept in the chunk cache, serves every later query
/// and map of the field, a window's only the same window again; with no
/// telling whether the field will be wanted whole, reading a window and
/// then the field costs window + field, and the window alone saves only
/// while it is under half the field. Point queries are far under it (a
/// MEPS point is 2 × 2 of 949 × 1069 cells); an area or a cube passes it
/// over more than half the grid, and reads the field whole.
fn worth_a_window(window: CellWindow, source: &SourceGrid) -> bool {
    let (columns, rows) = source.size();
    window.cells().saturating_mul(2) <= u64::from(columns) * u64::from(rows)
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

    /// Whether a per-step amount is selected (#1165): each step it reads
    /// needs the run's step before too.
    fn reads_steps_before(&self) -> bool {
        self.params.iter().any(|p| matches!(p, Param::PerStep(_)))
    }

    /// The distinct steps the query reads: the selected ones and, for a
    /// per-step amount, the run's step before each (#1165), at most one
    /// more for a window of consecutive steps.
    fn steps_read(&self) -> usize {
        let mut steps: HashSet<DateTime<Utc>> = self.steps.iter().map(|&(t, _)| t).collect();
        if self.reads_steps_before() {
            for &(valid, _) in &self.steps {
                steps.extend(self.run.previous(valid).map(|(before, _)| before));
            }
        }
        steps.len()
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

/// The layout every query reads through, or the 404 of a collection
/// before its first file.
fn layout(catalog: &Catalog) -> Result<&Arc<Layout>, DataServerError> {
    catalog
        .layout
        .as_ref()
        .ok_or_else(|| DataServerError::LocationNotFound("No NetCDF data available yet".into()))
}

impl NetcdfEngine {
    /// [`Self::select`] with the query-shape cap checked over every step,
    /// level and variable selected: position and area read them all.
    fn plan<'a>(
        catalog: &'a Catalog,
        reference_time: Option<DateTime<Utc>>,
        window: Window,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        all_steps: bool,
    ) -> Result<Reads<'a>, DataServerError> {
        let reads = Self::select(catalog, reference_time, window, parameters, z, all_steps)?;
        reads.check_bytes(*QUERY_BYTES)?;
        Ok(reads)
    }

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

    /// Read every planned field and sample it at `positions`:
    /// `[parameter][step × levels + level]` → one value per position
    /// ([`Self::sample_fields`] over every step × level, step-major).
    fn sample(
        &self,
        reads: &Reads,
        positions: &Positions,
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
        self.sample_fields(reads, &fields)
    }

    /// Each selected parameter at each of `fields`, sampled at the field's
    /// positions: `[parameter][field]` → one value per position. A variable
    /// is its field's samples; a per-step amount (#1165) the difference of
    /// its accumulation's samples at the field's step and at the run's step
    /// before, at the same level and positions ([`step_amount`]; a run's
    /// first step is its own accumulation).
    ///
    /// Every (variable, step, level) field this needs is read once, however
    /// many parameters and fields sample it (an accumulation and its
    /// per-step amount, or the step before one step that is another step),
    /// and sampled once at each distinct list of positions asked of it.
    /// Through OPeNDAP the read is one window covering every position asked
    /// of the field (#1171, [`worth_a_window`]), each distinct positions
    /// list's cells found once.
    /// Fields are read through `ds_executor::field_jobs`, four at a time,
    /// each job under the query's deadline; a failed read fails the query,
    /// never a null: a replaced file or the decode budget is a 503, the
    /// deadline a 504, any other failure a 500. A field with no position on
    /// the grid is not read ([`Sampling::is_read`]), nor is its step
    /// before: its values are null, as the read would sample them, and a
    /// point far off a remote grid costs no fetch.
    fn sample_fields(
        &self,
        reads: &Reads,
        fields: &[Sampling],
    ) -> Result<Vec<Vec<Vec<Option<f64>>>>, DataServerError> {
        let layout = reads.layout;
        // The fields to read, each once: its step, variable and level, and
        // the requests of it, each a positions list and the slot of its
        // samples in `samples`.
        type Read = (Step, usize, usize, Vec<(usize, Positions)>);
        let mut to_read: Vec<Read> = Vec::new();
        let mut keys: HashMap<(usize, DateTime<Utc>, usize), usize> = HashMap::new();
        let mut requests = 0usize;
        let mut request = |index: usize,
                           (valid, step): (DateTime<Utc>, &Step),
                           level: usize,
                           positions: &Positions| {
            let k = *keys.entry((index, valid, level)).or_insert_with(|| {
                to_read.push((step.clone(), index, level, Vec::new()));
                to_read.len() - 1
            });
            let asked = &mut to_read[k].3;
            if let Some(&(slot, _)) = asked.iter().find(|(_, p)| Arc::ptr_eq(p, positions)) {
                return slot;
            }
            asked.push((requests, positions.clone()));
            requests += 1;
            requests - 1
        };
        // Per parameter and field: the slot of its samples, and a per-step
        // amount's of the step before (`None` at the run's first step);
        // `None` for a field not read.
        type Slots = Option<(usize, Option<usize>)>;
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

        let mut samples: Vec<Vec<Option<f64>>> = vec![Vec::new(); requests];
        if !to_read.is_empty() {
            let flipped = layout.grid.flipped();
            let shared = layout.clone();
            let decodes: Arc<AtomicU64> = self.decodes.clone();
            // Each field's window, where its file reads windows (#1171):
            // each positions list's cells, keyed by the list's address,
            // found once however many fields sample it.
            let mut cells: HashMap<*const (), Option<CellWindow>> = HashMap::new();
            let jobs: Vec<_> = to_read
                .into_iter()
                .map(|(step, index, level, asked)| {
                    let window = step
                        .file
                        .reader
                        .reads_windows()
                        .then(|| {
                            asked
                                .iter()
                                .filter_map(|(_, positions)| {
                                    *cells
                                        .entry(Arc::as_ptr(positions).cast())
                                        .or_insert_with(|| cells_read(&layout.source, positions))
                                })
                                .reduce(CellWindow::union)
                        })
                        .flatten()
                        .filter(|&window| worth_a_window(window, &layout.source));
                    (step, index, level, asked, window)
                })
                .collect();
            ds_executor::field_jobs::run_field_jobs(
                ds_executor::query_runtime(),
                jobs.into_iter(),
                move |(step, index, level, asked, window)| {
                    let field = field::read_uncached(
                        &step.file, index, step.index, level, flipped, window, &decodes,
                    )?;
                    // Sampled from the stored field, then converted to the
                    // unit served, as the map converts each pixel (#1209).
                    let sampled = asked
                        .into_iter()
                        .map(|(slot, positions)| {
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
    fn positions(
        &self,
        points: &[(f64, f64)],
        window: Window,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
        emit: &mut dyn FnMut(CoverageResponse) -> Result<(), DataServerError>,
    ) -> Result<(), DataServerError> {
        ds_core::deadline::check()?;
        if points.is_empty() {
            return Err(DataServerError::InvalidParameter(
                "No positions supplied".into(),
            ));
        }
        let catalog = self.catalog.load_full();
        let reads = Self::plan(&catalog, reference_time, window, parameters, z, true)?;
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
        let sampled = self.sample(&reads, &positions)?;
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
}

impl Reads<'_> {
    /// The query-shape cap: `limit` ([`QUERY_BYTES`]) against the bytes
    /// the planned fields decode, from the metadata: the steps read
    /// ([`Self::steps_read`], a per-step amount's steps before included) ×
    /// levels × each variable's field ([`Self::field_bytes`]).
    fn check_bytes(&self, limit: u64) -> Result<(), DataServerError> {
        check_query_bytes(
            self.steps_read(),
            self.levels.len(),
            &self.field_bytes(),
            limit,
        )
    }

    /// The query-shape cap over the fields a query samples, of each
    /// variable read: only those it reads count ([`Sampling::is_read`]),
    /// each (step, level) once, and for a per-step amount the run's step
    /// before each at the same level too (#1165). A trajectory reads the
    /// fields its plan names, which an `M` or `Z` path picks per sample.
    /// `time` is how the refusal says to read fewer steps.
    fn check_fields(
        &self,
        fields: &[Sampling],
        limit: u64,
        time: &str,
    ) -> Result<(), DataServerError> {
        let before = self.reads_steps_before();
        let mut pairs: HashSet<(DateTime<Utc>, usize)> = HashSet::new();
        for field in fields.iter().filter(|f| f.is_read()) {
            pairs.insert((field.valid, field.level));
            if before {
                if let Some((t, _)) = self.run.previous(field.valid) {
                    pairs.insert((t, field.level));
                }
            }
        }
        let steps: HashSet<DateTime<Utc>> = pairs.iter().map(|&(t, _)| t).collect();
        let levels: HashSet<usize> = pairs.iter().map(|&(_, l)| l).collect();
        check_field_reads(
            steps.len(),
            levels.len(),
            pairs.len(),
            &self.field_bytes(),
            limit,
            time,
        )
    }

    /// The bytes one field of each variable read decodes
    /// ([`crate::ingest::FieldRead::decoded_bytes`]), from the metadata: a
    /// per-step amount's accumulation, once however many parameters read
    /// it. Every file of the collection has the layout's fields on its
    /// grid, so the first step's file sizes them all.
    fn field_bytes(&self) -> Vec<u64> {
        let file = &self.steps[0].1.file;
        let mut variables: Vec<usize> = Vec::new();
        for &param in &self.params {
            let index = self.layout.source_of(param);
            if !variables.contains(&index) {
                variables.push(index);
            }
        }
        variables
            .iter()
            .map(|&index| file.reads[index].decoded_bytes())
            .collect()
    }
}

/// Whether `steps` × `levels` reads of fields decoding `field_bytes` each
/// (one entry per parameter) fit `limit`: a 400 `QueryTooLarge` naming the
/// fields, the bytes and the limit when not, before anything is read. Each
/// pressure level is a field of its own, so 13 MEPS levels count 13 times.
fn check_query_bytes(
    steps: usize,
    levels: usize,
    field_bytes: &[u64],
    limit: u64,
) -> Result<(), DataServerError> {
    check_field_reads(
        steps,
        levels,
        steps.saturating_mul(levels),
        field_bytes,
        limit,
        "the datetime window",
    )
}

/// Whether `reads` (step, level) fields, over `steps` timesteps and
/// `levels` levels, of fields decoding `field_bytes` each fit `limit`
/// ([`check_query_bytes`]). `reads` is `steps × levels` unless a
/// trajectory picks its fields per sample, which the refusal then names as
/// pairs; `time` names what to narrow for fewer steps.
fn check_field_reads(
    steps: usize,
    levels: usize,
    reads: usize,
    field_bytes: &[u64],
    limit: u64,
    time: &str,
) -> Result<(), DataServerError> {
    let bytes = field_bytes.iter().sum::<u64>().saturating_mul(reads as u64);
    if bytes <= limit {
        return Ok(());
    }
    let mib = |b: u64| b as f64 / ds_cache::MIB as f64;
    // The levels, and the advice to name fewer, only when the query reads
    // several: a single-level collection ignores `z` (clause A).
    let shape = match (reads == steps.saturating_mul(levels), levels > 1) {
        (true, true) => format!("{steps} timesteps × {levels} levels"),
        (true, false) => format!("{steps} timesteps"),
        (false, _) => format!("{reads} timestep and level pairs"),
    };
    let narrow = if levels > 1 {
        ", name fewer parameters or fewer pressure levels (z)"
    } else {
        " or name fewer parameters"
    };
    Err(DataServerError::QueryTooLarge(format!(
        "Query would decode {} fields ({shape} × {} source variables, {:.0} MiB); the limit is \
         {:.0} MiB — narrow {time}{narrow}. A derived parameter reads each variable it is \
         derived from: a wind speed reads both wind components, and a per-step amount its \
         accumulation at the step before as well",
        reads.saturating_mul(field_bytes.len()),
        field_bytes.len(),
        mib(bytes),
        mib(limit)
    )))
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
        self.positions(&points, datetime, parameters, z, reference_time, emit)
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
        ds_core::deadline::check()?;
        let polygon = parse_area_coords(coords)?;
        let catalog = self.catalog.load_full();
        let reads = Self::plan(&catalog, reference_time, datetime, parameters, z, false)?;
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
        let sampled = self.sample(&reads, &positions)?;
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
            *QUERY_BYTES,
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
            *QUERY_BYTES,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A MEPS field: 949 × 1069 `f32`, in one chunk.
    const MEPS_FIELD: u64 = 949 * 1069 * 4;

    /// The default admits one parameter over one MEPS run (67 leads) as the
    /// client sees it, including a derived wind speed, which reads its two
    /// components (134 fields, 519 MiB), as the docs say. Three variables
    /// over a run are refused, and the refusal names the fields, the bytes
    /// and the limit.
    #[test]
    fn the_default_cap_admits_one_parameter_over_one_meps_run() {
        let limit = DEFAULT_QUERY_MB * ds_cache::MIB;
        check_query_bytes(67, 1, &[MEPS_FIELD], limit).unwrap();
        // A derived wind speed: x and y wind over the whole run.
        check_query_bytes(67, 1, &[MEPS_FIELD; 2], limit).unwrap();
        // Eight parameters at one step, or five over a day of leads.
        check_query_bytes(1, 1, &[MEPS_FIELD; 8], limit).unwrap();
        check_query_bytes(24, 1, &[MEPS_FIELD; 5], limit).unwrap();
        // 140 MEPS fields fit, 141 do not.
        check_query_bytes(140, 1, &[MEPS_FIELD], limit).unwrap();
        check_query_bytes(141, 1, &[MEPS_FIELD], limit).unwrap_err();
        let refused = check_query_bytes(67, 1, &[MEPS_FIELD; 3], limit).unwrap_err();
        let DataServerError::QueryTooLarge(message) = &refused else {
            panic!("{refused:?}");
        };
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

    /// A plan over the fixture is refused at the cap from the files'
    /// metadata, with the field size a read decodes (11 × 10 `f32`), and
    /// admitted under it.
    #[test]
    fn a_plan_is_checked_against_the_cap_from_the_metadata() {
        let dir =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/meps-netcdf");
        let config: ds_core::config::NetcdfConfig = toml::from_str(&format!(
            "data_path = {:?}\nfilename_template = \"meps_sfc_{{lead}}_%Y%m%dT{{run}}Z.nc\"\n\
             variables = [\"air_temperature_2m\", \"cloud_area_fraction\"]",
            dir.display().to_string()
        ))
        .unwrap();
        let engine = NetcdfEngine::new("cap", &config).unwrap();
        let catalog = engine.catalog.load_full();
        let reads = NetcdfEngine::plan(&catalog, None, None, None, None, true).unwrap();
        assert_eq!((reads.steps.len(), reads.params.len()), (3, 2));
        let bytes = 3 * 2 * 110 * 4;
        reads.check_bytes(bytes).unwrap();
        assert!(matches!(
            reads.check_bytes(bytes - 1),
            Err(DataServerError::QueryTooLarge(_))
        ));
        assert_eq!(engine.field_decodes(), 0, "nothing read");
    }

    /// Each pressure level is a field of its own (#1067 PR 8): the cap
    /// counts steps × levels × variables. One MEPS step at all 13 levels
    /// of three variables fits, a whole run at one level fits, a whole run
    /// at every level does not, and the refusal names the levels.
    #[test]
    fn the_cap_counts_pressure_levels() {
        let limit = DEFAULT_QUERY_MB * ds_cache::MIB;
        check_query_bytes(1, 13, &[MEPS_FIELD; 3], limit).unwrap();
        check_query_bytes(67, 1, &[MEPS_FIELD], limit).unwrap();
        check_query_bytes(10, 13, &[MEPS_FIELD], limit).unwrap();
        check_query_bytes(11, 13, &[MEPS_FIELD], limit).unwrap_err();
        let refused = check_query_bytes(67, 13, &[MEPS_FIELD], limit).unwrap_err();
        let DataServerError::QueryTooLarge(message) = &refused else {
            panic!("{refused:?}");
        };
        assert!(
            message.contains("871 fields")
                && message.contains("67 timesteps × 13 levels × 1 source variables")
                && message.contains("name fewer parameters or fewer pressure levels (z)."),
            "{message}"
        );
    }

    /// A plan over the pressure-level fixture reads every level without `z`
    /// and `z`'s levels with it, and the cap counts them from the metadata.
    #[test]
    fn a_plan_counts_the_levels_it_reads() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/meps-netcdf-pl");
        let config: ds_core::config::NetcdfConfig = toml::from_str(&format!(
            "data_path = {:?}\nfilename_template = \"meps_pl_{{lead}}_%Y%m%dT{{run}}Z.nc\"\n\
             variables = [\"air_temperature_pl\", \"geopotential_pl\"]",
            dir.display().to_string()
        ))
        .unwrap();
        let engine = NetcdfEngine::new("cap-pl", &config).unwrap();
        let catalog = engine.catalog.load_full();
        let every = NetcdfEngine::plan(&catalog, None, None, None, None, true).unwrap();
        assert_eq!(
            (every.steps.len(), every.levels.len(), every.params.len()),
            (2, 4, 2),
            "the newest run's two leads at all four levels"
        );
        let bytes = 2 * 4 * 2 * 110 * 4;
        every.check_bytes(bytes).unwrap();
        assert!(matches!(
            every.check_bytes(bytes - 1),
            Err(DataServerError::QueryTooLarge(_))
        ));
        // `z` in request order, each once; a level the collection lacks is
        // dropped, and none of them is a 400.
        let some = NetcdfEngine::plan(
            &catalog,
            None,
            None,
            None,
            Some(&[500.0, 1000.0, 500.0, 600.0]),
            true,
        )
        .unwrap();
        assert_eq!(some.levels, [2, 0]);
        assert_eq!(some.vertical().map(|z| z.values), Some(vec![500.0, 1000.0]));
        some.check_bytes(2 * 2 * 2 * 110 * 4).unwrap();
        let none = NetcdfEngine::plan(&catalog, None, None, None, Some(&[600.0]), true);
        assert!(
            matches!(&none, Err(DataServerError::InvalidParameter(m)) if m.contains("1000, 850, 500, 300 hPa")),
            "{:?}",
            none.err()
        );
        assert_eq!(engine.field_decodes(), 0, "nothing read");
    }

    /// A query reads a window of a field while it is at most half the
    /// field's cells (#1171), and the field whole past that.
    #[test]
    fn a_window_is_read_up_to_half_the_field() {
        use ds_core::geo::{Crs, GeoTransform};
        let gt = GeoTransform::from_cell_centres(0.0, 1.0, 10.0, -1.0, 10, 11, Crs::Wgs84);
        let source = SourceGrid::new(gt.unwrap());
        let window = |columns: u32, rows: u32| CellWindow {
            columns: (0, columns - 1),
            rows: (0, rows - 1),
        };
        assert!(worth_a_window(window(2, 2), &source));
        assert!(worth_a_window(window(5, 11), &source), "55 of 110 cells");
        assert!(!worth_a_window(window(8, 7), &source), "56 of 110");
        assert!(!worth_a_window(window(10, 11), &source));
    }

    /// One fixture field: 11 × 10 `f32`.
    const FIXTURE_FIELD: u64 = 110 * 4;

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

    fn epoch(s: &str) -> i64 {
        DateTime::parse_from_rfc3339(s).unwrap().timestamp()
    }

    /// The refusal's message, or a panic naming what came instead.
    fn too_large<T: std::fmt::Debug>(result: Result<T, DataServerError>) -> String {
        match result {
            Err(DataServerError::QueryTooLarge(message)) => message,
            other => panic!("expected QueryTooLarge, got {other:?}"),
        }
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
        let bytes = 3 * 2 * FIXTURE_FIELD;
        let message = too_large(engine.trajectory(path, None, None, None, None, bytes - 1));
        assert!(
            message.contains("6 fields (3 timesteps × 2 source variables")
                && message.contains("narrow the datetime window or name fewer parameters"),
            "{message}"
        );
        assert_eq!(engine.field_decodes(), 0, "refused before any read");
        engine
            .trajectory(path, None, None, None, None, bytes)
            .unwrap();
        assert_eq!(engine.field_decodes(), 6);

        // 09:00 to 10:00 snaps to two of the run's three steps.
        let m_path = format!(
            "LINESTRING M(5 58 {}, 30 66 {})",
            epoch("2026-10-15T09:00:00Z"),
            epoch("2026-10-15T10:00:00Z")
        );
        let bytes = 2 * 2 * FIXTURE_FIELD;
        let message = too_large(engine.trajectory(&m_path, None, None, None, None, bytes - 1));
        assert!(
            message.contains("4 fields (2 timesteps × 2 source variables")
                && message.contains("narrow the path's time span"),
            "{message}"
        );
        engine
            .trajectory(&m_path, None, None, None, None, bytes)
            .unwrap();

        let pl = fixture("meps-netcdf-pl", "pl", r#"["air_temperature_pl"]"#);
        let zm = format!(
            "LINESTRING ZM(5 58 1000 {}, 30 66 500 {})",
            epoch("2026-10-15T09:00:00Z"),
            epoch("2026-10-15T10:00:00Z")
        );
        let CoverageResponse::Single(result) = pl
            .trajectory(&zm, None, None, None, None, u64::MAX)
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
        let bytes = pairs.len() as u64 * FIXTURE_FIELD;
        pl.trajectory(&zm, None, None, None, None, bytes).unwrap();
        let message = too_large(pl.trajectory(&zm, None, None, None, None, bytes - 1));
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
        let bytes = 3 * 2 * FIXTURE_FIELD;
        let message = too_large(pl.cube(&bbox, None, None, None, resolution, None, bytes - 1));
        assert!(
            message.contains("6 fields (1 timesteps × 3 levels × 2 source variables")
                && message.contains("fewer pressure levels (z)"),
            "{message}"
        );
        assert_eq!(pl.field_decodes(), 0, "refused before any read");
        pl.cube(&bbox, None, None, None, resolution, None, bytes)
            .unwrap();
        assert_eq!(pl.field_decodes(), 6);
        // Every level read at native resolution: four.
        let all = CubeResolution::default();
        too_large(pl.cube(&bbox, None, None, None, all, None, bytes));
        pl.cube(&bbox, None, None, None, all, None, 4 * 2 * FIXTURE_FIELD)
            .unwrap();
    }

    /// A per-step amount (#1165) reads its accumulation at the step before
    /// each step too, and an accumulation asked for with its amount is one
    /// variable read: every query's cap counts those fields, from the
    /// metadata, before any read. A position window of 08Z-09Z in the 06Z
    /// run reads 07Z as well; every step of the run reads none more; an M
    /// path and a cube over 08Z-09Z read 07Z too.
    #[test]
    fn the_cap_counts_the_step_before_a_per_step_amount() {
        let engine = fixture(
            "meps-netcdf",
            "sfc",
            r#"["air_temperature_2m", "precipitation_amount_acc"]"#,
        );
        let catalog = engine.catalog.load_full();
        let run = DateTime::parse_from_rfc3339("2026-10-15T06:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let hours = |h: i64| run + chrono::Duration::hours(h);
        let both = ["precipitation_amount", "precipitation_amount_acc"].map(String::from);
        let window = NetcdfEngine::plan(
            &catalog,
            Some(run),
            Some((hours(2), hours(3))),
            Some(&both),
            None,
            true,
        )
        .unwrap();
        assert_eq!(
            (
                window.steps.len(),
                window.steps_read(),
                window.field_bytes().len()
            ),
            (2, 3, 1),
            "08Z and 09Z, and 07Z before them, of one variable"
        );
        window.check_bytes(3 * FIXTURE_FIELD).unwrap();
        let message = too_large(window.check_bytes(3 * FIXTURE_FIELD - 1));
        assert!(
            message.contains("3 fields (3 timesteps × 1 source variables")
                && message.contains("a per-step amount its accumulation at the step before"),
            "{message}"
        );
        let every = NetcdfEngine::plan(&catalog, Some(run), None, Some(&both), None, true).unwrap();
        assert_eq!((every.steps.len(), every.steps_read()), (4, 4));
        // The accumulation alone reads no step before.
        let raw = ["precipitation_amount_acc".to_string()];
        let alone = NetcdfEngine::plan(
            &catalog,
            Some(run),
            Some((hours(2), hours(3))),
            Some(&raw),
            None,
            true,
        )
        .unwrap();
        assert_eq!(alone.steps_read(), 2);

        let amount = ["precipitation_amount".to_string()];
        let m_path = format!(
            "LINESTRING M(5 58 {}, 30 66 {})",
            epoch("2026-10-15T08:00:00Z"),
            epoch("2026-10-15T09:00:00Z")
        );
        let bytes = 3 * FIXTURE_FIELD;
        let message =
            too_large(engine.trajectory(&m_path, None, Some(&amount), None, None, bytes - 1));
        assert!(
            message.contains("3 fields (3 timesteps × 1 source variables"),
            "{message}"
        );
        let bbox = Bbox::new(5.0, 58.0, 25.0, 66.0).unwrap();
        let cube = |limit| {
            engine.cube(
                &bbox,
                Some((hours(2), hours(3))),
                Some(&amount),
                None,
                CubeResolution::default(),
                Some(run),
                limit,
            )
        };
        let message = too_large(cube(bytes - 1));
        assert!(
            message.contains("3 fields (3 timesteps × 1 source variables"),
            "{message}"
        );
        assert_eq!(engine.field_decodes(), 0, "refused before any read");
        engine
            .trajectory(&m_path, None, Some(&amount), None, None, bytes)
            .unwrap();
        assert_eq!(engine.field_decodes(), 3, "07Z, 08Z and 09Z, once each");
        cube(bytes).unwrap();
        assert_eq!(engine.field_decodes(), 6);
    }
}
