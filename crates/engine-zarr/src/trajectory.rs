//! Trajectory queries (#926): values along a path, read as a few windows
//! rather than per sample.
//!
//! The path, time rules and `Trajectory` coverages come from
//! `ds_core::trajectory`. This module selects the run, timesteps and
//! variables exactly like a position query and samples with the same
//! bilinear interpolation, but reads the path in segments: consecutive
//! samples share one `read_window_span` subset per variable, covering their
//! bbox and timestep span, bounded in native cells (Critical Rule 9 — a
//! window read is a sequential blocking retrieval on this thread).

use chrono::{DateTime, Utc};
use ds_core::error::DataServerError;
use ds_core::feature::MAX_AREA_VALUES;
use ds_core::model::{CoverageResponse, ParameterDescription};
use ds_core::trajectory::{TrajectoryAxes, TrajectoryPath, TrajectoryPlan};

use crate::catalog::{Catalog, Variable};

/// Native cells one segment's window may cover per timestep.
const MAX_WINDOW_CELLS: usize = 256 * 256;
/// Most window reads (segments × variables) one trajectory may make; each
/// is a sequential subset retrieval that can touch several chunks.
pub(crate) const MAX_TRAJECTORY_READS: usize = 64;

pub(crate) fn query(
    cat: &Catalog,
    coords: &str,
    datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
    parameters: Option<&[String]>,
    reference_time: Option<DateTime<Utc>>,
) -> Result<CoverageResponse, DataServerError> {
    ds_core::deadline::check()?;
    let path = TrajectoryPath::parse(coords)?;
    if cat.times.is_empty() {
        return Err(DataServerError::Engine("No Zarr data available".into()));
    }
    let run = cat.resolve_run(reference_time)?;
    // A 2-D path takes the position query's steps; an M path snaps to any
    // step of the run.
    let (time_idx, run_times) = if path.has_m {
        let times = cat.valid_times(run);
        ((0..times.len()).collect(), times)
    } else {
        super::select_time_idx(cat, run, datetime)?
    };
    let times: Vec<DateTime<Utc>> = time_idx.iter().map(|&i| run_times[i]).collect();
    let selected = super::select_vars(cat, parameters)?;
    // Zarr collections have no vertical axis: a Z coordinate is ignored.
    let plan = TrajectoryPlan::new(
        &path,
        cat.native_spacing(),
        TrajectoryAxes {
            times: &times,
            vertical: None,
            z: None,
        },
        selected.len(),
    )?;
    plan.require_extent(Some(cat.extent))?;
    let values = sample(cat, run, &plan, &time_idx, &selected)?;
    let descriptions: Vec<(String, ParameterDescription)> = selected
        .iter()
        .map(|v| {
            (
                v.name.clone(),
                ParameterDescription {
                    label: v.label.clone(),
                    unit: v.units.clone(),
                    observed_property: v.name.clone(),
                    standard_name: v.standard_name.clone(),
                },
            )
        })
        .collect();
    plan.into_response(&descriptions, &values)
}

/// A run of consecutive path samples read through one window.
#[derive(Debug)]
struct Segment {
    /// Sample indices `start..end`.
    start: usize,
    end: usize,
    bbox: [f64; 4],
    /// Run-axis timestep span `t0..=t1` the samples read.
    t0: usize,
    t1: usize,
}

/// `values[variable][field][point]`: each segment's window read once per
/// variable, every field sampled at its points inside the segment.
fn sample(
    cat: &Catalog,
    run: Option<usize>,
    plan: &TrajectoryPlan,
    time_idx: &[usize],
    vars: &[&Variable],
) -> Result<Vec<Vec<Vec<Option<f64>>>>, DataServerError> {
    let points = plan.points();
    let fields = plan.fields();
    // The run-axis timestep span each sample reads, over every coverage.
    let mut span: Vec<Option<(usize, usize)>> = vec![None; points.len()];
    for field in fields {
        let t = time_idx[field.time];
        for &p in &field.points {
            span[p] = Some(span[p].map_or((t, t), |(a, b)| (a.min(t), b.max(t))));
        }
    }
    let segments = segments(cat, points, &span)?;
    let reads = segments.len().saturating_mul(vars.len());
    if reads > MAX_TRAJECTORY_READS {
        return Err(DataServerError::QueryTooLarge(format!(
            "The trajectory would need {reads} window reads ({} path segments × {} \
             variables); the limit is {MAX_TRAJECTORY_READS} — shorten the path or the \
             datetime window, or name fewer parameters",
            segments.len(),
            vars.len()
        )));
    }
    let mut values: Vec<Vec<Vec<Option<f64>>>> = vars
        .iter()
        .map(|_| fields.iter().map(|f| vec![None; f.points.len()]).collect())
        .collect();
    for (v, var) in vars.iter().enumerate() {
        for seg in &segments {
            ds_core::deadline::check()?;
            let Some(windows) = cat.read_window_span(var, run, seg.t0..seg.t1 + 1, seg.bbox)?
            else {
                continue; // the segment lies off the grid: nulls
            };
            for (f, field) in fields.iter().enumerate() {
                let t = time_idx[field.time];
                if t < seg.t0 || t > seg.t1 {
                    continue;
                }
                let lo = field.points.partition_point(|&p| p < seg.start);
                let hi = field.points.partition_point(|&p| p < seg.end);
                for k in lo..hi {
                    let (lon, lat) = points[field.points[k]];
                    values[v][f][k] = windows[t - seg.t0].sample(lon, lat);
                }
            }
        }
    }
    Ok(values)
}

/// Split the path into runs of consecutive samples whose window stays
/// within [`MAX_WINDOW_CELLS`] per timestep and [`MAX_AREA_VALUES`] across
/// its timestep span. A longitude jump over 180° (the antimeridian) starts
/// a new segment, so no window spans the globe. Samples no coverage reads
/// (a closed loop's repeat) ride along without widening anything.
fn segments(
    cat: &Catalog,
    points: &[(f64, f64)],
    span: &[Option<(usize, usize)>],
) -> Result<Vec<Segment>, DataServerError> {
    let cost = |bbox: [f64; 4], t0: usize, t1: usize| {
        let cells = cat
            .window_dims(bbox)
            .map_or(0, |(c, r)| c.saturating_mul(r));
        (cells, cells.saturating_mul(t1 - t0 + 1))
    };
    let fits =
        |(cells, values): (usize, usize)| cells <= MAX_WINDOW_CELLS && values <= MAX_AREA_VALUES;
    let mut out: Vec<Segment> = Vec::new();
    let mut last_lon: Option<f64> = None;
    for (i, (&(lon, lat), s)) in points.iter().zip(span).enumerate() {
        let Some((a, b)) = *s else {
            if let Some(seg) = out.last_mut() {
                seg.end = i + 1;
            }
            continue;
        };
        let seam = last_lon.is_some_and(|prev| (lon - prev).abs() > 180.0);
        last_lon = Some(lon);
        if let Some(seg) = out.last_mut().filter(|_| !seam) {
            let bbox = [
                seg.bbox[0].min(lon),
                seg.bbox[1].min(lat),
                seg.bbox[2].max(lon),
                seg.bbox[3].max(lat),
            ];
            let (t0, t1) = (seg.t0.min(a), seg.t1.max(b));
            if fits(cost(bbox, t0, t1)) {
                *seg = Segment {
                    start: seg.start,
                    end: i + 1,
                    bbox,
                    t0,
                    t1,
                };
                continue;
            }
        }
        let seg = Segment {
            start: i,
            end: i + 1,
            bbox: [lon, lat, lon, lat],
            t0: a,
            t1: b,
        };
        let (_, values) = cost(seg.bbox, seg.t0, seg.t1);
        if values > MAX_AREA_VALUES {
            return Err(DataServerError::QueryTooLarge(format!(
                "A trajectory sample would read {values} values across {} timesteps; the limit \
                 is {MAX_AREA_VALUES} — narrow the datetime window",
                seg.t1 - seg.t0 + 1
            )));
        }
        out.push(seg);
    }
    Ok(out)
}
