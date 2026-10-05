//! EDR cube queries (#925): a bbox of the native grid × vertical levels ×
//! forecast steps, optionally resampled by nearest neighbour.

use super::*;
use crate::cache::GridSubset;
use crate::runtime::{run_fetches, run_field_jobs};
use ds_core::cube::{axis_positions, check_cube_budget, nearest_indices, CubeResolution};
use ds_core::feature::Bbox;

#[cfg(test)]
mod tests;

/// A resampled position takes the nearest native node within half a cell
/// (with rounding slack); farther is off the grid, so missing.
const HALF_CELL: f64 = 0.5 + 1e-9;

impl GribEngine {
    pub(crate) fn query_batched_cube(
        &self,
        bbox: &Bbox,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        resolution: CubeResolution,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        ds_core::deadline::check()?;
        let kind = self.vertical_kind().ok_or_else(|| {
            DataServerError::InvalidParameter(
                "Cube queries need a collection with a vertical axis".into(),
            )
        })?;
        // GRIB grid subsets have never crossed the antimeridian (#667): say
        // so, before any I/O, rather than "does not intersect".
        if bbox.crosses_antimeridian() {
            return Err(DataServerError::InvalidParameter(format!(
                "bbox {},{},{},{} crosses the antimeridian (west > east), which GRIB \
                 collections do not serve; query each side of 180° separately",
                bbox.west, bbox.south, bbox.east, bbox.north
            )));
        }
        let catalog = self.catalog();
        let (run, steps) = cube_steps(&catalog, reference_time, datetime)?;
        let keys = catalog
            .parameter_keys(&run.reference_time)
            .cloned()
            .unwrap_or_default();
        let levels: Vec<f64> = self
            .selected_levels(&catalog, run.reference_time, z)?
            .into_iter()
            .flatten()
            .collect();
        // Every parameter of the collection by default, like a vertical
        // position query; repeats collapse in request order.
        let params: Vec<String> = match parameters {
            Some(names) => names.iter().fold(Vec::new(), |mut unique, name| {
                if !unique.contains(name) {
                    unique.push(name.clone());
                }
                unique
            }),
            None => self.default_parameters(steps.iter().map(|(_, file)| *file), &keys),
        };
        self.validate_parameters(&keys, &params)?;
        if params.is_empty() {
            return Err(DataServerError::InvalidParameter(
                "No parameters available for a cube query".into(),
            ));
        }

        // The output z axis and, per position, the selected level it samples.
        let (z_axis, z_source) = match resolution.z {
            Some(n) => {
                let lo = levels.iter().copied().fold(f64::INFINITY, f64::min);
                let hi = levels.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                let positions = axis_positions(lo, hi, n);
                let source = nearest_indices(&levels, &positions, f64::INFINITY)
                    .into_iter()
                    .collect::<Option<Vec<usize>>>()
                    .ok_or_else(|| {
                        DataServerError::Engine("cube level resampling found no level".into())
                    })?;
                (positions, source)
            }
            None => (levels.clone(), (0..levels.len()).collect()),
        };
        // Levels no output position samples are never read.
        let used_levels: Vec<usize> = (0..levels.len())
            .filter(|level| z_source.contains(level))
            .collect();
        // Everything but the native cell counts is known: reject before I/O,
        // entirely so when both horizontal resolutions are given.
        check_cube_budget(
            steps.len(),
            z_axis.len(),
            resolution.y.unwrap_or(1),
            resolution.x.unwrap_or(1),
            params.len(),
        )?;

        let level_keys: Vec<ParameterKeys> = levels
            .iter()
            .map(|&level| Self::keys_at_level(&keys, Some(level)))
            .collect();
        let field = |(step, level, param): (usize, usize, usize)| {
            let file = steps[step].1;
            let key = level_keys[level].get(&params[param])?;
            let entry = file.messages.iter().find(|entry| key.matches(entry))?;
            Some((
                step,
                level,
                param,
                file.message_url(entry).to_owned(),
                entry.clone(),
            ))
        };
        let (used, n_params) = (used_levels.as_slice(), params.len());
        let slots = (0..steps.len()).flat_map(move |step| {
            used.iter()
                .flat_map(move |&level| (0..n_params).map(move |param| (step, level, param)))
        });
        let (first_step, first_level, first_param, first_url, first_entry) =
            slots.clone().find_map(field).ok_or_else(|| {
                DataServerError::InvalidParameter(
                    "No requested parameter/level is present in the selected forecast steps".into(),
                )
            })?;

        // The first field supplies the geometry and its own output values.
        // Protect synchronous cache-fill waits just like the parallel jobs.
        let grid = run_fetches(async {
            tokio::task::block_in_place(|| self.fetch_grid_by_entry(&first_url, &first_entry))
        })?;
        ds_core::deadline::check()?;
        let wsen = [bbox.west, bbox.south, bbox.east, bbox.north];
        let subset = grid.bbox_subset(wsen).ok_or_else(|| {
            DataServerError::LocationNotFound("The bbox lies outside the collection's grid".into())
        })?;
        let layout = Arc::new(CubeLayout::new(
            &subset,
            wsen,
            resolution,
            (grid.lon_inc.abs(), grid.lat_inc.abs()),
            params[first_param].clone(),
        ));
        if layout.x.is_empty() || layout.y.is_empty() {
            return Err(DataServerError::LocationNotFound(
                "The bbox lies outside the collection's grid".into(),
            ));
        }
        check_cube_budget(
            steps.len(),
            z_axis.len(),
            layout.y.len(),
            layout.x.len(),
            params.len(),
        )?;

        let plane = layout.y.len() * layout.x.len();
        let (nt, nz) = (steps.len(), z_axis.len());
        let mut samples = vec![vec![None; nt * nz * plane]; params.len()];
        let mut place = |step: usize, level: usize, param: usize, values: &[Option<f64>]| {
            for (zi, _) in z_source.iter().enumerate().filter(|&(_, &l)| l == level) {
                let offset = (step * nz + zi) * plane;
                samples[param][offset..offset + plane].copy_from_slice(values);
            }
        };
        let meta = self.param_metadata_for(&level_keys[first_level], &params[first_param]);
        let values = layout.sample(&grid, &subset, meta.display);
        drop(grid); // only small samples survive while the other fields load
        place(first_step, first_level, first_param, &values);
        drop(values);

        let fields = slots
            .filter(|&slot| slot != (first_step, first_level, first_param))
            .filter_map(field);
        let engine = Arc::new(Self {
            collection_id: self.collection_id.clone(),
            family: self.family,
            source: self.source.clone(),
        });
        let worker_layout = layout.clone();
        let worker_keys = Arc::new(level_keys.clone());
        let names = Arc::new(params.clone());
        run_field_jobs(
            fields,
            move |(step, level, param, url, entry)| {
                let grid = engine.fetch_grid_by_entry(&url, &entry)?;
                let name = &names[param];
                let meta = engine.param_metadata_for(&worker_keys[level], name);
                let subset = worker_layout.subset(&grid, name)?;
                let values = worker_layout.sample(&grid, &subset, meta.display);
                Ok((step, level, param, values))
            },
            |(step, level, param, values)| {
                place(step, level, param, &values);
                Ok(())
            },
        )?;

        let mut descriptions = HashMap::new();
        let mut ranges = HashMap::new();
        for (name, values) in params.iter().zip(samples) {
            let meta = self.param_metadata_for(&keys, name);
            descriptions.insert(
                name.clone(),
                ParameterDescription {
                    label: meta.label(),
                    unit: meta.display.display_unit.to_string(),
                    observed_property: name.clone(),
                    standard_name: None,
                },
            );
            ranges.insert(
                name.clone(),
                NdArray {
                    shape: vec![nt, nz, layout.y.len(), layout.x.len()],
                    axis_names: vec!["t".into(), "z".into(), "y".into(), "x".into()],
                    values,
                },
            );
        }
        Ok(CoverageResponse::Single(QueryResult {
            domain: DomainDescription::Grid {
                x: layout.x.clone(),
                y: layout.y.clone(),
                t: Some(steps.iter().map(|(time, _)| *time).collect()),
                z: Some(VerticalCoord {
                    kind,
                    values: z_axis,
                }),
            },
            parameters: descriptions,
            ranges,
        }))
    }
}

/// The forecast run and steps a cube covers. An interval selects every step
/// of the run inside it, the run chosen as for position queries (the latest
/// covering the interval start), else the latest with any step inside it, so
/// an open start (`../end`) works too. An instant, or no datetime, is the one
/// step an area query selects: the nearest step, or the run's last.
#[allow(clippy::type_complexity)]
fn cube_steps<'a>(
    catalog: &'a Catalog,
    reference_time: Option<DateTime<Utc>>,
    datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> Result<(&'a ForecastRun, Vec<(DateTime<Utc>, &'a StepFile)>), DataServerError> {
    let valid_time = |run: &ForecastRun, step: u32| {
        run.reference_time + chrono::Duration::hours(i64::from(step))
    };
    let Some((start, end)) = datetime.filter(|(start, end)| start < end) else {
        let (run, step, file) = select_run_step(catalog, reference_time, datetime)?;
        return Ok((run, vec![(valid_time(run, step), file)]));
    };
    let inside = |run: &'a ForecastRun| -> Vec<(DateTime<Utc>, &'a StepFile)> {
        run.steps
            .iter()
            .map(|(&step, file)| (valid_time(run, step), file))
            .filter(|(time, _)| *time >= start && *time <= end)
            .collect()
    };
    let run = match resolve_run(catalog, reference_time, datetime) {
        Ok(run) => run,
        Err(error) if reference_time.is_none() => catalog
            .runs
            .values()
            .rev()
            .find(|run| !inside(run).is_empty())
            .ok_or(error)?,
        Err(error) => return Err(error),
    };
    let steps = inside(run);
    if steps.is_empty() {
        return Err(DataServerError::InvalidParameter(format!(
            "The forecast run {} has no step between {start} and {end}",
            run.reference_time
        )));
    }
    Ok((run, steps))
}

/// The output grid of a cube over one native grid: the native subset's axes
/// and, per output position, the subset index it samples (at the native
/// resolution, the subset's nodes whose cell meets the bbox).
struct CubeLayout {
    bbox: [f64; 4],
    native_x: Vec<f64>,
    native_y: Vec<f64>,
    x: Vec<f64>,
    y: Vec<f64>,
    x_map: Vec<Option<usize>>,
    y_map: Vec<Option<usize>>,
    reference_parameter: String,
}

impl CubeLayout {
    fn new(
        subset: &GridSubset,
        bbox: [f64; 4],
        resolution: CubeResolution,
        (dx, dy): (f64, f64),
        reference_parameter: String,
    ) -> Self {
        let [west, south, east, north] = bbox;
        let (x, mut x_map) = axis_map(&subset.x, resolution.x, west, east, dx);
        // A global grid wraps: 180° is −180°'s node, a turn away on the axis.
        if resolution.x.is_some() {
            for (index, &position) in x_map.iter_mut().zip(&x) {
                if index.is_none() {
                    *index = nearest_indices(
                        &subset.x,
                        &[position - 360.0, position + 360.0],
                        dx * HALF_CELL,
                    )
                    .into_iter()
                    .flatten()
                    .next();
                }
            }
        }
        let (y, y_map) = axis_map(&subset.y, resolution.y, south, north, dy);
        Self {
            bbox,
            native_x: subset.x.clone(),
            native_y: subset.y.clone(),
            x,
            y,
            x_map,
            y_map,
            reference_parameter,
        }
    }

    /// The same native subset for another field, or an error when it is on
    /// a different grid (checked before sampling anything).
    fn subset(&self, grid: &DecodedGrid, name: &str) -> Result<GridSubset, DataServerError> {
        let subset = grid.bbox_subset(self.bbox).ok_or_else(|| {
            DataServerError::InvalidParameter(format!("bbox does not intersect the grid of {name}"))
        })?;
        if subset.x != self.native_x || subset.y != self.native_y {
            return Err(DataServerError::InvalidParameter(format!(
                "Parameter '{name}' is on a different grid than '{}'; query them separately",
                self.reference_parameter
            )));
        }
        Ok(subset)
    }

    /// The field's output values, converted for display.
    fn sample(
        &self,
        grid: &DecodedGrid,
        subset: &GridSubset,
        display: DisplayConversion,
    ) -> Vec<Option<f64>> {
        grid.sample_subset(subset, &self.y_map, &self.x_map)
            .into_iter()
            .map(|value| value.map(|value| display.convert(value)))
            .collect()
    }
}

/// An output axis and its index map into the native `axis`: the native nodes
/// whose cell meets `min..=max`, or `n` evenly spaced positions from `min` to
/// `max` sampling the nearest node within half a native `spacing`.
fn axis_map(
    axis: &[f64],
    resolution: Option<usize>,
    min: f64,
    max: f64,
    spacing: f64,
) -> (Vec<f64>, Vec<Option<usize>>) {
    match resolution {
        // `bbox_subset` rounds out to the enclosing nodes, whose cells can
        // lie wholly outside an unaligned bbox (/req/edr/rc-bbox-response-cube
        // A, #966): keep the nodes whose cell, half a spacing either side
        // (with rounding slack), intersects it, as an area query's rectangle
        // mask does. The cells tile the grid, so a bbox between two nodes
        // keeps the cell it lies in, and none at all is a bbox off the grid.
        None => {
            let reach = spacing * HALF_CELL;
            let kept: Vec<usize> = (0..axis.len())
                .filter(|&i| axis[i] >= min - reach && axis[i] <= max + reach)
                .collect();
            (
                kept.iter().map(|&i| axis[i]).collect(),
                kept.into_iter().map(Some).collect(),
            )
        }
        Some(n) => {
            let positions = axis_positions(min, max, n);
            let map = nearest_indices(axis, &positions, spacing * HALF_CELL);
            (positions, map)
        }
    }
}
