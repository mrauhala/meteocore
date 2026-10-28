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
//! - **Per-step amounts** (#1165): each sample's value is its
//!   accumulation at its step less the accumulation at the run's step before
//!   that step, both sampled at the sample's position: for an M or ZM path
//!   the step before each sample's own step, in the run the path reads.
//!
//! Both query caps ([`super::QUERY_BYTES`], [`super::QUERY_FIELD_READS`]) count
//! the reads the plan names, each (step, level) field once per variable,
//! and a per-step amount's steps before, before any read. Through OPeNDAP
//! a Z path's levels at one step are one read while the window joining
//! their samples' cells stays worth reading (`super::worth_a_window`):
//! each level is sampled at its own samples, so a long path's joined
//! window soon passes half the field, and its levels are then read one by
//! one, each its window or its field whole.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use ds_core::error::DataServerError;
use ds_core::model::CoverageResponse;
use ds_core::trajectory::{GridSpacing, TrajectoryAxes, TrajectoryPath, TrajectoryPlan};

use super::{layout, on_grid, Limits, Positions, Reads, Sampling, Window};
use crate::catalog::Step;
use crate::field::VALUE_PRECISION;
use crate::NetcdfEngine;

impl NetcdfEngine {
    /// The trajectory query, its reads capped at `limits` (the configured
    /// ones from the trait).
    pub(crate) fn trajectory(
        &self,
        coords: &str,
        datetime: Window,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
        limits: Limits,
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
                Reads::new(layout, run, steps, parameters, query_z)?
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
        let (columns, rows) = layout.source.size();
        // The source's mean cell size in degrees, as an area samples at.
        let spacing = extent
            .and_then(|extent| {
                let (lon_span, lat_span) = ds_core::geo::crs84_bbox_spans(extent);
                GridSpacing::new(
                    lon_span / f64::from(columns.max(1)),
                    lat_span / f64::from(rows.max(1)),
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
            reads.params.len(),
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
                let (valid, step) = reads.steps[field.time];
                Ok(Sampling {
                    valid,
                    step,
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
        let values = self.sample_fields(&reads, &fields, limits, time)?;
        plan.into_response(
            &Self::described(&catalog, layout, &reads.params),
            &vec![VALUE_PRECISION; reads.params.len()],
            &values,
        )
    }
}
