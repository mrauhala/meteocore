//! OGC API - EDR position, area and radius queries (#1067 PR 7), and the
//! model runs as EDR instances.
//!
//! A query selects one run and its steps ([`Catalog::query_run`]) and, on a
//! pressure-level collection, its levels ([`Layout::query_levels`]: `z`, or
//! every level), checks what it will decode against [`QUERY_BYTES`] before
//! reading anything, then reads one field per (parameter, step, level)
//! through `ds_executor::field_jobs` (at most four at once, each under the
//! query's deadline) and samples it at every point the query asks for:
//! each point, or each cell centre of an area's CRS84 grid, is projected
//! onto the source grid once ([`SourceGrid::position`]) and sampled as a
//! map draws it ([`SourceGrid::value_at`]) and converted to the unit served
//! as the map converts it ([`Layout::to_display`], #1209), so
//! GetFeatureInfo answers what GetMap drew. Field reads go through
//! [`field::read_uncached`]: a decoded field the render cache holds is used,
//! any other is decoded for the query alone.

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, LazyLock};

use chrono::{DateTime, Utc};
use ds_core::edr_engine::{select_parameters, EdrEngine};
use ds_core::error::DataServerError;
use ds_core::feature::{
    check_area_budget, check_mask_budget, parse_area_coords, parse_point_coords, MAX_AREA_DIM,
};
use ds_core::instances::RunInfo;
use ds_core::model::{
    CoverageResponse, DomainDescription, LocationSnapshot, NdArray, ParameterDescription,
    QueryResult, VerticalCoord,
};
use ds_core::resample::SourceGrid;
use ds_core::vertical::VerticalDimension;

use crate::catalog::{Catalog, Layout, Run, Step};
use crate::field;
use crate::NetcdfEngine;

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

/// One query's field reads: the selected variables (layout indices) at the
/// selected steps and levels.
struct Reads<'a> {
    layout: &'a Arc<Layout>,
    steps: Vec<(DateTime<Utc>, &'a Step)>,
    /// Indices into the layout's pressure levels, in the order the
    /// response lists them; `[0]` without a vertical axis.
    levels: Vec<usize>,
    variables: Vec<usize>,
}

impl Reads<'_> {
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

impl NetcdfEngine {
    /// The run, steps, levels and variables a query reads, the query-shape
    /// cap checked. `all_steps`: a position series reads every step of the
    /// run without a `datetime` window; an area reads the step a map without
    /// `TIME` draws, the run's last. Levels: `z`'s, or every level
    /// ([`Layout::query_levels`]).
    fn plan<'a>(
        catalog: &'a Catalog,
        reference_time: Option<DateTime<Utc>>,
        window: Window,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        all_steps: bool,
    ) -> Result<Reads<'a>, DataServerError> {
        let layout = catalog.layout.as_ref().ok_or_else(|| {
            DataServerError::LocationNotFound("No NetCDF data available yet".into())
        })?;
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
        let names: Vec<&str> = layout.variables.iter().map(|v| &*v.name).collect();
        let variables: Vec<usize> = select_parameters(parameters, &names)?
            .into_iter()
            .filter_map(|name| names.iter().position(|n| *n == name))
            .collect();
        if variables.is_empty() {
            return Err(DataServerError::InvalidParameter(
                "No parameters available".into(),
            ));
        }
        let levels = layout.query_levels(z)?;
        let reads = Reads {
            layout,
            steps,
            levels,
            variables,
        };
        reads.check_bytes(*QUERY_BYTES)?;
        Ok(reads)
    }

    /// Read every planned field and sample it at `positions`:
    /// `[variable][step × levels + level]` → one value per position. Fields
    /// are read through `ds_executor::field_jobs`, four at a time, each job
    /// under the query's deadline; a failed read fails the query, never a
    /// null: a replaced file or the decode budget is a 503, the deadline a
    /// 504, any other failure a 500. With no position on the grid nothing is
    /// read: every value is null, as the reads would sample it, and a point
    /// far off a remote grid costs no fetch.
    fn sample(
        &self,
        reads: &Reads,
        positions: &Positions,
    ) -> Result<Vec<Vec<Vec<Option<f64>>>>, DataServerError> {
        let (n_vars, n_levels) = (reads.variables.len(), reads.levels.len());
        let n_fields = reads.steps.len() * n_levels;
        if positions.iter().all(Option::is_none) {
            return Ok(vec![vec![vec![None; positions.len()]; n_fields]; n_vars]);
        }
        let mut out: Vec<Vec<Vec<Option<f64>>>> = vec![vec![Vec::new(); n_fields]; n_vars];
        let flipped = reads.layout.grid.flipped;
        // One job per (variable, step, level): `f` is the step's and level's
        // slot, step-major, so a variable's slots are `[t][z]`.
        let jobs: Vec<(usize, usize, Step, usize, usize)> = reads
            .variables
            .iter()
            .enumerate()
            .flat_map(|(v, &index)| {
                reads
                    .steps
                    .iter()
                    .enumerate()
                    .flat_map(move |(s, (_, step))| {
                        reads.levels.iter().enumerate().map(move |(l, &level)| {
                            (v, s * n_levels + l, (*step).clone(), index, level)
                        })
                    })
            })
            .collect();
        let layout = reads.layout.clone();
        let positions = positions.clone();
        let decodes: Arc<AtomicU64> = self.decodes.clone();
        ds_executor::field_jobs::run_field_jobs(
            ds_executor::query_runtime(),
            jobs.into_iter(),
            move |(v, f, step, index, level)| {
                let values =
                    field::read_uncached(&step.file, index, step.index, level, flipped, &decodes)?;
                // Sampled from the stored field, then converted to the
                // unit served, as the map converts each pixel (#1209).
                let samples = positions
                    .iter()
                    .map(|position| {
                        position
                            .and_then(|p| layout.source.value_at(&values, None, p))
                            .map(|raw| layout.to_display(index, raw))
                    })
                    .collect::<Vec<_>>();
                Ok((v, f, samples))
            },
            |(v, f, samples)| {
                out[v][f] = samples;
                Ok(())
            },
        )?;
        Ok(out)
    }

    /// Label, unit and CF standard name of each variable a query returns,
    /// as the collection describes them: the unit served, after the
    /// display conversion ([`Layout::unit`]), as the map layers report it.
    fn descriptions(
        catalog: &Catalog,
        layout: &Layout,
        variables: &[usize],
    ) -> HashMap<String, ParameterDescription> {
        variables
            .iter()
            .map(|&index| {
                let variable = &layout.variables[index];
                let info = catalog.info.parameters.get(index);
                (
                    variable.name.to_string(),
                    ParameterDescription {
                        label: info.map_or_else(|| variable.title.clone(), |p| p.title.clone()),
                        unit: layout.unit(index).to_string(),
                        observed_property: variable.name.to_string(),
                        standard_name: variable.standard_name.clone(),
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
            reads.variables.len(),
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
        let descriptions = Self::descriptions(&catalog, reads.layout, &reads.variables);
        let vertical = reads.vertical();
        let n_levels = reads.levels.len();
        // One variable's values at point `p` in `slots` (of `sample`'s
        // step-major `[t][z]` order), as a range on `axis`.
        let range = |index: usize, slots: &[Vec<Option<f64>>], p: usize, axis: &str| {
            let values: Vec<Option<f64>> = slots.iter().map(|samples| samples[p]).collect();
            (
                reads.layout.variables[index].name.to_string(),
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
                                .variables
                                .iter()
                                .zip(&sampled)
                                .map(|(&index, slots)| {
                                    let step = &slots[s * n_levels..(s + 1) * n_levels];
                                    range(index, step, p, "z")
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
                        .variables
                        .iter()
                        .zip(&sampled)
                        .map(|(&index, slots)| range(index, slots, p, "t"))
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
    /// the planned fields decode, from the metadata: steps × levels × each
    /// variable's field. Every file of the collection has the layout's
    /// fields on its grid, so the first step's file sizes them all.
    fn check_bytes(&self, limit: u64) -> Result<(), DataServerError> {
        let file = &self.steps[0].1.file;
        let per_step: Vec<u64> = self
            .variables
            .iter()
            .map(|&index| file.reads[index].decoded_bytes())
            .collect();
        check_query_bytes(self.steps.len(), self.levels.len(), &per_step, limit)
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
    let reads = steps.saturating_mul(levels);
    let bytes = field_bytes.iter().sum::<u64>().saturating_mul(reads as u64);
    if bytes <= limit {
        return Ok(());
    }
    let mib = |b: u64| b as f64 / ds_cache::MIB as f64;
    // The levels, and the advice to name fewer, only when the query reads
    // several: a single-level collection ignores `z` (clause A).
    let (levels, narrow) = if levels > 1 {
        (
            format!(" × {levels} levels"),
            ", name fewer parameters or fewer pressure levels (z)",
        )
    } else {
        (String::new(), " or name fewer parameters")
    };
    Err(DataServerError::QueryTooLarge(format!(
        "Query would decode {} fields ({steps} timesteps{levels} × {} source variables, {:.0} \
         MiB); the limit is {:.0} MiB — narrow the datetime window{narrow}. A derived \
         parameter reads each variable it is derived from: a wind speed reads both wind \
         components",
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
        let all: Vec<usize> = (0..layout.variables.len()).collect();
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

    fn supported_query_types(&self) -> Vec<String> {
        vec!["position".into(), "area".into(), "radius".into()]
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
        let transform = reads.layout.source.transform();
        let (lon_span, lat_span) = ds_core::geo::crs84_bbox_spans(extent);
        let axes = polygon.sample_grid(
            lon_span / f64::from(transform.width.max(1)),
            lat_span / f64::from(transform.height.max(1)),
            MAX_AREA_DIM,
        );
        let (nx, ny) = axes.dims();
        check_area_budget(
            reads.steps.len() * reads.levels.len(),
            ny,
            nx,
            reads.variables.len(),
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
            .variables
            .iter()
            .zip(sampled)
            .map(|(&index, slots)| {
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
                    reads.layout.variables[index].name.to_string(),
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
            parameters: Self::descriptions(&catalog, reads.layout, &reads.variables),
            ranges,
        }))
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
        assert_eq!((reads.steps.len(), reads.variables.len()), (3, 2));
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
            (every.steps.len(), every.levels.len(), every.variables.len()),
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
}
