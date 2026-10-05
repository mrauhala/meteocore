//! Native-grid area/radius queries with bounded parallel field sampling.

use super::*;
use crate::runtime::{run_fetches, run_field_jobs};

#[cfg(test)]
mod tests;

impl GribEngine {
    pub(crate) fn query_batched_area(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        ds_core::deadline::check()?;
        // The polygon's bbox selects the native grid subset; cells outside
        // the polygon are masked to null (#671, #966).
        let polygon = ds_core::feature::parse_area_coords(coords)?;
        let bbox = [
            polygon.bbox.west,
            polygon.bbox.south,
            polygon.bbox.east,
            polygon.bbox.north,
        ];
        let catalog = self.catalog();
        let run = resolve_run(&catalog, reference_time, datetime)?;
        // An interval reads every step valid inside it, reported on a `t`
        // axis; an instant (only the step valid at it) or no datetime (the
        // run's last step) is the one step of a t-less Grid.
        let steps = grid_steps(run, datetime)?;
        let time_axis = datetime.is_some_and(|(start, end)| start < end);
        let keys = catalog
            .parameter_keys(&run.reference_time)
            .cloned()
            .unwrap_or_default();
        let levels = self.selected_levels(&catalog, run.reference_time, z)?;

        // Without `parameter-name`, every parameter of the view, as position
        // and cube answer (/req/edr/parameter-name-response A, #966); the
        // value budget below makes too many of them a 400.
        let defaults = match parameters {
            Some(_) => Vec::new(),
            None => self.default_parameters(steps.iter().map(|(_, file)| *file), &keys),
        };
        let query_params: Vec<&str> = parameters
            .unwrap_or(&defaults)
            .iter()
            .map(String::as_str)
            .collect();

        if query_params.is_empty() {
            return Err(DataServerError::InvalidParameter(
                "No parameters specified for area query".to_string(),
            ));
        }

        self.validate_parameters(
            &keys,
            &query_params
                .iter()
                .map(|s| (*s).to_owned())
                .collect::<Vec<_>>(),
        )?;
        // Reject even a one-cell response before any I/O when possible.
        let fields_per_param = steps.len() * levels.len();
        ds_core::feature::check_area_budget(fields_per_param, 1, 1, query_params.len())?;
        let level_keys = Arc::new(
            levels
                .iter()
                .map(|&z| Self::keys_at_level(&keys, z))
                .collect::<Vec<_>>(),
        );
        // A requested field no selected step has at its canonical level is
        // an error. One some steps lack (an aggregate at the analysis step)
        // is null there, as in a position series; a defaulted one is null
        // throughout, so one gap cannot fail the whole default.
        if self.vertical_kind().is_none() && parameters.is_some() {
            for &name in &query_params {
                if !steps
                    .iter()
                    .any(|(_, file)| file.messages.iter().any(|entry| keys[name].matches(entry)))
                {
                    return Err(DataServerError::InvalidParameter(format!(
                        "Parameter '{name}' at its canonical level not found in forecast step"
                    )));
                }
            }
        }
        let (first_step, first_param, first_level, first_entry) = steps
            .iter()
            .enumerate()
            .find_map(|(step, (_, file))| {
                level_keys.iter().enumerate().find_map(|(level, keys)| {
                    query_params.iter().enumerate().find_map(|(param, name)| {
                        let key = keys.get(*name)?;
                        let entry = file.messages.iter().find(|entry| key.matches(entry))?;
                        Some((step, param, level, entry))
                    })
                })
            })
            .ok_or_else(|| {
                DataServerError::InvalidParameter(
                    "No requested parameter/level is present in this forecast step".into(),
                )
            })?;
        // This first field supplies the geometry and its own output values.
        // Protect synchronous cache-fill waits just like the parallel jobs.
        let first_file = steps[first_step].1;
        let grid = run_fetches(async {
            tokio::task::block_in_place(|| {
                self.fetch_grid_by_entry(first_file.message_url(first_entry), first_entry)
            })
        })?;
        ds_core::deadline::check()?;
        let subset = grid.bbox_subset(bbox).ok_or_else(|| {
            DataServerError::InvalidParameter("Bbox does not intersect grid".to_string())
        })?;
        let (x_coords, y_coords) = (&subset.x, &subset.y);

        // The shared per-response budget (#673): every selected step and
        // level of every requested parameter on the same grid.
        let area_pixels = x_coords.len() * y_coords.len();
        ds_core::feature::check_area_budget(
            fields_per_param,
            y_coords.len(),
            x_coords.len(),
            query_params.len(),
        )?;

        // Row-major over y × x, like the values `extract_bbox` returns. The
        // longitude axis is continuous in the requester's frame (for example
        // …359.75, 360, 360.25…), so wrap into (−180, 180] for the test.
        ds_core::feature::check_mask_budget(area_pixels, &polygon)?;
        let x_wrapped: Vec<f64> = x_coords
            .iter()
            .map(|&x| ds_core::geo::wrap_lon(x))
            .collect();
        // The native spacing sizes a lone edge node's cell (a bbox clamped
        // at a regional grid's edge).
        let mask = polygon.mask_native_cells(
            &x_wrapped,
            y_coords,
            Some((grid.lon_inc.abs(), grid.lat_inc.abs())),
        );
        if !mask.iter().any(|&m| m) {
            return Err(DataServerError::LocationNotFound(
                "The polygon contains no grid cell".into(),
            ));
        }

        ds_core::deadline::check()?;
        let mut first_values = grid.subset_values(&subset);
        let layout = Arc::new(AreaLayout {
            bbox,
            x: subset.x,
            y: subset.y,
            mask,
            reference_parameter: query_params[first_param].to_owned(),
        });
        let first_meta =
            self.param_metadata_for(&level_keys[first_level], query_params[first_param]);
        layout.mask_and_convert(&mut first_values, first_meta.display);
        drop(grid); // only small subsets survive while the other fields load

        // Per parameter, `[t][z][y][x]`: one plane per step and level.
        let n_levels = levels.len();
        let plane = |step: usize, level: usize| {
            let offset = (step * n_levels + level) * area_pixels;
            offset..offset + area_pixels
        };
        let mut samples = vec![vec![None; area_pixels * fields_per_param]; query_params.len()];
        samples[first_param][plane(first_step, first_level)].copy_from_slice(&first_values);
        drop(first_values);

        let n_params = query_params.len();
        let fields = steps
            .iter()
            .enumerate()
            .flat_map(|(step, &(_, file))| {
                (0..n_params).flat_map(move |param| {
                    (0..n_levels).map(move |level| (step, file, param, level))
                })
            })
            .filter_map(|(step, file, param, level)| {
                if (step, param, level) == (first_step, first_param, first_level) {
                    return None;
                }
                let key = &level_keys[level][query_params[param]];
                let entry = file.messages.iter().find(|entry| key.matches(entry))?;
                Some((
                    step,
                    param,
                    level,
                    file.message_url(entry).to_owned(),
                    entry.clone(),
                ))
            });
        let engine = Arc::new(Self {
            collection_id: self.collection_id.clone(),
            family: self.family,
            source: self.source.clone(),
        });
        let worker_layout = layout.clone();
        let worker_keys = level_keys.clone();
        let names: Vec<_> = query_params.iter().map(|name| (*name).to_owned()).collect();
        run_field_jobs(
            fields,
            move |(step, param, level, url, entry)| {
                let grid = engine.fetch_grid_by_entry(&url, &entry)?;
                let name = &names[param];
                let meta = engine.param_metadata_for(&worker_keys[level], name);
                let values = worker_layout.sample(&grid, name, meta.display)?;
                Ok((step, param, level, values))
            },
            |(step, param, level, values)| {
                samples[param][plane(step, level)].copy_from_slice(&values);
                Ok(())
            },
        )?;

        let mut param_descs = HashMap::new();
        let mut ranges = HashMap::new();
        for (&pname, all_values) in query_params.iter().zip(samples) {
            let meta = self.param_metadata_for(&keys, pname);
            param_descs.insert(
                pname.to_string(),
                ParameterDescription {
                    label: meta.label(),
                    unit: meta.display.display_unit.to_string(),
                    observed_property: pname.to_string(),
                    standard_name: None,
                },
            );
            let mut shape = vec![layout.y.len(), layout.x.len()];
            let mut axis_names: Vec<String> = vec!["y".into(), "x".into()];
            if self.vertical_kind().is_some() {
                shape.insert(0, n_levels);
                axis_names.insert(0, "z".into());
            }
            if time_axis {
                shape.insert(0, steps.len());
                axis_names.insert(0, "t".into());
            }
            ranges.insert(
                pname.to_string(),
                NdArray {
                    shape,
                    axis_names,
                    values: all_values,
                },
            );
        }

        Ok(CoverageResponse::Single(QueryResult {
            domain: DomainDescription::Grid {
                x: layout.x.clone(),
                y: layout.y.clone(),
                t: time_axis.then(|| steps.iter().map(|(time, _)| *time).collect()),
                z: self.vertical_kind().map(|kind| VerticalCoord {
                    kind,
                    values: levels.into_iter().flatten().collect(),
                }),
            },
            parameters: param_descs,
            ranges,
        }))
    }
}

struct AreaLayout {
    bbox: [f64; 4],
    x: Vec<f64>,
    y: Vec<f64>,
    mask: Vec<bool>,
    reference_parameter: String,
}

impl AreaLayout {
    fn sample(
        &self,
        grid: &DecodedGrid,
        name: &str,
        display: DisplayConversion,
    ) -> Result<Vec<Option<f64>>, DataServerError> {
        let subset = grid.bbox_subset(self.bbox).ok_or_else(|| {
            DataServerError::InvalidParameter(format!("Bbox does not intersect grid for {name}"))
        })?;
        // Check compatibility before allocating the values of a denser grid.
        if subset.x != self.x || subset.y != self.y {
            return Err(DataServerError::InvalidParameter(format!(
                "Parameter '{name}' is on a different grid than '{}'; query them separately",
                self.reference_parameter
            )));
        }
        let mut values = grid.subset_values(&subset);
        self.mask_and_convert(&mut values, display);
        Ok(values)
    }

    fn mask_and_convert(&self, values: &mut [Option<f64>], display: DisplayConversion) {
        for (value, &inside) in values.iter_mut().zip(&self.mask) {
            *value = if inside {
                value.map(|value| display.convert(value))
            } else {
                None
            };
        }
    }
}
