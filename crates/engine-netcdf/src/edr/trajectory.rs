//! EDR trajectory queries (#1169): values along a WKT `LINESTRING`,
//! `LINESTRING Z`, `M` or `ZM` path.
//!
//! The path, time and level rules and the `Trajectory` coverages are
//! `ds_core::trajectory`'s: [`TrajectoryPlan`] densifies the path to about
//! one sample per source cell crossed, gives each sample its step and
//! level, and names the fields the samples read. This module selects the
//! run, steps, levels and parameters as a position query does, projects
//! each sample onto the grid once (Critical Rule 5), and reads each planned
//! field once for all its samples through the position query's field jobs
//! (`NetcdfEngine::sample_fields`), never per sample (Critical Rule 9).
//!
//! - **2-D and Z paths** take the position query's steps: the `datetime`
//!   window's in the run EDR selects ([`Catalog::query_run`]), every step of
//!   the newest run without one; one coverage per step.
//! - **M and ZM paths** read the run a map of the earliest vertex time
//!   draws from ([`Catalog::covering_run`], engine-grib's rule), and each
//!   sample snaps to that run's nearest step; a vertex time outside the run
//!   is a 400.
//! - **Levels:** on a pressure-level collection a 2-D or M path reads
//!   `z`'s levels (every level without `z`), one coverage per level; a Z
//!   path's samples snap to the nearest level, a vertex level outside the
//!   axis being a 400. Without a vertical axis a Z coordinate and `z` are
//!   ignored.
//!
//! The query-shape cap ([`super::QUERY_BYTES`]) counts the fields the plan
//! reads, each (step, level) once per variable, before any read.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use ds_core::error::DataServerError;
use ds_core::model::CoverageResponse;
use ds_core::trajectory::{GridSpacing, TrajectoryAxes, TrajectoryPath, TrajectoryPlan};

use super::{layout, on_grid, Positions, Reads, Sampling, Window};
use crate::catalog::Step;
use crate::NetcdfEngine;

impl NetcdfEngine {
    /// The trajectory query, its decoded bytes capped at `limit`
    /// ([`super::QUERY_BYTES`] from the trait).
    pub(super) fn trajectory(
        &self,
        coords: &str,
        datetime: Window,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
        limit: u64,
    ) -> Result<CoverageResponse, DataServerError> {
        ds_core::deadline::check()?;
        let path = TrajectoryPath::parse(coords)?;
        let catalog = self.catalog.load_full();
        let layout = layout(&catalog)?;
        if catalog.runs.is_empty() {
            return Err(DataServerError::LocationNotFound(
                "No NetCDF data available yet".into(),
            ));
        }
        // A Z path's levels are its vertices'; `z` with it is the plan's 400.
        let query_z = z.filter(|_| !path.has_z);
        let reads = match path.time_window() {
            Some((earliest, _)) => {
                let run = catalog.covering_run(reference_time, Some(earliest))?;
                let steps: Vec<(DateTime<Utc>, &Step)> =
                    run.steps.iter().map(|(&t, step)| (t, step)).collect();
                Reads::new(layout, steps, parameters, query_z)?
            }
            None => Self::select(
                &catalog,
                reference_time,
                datetime,
                parameters,
                query_z,
                true,
            )?,
        };
        let vertical = layout.vertical.as_ref();
        // The levels `z` selected (validated: each one the collection's), or
        // `z` as given with a Z path, which the plan refuses.
        let z_levels: Option<Vec<f64>> = match (vertical, z) {
            (Some(axis), Some(_)) if !path.has_z => {
                Some(reads.levels.iter().map(|&l| axis.levels[l]).collect())
            }
            (Some(_), Some(z)) => Some(z.to_vec()),
            _ => None,
        };
        let extent = catalog.info.spatial_extent;
        let transform = layout.source.transform();
        // The source's mean cell size in degrees, as an area samples at.
        let spacing = extent
            .and_then(|extent| {
                let (lon_span, lat_span) = ds_core::geo::crs84_bbox_spans(extent);
                GridSpacing::new(
                    lon_span / f64::from(transform.width.max(1)),
                    lat_span / f64::from(transform.height.max(1)),
                )
            })
            .ok_or_else(|| DataServerError::Engine("NetCDF grid has no extent".into()))?;
        let times: Vec<DateTime<Utc>> = reads.steps.iter().map(|(t, _)| *t).collect();
        let plan = TrajectoryPlan::new(
            &path,
            spacing,
            TrajectoryAxes {
                times: &times,
                vertical,
                z: z_levels.as_deref(),
            },
            reads.variables.len(),
        )?;
        plan.require_extent(extent)?;

        // Each sample projected once, and reused by every field and
        // variable that reads it.
        let projected: Vec<Option<(f64, f64)>> = plan
            .points()
            .iter()
            .map(|&(lon, lat)| on_grid(&layout.source, lon, lat))
            .collect();
        let fields: Vec<Sampling> = plan
            .fields()
            .iter()
            .map(|field| {
                let level = match (vertical, field.level) {
                    (Some(axis), Some(z)) => {
                        axis.levels.iter().position(|&l| l == z).ok_or_else(|| {
                            DataServerError::Engine(format!(
                                "trajectory level {z} is not a level of the collection"
                            ))
                        })?
                    }
                    _ => 0,
                };
                let positions: Positions =
                    Arc::new(field.points.iter().map(|&p| projected[p]).collect());
                Ok(Sampling {
                    step: reads.steps[field.time].1,
                    level,
                    positions,
                })
            })
            .collect::<Result<_, DataServerError>>()?;
        let time = if path.has_m {
            "the path's time span"
        } else {
            "the datetime window"
        };
        reads.check_fields(&fields, limit, time)?;
        let values = self.sample_fields(layout, &reads.variables, &fields)?;
        plan.into_response(
            &Self::described(&catalog, layout, &reads.variables),
            &values,
        )
    }
}
