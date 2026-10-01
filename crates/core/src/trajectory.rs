//! Shared machinery for OGC API - EDR `trajectory` queries on gridded
//! engines (#926): the WKT `LINESTRING` / `Z` / `M` / `ZM` parser, path
//! densification at the source grid spacing, the time and level rules, and
//! the CoverageJSON `Trajectory` coverages the samples are assembled into.
//!
//! An engine's `EdrEngine::query_trajectory` is glue around this module:
//!
//! 1. [`TrajectoryPath::parse`] the `coords`.
//! 2. Select the model run and the time axis the way the engine's position
//!    query does — over [`TrajectoryPath::time_window`] for an M path — and
//!    the levels.
//! 3. [`TrajectoryPlan::new`] densifies the path, gives every sample its
//!    timestep and level, and enforces the sample and value budgets before
//!    anything is read.
//! 4. Sample every [`TrajectoryPlan::fields`] entry — one timestep × level
//!    of each parameter — at its points, the way the engine's position query
//!    samples. Group the reads per field, chunk or window; never read per
//!    sample (Critical Rule 9).
//! 5. [`TrajectoryPlan::into_response`].
//!
//! The rules, from OGC API - EDR 1.2 (`requirements/edr/query_type/
//! trajectory.adoc` and the `/conf/trajectory/*` abstract tests):
//!
//! - **M** is a time in seconds since the Unix epoch. Each sample takes the
//!   run's timestep nearest its time, interpolated linearly along the path.
//!   A vertex time outside the time axis is a 400
//!   (`/conf/trajectory/coords-param-invalid-time`).
//! - **Z** is a level in the collection's vertical coordinate, snapped to
//!   the nearest advertised level. A vertex level outside the advertised
//!   range is a 400 (`/conf/trajectory/coords-param-invalid-linestringz`);
//!   a collection without a vertical axis ignores it (`/req/edr/z-response`
//!   A).
//! - A 2-D or Z path takes the `datetime` selection of a position query,
//!   one coverage per timestep; the `z` parameter (or every level) selects
//!   the levels of a 2-D or M path, one coverage per level.
//! - A Z path with `z`, or an M path with `datetime`, is a 400; the API
//!   layer rejects those before dispatch.
//!
//! Segments follow the short great circle, as the radar cross-section
//! does, so a path from 170° to −170° crosses the antimeridian.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};

use crate::error::DataServerError;
use crate::feature::{Bbox, MAX_AREA_VALUES, MAX_WKT_LENGTH};
use crate::geo::{intermediate_point, wrap_lon};
use crate::model::{
    CoverageResponse, DomainDescription, NdArray, ParameterDescription, QueryResult, VerticalCoord,
};
use crate::vertical::{VerticalDimension, VerticalKind};

/// Most samples one trajectory may take along its path. Above it the query
/// is a 400 `QueryTooLarge`: the path is densified to about one sample per
/// source grid cell, so the caller shortens or splits the path.
pub const MAX_TRAJECTORY_SAMPLES: usize = 2_000;

/// Most nodes (coverages × samples) one trajectory response may hold. Each
/// node is a composite-axis tuple (`[t, x, y]`, ~50 bytes of JSON and a few
/// hundred bytes of intermediate JSON tree), so this bounds the response
/// well below the [`MAX_AREA_VALUES`] value budget's worst case: about the
/// size of a full-budget area grid.
pub const MAX_TRAJECTORY_NODES: usize = 250_000;

/// One vertex of a WKT trajectory, `(lon, lat)` in CRS84 degrees plus the
/// optional level (`Z`) and time (`M`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrajectoryVertex {
    pub lon: f64,
    pub lat: f64,
    pub z: Option<f64>,
    pub t: Option<DateTime<Utc>>,
}

/// A parsed WKT `LINESTRING`, `LINESTRING Z`, `LINESTRING M` or
/// `LINESTRING ZM`. Every vertex carries a level when `has_z` and a time
/// when `has_m`.
#[derive(Debug, Clone, PartialEq)]
pub struct TrajectoryPath {
    pub vertices: Vec<TrajectoryVertex>,
    pub has_z: bool,
    pub has_m: bool,
}

impl TrajectoryPath {
    /// Parse the `coords` of a trajectory query. The keyword is
    /// case-insensitive, and the dimension may be written apart
    /// (`LINESTRING ZM (…)`, the ISO form) or attached (`LINESTRINGZM(…)`,
    /// the EDR examples). Each vertex is `lon lat [z] [m]`: finite, within
    /// ±180 / ±90, `m` in seconds since the Unix epoch (fractions allowed).
    /// At least two vertices, not all identical. `MULTILINESTRING` is not
    /// supported (EDR makes it optional per collection).
    pub fn parse(coords: &str) -> Result<Self, DataServerError> {
        let invalid = DataServerError::InvalidParameter;
        if coords.len() > MAX_WKT_LENGTH {
            return Err(invalid(format!(
                "LINESTRING geometry exceeds maximum length of {MAX_WKT_LENGTH} bytes"
            )));
        }
        let trimmed = coords.trim();
        if strip_prefix_ci(trimmed, "MULTILINESTRING").is_some() {
            return Err(invalid(
                "MULTILINESTRING is not supported — pass a single LINESTRING".into(),
            ));
        }
        let expected = || {
            invalid(
                "Expected WKT LINESTRING, LINESTRING Z, LINESTRING M or LINESTRING ZM \
                 geometry, e.g. LINESTRING(24 60, 25 61)"
                    .into(),
            )
        };
        let rest = strip_prefix_ci(trimmed, "LINESTRING")
            .ok_or_else(expected)?
            .trim_start();
        let (has_z, has_m, rest) = if let Some(r) = strip_prefix_ci(rest, "ZM") {
            (true, true, r)
        } else if let Some(r) = strip_prefix_ci(rest, "Z") {
            (true, false, r)
        } else if let Some(r) = strip_prefix_ci(rest, "M") {
            (false, true, r)
        } else {
            (false, false, rest)
        };
        let inner = rest
            .trim_start()
            .strip_prefix('(')
            .and_then(|r| r.strip_suffix(')'))
            .ok_or_else(expected)?;

        let keyword = keyword(has_z, has_m);
        let form = match (has_z, has_m) {
            (false, false) => "lon lat",
            (true, false) => "lon lat z",
            (false, true) => "lon lat m",
            (true, true) => "lon lat z m",
        };
        let arity = 2 + usize::from(has_z) + usize::from(has_m);
        let number = |token: &str, what: &str| -> Result<f64, DataServerError> {
            token
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())
                .ok_or_else(|| {
                    invalid(format!(
                        "Invalid {what} '{token}': expected a finite number"
                    ))
                })
        };
        let mut vertices = Vec::new();
        for part in inner.split(',') {
            let tokens: Vec<&str> = part.split_whitespace().collect();
            if tokens.len() != arity {
                return Err(invalid(format!(
                    "{keyword} vertex '{}' is not '{form}'",
                    part.trim()
                )));
            }
            let lon = number(tokens[0], "longitude")?;
            let lat = number(tokens[1], "latitude")?;
            if !(-180.0..=180.0).contains(&lon) || !(-90.0..=90.0).contains(&lat) {
                return Err(invalid(format!(
                    "{keyword} vertex out of range: lon={lon}, lat={lat}"
                )));
            }
            let z = has_z.then(|| number(tokens[2], "level")).transpose()?;
            let t = has_m
                .then(|| number(tokens[arity - 1], "time").and_then(epoch_seconds))
                .transpose()?;
            vertices.push(TrajectoryVertex { lon, lat, z, t });
        }
        if vertices.len() < 2 {
            return Err(invalid(format!(
                "{keyword} must have at least two vertices — use a position query for a \
                 single location"
            )));
        }
        if vertices.iter().all(|v| *v == vertices[0]) {
            return Err(invalid(format!(
                "{keyword} vertices must not all be identical — use a position query for a \
                 single location"
            )));
        }
        Ok(Self {
            vertices,
            has_z,
            has_m,
        })
    }

    /// The WKT keyword of this path's form, e.g. `LINESTRING ZM`.
    pub fn keyword(&self) -> &'static str {
        keyword(self.has_z, self.has_m)
    }

    /// `(earliest, latest)` vertex time of an M path — the window an engine
    /// selects its model run over. `None` for a 2-D or Z path.
    pub fn time_window(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        let mut times = self.vertices.iter().filter_map(|v| v.t);
        let first = times.next()?;
        Some(times.fold((first, first), |(lo, hi), t| (lo.min(t), hi.max(t))))
    }
}

fn keyword(has_z: bool, has_m: bool) -> &'static str {
    match (has_z, has_m) {
        (false, false) => "LINESTRING",
        (true, false) => "LINESTRING Z",
        (false, true) => "LINESTRING M",
        (true, true) => "LINESTRING ZM",
    }
}

/// Case-insensitive ASCII prefix strip. `str::get` keeps a multibyte
/// character straddling the prefix length from panicking.
fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    s.get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &s[prefix.len()..])
}

/// An `m` coordinate: seconds since the Unix epoch, as EDR defines it.
fn epoch_seconds(m: f64) -> Result<DateTime<Utc>, DataServerError> {
    let secs = m.floor();
    let mut nanos = ((m - secs) * 1e9).round();
    let mut whole = secs;
    if nanos >= 1e9 {
        whole += 1.0;
        nanos = 0.0;
    }
    (whole.abs() < 1e15)
        .then(|| DateTime::from_timestamp(whole as i64, nanos as u32))
        .flatten()
        .ok_or_else(|| {
            DataServerError::InvalidParameter(format!(
                "Invalid time '{m}': the M coordinate is seconds since the Unix epoch"
            ))
        })
}

/// A source grid's native cell size in degrees, which sets how densely a
/// trajectory samples: about one sample per cell crossed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridSpacing {
    dx_deg: f64,
    dy_deg: f64,
}

impl GridSpacing {
    /// `None` unless both sizes are finite and positive.
    pub fn new(dx_deg: f64, dy_deg: f64) -> Option<Self> {
        (dx_deg.is_finite() && dy_deg.is_finite() && dx_deg > 0.0 && dy_deg > 0.0)
            .then_some(Self { dx_deg, dy_deg })
    }

    /// The mean cell size of a CRS84 `[west, south, east, north]` extent
    /// divided into `[columns, rows]` cells. An extent with `west > east`
    /// crosses the antimeridian.
    pub fn from_extent(extent: [f64; 4], cells: [usize; 2]) -> Option<Self> {
        let [west, south, east, north] = extent;
        let width = if west > east {
            east + 360.0 - west
        } else {
            east - west
        };
        Self::new(
            width / cells[0].max(1) as f64,
            (north - south) / cells[1].max(1) as f64,
        )
    }
}

/// One densified sample: a point on the path with its interpolated level
/// and time.
#[derive(Debug, Clone, Copy, PartialEq)]
struct PathSample {
    lon: f64,
    lat: f64,
    z: Option<f64>,
    t: Option<DateTime<Utc>>,
}

/// Densify `path` along the short great circle of each segment, to about
/// one sample per grid cell crossed. The vertices are kept exactly; the
/// level and time between them are linear in the fraction of the segment.
fn densify(
    path: &TrajectoryPath,
    spacing: GridSpacing,
) -> Result<Vec<PathSample>, DataServerError> {
    let segments: Vec<usize> = path
        .vertices
        .windows(2)
        .map(|w| {
            let dlon = wrap_lon(w[1].lon - w[0].lon).abs();
            let dlat = (w[1].lat - w[0].lat).abs();
            let cells = (dlon / spacing.dx_deg).max(dlat / spacing.dy_deg).ceil();
            // Saturating: an absurd count only has to exceed the cap.
            (cells as usize).max(1)
        })
        .collect();
    let total = segments
        .iter()
        .fold(1usize, |acc, &n| acc.saturating_add(n));
    if total > MAX_TRAJECTORY_SAMPLES {
        return Err(DataServerError::QueryTooLarge(format!(
            "The trajectory would take {total} samples along the path at the source grid \
             spacing ({:.4}° × {:.4}°); the limit is {MAX_TRAJECTORY_SAMPLES} — shorten the path \
             or split it into several queries",
            spacing.dx_deg, spacing.dy_deg
        )));
    }

    let at = |v: &TrajectoryVertex| PathSample {
        lon: wrap_lon(v.lon) + 0.0,
        lat: v.lat + 0.0,
        z: v.z,
        t: v.t,
    };
    let mut samples = Vec::with_capacity(total);
    samples.push(at(&path.vertices[0]));
    for (i, (w, &n)) in path.vertices.windows(2).zip(&segments).enumerate() {
        let (a, b) = (&w[0], &w[1]);
        let antipodal = || {
            DataServerError::InvalidParameter(format!(
                "{} segment {} joins (nearly) antipodal points, which no single great circle \
                 connects — add a vertex between them",
                path.keyword(),
                i + 1
            ))
        };
        intermediate_point(a.lon, a.lat, b.lon, b.lat, 0.5).ok_or_else(antipodal)?;
        for k in 1..=n {
            let sample = if k == n {
                at(b)
            } else {
                let f = k as f64 / n as f64;
                let (lon, lat) =
                    intermediate_point(a.lon, a.lat, b.lon, b.lat, f).ok_or_else(antipodal)?;
                PathSample {
                    lon: lon + 0.0,
                    lat: lat + 0.0,
                    z: a.z.zip(b.z).map(|(z0, z1)| z0 + (z1 - z0) * f),
                    t: a.t.zip(b.t).map(|(t0, t1)| {
                        let span = (t1 - t0).num_milliseconds() as f64;
                        t0 + Duration::milliseconds((span * f).round() as i64)
                    }),
                }
            };
            if samples.last() != Some(&sample) {
                samples.push(sample);
            }
        }
    }
    Ok(samples)
}

/// The axes a trajectory samples from, chosen by the engine.
#[derive(Debug, Clone, Copy)]
pub struct TrajectoryAxes<'a> {
    /// Ascending timesteps. For a 2-D or Z path: the steps the engine's
    /// position query selects for the request's `datetime`, one coverage
    /// each. For an M path: every step of the selected run, which the
    /// samples snap to and whose range bounds the vertex times.
    pub times: &'a [DateTime<Utc>],
    /// The collection's (or selected run's) vertical levels; `None` without
    /// a vertical axis, when a Z coordinate is ignored.
    pub vertical: Option<&'a VerticalDimension>,
    /// The request's `z` levels, already validated by the engine; `None`
    /// selects every level of `vertical`. Never set with a Z path.
    pub z: Option<&'a [f64]>,
}

/// One field a trajectory reads: a timestep and level of every selected
/// parameter, sampled at `points`.
#[derive(Debug, Clone, PartialEq)]
pub struct TrajectoryField {
    /// Index into [`TrajectoryAxes::times`].
    pub time: usize,
    /// The level; `None` without a vertical axis.
    pub level: Option<f64>,
    /// Indices into [`TrajectoryPlan::points`], ascending, without repeats.
    pub points: Vec<usize>,
}

/// One output node: a sample, where its value comes from, and its level.
#[derive(Debug, Clone, Copy)]
struct Node {
    point: usize,
    time: usize,
    level: Option<f64>,
    field: usize,
    pos: usize,
}

#[derive(Debug, Clone)]
struct PlannedCoverage {
    nodes: Vec<Node>,
    /// A level every node shares (the single-valued `z` axis).
    shared_level: Option<f64>,
    /// Whether each node carries its own level (a `z` in the tuples).
    per_node_z: bool,
}

/// What a trajectory query reads and how the results are laid out.
#[derive(Debug, Clone)]
pub struct TrajectoryPlan {
    points: Vec<(f64, f64)>,
    times: Vec<DateTime<Utc>>,
    kind: Option<VerticalKind>,
    fields: Vec<TrajectoryField>,
    coverages: Vec<PlannedCoverage>,
}

/// How the samples of one coverage pick a timestep or a level.
#[derive(Debug, Clone)]
enum Assign<T> {
    Fixed(T),
    PerSample(Vec<T>),
}

impl<T: Copy> Assign<T> {
    fn at(&self, i: usize) -> T {
        match self {
            Assign::Fixed(v) => *v,
            Assign::PerSample(v) => v[i],
        }
    }
}

impl TrajectoryPlan {
    /// Densify `path` at `spacing` and assign every sample its timestep and
    /// level from `axes` (see the module docs for the rules). `parameters`
    /// is the number of parameters the query returns: the whole response
    /// must fit [`MAX_TRAJECTORY_NODES`] (coverages × samples) and
    /// [`MAX_AREA_VALUES`] (× parameters), checked before any allocation
    /// that grows with it.
    pub fn new(
        path: &TrajectoryPath,
        spacing: GridSpacing,
        axes: TrajectoryAxes<'_>,
        parameters: usize,
    ) -> Result<Self, DataServerError> {
        let invalid = DataServerError::InvalidParameter;
        let (Some(&first), Some(&last)) = (axes.times.first(), axes.times.last()) else {
            return Err(invalid(
                "No data available for the requested time range".into(),
            ));
        };
        let samples = densify(path, spacing)?;
        let n = samples.len();

        let time_assign: Vec<Assign<usize>> = if path.has_m {
            for t in path.vertices.iter().filter_map(|v| v.t) {
                if t < first || t > last {
                    return Err(invalid(format!(
                        "{} time {} is outside the available time range {}/{}",
                        path.keyword(),
                        t.to_rfc3339(),
                        first.to_rfc3339(),
                        last.to_rfc3339()
                    )));
                }
            }
            vec![Assign::PerSample(
                samples
                    .iter()
                    .map(|s| nearest_time(axes.times, s.t.expect("M path samples carry a time")))
                    .collect(),
            )]
        } else {
            (0..axes.times.len()).map(Assign::Fixed).collect()
        };

        let level_assign: Vec<Assign<Option<f64>>> = match axes.vertical {
            None => {
                if axes.z.is_some() {
                    return Err(invalid(
                        "This collection has no vertical axis; `z` is not supported".into(),
                    ));
                }
                vec![Assign::Fixed(None)]
            }
            Some(vertical) if path.has_z => {
                if axes.z.is_some() {
                    return Err(invalid(format!(
                        "A {} carries each vertex's level; do not also pass `z`",
                        path.keyword()
                    )));
                }
                let (lo, hi) = vertical.extent().ok_or_else(|| {
                    invalid("This collection advertises no vertical levels".into())
                })?;
                for z in path.vertices.iter().filter_map(|v| v.z) {
                    if z < lo || z > hi {
                        return Err(invalid(format!(
                            "{} level {z} is outside the collection's vertical extent \
                             {lo}/{hi} ({})",
                            path.keyword(),
                            vertical.unit()
                        )));
                    }
                }
                vec![Assign::PerSample(
                    samples
                        .iter()
                        .map(|s| {
                            Some(nearest_level(
                                &vertical.levels,
                                s.z.expect("Z path samples carry a level"),
                            ))
                        })
                        .collect(),
                )]
            }
            Some(vertical) => {
                // One coverage per distinct level, in request order.
                let mut levels: Vec<f64> = Vec::new();
                for &l in axes.z.unwrap_or(&vertical.levels) {
                    if !levels.contains(&l) {
                        levels.push(l);
                    }
                }
                if levels.is_empty() {
                    return Err(invalid(
                        "This collection advertises no vertical levels".into(),
                    ));
                }
                levels.into_iter().map(|l| Assign::Fixed(Some(l))).collect()
            }
        };

        let coverage_count = time_assign.len().saturating_mul(level_assign.len());
        let nodes = coverage_count.saturating_mul(n);
        if nodes > MAX_TRAJECTORY_NODES {
            return Err(DataServerError::QueryTooLarge(format!(
                "Trajectory query would return {nodes} path nodes ({coverage_count} coverages × \
                 {n} samples); the limit is {MAX_TRAJECTORY_NODES} — narrow the datetime window, \
                 the levels or the path"
            )));
        }
        let total = nodes.saturating_mul(parameters.max(1));
        if total > MAX_AREA_VALUES {
            return Err(DataServerError::QueryTooLarge(format!(
                "Trajectory query would return {total} values ({coverage_count} coverages × \
                 {n} samples × {parameters} parameters); the limit is {MAX_AREA_VALUES} — narrow \
                 the datetime window, the levels, the parameters or the path"
            )));
        }

        let mut fields: Vec<TrajectoryField> = Vec::new();
        let mut field_index: HashMap<(usize, Option<u64>), usize> = HashMap::new();
        let mut coverages = Vec::with_capacity(coverage_count);
        for ta in &time_assign {
            for la in &level_assign {
                let per_node_z = matches!(la, Assign::PerSample(_));
                let shared_level = match la {
                    Assign::Fixed(level) => *level,
                    Assign::PerSample(_) => None,
                };
                // CoverageJSON axis values are `uniqueItems`: a path that
                // revisits a point (a closed loop) would repeat a tuple.
                // The repeat reads the same field at the same place, so
                // dropping it loses no value.
                let mut seen = std::collections::HashSet::new();
                let mut nodes = Vec::with_capacity(n);
                for (i, s) in samples.iter().enumerate() {
                    let time = ta.at(i);
                    let level = la.at(i);
                    let key = (
                        time,
                        s.lon.to_bits(),
                        s.lat.to_bits(),
                        level.filter(|_| per_node_z).map(f64::to_bits),
                    );
                    if !seen.insert(key) {
                        continue;
                    }
                    let field = *field_index
                        .entry((time, level.map(f64::to_bits)))
                        .or_insert_with(|| {
                            fields.push(TrajectoryField {
                                time,
                                level,
                                points: Vec::new(),
                            });
                            fields.len() - 1
                        });
                    // A field belongs to one coverage (its time or level is
                    // what tells the coverages apart), and each sample is
                    // visited once per coverage: points stay ascending and
                    // unique.
                    let points = &mut fields[field].points;
                    debug_assert!(points.last().is_none_or(|&p| p < i));
                    points.push(i);
                    nodes.push(Node {
                        point: i,
                        time,
                        level,
                        field,
                        pos: points.len() - 1,
                    });
                }
                coverages.push(PlannedCoverage {
                    nodes,
                    shared_level,
                    per_node_z,
                });
            }
        }

        Ok(Self {
            points: samples.iter().map(|s| (s.lon, s.lat)).collect(),
            times: axes.times.to_vec(),
            kind: axes.vertical.map(|v| v.kind),
            fields,
            coverages,
        })
    }

    /// The densified path samples, `(lon, lat)` with longitude in
    /// (−180, 180]. [`TrajectoryField::points`] index into it.
    pub fn points(&self) -> &[(f64, f64)] {
        &self.points
    }

    /// The fields to read, each a timestep × level of every parameter.
    pub fn fields(&self) -> &[TrajectoryField] {
        &self.fields
    }

    /// 404 when every sample lies outside the collection's CRS84 `extent`
    /// (`west > east` crosses the antimeridian): no part of the path is
    /// representative of the collection, like an area query outside it.
    /// An unknown or malformed extent passes.
    pub fn require_extent(&self, extent: Option<[f64; 4]>) -> Result<(), DataServerError> {
        let Some(bbox) = extent.and_then(|[w, s, e, n]| Bbox::new(w, s, e, n).ok()) else {
            return Ok(());
        };
        if self.points.iter().any(|&(x, y)| bbox.contains(x, y)) {
            return Ok(());
        }
        Err(DataServerError::LocationNotFound(
            "The trajectory lies outside the collection's spatial extent".into(),
        ))
    }

    /// Assemble the response: one `Trajectory` coverage per timestep (2-D
    /// and Z paths) × level (2-D and M paths), a bare `Coverage` when there
    /// is one. `values[p][f][k]` is parameter `p` of field `f` at its `k`-th
    /// point, parallel to `parameters`, [`Self::fields`] and
    /// [`TrajectoryField::points`].
    pub fn into_response(
        self,
        parameters: &[(String, ParameterDescription)],
        values: &[Vec<Vec<Option<f64>>>],
    ) -> Result<CoverageResponse, DataServerError> {
        let shaped = values.len() == parameters.len()
            && values.iter().all(|by_field| {
                by_field.len() == self.fields.len()
                    && by_field
                        .iter()
                        .zip(&self.fields)
                        .all(|(v, f)| v.len() == f.points.len())
            });
        if !shaped {
            return Err(DataServerError::Engine(
                "trajectory samples do not match the plan's fields".into(),
            ));
        }
        let descriptions: HashMap<String, ParameterDescription> =
            parameters.iter().cloned().collect();
        let vertical = |values: Vec<f64>| -> Result<VerticalCoord, DataServerError> {
            let kind = self.kind.ok_or_else(|| {
                DataServerError::Engine("trajectory level without a vertical kind".into())
            })?;
            Ok(VerticalCoord { kind, values })
        };
        let mut results = Vec::with_capacity(self.coverages.len());
        for coverage in &self.coverages {
            let nodes: Vec<(DateTime<Utc>, f64, f64)> = coverage
                .nodes
                .iter()
                .map(|node| {
                    let (x, y) = self.points[node.point];
                    (self.times[node.time], x, y)
                })
                .collect();
            let node_z = coverage
                .per_node_z
                .then(|| vertical(coverage.nodes.iter().filter_map(|n| n.level).collect()))
                .transpose()?;
            let z = coverage
                .shared_level
                .map(|level| vertical(vec![level]))
                .transpose()?;
            let ranges = parameters
                .iter()
                .zip(values)
                .map(|((name, _), by_field)| {
                    let values = coverage
                        .nodes
                        .iter()
                        .map(|node| by_field[node.field][node.pos])
                        .collect();
                    (
                        name.clone(),
                        NdArray {
                            shape: vec![coverage.nodes.len()],
                            axis_names: vec!["composite".into()],
                            values,
                        },
                    )
                })
                .collect();
            results.push(QueryResult {
                domain: DomainDescription::Trajectory { nodes, node_z, z },
                parameters: descriptions.clone(),
                ranges,
            });
        }
        Ok(if results.len() == 1 {
            CoverageResponse::Single(results.remove(0))
        } else {
            CoverageResponse::Collection(results)
        })
    }
}

/// Index of the timestep nearest `t` in ascending `times` (non-empty); the
/// earlier one on a tie.
fn nearest_time(times: &[DateTime<Utc>], t: DateTime<Utc>) -> usize {
    let after = times.partition_point(|&x| x < t);
    match (after.checked_sub(1), times.get(after)) {
        (Some(before), Some(&next)) if next - t < t - times[before] => after,
        (Some(before), _) => before,
        (None, _) => 0,
    }
}

/// The level in `levels` (non-empty) nearest `z`; the first on a tie.
fn nearest_level(levels: &[f64], z: f64) -> f64 {
    levels
        .iter()
        .copied()
        .fold(None::<f64>, |best, l| match best {
            Some(b) if (b - z).abs() <= (l - z).abs() => Some(b),
            _ => Some(l),
        })
        .expect("levels are non-empty")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn hour(h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, h, 0, 0).unwrap()
    }

    fn deg(d: f64) -> GridSpacing {
        GridSpacing::new(d, d).unwrap()
    }

    fn desc(name: &str) -> (String, ParameterDescription) {
        (
            name.to_string(),
            ParameterDescription {
                label: name.into(),
                unit: "K".into(),
                observed_property: name.into(),
                standard_name: None,
            },
        )
    }

    /// Fill every field with a value that identifies it: field index × 1000
    /// + point index.
    fn fill(plan: &TrajectoryPlan, params: usize) -> Vec<Vec<Vec<Option<f64>>>> {
        (0..params)
            .map(|_| {
                plan.fields()
                    .iter()
                    .enumerate()
                    .map(|(f, field)| {
                        field
                            .points
                            .iter()
                            .map(|&p| Some(f as f64 * 1000.0 + p as f64))
                            .collect()
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn parses_every_dimension_spelling() {
        let p = TrajectoryPath::parse("LINESTRING(24 60, 25 61)").unwrap();
        assert!(!p.has_z && !p.has_m);
        assert_eq!(p.vertices[1].lon, 25.0);
        for s in [
            "LINESTRING Z(24 60 850, 25 61 700)",
            "LINESTRINGZ(24 60 850,25 61 700)",
            "linestring z (24 60 850, 25 61 700)",
        ] {
            let p = TrajectoryPath::parse(s).unwrap();
            assert!(p.has_z && !p.has_m, "{s}");
            assert_eq!(p.vertices[1].z, Some(700.0), "{s}");
        }
        // The EDR 1.2 example epoch: 1560507000 = 2019-06-14T10:10:00Z.
        for s in [
            "LINESTRING M(-3.53 50.72 1560507000, -3.35 50.92 1560507600)",
            "LINESTRINGM(-3.53 50.72 1560507000,-3.35 50.92 1560507600)",
        ] {
            let p = TrajectoryPath::parse(s).unwrap();
            assert!(!p.has_z && p.has_m, "{s}");
            assert_eq!(
                p.vertices[0].t,
                Some(Utc.with_ymd_and_hms(2019, 6, 14, 10, 10, 0).unwrap())
            );
            assert_eq!(
                p.time_window(),
                Some((
                    Utc.with_ymd_and_hms(2019, 6, 14, 10, 10, 0).unwrap(),
                    Utc.with_ymd_and_hms(2019, 6, 14, 10, 20, 0).unwrap()
                ))
            );
        }
        for s in [
            "LINESTRING ZM(-3.53 50.72 0.1 1560507000, -3.35 50.92 0.2 1560508800)",
            "LINESTRINGZM (-3.53 50.72 0.1 1560507000,-3.35 50.92 0.2 1560508800)",
        ] {
            let p = TrajectoryPath::parse(s).unwrap();
            assert!(p.has_z && p.has_m, "{s}");
            assert_eq!(p.vertices[1].z, Some(0.2));
            assert_eq!(p.keyword(), "LINESTRING ZM");
        }
        // Fractional epoch seconds keep their fraction.
        let p = TrajectoryPath::parse("LINESTRING M(0 0 0.5, 1 1 10)").unwrap();
        assert_eq!(p.vertices[0].t.unwrap().timestamp_millis(), 500);
        assert!(TrajectoryPath::parse("LINESTRING(0 0, 1 1)")
            .unwrap()
            .time_window()
            .is_none());
    }

    #[test]
    fn rejects_malformed_trajectories() {
        for s in [
            "",
            "POINT(24 60)",
            "LINESTRING EMPTY",
            "LINESTRING(24 60)",
            "LINESTRING(24 60 850, 25 61 700)", // a 2-D path with a third ordinate
            "LINESTRING Z(24 60, 25 61)",       // a Z path without levels
            "LINESTRING M(24 60 1, 25 61)",
            "LINESTRING ZM(24 60 850, 25 61 700)", // ZM with only three ordinates
            "LINESTRING(200 60, 25 61)",
            "LINESTRING(24 95, 25 61)",
            "LINESTRING(NaN 60, 25 61)",
            "LINESTRING Z(24 60 inf, 25 61 1)",
            "LINESTRING M(24 60 1e300, 25 61 1)", // no such instant
            "LINESTRING(24 60, 24 60)",
            "LINESTRING Z(24 60 850, 24 60 850)",
            "LINESTRINGé(24 60, 25 61)",
            "LINESTRING\u{1F600}(0 0, 1 1)",
        ] {
            let err = TrajectoryPath::parse(s).expect_err(s);
            assert!(
                matches!(err, DataServerError::InvalidParameter(_)),
                "{s}: {err}"
            );
        }
        let err = TrajectoryPath::parse("MULTILINESTRING((0 0, 1 1))").unwrap_err();
        assert!(err.to_string().contains("MULTILINESTRING"), "{err}");
        let long = format!("LINESTRING({})", "1 1, ".repeat(3000));
        assert!(TrajectoryPath::parse(&long).is_err());
        // Identical positions at different times are a path through time.
        assert!(TrajectoryPath::parse("LINESTRING M(24 60 0, 24 60 3600)").is_ok());
    }

    #[test]
    fn densifies_at_the_grid_spacing_keeping_vertices() {
        let path = TrajectoryPath::parse("LINESTRING(0 0, 0 10, 5 10)").unwrap();
        let s = densify(&path, deg(1.0)).unwrap();
        // 10 cells up the meridian, 5 along the parallel, plus the start.
        assert_eq!(s.len(), 16);
        assert_eq!((s[0].lon, s[0].lat), (0.0, 0.0));
        assert_eq!((s[10].lon, s[10].lat), (0.0, 10.0));
        assert_eq!((s[15].lon, s[15].lat), (5.0, 10.0));
        assert!((s[3].lat - 3.0).abs() < 1e-9);
        // A coarser grid samples less; the endpoints stay.
        assert_eq!(densify(&path, deg(2.5)).unwrap().len(), 7);
    }

    #[test]
    fn densify_interpolates_level_and_time() {
        let path = TrajectoryPath::parse("LINESTRING ZM(0 0 1000 0, 0 4 600 14400)").unwrap();
        let s = densify(&path, deg(1.0)).unwrap();
        assert_eq!(s.len(), 5);
        assert_eq!(s[1].z, Some(900.0));
        assert_eq!(s[2].t, Some(DateTime::from_timestamp(7200, 0).unwrap()));
        assert_eq!(s[4].z, Some(600.0));
    }

    #[test]
    fn densify_crosses_the_antimeridian_the_short_way() {
        let path = TrajectoryPath::parse("LINESTRING(170 10, -170 20)").unwrap();
        let s = densify(&path, deg(1.0)).unwrap();
        // 20° of longitude the short way, not 340°.
        assert_eq!(s.len(), 21);
        assert!(s.iter().all(|p| p.lon.abs() >= 169.9), "{s:?}");
        assert!(s.iter().all(|p| p.lon > -180.0 && p.lon <= 180.0));
        // Antipodal endpoints are refused rather than guessed.
        let path = TrajectoryPath::parse("LINESTRING(0 0, 180 0)").unwrap();
        assert!(matches!(
            densify(&path, deg(1.0)),
            Err(DataServerError::InvalidParameter(_))
        ));
    }

    #[test]
    fn densify_caps_the_sample_count() {
        let path = TrajectoryPath::parse("LINESTRING(0 0, 30 0)").unwrap();
        let err = densify(&path, deg(0.01)).unwrap_err();
        assert!(matches!(err, DataServerError::QueryTooLarge(_)), "{err}");
        assert!(densify(&path, deg(0.016)).is_ok());
    }

    #[test]
    fn spacing_from_an_extent_handles_the_seam() {
        let s = GridSpacing::from_extent([-180.0, -90.0, 180.0, 90.0], [1440, 720]).unwrap();
        assert_eq!(s, deg(0.25));
        let s = GridSpacing::from_extent([170.0, 0.0, -170.0, 10.0], [20, 10]).unwrap();
        assert_eq!(s, deg(1.0));
        assert!(GridSpacing::new(0.0, 1.0).is_none());
        assert!(GridSpacing::new(f64::NAN, 1.0).is_none());
    }

    #[test]
    fn a_2d_path_is_one_coverage_per_timestep() {
        let path = TrajectoryPath::parse("LINESTRING(0 0, 2 0)").unwrap();
        let times = [hour(0), hour(6)];
        let axes = TrajectoryAxes {
            times: &times,
            vertical: None,
            z: None,
        };
        let plan = TrajectoryPlan::new(&path, deg(1.0), axes, 1).unwrap();
        assert_eq!(plan.points().len(), 3);
        assert_eq!(plan.fields().len(), 2);
        assert_eq!(plan.fields()[1].time, 1);
        assert_eq!(plan.fields()[1].points, [0, 1, 2]);
        let values = fill(&plan, 1);
        let CoverageResponse::Collection(covs) =
            plan.into_response(&[desc("t2m")], &values).unwrap()
        else {
            panic!("two timesteps → a collection")
        };
        assert_eq!(covs.len(), 2);
        let DomainDescription::Trajectory { nodes, node_z, z } = &covs[1].domain else {
            panic!()
        };
        assert!(node_z.is_none() && z.is_none());
        assert_eq!(nodes[2], (hour(6), 2.0, 0.0));
        let range = &covs[1].ranges["t2m"];
        assert_eq!(range.shape, [3]);
        assert_eq!(range.axis_names, ["composite"]);
        assert_eq!(range.values, [Some(1000.0), Some(1001.0), Some(1002.0)]);
    }

    #[test]
    fn an_m_path_snaps_each_sample_to_the_nearest_timestep() {
        // 00Z → 12Z over four samples: 00, 04, 08, 12 snap to 00, 06, 06, 12
        // (04 is nearer 06; 08 is nearer 06).
        let path = TrajectoryPath::parse(&format!(
            "LINESTRING M(0 0 {}, 3 0 {})",
            hour(0).timestamp(),
            hour(12).timestamp()
        ))
        .unwrap();
        let times = [hour(0), hour(6), hour(12), hour(18)];
        let axes = TrajectoryAxes {
            times: &times,
            vertical: None,
            z: None,
        };
        let plan = TrajectoryPlan::new(&path, deg(1.0), axes, 1).unwrap();
        let used: Vec<usize> = plan.fields().iter().map(|f| f.time).collect();
        assert_eq!(used, [0, 1, 2]);
        let values = fill(&plan, 1);
        let CoverageResponse::Single(cov) = plan.into_response(&[desc("t")], &values).unwrap()
        else {
            panic!("an M path is one coverage")
        };
        let DomainDescription::Trajectory { nodes, .. } = &cov.domain else {
            panic!()
        };
        let t: Vec<_> = nodes.iter().map(|n| n.0).collect();
        assert_eq!(t, [hour(0), hour(6), hour(6), hour(12)]);

        // A vertex outside the time axis is a 400 (EDR
        // /conf/trajectory/coords-param-invalid-time).
        let late = TrajectoryPath::parse(&format!(
            "LINESTRING M(0 0 {}, 3 0 {})",
            hour(0).timestamp(),
            hour(19).timestamp()
        ))
        .unwrap();
        let err = TrajectoryPlan::new(&late, deg(1.0), axes, 1).unwrap_err();
        assert!(
            matches!(&err, DataServerError::InvalidParameter(m) if m.contains("outside")),
            "{err}"
        );
        // No timesteps at all is a 400 too.
        let none = TrajectoryAxes {
            times: &[],
            vertical: None,
            z: None,
        };
        assert!(TrajectoryPlan::new(&path, deg(1.0), none, 1).is_err());
    }

    #[test]
    fn a_z_path_snaps_levels_and_carries_them_in_the_tuples() {
        let vertical = VerticalDimension::new(VerticalKind::Pressure, vec![1000.0, 850.0, 700.0]);
        let path = TrajectoryPath::parse("LINESTRING Z(0 0 1000, 0 4 700)").unwrap();
        let times = [hour(0)];
        let axes = TrajectoryAxes {
            times: &times,
            vertical: Some(&vertical),
            z: None,
        };
        let plan = TrajectoryPlan::new(&path, deg(1.0), axes, 1).unwrap();
        // 1000, 925, 850, 775, 700 → 1000, 1000 (tie → first), 850, 850 (tie), 700.
        let levels: Vec<_> = plan.fields().iter().map(|f| f.level).collect();
        assert_eq!(levels, [Some(1000.0), Some(850.0), Some(700.0)]);
        let values = fill(&plan, 1);
        let CoverageResponse::Single(cov) = plan.into_response(&[desc("t")], &values).unwrap()
        else {
            panic!()
        };
        let DomainDescription::Trajectory { nodes, node_z, z } = &cov.domain else {
            panic!()
        };
        assert!(z.is_none());
        let node_z = node_z.as_ref().unwrap();
        assert_eq!(node_z.kind, VerticalKind::Pressure);
        assert_eq!(node_z.values, [1000.0, 1000.0, 850.0, 850.0, 700.0]);
        assert_eq!(nodes.len(), 5);

        // Outside the advertised range → 400; `z` alongside a Z path → 400.
        let high = TrajectoryPath::parse("LINESTRING Z(0 0 1000, 0 4 500)").unwrap();
        assert!(TrajectoryPlan::new(&high, deg(1.0), axes, 1).is_err());
        let both = TrajectoryAxes {
            z: Some(&[850.0]),
            ..axes
        };
        assert!(TrajectoryPlan::new(&path, deg(1.0), both, 1).is_err());

        // Without a vertical axis the Z coordinate is ignored (EDR
        // /req/edr/z-response A), even out of any range.
        let flat = TrajectoryAxes {
            vertical: None,
            ..axes
        };
        let plan = TrajectoryPlan::new(&high, deg(1.0), flat, 1).unwrap();
        assert!(plan.fields().iter().all(|f| f.level.is_none()));
        let values = fill(&plan, 1);
        let CoverageResponse::Single(cov) = plan.into_response(&[desc("t")], &values).unwrap()
        else {
            panic!()
        };
        assert!(matches!(
            cov.domain,
            DomainDescription::Trajectory {
                node_z: None,
                z: None,
                ..
            }
        ));
    }

    #[test]
    fn a_2d_path_on_levels_is_one_coverage_per_level() {
        let vertical = VerticalDimension::new(VerticalKind::Pressure, vec![1000.0, 850.0, 700.0]);
        let path = TrajectoryPath::parse("LINESTRING(0 0, 1 0)").unwrap();
        let times = [hour(0)];
        let every = TrajectoryAxes {
            times: &times,
            vertical: Some(&vertical),
            z: None,
        };
        let plan = TrajectoryPlan::new(&path, deg(1.0), every, 1).unwrap();
        assert_eq!(plan.fields().len(), 3, "z omitted → every level");
        let one = TrajectoryAxes {
            z: Some(&[850.0]),
            ..every
        };
        let plan = TrajectoryPlan::new(&path, deg(1.0), one, 1).unwrap();
        // A repeated `z` level is one coverage, not two sharing a field.
        let twice = TrajectoryAxes {
            z: Some(&[850.0, 850.0]),
            ..every
        };
        assert_eq!(
            TrajectoryPlan::new(&path, deg(1.0), twice, 1)
                .unwrap()
                .fields()
                .len(),
            1
        );
        let values = fill(&plan, 1);
        let CoverageResponse::Single(cov) = plan.into_response(&[desc("t")], &values).unwrap()
        else {
            panic!()
        };
        let DomainDescription::Trajectory { node_z, z, .. } = &cov.domain else {
            panic!()
        };
        assert!(node_z.is_none());
        assert_eq!(z.as_ref().unwrap().values, [850.0]);
    }

    #[test]
    fn a_closed_loop_does_not_repeat_a_tuple() {
        let path = TrajectoryPath::parse("LINESTRING(0 0, 1 0, 1 1, 0 0)").unwrap();
        let times = [hour(0)];
        let axes = TrajectoryAxes {
            times: &times,
            vertical: None,
            z: None,
        };
        let plan = TrajectoryPlan::new(&path, deg(1.0), axes, 1).unwrap();
        let values = fill(&plan, 1);
        let CoverageResponse::Single(cov) = plan.into_response(&[desc("t")], &values).unwrap()
        else {
            panic!()
        };
        let DomainDescription::Trajectory { nodes, .. } = &cov.domain else {
            panic!()
        };
        // Four vertices, the last revisiting the first: three nodes.
        assert_eq!(nodes.len(), 3);
        assert_eq!(cov.ranges["t"].values.len(), 3);
    }

    #[test]
    fn the_node_budget_is_checked_up_front() {
        let path = TrajectoryPath::parse("LINESTRING(0 0, 19.5 0)").unwrap();
        // 1950 cells + 1 = 1951 samples; 129 × 1951 > 250 000 nodes.
        let times: Vec<_> = (0..129).map(|i| hour(0) + Duration::hours(i)).collect();
        let axes = TrajectoryAxes {
            times: &times,
            vertical: None,
            z: None,
        };
        let err = TrajectoryPlan::new(&path, deg(0.01), axes, 1).unwrap_err();
        assert!(
            matches!(&err, DataServerError::QueryTooLarge(m) if m.contains("nodes")),
            "{err}"
        );
        let axes = TrajectoryAxes {
            times: &times[..128],
            ..axes
        };
        assert!(TrajectoryPlan::new(&path, deg(0.01), axes, 1).is_ok());
    }

    #[test]
    fn the_value_budget_is_checked_up_front() {
        let path = TrajectoryPath::parse("LINESTRING(0 0, 10 0)").unwrap();
        let times: Vec<_> = (0..24).map(hour).collect();
        let axes = TrajectoryAxes {
            times: &times,
            vertical: None,
            z: None,
        };
        // 24 coverages × 1001 samples × 50 parameters > 1M values.
        let err = TrajectoryPlan::new(&path, deg(0.01), axes, 50).unwrap_err();
        assert!(matches!(err, DataServerError::QueryTooLarge(_)), "{err}");
        assert!(TrajectoryPlan::new(&path, deg(0.01), axes, 40).is_ok());
    }

    #[test]
    fn extent_and_shape_checks() {
        let path = TrajectoryPath::parse("LINESTRING(0 0, 1 0)").unwrap();
        let times = [hour(0)];
        let axes = TrajectoryAxes {
            times: &times,
            vertical: None,
            z: None,
        };
        let plan = TrajectoryPlan::new(&path, deg(1.0), axes, 1).unwrap();
        assert!(plan.require_extent(Some([0.5, -1.0, 5.0, 1.0])).is_ok());
        assert!(plan.require_extent(None).is_ok());
        assert!(matches!(
            plan.require_extent(Some([10.0, 10.0, 20.0, 20.0])),
            Err(DataServerError::LocationNotFound(_))
        ));
        // Across the seam: an extent 170..−170 holds a path at 179.5.
        let seam = TrajectoryPath::parse("LINESTRING(179 0, -179 0)").unwrap();
        let plan = TrajectoryPlan::new(&seam, deg(1.0), axes, 1).unwrap();
        assert!(plan
            .require_extent(Some([170.0, -5.0, -170.0, 5.0]))
            .is_ok());
        // A sampler that returns the wrong shape is an engine error.
        let err = plan
            .clone()
            .into_response(&[desc("t")], &[vec![]])
            .unwrap_err();
        assert!(matches!(err, DataServerError::Engine(_)));
    }

    #[test]
    fn nearest_helpers_break_ties_towards_the_first() {
        let times = [hour(0), hour(6), hour(12)];
        assert_eq!(nearest_time(&times, hour(3)), 0);
        assert_eq!(nearest_time(&times, hour(4)), 1);
        assert_eq!(nearest_time(&times, hour(12)), 2);
        assert_eq!(nearest_level(&[1000.0, 850.0], 925.0), 1000.0);
        assert_eq!(nearest_level(&[1000.0, 850.0], 900.0), 850.0);
    }
}
