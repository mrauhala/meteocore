//! EDR cube queries (#1169): every selected parameter over a CRS84 bbox, at
//! the selected pressure levels, over the steps of a `datetime` window.
//!
//! The response is a CoverageJSON `Grid` on CRS84 `x`/`y` axes, always
//! with a `t` axis, and with a `z` axis on a pressure-level collection
//! (`[t, z, y, x]`); a single-level collection's cube is `[t, y, x]`, as a
//! station grid's is.
//!
//! - **Steps** are an area's: without `datetime` the step a map without
//!   `TIME` draws (the newest run's last), else every step in the window of
//!   the run EDR selects; an instance pins the run.
//! - **Levels:** `z`'s (each one the collection's, in request order), or
//!   every level. `resolution-z` asks for that many evenly spaced positions
//!   from the lowest selected level to the highest, each taking the nearest
//!   selected level's values (`ds_core::cube`); a level no position takes
//!   is not read.
//! - **Cells:** at the native resolution, the cell centres of a CRS84 grid
//!   over the bbox at the source's mean cell size in degrees, as an area
//!   query lays them out, but never coarsened: the 1 million value budget
//!   bounds it (`check_cube_budget`). `resolution-x`/`-y` ask for that many
//!   evenly spaced positions from edge to edge, both edges included. A
//!   source grid is rarely CRS84-aligned (MEPS is Lambert conformal conic),
//!   so every cell is projected onto it once (Critical Rule 5) and sampled
//!   as a position query samples a point: bilinear in the grid's x/y, as
//!   the map draws it, and converted to the unit served. engine-grib's
//!   lat/lon cube instead takes the nearest native node.
//!
//! A per-step amount (#1165) is an area's: each cell's accumulation at the
//! step less the accumulation at the run's step before, both sampled there.
//!
//! Both caps come before any read: `check_cube_budget` on the response's
//! values, then [`super::QUERY_BYTES`] on the fields decoded, steps (a
//! per-step amount's steps before included) × read levels × variables. A
//! bbox outside the grid is a 404 and reads nothing.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use ds_core::cube::{axis_positions, check_cube_budget, nearest_indices, CubeResolution};
use ds_core::error::DataServerError;
use ds_core::feature::Bbox;
use ds_core::model::{CoverageResponse, DomainDescription, NdArray, QueryResult, VerticalCoord};

use super::{check_query_bytes, on_grid, Positions, Sampling, Window};
use crate::NetcdfEngine;

impl NetcdfEngine {
    /// The cube query, its decoded bytes capped at `limit`
    /// ([`super::QUERY_BYTES`] from the trait).
    #[allow(clippy::too_many_arguments)] // the trait's selectors plus the cap
    pub(super) fn cube(
        &self,
        bbox: &Bbox,
        datetime: Window,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        resolution: CubeResolution,
        reference_time: Option<DateTime<Utc>>,
        limit: u64,
    ) -> Result<CoverageResponse, DataServerError> {
        ds_core::deadline::check()?;
        let catalog = self.catalog.load_full();
        let reads = Self::select(&catalog, reference_time, datetime, parameters, z, false)?;
        let layout = reads.layout;
        if resolution.z.is_some() && layout.vertical.is_none() {
            return Err(DataServerError::InvalidParameter(
                "This collection has no vertical dimension; resolution-z is not supported".into(),
            ));
        }

        // The output z axis and, per position, the selected level it takes
        // (an index into `reads.levels`).
        let (z_axis, z_source): (Option<VerticalCoord>, Vec<usize>) = match &layout.vertical {
            None => (None, vec![0]),
            Some(axis) => {
                let selected: Vec<f64> = reads.levels.iter().map(|&l| axis.levels[l]).collect();
                let (values, source) = match resolution.z {
                    Some(n) => {
                        let lo = selected.iter().copied().fold(f64::INFINITY, f64::min);
                        let hi = selected.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                        let positions = axis_positions(lo, hi, n);
                        let source = nearest_indices(&selected, &positions, f64::INFINITY)
                            .into_iter()
                            .collect::<Option<Vec<usize>>>()
                            .ok_or_else(|| {
                                DataServerError::Engine(
                                    "cube level resampling found no level".into(),
                                )
                            })?;
                        (positions, source)
                    }
                    None => (selected.clone(), (0..selected.len()).collect()),
                };
                (
                    Some(VerticalCoord {
                        kind: axis.kind,
                        values,
                    }),
                    source,
                )
            }
        };
        // The selected levels some output position takes: only they are read.
        let used: Vec<usize> = (0..reads.levels.len())
            .filter(|level| z_source.contains(level))
            .collect();

        let extent = catalog.info.spatial_extent.ok_or_else(|| {
            DataServerError::LocationNotFound("No NetCDF data available yet".into())
        })?;
        let (columns, rows) = layout.source.size();
        let (lon_span, lat_span) = ds_core::geo::crs84_bbox_spans(extent);
        let res_lon = lon_span / f64::from(columns.max(1));
        let res_lat = lat_span / f64::from(rows.max(1));
        let width = if bbox.crosses_antimeridian() {
            bbox.east + 360.0 - bbox.west
        } else {
            bbox.east - bbox.west
        };
        let height = bbox.north - bbox.south;
        let nx = resolution.x.unwrap_or_else(|| cells(width, res_lon));
        let ny = resolution.y.unwrap_or_else(|| cells(height, res_lat));
        let (nt, nz) = (reads.steps.len(), z_source.len());
        // Both caps before anything that grows with them is allocated.
        check_cube_budget(nt, nz, ny, nx, reads.params.len())?;
        check_query_bytes(reads.steps_read(), used.len(), &reads.field_bytes(), limit)?;
        if !bbox.intersects_bbox(&extent) {
            return Err(DataServerError::LocationNotFound(
                "The bbox lies outside the collection's spatial extent".into(),
            ));
        }

        // From the west edge eastwards (across the antimeridian when the
        // bbox crosses it), and from the north edge southwards, as an area's
        // rows run.
        let x: Vec<f64> = match resolution.x {
            Some(n) => axis_positions(bbox.west, bbox.west + width, n),
            None => centres(bbox.west, width, nx),
        }
        .into_iter()
        .map(|lon| if lon > 180.0 { lon - 360.0 } else { lon })
        .collect();
        let y: Vec<f64> = match resolution.y {
            Some(n) => axis_positions(bbox.north, bbox.south, n),
            None => centres(bbox.north, -height, ny),
        };
        // Each cell centre projected once; every field samples it.
        let source = &layout.source;
        let positions: Positions = Arc::new(
            y.iter()
                .flat_map(|&lat| x.iter().map(move |&lon| on_grid(source, lon, lat)))
                .collect(),
        );
        if positions.iter().all(Option::is_none) {
            return Err(DataServerError::LocationNotFound(
                "The bbox lies outside the collection's grid".into(),
            ));
        }

        // One field per step and read level, step-major.
        let fields: Vec<Sampling> = reads
            .steps
            .iter()
            .flat_map(|&(valid, step)| {
                let (levels, positions) = (&reads.levels, &positions);
                used.iter().map(move |&u| Sampling {
                    valid,
                    step,
                    level: levels[u],
                    positions: positions.clone(),
                })
            })
            .collect();
        let sampled = self.sample_fields(&reads, &fields)?;
        let plane = nx * ny;
        let ranges = reads
            .params
            .iter()
            .zip(sampled)
            .map(|(&param, slots)| {
                let mut values = Vec::with_capacity(nt * nz * plane);
                for step in 0..nt {
                    for &level in &z_source {
                        let u = used
                            .iter()
                            .position(|&l| l == level)
                            .expect("every level a position takes is read");
                        values.extend_from_slice(&slots[step * used.len() + u]);
                    }
                }
                let (shape, axis_names) = if z_axis.is_some() {
                    (vec![nt, nz, ny, nx], vec!["t", "z", "y", "x"])
                } else {
                    (vec![nt, ny, nx], vec!["t", "y", "x"])
                };
                (
                    layout.name(param).to_string(),
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
                x,
                y,
                t: Some(reads.steps.iter().map(|(t, _)| *t).collect()),
                z: z_axis,
            },
            parameters: Self::descriptions(&catalog, layout, &reads.params),
            ranges,
        }))
    }
}

/// Cells of `resolution` degrees across `span` degrees, at least one: the
/// native cell count of an axis, as an area query's grid counts it, never
/// clamped (a float past `usize::MAX` saturates, and the value budget
/// refuses it).
fn cells(span: f64, resolution: f64) -> usize {
    if !(resolution.is_finite() && resolution > 0.0) {
        return 1;
    }
    ((span / resolution).ceil() as usize).max(1)
}

/// The centres of `n` equal cells across `span` from `start` (`span`
/// negative runs downwards): an area query's cell centres.
fn centres(start: f64, span: f64, n: usize) -> Vec<f64> {
    let cell = span / n as f64;
    (0..n).map(|i| start + (i as f64 + 0.5) * cell).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_cells_are_counted_and_centred_like_an_area() {
        assert_eq!(cells(10.0, 2.5), 4);
        assert_eq!(cells(10.0, 3.0), 4, "a partial cell counts");
        assert_eq!(cells(0.5, 2.5), 1, "never none");
        assert_eq!(cells(10.0, 0.0), 1);
        assert_eq!(cells(1e30, 1e-30), usize::MAX, "saturates for the budget");
        assert_eq!(centres(10.0, 10.0, 4), [11.25, 13.75, 16.25, 18.75]);
        assert_eq!(centres(64.0, -4.0, 2), [63.0, 61.0]);
        let axes = ds_core::feature::parse_area_coords("10,60,20,64")
            .unwrap()
            .sample_grid(2.5, 2.0, 256);
        assert_eq!(axes.x, centres(10.0, 10.0, cells(10.0, 2.5)));
        assert_eq!(axes.y, centres(64.0, -4.0, cells(4.0, 2.0)));
    }
}
